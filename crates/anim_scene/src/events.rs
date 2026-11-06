//! Event extraction, occurrence identities and the canonical sort (spec 13.3, 13.5 steps
//! 6 and 7, 13.6, 13.7, 13.8, 13.9).
//!
//! Three rules hold this file together.
//!
//! **Frame ownership.** 13.6: "A frame snapshot for whole frame `n` owns events in
//! `[time_of(n), time_of(n+1))`. An event exactly at the upper boundary belongs to the
//! next frame." The half-open interval is evaluated on exact rationals through
//! `FrameRate::time_of`, so the boundary is decided by arithmetic rather than by a float
//! comparison that could land on either side of it. 13.12's "adjacent-frame event
//! exclusivity" clause is that sentence tested from both ends.
//!
//! **Identity.** 13.7: "derive `EventOccurrenceId` from `(EventDefinitionId`,
//! `occurrence_index)`". The index is `0` for every occurrence this package can produce,
//! and that is a statement about the authored data rather than a placeholder: one
//! [`EventDefinition`] carries exactly one authored position, so it fires once. The index
//! is in the identity anyway, because the definition that eventually fires more than once
//! — WP14's emitter scheduling is the package that authors repetition — extends the index
//! without changing how an identity is formed, and the identity consumers already hold for
//! the first firing does not move under them.
//!
//! **Order.** 13.7's five-term key `(event_time, priority, source_node_id, local_sequence,
//! occurrence_id)` is total because its last term is an identifier and identifiers are
//! unique, which is what makes 13.12's "occurrence ordering under shuffled source
//! insertion" a property rather than a hope. The other four are authored data, and
//! `local_sequence` is 13.7's insistence: "persisted or derived from a stable source key,
//! never hash-map or filesystem enumeration".

use std::sync::Arc;

use anim_core::hash::CanonicalBytes;
use anim_core::id::{EventDefinitionId, NodeId};
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::{
    EventDefinition, EventKind, EventOccurrenceId, ProjectDocument, SceneData, SceneId,
};
use uuid::Uuid;

use crate::{position_label, Result, SceneError};

/// The domain separator inside an occurrence identity.
///
/// ADR-0004 consequence 5's registry, the same shape as every other domain in this
/// workspace. It goes into the pre-image rather than being assumed, because a v5 identity
/// is a hash of a name and two consumers deriving the same `(definition, index)` under
/// different names would get different ids with nothing in the bytes to explain why.
pub const OCCURRENCE_DOMAIN: &str = "anim_engine/event_occurrence/v1";

/// The namespace occurrence identities are derived in: the RFC 4122 DNS namespace, spelled
/// as the u128 its hyphenated form denotes.
///
/// The same namespace every other derived identifier in this workspace uses —
/// `anim_core::id`'s tests, `anim_assets/tests/store.rs`, `anim_project::ID_NAMESPACE` —
/// so a new id type does not introduce a second convention. Uniqueness against those other
/// derivations comes from [`OCCURRENCE_DOMAIN`] inside the name bytes, not from a different
/// namespace.
pub const OCCURRENCE_NAMESPACE: Uuid = Uuid::from_u128(0x6ba7b810_9dad_11d1_80b4_00c04fd430c8);

/// One event's occurrence in the canonical sequence (spec 13.8's `SceneEvent`).
///
/// Field order is 13.8's listing order, which is also the encoding order, which is the rule
/// ADR-0005 Decision 9 applies to every hashed object in this project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneEvent {
    /// The identity 13.7 derives and 13.8 puts first. It is the total tie-break of the
    /// ordering, so an event log assembled from two insertion orders is one log.
    pub occurrence_id: EventOccurrenceId,
    /// The authored position, which is `event_time`. A `FramePos` and not a float, because
    /// this field *is* the first sort term and 10.12 forbids a rounded value in
    /// deterministic state.
    pub time: FramePos,
    pub priority: i32,
    pub source_id: NodeId,
    pub local_sequence: u32,
    pub kind: EventKind,
}

impl SceneEvent {
    /// 13.7's ordering key, spelled as the tuple the spec spells as an order.
    #[must_use]
    pub fn order_key(&self) -> (FramePos, i32, NodeId, u32, EventOccurrenceId) {
        (
            self.time,
            self.priority,
            self.source_id,
            self.local_sequence,
            self.occurrence_id,
        )
    }

    /// Encode into a snapshot's canonical stream.
    ///
    /// Infallible, unlike the other `encode` methods in this crate: 13.8 gives this type no
    /// `f64` field, so there is no nonfinite value to refuse.
    pub fn encode(&self, cp: &mut CanonicalBytes) {
        cp.fixed_bytes(self.occurrence_id.as_bytes());
        cp.i64(self.time.frame);
        cp.rational(&self.time.subframe());
        cp.i32(self.priority);
        cp.fixed_bytes(self.source_id.as_bytes());
        cp.u32(self.local_sequence);
        cp.text(self.kind.as_str());
    }
}

impl Ord for SceneEvent {
    /// The comparator `sort_by_key` would produce over [`SceneEvent::order_key`], written
    /// as `Ord` so that any consumer that puts events in a set or a map gets the same order
    /// the extraction does. 13.7's "UUID comparison uses its 16 bytes" is honoured by
    /// `anim_core::id`'s own implementation of that rule rather than by `uuid`'s, whose
    /// representation is caret-pinned and therefore not this project's to depend on.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order_key().cmp(&other.order_key())
    }
}

impl PartialOrd for SceneEvent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The identity of one firing of one definition (spec 13.7).
///
/// # Errors
///
/// [`SceneError::EventOrderOverflow`] if the domain tag cannot be encoded — a defect in
/// this build rather than in a document, and reported as one.
pub fn occurrence_id(
    definition: &EventDefinitionId,
    occurrence_index: u64,
) -> Result<EventOccurrenceId> {
    let mut cp = CanonicalBytes::with_domain(OCCURRENCE_DOMAIN).map_err(|e| {
        SceneError::EventOrderOverflow {
            event: definition.to_string(),
            detail: format!("the occurrence domain could not be encoded: {e}"),
        }
    })?;
    cp.fixed_bytes(definition.as_bytes());
    cp.u64(occurrence_index);
    Ok(EventOccurrenceId::derive(
        &OCCURRENCE_NAMESPACE,
        cp.as_slice(),
    ))
}

/// The authored events of one scene, as a sequence that can be handed over in any order.
///
/// A view rather than a call into `SceneData`, for the reason [`crate::hierarchy`] gives:
/// extraction from an unordered input is the only way 13.12's shuffled-insertion clause can
/// be tested, and a function that required pre-sorted input would be asserting the property
/// it exists to establish. Production callers build the view from the document, so the
/// checks are still on every real path.
pub struct EventSources<'a> {
    definitions: Vec<&'a EventDefinition>,
}

impl<'a> EventSources<'a> {
    /// A scene's definitions, in the order the scene stores them.
    #[must_use]
    pub fn of(scene: &'a SceneData) -> Self {
        Self {
            definitions: scene.events().collect(),
        }
    }

    /// A sequence in arbitrary order, which is how a shuffled input and a caller holding
    /// its own list enter.
    ///
    /// Named `in_order` rather than anything `from_*`: an inherent `from_iter` shadows the
    /// `FromIterator` trait method for every reader who meets the type, and the trait
    /// would be the wrong thing to implement here — `FromIterator` promises a collection
    /// that keeps what it was given, and this one exists to accept an order it does not
    /// keep.
    #[must_use]
    pub fn in_order(items: impl IntoIterator<Item = &'a EventDefinition>) -> Self {
        Self {
            definitions: items.into_iter().collect(),
        }
    }

    /// One whole frame's events, in canonical order.
    ///
    /// # Errors
    ///
    /// [`SceneError::TimeOverflow`] if a boundary or an event time cannot be computed, and
    /// [`SceneError::EventOrderOverflow`] from [`occurrence_id`].
    pub fn occurrences(&self, frame_rate: FrameRate, frame: i64) -> Result<Vec<SceneEvent>> {
        let lower = boundary_time(frame_rate, frame)?;
        let upper = boundary_time(frame_rate, next_frame(frame)?)?;
        let mut events = Vec::new();
        for definition in &self.definitions {
            let at =
                frame_rate
                    .time_of(definition.position)
                    .map_err(|e| SceneError::TimeOverflow {
                        operation: "event time",
                        value: format!("{}: {e}", position_label(&definition.position)),
                    })?;
            // Half-open, so an event exactly on `upper` is the next frame's. 13.12 tests
            // both directions of this one comparison.
            if at >= lower && at < upper {
                events.push(occurring(definition)?);
            }
        }
        events.sort();
        Ok(events)
    }

    /// Every occurrence the scene holds, across all frames, in canonical order.
    ///
    /// Camera evaluation needs the whole sequence: "which camera is on at position `p`" is
    /// answered by the latest cut at or before `p`, and reading only `p`'s frame would
    /// answer it with whichever cut happened to share that frame. 13.6's rule that
    /// arbitrary-position evaluation "does not independently redefine event delivery" is
    /// what makes this legal rather than a second delivery rule: the order here is the same
    /// order the per-frame sequences agree on, which its own test asserts.
    ///
    /// # Errors
    ///
    /// As [`EventSources::occurrences`], for the whole scene.
    pub fn all_occurrences(&self) -> Result<Vec<SceneEvent>> {
        let mut events = Vec::with_capacity(self.definitions.len());
        for definition in &self.definitions {
            events.push(occurring(definition)?);
        }
        events.sort();
        Ok(events)
    }
}

fn occurring(definition: &EventDefinition) -> Result<SceneEvent> {
    Ok(SceneEvent {
        occurrence_id: occurrence_id(&definition.id, 0)?,
        time: definition.position,
        priority: definition.priority,
        source_id: definition.source,
        local_sequence: definition.local_sequence,
        kind: definition.kind,
    })
}

fn next_frame(frame: i64) -> Result<i64> {
    frame
        .checked_add(1)
        .ok_or_else(|| SceneError::TimeOverflow {
            operation: "advance frame for event range",
            value: frame.to_string(),
        })
}

fn boundary_time(frame_rate: FrameRate, frame: i64) -> Result<Rational> {
    frame_rate
        .time_of(FramePos::at_frame(frame))
        .map_err(|e| SceneError::TimeOverflow {
            operation: "frame boundary time",
            value: format!("frame {frame}: {e}"),
        })
}

/// The events belonging to one whole frame (spec 13.9's free function).
///
/// A caller that wants the sequence without the rest of a snapshot uses this; a caller that
/// wants both uses [`SceneEvaluator::evaluate_frame`](crate::SceneEvaluator::evaluate_frame),
/// and the two agree because they run the same extraction. 13.10's rule that simulation
/// consumes "the canonical event sequence" rests on that identity.
///
/// # Errors
///
/// [`SceneError::MissingScene`] if `scene_id` is not in the document, plus what
/// [`EventSources::occurrences`] returns.
pub fn events_for_frame(
    document: &ProjectDocument,
    scene_id: SceneId,
    frame: i64,
) -> Result<Arc<[SceneEvent]>> {
    let scene = document
        .scene(&scene_id)
        .ok_or_else(|| SceneError::MissingScene {
            scene: scene_id.to_string(),
        })?;
    let events = EventSources::of(scene)
        .occurrences(document.frame_rate, frame)?
        .into_boxed_slice();
    Ok(Arc::from(events))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the workspace denial governs production code"
    )]

    use super::*;
    use anim_core::id::{EventDefinitionId, ProjectId};

    fn event_id(label: &str) -> EventDefinitionId {
        EventDefinitionId::derive(&OCCURRENCE_NAMESPACE, label.as_bytes())
    }

    fn node_id(label: &str) -> NodeId {
        NodeId::derive(&OCCURRENCE_NAMESPACE, label.as_bytes())
    }

    fn definition(
        label: &str,
        frame: i64,
        subframe: Rational,
        priority: i32,
        source: &str,
        local: u32,
    ) -> EventDefinition {
        EventDefinition::new(
            event_id(label),
            FramePos::new(frame, subframe).unwrap(),
            priority,
            node_id(source),
            EventKind::Signal,
            local,
        )
    }

    fn rate(num: u32, den: u32) -> FrameRate {
        FrameRate::new(num, den).unwrap()
    }

    fn r(num: i128, den: i128) -> Rational {
        Rational::new(num, den).unwrap()
    }

    /// The labels of an extracted sequence, in the order it came out, by matching each
    /// occurrence back to the definition whose id it was derived from.
    fn labels(events: &[SceneEvent], definitions: &[EventDefinition]) -> Vec<String> {
        events
            .iter()
            .map(|event| {
                definitions
                    .iter()
                    .find(|d| occurrence_id(&d.id, 0).unwrap() == event.occurrence_id)
                    .map(|d| d.id.to_string())
                    .unwrap_or_else(|| "?".to_string())
            })
            .collect()
    }

    #[test]
    fn an_event_on_the_upper_boundary_belongs_to_the_next_frame() {
        // 13.6's sentence, and 13.12's "adjacent-frame event exclusivity". Exactly one of
        // the two frames may own it, and it is the later one.
        let on_four = definition("cut", 4, Rational::from_i128(0), 0, "root", 0);
        let sources = EventSources::in_order([&on_four]);
        assert!(
            sources.occurrences(rate(24, 1), 3).unwrap().is_empty(),
            "frame 3 ends at time_of(4)"
        );
        let owned = sources.occurrences(rate(24, 1), 4).unwrap();
        assert_eq!(owned.len(), 1, "and frame 4 begins there");
        assert_eq!(owned[0].time, FramePos::at_frame(4));
    }

    #[test]
    fn a_subframe_event_stays_in_its_frame_at_both_rates() {
        // The same authored position must not change frames with the rate, which is why
        // this runs 24/1 and 29.97-class 30000/1001 rather than only the exact one: at the
        // NTSC rate `time_of(7)` is not representable as a decimal at all.
        let mid = definition("hit", 7, r(1, 2), 0, "root", 0);
        let sources = EventSources::in_order([&mid]);
        for frame_rate in [rate(24, 1), rate(30_000, 1_001)] {
            assert_eq!(
                sources.occurrences(frame_rate, 7).unwrap().len(),
                1,
                "frame 7 at {frame_rate:?}"
            );
            assert!(sources.occurrences(frame_rate, 6).unwrap().is_empty());
            assert!(sources.occurrences(frame_rate, 8).unwrap().is_empty());
        }
    }

    #[test]
    fn negative_frames_own_their_events_on_the_same_half_open_rule() {
        let before_zero = definition("pre", -1, r(1, 4), 0, "root", 0);
        let sources = EventSources::in_order([&before_zero]);
        assert_eq!(sources.occurrences(rate(24, 1), -1).unwrap().len(), 1);
        assert!(sources.occurrences(rate(24, 1), 0).unwrap().is_empty());
        // And a position exactly at zero belongs to frame 0, not to frame -1: floor, not
        // truncation, which is DECISIONS.md #22's rule arriving in an event log.
        let at_zero = definition("start", 0, Rational::from_i128(0), 0, "root", 0);
        assert_eq!(
            EventSources::in_order([&at_zero])
                .occurrences(rate(24, 1), 0)
                .unwrap()
                .len(),
            1
        );
        assert!(EventSources::in_order([&at_zero])
            .occurrences(rate(24, 1), -1)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn shuffled_insertion_produces_one_sequence() {
        // 13.12's clause, stated as the property it actually tests: five definitions that
        // tie on the first four terms of 13.7's key, offered in every permutation, come out
        // as one sequence. This is what fails first if a hash map or a filesystem order
        // ever reaches an event log.
        let definitions: Vec<EventDefinition> = (0..5)
            .map(|i| {
                definition(
                    &format!("same-{i}"),
                    12,
                    Rational::from_i128(0),
                    3,
                    "root",
                    7,
                )
            })
            .collect();
        let reference = {
            let refs: Vec<&EventDefinition> = definitions.iter().collect();
            labels(
                &EventSources::in_order(refs)
                    .occurrences(rate(24, 1), 12)
                    .unwrap(),
                &definitions,
            )
        };
        let mut orders_seen = 0usize;
        for a in 0..5 {
            for b in 0..5 {
                for c in 0..5 {
                    for d in 0..5 {
                        for e in 0..5 {
                            let order = [a, b, c, d, e];
                            let distinct: std::collections::BTreeSet<usize> =
                                order.iter().copied().collect();
                            if distinct.len() != 5 {
                                continue;
                            }
                            orders_seen += 1;
                            let refs: Vec<&EventDefinition> =
                                order.iter().map(|i| &definitions[*i]).collect();
                            let again = labels(
                                &EventSources::in_order(refs)
                                    .occurrences(rate(24, 1), 12)
                                    .unwrap(),
                                &definitions,
                            );
                            assert_eq!(
                                again, reference,
                                "insertion order {order:?} changed the sequence"
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(orders_seen, 120, "all 120 permutations were run");
    }

    #[test]
    fn the_sort_follows_the_five_terms_in_the_order_the_spec_lists_them() {
        // Each case differs from the one before it in exactly one term of 13.7's key, so a
        // comparator that dropped or reordered a term fails one of these rather than
        // passing by luck. The expected answers are the label orders a person gets from
        // reading the spec sentence, which is why the definitions carry their labels into
        // the assertion: a failure that prints two uuids says nothing about which term
        // broke.
        //
        // `all_occurrences` is the function under test here rather than
        // `occurrences`, because the first case spans two frames and a per-frame
        // extraction would never see both events at once.
        let cases: Vec<(&str, Vec<(&str, EventDefinition)>)> = vec![
            // 1. time wins over everything else.
            (
                "time",
                vec![
                    (
                        "late",
                        definition("late", 9, Rational::from_i128(0), 0, "b", 0),
                    ),
                    (
                        "early",
                        definition("early", 2, Rational::from_i128(0), 5, "a", 5),
                    ),
                ],
            ),
            // 2. then priority, at one time — including a negative priority, which 13.8's
            // signed field exists to mean.
            (
                "priority",
                vec![
                    (
                        "second",
                        definition("second", 4, Rational::from_i128(0), 1, "z", 9),
                    ),
                    (
                        "first",
                        definition("first", 4, Rational::from_i128(0), -7, "a", 0),
                    ),
                ],
            ),
            // 3. then the source node id, over its 16 bytes.
            (
                "source node",
                vec![
                    (
                        "b-node",
                        definition("b-node", 4, Rational::from_i128(0), 0, "b", 0),
                    ),
                    (
                        "a-node",
                        definition("a-node", 4, Rational::from_i128(0), 0, "a", 0),
                    ),
                ],
            ),
            // 4. then the persisted local sequence, the term an operator authored.
            (
                "local sequence",
                vec![
                    (
                        "later-seq",
                        definition("later-seq", 4, Rational::from_i128(0), 0, "a", 4),
                    ),
                    (
                        "earlier-seq",
                        definition("earlier-seq", 4, Rational::from_i128(0), 0, "a", 1),
                    ),
                ],
            ),
        ];
        let expected: Vec<Vec<&str>> = vec![
            vec!["early", "late"],
            vec!["first", "second"],
            // The third case is the one whose answer is not obvious from the labels: "a"
            // sorts before "b" only over the derived bytes, so the expectation is the byte
            // order of the two node ids, read from the ids themselves rather than guessed
            // from the names.
            {
                let mut pair = vec!["a-node", "b-node"];
                if node_id("b") < node_id("a") {
                    pair.reverse();
                }
                pair
            },
            vec!["earlier-seq", "later-seq"],
        ];
        for ((what, definitions), want) in cases.iter().zip(expected.iter()) {
            let refs: Vec<&EventDefinition> = definitions.iter().map(|(_, d)| d).collect();
            let events = EventSources::in_order(refs).all_occurrences().unwrap();
            let actual: Vec<String> = events
                .iter()
                .map(|event| {
                    definitions
                        .iter()
                        .find(|(_, d)| occurrence_id(&d.id, 0).unwrap() == event.occurrence_id)
                        .map(|(label, _)| (*label).to_string())
                        .unwrap_or_else(|| "?".to_string())
                })
                .collect();
            assert_eq!(
                actual, *want,
                "the {what} term ordered {actual:?} instead of {want:?}"
            );
        }
    }

    #[test]
    fn two_events_identical_but_for_their_definition_differ_in_order() {
        // The last term is the only one left, so it has to break the tie. This is the
        // narrowest case 13.7's totality claim rests on.
        let a = definition("a", 4, Rational::from_i128(0), 0, "n", 0);
        let b = definition("b", 4, Rational::from_i128(0), 0, "n", 0);
        let forwards = EventSources::in_order([&a, &b])
            .occurrences(rate(24, 1), 4)
            .unwrap();
        let backwards = EventSources::in_order([&b, &a])
            .occurrences(rate(24, 1), 4)
            .unwrap();
        assert_eq!(forwards.len(), 2);
        assert_eq!(
            labels(&forwards, &[a.clone(), b.clone()]),
            labels(&backwards, &[a, b]),
            "the definition id is the tie-break and it does not depend on the input order"
        );
    }

    #[test]
    fn an_occurrence_identity_is_stable_and_specific() {
        let a = event_id("a");
        let b = event_id("b");
        assert_eq!(
            occurrence_id(&a, 0).unwrap(),
            occurrence_id(&a, 0).unwrap(),
            "derived, not generated: the same inputs are the same id in every process"
        );
        assert_ne!(occurrence_id(&a, 0).unwrap(), occurrence_id(&b, 0).unwrap());
        assert_ne!(
            occurrence_id(&a, 0).unwrap(),
            occurrence_id(&a, 1).unwrap(),
            "a second firing of one definition is a different occurrence"
        );
    }

    #[test]
    fn an_occurrence_identity_is_the_domain_and_the_pair_and_nothing_else() {
        // Rebuilt by hand from ADR-0004's layout so the expectation is the rule rather than
        // the function's own answer (DECISIONS.md #32's oracle rule).
        let definition = event_id("a");
        let mut expected = CanonicalBytes::with_domain(OCCURRENCE_DOMAIN).unwrap();
        expected.fixed_bytes(definition.as_bytes());
        expected.u64(0);
        assert_eq!(
            occurrence_id(&definition, 0).unwrap(),
            EventOccurrenceId::derive(&OCCURRENCE_NAMESPACE, expected.as_slice())
        );
        assert_eq!(
            occurrence_id(&definition, 0).unwrap().kind(),
            "event_occurrence"
        );
    }

    #[test]
    fn a_frame_at_the_width_limit_reports_rather_than_wrapping() {
        let edge = definition("edge", i64::MAX, Rational::from_i128(0), 0, "a", 0);
        let sources = EventSources::in_order([&edge]);
        assert!(matches!(
            sources.occurrences(rate(24, 1), i64::MAX),
            Err(SceneError::TimeOverflow { .. })
        ));
        // The frame before it still works, so the failure is the boundary and not the rule.
        assert!(sources
            .occurrences(rate(24, 1), i64::MAX - 1)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn events_can_be_extracted_without_building_a_snapshot() {
        // 13.9 lists `events_for_frame` as its own entry point and 13.10 has simulation
        // consuming event slices, so this path must not require the transform work.
        let node = NodeId::derive(&OCCURRENCE_NAMESPACE, b"n");
        let cue = definition("cue", 2, Rational::from_i128(0), 0, "n", 0);
        let mut scene = SceneData::new("shot01").unwrap();
        scene
            .insert_node(anim_project::NodeData::new(node, None, 0, "n"))
            .unwrap();
        scene.insert_event(cue).unwrap();
        let mut document =
            ProjectDocument::new(ProjectId::derive(&OCCURRENCE_NAMESPACE, b"p"), rate(24, 1));
        let scene_id = SceneId::derive(&OCCURRENCE_NAMESPACE, b"shot01");
        assert!(document.insert_scene(scene_id, scene).is_none());
        assert_eq!(events_for_frame(&document, scene_id, 2).unwrap().len(), 1);
        assert!(events_for_frame(&document, scene_id, 3).unwrap().is_empty());
        let absent = SceneId::derive(&OCCURRENCE_NAMESPACE, b"absent");
        let Err(SceneError::MissingScene { scene }) = events_for_frame(&document, absent, 2) else {
            panic!("a scene the document does not have is named, not read as empty");
        };
        assert_eq!(scene, absent.to_string());
    }

    #[test]
    fn the_whole_scene_sequence_is_the_frame_by_frame_sequence() {
        // `all_occurrences` exists for `camera::active_camera_at`. The two must agree,
        // because a camera decision made from a different order than the event log
        // contradicts 13.6's "does not independently redefine event delivery".
        let early = definition("early", 1, Rational::from_i128(0), 0, "cam-a", 0);
        let middle = definition("middle", 8, r(1, 3), 2, "cam-b", 0);
        let late = definition("late", 20, Rational::from_i128(0), -1, "cam-c", 0);
        let sources = EventSources::in_order([&late, &middle, &early]);
        let all = sources.all_occurrences().unwrap();
        assert_eq!(all.len(), 3);
        let mut per_frame = Vec::new();
        for frame in -5..30 {
            per_frame.extend(sources.occurrences(rate(24, 1), frame).unwrap());
        }
        assert_eq!(
            per_frame
                .iter()
                .map(|e| e.occurrence_id)
                .collect::<Vec<_>>(),
            all.iter().map(|e| e.occurrence_id).collect::<Vec<_>>(),
            "walking frames one at a time is the same sequence as the whole scene"
        );
    }

    #[test]
    fn a_ntsc_boundary_is_decided_by_rational_arithmetic_not_by_a_decimal() {
        // At 30000/1001, `time_of(4)` is 4 * 1001 / 30000 seconds, which is not exactly
        // representable in base 10 and would put a float comparison on either side of the
        // ownership rule. The expected times are written as the fractions the rate says.
        let ntsc = rate(30_000, 1_001);
        let cut = definition("cut", 4, Rational::from_i128(0), 0, "a", 0);
        assert_eq!(
            ntsc.time_of(FramePos::at_frame(4)).unwrap(),
            r(1_001, 7_500),
            "4 frames at 1001/30000 seconds per frame, reduced: 4004/30000 is 1001/7500"
        );
        let sources = EventSources::in_order([&cut]);
        assert_eq!(sources.occurrences(ntsc, 4).unwrap().len(), 1);
        assert!(sources.occurrences(ntsc, 3).unwrap().is_empty());
        // A position one subframe short of the boundary stays in frame 3.
        let almost = definition("almost", 3, r(1000, 1001), 0, "a", 0);
        let _ = &almost;
        assert_eq!(
            EventSources::in_order([&almost])
                .occurrences(ntsc, 3)
                .unwrap()
                .len(),
            1
        );
    }
}
