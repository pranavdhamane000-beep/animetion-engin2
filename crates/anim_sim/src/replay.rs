//! Replay, warm-up and export initialization (spec 14.3, 14.5 step 9, 14.7, 14.9).
//!
//! 14.5 step 9's requirement is the one word in this package that is easy to get wrong
//! quietly: implement seek, warm-up and export preparation **using the same stepping code**.
//! Not similar code, not code that produces the same numbers — the same loop. The reason the
//! spec insists is that a warm-up written as its own tight loop is a second implementation of
//! 14.6's eight stages, and the two drift the first time a stage is added. 14.12 then cannot
//! tell a correct export from an export that stepped a different world, because both finish,
//! both hash, and both look right in a viewer.
//!
//! So there is one driver in this crate — [`crate::context::SimulationContext::advance`],
//! which is the only `step_once` call site — and everything in this file is a *route* through
//! it that differs only in the mode it passes and the tick it stops at.
//!
//! ## What warm-up is allowed to differ about
//!
//! [`SimulationMode::WarmUp`] suppresses presentation and nothing else, and 14.7's sentence
//! says the same in more words: "Warm-up performs all internal spawns, random draws, events
//! and solver work but suppresses external one-shot presentation effects such as device
//! audio." All of the work. The spawns, the draws, the events, the solve. A warm-up that
//! skipped any of them would be an approximation, and 14.7 names the alternative to a full
//! run explicitly: "continued replay with a higher explicit limit or a clear error, never
//! approximate state."
//!
//! ## The reference run
//!
//! 14.12 compares four paths against "an uncheckpointed reference". [`sequential_hashes`] is
//! that reference and it is deliberately the plainest code in the crate: no checkpoints, no
//! seek, one loop from tick zero, hashing every tick. Every other path in this package is
//! required to equal it, and the freshness half of G4 — two processes, 10,000 frames — is the
//! same function run twice from outside, in
//! `crates/anim_test_support/tests/wp05_isolation.rs`.

use anim_core::time::FrameRange;

use crate::checkpoint::state_hash;
use crate::context::SimulationContext;
use crate::mode::SimulationMode;
use crate::state::SimState;
use crate::{Result, SimError, SimTick};

/// Every state hash from tick zero to `last_tick`, in order, with no checkpoints at all
/// (spec 14.12's "uncheckpointed reference").
///
/// The returned vector is indexed by tick, so `hashes[0]` is the initialized state at
/// [`SimTick::ZERO`] and `hashes[n]` is the state after `n` steps. A caller that wants the
/// pairs names them: [`crate::anim_headless::report`]'s output spells out both.
///
/// # Errors
///
/// The context's initial-state and stepping errors.
pub fn sequential_hashes(
    context: &mut SimulationContext<'_>,
    last_tick: SimTick,
    mode: SimulationMode,
) -> Result<Vec<anim_assets::ContentHash>> {
    if last_tick.is_negative() {
        return Err(SimError::NegativeReplayTarget {
            target: last_tick.0,
        });
    }
    let mut state = context.initial_state()?;
    let mut hashes = vec![state_hash(&state)?];
    while state.tick < last_tick {
        let next = state.tick.checked_add_ticks(1)?;
        context.advance(&mut state, next, mode, None)?;
        hashes.push(state_hash(&state)?);
    }
    Ok(hashes)
}

/// The same run as tick/hash pairs, for a report that has to name which tick a hash is of.
///
/// # Errors
///
/// As [`sequential_hashes`].
pub fn sequential_trace(
    context: &mut SimulationContext<'_>,
    last_tick: SimTick,
    mode: SimulationMode,
) -> Result<Vec<(SimTick, anim_assets::ContentHash)>> {
    Ok(sequential_hashes(context, last_tick, mode)?
        .into_iter()
        .enumerate()
        .map(|(index, hash)| (SimTick(i64::try_from(index).unwrap_or(i64::MAX)), hash))
        .collect())
}

/// Replay from tick zero to `target`, with the cache untouched by the route.
///
/// This is the path 14.12 compares a seek against, and it is a seek with the checkpoint lookup
/// removed rather than a second implementation: same initial state, same `advance`, same
/// [`SimulationMode::Replay`].
///
/// # Errors
///
/// [`SimError::NegativeReplayTarget`] and the advance errors.
pub fn replay_to(context: &mut SimulationContext<'_>, target: SimTick) -> Result<SimState> {
    if target.is_negative() {
        return Err(SimError::NegativeReplayTarget { target: target.0 });
    }
    let mut state = context.initial_state()?;
    context.advance(&mut state, target, SimulationMode::Replay, None)?;
    Ok(state)
}

/// Reach `tick` having announced nothing, which is 14.7's warm-up (spec 14.5 step 9).
///
/// # Errors
///
/// [`SimError::NegativeReplayTarget`] and the advance errors.
pub fn warm_up_to(context: &mut SimulationContext<'_>, tick: SimTick) -> Result<SimState> {
    if tick.is_negative() {
        return Err(SimError::NegativeReplayTarget { target: tick.0 });
    }
    let mut state = context.initial_state()?;
    context.advance(&mut state, tick, SimulationMode::WarmUp, None)?;
    Ok(state)
}

/// The state the first exported frame is produced from (spec 14.9's `prepare_export`).
///
/// 14.7's three clauses in order: restore the nearest valid checkpoint before the export
/// start, advance in warm-up mode to it, then hand the caller a state whose next step is in
/// export mode. The returned state carries an empty ledger, because everything it passed
/// through was suppressed — which is the whole of 14.10's "warm-up suppression prevents
/// reconstructed history from playing device audio".
///
/// # Errors
///
/// [`SimError::TickOverflow`] if a range boundary cannot be converted, and the advance
/// errors. An empty range is legal and produces the state at its start tick, because "export
/// nothing" is a decision a caller is allowed to make after initialization rather than a
/// malformed request.
pub fn prepare_export(context: &mut SimulationContext<'_>, range: FrameRange) -> Result<SimState> {
    let start_tick = context.clock().tick_of_frame(range.start)?;
    if start_tick.is_negative() {
        return Err(SimError::NegativeReplayTarget {
            target: start_tick.0,
        });
    }
    let mut state = match context.checkpoints_mut().verified_at_or_before(start_tick) {
        Some(checkpoint) => crate::checkpoint::restore_checkpoint(&checkpoint)?,
        None => context.initial_state()?,
    };
    if state.tick > start_tick {
        // A checkpoint cannot be ahead of the export start by construction
        // (`verified_at_or_before` ranges to the target), and a restored state's own tick is
        // the envelope's, so reaching here means the cache contradicted itself. 14.11's
        // remedy for that is a discard, and the route then is tick zero.
        state = context.initial_state()?;
    }
    context.advance(&mut state, start_tick, SimulationMode::WarmUp, None)?;
    Ok(state)
}

/// The tick an export range begins and ends on, for a caller that must not recompute it.
///
/// Kept here rather than in
/// [`crate::context`] because it is export preparation's arithmetic and nothing else reads
/// it, and 14.12's "different render frame grouping" test varies exactly these two numbers.
#[must_use]
pub fn export_ticks(context: &SimulationContext<'_>, range: FrameRange) -> (SimTick, SimTick) {
    (
        context
            .clock()
            .tick_of_frame(range.start)
            .unwrap_or(SimTick::ZERO),
        context
            .clock()
            .tick_of_frame(range.end)
            .unwrap_or(SimTick::ZERO),
    )
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
    use anim_core::id::{EventDefinitionId, NodeId, ProjectId, SceneId};
    use anim_core::rational::Rational;
    use anim_core::time::{FramePos, FrameRate};
    use anim_project::{
        EventDefinition, EventKind, NodeData, ProjectDocument, SceneData, SimulationSettings,
    };

    fn namespace() -> uuid::Uuid {
        uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn scene_id() -> SceneId {
        SceneId::derive(&namespace(), b"shot01")
    }

    fn fixture(ticks_per_frame: u32) -> ProjectDocument {
        let mut document = ProjectDocument::new(
            ProjectId::derive(&namespace(), b"replay-fixture"),
            FrameRate::new(24, 1).unwrap(),
        );
        document.simulation =
            SimulationSettings::new(ticks_per_frame, [23u8; anim_project::PROJECT_SEED_BYTES])
                .unwrap();
        let mut scene = SceneData::new("shot01").unwrap();
        let root = NodeId::derive(&namespace(), b"root");
        let child = NodeId::derive(&namespace(), b"child");
        scene
            .insert_node(NodeData::new(root, None, 0, "root"))
            .unwrap();
        scene
            .insert_node(NodeData::new(child, Some(root), 0, "child"))
            .unwrap();
        // Events at a whole frame and at a half frame, so one of the two paths through the
        // subframe age arithmetic is exercised by the ordinary reference run.
        for (label, position) in [
            ("on", FramePos::at_frame(1)),
            (
                "off",
                FramePos::new(2, Rational::new(1, 2).unwrap()).unwrap(),
            ),
            ("late", FramePos::at_frame(7)),
        ] {
            scene
                .insert_event(EventDefinition::new(
                    EventDefinitionId::derive(&namespace(), label.as_bytes()),
                    position,
                    0,
                    child,
                    EventKind::Signal,
                    0,
                ))
                .unwrap();
        }
        assert!(document.insert_scene(scene_id(), scene).is_none());
        document
    }

    #[test]
    fn the_reference_starts_at_the_initialized_state() {
        let document = fixture(4);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let hashes = sequential_hashes(&mut context, SimTick(6), SimulationMode::Export).unwrap();
        assert_eq!(hashes.len(), 7, "tick 0 through 6 is seven states");
        assert_eq!(
            hashes[0],
            state_hash(&context.initial_state().unwrap()).unwrap(),
            "the first entry is 14.6's initialized state, not the first stepped one"
        );
        for pair in hashes.windows(2) {
            assert_ne!(pair[0], pair[1], "a stepping run changes state every tick");
        }
    }

    #[test]
    fn warm_up_replay_and_export_all_equal_the_reference() {
        // 14.12's "compare forward playback, backward scrub, warm-up and export against an
        // uncheckpointed reference", at one tick, three routes.
        let document = fixture(4);
        let mut reference_context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let expected =
            sequential_hashes(&mut reference_context, SimTick(20), SimulationMode::Export).unwrap();

        // Each route stops where its own contract says it stops. Warm-up and replay are
        // driven to tick 19 directly; `prepare_export` stops at the tick its range *starts*
        // on, and frame 4 at four ticks per frame is tick 16 -- comparing it to 19 would be
        // a claim about the export range that the function never made.
        for (name, at_tick, got) in [
            ("warm-up", 19usize, {
                let mut c =
                    SimulationContext::new(&document, scene_id(), CheckpointCache::default())
                        .unwrap();
                warm_up_to(&mut c, SimTick(19)).unwrap()
            }),
            ("replay", 19, {
                let mut c =
                    SimulationContext::new(&document, scene_id(), CheckpointCache::default())
                        .unwrap();
                replay_to(&mut c, SimTick(19)).unwrap()
            }),
            ("export", 16, {
                let mut c =
                    SimulationContext::new(&document, scene_id(), CheckpointCache::new(3, 4))
                        .unwrap();
                prepare_export(&mut c, FrameRange::new(4, 8).unwrap()).unwrap()
            }),
        ] {
            assert_eq!(
                state_hash(&got).unwrap(),
                expected[at_tick],
                "{name} reached tick {at_tick} by a route that disagrees with the \
                 uncheckpointed reference"
            );
            assert_eq!(
                got.tick,
                SimTick(i64::try_from(at_tick).unwrap()),
                "and it really is stopped where the contract says, not merely hashed alike"
            );
        }
    }

    #[test]
    fn warm_up_announces_nothing_and_export_announces_everything() {
        let document = fixture(4);
        let mut warm_context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let warm = warm_up_to(&mut warm_context, SimTick(20)).unwrap();
        assert!(
            warm.presentation.is_empty(),
            "14.7: warm-up suppresses external one-shot presentation effects"
        );

        let mut export_context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let mut state = export_context.initial_state().unwrap();
        export_context
            .advance(&mut state, SimTick(20), SimulationMode::Export, None)
            .unwrap();
        assert_eq!(
            state_hash(&state).unwrap(),
            state_hash(&warm).unwrap(),
            "the two agree about the world, which is the requirement"
        );

        // And a cue event is what makes the ledgers differ, so put one in and run again.
        let mut cued_project = fixture(4);
        {
            let scene_id_value = scene_id();
            let scene = cued_project.scene_mut(&scene_id_value).unwrap();
            let child = NodeId::derive(&namespace(), b"child");
            scene
                .insert_event(EventDefinition::new(
                    EventDefinitionId::derive(&namespace(), b"cue"),
                    FramePos::at_frame(3),
                    0,
                    child,
                    EventKind::AudioCue,
                    0,
                ))
                .unwrap();
        }
        let mut cued_export =
            SimulationContext::new(&cued_project, scene_id(), CheckpointCache::default()).unwrap();
        let mut cued_warm =
            SimulationContext::new(&cued_project, scene_id(), CheckpointCache::default()).unwrap();
        let heard = {
            let mut s = cued_export.initial_state().unwrap();
            cued_export
                .advance(&mut s, SimTick(20), SimulationMode::Export, None)
                .unwrap();
            s
        };
        let silent = warm_up_to(&mut cued_warm, SimTick(20)).unwrap();
        assert_eq!(
            heard.presentation.len(),
            1,
            "export mode records the cue it passed through"
        );
        assert!(silent.presentation.is_empty());
        assert_eq!(
            state_hash(&heard).unwrap(),
            state_hash(&silent).unwrap(),
            "and recording it is not part of what the world is"
        );
    }

    #[test]
    fn export_start_ticks_come_from_the_range_and_the_clock() {
        let document = fixture(4);
        let context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        assert_eq!(
            export_ticks(&context, FrameRange::new(4, 8).unwrap()),
            (SimTick(16), SimTick(32))
        );
        assert_eq!(
            export_ticks(&context, FrameRange::new(0, 0).unwrap()),
            (SimTick(0), SimTick(0)),
            "an empty range is two equal ticks, not an error"
        );
    }

    #[test]
    fn a_negative_last_tick_is_refused_before_any_stepping() {
        let document = fixture(4);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        for result in [
            sequential_hashes(&mut context, SimTick(-1), SimulationMode::Export).map(drop),
            replay_to(&mut context, SimTick(-3)).map(drop),
            warm_up_to(&mut context, SimTick(-2)).map(drop),
        ] {
            assert!(matches!(result, Err(SimError::NegativeReplayTarget { .. })));
        }
    }

    #[test]
    fn prepare_export_before_zero_is_refused() {
        let document = fixture(4);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        // A range that starts below zero is a legitimate `FrameRange` on WP01's rules --
        // negative time is supported -- and 14.7 says stateful replay begins at zero.
        assert!(matches!(
            prepare_export(&mut context, FrameRange::new(-2, 4).unwrap()),
            Err(SimError::NegativeReplayTarget { target: -8 })
        ));
    }

    #[test]
    fn a_warm_up_chunk_size_changes_nothing() {
        // 14.7's last sentence in warm-up form: "Changing checkpoint cadence or warm-up chunk
        // size cannot change the exported state." The chunk here is how often the caller asks
        // for a state, which is a presentation grouping rather than a stepping decision -- and
        // this is what 14.12's "different render frame grouping" row is about.
        let document = fixture(3);
        let mut whole =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let target = SimTick(30);
        let expected = state_hash(&warm_up_to(&mut whole, target).unwrap()).unwrap();

        for chunk in [1u64, 2, 7, 16, 30] {
            let mut context =
                SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
            let mut state = context.initial_state().unwrap();
            let mut visited = vec![state_hash(&state).unwrap()];
            while state.tick < target {
                let limit = std::cmp::min(
                    state
                        .tick
                        .checked_add_ticks(i64::try_from(chunk).unwrap())
                        .unwrap(),
                    target,
                );
                context
                    .advance(&mut state, limit, SimulationMode::WarmUp, None)
                    .unwrap();
                visited.push(state_hash(&state).unwrap());
            }
            assert_eq!(
                *visited.last().unwrap(),
                expected,
                "grouping the calls every {chunk} ticks must not reach a different world"
            );
            // Stops = the number of chunks needed to cover 30 ticks, plus the initial state.
            let stops = 30u64.div_ceil(chunk) + 1;
            assert_eq!(
                usize::try_from(stops).unwrap(),
                visited.len(),
                "and grouping every {chunk} ticks really did stop at a different number of \
                 points, or the equality above would be testing one route twice"
            );
        }
    }

    #[test]
    fn the_trace_pairs_name_their_own_ticks() {
        let document = fixture(2);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let trace = sequential_trace(&mut context, SimTick(5), SimulationMode::Export).unwrap();
        let ticks: Vec<i64> = trace.iter().map(|(tick, _)| tick.0).collect();
        assert_eq!(ticks, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(trace[3].1, trace[3].1, "a hash is a value");
    }
}
