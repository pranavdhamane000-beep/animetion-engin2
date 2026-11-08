//! Evaluated scene snapshots and canonical events (spec 13, WP04).
//!
//! This crate turns the authoritative document into immutable, revision-bound snapshots
//! and one canonical event sequence per frame. It is the only place either of those
//! exists: spec 13.2's rule that "no downstream system may read mutable project internals
//! directly" is what makes `anim_sim`, `anim_render`, `anim_audio`, `anim_export` and the
//! editor all see one evaluated instant, and spec 13.10 makes the editor's panels read
//! snapshots while editing only through commands.
//!
//! Two rules decide correctness here rather than style, and both are stated in 13.6:
//!
//! - evaluation is **stateless**. The time at a [`FramePos`](anim_project::FramePos) is
//!   computed from that position and never accumulated from the previous frame, so seeking
//!   to frame 4 000 costs the same as seeking to frame 4 and an export cannot drift.
//! - the **float appears last**. The interval fraction `u = (t - t0) / (t1 - t0)` and the
//!   easing applied to it are exact rationals; `f64` enters at the final numeric operation
//!   that writes a [`EvaluatedNode`] field. A float used mid-interpolation is the drift
//!   spec 10.12's "no accumulated drift" clause exists to forbid.
//!
//! Snapshots are built from a document and are never handed back a `&mut`, which is what
//! makes 13.2's "expose no setters or interior mutability" a property of the type rather
//! than a convention. The one cache in this crate, [`cache::SnapshotCache`], owns its map
//! behind `&mut self` rather than behind a lock: `clippy.toml` denies `Mutex` and `RwLock`
//! workspace-wide because the order of concurrent access must never reach exported bytes.

pub mod animation;
pub mod cache;
pub mod camera;
pub mod evaluator;
pub mod events;
pub mod hierarchy;
pub mod properties;
pub mod snapshot;

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
use anim_core::error::CoreError;

pub use animation::{
    blend_layers, evaluate_node, local_matrices, local_transform, LocalTransform, PropertySources,
    PATH_ASSET, PATH_OPACITY, PATH_TRANSFORM_ROTATION, PATH_TRANSFORM_SCALE, PATH_TRANSFORM_X,
    PATH_TRANSFORM_Y, PATH_VISIBLE, PATH_Z,
};
pub use cache::{CacheKey, SnapshotCache};
pub use camera::{active_camera_at, EvaluatedCamera};
pub use evaluator::{containing_frame, frame_bounds, SceneEvaluator};
pub use events::{events_for_frame, occurrence_id, EventSources, SceneEvent};
pub use hierarchy::{multiply, visit, NodeView, WorldEntry, IDENTITY};
pub use properties::{
    eased_fraction, frame_count, interval_fraction, sample_property, value_as_f64, with_subject,
};
pub use snapshot::{snapshot_hash, AudioCue, EvaluatedNode, SceneSnapshot, SimulationInput};
// WP08's clip type and its channel-layout vocabulary are authored in `anim_project` and
// exposed through the snapshot; re-export them here so a consumer of `anim_scene` (and
// `anim_audio` behind it) names one type. `ChannelLayout` rides along because 17.4 forbids
// `anim_audio` depending on `anim_project`, yet the mixer and its fixtures must name the
// layout variants — the snapshot is the only legal route (DECISIONS.md #46 ruling 1).
pub use anim_project::{AudioClip, ChannelLayout};

/// The domain separator for a snapshot's canonical byte stream (ADR-0004 consequence 5's
/// registry, the convention `anim_engine/document/v1` and `anim_engine/manifest/v1`
/// follow).
///
/// Spec 13.5 step 8 calls the snapshot's hash "optional", and this constant is why it is
/// nonetheless the load-bearing part of the package: G3 (13.12) is "identical canonical
/// snapshots and event logs from two fresh processes for the same document revision", and
/// that is only checkable by a digest whose pre-image is one byte stream.
///
/// The pre-image includes
/// [`anim_project::property::EASING_ALGORITHM_VERSION`](anim_project::EASING_ALGORITHM_VERSION)
/// and not the document's schema version. The reasoning is ADR-0005 Decision 4's split
/// applied to a new object: a field addition changes the *data*, which the snapshot
/// already reflects in its bytes, whereas a change to how a curve is *read* changes
/// nothing in the document and everything in the snapshot. Two snapshots that differ must
/// never share a hash, and an algorithm version inside the pre-image is what makes the
/// silent case impossible.
///
/// The suffix moved v1 -> v2 for WP08 (DECISIONS.md #46): the snapshot gained the
/// `audio_clips` field, so its byte stream — and therefore every digest over it — changed.
/// No committed test pins a stored v1 snapshot constant (the WP04 suites recompute the
/// digest cross-process), so the bump breaks no golden; it is here so the changed reading
/// is visible in the identity rather than silent, the same reasoning that put
/// `EASING_ALGORITHM_VERSION` in the pre-image.
pub const SNAPSHOT_DOMAIN: &str = "anim_engine/snapshot/v2";

/// The package's error taxonomy (spec 13.11).
///
/// All nine names spec 13.11 lists are present, and eight of the nine are reached by a
/// test. That half-sentence is the point of this comment rather than the first:
/// `anim_project`'s types make several of these states unconstructable in a *document*,
/// which is why [`hierarchy`] and [`properties`] work over extracted views of the scene
/// instead of over `SceneData` directly — a view is the level at which an authored mistake
/// is still expressible, and so the level at which refusing it is a real branch rather
/// than a dead one. See each module's comment for which of 13.11's categories it owns.
///
/// The one exception is [`SceneError::EventOrderOverflow`]. 13.11 names it for the event
/// ordering path, and in this package's authored data each event definition carries exactly
/// one position, so the only arithmetic in that path is a constant occurrence index of
/// `0` and a domain tag that is a literal. Nothing can overflow it, and no test reaches
/// it. It is declared rather than omitted because 13.11 lists it and because the package
/// that authors repetition — WP14's emitter scheduling, spec 29.5 — gives the index
/// something to exceed. `BUILD_STATE.md` carries it as an open item; a later session must
/// not read the variant's presence as coverage.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SceneError {
    /// The scene the caller named is not in the document.
    #[error("scene {scene} is not present in this document")]
    MissingScene { scene: String },

    /// The parent relation contains a cycle, so no topological visit terminates.
    ///
    /// Spec 13.11 pairs this variant with "never skip invalid nodes silently": the
    /// refusal is the whole response, and a snapshot with its cyclic nodes quietly left
    /// out would be a snapshot stamped with a revision whose contents it does not have.
    #[error("the scene hierarchy contains a cycle at node {node}")]
    HierarchyCycle { node: String },

    /// A node names a parent that is not in the scene.
    #[error("node {node} names parent {parent}, which the scene does not have")]
    MissingParent { node: String, parent: String },

    /// Two visited nodes carry one identity.
    #[error("node {node} appears twice in the visited hierarchy")]
    DuplicateNode { node: String },

    /// A property's key list cannot produce a value at this time: no keys and no authored
    /// default, where a value is required.
    #[error("property {property} on node {node} has no usable key list: {detail}")]
    InvalidPropertyKeys {
        node: String,
        property: String,
        detail: String,
    },

    /// The declared easing cannot be applied.
    ///
    /// Two conditions reach this, and both are about a combination rather than about a
    /// stored value, which is why they belong to the evaluator and not to
    /// `anim_project`'s validating constructors: an easing table that violates its own
    /// endpoint rules if one arrives through a path that did not run them, and a
    /// non-`Step` easing declared on a value that has no interval arithmetic — a bool, a
    /// label, an asset reference. Interpolating between two `AssetId`s is not a thing this
    /// engine does, and 13.11's separate `InvalidNumericValue` is for the numeric case.
    #[error("invalid easing on {property} at frame {position}: {detail}")]
    InvalidEasing {
        property: String,
        position: String,
        detail: String,
    },

    /// An exact time, frame or sample computation exceeded its width.
    #[error("time arithmetic overflowed in {operation} at {value}")]
    TimeOverflow {
        operation: &'static str,
        value: String,
    },

    /// A matrix, opacity or depth value is not finite.
    ///
    /// Spec 13.7 states it for matrices ("Reject nonfinite matrices") and 13.8's other
    /// `f64` fields inherit the rule, because a nonfinite `opacity` reaching a blend is
    /// the same failure one frame later. `CanonicalBytes::f64` refuses to hash one, so a
    /// snapshot that held a nonfinite value could not be hashed at all; catching it here
    /// names the field instead of failing later and more obscurely.
    #[error("nonfinite evaluated value in {field} for {subject}: {detail}")]
    InvalidNumericValue {
        field: &'static str,
        subject: String,
        detail: String,
    },

    /// The canonical event order could not be produced.
    ///
    /// 13.7's ordering is total only because `EventOccurrenceId` breaks every tie, so this
    /// is for the arithmetic that has to succeed for an occurrence to exist at all: an
    /// occurrence index that will not fit, or a canonical pre-image the encoder refuses.
    /// It is not for an ordering disagreement, and the distinction matters in a report —
    /// "cannot order" sends a reader to the comparator, "cannot name" sends them here.
    #[error("event ordering failed for {event}: {detail}")]
    EventOrderOverflow { event: String, detail: String },

    /// A checked-arithmetic or canonical-encoding failure from `anim_core`.
    ///
    /// An addition, recorded the way DECISIONS.md #29 recorded `AssetError`'s four: 13.11's
    /// list has no row for it, and folding it into `TimeOverflow` would erase the width
    /// information from an overflow report — the one value spec 7.4 requires an arithmetic
    /// diagnostic to carry.
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl SceneError {
    /// The stable category for this error (spec 7.4). Append-only and dot-separated,
    /// matching `anim_core::error`, `anim_assets` and `anim_project`.
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        let raw = match self {
            Self::MissingScene { .. } => "scene.missing_scene",
            Self::HierarchyCycle { .. } => "scene.hierarchy_cycle",
            Self::MissingParent { .. } => "scene.missing_parent",
            Self::DuplicateNode { .. } => "scene.duplicate_node",
            Self::InvalidPropertyKeys { .. } => "scene.invalid_property_keys",
            Self::InvalidEasing { .. } => "scene.invalid_easing",
            Self::TimeOverflow { .. } => "scene.time_overflow",
            Self::InvalidNumericValue { .. } => "scene.invalid_numeric_value",
            Self::EventOrderOverflow { .. } => "scene.event_order_overflow",
            Self::Core(_) => "scene.core",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    #[must_use]
    pub fn severity(&self) -> DiagnosticSeverity {
        // Every variant is an error. 13.11's "never skip invalid nodes silently, and never
        // emit a snapshot stamped with another revision" leaves no room for a warning at
        // this layer: an evaluation that could not be completed has no result to attach a
        // warning to.
        DiagnosticSeverity::Error
    }

    /// The structured form a panel or a report renders (spec 7.4's safe context: ids,
    /// properties, frames — never file contents).
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        let base = Diagnostic::new(self.code(), self.severity(), self.to_string());
        match self {
            Self::MissingScene { scene } => base.with_context("scene", scene.clone()),
            Self::HierarchyCycle { node } | Self::DuplicateNode { node } => {
                base.with_context("node", node.clone())
            }
            Self::MissingParent { node, parent } => base
                .with_context("node", node.clone())
                .with_context("parent", parent.clone()),
            Self::InvalidPropertyKeys {
                node,
                property,
                detail,
            } => base
                .with_context("node", node.clone())
                .with_context("property", property.clone())
                .with_context("detail", detail.clone()),
            Self::InvalidEasing {
                property, position, ..
            } => base
                .with_context("property", property.clone())
                .with_context("frame", position.clone()),
            Self::TimeOverflow { operation, value } => base
                .with_context("operation", (*operation).to_string())
                .with_context("value", value.clone()),
            Self::InvalidNumericValue { field, subject, .. } => base
                .with_context("field", (*field).to_string())
                .with_context("subject", subject.clone()),
            Self::EventOrderOverflow { event, .. } => base.with_context("event", event.clone()),
            Self::Core(error) => error.to_diagnostic(),
        }
    }
}

/// The result type every fallible function in this crate returns.
pub type Result<T, E = SceneError> = std::result::Result<T, E>;

/// How a frame position is named in a diagnostic.
///
/// `FramePos` has no `Display`, and giving it one would be a mistake: `Rational`'s
/// `Display` is the *canonical* persisted spelling that `parse_canonical` reads back
/// (ADR-0004), and a second type with a `Display` invites the same round trip whether or
/// not the form supports it. This is a report string and nothing else — never parsed,
/// never hashed, never written to a file — so it spells the position the way a person
/// reading a bug report reads it: a whole frame as a number, a subframe position as the
/// frame plus the fraction.
#[must_use]
pub fn position_label(position: &anim_core::time::FramePos) -> String {
    if position.is_whole_frame() {
        position.frame.to_string()
    } else {
        format!("{}+{}", position.frame, position.subframe())
    }
}
