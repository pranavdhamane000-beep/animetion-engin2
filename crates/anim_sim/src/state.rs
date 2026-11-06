//! Complete simulation state and canonical initialization (spec 14.3, 14.5 step 4, 14.8).
//!
//! [`SimState`] is 14.8's struct with its eight fields and exactly one addition, and the
//! addition is stated rather than slipped in.
//!
//! ## The one extra field, and why it is not hashed
//!
//! `presentation` is the ledger of external one-shot effects this run has announced. 14.9
//! gives [`crate::step::step_once`] a `SimulationMode` parameter and says the mode "controls
//! external side effects only; it never changes solver parameters" — so there has to be
//! something the mode actually controls, or the parameter is decoration. The only thing in
//! WP05 that fits is this ledger.
//!
//! It is therefore **excluded from the canonical state encoding**, which is what keeps 14.12's
//! central criterion intact: warm-up-to-tick-400 and sequential-to-tick-400 must hash
//! identically, and if the ledger were in the digest they never could. 14.7's own list —
//! "tick, algorithm versions, configuration, particles/springs in stable ID order, generation
//! counters and event cursor" — does not mention it either, so the exclusion is the spec's,
//! not this package's convenience.
//!
//! The consequence is a real rule and not a caveat: **two states can hash the same and hold
//! different ledgers**, which is exactly right for a record of what a viewer was told, and
//! which is why [`crate::checkpoint::restore_checkpoint`] clears it. A restored state has
//! never announced anything.

use anim_core::hash::CanonicalBytes;
use anim_core::id::NodeId;
use anim_core::time::FrameRate;
use anim_project::SimulationSettings;
use anim_scene::SceneSnapshot;

use crate::clock::SimulationClock;
use crate::mode::SimulationEffect;
use crate::particles::Particle;
use crate::springs::{Spring, SpringBody};
use crate::{
    Result, SimError, SimTick, CHECKPOINT_FORMAT_VERSION, DEFAULT_SOLVER_ITERATIONS,
    PHILOX_ALGORITHM_VERSION, SIMULATION_MODEL_VERSION,
};

/// What a run has announced, in the order it announced it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresentationLedger {
    /// Every effect recorded, oldest first. Order is presentation order and is not sorted,
    /// because a cue that arrives after another one is a different statement to an audio
    /// consumer even when both name the same occurrence.
    pub effects: Vec<SimulationEffect>,
}

impl PresentationLedger {
    /// Record one effect, if the mode allows it at all.
    pub fn push(&mut self, effect: SimulationEffect) {
        self.effects.push(effect);
    }

    /// A ledger is never replayed: see the module comment.
    pub fn clear(&mut self) {
        self.effects.clear();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.effects.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }
}

/// The configuration `initial_state` and every step read, resolved against the document.
///
/// 14.9 names this parameter `SimulationSettings`, and that name already belongs to the
/// persisted section in [`anim_project::SimulationSettings`] — the one 14.2's
/// `ProjectDocument.simulation.ticks_per_frame` path points at. This is the same data after a
/// clock has been built from it, and it is a separate type for one reason: the clock needs the
/// document's `frame_rate`, and copying that rate into the persisted section would give one
/// timeline two rate bindings that can disagree, which is the same objection
/// [`crate::particles`] raises about a cue's asset. So the resolved form lives here, is built
/// by [`SimSettings::for_document`], and the section keeps exactly the two fields 14.2 and
/// 14.7 name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimSettings {
    /// The clock every conversion runs through.
    pub clock: SimulationClock,
    /// The seed every stream key is derived from (14.7).
    pub project_seed: [u8; anim_project::PROJECT_SEED_BYTES],
    /// The solver pass count a state this build creates starts with.
    ///
    /// From [`DEFAULT_SOLVER_ITERATIONS`] rather than authored, per
    /// `crate::DEFAULT_SOLVER_ITERATIONS`'s reading of 14.6's "fixed number".
    pub solver_iterations: u16,
}

impl SimSettings {
    /// Resolve the persisted section against the document's frame rate.
    ///
    /// # Errors
    ///
    /// [`SimError::InvalidRate`] for a zero tick count or a product that does not fit.
    pub fn new(frame_rate: FrameRate, settings: &SimulationSettings) -> Result<Self> {
        // The section's own refusal is the one 14.2 states -- a zero tick rate -- and it
        // arrives as ProjectError, which has no place in this crate's taxonomy. Renamed
        // rather than chained, because `SimError::InvalidRate` is the answer an operator
        // can act on and a project error would send them looking for a schema problem.
        settings.validate().map_err(|error| SimError::InvalidRate {
            which: "simulation.ticks_per_frame",
            detail: error.to_string(),
        })?;
        Ok(Self {
            clock: SimulationClock::new(frame_rate, settings.ticks_per_frame)?,
            project_seed: settings.project_seed,
            solver_iterations: DEFAULT_SOLVER_ITERATIONS,
        })
    }

    /// The clock's frame rate, which is the document's and is not stored twice.
    #[must_use]
    pub fn frame_rate(&self) -> FrameRate {
        self.clock.frame_rate
    }
}

/// The complete state of one simulation at one tick (spec 14.8).
///
/// 14.8's closing sentence is the ownership rule: "Mutation is confined to initialization,
/// `step_once` and restore." Everything else in this crate takes `&SimState`, and the two
/// exceptions are [`crate::checkpoint::capture_checkpoint`], which reads, and
/// [`crate::checkpoint::restore_checkpoint`], which returns a new value.
#[derive(Clone, Debug, PartialEq)]
pub struct SimState {
    /// The tick this state has reached.
    pub tick: SimTick,
    /// The clock it is measured with, so a state carries the rate it was stepped at rather
    /// than relying on a caller to remember one — which is what lets a checkpoint's mismatch
    /// be detectable at all.
    pub clock: SimulationClock,
    /// Live particles, sorted by [`Particle::id`].
    pub particles: Vec<Particle>,
    /// Spring edges, sorted by [`Spring::id`].
    pub springs: Vec<Spring>,
    /// Dynamic bodies, sorted by [`SpringBody::id`].
    pub bodies: Vec<SpringBody>,
    /// The next generation each emitter will use. 14.8: "Initialization reserves generation
    /// counters for every emitter", and [`initial_state`] does exactly that, which is why a
    /// node that never spawns anything still occupies an entry.
    pub next_generation: std::collections::BTreeMap<NodeId, u32>,
    /// How many scheduled events have been applied. Part of the digest, so a replay that
    /// missed or doubled an event cannot hash to the same value as one that did not.
    pub event_cursor: u64,
    /// The solver pass count in force. Carried rather than re-read from configuration, so a
    /// checkpoint from a build with a different count is a mismatch instead of a quiet change.
    pub solver_iterations: u16,
    /// What this run has announced. **Not hashed** — see the module comment.
    pub presentation: PresentationLedger,
}

impl SimState {
    /// The canonical field stream, in 14.7's stated order and with the domain tag
    /// [`crate::STATE_DOMAIN`].
    ///
    /// The order is the spec's sentence, not a struct layout: "tick, algorithm versions,
    /// configuration, particles/springs in stable ID order, generation counters and event
    /// cursor". `presentation` is absent because that sentence does not include it.
    ///
    /// # Errors
    ///
    /// Any error the component encoders report, which is where 14.11's non-finite and
    /// invalid-solver-count refusals actually surface for a state being hashed.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        // `with_domain` is `new()` plus a length-prefixed `text`, so an id derivation
        // whose signature is infallible can spell it directly instead of unwrapping a
        // constant it already knows is valid ASCII. The bytes are identical; see
        // anim_core::hash::CanonicalBytes::with_domain.
        let mut cp = CanonicalBytes::new();
        cp.text(crate::STATE_DOMAIN);
        self.encode(&mut cp)?;
        Ok(cp.into_vec())
    }

    /// The BLAKE3 digest of the domain-tagged canonical stream (spec 14.7's state hash).
    ///
    /// Reached through [`anim_core::hash::CanonicalBytes::digest`] rather than a direct
    /// `blake3` call, which is how every other digest in this workspace is taken and why
    /// 14.4's "it uses blake3" does not add a dependency here.
    pub fn digest(&self) -> Result<[u8; 32]> {
        // `with_domain` is `new()` plus a length-prefixed `text`, so an id derivation
        // whose signature is infallible can spell it directly instead of unwrapping a
        // constant it already knows is valid ASCII. The bytes are identical; see
        // anim_core::hash::CanonicalBytes::with_domain.
        let mut cp = CanonicalBytes::new();
        cp.text(crate::STATE_DOMAIN);
        self.encode(&mut cp)?;
        Ok(cp.digest())
    }

    /// The state's contribution to a checkpoint digest, without the domain tag.
    ///
    /// Split from [`SimState::canonical_bytes`] because 14.8's `Checkpoint` hashes a state
    /// *and* names the format that wrapped it, and one stream with two tags would make a
    /// checkpoint's hash depend on which caller produced it.
    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) -> Result<()> {
        cp.i64(self.tick.0);
        AlgorithmVersions::current().encode(cp);
        self.clock_config(cp)?;
        cp.u32(u32::from(self.solver_iterations));
        cp.u64(u64::try_from(self.particles.len()).unwrap_or(u64::MAX));
        for particle in &self.particles {
            particle.encode(cp)?;
        }
        cp.u64(u64::try_from(self.springs.len()).unwrap_or(u64::MAX));
        for spring in &self.springs {
            spring.encode(cp)?;
        }
        cp.u64(u64::try_from(self.bodies.len()).unwrap_or(u64::MAX));
        for body in &self.bodies {
            body.encode(cp)?;
        }
        cp.u64(u64::try_from(self.next_generation.len()).unwrap_or(u64::MAX));
        for (node, generation) in &self.next_generation {
            cp.fixed_bytes(node.as_bytes());
            cp.u32(*generation);
        }
        cp.u64(self.event_cursor);
        Ok(())
    }

    /// The configuration part of the stream: everything about *this* clock that a state
    /// depends on, encoded as exact integers rather than as a float.
    ///
    /// The step duration is deliberately not written as `step_seconds`. It is `1/(fps*N)`,
    /// fully determined by the two integers below, and encoding the converted float would
    /// put a second, disagreeable copy of it into the digest — the case ADR-0005 Decision 10
    /// keeps three schema versions apart for, in a smaller frame.
    fn clock_config(&self, cp: &mut CanonicalBytes) -> Result<()> {
        cp.u32(self.clock.frame_rate.num);
        cp.u32(self.clock.frame_rate.den);
        cp.u32(self.clock.ticks_per_frame);
        cp.u32(self.clock.rate.num);
        cp.u32(self.clock.rate.den);
        Ok(())
    }

    /// Every rule that makes this a state rather than a bag of numbers.
    ///
    /// Run after initialization, after restore and by every checkpoint capture, because the
    /// failure it catches is a corrupt checkpoint that decodes cleanly: two particles with one
    /// id encode identically, so the digest alone cannot see the duplicate.
    ///
    /// # Errors
    ///
    /// [`SimError::DuplicateId`] for a repeated particle, body or spring identity;
    /// [`SimError::InvalidSolverCount`] for a count outside the bound;
    /// [`SimError::NonFiniteState`] for any non-finite value;
    /// [`SimError::InvalidRate`] if the carried clock no longer matches its own product.
    pub fn validate(&self) -> Result<()> {
        if self.solver_iterations == 0 || self.solver_iterations > crate::MAX_SOLVER_ITERATIONS {
            return Err(SimError::InvalidSolverCount {
                count: self.solver_iterations,
            });
        }
        crate::clock::SimulationClock::new(self.clock.frame_rate, self.clock.ticks_per_frame)
            .map_err(|error| match error {
                SimError::InvalidRate { .. } => error,
                other => other,
            })?;
        crate::particles::check_unique_ids(&self.particles)?;
        crate::springs::check_unique_chains(self)?;
        for particle in &self.particles {
            particle.validate_finite()?;
        }
        for spring in &self.springs {
            spring.validate_finite()?;
        }
        for body in &self.bodies {
            body.validate_finite()?;
        }
        Ok(())
    }

    /// The tick this state is at, as the frame it belongs to (14.6's `f*N` relation).
    #[must_use]
    pub fn frame(&self) -> i64 {
        self.clock.frame_of_tick(self.tick)
    }

    /// A state with nothing in it, for this crate's tests.
    ///
    /// Not `#[cfg(test)]`-only: [`crate::replay`] and the G4 suite in
    /// `crates/anim_sim/tests/determinism.rs` build reference runs from the same shape, and a
    /// second private copy in each of them would be a fixture that can drift from the type it
    /// is a fixture for. 24 fps at one tick per frame is the deliberately plain choice: every
    /// tick is a frame, so a test failure names the arithmetic rather than the clock.
    #[must_use]
    pub fn empty(clock: SimulationClock) -> Self {
        Self {
            tick: SimTick::ZERO,
            clock,
            particles: Vec::new(),
            springs: Vec::new(),
            bodies: Vec::new(),
            next_generation: std::collections::BTreeMap::new(),
            event_cursor: 0,
            solver_iterations: DEFAULT_SOLVER_ITERATIONS,
            presentation: PresentationLedger::default(),
        }
    }
}

/// Constructors for this crate's own tests, and for the tests of every other module here.
///
/// Its own module rather than an `#[cfg(test)]` associated function because the fallible
/// calls it makes need `unwrap`, and `clippy::unwrap_used` is denied workspace-wide with a
/// per-test-module allow as the only exemption -- an associated function on `SimState` sits
/// outside any test module and would be production code that panics on an arithmetic error.
/// Putting them here keeps that rule intact and gives every module in the crate one way to
/// build a plain state instead of four private copies that can drift.
#[cfg(test)]
pub(crate) mod test_support {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test fixture construction only; 24 fps at one tick per frame is a rate                   `SimulationClock::new` cannot refuse"
    )]

    use super::*;

    /// 24 fps, one tick per frame: the deliberately plain clock, so a failure names the
    /// arithmetic rather than the rate conversion.
    pub(crate) fn clock() -> SimulationClock {
        SimulationClock::new(anim_core::time::FrameRate::new(24, 1).unwrap(), 1).unwrap()
    }

    /// An empty state on that clock.
    pub(crate) fn state() -> SimState {
        SimState::empty(clock())
    }
}

/// The algorithm versions a state carries (spec 14.8's `AlgorithmVersions`, which 14.7
/// requires be encoded in every checkpoint).
///
/// 14.8 names the type and never gives it fields. Three numbers, one per thing whose
/// *reading* can change without any authored data changing — the same distinction WP04 drew
/// when it put `EASING_ALGORITHM_VERSION` into snapshot digests and not into documents
/// (`BUILD_STATE.md`: "a field addition is a data event; a changed reading is invisible in
/// the data and must be visible in the digest"). ADR-0008 is where the first of the three is
/// argued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlgorithmVersions {
    /// The Philox permutation, key split and counter layout.
    pub philox: u32,
    /// The stepping model: stage order, spawn arithmetic, force functions and their
    /// constants.
    pub model: u32,
    /// The checkpoint byte layout that wraps this state.
    pub checkpoint_format: u32,
}

impl AlgorithmVersions {
    /// The versions this build implements.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            philox: PHILOX_ALGORITHM_VERSION,
            model: SIMULATION_MODEL_VERSION,
            checkpoint_format: CHECKPOINT_FORMAT_VERSION,
        }
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.u32(self.philox);
        cp.u32(self.model);
        cp.u32(self.checkpoint_format);
    }

    /// Whether this set is the one the running build produces.
    ///
    /// A checkpoint from a different set is not an error to propagate: 14.7 says malformed or
    /// mismatched checkpoints "are discarded and reconstructed from an earlier verified
    /// state", and this is the predicate that decides the discard.
    #[must_use]
    pub fn is_current(self) -> bool {
        self == Self::current()
    }
}

/// The complete initial state at [`SimTick::ZERO`] for one snapshot (spec 14.9).
///
/// 14.6's first sentence is the definition: "`SimTick(0)` is the initialized state before any
/// simulation step." So the snapshot this is called with is the one at tick zero, and the
/// state it produces is what [`crate::seek`] restores when no checkpoint verifies — the root
/// every comparison in 14.12 is measured against.
///
/// Canonical means canonical: nothing here reads a clock, a counter, a hash map or the
/// environment. Two calls with equal snapshots produce bit-equal states, which is the
/// property the fresh-process half of G4 re-tests from outside.
///
/// # Errors
///
/// [`SimError::NonFiniteState`] if an evaluated transform is not finite, and
/// [`SimError::UnknownEventTarget`] if a node's parent is missing from the input list, which
/// is what [`crate::springs::initial_chains`] refuses rather than defaulting.
pub fn initial_state(snapshot: &SceneSnapshot, settings: &SimSettings) -> Result<SimState> {
    let (bodies, springs) =
        crate::springs::initial_chains(&snapshot.nodes, &snapshot.simulation_inputs)?;
    let mut next_generation = std::collections::BTreeMap::new();
    for input in snapshot.simulation_inputs.iter() {
        // Reserved for every node, not only for the ones that turn out to spawn: 14.8 says
        // "every emitter", and at initialization nothing knows yet which nodes will be
        // named by an event. Reserving the full set also makes the map's key order — and so
        // the digest — independent of which events a scene happens to hold.
        next_generation.insert(input.node, 0);
    }
    let state = SimState {
        tick: SimTick::ZERO,
        clock: settings.clock,
        particles: Vec::new(),
        springs,
        bodies,
        next_generation,
        event_cursor: 0,
        solver_iterations: settings.solver_iterations,
        presentation: PresentationLedger::default(),
    };
    state.validate()?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    #[test]
    fn a_ledger_effect_is_not_part_of_the_digest() {
        // The rule the module comment exists to state, tested as bytes rather than asserted
        // as a claim: two states identical except for what they announced must hash the same,
        // or warm-up and sequential replay can never satisfy 14.12.
        let mut a = test_support::state();
        let mut b = test_support::state();
        b.presentation.push(SimulationEffect {
            tick: SimTick(3),
            occurrence: anim_core::id::EventOccurrenceId::derive(&uuid::Uuid::nil(), b"cue"),
            source: NodeId::derive(&uuid::Uuid::nil(), b"node"),
            kind: anim_project::EventKind::AudioCue,
        });
        assert_eq!(a.presentation.len(), 0);
        assert_eq!(b.presentation.len(), 1);
        assert_eq!(
            a.canonical_bytes().unwrap(),
            b.canonical_bytes().unwrap(),
            "what a viewer was told cannot be part of what the world is"
        );
        a.tick = SimTick(1);
        assert_ne!(
            a.canonical_bytes().unwrap(),
            b.canonical_bytes().unwrap(),
            "and the same two states differing in a hashed field still differ"
        );
    }

    #[test]
    fn the_digest_covers_everything_14_7_lists() {
        let base = test_support::state();
        let base_bytes = base.canonical_bytes().unwrap();
        let mut changed = base.clone();
        changed.tick = SimTick(1);
        assert_ne!(changed.canonical_bytes().unwrap(), base_bytes, "tick");

        for mutate in [
            |state: &mut SimState| state.event_cursor = 7,
            |state: &mut SimState| state.solver_iterations = 9,
            |state: &mut SimState| {
                state.clock.ticks_per_frame = 3;
                state.clock.rate = FrameRate::new(72, 1).unwrap();
            },
            |state: &mut SimState| {
                state
                    .next_generation
                    .insert(NodeId::derive(&uuid::Uuid::nil(), b"late"), 4);
            },
        ] {
            let mut other = base.clone();
            mutate(&mut other);
            assert_ne!(
                other.canonical_bytes().unwrap(),
                base_bytes,
                "a hashed field that leaves the digest unchanged is a field the checkpoint \
                 cannot protect"
            );
        }
        // The domain tag is first, which is ADR-0004's rule for every domain in this engine.
        assert!(base_bytes.starts_with(&(crate::STATE_DOMAIN.len() as u64).to_le_bytes()));
    }

    #[test]
    fn a_repeated_particle_identity_fails_validation_not_the_digest() {
        let mut state = test_support::state();
        let particle = Particle {
            id: 5,
            emitter: NodeId::derive(&uuid::Uuid::nil(), b"e"),
            generation: 0,
            spawn_ordinal: 0,
            position: [0.0; 3],
            previous_position: [0.0; 3],
            velocity: [0.0; 3],
            age_ticks: 0,
            lifetime_ticks: 1,
            size: 0.1,
            rotation: 0.0,
        };
        state.particles = vec![particle, particle];
        assert!(matches!(
            state.validate(),
            Err(SimError::DuplicateId {
                kind: "particle",
                ..
            })
        ));
        // A recomputed digest *does* change here, because the particle count is encoded, so a
        // hash comparison catches a careless tamper. What it cannot catch is a tamper whose
        // hash was recomputed to match -- and that is the case validate exists for, so the
        // claim this test makes is the weaker true one rather than the stronger false one.
        let forged_hash = crate::checkpoint::state_hash(&state).unwrap();
        let forged_entry = crate::checkpoint::Checkpoint {
            tick: state.tick,
            algorithm_versions: AlgorithmVersions::current(),
            state: Box::new(state.clone()),
            state_hash: forged_hash,
        };
        assert_eq!(
            crate::checkpoint::state_hash(&forged_entry.state).unwrap(),
            forged_entry.state_hash,
            "the forged entry is self-consistent, which is exactly what a hash check needs"
        );
        assert!(
            matches!(
                crate::checkpoint::restore_checkpoint(&forged_entry),
                Err(SimError::DuplicateId { .. })
            ),
            "...and only validate refuses it, which is why capture validates before hashing \
             instead of trusting whatever a cache handed back"
        );
    }

    #[test]
    fn an_out_of_bound_solver_count_is_refused_at_both_ends() {
        for count in [0u16, crate::MAX_SOLVER_ITERATIONS + 1, u16::MAX] {
            let mut state = test_support::state();
            state.solver_iterations = count;
            assert!(
                matches!(state.validate(), Err(SimError::InvalidSolverCount { .. })),
                "{count} is not a legal pass count"
            );
        }
        let state = test_support::state();
        assert_eq!(state.solver_iterations, DEFAULT_SOLVER_ITERATIONS);
        state.validate().expect("the default is legal");
    }

    #[test]
    fn algorithm_versions_name_this_build() {
        let versions = AlgorithmVersions::current();
        assert!(versions.is_current());
        assert_eq!(versions.philox, PHILOX_ALGORITHM_VERSION);
        assert_eq!(versions.model, SIMULATION_MODEL_VERSION);
        assert_eq!(versions.checkpoint_format, CHECKPOINT_FORMAT_VERSION);
        let mut other = AlgorithmVersions::current();
        other.model += 1;
        assert!(
            !other.is_current(),
            "a changed reading of the stepping model is a different state family"
        );
        let mut cp = CanonicalBytes::new();
        versions.encode(&mut cp);
        assert_eq!(
            cp.as_slice(),
            [
                PHILOX_ALGORITHM_VERSION.to_le_bytes().as_slice(),
                SIMULATION_MODEL_VERSION.to_le_bytes().as_slice(),
                CHECKPOINT_FORMAT_VERSION.to_le_bytes().as_slice()
            ]
            .concat()
        );
    }

    #[test]
    fn settings_resolve_the_clock_from_the_sections_two_fields() {
        let settings = SimulationSettings::new(4, [9u8; anim_project::PROJECT_SEED_BYTES]).unwrap();
        let resolved = SimSettings::new(FrameRate::new(24, 1).unwrap(), &settings).unwrap();
        assert_eq!(resolved.clock.ticks_per_frame, 4);
        assert_eq!(resolved.clock.rate, FrameRate::new(96, 1).unwrap());
        assert_eq!(
            resolved.project_seed,
            [9u8; anim_project::PROJECT_SEED_BYTES]
        );
        assert_eq!(resolved.frame_rate(), FrameRate::new(24, 1).unwrap());
        assert_eq!(
            resolved.solver_iterations, DEFAULT_SOLVER_ITERATIONS,
            "the pass count is this build's constant, not a persisted field, and this is the \
            line that says so"
        );
    }

    #[test]
    fn the_frame_of_a_state_is_the_floor_divided_tick() {
        let mut state = test_support::state();
        state.clock.ticks_per_frame = 4;
        for (tick, frame) in [(0i64, 0i64), (3, 0), (4, 1), (-1, -1), (-4, -1), (-5, -2)] {
            state.tick = SimTick(tick);
            assert_eq!(state.frame(), frame, "tick {tick} at four per frame");
        }
    }
}
