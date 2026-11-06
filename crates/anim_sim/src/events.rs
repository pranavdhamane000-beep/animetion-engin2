//! Event-to-tick scheduling and subframe spawn ages (spec 14.3, 14.5 step 5, 14.7).
//!
//! 14.5 step 5 asks for scene events compiled into a deterministic tick schedule, and 14.6's
//! first stage is "applies scheduled events". This file is the whole of the translation from
//! 13.7's ordered event sequence to "which events land on which tick, in which order, and
//! how late each one is within its frame" — and the last of those three is the reason the
//! schedule carries more than a tick.
//!
//! ## Why the schedule is compiled once, over the whole scene
//!
//! `BUILD_STATE.md` points at [`anim_scene::events_for_frame`] for this step, and that is
//! the right function for a per-frame consumer. This is not one: a seek to an arbitrary tick
//! has to know which events *preceded* it, so a schedule gathered frame by frame during a
//! replay would make the schedule itself a function of where the caller started — and 14.12
//! compares a seek against sequential execution, which that would break by construction.
//!
//! So the schedule is built from [`anim_scene::EventSources::all_occurrences`] once, over
//! the whole scene, and both the sequential and the seek path read the same map. The two
//! entry points are the same delivery decision (13.6's rule, tested in WP04), so this is not
//! a second event rule; it is one rule with the range widened.
//!
//! ## The age
//!
//! 14.7: "For a spawn scheduled at tick `k`, compute exact `age = time_of_tick(k) - t`;
//! convert once to working precision and use the same force function to initialize its
//! state. Negative age is an internal error."
//!
//! The age is the part of the previous tick the particle should already have lived, and it
//! is exact rational arithmetic here — `Rational`, not seconds — because a float age would
//! be a different number in a schedule built at NTSC than at 24, and both are the same
//! authored event. [`crate::particles::spawn_for_event`] converts it once, at the point
//! where it changes a position.
//!
//! Note what that division of labour buys: this file decides *when*, and never touches a
//! float.

use anim_core::id::EventOccurrenceId;
use anim_core::rational::Rational;
use anim_project::SceneData;
use anim_scene::{EventSources, SceneEvent};

use crate::clock::SimulationClock;
use crate::{Result, SimError, SimTick};

/// One scene event placed on the simulation timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct ScheduledEvent {
    /// The occurrence, unchanged from the snapshot's sequence: identity, position, priority,
    /// source, sequence and kind all travel together, because a schedule that dropped the
    /// ordering fields would let a consumer re-derive an order that 13.7 already fixed.
    pub event: SceneEvent,
    /// `ceil(t * simulation_rate)` (14.7).
    pub tick: SimTick,
    /// The exact time `t` the event was authored at.
    pub exact_time: Rational,
    /// `time_of_tick(tick) - t`, exact, and never negative (14.7).
    pub age: Rational,
}

impl ScheduledEvent {
    /// Whether this event's kind is the one [`crate::particles`] reads as a spawn.
    #[must_use]
    pub fn spawns(&self) -> bool {
        self.event.kind == anim_project::EventKind::Signal
    }

    /// The occurrence identity, for a cursor or a report that must not rebuild an id.
    #[must_use]
    pub fn occurrence(&self) -> EventOccurrenceId {
        self.event.occurrence_id
    }
}

/// The tick schedule for one scene, keyed by tick.
///
/// A `BTreeMap` because `clippy.toml` rules out `HashMap` and because 14.7's own iteration
/// requirement — "Collections affecting results are sorted vectors or BTreeMap" — is then a
/// property of the type. The values are `Vec`s in 13.7's canonical order, which is what 14.7
/// means by "within one tick, preserve canonical scene-event order".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TickSchedule {
    entries: std::collections::BTreeMap<i64, Vec<ScheduledEvent>>,
    total: u64,
}

impl TickSchedule {
    /// Compile 13.7's event sequence onto this clock's ticks.
    ///
    /// # Errors
    ///
    /// [`SimError::TickOverflow`] or [`SimError::InvalidRate`] from the clock, and
    /// [`SimError::UnknownEventTarget`] for a negative age — which 14.7 calls an internal
    /// error, and which means the clock's ceiling and its tick-to-time function disagree
    /// about the same event.
    pub fn compile(clock: &SimulationClock, events: &[SceneEvent]) -> Result<Self> {
        let mut entries: std::collections::BTreeMap<i64, Vec<ScheduledEvent>> =
            std::collections::BTreeMap::new();
        let mut total = 0u64;
        for event in events {
            let tick = clock.tick_of_position(event.time)?;
            let authored = clock.frame_rate.time_of(event.time)?;
            let boundary = clock.time_of_tick(tick)?;
            let age = boundary
                .checked_sub(authored)
                .map_err(|e| SimError::UnknownEventTarget {
                    event: event.occurrence_id.to_canonical_string(),
                    detail: format!("the spawn age could not be computed exactly: {e}"),
                })?;
            if age < Rational::from_i128(0) {
                // 14.7 names this case an internal error rather than a data error, and it is
                // worth the variant: `tick_of_position` ceilings and `time_of_tick` floors,
                // so a negative here means the two directions of one conversion disagree —
                // a clock bug, not a bad project.
                return Err(SimError::UnknownEventTarget {
                    event: event.occurrence_id.to_canonical_string(),
                    detail: format!(
                        "negative spawn age {age}: tick {} resolves to {boundary}, which is \
                         before the authored time {authored}",
                        tick.0
                    ),
                });
            }
            entries.entry(tick.0).or_default().push(ScheduledEvent {
                event: event.clone(),
                tick,
                exact_time: authored,
                age,
            });
            total = total.checked_add(1).ok_or_else(|| SimError::TickOverflow {
                operation: "schedule size",
                value: total.to_string(),
            })?;
        }
        Ok(Self { entries, total })
    }

    /// The scene's schedule, compiled from the authored definitions (13.5 step 1's data).
    ///
    /// # Errors
    ///
    /// [`SimError::Scene`] for an event whose source node the scene does not have, and the
    /// compile errors above.
    pub fn for_scene(clock: &SimulationClock, scene: &SceneData) -> Result<Self> {
        let events = EventSources::of(scene)
            .all_occurrences()
            .map_err(SimError::Scene)?;
        Self::compile(clock, &events)
    }

    /// The events scheduled at `tick`, in canonical order. An empty slice is a legal answer
    /// for most ticks, which is why this returns a slice rather than an `Option`.
    #[must_use]
    pub fn at(&self, tick: SimTick) -> &[ScheduledEvent] {
        self.entries
            .get(&tick.0)
            .map_or(&[], |events| events.as_slice())
    }

    /// Every tick that has at least one event, in ascending order.
    pub fn scheduled_ticks(&self) -> impl Iterator<Item = SimTick> + '_ {
        self.entries.keys().copied().map(SimTick)
    }

    /// The total number of scheduled events, across all ticks.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total == 0
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
    use anim_core::id::{EventDefinitionId, NodeId};
    use anim_core::time::{FramePos, FrameRate};
    use anim_project::{EventDefinition, EventKind};

    fn namespace() -> uuid::Uuid {
        uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn node(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    fn clock(ticks_per_frame: u32) -> SimulationClock {
        SimulationClock::new(FrameRate::new(24, 1).unwrap(), ticks_per_frame).unwrap()
    }

    /// An occurrence identity for one test event.
    ///
    /// Derived directly rather than through `anim_scene::occurrence_id`, because this file's
    /// subject is where an event lands, not what its identity is — and WP04 owns that
    /// derivation and its tests. What matters here is that two test events never share one.
    fn occurrence(definition: &str, position: FramePos) -> EventOccurrenceId {
        EventOccurrenceId::derive(
            &namespace(),
            format!("{definition}@{}", position.frame).as_bytes(),
        )
    }

    fn event(definition: &str, position: FramePos, priority: i32, kind: EventKind) -> SceneEvent {
        SceneEvent {
            occurrence_id: occurrence(definition, position),
            time: position,
            priority,
            source_id: node("hero"),
            local_sequence: 0,
            kind,
        }
    }

    #[test]
    fn an_event_on_a_tick_boundary_has_zero_age() {
        let schedule = TickSchedule::compile(
            &clock(4),
            &[event("cut", FramePos::at_frame(3), 0, EventKind::Signal)],
        )
        .unwrap();
        let at = schedule.at(SimTick(12));
        assert_eq!(at.len(), 1, "frame 3 at 24 fps with 4 ticks is tick 12");
        assert_eq!(at[0].age, Rational::from_i128(0), "no correction at all");
        assert_eq!(at[0].exact_time, Rational::new(3, 24).unwrap());
        assert!(at[0].spawns());
    }

    #[test]
    fn a_subframe_event_carries_the_exact_part_of_the_previous_tick() {
        // The case 14.7's whole age sentence exists for. 24 fps, four ticks per frame, so one
        // tick is 1/96 s. An event at frame 3 + 1/8 is 3.125/24 = 5/40 = 1/8 of a frame...
        // spelled exactly: position value 25/8 frames, t = 25/200 s = 1/8 s, and
        // t * 96 = 12 ticks exactly plus 1/2 -- no. Recomputed independently:
        //   t = (3 + 1/8) / 24 = (25/8)/24 = 25/192 s
        //   t * rate = (25/192) * 96 = 25/2 = 12.5  ->  ceil = 13
        //   time_of_tick(13) = 13/96 s
        //   age = 13/96 - 25/192 = 26/192 - 25/192 = 1/192 s
        let schedule = TickSchedule::compile(
            &clock(4),
            &[event(
                "hit",
                FramePos::new(3, Rational::new(1, 8).unwrap()).unwrap(),
                0,
                EventKind::Signal,
            )],
        )
        .unwrap();
        let at = schedule.at(SimTick(13));
        assert_eq!(at.len(), 1, "the event schedules to tick 13, not 12");
        assert_eq!(
            at[0].age,
            Rational::new(1, 192).unwrap(),
            "and the age is the exact remainder, not a float approximation of it"
        );
        assert!(schedule.at(SimTick(12)).is_empty());
    }

    #[test]
    fn the_age_is_one_quarter_of_a_tick_at_ntsc_where_a_float_would_drift() {
        // 30000/1001 fps with two ticks per frame. An event at frame 7 + 1/3:
        //   t = (22/3) / (30000/1001) = (22/3) * (1001/30000) = 22022/90000 = 1001/4095... no,
        //   reduced: 22022/90000 = 11011/45000 s
        //   rate = 60000/1001 ticks/s, so t*rate = (11011/45000)*(60000/1001)
        //        = (11011*60000)/(45000*1001) = (11011*4)/(3*1001) = 44044/3003 = 14.666...
        //        = 44/3 exactly, so ceil = 15
        //   time_of_tick(15) = 15 * 1001/60000 = 15015/60000 = 1001/4000 s
        //   age = 1001/4000 - 11011/45000
        let clock = SimulationClock::new(FrameRate::new(30000, 1001).unwrap(), 2).unwrap();
        let position = FramePos::new(7, Rational::new(1, 3).unwrap()).unwrap();
        let schedule =
            TickSchedule::compile(&clock, &[event("ntsc", position, 0, EventKind::Signal)])
                .unwrap();
        let at = schedule.at(SimTick(15));
        assert_eq!(at.len(), 1, "44/3 ceilings to 15");
        let authored = clock.frame_rate.time_of(position).unwrap();
        assert_eq!(authored, Rational::new(11011, 45000).unwrap());
        assert_eq!(
            at[0].age,
            clock
                .time_of_tick(SimTick(15))
                .unwrap()
                .checked_sub(authored)
                .unwrap(),
            "and the stored age is the exact difference"
        );
        // The same age in f64 is not representable, which is why this file never converts.
        let as_float = at[0].age.to_f64();
        assert!(
            Rational::parse_canonical(&format!("{as_float}")).is_err(),
            "a decimal spelling of the float is not the rational: {as_float}"
        );
    }

    #[test]
    fn events_at_one_tick_keep_the_order_they_arrived_in() {
        // 14.7: "Within one tick, preserve canonical scene-event order." Two events one
        // priority apart, both inside the same tick, must stay in the sequence 13.7 built.
        let first = event("a", FramePos::at_frame(2), 0, EventKind::Signal);
        let second = event("b", FramePos::at_frame(2), 1, EventKind::AudioCue);
        let schedule = TickSchedule::compile(&clock(1), &[first.clone(), second.clone()]).unwrap();
        let at = schedule.at(SimTick(2));
        assert_eq!(at.len(), 2);
        assert_eq!(at[0].event, first, "the earlier key term stays first");
        assert_eq!(at[1].event, second);

        // And reversed input is a different schedule, not the same one sorted: the order is
        // 13.7's, and a compile that re-sorted would be a second delivery rule.
        let reversed = TickSchedule::compile(&clock(1), &[second, first]).unwrap();
        assert_eq!(
            reversed.at(SimTick(2))[0].event.occurrence_id,
            at[1].event.occurrence_id
        );
    }

    #[test]
    fn a_signal_is_a_spawn_and_the_other_kinds_are_not() {
        for kind in [EventKind::Signal, EventKind::AudioCue, EventKind::CameraCut] {
            let schedule =
                TickSchedule::compile(&clock(1), &[event("k", FramePos::at_frame(4), 0, kind)])
                    .unwrap();
            let at = schedule.at(SimTick(4));
            assert_eq!(
                at[0].spawns(),
                kind == EventKind::Signal,
                "only the kind with no other consumer is a spawn instruction"
            );
        }
    }

    #[test]
    fn an_empty_scene_compiles_to_an_empty_schedule() {
        let schedule = TickSchedule::compile(&clock(4), &[]).unwrap();
        assert!(schedule.is_empty());
        assert_eq!(schedule.len(), 0);
        assert!(schedule.at(SimTick(0)).is_empty());
        assert_eq!(schedule.scheduled_ticks().count(), 0);
    }

    #[test]
    fn the_scene_route_uses_the_same_occurrences_the_snapshot_carries() {
        // 13.6's one-delivery rule, asserted where it could actually break: if the schedule
        // came from a different event path than the snapshot, a seek and a sequential run
        // would disagree about which tick a cue lands on.
        let mut scene = SceneData::new("shot01").unwrap();
        scene
            .insert_node(anim_project::NodeData::new(node("hero"), None, 0, "hero"))
            .unwrap();
        for (label, frame) in [("early", 1i64), ("late", 5)] {
            scene
                .insert_event(EventDefinition::new(
                    EventDefinitionId::derive(&namespace(), label.as_bytes()),
                    FramePos::at_frame(frame),
                    0,
                    node("hero"),
                    EventKind::Signal,
                    0,
                ))
                .unwrap();
        }
        let clock = clock(2);
        let schedule = TickSchedule::for_scene(&clock, &scene).unwrap();
        assert_eq!(schedule.len(), 2);
        let direct = EventSources::of(&scene).all_occurrences().unwrap();
        let same = TickSchedule::compile(&clock, &direct).unwrap();
        assert_eq!(
            schedule, same,
            "the convenience path and the explicit path are one schedule"
        );
        assert_eq!(schedule.at(SimTick(2)).len(), 1, "frame 1 is tick 2");
        assert_eq!(schedule.at(SimTick(10)).len(), 1, "frame 5 is tick 10");
    }

    #[test]
    fn a_negative_position_schedules_below_zero_and_is_still_not_a_negative_age() {
        // The age stays non-negative below zero, which is what 14.7's prohibition actually
        // means: a signed tick is legal (14.7's first sentence) and a late-spawned particle
        // is not.
        // Worked out independently of `compile`. 24 fps, four ticks per frame, so the
        // simulation rate is 96 ticks per second and one tick is 1/96 s.
        //   position value   = -1 + 1/3 = -2/3 frames
        //   t                = (-2/3)/24 = -1/36 s
        //   t * rate         = (-1/36) * 96 = -8/3 = -2.66... -> ceil = -2
        //   time_of_tick(-2) = -2/96 = -1/48 s
        //   age              = -1/48 - (-1/36) = (-3 + 4)/144 = 1/144 s
        let schedule = TickSchedule::compile(
            &clock(4),
            &[event(
                "pre",
                FramePos::new(-1, Rational::new(1, 3).unwrap()).unwrap(),
                0,
                EventKind::Signal,
            )],
        )
        .unwrap();
        let tick = schedule.scheduled_ticks().next().expect("one event");
        assert_eq!(
            tick,
            SimTick(-2),
            "an event before zero schedules before zero"
        );
        assert!(tick.is_negative());
        assert_eq!(
            schedule.at(tick)[0].age,
            Rational::new(1, 144).unwrap(),
            "the ceiling moves the tick later, and the age is exactly that much"
        );
        assert!(
            schedule.at(tick)[0].age >= Rational::from_i128(0),
            "14.7's prohibition holds below zero as well as above it"
        );

        // And a negative position already on a boundary has zero age. The case above would
        // have hidden a broken boundary rule: -1 + 1/2 frames is -1/48 s, and -1/48 * 96 is
        // -2 exactly, so the tick does not move and there is nothing to correct.
        let on_boundary = TickSchedule::compile(
            &clock(4),
            &[event(
                "pre2",
                FramePos::new(-1, Rational::new(1, 2).unwrap()).unwrap(),
                0,
                EventKind::Signal,
            )],
        )
        .unwrap();
        assert_eq!(on_boundary.scheduled_ticks().next(), Some(SimTick(-2)));
        assert_eq!(
            on_boundary.at(SimTick(-2))[0].age,
            Rational::from_i128(0),
            "an exact boundary below zero behaves as one above it"
        );
    }
}
