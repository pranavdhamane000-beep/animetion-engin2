//! Authored event definitions (spec 13.5 step 1, 13.7, 13.8).
//!
//! Spec 12.3 lists no file like this one: WP03's raw scene carries nodes, tracks and
//! markers only, and spec 13.3 assigns events to `anim_scene/src/events.rs`. That split
//! cannot hold, because 13.5's first step is "validate scene hierarchy, property keys,
//! tracks **and event definitions**" and 13.7 orders occurrences by fields — `priority`,
//! `source_node_id`, `local_sequence` — that only exist as *authored* data. An evaluator
//! cannot derive what nobody wrote. So the authored half lives here, in the crate that
//! owns authored state, and `anim_scene` owns the occurrence half: the derivation of
//! `EventOccurrenceId`, the sort, and `SceneEvent` itself. Recorded as a deviation.
//!
//! A [`crate::scene::Marker`] stays what WP03 made it — a named point on the timeline
//! with no identity and no consequences — rather than being promoted into an event. The
//! two differ in exactly one respect that later packages depend on: a marker is read by
//! a person, an event is read by `anim_sim`, `anim_audio` and `anim_render` through the
//! canonical event sequence (spec 13.10). Collapsing them would make every annotation on
//! a timing sheet a side effect.

use anim_core::hash::CanonicalBytes;
use anim_core::id::{EventDefinitionId, NodeId};
use anim_core::time::FramePos;
use serde::{Deserialize, Serialize};

use crate::ProjectError;

/// What an authored event means downstream.
///
/// Three variants, one per thing spec 13 itself asks an event to do: 13.5 step 5's
/// "camera and audio cues" and the generic cue that 13.7's ordering exists to deliver.
///
/// This is deliberately not the whole list. WP13 adds `Clash { contact }` and carries
/// `effect_cue` and `audio_cue` as `Option<EventDefinitionId>` (spec 28.8), and WP14's
/// emitters schedule through authored cues too (spec 29.5) — each of those packages adds
/// its variant with its own payload, which is why the variants here carry no data of
/// their own beyond what [`EventDefinition`] already holds. An enum that started out
/// with a `Clash` nobody can produce yet would be authoring Part B from S1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A cue with no engine-level consequence beyond its own occurrence: a signal for
    /// a consumer to react to. The only kind that needs no further authored data.
    Signal,
    /// A camera cut at this position, on this definition's source node. Spec 13.3's
    /// `camera.rs` responsibility is "camera evaluation and cut markers" and 13.12
    /// tests camera cuts, so the state has to be authorable somewhere.
    CameraCut,
    /// An audio cue. Spec 13.10 has `anim_audio` convert cues to exact sample
    /// positions, and 13.12 tests "audio cue identity"; the asset a cue points at is
    /// the source node's evaluated `asset` property, not a second reference here, so
    /// that one authored asset never has two bindings that can disagree.
    AudioCue,
}

impl EventKind {
    /// The canonical tag: the lowercase name JSON writes, with no table between the
    /// two (ADR-0005 Decision 5's rule, the one `Easing::as_str` and
    /// `PropertyValue::kind_name` also follow).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Signal => "signal",
            Self::CameraCut => "camera_cut",
            Self::AudioCue => "audio_cue",
        }
    }

    pub(crate) fn encode(self, cp: &mut CanonicalBytes) {
        cp.text(self.as_str());
    }
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One authored event: what fires, when, from which node, and in what order relative to
/// the others at the same instant.
///
/// The four ordering fields are 13.7's sort key, stated here rather than discovered at
/// evaluation time: `(event_time, priority, source_node_id, local_sequence,
/// occurrence_id)`. `event_time` is [`EventDefinition::position`], `source_node_id` is
/// [`EventDefinition::source`], and the last term is derived, so this type supplies
/// three of the five and the derivation supplies the rest.
///
/// `source` is required rather than `Option<NodeId>`, because spec 13.8 types
/// `SceneEvent.source_id` as a plain `NodeId`. An event with no node would have no
/// value for a field the snapshot says is not optional, and 13.7's third sort term would
/// be a gap every consumer would have to invent a rule for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDefinition {
    /// Identity, generated once and copied through save, load and undo (spec 10.8). The
    /// value `EventOccurrenceId` is derived from, so an event renamed to a new id is a
    /// new event to every consumer that remembers the old one.
    pub id: EventDefinitionId,
    /// When it fires. Whole-frame and subframe positions are both legal: 13.6 assigns an
    /// event at exactly the upper boundary of frame `n` to frame `n + 1`, which only
    /// means something if the position is an exact rational.
    pub position: FramePos,
    /// Later than `position` in 13.7's key. Signed, and `i32` because spec 13.8 declares
    /// it so; a negative priority is a legal authored choice meaning "first".
    pub priority: i32,
    /// The node this event belongs to, which is 13.7's `source_node_id` and must name a
    /// node the scene has — `SceneData::validate` refuses one that does not, rather than
    /// emitting an occurrence whose source no consumer can resolve.
    pub source: NodeId,
    /// What it means.
    pub kind: EventKind,
    /// 13.7's fourth sort term, and the one it insists on: "persisted or derived from a
    /// stable source key, never hash-map or filesystem enumeration". Persisted here,
    /// because an authored order is a statement about intent that no derivation from an
    /// id can recover — two cues on one node at one frame are exactly the case where
    /// which came first is the thing the operator decided.
    pub local_sequence: u32,
}

impl EventDefinition {
    /// A new event definition.
    #[must_use]
    pub fn new(
        id: EventDefinitionId,
        position: FramePos,
        priority: i32,
        source: NodeId,
        kind: EventKind,
        local_sequence: u32,
    ) -> Self {
        Self {
            id,
            position,
            priority,
            source,
            kind,
            local_sequence,
        }
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.fixed_bytes(self.id.as_bytes());
        cp.i64(self.position.frame);
        cp.rational(&self.position.subframe());
        cp.i32(self.priority);
        cp.fixed_bytes(self.source.as_bytes());
        self.kind.encode(cp);
        cp.u32(self.local_sequence);
    }

    /// The definition's canonical field stream, with no domain tag, so a command
    /// payload holding one hashes these bytes rather than a second rendering of them
    /// (ADR-0005 Decision 8's rule, as extended to every project object by
    /// `NodeData::canonical_bytes`).
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp);
        cp.into_vec()
    }

    pub(crate) fn missing_source(source: &NodeId) -> ProjectError {
        ProjectError::Validation {
            location: format!("event source {source}"),
            detail: "names a node the scene does not have; an occurrence whose source cannot \
                     be evaluated would enter the event sequence with nothing to act on"
                .to_string(),
        }
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
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn event_id(label: &str) -> EventDefinitionId {
        EventDefinitionId::derive(&namespace(), label.as_bytes())
    }

    /// One source node for the whole file. `13_7`'s sort term is a `NodeId`, and every
    /// fixture here puts its events on this one node so the byte-level oracle below has
    /// a value it can name.
    fn node_id() -> NodeId {
        NodeId::derive(&namespace(), b"camera")
    }

    fn definition(label: &str, frame: i64, priority: i32) -> EventDefinition {
        EventDefinition::new(
            event_id(label),
            FramePos::at_frame(frame),
            priority,
            node_id(),
            EventKind::CameraCut,
            0,
        )
    }

    #[test]
    fn kind_spellings_agree_between_json_and_bytes() {
        for (kind, spelling) in [
            (EventKind::Signal, "signal"),
            (EventKind::CameraCut, "camera_cut"),
            (EventKind::AudioCue, "audio_cue"),
        ] {
            assert_eq!(kind.as_str(), spelling);
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{spelling}\"")
            );
            let mut cp = CanonicalBytes::new();
            kind.encode(&mut cp);
            let mut expected = Vec::new();
            expected.extend_from_slice(&(spelling.len() as u64).to_le_bytes());
            expected.extend_from_slice(spelling.as_bytes());
            assert_eq!(
                cp.as_slice(),
                expected,
                "the tag is a little-endian u64 length prefix and then the same text \
                 JSON writes"
            );
        }
    }

    #[test]
    fn a_definition_carries_every_field_of_13_7_s_key_except_the_derived_one() {
        let event = definition("cut", 12, -3);
        let source = node_id();
        let bytes = event.canonical_bytes();
        // Rebuilt by hand from ADR-0004's layout rather than read off the encoder: an
        // oracle that calls the code under test proves only self-consistency (DECISIONS
        // #32's rule, and the one that found WP02's five and WP03's four defects).
        // Fixed-width ids raw, every variable-width field behind a little-endian u64.
        fn prefixed(value: &str) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
            out
        }
        let mut expected = Vec::new();
        expected.extend_from_slice(event.id.as_bytes());
        expected.extend_from_slice(&12i64.to_le_bytes());
        expected.extend_from_slice(&prefixed("0/1"));
        expected.extend_from_slice(&(-3i32).to_le_bytes());
        expected.extend_from_slice(source.as_bytes());
        expected.extend_from_slice(&prefixed("camera_cut"));
        expected.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(bytes, expected);
    }

    #[test]
    fn a_definition_round_trips_through_json_unchanged() {
        let event = definition("cut", 12, -3);
        let bytes = serde_json::to_vec(&event).unwrap();
        let loaded: EventDefinition = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded, event);
        assert_eq!(serde_json::to_vec(&loaded).unwrap(), bytes);
    }
}
