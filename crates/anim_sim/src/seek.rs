//! Restore-and-advance random-access seek (spec 14.3, 14.5 step 9, 14.7, 14.9).
//!
//! 14.7 states the whole algorithm in three sentences, and each one is a defect this file has
//! to avoid:
//!
//! 1. **"Select the greatest verified checkpoint not exceeding target."** Not the nearest,
//!    not the most recent, and never one *after* the target. A checkpoint ahead of the target
//!    is future mutable state, and handing it back would produce a state that is correct for
//!    a tick nobody asked about.
//! 2. **"Otherwise restore tick zero."** The fallback is total: with checkpoints disabled, or
//!    with every entry failing verification, the answer is still the right state, only slower
//!    to reach. That is what makes 14.12's "disabled checkpoints" row a configuration rather
//!    than a mode of operation.
//! 3. **"Return a state whose canonical hash equals sequential execution at target."** The
//!    output clause, and the reason `mode` here is
//!    [`SimulationMode::Replay`]: the reconstruction is not a performance, so it announces
//!    nothing — and because the ledger is outside the digest, that choice cannot affect the
//!    equality this sentence demands.
//!
//! The `&mut` on the context parameter is deviation-row-15's reason, stated in
//! [`crate::context`]: the snapshot cache is owned and mutable rather than locked, so a shared
//! borrow could not fill it.

use crate::checkpoint::CheckpointCache;
use crate::context::SimulationContext;
use crate::mode::SimulationMode;
use crate::state::SimState;
use crate::{Result, SimError, SimTick};

/// Reach the state at `target` by the shortest route the cache permits (spec 14.9).
///
/// # Errors
///
/// [`SimError::NegativeReplayTarget`] below zero, and the [`crate::context`] stepping errors
/// for anything the snapshot, the schedule or a stage refuses.
pub fn seek(context: &mut SimulationContext<'_>, target: SimTick) -> Result<SimState> {
    seek_with_budget(context, target, None)
}

/// [`seek`] with 14.7's explicit replay budget.
///
/// The budget counts the ticks this call steps, not the ticks the project has. A caller that
/// wants a global bound reads [`CheckpointCache`] and the tick itself.
///
/// # Errors
///
/// As [`seek`], plus [`SimError::ReplayBudgetExhausted`].
pub fn seek_with_budget(
    context: &mut SimulationContext<'_>,
    target: SimTick,
    budget: Option<u64>,
) -> Result<SimState> {
    if target.is_negative() {
        return Err(SimError::NegativeReplayTarget { target: target.0 });
    }
    let mut state = match context.checkpoints_mut().verified_at_or_before(target) {
        Some(checkpoint) => crate::checkpoint::restore_checkpoint(&checkpoint)?,
        None => context.initial_state()?,
    };
    context.advance(&mut state, target, SimulationMode::Replay, budget)?;
    Ok(state)
}

/// Whether a seek is cheaper than a replay, for a caller deciding which to offer.
///
/// Not a heuristic and not a policy: it is the question "did this cache retain anything at or
/// before the target", and it is exposed because 14.10 makes the editor decide how far behind
/// a scrub it will tolerate before pausing and reconstructing.
#[must_use]
pub fn has_checkpoint_before(cache: &CheckpointCache, target: SimTick) -> bool {
    cache.entries.range(..=target.0).next_back().is_some()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use anim_core::id::{EventDefinitionId, NodeId, ProjectId, SceneId};
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

    fn document(ticks_per_frame: u32) -> ProjectDocument {
        let mut document = ProjectDocument::new(
            ProjectId::derive(&namespace(), b"seek-fixture"),
            FrameRate::new(24, 1).unwrap(),
        );
        document.simulation =
            SimulationSettings::new(ticks_per_frame, [17u8; anim_project::PROJECT_SEED_BYTES])
                .unwrap();
        let mut scene = SceneData::new("shot01").unwrap();
        let root = NodeId::derive(&namespace(), b"root");
        let child = NodeId::derive(&namespace(), b"child");
        let tail = NodeId::derive(&namespace(), b"tail");
        for (label, id, parent) in [
            ("root", root, None),
            ("child", child, Some(root)),
            ("tail", tail, Some(child)),
        ] {
            scene
                .insert_node(NodeData::new(id, parent, 0, label))
                .unwrap();
        }
        for frame in [0i64, 2, 5, 9] {
            scene
                .insert_event(EventDefinition::new(
                    EventDefinitionId::derive(&namespace(), format!("hit{frame}").as_bytes()),
                    FramePos::at_frame(frame),
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

    /// The uncheckpointed reference 14.12 compares every other path against.
    fn reference(
        document: &ProjectDocument,
        ticks: i64,
    ) -> Vec<(SimTick, anim_assets::ContentHash)> {
        let mut context =
            SimulationContext::new(document, scene_id(), CheckpointCache::default()).unwrap();
        let mut state = context.initial_state().unwrap();
        let mut hashes = vec![(state.tick, crate::checkpoint::state_hash(&state).unwrap())];
        for tick in 0..ticks {
            context
                .advance(&mut state, SimTick(tick + 1), SimulationMode::Export, None)
                .unwrap();
            hashes.push((state.tick, crate::checkpoint::state_hash(&state).unwrap()));
        }
        hashes
    }

    #[test]
    fn a_seek_with_no_checkpoints_replays_from_zero() {
        let document = document(4);
        let hashes = reference(&document, 20);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        assert!(
            context.checkpoints().is_empty(),
            "the disabled configuration is the interesting one"
        );
        for target in [0i64, 1, 7, 13, 20] {
            let state = seek(&mut context, SimTick(target)).unwrap();
            assert_eq!(state.tick, SimTick(target));
            assert_eq!(
                crate::checkpoint::state_hash(&state).unwrap(),
                hashes[usize::try_from(target).unwrap()].1,
                "tick {target} must equal sequential execution, with nothing cached at all"
            );
        }
    }

    #[test]
    fn every_reachable_target_matches_the_reference() {
        // 14.12's "compare sequential execution with seeks to every tick in randomized
        // ranges", here exhaustively over a short run: a sampled range would let one bad tick
        // survive, and the tick that breaks is usually the one next to an event.
        let document = document(2);
        let hashes = reference(&document, 24);
        for (cadence, capacity) in [(1usize, 8usize), (5, 3), (7, 64), (24, 1), (0, 8)] {
            let mut context = SimulationContext::new(
                &document,
                scene_id(),
                CheckpointCache::new(u64::from(cadence as u32), capacity),
            )
            .unwrap();
            // Populate the cache the way a forward run would.
            let mut warm = context.initial_state().unwrap();
            context
                .advance(&mut warm, SimTick(24), SimulationMode::Export, None)
                .unwrap();
            for (tick, expected) in &hashes {
                let state = seek(&mut context, *tick).unwrap();
                assert_eq!(
                    crate::checkpoint::state_hash(&state).unwrap(),
                    *expected,
                    "cadence {cadence} capacity {capacity} differs at {tick}"
                );
            }
        }
    }

    #[test]
    fn a_later_checkpoint_is_never_used_to_reach_an_earlier_tick() {
        let document = document(4);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::new(8, 8)).unwrap();
        let mut state = context.initial_state().unwrap();
        context
            .advance(&mut state, SimTick(24), SimulationMode::Export, None)
            .unwrap();
        assert!(
            context.checkpoints().len() >= 3,
            "ticks 0, 8, 16, 24 are all on the cadence"
        );
        assert!(!has_checkpoint_before(context.checkpoints(), SimTick(7)));
        assert!(has_checkpoint_before(context.checkpoints(), SimTick(8)));
        let at_seven = seek(&mut context, SimTick(7)).unwrap();
        assert_eq!(at_seven.tick, SimTick(7));
        let reference_state = {
            let mut fresh =
                SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
            let mut plain = fresh.initial_state().unwrap();
            fresh
                .advance(&mut plain, SimTick(7), SimulationMode::Export, None)
                .unwrap();
            plain
        };
        assert_eq!(
            crate::checkpoint::state_hash(&at_seven).unwrap(),
            crate::checkpoint::state_hash(&reference_state).unwrap(),
            "and reaching it the hard way is the same state"
        );
    }

    #[test]
    fn a_seek_below_zero_is_refused_without_touching_the_cache() {
        let document = document(4);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::new(1, 4)).unwrap();
        let mut state = context.initial_state().unwrap();
        context
            .advance(&mut state, SimTick(3), SimulationMode::Export, None)
            .unwrap();
        let before = context.checkpoints().len();
        assert!(matches!(
            seek(&mut context, SimTick(-1)),
            Err(SimError::NegativeReplayTarget { target: -1 })
        ));
        assert_eq!(
            context.checkpoints().len(),
            before,
            "14.7 supports ticks at or above zero, and the refusal costs the cache nothing"
        );
    }

    #[test]
    fn a_corrupt_cache_entry_falls_back_instead_of_failing_the_seek() {
        let document = document(4);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::new(1, 8)).unwrap();
        let mut state = context.initial_state().unwrap();
        context
            .advance(&mut state, SimTick(6), SimulationMode::Export, None)
            .unwrap();
        assert!(context.checkpoints().len() > 1);
        // Corrupt every entry the way a lost write would: the digest no longer matches.
        let entries = &mut context.checkpoints_mut().entries;
        for checkpoint in entries.values_mut() {
            checkpoint.state.event_cursor = checkpoint.state.event_cursor.wrapping_add(1);
        }
        let sought = seek(&mut context, SimTick(6)).unwrap();
        assert_eq!(sought.tick, SimTick(6));
        // The corrupted entry that would have been chosen first is gone, and every entry the
        // cache still holds now verifies. `advance` re-records clean entries on the way to the
        // target, so "is_empty" would be a claim about the loop rather than about the discard.
        assert!(
            crate::checkpoint::restore_checkpoint(
                context
                    .checkpoints()
                    .entries
                    .get(&6)
                    .expect("tick 6 is cached again")
            )
            .is_ok(),
            "the entry at the target tick verifies, which the corrupted one could not"
        );
        for (tick, entry) in &context.checkpoints().entries {
            assert!(
                crate::checkpoint::restore_checkpoint(entry).is_ok(),
                "no entry left in the cache is one seek would refuse at tick {tick}"
            );
        }
        assert_eq!(
            crate::checkpoint::state_hash(&sought).unwrap(),
            reference(&document, 7)[6].1,
            "and the reconstruction is right"
        );
    }

    #[test]
    fn seeking_twice_to_one_tick_is_the_same_state() {
        let document = document(2);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::new(3, 8)).unwrap();
        let first = seek(&mut context, SimTick(9)).unwrap();
        let second = seek(&mut context, SimTick(9)).unwrap();
        assert_eq!(
            crate::checkpoint::state_hash(&first).unwrap(),
            crate::checkpoint::state_hash(&second).unwrap()
        );
        assert_eq!(
            first, second,
            "including the ledger, which both runs leave empty"
        );
    }

    #[test]
    fn a_budget_that_is_too_small_reports_rather_than_approximating() {
        let document = document(4);
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::new(64, 4)).unwrap();
        let error = seek_with_budget(&mut context, SimTick(40), Some(2)).unwrap_err();
        assert!(
            matches!(
                error,
                SimError::ReplayBudgetExhausted {
                    budget: 2,
                    target: 40
                }
            ),
            "the nearest checkpoint is at 0, so 2 ticks cannot reach 40: {error}"
        );
        let state = seek_with_budget(&mut context, SimTick(40), None).unwrap();
        assert_eq!(state.tick, SimTick(40));
    }
}
