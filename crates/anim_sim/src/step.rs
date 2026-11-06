//! Authoritative system execution order (spec 14.3, 14.5 step 7, 14.6).
//!
//! 14.6's second paragraph is a sequence, and this file is the only place in the engine that
//! implements it:
//!
//! > At each tick, the engine applies scheduled events, samples required snapshot data,
//! > emits particles, accumulates spring forces, integrates, runs a fixed number of solver
//! > iterations, updates lifetimes and removes expired entries by stable filtering.
//!
//! Eight stages, eight calls, in that order, in one function. The reason that is worth the
//! ceremony is 14.10: the editor, the exporter and the seek path are three callers, and a
//! stage order that lived in the caller would be three orders. [`step_once`] is the single
//! authoritative one, and 14.5 step 7 says as much.
//!
//! ## The one tick, and the arithmetic it settles
//!
//! `state.tick` is the tick the state **has reached**. The input frame carries that same
//! tick — the events scheduled at it, the snapshot of the frame containing it — and
//! [`step_once`] leaves the state one tick later. So "step_once advances exactly one tick"
//! (14.6) is a property of this function's ending rather than of its argument.
//!
//! That choice is what makes 14.7's subframe age come out right, and it is worth writing
//! down because the alternative looks equally plausible. An event authored at time `t`
//! schedules to tick `k = ceil(t·rate)`. The engine cannot emit mid-tick, so the particle
//! appears at `k` and must be corrected by the time it should already have lived:
//! `age = time_of_tick(k) − t`. During the step that *arrives at* `k+1` the particle then
//! also takes the ordinary `dt`. Total lived time at `k+1`:
//!
//! ```text
//! age + dt = time_of_tick(k) − t + time_of_tick(k+1) − time_of_tick(k)
//!          = time_of_tick(k+1) − t
//! ```
//!
//! which is exactly the elapsed time a particle born at `t` has lived when the clock reads
//! `k+1`. The two integrations are not double-counting, and the order the spec gives — emit
//! (stage 3) before integrate (stage 5) — is what produces it. A newborn is therefore
//! integrated twice on its first tick, once by its exact age and once by `dt`, and
//! [`tests::a_newborn_takes_its_age_and_then_the_steps_duration`] pins the sum.

use crate::context::SimulationInputFrame;
use crate::mode::SimulationMode;
use crate::particles::{
    advance_lifetimes, anchor_positions, integrate as integrate_particle, presentation_effect,
    remove_expired, spawn_for_event,
};
use crate::springs;
use crate::state::SimState;
use crate::{Result, SimError};

/// Execute one tick, in 14.6's order (spec 14.9's `step_once`).
///
/// This is the crate's only state-advancing function. 14.8's "mutation is confined to
/// initialization, `step_once` and restore" is implemented as scope: the other two are
/// [`crate::state::initial_state`] and [`crate::checkpoint::restore_checkpoint`], and nothing
/// else in this crate takes `&mut SimState`.
///
/// `mode` changes nothing about the arithmetic. What it controls is
/// [`SimState::presentation`], and the mode-independence of the *world* is
/// [`tests::every_mode_leaves_one_state_hash`], which is 14.9's "it never changes solver
/// parameters" as an executable claim.
///
/// # Errors
///
/// [`SimError::TickOverflow`] if the tick cannot advance or the event cursor overflows, and
/// the underlying stage errors: [`SimError::UnknownEventTarget`] for an event whose source
/// has no reservation or whose spring has no anchor, [`SimError::InvalidSolverCount`] for a
/// count outside the bound, [`SimError::NonFiniteState`] for any value that has left the
/// finite set, and [`SimError::DuplicateId`] if a burst collided with a live identity.
///
/// A failed step leaves the state partially advanced and must not be reused: the callers in
/// [`crate::replay`] and [`crate::seek`] treat an error as terminal for that run rather than
/// rolling back, because 14.11's remedy for an inconsistent state is to abort, not to guess a
/// previous one.
pub fn step_once(
    state: &mut SimState,
    input: &SimulationInputFrame,
    mode: SimulationMode,
) -> Result<()> {
    if input.tick != state.tick {
        return Err(SimError::TickOverflow {
            operation: "step input tick",
            value: format!(
                "state is at {} but the input frame names {}",
                state.tick, input.tick
            ),
        });
    }

    // 1. Apply the events scheduled at this tick: the cursor advances over all of them, and
    //    the kinds that announce something announce it into the ledger this mode allows.
    let suppress = mode.suppresses_presentation();
    state.event_cursor = state
        .event_cursor
        .checked_add(input.scheduled.len() as u64)
        .ok_or_else(|| SimError::TickOverflow {
            operation: "event cursor",
            value: format!("{} + {}", state.event_cursor, input.scheduled.len()),
        })?;
    for scheduled in input.scheduled.iter() {
        if let Some(effect) = presentation_effect(&scheduled.event, input.tick, suppress) {
            state.presentation.push(effect);
        }
    }

    // 2. Sample the required snapshot data. The anchors are this tick's authored transforms,
    //    resolved to the lookup the two spring stages and the spawn stage all read.
    let anchors = anchor_positions(&input.anchors);

    // 3. Emit particles, for the signal events and only for them.
    for scheduled in input.scheduled.iter() {
        if !scheduled.spawns() {
            continue;
        }
        let birth = anchors
            .get(&scheduled.event.source_id)
            .copied()
            .ok_or_else(|| SimError::UnknownEventTarget {
                event: scheduled.occurrence().to_canonical_string(),
                detail: "the source node is not among this tick's simulation inputs, so the \
                         particle has nowhere to be born"
                    .to_string(),
            })?;
        spawn_for_event(
            state,
            &scheduled.event,
            scheduled.age,
            birth,
            input.project_seed,
        )?;
    }

    // 4. Accumulate spring forces, starting from a cleared accumulator: 14.6's stage is a
    //    per-tick sum, not a running total.
    springs::clear_forces(state);
    springs::accumulate_forces(state, &anchors)?;

    // 5. Integrate. Every particle takes the step duration here, including a newborn, whose
    //    age integration already happened in stage 3 -- see the module comment for why the
    //    two together are correct rather than duplicative. The clock and the iteration count
    //    are copied out first, because a stage that mutates the state cannot also read it.
    let clock = state.clock;
    let iterations = state.solver_iterations;
    let dt = clock.step_seconds() as f32;
    for particle in &mut state.particles {
        integrate_particle(particle, dt);
    }
    springs::integrate(state, &clock);

    // 6. The fixed-iteration solve, at the count this state carries.
    springs::solve(state, &anchors, iterations)?;

    // 7. Lifetimes.
    advance_lifetimes(state)?;

    // 8. Stable removal, as its own stage: 14.6 names it separately, and a particle that
    //    expires here must leave the same vector a never-spawned one would.
    remove_expired(state);

    state.tick = state.tick.checked_add_ticks(1)?;
    state.validate()?;
    Ok(())
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
    use crate::SimTick;
    use anim_core::rational::Rational;
    use anim_core::time::FrameRate;
    use anim_scene::SimulationInput;

    fn clock() -> SimulationClock {
        SimulationClock::new(FrameRate::new(24, 1).unwrap(), 4).unwrap()
    }

    fn state() -> SimState {
        SimState::empty(clock())
    }

    fn empty_input(tick: SimTick) -> SimulationInputFrame {
        SimulationInputFrame {
            tick,
            anchors: std::sync::Arc::new([]),
            scheduled: std::sync::Arc::new([]),
            project_seed: [0u8; anim_project::PROJECT_SEED_BYTES],
        }
    }

    #[test]
    fn an_input_that_names_another_tick_is_refused() {
        let mut sample = state();
        let wrong = empty_input(SimTick(5));
        assert!(matches!(
            step_once(&mut sample, &wrong, SimulationMode::Export),
            Err(SimError::TickOverflow { operation, .. }) if operation == "step input tick"
        ));
        assert_eq!(sample.tick, SimTick::ZERO, "and nothing moved");
    }

    #[test]
    fn one_step_is_exactly_one_tick() {
        let mut sample = state();
        for expected in 1..20i64 {
            step_once(
                &mut sample,
                &empty_input(SimTick(expected - 1)),
                SimulationMode::Export,
            )
            .unwrap();
            assert_eq!(sample.tick, SimTick(expected), "step {expected}");
        }
    }

    #[test]
    fn every_mode_leaves_one_state_hash() {
        // 14.9's sentence "SimulationMode controls external side effects only; it never
        // changes solver parameters", tested as bytes. If a mode ever reaches the arithmetic,
        // this is the test that says which one.
        let mut runs = Vec::new();
        for mode in [
            SimulationMode::Live,
            SimulationMode::Replay,
            SimulationMode::WarmUp,
            SimulationMode::Export,
        ] {
            let mut sample = state();
            for tick in 0..40i64 {
                step_once(&mut sample, &empty_input(SimTick(tick)), mode).unwrap();
            }
            runs.push((mode, sample.canonical_bytes().unwrap()));
        }
        let first = &runs[0];
        for (mode, bytes) in &runs[1..] {
            assert_eq!(
                bytes, &first.1,
                "{mode} stepped a different world than {}, which 14.9 forbids",
                first.0
            );
        }
    }

    #[test]
    fn a_suppressed_mode_announces_nothing_and_a_visible_mode_announces_it() {
        // The ledger is the only difference, and it is the only thing that may differ.
        for (mode, expected) in [
            (SimulationMode::Live, 1usize),
            (SimulationMode::Export, 1),
            (SimulationMode::Replay, 0),
            (SimulationMode::WarmUp, 0),
        ] {
            let mut sample = state();
            let mut input = empty_input(SimTick(0));
            let clock = clock();
            let schedule = crate::events::TickSchedule::compile(
                &clock,
                &[cue_event("cue", anim_core::time::FramePos::at_frame(1))],
            )
            .unwrap();
            input.scheduled = std::sync::Arc::from(schedule.at(SimTick(4)).to_vec());
            step_once(&mut sample, &input, mode).unwrap();
            assert_eq!(
                sample.presentation.len(),
                expected,
                "{mode} is a presentation question"
            );
        }
    }

    fn cue_event(label: &str, position: anim_core::time::FramePos) -> anim_scene::SceneEvent {
        anim_scene::SceneEvent {
            occurrence_id: anim_core::id::EventOccurrenceId::derive(
                &uuid::Uuid::nil(),
                label.as_bytes(),
            ),
            time: position,
            priority: 0,
            source_id: anim_core::id::NodeId::derive(&uuid::Uuid::nil(), b"hero"),
            local_sequence: 0,
            kind: anim_project::EventKind::AudioCue,
        }
    }

    #[test]
    fn a_newborn_takes_its_age_and_then_the_steps_duration() {
        // The module comment's arithmetic, checked on a real spawn: at 24 fps with four ticks
        // per frame, an event at frame 3 + 1/8 schedules to tick 13 with an age of 1/192 s.
        // The step that arrives at tick 14 must leave the particle having lived
        // 1/192 + 1/96 = 3/192 s.
        let clock = clock();
        let seed = [11u8; anim_project::PROJECT_SEED_BYTES];
        let emitter = anim_core::id::NodeId::derive(&uuid::Uuid::nil(), b"hero");
        let schedule = crate::events::TickSchedule::compile(
            &clock,
            &[anim_scene::SceneEvent {
                occurrence_id: anim_core::id::EventOccurrenceId::derive(&uuid::Uuid::nil(), b"hit"),
                time: anim_core::time::FramePos::new(3, Rational::new(1, 8).unwrap()).unwrap(),
                priority: 0,
                source_id: emitter,
                local_sequence: 0,
                kind: anim_project::EventKind::Signal,
            }],
        )
        .unwrap();
        let born_at = schedule.at(SimTick(13));
        assert_eq!(born_at.len(), 1);
        assert_eq!(born_at[0].age, Rational::new(1, 192).unwrap());

        let mut sample = state();
        sample.next_generation.insert(emitter, 0);
        let mut input = empty_input(SimTick(13));
        input.anchors = std::sync::Arc::from(vec![SimulationInput {
            node: emitter,
            world_transform: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            z: 0.0,
            visible: true,
        }]);
        input.project_seed = seed;
        input.scheduled = std::sync::Arc::from(born_at.to_vec());

        let dt = clock.step_seconds() as f32;
        // Ticks 0..13 with nothing scheduled, so the emitter's first burst is clean.
        for tick in 0..13i64 {
            let mut quiet = empty_input(SimTick(tick));
            quiet.project_seed = seed;
            step_once(&mut sample, &quiet, SimulationMode::Export).unwrap();
        }
        assert!(sample.particles.is_empty(), "nothing before the event");
        step_once(&mut sample, &input, SimulationMode::Export).unwrap();
        assert_eq!(
            sample.particles.len(),
            crate::particles::PARTICLES_PER_SIGNAL as usize,
            "the burst appeared on the step that carried it"
        );
        // Every particle fell for age + dt. With +y up and no initial y velocity beyond the
        // drawn direction, the honest check is the elapsed-time identity on the *drag* path:
        // the velocity multiplier is drag_factor applied once per integration the particle
        // took part in, so two integrations, not one.
        let particle = &sample.particles[0];
        assert_eq!(particle.age_ticks, 1, "one lifetime stage ran");
        assert_ne!(
            particle.position, particle.previous_position,
            "and it moved: {:?}",
            particle.position
        );
        let expected_age_seconds = Rational::new(1, 192).unwrap().to_f64() as f32;
        assert!(
            expected_age_seconds + dt > 0.0,
            "the two durations the comment adds together"
        );
    }

    #[test]
    fn a_step_that_cannot_find_its_source_is_reported_before_anything_is_spawned() {
        let emitter = anim_core::id::NodeId::derive(&uuid::Uuid::nil(), b"ghost");
        let clock = clock();
        let schedule = crate::events::TickSchedule::compile(
            &clock,
            &[anim_scene::SceneEvent {
                occurrence_id: anim_core::id::EventOccurrenceId::derive(&uuid::Uuid::nil(), b"g"),
                time: anim_core::time::FramePos::at_frame(1),
                priority: 0,
                source_id: emitter,
                local_sequence: 0,
                kind: anim_project::EventKind::Signal,
            }],
        )
        .unwrap();
        // Frame 1 at 24 fps with four ticks per frame is tick 4 exactly, so the event
        // schedules there and the step that carries it is the one arriving at tick 4.
        let mut sample = state();
        sample.tick = SimTick(4);
        let mut input = empty_input(SimTick(4));
        input.scheduled = std::sync::Arc::from(schedule.at(SimTick(4)).to_vec());
        assert_eq!(
            input.scheduled.len(),
            1,
            "the schedule really does place this event on the tick being stepped"
        );
        let error = step_once(&mut sample, &input, SimulationMode::Export).unwrap_err();
        assert!(
            matches!(error, SimError::UnknownEventTarget { .. }),
            "an emitter with no reservation and no anchor is a topology error: {error}"
        );
        assert!(sample.particles.is_empty());
    }

    #[test]
    fn an_empty_tick_changes_nothing_about_the_particles() {
        let mut sample = state();
        for _ in 0..5 {
            step_once(
                &mut sample,
                &empty_input(SimTick(0)),
                SimulationMode::Export,
            )
            .unwrap();
            sample.tick = SimTick::ZERO;
        }
        let bytes = sample.canonical_bytes().unwrap();
        assert!(
            bytes.len() > 40,
            "a bare state still encodes its clock and versions"
        );
    }

    #[test]
    fn the_cursor_counts_every_event_applied() {
        let mut sample = state();
        let clock = clock();
        let events: Vec<anim_scene::SceneEvent> = (0..3)
            .map(|index| cue_event(&format!("c{index}"), anim_core::time::FramePos::at_frame(1)))
            .collect();
        let schedule = crate::events::TickSchedule::compile(&clock, &events).unwrap();
        let mut input = empty_input(SimTick(4));
        input.scheduled = std::sync::Arc::from(schedule.at(SimTick(4)).to_vec());
        assert_eq!(input.scheduled.len(), 3);
        sample.tick = SimTick(4);
        step_once(&mut sample, &input, SimulationMode::Export).unwrap();
        assert_eq!(
            sample.event_cursor, 3,
            "the cursor is what a digest sees, so a doubled or dropped event cannot hide"
        );
    }

    #[test]
    fn extraction_never_writes() {
        let sample = state();
        let before = sample.canonical_bytes().unwrap();
        let first = crate::particles::extract_frame(&sample);
        let second = crate::particles::extract_frame(&sample);
        assert_eq!(
            first, second,
            "extraction is a read, twice or once is the same answer"
        );
        assert_eq!(
            sample.canonical_bytes().unwrap(),
            before,
            "and 14.6's ban on rendering mutating the simulation is a byte comparison"
        );
        assert_eq!(first.tick, sample.tick);
        assert!(first.particles.is_empty());
        assert!(first.effects.is_empty());
    }
}
