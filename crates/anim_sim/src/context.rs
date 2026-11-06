//! The simulation's own view of one document: its inputs, its schedule and its cache.
//!
//! Spec 14.3 lists eleven files for this crate and none of them is this one, yet 14.9 defines
//! two types that file does not place: `SimulationContext`, described in prose as owning
//! "immutable project/evaluation inputs and the checkpoint cache", and
//! `SimulationInputFrame`, described as "the ordered inputs for one tick". A type 14.9 names
//! and 14.3 does not locate has to live somewhere, and the twelfth file is the smallest
//! departure from the listing — the alternative was to put the driver for every other module
//! inside one of them and make `state.rs` or `replay.rs` mean two things. Recorded as a
//! deviation row in `BUILD_STATE.md`.
//!
//! ## Why the context takes `&mut` where 14.9 writes `&`
//!
//! 14.9's signatures read `seek(context: &SimulationContext, …)` and
//! `prepare_export(context: &SimulationContext, …)`. Both must reach a snapshot through
//! [`anim_scene::SnapshotCache`], and that cache is *owned and mutable* rather than locked —
//! a deliberate WP04 decision, because `clippy.toml` denies `Mutex` and `RwLock`
//! workspace-wide and DECISIONS.md #22 prefers an owned invariant over an interior one. A
//! shared `&` context therefore cannot produce a snapshot at all without either a lock or a
//! cache that never fills.
//!
//! The parameter is `&mut` instead, which keeps the cache the single-owner design WP04 built
//! and keeps 14.1's "results must not depend on cache cadence" honest: the cache visibly
//! changes, so the test that says it must not change the answer is testing something. This is
//! the same class of adjustment as deviation row 11's two field changes, made in the
//! direction the rest of the engine already points rather than toward a signature that would
//! need a lock to satisfy.

use std::sync::Arc;

use anim_core::id::SceneId;
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::ProjectDocument;
use anim_scene::{SimulationInput, SnapshotCache};

use crate::checkpoint::CheckpointCache;
use crate::clock::SimulationClock;
use crate::events::{ScheduledEvent, TickSchedule};
use crate::mode::SimulationMode;
use crate::state::{initial_state, SimSettings, SimState};
use crate::{Result, SimError, SimTick};

/// The ordered inputs for one tick (spec 14.9).
///
/// Immutable by construction: every field is a `Vec`/`Arc` that [`crate::step::step_once`]
/// only reads, and `SimulationMode` is the only other argument a step takes, which is how
/// 14.9's "controls external side effects only" stays checkable.
///
/// `project_seed` is document configuration rather than an input for one tick, and it is here
/// because [`crate::step::step_once`] has three parameters and no other channel for it. The
/// alternative — a copy inside `SimState` — would give one authored value two digests.
#[derive(Clone, Debug, PartialEq)]
pub struct SimulationInputFrame {
    /// The tick these inputs are for, which must equal the state's.
    pub tick: SimTick,
    /// The timeline position of that tick.
    pub position: FramePos,
    /// Its exact time, from the clock rather than accumulated.
    pub time: Rational,
    /// This tick's evaluated per-node inputs, in 13.7's visit order.
    pub anchors: Arc<[SimulationInput]>,
    /// The events scheduled at this tick, in 13.7's canonical order.
    pub scheduled: Arc<[ScheduledEvent]>,
    /// The document's random-stream seed (spec 14.7).
    pub project_seed: [u8; anim_project::PROJECT_SEED_BYTES],
}

/// One document, one scene, and the caches between them and a steppable state (spec 14.9).
#[derive(Debug)]
pub struct SimulationContext<'a> {
    document: &'a ProjectDocument,
    scene_id: SceneId,
    settings: SimSettings,
    schedule: TickSchedule,
    snapshots: SnapshotCache,
    checkpoints: CheckpointCache,
}

impl<'a> SimulationContext<'a> {
    /// Bind a context to one document and scene, with one checkpoint policy.
    ///
    /// The schedule is compiled here, over the whole scene, rather than gathered as the run
    /// proceeds: see [`crate::events`]'s module comment for why that is the difference between
    /// a seek that agrees with sequential execution and one that cannot.
    ///
    /// # Errors
    ///
    /// [`SimError::InvalidRate`] for settings this clock refuses, and [`SimError::Scene`] for
    /// an event whose source node the scene does not have.
    pub fn new(
        document: &'a ProjectDocument,
        scene_id: SceneId,
        checkpoints: CheckpointCache,
    ) -> Result<Self> {
        let settings = SimSettings::new(document.frame_rate, &document.simulation)?;
        let scene = document.scene(&scene_id).ok_or_else(|| {
            SimError::Scene(anim_scene::SceneError::MissingScene {
                scene: scene_id.to_canonical_string(),
            })
        })?;
        let schedule = TickSchedule::for_scene(&settings.clock, scene)?;
        Ok(Self {
            document,
            scene_id,
            settings,
            schedule,
            snapshots: SnapshotCache::new(),
            checkpoints,
        })
    }

    /// The clock every conversion in this run goes through.
    #[must_use]
    pub fn clock(&self) -> &SimulationClock {
        &self.settings.clock
    }

    /// A copy of the clock, for a caller that needs it without borrowing the context.
    #[must_use]
    pub fn clock_value(&self) -> SimulationClock {
        self.settings.clock
    }

    #[must_use]
    pub const fn settings(&self) -> &SimSettings {
        &self.settings
    }

    #[must_use]
    pub const fn scene_id(&self) -> SceneId {
        self.scene_id
    }

    #[must_use]
    pub const fn frame_rate(&self) -> FrameRate {
        self.document.frame_rate
    }

    /// The compiled event schedule, read by anything that wants to know which ticks have work.
    #[must_use]
    pub const fn schedule(&self) -> &TickSchedule {
        &self.schedule
    }

    /// The document this context reads. Exposed for a report that has to name a revision
    /// without holding the whole context borrowed.
    #[must_use]
    pub const fn document(&self) -> &ProjectDocument {
        self.document
    }

    #[must_use]
    pub const fn checkpoints(&self) -> &CheckpointCache {
        &self.checkpoints
    }

    /// The cache, mutably, which is what [`crate::seek`] needs in order to verify entries and
    /// discard the ones that fail.
    ///
    /// Public rather than private to `seek` because the discard *is* the policy 14.7 asks for,
    /// and a caller that wants to clear or reconfigure the cache between runs should be able to
    /// do it without going through a seek.
    #[must_use]
    pub const fn checkpoints_mut(&mut self) -> &mut CheckpointCache {
        &mut self.checkpoints
    }

    /// The cache's own counters, for the report 14.12's cadence comparison prints.
    #[must_use]
    pub fn cache_report(&self) -> CacheReport {
        CacheReport {
            snapshots: self.snapshots.len(),
            evaluations: self.snapshots.evaluations(),
            hits: self.snapshots.hits(),
            checkpoints: self.checkpoints.len(),
        }
    }

    /// Discard every cached snapshot and checkpoint.
    ///
    /// The one place a caller can say "assume nothing you remember", which is what WP04's
    /// cache-isolation-after-undo criterion asks for from a session's side. A state that
    /// already exists is not affected, and cannot be: it is a value, not a view.
    pub fn invalidate_caches(&mut self) {
        self.snapshots.clear();
        self.checkpoints.clear();
    }

    /// The snapshot for `tick`, cached.
    fn cached_snapshot(&mut self, tick: SimTick) -> Result<Arc<anim_scene::SceneSnapshot>> {
        let position = FramePos::at_frame(self.clock().frame_of_tick(tick));
        self.snapshots
            .snapshot(self.document, self.scene_id, position)
            .map_err(SimError::Scene)
    }

    /// The ordered inputs for one tick (spec 14.9's `SimulationInputFrame`).
    ///
    /// This is 14.6's "samples required snapshot data" stage made explicit: the snapshot is
    /// chosen by the tick being stepped, never by the last frame the caller happened to
    /// present, which is the sentence that makes render-rate independence a fact rather than a
    /// hope.
    ///
    /// # Errors
    ///
    /// The snapshot and clock errors above.
    pub fn input_at(&mut self, tick: SimTick) -> Result<SimulationInputFrame> {
        let snapshot = self.cached_snapshot(tick)?;
        let position = self.clock().position_of_tick(tick)?;
        let time = self.clock().time_of_tick(tick)?;
        Ok(SimulationInputFrame {
            tick,
            position,
            time,
            anchors: Arc::clone(&snapshot.simulation_inputs),
            scheduled: Arc::from(self.schedule.at(tick).to_vec()),
            project_seed: self.settings.project_seed,
        })
    }

    /// The state at [`SimTick::ZERO`], from the tick-zero snapshot (spec 14.9).
    ///
    /// # Errors
    ///
    /// [`initial_state`]'s errors and any evaluation failure at frame zero.
    pub fn initial_state(&mut self) -> Result<SimState> {
        let snapshot = self.cached_snapshot(SimTick::ZERO)?;
        initial_state(&snapshot, &self.settings)
    }

    /// Apply one tick to a state: build its input, step, and let the cache policy record.
    ///
    /// # Errors
    ///
    /// The [`crate::step::step_once`] and input errors.
    pub fn step(&mut self, state: &mut SimState, mode: SimulationMode) -> Result<()> {
        let input = self.input_at(state.tick)?;
        crate::step::step_once(state, &input, mode)?;
        self.checkpoints.record(state)?;
        Ok(())
    }

    /// Advance a state to `target`, one tick at a time, in `mode`.
    ///
    /// The one stepping loop in this crate. [`crate::seek`] and [`crate::replay`] both call
    /// it, which is how 14.5 step 9's "implement seek, warm-up and export preparation using
    /// the same stepping code" is a fact about the code rather than about the documentation.
    ///
    /// `budget` is 14.7's explicit limit. Exhausting it is an error rather than a reason to
    /// take bigger steps, and the reason the parameter exists at all is that a caller driving
    /// an interactive preview needs to know it is behind, not to be silently corrected.
    ///
    /// # Errors
    ///
    /// [`SimError::NegativeReplayTarget`] for a target below zero,
    /// [`SimError::ReplayBudgetExhausted`] when `budget` runs out first, and the [`Self::step`]
    /// errors.
    pub fn advance(
        &mut self,
        state: &mut SimState,
        target: SimTick,
        mode: SimulationMode,
        budget: Option<u64>,
    ) -> Result<()> {
        if target.is_negative() {
            return Err(SimError::NegativeReplayTarget { target: target.0 });
        }
        if state.tick > target {
            // There is no backward step in this engine, which is 14.7's "never restore a later
            // checkpoint or reuse future mutable state" read as a rule about `SimState` rather
            // than about caches: reaching an earlier tick means starting a run again, and a
            // loop that quietly did nothing would let a caller hand back a state from the
            // future while claiming it was the past. Same category, same report.
            return Err(SimError::NegativeReplayTarget { target: target.0 });
        }
        let mut remaining = budget.unwrap_or(u64::MAX);
        while state.tick < target {
            if remaining == 0 {
                return Err(SimError::ReplayBudgetExhausted {
                    budget: budget.unwrap_or(0),
                    target: target.0,
                });
            }
            self.step(state, mode)?;
            remaining -= 1;
        }
        Ok(())
    }
}

/// What the caches held at one moment, for a report.
///
/// 14.1's four independencies are only falsifiable if a run can say how much it cached, and
/// 14.12's cadence comparisons read these numbers to prove that two runs with the same answers
/// had deliberately different caches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheReport {
    /// Snapshot entries held.
    pub snapshots: usize,
    /// Evaluations actually performed.
    pub evaluations: u64,
    /// Cache hits served instead.
    pub hits: u64,
    /// Checkpoint entries held.
    pub checkpoints: usize,
}

impl CacheReport {
    /// Whether the two runs cached differently, which is the precondition for a cadence test
    /// having tested anything at all.
    #[must_use]
    pub fn differs_in_caching(&self, other: &Self) -> bool {
        (self.evaluations, self.checkpoints) != (other.evaluations, other.checkpoints)
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
    use crate::checkpoint::CheckpointCache;
    use anim_core::id::{EventDefinitionId, NodeId, ProjectId};
    use anim_project::{EventDefinition, EventKind, NodeData, SimulationSettings};

    fn namespace() -> uuid::Uuid {
        uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn scene_id() -> SceneId {
        SceneId::derive(&namespace(), b"shot01")
    }

    /// A document with a two-node hierarchy and one signal event, at four ticks per frame.
    fn document() -> ProjectDocument {
        let mut document = ProjectDocument::new(
            ProjectId::derive(&namespace(), b"wp05"),
            FrameRate::new(24, 1).unwrap(),
        );
        document.simulation =
            SimulationSettings::new(4, [3u8; anim_project::PROJECT_SEED_BYTES]).unwrap();
        let mut scene = anim_project::SceneData::new("shot01").unwrap();
        let root = NodeId::derive(&namespace(), b"root");
        let child = NodeId::derive(&namespace(), b"child");
        scene
            .insert_node(NodeData::new(root, None, 0, "root"))
            .unwrap();
        scene
            .insert_node(NodeData::new(child, Some(root), 0, "child"))
            .unwrap();
        scene
            .insert_event(EventDefinition::new(
                EventDefinitionId::derive(&namespace(), b"hit"),
                FramePos::new(1, Rational::new(1, 2).unwrap()).unwrap(),
                0,
                child,
                EventKind::Signal,
                0,
            ))
            .unwrap();
        assert!(document.insert_scene(scene_id(), scene).is_none());
        document
    }

    #[test]
    fn a_context_reads_the_settings_through_the_document() {
        let document = document();
        let context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        assert_eq!(context.clock().ticks_per_frame, 4);
        assert_eq!(
            context.settings().project_seed,
            [3u8; anim_project::PROJECT_SEED_BYTES]
        );
        assert_eq!(context.frame_rate(), FrameRate::new(24, 1).unwrap());
        assert_eq!(context.scene_id(), scene_id());
        assert_eq!(
            context.schedule().len(),
            1,
            "the one authored event is the whole schedule"
        );
    }

    #[test]
    fn a_missing_scene_is_the_evaluators_own_refusal() {
        let document = document();
        let other = SceneId::derive(&namespace(), b"absent");
        let error =
            SimulationContext::new(&document, other, CheckpointCache::default()).unwrap_err();
        assert!(
            matches!(
                error,
                SimError::Scene(anim_scene::SceneError::MissingScene { .. })
            ),
            "14.3's file list has no scene-not-found variant of its own, and 13.11 already \
             names one: {error}"
        );
    }

    #[test]
    fn the_initial_state_is_at_tick_zero_and_built_from_frame_zero() {
        let document = document();
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let state = context.initial_state().unwrap();
        assert_eq!(state.tick, SimTick::ZERO, "14.6's first sentence");
        assert_eq!(state.event_cursor, 0);
        assert!(state.particles.is_empty());
        assert_eq!(
            state.next_generation.len(),
            2,
            "a counter reserved for every node in the snapshot, which is 14.8's rule and is \
             why a node that never spawns still occupies an entry"
        );
        assert_eq!(
            state.bodies.len(),
            1,
            "only the child has a parent, so only the child is a body"
        );
        assert_eq!(state.springs.len(), 1);
    }

    #[test]
    fn the_input_for_a_tick_names_the_tick_and_its_frame() {
        let document = document();
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let input = context.input_at(SimTick(0)).unwrap();
        assert_eq!(input.tick, SimTick(0));
        assert_eq!(
            input.position,
            FramePos::at_frame(0),
            "tick 0 of four is in frame 0"
        );
        assert_eq!(input.anchors.len(), 2);
        assert_eq!(input.project_seed, [3u8; anim_project::PROJECT_SEED_BYTES]);
        assert!(
            input.scheduled.is_empty(),
            "frame 1.5 at four ticks per frame is tick 6 only for an event whose time \
             ceilings to 6 -- which this one does, so the assertion below is the one that \
             matters"
        );
    }

    #[test]
    fn an_event_lands_on_exactly_one_tick_of_the_schedule() {
        let document = document();
        let context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        // Frame 1 + 1/2 at 24 fps, four ticks per frame: t = 1.5/24 = 1/16 s, rate 96/s,
        // so t*rate = 6 exactly and the event schedules to tick 6 on the boundary.
        let scheduled = context.schedule().at(SimTick(6));
        assert_eq!(scheduled.len(), 1, "the boundary case keeps its own tick");
        assert_eq!(scheduled[0].age, Rational::from_i128(0));
        assert!(scheduled[0].spawns());

        // And the input frame for that tick carries it, which is the link between the
        // schedule the context compiled and the step the caller runs: a schedule nothing
        // reads is a second event rule, and 13.6 forbids one.
        let mut reading =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let input = reading.input_at(SimTick(6)).unwrap();
        assert_eq!(input.scheduled.len(), 1, "tick 6 is the event's tick");
        assert_eq!(input.scheduled[0].event, scheduled[0].event);
        assert!(reading.input_at(SimTick(7)).unwrap().scheduled.is_empty());
    }

    #[test]
    fn one_step_is_one_tick_and_the_cache_records_to_policy() {
        let document = document();
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::new(2, 8)).unwrap();
        let mut state = context.initial_state().unwrap();
        assert_eq!(context.checkpoints().len(), 0);
        context.step(&mut state, SimulationMode::Export).unwrap();
        assert_eq!(state.tick, SimTick(1));
        assert_eq!(
            context.checkpoints().len(),
            0,
            "tick 1 is not on a cadence of 2"
        );
        context.step(&mut state, SimulationMode::Export).unwrap();
        assert_eq!(context.checkpoints().len(), 1, "tick 2 is");
    }

    #[test]
    fn advancing_backward_is_refused_rather_than_ignored() {
        let document = document();
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let mut state = context.initial_state().unwrap();
        context
            .advance(&mut state, SimTick(5), SimulationMode::Export, None)
            .unwrap();
        assert_eq!(state.tick, SimTick(5));
        assert!(matches!(
            context.advance(&mut state, SimTick(2), SimulationMode::Export, None),
            Err(SimError::NegativeReplayTarget { .. })
        ));
        assert_eq!(
            state.tick,
            SimTick(5),
            "14.7: never restore a later checkpoint or reuse future mutable state"
        );
    }

    #[test]
    fn a_negative_target_is_its_own_error() {
        let document = document();
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let mut state = context.initial_state().unwrap();
        assert!(matches!(
            context.advance(&mut state, SimTick(-1), SimulationMode::Export, None),
            Err(SimError::NegativeReplayTarget { target: -1 })
        ));
    }

    #[test]
    fn an_exhausted_budget_is_an_error_and_never_an_approximation() {
        // 14.7's sentence tested from both sides: the run stops at the limit with the state it
        // honestly reached, and the remedy the spec names -- a higher explicit limit -- reaches
        // the target and matches the unbounded run.
        let document = document();
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let mut state = context.initial_state().unwrap();
        let error = context
            .advance(&mut state, SimTick(10), SimulationMode::Export, Some(3))
            .unwrap_err();
        let SimError::ReplayBudgetExhausted { budget, target } = error else {
            panic!("3 ticks cannot reach tick 10: {error}");
        };
        assert_eq!((budget, target), (3, 10));
        assert_eq!(
            state.tick,
            SimTick(3),
            "and the caller is told where it actually got to rather than being handed a guess"
        );
        context
            .advance(&mut state, SimTick(10), SimulationMode::Export, Some(100))
            .expect("a higher explicit limit is the remedy 14.7 names");
        assert_eq!(state.tick, SimTick(10));

        let mut reference =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let mut plain = reference.initial_state().unwrap();
        reference
            .advance(&mut plain, SimTick(10), SimulationMode::Export, None)
            .unwrap();
        assert_eq!(
            plain.canonical_bytes().unwrap(),
            state.canonical_bytes().unwrap(),
            "and it is the same state the unbounded run reached"
        );
    }

    #[test]
    fn invalidating_the_caches_changes_no_answer() {
        let document = document();
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::new(1, 8)).unwrap();
        let mut state = context.initial_state().unwrap();
        let mut hashes = Vec::new();
        for tick in 0..12i64 {
            context.step(&mut state, SimulationMode::Export).unwrap();
            hashes.push(crate::checkpoint::state_hash(&state).unwrap());
            if tick == 5 {
                context.invalidate_caches();
                assert_eq!(context.cache_report().checkpoints, 0);
            }
        }
        let mut fresh =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let mut plain = fresh.initial_state().unwrap();
        let mut reference = Vec::new();
        for _ in 0..12 {
            fresh.step(&mut plain, SimulationMode::Export).unwrap();
            reference.push(crate::checkpoint::state_hash(&plain).unwrap());
        }
        assert_eq!(
            hashes, reference,
            "a cache emptied mid-run must not move one tick's digest"
        );
        assert!(
            context
                .cache_report()
                .differs_in_caching(&fresh.cache_report()),
            "and the two runs really did cache differently, or this proves nothing"
        );
    }

    #[test]
    fn a_snapshot_hit_and_a_fresh_evaluation_are_the_same_input() {
        let document = document();
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let first = context.input_at(SimTick(0)).unwrap();
        let second = context.input_at(SimTick(4)).unwrap();
        let third = context.input_at(SimTick(0)).unwrap();
        assert_eq!(
            first.anchors, third.anchors,
            "frame 0 evaluated twice is one answer, whatever the cache did in between"
        );
        assert_ne!(first.position, second.position);
        let report = context.cache_report();
        assert_eq!(report.evaluations, 2, "two distinct frames");
        assert_eq!(report.hits, 1, "and the third came from the cache");
    }
}
