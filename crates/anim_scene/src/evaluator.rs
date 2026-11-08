//! The stateless evaluation entry point (spec 13.3, 13.5, 13.6, 13.9).
//!
//! 13.9 gives two functions and this file is those two, in the order 13.5 lists its steps.
//! Neither takes a `&mut` anywhere, and that is the shape of 13.2's rule that no
//! downstream system reads mutable project internals: a snapshot is built *from* a
//! document, and the only writer of a document remains
//! `anim_commands::CommandProcessor` (DECISIONS.md #31.10).
//!
//! Two properties decide whether an evaluation here is correct, and both are 13.6.
//!
//! **Statelessness.** The time at a position is computed from that position, never from
//! the previous frame, so seeking to frame 4 000 costs exactly what seeking to frame 4
//! costs and an export cannot accumulate drift across ten thousand frames. 13.12's
//! "repeated snapshot hashes" clause is this property, and [`crate::cache`]'s tests are
//! its consequence.
//!
//! **One event delivery rule.** 13.6: "Arbitrary-position evaluation does not
//! independently redefine event delivery." The frame an arbitrary position belongs to is
//! derived once, in [`containing_frame`], from the fact that `FramePos`'s subframe is
//! normalized into `[0, 1)` (DECISIONS.md #22) — so `position.frame` *is* the whole frame
//! that owns the position, and `evaluate_frame(n)` and `evaluate(at_frame(n))` are the same
//! call. `camera::active_camera_at` reads the whole cut sequence for the same reason: a
//! camera decision made from a different order than the event log is a second delivery
//! rule, whatever it says about it.

use std::collections::BTreeMap;
use std::sync::Arc;

use anim_core::id::{NodeId, SceneId};
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::{AudioClip, EventKind, NodeData, ProjectDocument, SceneData};

use crate::animation::{self, blend_layers, PropertySources};
use crate::camera::{self, EvaluatedCamera};
use crate::events::{EventSources, SceneEvent};
use crate::hierarchy::{self, NodeView, WorldEntry};
use crate::snapshot::{AudioCue, EvaluatedNode, SceneSnapshot, SimulationInput};
use crate::{position_label, Result, SceneError};

/// Evaluate one scene at one exact position (spec 13.9's `SceneEvaluator::evaluate`).
///
/// Stateless, and therefore referentially transparent: the same document and position
/// always produce an equal snapshot, which is what lets [`crate::SnapshotCache`] key on the pair
/// and what lets two processes be compared by digest.
///
/// # Errors
///
/// Every category in [`crate::SceneError`] that 13.11 attaches to evaluation. A failure
/// returns no snapshot at all, which is 13.11's "never emit a snapshot stamped with
/// another revision" honoured structurally: there is no partially evaluated snapshot in
/// this API to mis-stamp.
pub struct SceneEvaluator;

impl SceneEvaluator {
    /// The snapshot at one exact position.
    pub fn evaluate(
        document: &ProjectDocument,
        scene_id: SceneId,
        position: FramePos,
    ) -> Result<Arc<SceneSnapshot>> {
        let scene = document
            .scene(&scene_id)
            .ok_or_else(|| SceneError::MissingScene {
                scene: scene_id.to_string(),
            })?;
        let frame_rate = document.frame_rate;
        let built = evaluate_scene(scene, scene_id, document.revision, frame_rate, position)?;
        Ok(Arc::new(built))
    }

    /// The snapshot for whole frame `frame` (spec 13.9's second entry point, 13.10's
    /// "export drives evaluation at fixed whole-frame positions").
    ///
    /// The whole-frame case of [`SceneEvaluator::evaluate`], and deliberately nothing more:
    /// two calls that meant different things would be two delivery rules, which 13.6
    /// forbids. The checked addition is the one way a frame number fails here, and it is
    /// reported as 13.11's time overflow rather than wrapping onto a negative frame.
    pub fn evaluate_frame(
        document: &ProjectDocument,
        scene_id: SceneId,
        frame: i64,
    ) -> Result<Arc<SceneSnapshot>> {
        SceneEvaluator::evaluate(document, scene_id, FramePos::at_frame(frame))
    }
}

/// The evaluation itself, shared by both entry points and by nothing else.
///
/// Split out because `SceneSnapshot` is built from a `&SceneData` while the public API
/// takes a document, and because the sequence of 13.5's steps is easier to read as one
/// function than as a method with two calls into it.
fn evaluate_scene(
    scene: &SceneData,
    scene_id: SceneId,
    revision: anim_core::id::Revision,
    frame_rate: FrameRate,
    position: FramePos,
) -> Result<SceneSnapshot> {
    let nodes: Vec<NodeData> = scene.nodes().cloned().collect();
    let views: Vec<NodeView> = nodes
        .iter()
        .map(|node| NodeView {
            id: node.node_id,
            parent: node.parent,
            sibling_order: node.sibling_order,
        })
        .collect();

    // Step 2, in 13.5's order: local properties at an exact position. The transform half
    // has to be finished before the hierarchy pass, which is why `local_transform` is a
    // public function rather than a line inside `evaluate_node`.
    let sources = PropertySources::of(scene);
    let locals = animation::local_matrices(&nodes, &sources, &position)?;

    // Step 3, with the one layer this build has. See `animation::blend_layers` for why it
    // is a call and not a comment.
    //
    // Step 4: world transforms in validated topological order.
    let entries = hierarchy::visit(&views, &locals)?;
    let worlds: BTreeMap<NodeId, [[f64; 3]; 3]> = entries
        .iter()
        .map(|entry| (entry.id, entry.world))
        .collect();

    let evaluated = evaluate_nodes(&nodes, &entries, &sources, &worlds, position)?;

    // Steps 5 and 6: camera, audio cues and the events of the frame that owns the
    // position.
    let sources_of_events = EventSources::of(scene);
    let cuts = sources_of_events.all_occurrences()?;
    let events = sources_of_events.occurrences(frame_rate, containing_frame(position))?;
    let cameras: Arc<[EvaluatedCamera]> = camera::evaluate(&cuts, &worlds, &position)?.into();
    let audio_cues = audio_cues_from(&events, &evaluated);

    // WP08's clips are full-timeline and position-independent: every authored clip is
    // exposed at every position, unlike `audio_cues`, which is the per-frame event view.
    // The evaluated clip is the authored clip — immutable exposure, no per-position
    // computation — so this is a clone of the scene's collection in id order.
    let audio_clips: Arc<[AudioClip]> = scene.audio_clips().cloned().collect::<Vec<_>>().into();

    // Step 7, in 13.5's numbering: what simulation may read.
    let simulation_inputs: Arc<[SimulationInput]> = evaluated
        .iter()
        .map(|node| SimulationInput {
            node: node.id,
            world_transform: node.world_transform,
            z: node.z,
            visible: node.visible,
        })
        .collect::<Vec<_>>()
        .into();

    // Step 8: freeze, and the optional canonical hash that
    // `crate::snapshot::snapshot_hash` supplies on request.
    let time = frame_rate
        .time_of(position)
        .map_err(|e| SceneError::TimeOverflow {
            operation: "snapshot time",
            value: format!("{}: {e}", position_label(&position)),
        })?;

    Ok(SceneSnapshot {
        scene_id,
        revision,
        position,
        time,
        nodes: evaluated,
        cameras,
        audio_cues,
        simulation_inputs,
        events: events.into(),
        audio_clips,
    })
}

/// Steps 5 and 3's visible result: every node's evaluated fields, in 13.7's visit order.
///
/// The order comes from [`hierarchy::visit`]'s entries rather than from the scene's map,
/// which is what makes 13.7's "visit roots in NodeId order and children in persisted
/// sibling order" observable in a snapshot instead of only in a traversal nobody sees.
fn evaluate_nodes(
    nodes: &[NodeData],
    entries: &[WorldEntry],
    sources: &PropertySources<'_>,
    worlds: &BTreeMap<NodeId, [[f64; 3]; 3]>,
    position: FramePos,
) -> Result<Arc<[EvaluatedNode]>> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        // `visit` carries the index of the node it produced this entry for, so the pairing
        // is total and 13.11's taxonomy stays the size of the defect list rather than
        // growing an entry for an internal inconsistency.
        let node = &nodes[entry.index];
        // The matrix passed in is the *parent's*, and `evaluate_node` composes this node's
        // authored local onto it. That is the same product `visit` formed to produce
        // `entry.world`, and `a_parent_offset_reaches_a_child_through_the_two_passes`
        // asserts the two are equal — the assertion that makes the duplication a check
        // rather than a cost.
        let parent_world = match entry.parent {
            None => hierarchy::IDENTITY,
            Some(parent) => worlds.get(&parent).copied().ok_or_else(|| {
                // Not `unwrap_or(IDENTITY)`. A parent whose matrix the hierarchy pass did
                // not produce is a chain that never reached a root, which is exactly what
                // `visit` names `HierarchyCycle` for; defaulting it to the identity would
                // draw an animation under a parent that was never placed. Unreachable while
                // `visit` enqueues a child only after its own entry exists, and the report
                // stays right if that ever stops being true.
                SceneError::HierarchyCycle {
                    node: entry.id.to_string(),
                }
            })?,
        };
        let one = animation::evaluate_node(node, parent_world, sources, &position)?;
        out.push(blend_layers(one));
    }
    Ok(out.into())
}

/// The cues among one frame's events, each with the asset its node is showing.
///
/// 13.12's "audio cue identity" is answered by [`SceneEvent::occurrence_id`], and 13.10's
/// conversion to exact sample positions needs `position` plus the project's sample rate —
/// which is why there is no sample index here. An event that is not a cue is not a cue, so
/// the audio array is exactly the frame's `AudioCue` events, in the same order, and a cue
/// keeps the identity the event log gave it.
fn audio_cues_from(events: &[SceneEvent], nodes: &[EvaluatedNode]) -> Arc<[AudioCue]> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::AudioCue)
        .map(|event| AudioCue {
            occurrence_id: event.occurrence_id,
            node: event.source_id,
            position: event.time,
            asset: nodes
                .iter()
                .find(|node| node.id == event.source_id)
                .and_then(|node| node.asset),
        })
        .collect::<Vec<_>>()
        .into()
}

/// The whole frame that owns an arbitrary position.
///
/// `position.frame`, and it is worth saying why in one sentence rather than leaving it to
/// luck: `FramePos`'s constructor keeps the subframe in `[0, 1)` (DECISIONS.md #22), so
/// `time_of(position)` is at or after `time_of(position.frame)` and strictly before
/// `time_of(position.frame + 1)` — which is exactly 13.6's ownership interval, with the
/// upper-boundary event already assigned to the next frame because a `FramePos` at `n + 1`
/// has frame `n + 1` and zero subframe.
#[must_use]
pub const fn containing_frame(position: FramePos) -> i64 {
    position.frame
}

/// 13.6's boundary sentence, expressed as the pair of times it compares.
///
/// Exposed because [`crate::events`] and this file must agree on the same half-open rule,
/// and a second spelling of it somewhere else is how the two drift apart.
///
/// # Errors
///
/// [`SceneError::TimeOverflow`] if the frame or the rate cannot produce a time.
pub fn frame_bounds(frame_rate: FrameRate, frame: i64) -> Result<(Rational, Rational)> {
    let lower = time_at(frame_rate, frame)?;
    let upper = time_at(
        frame_rate,
        frame
            .checked_add(1)
            .ok_or_else(|| SceneError::TimeOverflow {
                operation: "advance frame for event range",
                value: frame.to_string(),
            })?,
    )?;
    Ok((lower, upper))
}

fn time_at(frame_rate: FrameRate, frame: i64) -> Result<Rational> {
    frame_rate
        .time_of(FramePos::at_frame(frame))
        .map_err(|e| SceneError::TimeOverflow {
            operation: "frame boundary time",
            value: format!("frame {frame}: {e}"),
        })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the workspace denial governs production code"
    )]

    use super::*;
    use anim_core::id::{EventDefinitionId, ProjectId, Revision, TrackId};
    use anim_project::{
        AudioClip, ChannelLayout, Easing, EventDefinition, NodeData, PropertyCurve, PropertyKey,
        PropertyPath, PropertyValue, TrackData,
    };
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn scene_id() -> SceneId {
        SceneId::derive(&namespace(), b"shot01")
    }

    fn node_id(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    fn r(num: i128, den: i128) -> Rational {
        Rational::new(num, den).unwrap()
    }

    fn hero() -> NodeData {
        NodeData::new(node_id("hero"), None, 0, "hero")
    }

    fn x_track(from: i64, to: i64, low: i128, high: i128) -> TrackData {
        TrackData::new(
            TrackId::derive(&namespace(), b"x"),
            node_id("hero"),
            PropertyPath::new(animation::PATH_TRANSFORM_X).unwrap(),
            PropertyCurve::new(
                [
                    PropertyKey::new(
                        FramePos::at_frame(from),
                        PropertyValue::Rational(Rational::from_i128(low)),
                        Easing::Linear,
                    ),
                    PropertyKey::new(
                        FramePos::at_frame(to),
                        PropertyValue::Rational(Rational::from_i128(high)),
                        Easing::Linear,
                    ),
                ],
                None,
            )
            .unwrap(),
        )
    }

    fn document(nodes: Vec<NodeData>, tracks: Vec<TrackData>) -> ProjectDocument {
        document_with(FrameRate::new(24, 1).unwrap(), nodes, tracks, vec![])
    }

    fn document_with(
        rate: FrameRate,
        nodes: Vec<NodeData>,
        tracks: Vec<TrackData>,
        events: Vec<EventDefinition>,
    ) -> ProjectDocument {
        let scene = SceneData::from_parts("shot01", nodes, tracks, [], events, []).unwrap();
        let mut out = ProjectDocument::new(ProjectId::derive(&namespace(), b"p"), rate);
        assert!(
            out.insert_scene(scene_id(), scene).is_none(),
            "a fresh document displaces nothing"
        );
        out
    }

    fn evaluate_at(doc: &ProjectDocument, at: FramePos) -> Arc<SceneSnapshot> {
        SceneEvaluator::evaluate(doc, scene_id(), at).unwrap()
    }

    #[test]
    fn a_moving_node_at_two_positions_agrees_with_the_interval_arithmetic() {
        let doc = document(vec![hero()], vec![x_track(0, 10, 0, 100)]);
        assert_eq!(
            evaluate_at(&doc, FramePos::at_frame(0)).nodes[0].world_transform[0][2],
            0.0
        );
        assert_eq!(
            evaluate_at(&doc, FramePos::at_frame(5)).nodes[0].world_transform[0][2],
            50.0
        );
        // And the subframe case 13.12 names, with the answer written by hand: a quarter of
        // the way into frame 7 is 7.25/10 of the way to 100.
        let quarter = evaluate_at(&doc, FramePos::new(7, r(1, 4)).unwrap());
        assert!(
            (quarter.nodes[0].world_transform[0][2] - 72.5).abs() < 1e-12,
            "{}",
            quarter.nodes[0].world_transform[0][2]
        );
    }

    #[test]
    fn evaluation_is_stateless_so_the_visit_order_is_not_an_accumulator() {
        // The failure 13.6's first sentence exists to prevent, tested the way it can
        // actually be caught: a frame far along, reached directly, equals the answer
        // reached by walking every frame before it.
        let doc = document(vec![hero()], vec![x_track(0, 4_000, 0, 4_000)]);
        let direct = evaluate_at(&doc, FramePos::at_frame(4_000));
        let mut walked: Option<Arc<SceneSnapshot>> = None;
        for frame in 0..=4_000 {
            walked = Some(SceneEvaluator::evaluate_frame(&doc, scene_id(), frame).unwrap());
        }
        let walked = walked.expect("the loop ran");
        assert_eq!(
            direct.nodes[0].world_transform, walked.nodes[0].world_transform,
            "and no drift was accumulated over the four thousand frames in between"
        );
        assert_eq!(direct.time, walked.time);
        assert_eq!(
            crate::snapshot::snapshot_hash(&direct).unwrap(),
            crate::snapshot::snapshot_hash(&walked).unwrap()
        );
    }

    #[test]
    fn negative_frames_evaluate_and_own_their_events() {
        // 13.12's "negative frame boundaries". Floor, not truncation, is what makes frame
        // -1 with a half subframe still belong to frame -1, and DECISIONS.md #22 records
        // the reason the type is that way.
        let doc = document(vec![hero()], vec![x_track(-10, 0, -100, 0)]);
        let mid = evaluate_at(&doc, FramePos::new(-5, Rational::from_i128(0)).unwrap());
        assert!(
            (mid.nodes[0].world_transform[0][2] + 50.0).abs() < 1e-12,
            "halfway from -100 to 0 is -50, got {}",
            mid.nodes[0].world_transform[0][2]
        );
        assert_eq!(containing_frame(FramePos::new(-1, r(1, 2)).unwrap()), -1);
        let before_zero = FramePos::new(-1, r(1, 2)).unwrap();
        assert!(
            evaluate_at(&doc, before_zero).time < evaluate_at(&doc, FramePos::at_frame(0)).time,
            "and exact time keeps the order"
        );
    }

    #[test]
    fn a_whole_frame_call_and_a_position_call_at_the_same_instant_are_one_snapshot() {
        // 13.6's "does not independently redefine event delivery", checked as equality of
        // the two entry points 13.9 lists rather than argued about.
        let cue = EventDefinition::new(
            EventDefinitionId::derive(&namespace(), b"cue"),
            FramePos::at_frame(3),
            0,
            node_id("hero"),
            EventKind::AudioCue,
            0,
        );
        let doc = document_with(
            FrameRate::new(24, 1).unwrap(),
            vec![hero()],
            vec![],
            vec![cue],
        );
        let by_frame = SceneEvaluator::evaluate_frame(&doc, scene_id(), 3).unwrap();
        let by_position =
            SceneEvaluator::evaluate(&doc, scene_id(), FramePos::at_frame(3)).unwrap();
        assert_eq!(*by_frame, *by_position);
        assert_eq!(by_frame.events.len(), 1);
        // A position inside frame 3 sees frame 3's cue, and the boundary case belongs to
        // frame 4 — the two halves of the exclusivity clause, from the snapshot side.
        let inside =
            SceneEvaluator::evaluate(&doc, scene_id(), FramePos::new(3, r(3, 4)).unwrap()).unwrap();
        assert_eq!(inside.events.len(), 1);
        let after = SceneEvaluator::evaluate(&doc, scene_id(), FramePos::at_frame(4)).unwrap();
        assert!(after.events.is_empty());
    }

    #[test]
    fn a_scene_the_document_does_not_have_is_named_and_not_evaluated_as_empty() {
        let doc = document(vec![hero()], vec![]);
        let absent = SceneId::derive(&namespace(), b"absent");
        let Err(SceneError::MissingScene { scene }) =
            SceneEvaluator::evaluate(&doc, absent, FramePos::at_frame(0))
        else {
            panic!("an empty snapshot for a missing scene is the silent skip 13.11 forbids");
        };
        assert_eq!(scene, absent.to_string());
    }

    #[test]
    fn a_snapshot_is_stamped_with_the_revision_it_was_built_from() {
        let mut doc = document(vec![hero()], vec![]);
        assert_eq!(
            evaluate_at(&doc, FramePos::at_frame(0)).revision,
            doc.revision,
            "a new document's revision is zero and the snapshot says so"
        );
        doc.revision = Revision(12);
        assert_eq!(
            evaluate_at(&doc, FramePos::at_frame(0)).revision,
            Revision(12)
        );
    }

    #[test]
    fn a_camera_cut_selects_the_camera_the_snapshot_says_is_on() {
        // 13.12's "camera cuts", end to end: two authored camera nodes, cuts naming each,
        // and one snapshot saying which is on at a position between them.
        let camera_a = NodeData::new(node_id("cam-a"), None, 0, "cam-a");
        let camera_b = NodeData::new(node_id("cam-b"), None, 1, "cam-b");
        let cuts = vec![
            EventDefinition::new(
                EventDefinitionId::derive(&namespace(), b"cut-a"),
                FramePos::at_frame(1),
                0,
                node_id("cam-a"),
                EventKind::CameraCut,
                0,
            ),
            EventDefinition::new(
                EventDefinitionId::derive(&namespace(), b"cut-b"),
                FramePos::at_frame(6),
                0,
                node_id("cam-b"),
                EventKind::CameraCut,
                0,
            ),
        ];
        let mut camera_b = camera_b;
        camera_b.defaults.insert(
            PropertyPath::new(animation::PATH_TRANSFORM_X).unwrap(),
            PropertyValue::Rational(r(40, 1)),
        );
        let doc = document_with(
            FrameRate::new(24, 1).unwrap(),
            vec![camera_a, camera_b],
            vec![],
            cuts,
        );

        let before = evaluate_at(&doc, FramePos::at_frame(3));
        let on_before: Vec<&EvaluatedCamera> = before.cameras.iter().filter(|c| c.is_on).collect();
        assert_eq!(on_before.len(), 1, "exactly one camera is on");
        assert_eq!(on_before[0].node, node_id("cam-a"));

        let after = evaluate_at(&doc, FramePos::at_frame(6));
        let on_after: Vec<&EvaluatedCamera> = after.cameras.iter().filter(|c| c.is_on).collect();
        assert_eq!(on_after[0].node, node_id("cam-b"));
        assert_eq!(
            on_after[0].world_transform[0][2], 40.0,
            "and it is at its own position"
        );
        assert_eq!(
            after.cameras.len(),
            2,
            "the inactive camera keeps its pose, which is what WP16 plans a move with"
        );
        assert_eq!(
            after.events.len(),
            1,
            "and frame 6's event log holds only the cut that happened in it"
        );
    }

    #[test]
    fn an_audio_cue_keeps_the_identity_the_event_log_gave_it() {
        // 13.12's "audio cue identity". The occurrence id in `audio_cues` is the same
        // derived value as in `events`, so a consumer that keyed a device buffer on one
        // can be found by the log that ordered it.
        let mut hero = hero();
        hero.defaults.insert(
            PropertyPath::new(animation::PATH_ASSET).unwrap(),
            PropertyValue::Asset(anim_core::id::AssetId::derive(&namespace(), b"boom")),
        );
        let cue = EventDefinition::new(
            EventDefinitionId::derive(&namespace(), b"cue"),
            FramePos::at_frame(2),
            0,
            node_id("hero"),
            EventKind::AudioCue,
            0,
        );
        let doc = document_with(
            FrameRate::new(24, 1).unwrap(),
            vec![hero],
            vec![],
            vec![cue],
        );
        let snapshot = evaluate_at(&doc, FramePos::at_frame(2));
        assert_eq!(snapshot.audio_cues.len(), 1);
        assert_eq!(
            snapshot.audio_cues[0].occurrence_id,
            snapshot.events[0].occurrence_id
        );
        assert_eq!(
            snapshot.audio_cues[0].asset, snapshot.nodes[0].asset,
            "the cue plays what the node is showing, which is one binding and not two"
        );
        assert_eq!(snapshot.audio_cues[0].position, FramePos::at_frame(2));
    }

    #[test]
    fn the_snapshot_order_is_the_hierarchy_order_and_the_two_are_not_the_same_thing() {
        // 13.7 separates "visit order" from "draw extraction keys", and 13.8 gives the node
        // array one order. Roots by id, children by persisted sibling order, asserted as
        // the snapshot's own sequence.
        let root = NodeData::new(node_id("root"), None, 0, "root");
        let mut first = NodeData::new(node_id("a-cel"), Some(root.node_id), 9, "a");
        let mut second = NodeData::new(node_id("b-cel"), Some(root.node_id), 1, "b");
        first.sibling_order = 9;
        second.sibling_order = 1;
        let doc = document(vec![root, first, second], vec![]);
        let snapshot = evaluate_at(&doc, FramePos::at_frame(0));
        let order: Vec<NodeId> = snapshot.nodes.iter().map(|n| n.id).collect();
        assert_eq!(order[0], node_id("root"));
        assert_eq!(
            order[1],
            node_id("b-cel"),
            "sibling order 1 before 9, whatever the ids say"
        );
        assert_eq!(order[2], node_id("a-cel"));
    }

    #[test]
    fn two_evaluations_of_one_document_hash_alike_and_a_changed_one_does_not() {
        // The same claim 13.12 makes about two processes, at the strength a single process
        // can prove: the digest is a function of the snapshot and of nothing else. The
        // cross-process half is `anim_test_support`'s `wp04_isolation`, which runs this in
        // two fresh processes and compares the printed digests.
        let doc = document(vec![hero()], vec![x_track(0, 10, 0, 100)]);
        let first = evaluate_at(&doc, FramePos::at_frame(4));
        let second = evaluate_at(&doc, FramePos::at_frame(4));
        assert_eq!(
            crate::snapshot::snapshot_hash(&first).unwrap(),
            crate::snapshot::snapshot_hash(&second).unwrap()
        );

        // A real change: one more frame authored on the track, at a revision above.
        let mut changed = document(vec![hero()], vec![x_track(0, 10, 0, 101)]);
        changed.revision = Revision(1);
        let changed_snapshot = evaluate_at(&changed, FramePos::at_frame(4));
        assert_ne!(
            crate::snapshot::snapshot_hash(&first).unwrap(),
            crate::snapshot::snapshot_hash(&changed_snapshot).unwrap(),
            "a different authored value must move the digest"
        );
        assert_ne!(first.revision, changed_snapshot.revision);
    }

    #[test]
    fn the_frame_bounds_are_the_half_open_interval_13_6_describes() {
        let ntsc = FrameRate::new(30_000, 1_001).unwrap();
        let (lower, upper) = frame_bounds(ntsc, 4).unwrap();
        assert_eq!(
            (lower, upper),
            (r(4_004, 30_000), r(5_005, 30_000)),
            "exact at an NTSC boundary, where no decimal spelling of the frame time exists"
        );
        assert!(frame_bounds(ntsc, i64::MAX).is_err());
    }

    #[test]
    fn a_node_with_an_unusable_key_list_is_reported_and_not_defaulted_away() {
        // The evaluator's own reachability claim for `InvalidEasing`: a bool authored with
        // `Linear` is a legal document and an impossible evaluation.
        let mut hero = hero();
        hero.defaults.insert(
            PropertyPath::new(animation::PATH_VISIBLE).unwrap(),
            PropertyValue::Bool(true),
        );
        let track = TrackData::new(
            TrackId::derive(&namespace(), b"visible"),
            node_id("hero"),
            PropertyPath::new(animation::PATH_VISIBLE).unwrap(),
            PropertyCurve::new(
                [
                    PropertyKey::new(
                        FramePos::at_frame(0),
                        PropertyValue::Bool(true),
                        Easing::Linear,
                    ),
                    PropertyKey::new(
                        FramePos::at_frame(4),
                        PropertyValue::Bool(false),
                        Easing::Linear,
                    ),
                ],
                None,
            )
            .unwrap(),
        );
        let doc = document(vec![hero], vec![track]);
        let Err(SceneError::InvalidEasing {
            position, detail, ..
        }) = SceneEvaluator::evaluate(&doc, scene_id(), FramePos::at_frame(2))
        else {
            panic!("there is no fraction of a switch");
        };
        assert_eq!(position, "4");
        assert!(detail.contains("switches at a key"), "{detail}");
    }

    #[test]
    fn an_authored_audio_clip_is_full_timeline_not_per_frame() {
        // WP08's clips reach the snapshot at every position, unlike audio_cues, which fire
        // only in their frame. A clip spanning frames 2..6 is present at frame 0 (before it
        // starts) and frame 100 (after it ends), proving the exposure is position-independent
        // and that the mixer, not the evaluator, decides what intersects the export range.
        let clip = AudioClip::new(
            TrackId::derive(&namespace(), b"music"),
            anim_core::id::AssetId::derive(&namespace(), b"score"),
            FramePos::at_frame(2),
            FramePos::at_frame(6),
            Rational::from_i128(0),
            false,
            Rational::from_i128(1),
            Rational::from_i128(0),
            Rational::from_i128(0),
            false,
            ChannelLayout::Stereo,
        );
        let mut doc = document(vec![hero()], vec![]);
        doc.scene_mut(&scene_id())
            .unwrap()
            .insert_audio_clip(clip.clone())
            .unwrap();

        for frame in [0, 100] {
            let snapshot = evaluate_at(&doc, FramePos::at_frame(frame));
            assert_eq!(
                snapshot.audio_clips.len(),
                1,
                "frame {frame} carries the clip"
            );
            assert_eq!(snapshot.audio_clips[0], clip);
            assert!(
                snapshot.audio_cues.is_empty(),
                "and no cue fires: a clip is a timeline range, not a per-frame event"
            );
        }
    }
}
