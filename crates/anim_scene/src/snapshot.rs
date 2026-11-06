//! The immutable snapshot and the evaluated node types (spec 13.8).
//!
//! Spec 13.8 gives the field lists for [`SceneSnapshot`], [`EvaluatedNode`] and
//! [`SceneEvent`] (the last is re-exported from [`crate::events`] because 13.3 assigns
//! events to that file) and names `EvaluatedCamera`, `AudioCue` and `SimulationInput`
//! without giving theirs. Three consequences follow, and each is a rule rather than a
//! preference:
//!
//! - Nothing here is `pub fn`-mutable. Every field is public and there is no setter, no
//!   `&mut` accessor and no interior `Cell`: 13.8's "snapshots expose no setters or
//!   interior mutability" is a property of the type, so a consumer that tried to write
//!   through a snapshot does not compile rather than being discouraged.
//! - The collections are `Arc<[T]>` exactly as 13.8 spells them, because a snapshot is
//!   shared by simulation, render, audio and export at once (13.10) and a `Vec` would
//!   either copy per consumer or hand one of them ownership.
//! - The three unspecified types carry only what an evaluation at 13.5's steps can
//!   produce. `WP05`, `WP06` and `WP15` extend them, and 14.8, 15.8 and 30.8 are where
//!   their own field lists belong; inventing those fields here would be authoring three
//!   later packages from inside this one.
//!
//! Canonical bytes exist for one reason: spec 13.5 step 8's "optional canonical hash" and
//! G3's "identical canonical snapshots from two fresh processes". The layout is
//! ADR-0004's — little-endian fixed widths, a `u64` length prefix on every
//! variable-length field, the domain tag first — and `f64` fields go through
//! `CanonicalBytes::f64`, which refuses a nonfinite value rather than hashing a bit
//! pattern that no other platform reproduces.

use std::sync::Arc;

use anim_core::hash::CanonicalBytes;
use anim_core::id::{AssetId, NodeId, SceneId};
use anim_core::rational::Rational;
use anim_core::time::FramePos;
use anim_project::{ContentHash, EventOccurrenceId, Revision, EASING_ALGORITHM_VERSION};

use crate::camera::EvaluatedCamera;
use crate::events::SceneEvent;
use crate::SNAPSHOT_DOMAIN;
use crate::{Result, SceneError};

/// One evaluated instant of one scene, bound to one revision (spec 13.8).
#[derive(Clone, Debug, PartialEq)]
pub struct SceneSnapshot {
    /// Which scene this is. Part of the cache key.
    pub scene_id: SceneId,
    /// Which revision of the document it was evaluated from. Part of the cache key, and
    /// the reason 13.8's "existing snapshots remain valid for their original revision
    /// after undo or new edits but cannot be presented as current" needs no invalidation
    /// protocol: a stale snapshot is still correct about the revision it names.
    pub revision: Revision,
    /// Where on the timeline. Part of the cache key, and `FramePos`'s private normalized
    /// subframe is what makes that key unique rather than doubled (DECISIONS.md #22).
    pub position: FramePos,
    /// Exact time at that position, in seconds: `frame_rate.time_of(position)`. Held
    /// because 13.6 forbids every consumer from re-deriving it and 13.10 has audio
    /// convert cues to sample positions from it.
    pub time: Rational,
    /// Every node, in the visit order spec 13.7 defines: roots in `NodeId` order, children
    /// in persisted sibling order. Not sorted by anything else — 13.7 states separately
    /// that "draw extraction keys" are produced apart from hierarchy order, and that
    /// separation is the reason this is one order and not two.
    pub nodes: Arc<[EvaluatedNode]>,
    /// The cameras this instant, with which one is on.
    pub cameras: Arc<[EvaluatedCamera]>,
    /// Audio cues authored to fire in the frame this position is in.
    pub audio_cues: Arc<[AudioCue]>,
    /// What `anim_sim` reads so that it never touches the document (13.10).
    pub simulation_inputs: Arc<[SimulationInput]>,
    /// The canonical event sequence for the frame this position is in, in 13.7's order.
    pub events: Arc<[SceneEvent]>,
}

impl SceneSnapshot {
    /// The snapshot's canonical field stream, with no domain tag, for the same reason
    /// every `canonical_bytes` in this workspace has one: a caller that wants to hash it
    /// must add the tag itself, so the tag cannot be forgotten (ADR-0005 Decision 8).
    ///
    /// Unlike the other `canonical_bytes` methods here, this one is fallible, and for one
    /// reason: 13.8 gives the snapshot `f64` fields, and
    /// [`CanonicalBytes::f64`] refuses a nonfinite value rather than hashing a bit pattern
    /// no other platform reproduces. A `Vec<u8>` return would have to either ignore that
    /// refusal — producing a digest over a truncated stream, which is the one outcome
    /// worse than an error — or panic, and 13.11 forbids the silence.
    ///
    /// # Errors
    ///
    /// As [`SceneSnapshot::encode`].
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp)?;
        Ok(cp.into_vec())
    }

    /// Encode into a caller's builder.
    ///
    /// The order is this struct's declaration order, which is spec 13.8's listing order,
    /// which is the rule ADR-0005 Decision 9 applies to the document. `EASING_ALGORITHM_
    /// VERSION` is written first and is not a field of the snapshot: see
    /// [`crate::SNAPSHOT_DOMAIN`]'s comment for why the algorithm belongs in the identity
    /// of an *evaluation* and not in the identity of authored data.
    ///
    /// # Errors
    ///
    /// [`SceneError::InvalidNumericValue`] if an evaluated `f64` is not finite, which is
    /// 13.7's matrix rule arriving at the place where a nonfinite value would otherwise
    /// become a hash of a bit pattern.
    pub fn encode(&self, cp: &mut CanonicalBytes) -> Result<()> {
        cp.u32(EASING_ALGORITHM_VERSION);
        cp.fixed_bytes(self.scene_id.as_bytes());
        cp.u64(self.revision.0);
        cp.i64(self.position.frame);
        cp.rational(&self.position.subframe());
        cp.rational(&self.time);
        cp.u64(
            u64::try_from(self.nodes.len())
                .map_err(|_| count_overflow("node count", &self.scene_id.to_string()))?,
        );
        for node in self.nodes.iter() {
            node.encode(cp)?;
        }
        cp.u64(
            u64::try_from(self.cameras.len())
                .map_err(|_| count_overflow("camera count", &self.scene_id.to_string()))?,
        );
        for camera in self.cameras.iter() {
            camera.encode(cp)?;
        }
        cp.u64(
            u64::try_from(self.audio_cues.len())
                .map_err(|_| count_overflow("audio cue count", &self.scene_id.to_string()))?,
        );
        for cue in self.audio_cues.iter() {
            cue.encode(cp)?;
        }
        cp.u64(
            u64::try_from(self.simulation_inputs.len()).map_err(|_| {
                count_overflow("simulation input count", &self.scene_id.to_string())
            })?,
        );
        for input in self.simulation_inputs.iter() {
            input.encode(cp)?;
        }
        cp.u64(
            u64::try_from(self.events.len())
                .map_err(|_| count_overflow("event count", &self.scene_id.to_string()))?,
        );
        for event in self.events.iter() {
            event.encode(cp);
        }
        Ok(())
    }
}

/// One node's evaluated state at one instant (spec 13.8).
#[derive(Clone, Debug, PartialEq)]
pub struct EvaluatedNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    /// `parent_world * local`, row-major, as 13.7 states it. An unparented node carries
    /// its local matrix.
    pub world_transform: [[f64; 3]; 3],
    pub z: f64,
    pub opacity: f64,
    pub visible: bool,
    /// The cel or sprite this node currently shows. From the node's `asset` property,
    /// whose resolution rule is [`crate::animation`]'s.
    pub asset: Option<AssetId>,
}

impl EvaluatedNode {
    /// # Errors
    ///
    /// [`SceneError::InvalidNumericValue`] on a nonfinite `f64`.
    pub fn encode(&self, cp: &mut CanonicalBytes) -> Result<()> {
        cp.fixed_bytes(self.id.as_bytes());
        match self.parent {
            None => cp.bool(false),
            Some(parent) => {
                cp.bool(true);
                cp.fixed_bytes(parent.as_bytes());
            }
        }
        for row in &self.world_transform {
            for value in row {
                finite("world_transform", &self.id.to_string(), *value, cp)?;
            }
        }
        finite("z", &self.id.to_string(), self.z, cp)?;
        finite("opacity", &self.id.to_string(), self.opacity, cp)?;
        cp.bool(self.visible);
        match self.asset {
            None => cp.bool(false),
            Some(asset) => {
                cp.bool(true);
                cp.fixed_bytes(asset.as_bytes());
            }
        }
        Ok(())
    }
}

/// An audio cue as evaluated for one frame (spec 13.5 step 5, 13.10).
///
/// 13.8 names the type without fields. What `anim_audio` needs to turn a cue into an exact
/// sample position (13.10) is the cue's identity, its node, its authored position and the
/// asset to play — nothing else is derivable at this layer, and the sample rate is a
/// project setting WP08 reads from the document rather than a fact about the cue.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioCue {
    /// The cue's identity, which is 13.12's "audio cue identity" and is the same value
    /// that appears in the event sequence. One event, one identity, two views.
    pub occurrence_id: EventOccurrenceId,
    pub node: NodeId,
    /// Exact authored position. `FramePos` and not seconds, because 10.12 forbids a
    /// rounded value in deterministic state and 13.10's conversion to samples is the
    /// consumer's arithmetic.
    pub position: FramePos,
    /// The sound to play: this cue's source node's evaluated `asset` property, or `None`
    /// when the node has no asset bound. A cue with no asset is a legal authored state
    /// and is reported by the audio package rather than refused here.
    pub asset: Option<AssetId>,
}

impl AudioCue {
    /// # Errors
    ///
    /// [`SceneError::InvalidNumericValue`] if the position's subframe cannot be encoded.
    pub fn encode(&self, cp: &mut CanonicalBytes) -> Result<()> {
        cp.fixed_bytes(self.occurrence_id.as_bytes());
        cp.fixed_bytes(self.node.as_bytes());
        cp.i64(self.position.frame);
        cp.rational(&self.position.subframe());
        match self.asset {
            None => cp.bool(false),
            Some(asset) => {
                cp.bool(true);
                cp.fixed_bytes(asset.as_bytes());
            }
        }
        Ok(())
    }
}

/// What `anim_sim` may read about one node for one tick (spec 13.8, 13.10, 14.8).
///
/// WP05's own state types are keyed by `NodeId` and carry positions and velocities it
/// computes itself (spec 14.8's `Particle` and `SpringBody`), so what a snapshot has to
/// supply is the authored world state a spring chain or an emitter attaches to, and the
/// visibility that decides whether it runs at all. `next_generation`'s counter, the
/// solver's iteration count and the event cursor are simulation's own, and none of them
/// appear here because 13.2 forbids this layer from knowing about them.
#[derive(Clone, Debug, PartialEq)]
pub struct SimulationInput {
    pub node: NodeId,
    /// The node's world matrix at this instant, the same value `EvaluatedNode` carries.
    /// Repeated on purpose: 14.8's bodies index by `NodeId` and a simulation that had to
    /// search the node array for each one would be re-deriving an index this type exists
    /// to hand it.
    pub world_transform: [[f64; 3]; 3],
    pub z: f64,
    pub visible: bool,
}

impl SimulationInput {
    /// # Errors
    ///
    /// [`SceneError::InvalidNumericValue`] on a nonfinite `f64`.
    pub fn encode(&self, cp: &mut CanonicalBytes) -> Result<()> {
        cp.fixed_bytes(self.node.as_bytes());
        for row in &self.world_transform {
            for value in row {
                finite(
                    "simulation world_transform",
                    &self.node.to_string(),
                    *value,
                    cp,
                )?;
            }
        }
        finite("simulation z", &self.node.to_string(), self.z, cp)?;
        cp.bool(self.visible);
        Ok(())
    }
}

fn finite(field: &'static str, subject: &str, value: f64, cp: &mut CanonicalBytes) -> Result<()> {
    cp.f64(value).map_err(|_| SceneError::InvalidNumericValue {
        field,
        subject: subject.to_string(),
        detail: format!("{value} is not finite, and ADR-0004 refuses to hash one"),
    })
}

fn count_overflow(field: &'static str, subject: &str) -> SceneError {
    SceneError::InvalidNumericValue {
        field,
        subject: subject.to_string(),
        detail: "a collection longer than this encoding's width cannot be counted".to_string(),
    }
}

/// The snapshot's canonical digest (spec 13.9's `snapshot_hash`).
///
/// Spec 13.5 step 8 calls this "optional", and it is: nothing in the snapshot's own
/// identity depends on it. It exists for G3, whose criterion is that two fresh processes
/// produce identical snapshots for one revision, and a 256-bit digest is the only way to
/// state that comparison in one line of a report.
///
/// The pre-image is the domain tag followed by
/// [`SceneSnapshot::canonical_bytes`], so this function is the only place
/// [`SNAPSHOT_DOMAIN`] is applied — the ADR-0004 consequence 5 rule that a recorded digest
/// must be reproducible from a named domain and nothing else.
pub fn snapshot_hash(snapshot: &SceneSnapshot) -> Result<ContentHash> {
    let mut cp = CanonicalBytes::with_domain(SNAPSHOT_DOMAIN)?;
    snapshot.encode(&mut cp)?;
    Ok(ContentHash::from_bytes(cp.digest()))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the workspace denial governs production code"
    )]

    use super::*;
    use anim_project::EventKind;
    use uuid::Uuid;

    fn node_id(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn snapshot() -> SceneSnapshot {
        SceneSnapshot {
            scene_id: SceneId::derive(&namespace(), b"shot01"),
            revision: Revision(7),
            position: FramePos::at_frame(3),
            time: Rational::new(3, 1).unwrap(),
            nodes: Arc::new([EvaluatedNode {
                id: node_id("root"),
                parent: None,
                world_transform: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                z: 0.0,
                opacity: 1.0,
                visible: true,
                asset: None,
            }]),
            cameras: Arc::new([]),
            audio_cues: Arc::new([]),
            simulation_inputs: Arc::new([]),
            events: Arc::new([]),
        }
    }

    #[test]
    fn a_snapshot_hash_is_reproducible_from_the_domain_and_the_bytes_alone() {
        // The G3 claim, stated as a property a second process can repeat: recomputing the
        // digest from the snapshot's own canonical bytes and the published domain tag must
        // equal the function's answer. If `snapshot_hash` folded in anything else — a
        // thread id, a hashmap order, the wall clock — this fails.
        let snapshot = snapshot();
        let hash = snapshot_hash(&snapshot).unwrap();
        let mut cp = CanonicalBytes::with_domain(SNAPSHOT_DOMAIN).unwrap();
        snapshot.encode(&mut cp).unwrap();
        assert_eq!(hash, ContentHash::from_bytes(cp.digest()));
        assert_eq!(
            hash.as_bytes(),
            &snapshot_hash(&snapshot).unwrap().as_bytes()[..]
        );
    }

    #[test]
    fn the_algorithm_version_is_in_the_pre_image_so_a_changed_reading_cannot_hide() {
        // Two snapshots whose *authored* data is identical but whose numeric interpretation
        // differs must not hash alike. This test cannot build the second snapshot, so it
        // asserts the cheaper half: that the constant is the first thing in the stream.
        let snapshot = snapshot();
        let mut cp = CanonicalBytes::new();
        snapshot.encode(&mut cp).unwrap();
        assert_eq!(
            &cp.as_slice()[..4],
            &EASING_ALGORITHM_VERSION.to_le_bytes(),
            "the version leads the stream, so bumping it moves every snapshot digest \
             and nothing has to remember to re-check it"
        );
    }

    #[test]
    fn a_nonfinite_evaluated_value_is_refused_rather_than_hashed() {
        let mut snapshot = snapshot();
        let mut node = snapshot.nodes[0].clone();
        node.world_transform[0][0] = f64::NAN;
        snapshot.nodes = Arc::new([node]);
        let Err(SceneError::InvalidNumericValue { field, detail, .. }) = snapshot_hash(&snapshot)
        else {
            panic!("a NaN matrix entry must not become a snapshot digest");
        };
        assert_eq!(field, "world_transform");
        assert!(detail.contains("not finite"), "{detail}");
    }

    #[test]
    fn two_snapshots_differing_only_in_event_order_have_different_bytes() {
        // The ordering is fixed before it reaches here, so the encoder's job is to write
        // it faithfully: swapping two events must change the bytes, or a snapshot hash
        // would not detect the exact defect 13.7's total key exists to prevent.
        let mut left = snapshot();
        let mut right = snapshot();
        let a = scene_event("a", 0);
        let b = scene_event("b", 1);
        left.events = Arc::new([a.clone(), b.clone()]);
        right.events = Arc::new([b, a]);
        assert_ne!(
            left.canonical_bytes().unwrap(),
            right.canonical_bytes().unwrap()
        );
        assert_ne!(
            snapshot_hash(&left).unwrap(),
            snapshot_hash(&right).unwrap()
        );
    }

    fn scene_event(label: &str, sequence: u32) -> SceneEvent {
        SceneEvent {
            occurrence_id: EventOccurrenceId::derive(&namespace(), label.as_bytes()),
            time: FramePos::at_frame(4),
            priority: 0,
            source_id: node_id("root"),
            local_sequence: sequence,
            kind: EventKind::Signal,
        }
    }
}
