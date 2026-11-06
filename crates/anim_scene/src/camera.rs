//! Camera evaluation and cut markers (spec 13.3, 13.5 step 5, 13.8's `EvaluatedCamera`,
//! 13.12's "camera cuts").
//!
//! 13.8 names the type and gives it no fields, and 13.5 asks for "basic camera"
//! evaluation, so the first question a file like this has to answer is what a camera *is*
//! at this layer. The answer here is the one the spec already supports: 13.3 gives this
//! file "camera evaluation and cut markers", 13.12 tests camera cuts, and an event of kind
//! [`EventKind::CameraCut`] is the only authored statement in WP04's raw schema that says
//! "look here now". So a camera is a node that some cut names, its matrix is the node's
//! evaluated world transform, and the camera that is on at a position is the one named by
//! the latest cut at or before it.
//!
//! What that deliberately does not include is the rig. `CameraPose`, `CameraTrack`,
//! `MotionLayer`, `ShotSpec`, `ShotBoard`, `PathId`, `CameraId` and the whole framing
//! solver are WP15's (spec 30.8) and WP16's and WP17's, and none of their identifiers are
//! in spec 10.8's id list, so authoring them here would be building three later packages'
//! data model from inside the package whose job is to be the substrate they extend.
//! `EvaluatedCamera` will gain their fields when they do, in the commit that adds them.
//!
//! One rule from 13.6 is load-bearing here rather than decorative: arbitrary-position
//! evaluation "does not independently redefine event delivery". That is why
//! [`active_camera_at`] reads the scene's whole cut sequence instead of one frame's. A cut
//! two frames back is not in this frame's event log, but it is why this frame is looking
//! where it is looking, and a per-frame read would answer "which camera is on" with
//! whichever cut happened to share the frame — a second delivery rule in all but name.

use anim_core::id::NodeId;
use anim_core::time::FramePos;
use anim_project::EventKind;

use crate::events::SceneEvent;
use crate::{Result, SceneError};

/// One camera as evaluated for one instant (spec 13.8).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvaluatedCamera {
    /// The node the authored cuts name.
    pub node: NodeId,
    /// The node's world matrix, from [`crate::hierarchy::visit`]. A camera that is not on
    /// still has one, because WP16 and WP17 need inactive cameras' poses to plan a move
    /// before cutting to it, and dropping them from the snapshot would put that
    /// reconstruction on the far side of 13.2's boundary.
    pub world_transform: [[f64; 3]; 3],
    /// Whether the latest cut at or before the snapshot's position names this node.
    pub is_on: bool,
}

impl EvaluatedCamera {
    /// Encode into a snapshot's canonical stream.
    ///
    /// # Errors
    ///
    /// [`SceneError::InvalidNumericValue`] on a nonfinite matrix entry, which is 13.7's
    /// matrix rule applying to a camera exactly as it does to a sprite.
    pub fn encode(&self, cp: &mut anim_core::hash::CanonicalBytes) -> Result<()> {
        cp.fixed_bytes(self.node.as_bytes());
        for (row, values) in self.world_transform.iter().enumerate() {
            for (column, value) in values.iter().enumerate() {
                cp.f64(*value)
                    .map_err(|_| SceneError::InvalidNumericValue {
                        field: "camera_world_transform",
                        subject: self.node.to_string(),
                        detail: format!("entry [{row}][{column}] is {value}, which is not finite"),
                    })?;
            }
        }
        cp.bool(self.is_on);
        Ok(())
    }
}

/// Which camera the latest cut at or before `at` names, if any.
///
/// "Latest" is by [`SceneEvent::order_key`], which is 13.7's canonical order, so the
/// answer at a position on an exact boundary is the answer the frame owning that boundary
/// would give: an event at the upper edge of frame `n` belongs to frame `n + 1`, and a cut
/// exactly at `at` is therefore already in force.
///
/// Cuts at the same position, if a document ever authors two, resolve by 13.7's remaining
/// terms — priority, then source node, then local sequence — rather than by whichever
/// definition id happens to sort lower as a side effect. That is the point of sorting the
/// sequence rather than scanning it unsorted.
#[must_use]
pub fn active_camera_at(cuts: &[SceneEvent], at: &FramePos) -> Option<NodeId> {
    cuts.iter()
        .filter(|event| event.kind == EventKind::CameraCut && &event.time <= at)
        .max_by_key(|event| event.order_key())
        .map(|event| event.source_id)
}

/// The cameras the scene's cut markers name, each with whether it is on.
///
/// `worlds` is the map [`crate::hierarchy::visit`] produced. A node named by a cut but
/// absent from the scene cannot happen — `SceneData::validate` refuses an event whose
/// source node the scene does not have — but if a caller hands this function a matrix map
/// that does not match the cuts it was given, the refusal is here rather than an identity
/// matrix quietly standing in for a camera nobody authored.
///
/// # Errors
///
/// [`SceneError::MissingParent`] is reused for exactly that mismatch, named for the
/// relation that broke: the cut refers to a node the evaluated hierarchy does not contain.
/// It is not a new error category, and inventing one for a caller's argument mismatch would
/// put a variant in 13.11's list that no document can cause.
pub fn evaluate(
    cuts: &[SceneEvent],
    worlds: &std::collections::BTreeMap<NodeId, [[f64; 3]; 3]>,
    at: &FramePos,
) -> Result<Vec<EvaluatedCamera>> {
    let active = active_camera_at(cuts, at);
    let mut nodes: Vec<NodeId> = cuts
        .iter()
        .filter(|event| event.kind == EventKind::CameraCut)
        .map(|event| event.source_id)
        .collect();
    nodes.sort();
    nodes.dedup();
    nodes
        .into_iter()
        .map(|node| {
            let world = worlds
                .get(&node)
                .copied()
                .ok_or_else(|| SceneError::HierarchyCycle {
                    node: node.to_string(),
                })?;
            Ok(EvaluatedCamera {
                node,
                world_transform: world,
                is_on: active == Some(node),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the workspace denial governs production code"
    )]

    use super::*;
    use crate::hierarchy::IDENTITY;
    use anim_core::rational::Rational;
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn node(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    fn occurrence(label: &str, frame: i64, kind: EventKind) -> SceneEvent {
        use anim_project::EventOccurrenceId;
        SceneEvent {
            occurrence_id: EventOccurrenceId::derive(&namespace(), label.as_bytes()),
            time: FramePos::at_frame(frame),
            priority: 0,
            source_id: node(label),
            local_sequence: 0,
            kind,
        }
    }

    fn worlds(
        entries: &[(&str, [[f64; 3]; 3])],
    ) -> std::collections::BTreeMap<NodeId, [[f64; 3]; 3]> {
        entries.iter().map(|(l, m)| (node(l), *m)).collect()
    }

    #[test]
    fn the_latest_cut_at_or_before_a_position_is_the_one_in_force() {
        let a = occurrence("cam-a", 2, EventKind::CameraCut);
        let b = occurrence("cam-b", 7, EventKind::CameraCut);
        let cuts = [a.clone(), b.clone()];
        assert_eq!(active_camera_at(&cuts, &FramePos::at_frame(1)), None);
        assert_eq!(
            active_camera_at(&cuts, &FramePos::at_frame(2)),
            Some(node("cam-a"))
        );
        assert_eq!(
            active_camera_at(&cuts, &FramePos::at_frame(6)),
            Some(node("cam-a")),
            "and it stays on until the next cut, not until the next frame"
        );
        assert_eq!(
            active_camera_at(&cuts, &FramePos::at_frame(7)),
            Some(node("cam-b"))
        );
        assert_eq!(
            active_camera_at(
                &cuts,
                &FramePos::new(9, Rational::new(1, 3).unwrap()).unwrap()
            ),
            Some(node("cam-b"))
        );
    }

    #[test]
    fn a_cut_exactly_at_the_snapshot_position_is_already_in_force() {
        // 13.6's boundary sentence, on the camera side: the cut that defines frame 7 is
        // what frame 7 looks like, so `<=` and not `<` is the rule here, and the test
        // would fail if the two halves of the spec were read against each other.
        let cut = occurrence("cam-a", 7, EventKind::CameraCut);
        assert_eq!(
            active_camera_at(&[cut], &FramePos::at_frame(7)),
            Some(node("cam-a"))
        );
    }

    #[test]
    fn non_cut_events_never_change_the_camera() {
        let audio = occurrence("foley", 5, EventKind::AudioCue);
        let signal = occurrence("beat", 6, EventKind::Signal);
        assert_eq!(
            active_camera_at(&[audio, signal], &FramePos::at_frame(10)),
            None,
            "an audio cue is not a view"
        );
    }

    #[test]
    fn two_cuts_at_one_position_resolve_by_the_canonical_order() {
        let first = occurrence("cam-a", 4, EventKind::CameraCut);
        let mut second = occurrence("cam-b", 4, EventKind::CameraCut);
        second.priority = 1;
        // Whichever way the pair is offered, the higher-priority cut at the same time wins,
        // because `max_by_key` reads 13.7's key rather than the input order.
        let forward = [first.clone(), second.clone()];
        let backward = [second.clone(), first.clone()];
        assert_eq!(
            active_camera_at(&forward, &FramePos::at_frame(4)),
            active_camera_at(&backward, &FramePos::at_frame(4))
        );
        assert_eq!(
            active_camera_at(&forward, &FramePos::at_frame(4)),
            Some(node("cam-b"))
        );
    }

    #[test]
    fn every_named_camera_is_evaluated_and_exactly_one_is_on() {
        let cuts = [
            occurrence("cam-a", 2, EventKind::CameraCut),
            occurrence("cam-b", 7, EventKind::CameraCut),
            occurrence("cam-c", 1, EventKind::CameraCut),
        ];
        let worlds = worlds(&[
            ("cam-a", [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
            ("cam-b", [[1.0, 0.0, 5.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
            ("cam-c", IDENTITY),
        ]);
        let cameras = evaluate(&cuts, &worlds, &FramePos::at_frame(8)).unwrap();
        assert_eq!(
            cameras.len(),
            3,
            "inactive cameras keep their pose for WP16"
        );
        assert_eq!(
            cameras.iter().filter(|c| c.is_on).count(),
            1,
            "one camera is on, not zero and not three"
        );
        let on = cameras.iter().find(|c| c.is_on).unwrap();
        assert_eq!(on.node, node("cam-b"));
        assert_eq!(on.world_transform[0][2], 5.0);
        let ids: Vec<String> = cameras.iter().map(|c| c.node.to_string()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "the camera list is in NodeId order");
    }

    #[test]
    fn a_cut_naming_a_node_the_hierarchy_does_not_have_is_refused() {
        let cuts = [occurrence("ghost", 3, EventKind::CameraCut)];
        let worlds = worlds(&[]);
        let Err(SceneError::HierarchyCycle { node: named }) =
            evaluate(&cuts, &worlds, &FramePos::at_frame(4))
        else {
            panic!("an identity matrix for a camera nobody authored is the silent skip");
        };
        assert_eq!(
            named,
            node("ghost").to_string(),
            "a cut whose node the hierarchy never produced is the one case this cannot mean"
        );
    }

    #[test]
    fn a_scene_with_no_cuts_evaluates_to_no_cameras() {
        let worlds = worlds(&[("hero", IDENTITY)]);
        assert!(evaluate(&[], &worlds, &FramePos::at_frame(0))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_nonfinite_camera_matrix_is_refused_at_encoding() {
        use anim_core::hash::CanonicalBytes;
        let camera = EvaluatedCamera {
            node: node("cam-a"),
            world_transform: [[f64::NAN, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            is_on: true,
        };
        let mut cp = CanonicalBytes::new();
        let Err(SceneError::InvalidNumericValue { field, subject, .. }) = camera.encode(&mut cp)
        else {
            panic!("a NaN in a view matrix must not reach a snapshot digest");
        };
        assert_eq!(field, "camera_world_transform");
        assert_eq!(subject, node("cam-a").to_string());
    }
}
