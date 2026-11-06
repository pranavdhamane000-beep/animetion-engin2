//! Spawn, integrate, age, stable removal and draw extraction (spec 14.3, 14.6, 14.8).
//!
//! ## What feeds a particle, when nothing in the schema authorizes one yet
//!
//! 14.5 step 6 asks for "particles and springs with stable ordering and fixed solver
//! counts" and 14.3 gives this file four verbs, but no section of the spec authors an
//! emitter. `EventKind` has three variants and no payloads; `EvaluatedNode` carries a
//! transform, a `z`, an opacity, a visibility and an asset; and the per-emitter parameters
//! WP14 will want — rate, cone, lifetime range, capacity — live in spec 29's
//! `EmitterRuntime`/`ChainParams`, which is WP14's schema in
//! `anim_scene/src/components/vfx.rs`.
//!
//! So WP05 builds the machinery over the data that exists and version-stamps the parameters
//! it had to supply itself:
//!
//! - **a spawn is an event.** 14.5 step 5 compiles scene events into a tick schedule, and
//!   14.6's first stage is "applies scheduled events, … emits particles". The kind that
//!   means *react to me* and has no other consumer is [`EventKind::Signal`]: a camera cut is
//!   read by `anim_scene`'s cut evaluation and an audio cue by `anim_audio` (13.10), so the
//!   signal is the one authored event this crate may read as an instruction.
//! - **the emitter is the event's source node** — 13.7's non-optional third sort term and
//!   14.8's `Particle.emitter: NodeId` are the same identity, so no second binding can
//!   disagree with it. This is the reasoning [`crate::event`] applies to a cue's asset.
//! - **the birth position is the snapshot's transform for that node**, because
//!   `SimulationInput` is exactly the ordered per-node input 14.9's `SimulationInputFrame`
//!   is made of.
//! - **everything else is a constant in this file**, inside
//!   [`crate::SIMULATION_MODEL_VERSION`] and therefore inside every state digest. Not
//!   provisional: an authored field has to arrive with a schema bump regardless, and a
//!   constant whose *meaning* changed while its name did not is the failure WP04 built
//!   `EASING_ALGORITHM_VERSION` to make loud.
//!
//! ## The float discipline
//!
//! The working type is `f32`, because 14.8's particle fields are `f32`. The step duration is
//! the exact rational `1/(fps*N)` converted once in
//! [`crate::clock::SimulationClock::step_seconds`], and after that this file and
//! [`crate::springs`] use only `+ - * /` and `sqrt`. No trigonometry appears anywhere in
//! WP05, deliberately: `BUILD_STATE.md` risk 20 records that `f64::sin_cos` is a `libm` call
//! whose bits depend on a library the spec does not pin, whereas IEEE-754 requires binary32
//! `sqrt` to be correctly rounded. A particle's rotation is stored as an angle and never
//! turned into a matrix here — that is a WP06 shader's job.

use anim_core::hash::CanonicalBytes;
use anim_core::id::NodeId;
use anim_core::rational::Rational;
use anim_project::EventKind;
use anim_scene::SceneEvent;

use crate::mode::SimulationEffect;
use crate::random::{purpose, random_f32, system, STREAM_VERSION};
use crate::state::SimState;
use crate::{Result, SimError, SimTick, StreamKey, PARTICLE_ID_DOMAIN};

/// Particles spawned per signal event.
///
/// A burst of three rather than one, so one authored event exercises identity, ordering and
/// per-particle draws, and so 14.12's insertion-stability criterion has something it can be
/// unstable about. WP14 replaces this with an authored rate and burst; ADR-0021 is
/// registered for that decision.
pub const PARTICLES_PER_SIGNAL: u32 = 3;

/// Downward acceleration, in scene length units per second squared.
///
/// The sign states the engine's convention: +y is up. A `z` of zero is not an oversight —
/// WP05 simulates the 2D/2.5D scene it reads, and a z acceleration would be a dimension no
/// camera in spec 13 or 30 projects.
pub const GRAVITY_Y: f32 = -9.81;

/// Linear drag per second, applied as `v *= 1 - drag * dt`.
pub const DRAG_PER_SECOND: f32 = 0.6;

/// The speed a spawn draws between, units per second.
pub const SPEED_MIN: f32 = 0.8;
/// See [`SPEED_MIN`].
pub const SPEED_MAX: f32 = 2.4;

/// The shortest life a spawn can draw, in ticks.
///
/// Ticks rather than seconds, so the range means the same thing at every tick rate: a
/// lifetime authored in seconds would live a different number of steps in a 4-tick project
/// than in a 1-tick one, and `lifetime_ticks` says ticks.
pub const LIFETIME_MIN_TICKS: u64 = 12;
/// See [`LIFETIME_MIN_TICKS`].
pub const LIFETIME_MAX_TICKS: u64 = 96;

/// The smallest size a spawn can draw, scene length units.
pub const SIZE_MIN: f32 = 0.05;
/// See [`SIZE_MIN`].
pub const SIZE_MAX: f32 = 0.25;

/// The rotation range a spawn draws across, in turns.
pub const ROTATION_TURNS: f32 = 1.0;

/// The direction used when a drawn direction vector is exactly zero.
///
/// Reachable in principle — the three components are `2v - 1`, all zero when every draw
/// lands on exactly `0.5` — so it needs a defined answer, and `+y` is the one that does not
/// start a particle driving itself into the ground.
pub const FALLBACK_DIRECTION: [f32; 3] = [0.0, 1.0, 0.0];

/// One simulated particle (spec 14.8, field for field).
///
/// `generation` and `spawn_ordinal` are not bookkeeping but the stream-address inputs 14.7
/// names ("emitter/entity ID … deterministic spawn/particle ordinals"), which is why they
/// are part of the identity and never recovered from a cursor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Particle {
    /// Stable identity, from [`particle_id`], and the sort key of this vector.
    pub id: u128,
    /// The node that emitted it — the scheduling event's source, unchanged in identity.
    pub emitter: NodeId,
    /// How many times this emitter has spawned before this particle, from
    /// [`SimState::next_generation`].
    pub generation: u32,
    /// Position within this generation's burst: `0..PARTICLES_PER_SIGNAL`.
    pub spawn_ordinal: u32,
    /// Current position.
    pub position: [f32; 3],
    /// Position as of the previous step, which with [`Particle::position`] is the pair
    /// 14.8's `SimulationFrame` and 15.x's velocity pass both read.
    pub previous_position: [f32; 3],
    /// Velocity, units per second.
    pub velocity: [f32; 3],
    /// Steps lived.
    pub age_ticks: u64,
    /// Steps this particle lives for, drawn at spawn.
    pub lifetime_ticks: u64,
    /// Drawn size, units.
    pub size: f32,
    /// Drawn rotation, turns.
    pub rotation: f32,
}

impl Particle {
    /// Whether this particle has reached the end of its life.
    #[must_use]
    pub const fn is_expired(&self) -> bool {
        self.age_ticks >= self.lifetime_ticks
    }

    /// The stream one of this particle's attribute channels draws from (spec 14.7).
    ///
    /// The address pairs emitter with generation and ordinal, so two bursts from one
    /// emitter never share a number and one particle never shares a number with its own next
    /// tick.
    #[must_use]
    pub fn stream(&self, seed: [u8; anim_project::PROJECT_SEED_BYTES], channel: u16) -> StreamKey {
        StreamKey {
            project_seed: seed,
            system_id: system::PARTICLES,
            entity_id: self.emitter,
            purpose: channel,
            stream_version: STREAM_VERSION,
        }
    }

    /// Every value a digest must not be computed over blindly (spec 14.7, 14.11).
    ///
    /// # Errors
    ///
    /// [`SimError::NonFiniteState`] naming the field that failed.
    pub fn validate_finite(&self) -> Result<()> {
        let subject = format!("particle {}", self.id);
        let fields = [
            ("position[0]", self.position[0]),
            ("position[1]", self.position[1]),
            ("position[2]", self.position[2]),
            ("previous_position[0]", self.previous_position[0]),
            ("previous_position[1]", self.previous_position[1]),
            ("previous_position[2]", self.previous_position[2]),
            ("velocity[0]", self.velocity[0]),
            ("velocity[1]", self.velocity[1]),
            ("velocity[2]", self.velocity[2]),
            ("size", self.size),
            ("rotation", self.rotation),
        ];
        for (name, value) in fields {
            if !value.is_finite() {
                return Err(SimError::NonFiniteState {
                    subject: subject.clone(),
                    field: name,
                });
            }
        }
        Ok(())
    }

    /// The canonical field stream.
    ///
    /// Floats go in as **bit patterns, little-endian, after the finite check** — 14.7's
    /// rule, and the reason this does not route through
    /// [`anim_core::hash::CanonicalBytes::f64`]: widening a `f32` to `f64` to hash it would
    /// make one particle have two digests depending on which type a caller happened to hold,
    /// and only the bit pattern survives a checkpoint round trip unchanged.
    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) -> Result<()> {
        self.validate_finite()?;
        // A u128 id goes in as its 16 raw little-endian bytes rather than through a
        // `u128` writer, because CanonicalBytes has none and widening to i128 would drop
        // the top bit of half the ids this function produces. Fixed-width fields are raw
        // by ADR-0004's rule, and 16 bytes is the width of the type.
        cp.fixed_bytes(&self.id.to_le_bytes());
        cp.fixed_bytes(self.emitter.as_bytes());
        cp.u32(self.generation);
        cp.u32(self.spawn_ordinal);
        for value in self.position {
            cp.u32(value.to_bits());
        }
        for value in self.previous_position {
            cp.u32(value.to_bits());
        }
        for value in self.velocity {
            cp.u32(value.to_bits());
        }
        cp.u64(self.age_ticks);
        cp.u64(self.lifetime_ticks);
        cp.u32(self.size.to_bits());
        cp.u32(self.rotation.to_bits());
        Ok(())
    }
}

/// A particle's stable identity: emitter, generation and ordinal, hashed (spec 14.7, 14.8).
///
/// Derived rather than counted, on the no-entropy rule the whole workspace follows
/// (DECISIONS.md #23). A counter would make an id depend on the order the simulation
/// happened to append the particle, which is the history dependence 14.1 forbids, and would
/// make 14.12's insertion-stability criterion untestable: two runs that built the same three
/// particles in different orders have to land on the same three ids and one sorted vector.
#[must_use]
pub fn particle_id(emitter: &NodeId, generation: u32, spawn_ordinal: u32) -> u128 {
    // `with_domain` is `new()` plus a length-prefixed `text`, so an id derivation
    // whose signature is infallible can spell it directly instead of unwrapping a
    // constant it already knows is valid ASCII. The bytes are identical; see
    // anim_core::hash::CanonicalBytes::with_domain.
    let mut cp = CanonicalBytes::new();
    cp.text(PARTICLE_ID_DOMAIN);
    cp.fixed_bytes(emitter.as_bytes());
    cp.u32(generation);
    cp.u32(spawn_ordinal);
    let digest = cp.digest();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    u128::from_le_bytes(bytes)
}

/// The burst one signal event scheduled at this tick produces, already age-integrated.
///
/// The subframe age is applied here rather than left to the next tick, which is 14.7's
/// instruction — "compute exact `age = time_of_tick(k) - t`; convert once to working
/// precision and **use the same force function** to initialize its state". Running the age
/// through [`integrate`] is what makes a particle born 0.3 ticks into a frame occupy the
/// place a particle born at the authored instant would have reached, instead of lagging a
/// fraction of a tick for its entire life.
///
/// The age is initialization only: `age_ticks` stays zero and the normal lifetime stage
/// counts this tick like any other, because `lifetime_ticks` is measured in steps and the
/// age is not a step.
///
/// # Errors
///
/// [`SimError::UnknownEventTarget`] for a negative age (14.7 calls that an internal error)
/// or for a source node with no reserved generation, [`RandomError`]-derived address
/// failures from the draws, and [`SimError::DuplicateId`] if the burst collided with an id
/// already in the vector.
pub fn spawn_for_event(
    state: &mut SimState,
    event: &SceneEvent,
    age: Rational,
    birth: [f32; 3],
    seed: [u8; anim_project::PROJECT_SEED_BYTES],
) -> Result<()> {
    if age < Rational::from_i128(0) {
        return Err(SimError::UnknownEventTarget {
            event: event.occurrence_id.to_canonical_string(),
            detail: format!(
                "spawn age {age} is negative: the scheduled tick precedes the authored time, \
                 so the event would take effect before it was authored to"
            ),
        });
    }
    let generation = *state.next_generation.get(&event.source_id).ok_or_else(|| {
        SimError::UnknownEventTarget {
            event: event.occurrence_id.to_canonical_string(),
            detail: "the source node has no reserved generation counter, so initialization \
                     and this snapshot disagree about which nodes are emitters"
                .to_string(),
        }
    })?;

    // 14.7's "convert once to working precision": the exact rational is converted here and
    // only here, and the newborn carries the result rather than a second derivation of it.
    // The conversion is `Rational::to_f64` -- correctly rounded by the language definition --
    // narrowed once to the working type, which is stable rather than single-rounding; see
    // `BUILD_STATE.md` risk 20 for why the narrowing is not the hazard the trig call is.
    let age_seconds = age.to_f64() as f32;
    let mut burst = Vec::with_capacity(usize::try_from(PARTICLES_PER_SIGNAL).unwrap_or(0));
    for ordinal in 0..PARTICLES_PER_SIGNAL {
        let mut particle = Particle {
            id: particle_id(&event.source_id, generation, ordinal),
            emitter: event.source_id,
            generation,
            spawn_ordinal: ordinal,
            position: birth,
            previous_position: birth,
            velocity: [0.0; 3],
            age_ticks: 0,
            lifetime_ticks: 0,
            size: 0.0,
            rotation: 0.0,
        };
        draw_attributes(&mut particle, seed)?;
        integrate(&mut particle, age_seconds);
        burst.push(particle);
    }

    state.particles.extend(burst);
    sort_by_id(&mut state.particles);
    check_unique_ids(&state.particles)?;

    // The reservation advances only after the burst is in, so any failure above leaves the
    // counter exactly where it was rather than skipping a generation.
    if let Some(counter) = state.next_generation.get_mut(&event.source_id) {
        *counter = counter
            .checked_add(1)
            .ok_or_else(|| SimError::TickOverflow {
                operation: "generation advance",
                value: format!("emitter {} at u32::MAX", event.source_id),
            })?;
    }
    Ok(())
}

/// The four attribute draws of one new particle, one address per semantic channel.
///
/// Separate channels rather than four lanes of one address because spec 29 names the
/// attributes separately (`direction, life, size, …`) and 14.12's "changing one stream
/// version must leave unrelated stream outputs unchanged" is only a meaningful test when the
/// channels really are independent streams.
///
/// # Errors
///
/// [`SimError`] for any draw this address cannot serve. There is no fallback value here on
/// purpose: a swallowed draw would look like an animation that had merely frozen, and the
/// whole reason the draw is an address function is that a failure is a fact rather than a
/// nuisance.
fn draw_attributes(
    particle: &mut Particle,
    seed: [u8; anim_project::PROJECT_SEED_BYTES],
) -> Result<()> {
    let draw = particle.spawn_ordinal;
    // The tick component of a spawn's address is its generation, not the current tick: two
    // bursts of one emitter must not reuse each other's numbers, and the ordinal is already
    // what separates the particles inside one burst.
    let tick = SimTick(i64::from(particle.generation));
    let direction_channel = purpose::PARTICLE_DIRECTION;

    let mut direction = [0.0f32; 3];
    for (index, component) in direction.iter_mut().enumerate() {
        *component =
            2.0 * particle_draw(particle, seed, direction_channel, draw, tick, index as u8)? - 1.0;
    }
    let length_squared: f32 = direction.iter().map(|value| value * value).sum();
    let direction = if length_squared == 0.0 {
        FALLBACK_DIRECTION
    } else {
        let length = length_squared.sqrt();
        [
            direction[0] / length,
            direction[1] / length,
            direction[2] / length,
        ]
    };

    let speed = SPEED_MIN
        + particle_draw(particle, seed, purpose::PARTICLE_SPEED, draw, tick, 0)?
            * (SPEED_MAX - SPEED_MIN);
    particle.velocity = [
        direction[0] * speed,
        direction[1] * speed,
        direction[2] * speed,
    ];

    // The span is 84 ticks, so the conversion to f64 is exact and the narrowing below is a
    // floor of a value in 0..84 -- never 84 itself, which is why the range stays half-open.
    let lifetime_span =
        f64::from(u32::try_from(LIFETIME_MAX_TICKS - LIFETIME_MIN_TICKS).unwrap_or(u32::MAX));
    let fraction = particle_draw(particle, seed, purpose::PARTICLE_LIFETIME, draw, tick, 0)?;
    particle.lifetime_ticks = LIFETIME_MIN_TICKS + (f64::from(fraction) * lifetime_span) as u64;

    particle.size = SIZE_MIN
        + particle_draw(particle, seed, purpose::PARTICLE_SIZE, draw, tick, 0)?
            * (SIZE_MAX - SIZE_MIN);
    particle.rotation =
        particle_draw(particle, seed, purpose::PARTICLE_ROTATION, draw, tick, 0)? * ROTATION_TURNS;
    Ok(())
}

/// One `[0,1)` draw from one of this particle's channels.
fn particle_draw(
    particle: &Particle,
    seed: [u8; anim_project::PROJECT_SEED_BYTES],
    channel: u16,
    draw: u32,
    tick: SimTick,
    lane: u8,
) -> Result<f32> {
    let key = particle.stream(seed, channel);
    random_f32(&key, tick, draw, lane).map_err(SimError::from)
}

/// The drag multiplier for one step, bounded below by zero.
///
/// A `dt` large enough to make `1 - drag * dt` non-positive would flip the sign of every
/// velocity in the scene and still produce a finite, plausible, wrong state — the loud
/// failure over silent difference that DECISIONS.md #18 argues in the same terms about
/// adapter identity. Clamping is the defined answer, and the test that the clamp only
/// engages at a rate no project uses is
/// [`tests::drag_clamps_only_at_a_rate_no_step_duration_reaches`].
#[must_use]
pub fn drag_factor(dt_seconds: f32) -> f32 {
    (1.0 - DRAG_PER_SECOND * dt_seconds).max(0.0)
}

/// One step of one particle: gravity, then drag, then position.
///
/// Fixed order, per 29's "Apply gravity, wind, curl noise, drag and collision in fixed
/// order", of which WP05 has two of the five. `previous_position` is written before the
/// position moves, which is what makes the pair a velocity the renderer can use without a
/// second stored value.
///
/// This function is the *same* one [`spawn_for_event`] uses for the subframe age, per 14.7,
/// and it consumes no random value and hashes nothing — 14.6's "rendering, hashing and
/// checkpointing do not mutate simulation state or consume random values" is checkable
/// because the arithmetic lives here and nowhere else.
pub fn integrate(particle: &mut Particle, dt_seconds: f32) {
    particle.previous_position = particle.position;
    particle.velocity[1] += GRAVITY_Y * dt_seconds;
    let drag = drag_factor(dt_seconds);
    for component in &mut particle.velocity {
        *component *= drag;
    }
    for (index, component) in particle.velocity.iter().enumerate() {
        particle.position[index] += component * dt_seconds;
    }
}

/// One lifetime step (spec 14.6's seventh stage).
///
/// # Errors
///
/// [`SimError::TickOverflow`] if an age cannot advance, which a restored or hand-built state
/// with `age_ticks` near `u64::MAX` can reach. Never wraps (spec 10.11).
pub fn advance_lifetimes(state: &mut SimState) -> Result<()> {
    for particle in &mut state.particles {
        particle.age_ticks =
            particle
                .age_ticks
                .checked_add(1)
                .ok_or_else(|| SimError::TickOverflow {
                    operation: "particle age",
                    value: format!("particle {}", particle.id),
                })?;
    }
    Ok(())
}

/// The stable removal 14.6 names as the eighth and last stage.
///
/// `retain` is the stable filter: it preserves the relative order of what it keeps, so
/// removal cannot reorder the vector and a vector that dropped an expired particle hashes
/// the same as one that never held it. That is the property 14.12's
/// insertion/removal-stability criterion is checking, and it is why this is a separate stage
/// rather than a condition inside [`advance_lifetimes`]: a particle that expires and one
/// that was never spawned must leave the same vector behind.
pub fn remove_expired(state: &mut SimState) {
    state.particles.retain(|particle| !particle.is_expired());
}

/// Sort into `id` order (14.8: "sorted by stable IDs").
///
/// `sort_by_key` rather than `sort_unstable_by_key` is not a performance choice: an unstable
/// sort may permute equal keys, and two particles with one id is exactly the
/// [`SimError::DuplicateId`] condition [`check_unique_ids`] reports, so the sort must not be
/// the thing that decides which of two identical ids comes first.
pub fn sort_by_id(particles: &mut [Particle]) {
    particles.sort_by_key(|particle| particle.id);
}

/// Report a repeated identity instead of hashing two objects as one vector.
///
/// This is the producer 14.11's `DuplicateId` needs without a fake call site: a checkpoint
/// that decoded into a state holding two particles with one id is corrupt in a way the
/// digest cannot see, because both entries encode identically and the hash matches a
/// legitimate state.
///
/// # Errors
///
/// [`SimError::DuplicateId`] for the first repeated id in an id-sorted slice.
pub fn check_unique_ids(particles: &[Particle]) -> Result<()> {
    for pair in particles.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(SimError::DuplicateId {
                kind: "particle",
                id: pair[0].id.to_string(),
            });
        }
    }
    Ok(())
}

/// The immutable per-tick extraction a renderer reads (spec 14.8's closing sentence).
///
/// 14.8 describes it in prose without fields and 15.3 is the first package that consumes it,
/// so this carries only what that sentence names — current and previous transforms plus
/// particle draw data — and nothing a consumer could recompute differently. The revision is
/// not here because 14.9's `extract_frame(state)` has no document to read it from, and a
/// frame that claimed a revision it could not see would be a guess.
#[derive(Clone, Debug, PartialEq)]
pub struct SimulationFrame {
    /// The tick this extraction is of.
    pub tick: SimTick,
    /// Every live particle, in stable-id order, with both positions.
    pub particles: Vec<Particle>,
    /// Current and previous position per spring body, with the node it belongs to, in
    /// [`SimState::bodies`] order.
    pub body_transforms: Vec<BodyTransform>,
    /// The presentation effects accumulated so far, empty in a mode that suppresses them
    /// (spec 14.7, 14.10).
    pub effects: Vec<SimulationEffect>,
}

/// One body's two positions and the node it stands for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyTransform {
    /// The body's node.
    pub node: NodeId,
    /// Where it is.
    pub position: [f32; 3],
    /// Where it was.
    pub previous_position: [f32; 3],
}

/// Build the extraction for one state (spec 14.9's `extract_frame`).
///
/// Read-only, order-preserving, consuming nothing: extracting twice gives two equal values
/// and changes no digest, which is what lets 14.12's cadence tests compare runs that extract
/// every tick against runs that extract none.
#[must_use]
pub fn extract_frame(state: &SimState) -> SimulationFrame {
    SimulationFrame {
        tick: state.tick,
        particles: state.particles.to_vec(),
        body_transforms: state
            .bodies
            .iter()
            .map(|body| BodyTransform {
                node: body.id,
                position: body.position,
                previous_position: body.previous_position,
            })
            .collect(),
        effects: state.presentation.effects.clone(),
    }
}

/// The authored position of each node at this tick, as the working type.
///
/// Built from 13.7's visit order rather than trusted from it: `SimulationInput` arrives in
/// the evaluator's traversal order, and both a spawn and a spring need their node's position
/// by identity, so the lookup is keyed on the id and the traversal order stops mattering. A
/// duplicated node in a snapshot would be `anim_scene`'s defect rather than this crate's, so
/// the first entry wins and the result does not depend on which duplicate was visited second.
///
/// The translation column is `matrix[i][2]`, which is the 2D-affine form
/// [`anim_scene::animation`] writes and the same three values
/// [`crate::springs::initial_chains`] measures a rest length from.
#[must_use]
pub fn anchor_positions(
    inputs: &[anim_scene::SimulationInput],
) -> std::collections::BTreeMap<anim_core::id::NodeId, [f32; 3]> {
    let mut anchors = std::collections::BTreeMap::new();
    for input in inputs {
        let matrix = &input.world_transform;
        let position = [
            matrix[0][2] as f32,
            matrix[1][2] as f32,
            matrix[2][2] as f32,
        ];
        anchors.entry(input.node).or_insert(position);
    }
    anchors
}

/// The effect a cue-kind event announces, withheld in a suppressing mode (spec 14.7).
///
/// Only [`EventKind::AudioCue`] and [`EventKind::CameraCut`] are presentation events; a
/// [`EventKind::Signal`] is a spawn instruction consumed by [`spawn_for_event`]. Keeping the
/// three distinguished here is what stops one authored event from being both a spawn and a
/// cue, which would give 13.10's audio conversion two drivers.
pub fn presentation_effect(
    event: &SceneEvent,
    tick: SimTick,
    suppress: bool,
) -> Option<SimulationEffect> {
    if suppress {
        return None;
    }
    match event.kind {
        EventKind::Signal => None,
        EventKind::AudioCue | EventKind::CameraCut => Some(SimulationEffect {
            tick,
            occurrence: event.occurrence_id,
            source: event.source_id,
            kind: event.kind,
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use crate::clock::SimulationClock;
    use anim_core::time::FrameRate;

    fn node(label: &str) -> NodeId {
        NodeId::derive(
            &uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            label.as_bytes(),
        )
    }

    fn clock() -> SimulationClock {
        SimulationClock::new(FrameRate::new(24, 1).unwrap(), 4).unwrap()
    }

    fn particle(emitter: &NodeId, generation: u32, ordinal: u32) -> Particle {
        Particle {
            id: particle_id(emitter, generation, ordinal),
            emitter: *emitter,
            generation,
            spawn_ordinal: ordinal,
            position: [0.0; 3],
            previous_position: [0.0; 3],
            velocity: [0.0; 3],
            age_ticks: 0,
            lifetime_ticks: 40,
            size: 0.1,
            rotation: 0.0,
        }
    }

    #[test]
    fn a_particle_id_is_a_function_of_its_address_only() {
        let emitter = node("emitter");
        let first = particle_id(&emitter, 0, 0);
        assert_eq!(first, particle_id(&emitter, 0, 0));
        assert_ne!(first, particle_id(&emitter, 1, 0));
        assert_ne!(first, particle_id(&emitter, 0, 1));
        assert_ne!(first, particle_id(&node("other"), 0, 0));
    }

    #[test]
    fn insertion_order_does_not_change_the_sorted_vector() {
        // 14.12's insertion stability in its particle form, and the payoff for deriving the
        // id instead of counting it.
        let emitter = node("emitter");
        let burst: Vec<Particle> = (0..PARTICLES_PER_SIGNAL)
            .map(|ordinal| particle(&emitter, 7, ordinal))
            .collect();
        let mut forward = burst.clone();
        sort_by_id(&mut forward);
        let mut reversed: Vec<Particle> = burst.iter().rev().cloned().collect();
        sort_by_id(&mut reversed);
        assert_eq!(forward, reversed);
        check_unique_ids(&forward).expect("a burst has distinct ids");
    }

    #[test]
    fn a_repeated_identity_is_reported_rather_than_hashed_as_one() {
        let emitter = node("emitter");
        let one = particle(&emitter, 0, 0);
        assert!(matches!(
            check_unique_ids(&[one, one]),
            Err(SimError::DuplicateId {
                kind: "particle",
                ..
            })
        ));
        let mut second = one;
        second.id = one.id ^ 1;
        let ordered = [one, second];
        sort_by_id(&mut ordered.to_vec());
        check_unique_ids(&[one, second]).expect("two distinct ids are a legal vector");
        check_unique_ids(&[]).expect("an empty vector has nothing to repeat");
    }

    #[test]
    fn a_non_finite_particle_is_named_and_not_encoded() {
        let emitter = node("emitter");
        let mut sample = particle(&emitter, 0, 0);
        sample.velocity = [f32::NAN, 0.0, 0.0];
        let Err(SimError::NonFiniteState { field, .. }) = sample.validate_finite() else {
            panic!("14.11 makes a non-finite state an explicit error");
        };
        assert_eq!(field, "velocity[0]", "and the report says which value");
        let mut cp = CanonicalBytes::new();
        assert!(sample.encode(&mut cp).is_err());

        sample.velocity = [0.0, f32::INFINITY, 0.0];
        assert!(matches!(
            sample.validate_finite(),
            Err(SimError::NonFiniteState {
                field: "velocity[1]",
                ..
            })
        ));
        sample.previous_position = [0.0, 0.0, f32::NEG_INFINITY];
        assert!(matches!(
            sample.validate_finite(),
            Err(SimError::NonFiniteState {
                field: "previous_position[2]",
                ..
            })
        ));
    }

    #[test]
    fn floats_are_encoded_as_little_endian_bit_patterns() {
        // 14.7's "encode floating-point bit patterns little-endian after finite validation",
        // checked as bytes rather than as one function agreeing with another.
        let emitter = node("emitter");
        let mut sample = particle(&emitter, 3, 1);
        sample.position = [1.5, 2.5, 3.5];
        sample.previous_position = [4.5, 5.5, 6.5];
        sample.velocity = [7.5, 8.5, 9.5];
        sample.age_ticks = 7;
        sample.lifetime_ticks = 8;
        sample.size = 0.5;
        sample.rotation = 0.25;
        let mut cp = CanonicalBytes::new();
        sample.encode(&mut cp).unwrap();
        let bytes = cp.as_slice();
        assert_eq!(
            bytes.len(),
            16 + 16 + 4 + 4 + 9 * 4 + 8 + 8 + 4 + 4,
            "the width is arithmetic, not a measurement of the encoder"
        );
        assert_eq!(&bytes[40..44], &1.5f32.to_bits().to_le_bytes());
        assert_eq!(&bytes[48..52], &3.5f32.to_bits().to_le_bytes());
        assert_eq!(&bytes[52..56], &4.5f32.to_bits().to_le_bytes());
        // Offsets from the layout rule, not from where the encoder happened to put things:
        // 16 id + 16 emitter + 4 generation + 4 ordinal, then nine f32 words, then the two
        // u64 tick counts, then size and rotation.
        assert_eq!(&bytes[76..84], &7u64.to_le_bytes(), "age_ticks");
        assert_eq!(&bytes[84..92], &8u64.to_le_bytes(), "lifetime_ticks");
        assert_eq!(&bytes[92..96], &0.5f32.to_bits().to_le_bytes(), "size");
        assert_eq!(
            &bytes[96..100],
            &0.25f32.to_bits().to_le_bytes(),
            "rotation is the last field and every one of them is a bit pattern, not a decimal"
        );
        assert_eq!(
            bytes.len(),
            100,
            "and the arithmetic in the length check agrees"
        );
    }

    #[test]
    fn drag_clamps_only_at_a_rate_no_step_duration_reaches() {
        assert!(drag_factor(1.0 / 96.0) > 0.99);
        // The clamp engages only where 1 - 0.6*dt goes negative, i.e. dt > 1.666 s: a step
        // longer than a frame and a half.
        assert!(drag_factor(1.0) > 0.0);
        assert_eq!(drag_factor(1000.0), 0.0);
        let every_real_step = [
            SimulationClock::new(FrameRate::new(24000, 1001).unwrap(), 1)
                .unwrap()
                .step_seconds() as f32,
            SimulationClock::new(FrameRate::new(1, 1).unwrap(), 1)
                .unwrap()
                .step_seconds() as f32,
        ];
        for dt in every_real_step {
            assert!(
                drag_factor(dt) > 0.0,
                "any rate a project can author leaves drag unclamped: dt {dt}"
            );
        }
    }

    #[test]
    fn the_previous_position_is_the_position_before_the_step() {
        let emitter = node("emitter");
        let mut sample = particle(&emitter, 0, 0);
        sample.position = [4.0, 5.0, 6.0];
        sample.velocity = [1.0, 0.0, 0.0];
        let dt = clock().step_seconds() as f32;
        integrate(&mut sample, dt);
        assert_eq!(sample.previous_position, [4.0, 5.0, 6.0]);
        assert!(sample.position[0] > 4.0);
        assert!(
            sample.position[1] < 5.0,
            "gravity takes y down, and 14.8's previous/current pair is what a renderer \
             reads as motion: {:?}",
            sample.position
        );
        let recovered = (sample.position[0] - sample.previous_position[0]) / dt;
        assert!(
            (recovered - sample.velocity[0]).abs() < 0.01,
            "the pair is the velocity, without a third stored value"
        );
    }

    #[test]
    fn expiry_removes_in_place() {
        // 14.6's "removes expired entries by stable filtering": a filter may drop entries and
        // must not permute the ones it keeps. The expired particle goes in the middle of the
        // sorted order rather than at an end, because an end position is indistinguishable
        // from a rotation, and a rotation is the defect being ruled out.
        let emitter = node("emitter");
        let mut keep_first = particle(&emitter, 0, 0);
        keep_first.lifetime_ticks = 9;
        let mut drop_middle = particle(&emitter, 0, 1);
        drop_middle.lifetime_ticks = 1;
        let mut keep_last = particle(&emitter, 0, 2);
        keep_last.lifetime_ticks = 9;
        let mut state = crate::state::test_support::state();
        // Arrival order deliberately unlike the sorted order.
        state.particles.push(keep_last);
        state.particles.push(drop_middle);
        state.particles.push(keep_first);
        sort_by_id(&mut state.particles);
        let before: Vec<u128> = state.particles.iter().map(|p| p.id).collect();
        assert_eq!(before.len(), 3);
        let dropped = drop_middle.id;
        let expected: Vec<u128> = before.iter().copied().filter(|id| *id != dropped).collect();
        assert_eq!(
            expected.len(),
            2,
            "the middle of the sorted order is the one going"
        );

        for _ in 0..2 {
            advance_lifetimes(&mut state).unwrap();
            remove_expired(&mut state);
        }
        let after: Vec<u128> = state.particles.iter().map(|p| p.id).collect();
        assert_eq!(
            after, expected,
            "two survivors, in the order they were already in"
        );

        // And a vector that never held the expired particle is indistinguishable from the one
        // that dropped it, which is the property a checkpoint digest depends on: one world has
        // one byte stream, however it got there.
        let mut never = crate::state::test_support::state();
        never.particles.push(keep_first);
        never.particles.push(keep_last);
        sort_by_id(&mut never.particles);
        for _ in 0..2 {
            advance_lifetimes(&mut never).unwrap();
            remove_expired(&mut never);
        }
        assert_eq!(
            never.particles, state.particles,
            "same two survivors, same ages, same order -- one world, one byte stream"
        );
    }

    #[test]
    fn an_age_that_would_run_backwards_is_refused() {
        let emitter = node("emitter");
        let mut state = crate::state::test_support::state();
        state.next_generation.insert(emitter, 0);
        let event = SceneEvent {
            occurrence_id: anim_core::id::EventOccurrenceId::derive(
                &uuid::Uuid::nil(),
                b"occurrence",
            ),
            time: anim_core::time::FramePos::at_frame(3),
            priority: 0,
            source_id: emitter,
            local_sequence: 0,
            kind: EventKind::Signal,
        };
        let negative = Rational::new(-1, 24).unwrap();
        let Err(SimError::UnknownEventTarget { detail, .. }) = spawn_for_event(
            &mut state,
            &event,
            negative,
            [0.0; 3],
            [0u8; anim_project::PROJECT_SEED_BYTES],
        ) else {
            panic!("14.7 calls a negative age an internal error, so it has to be reported");
        };
        assert!(
            detail.contains("negative"),
            "and the report says which: {detail}"
        );
        assert!(
            state.particles.is_empty(),
            "and nothing was spawned by the refused event"
        );
    }

    #[test]
    fn an_emitter_with_no_reservation_is_refused() {
        let emitter = node("unreserved");
        let mut state = crate::state::test_support::state();
        let event = SceneEvent {
            occurrence_id: anim_core::id::EventOccurrenceId::derive(&uuid::Uuid::nil(), b"x"),
            time: anim_core::time::FramePos::at_frame(0),
            priority: 0,
            source_id: emitter,
            local_sequence: 0,
            kind: EventKind::Signal,
        };
        assert!(matches!(
            spawn_for_event(
                &mut state,
                &event,
                Rational::from_i128(0),
                [0.0; 3],
                [0u8; anim_project::PROJECT_SEED_BYTES],
            ),
            Err(SimError::UnknownEventTarget { .. })
        ));
    }

    #[test]
    fn a_burst_lands_in_the_reserved_range_of_every_draw() {
        let emitter = node("emitter");
        let seed = [7u8; anim_project::PROJECT_SEED_BYTES];
        for generation in 0..12u32 {
            for ordinal in 0..PARTICLES_PER_SIGNAL {
                let mut sample = particle(&emitter, generation, ordinal);
                draw_attributes(&mut sample, seed).unwrap();
                assert!(
                    (0.0..1.0).contains(&sample.rotation),
                    "rotation is drawn across one turn: {}",
                    sample.rotation
                );
                assert!(
                    sample.size >= SIZE_MIN && sample.size < SIZE_MAX,
                    "size is inside its authored range: {}",
                    sample.size
                );
                assert!(
                    sample.lifetime_ticks >= LIFETIME_MIN_TICKS
                        && sample.lifetime_ticks < LIFETIME_MAX_TICKS,
                    "lifetime is inside its authored range: {}",
                    sample.lifetime_ticks
                );
                let speed = (sample.velocity[0] * sample.velocity[0]
                    + sample.velocity[1] * sample.velocity[1]
                    + sample.velocity[2] * sample.velocity[2])
                    .sqrt();
                assert!(
                    (SPEED_MIN..=SPEED_MAX + 1e-4).contains(&speed),
                    "a normalized direction times a bounded speed: {speed}"
                );
                sample.validate_finite().expect("a draw is always finite");
            }
        }
    }

    #[test]
    fn only_cue_events_are_presentation_effects() {
        let emitter = node("hero");
        let event = SceneEvent {
            occurrence_id: anim_core::id::EventOccurrenceId::derive(&uuid::Uuid::nil(), b"o"),
            time: anim_core::time::FramePos::at_frame(3),
            priority: 0,
            source_id: emitter,
            local_sequence: 0,
            kind: EventKind::Signal,
        };
        assert!(presentation_effect(&event, SimTick(12), false).is_none());
        for kind in [EventKind::AudioCue, EventKind::CameraCut] {
            let cued = SceneEvent { kind, ..event };
            assert!(presentation_effect(&cued, SimTick(12), false).is_some());
            assert!(
                presentation_effect(&cued, SimTick(12), true).is_none(),
                "suppressed modes announce nothing at all ({kind})"
            );
        }
    }
}
