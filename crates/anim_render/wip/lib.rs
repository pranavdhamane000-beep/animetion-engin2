//! Device, graph, passes, GPU resources, memory policy and readback (spec 4.2).
//!
//! Owns the production GPU and DXGI code. Nothing in this crate imports a WP00
//! probe binary (spec 9.10); the probes prove capabilities this crate later uses.
//!
//! The crate is built as one forward pass sequence — opaque and alpha-tested, ordered
//! 2D, sorted transparent, the linear HDR post chain, one tone map, output transfer —
//! over six resize-owned full-resolution targets (spec 15.6). Two rules shape everything
//! below:
//!
//!  * **Nothing falls back.** Spec 15.11 forbids a silent backend, transfer-function or
//!    quality substitute, so every capability the graph needs is validated against the
//!    adapter before the device is opened, and a missing one is an error rather than a
//!    degraded run. Spec 9.11 says the same thing about adapters.
//!  * **Order comes from data, not from hardware.** Spec 15.7 prohibits hash-map and GPU
//!    timing order in the draw list, so the transparent sort is a stable total
//!    comparator over authored fields, and the pass sequence is recorded in a log the
//!    gate can read (15.12).
//!
//! # Error taxonomy
//!
//! [`RenderError`] lives here rather than in an `error.rs` because spec 15.3 lists no
//! such file for this crate — the same omission DECISIONS.md #29 recorded for
//! `anim_assets`, whose error type also sits in `lib.rs`.

use anim_core::id::NodeId;

pub mod alpha;
pub mod camera;
pub mod device;
pub mod draw_list;
pub mod graph;
pub mod passes;
pub mod readback;
pub mod resources;
pub mod targets;

/// One rendered frame's worth of outputs, with the generation they were drawn into.
///
/// Spec 15.9 gives this type as `render_scene`'s result and makes it the owner of "the
/// final clean output and generation metadata". It deliberately does *not* carry a
/// surface view: `composite_editor_ui` takes a `&TextureView` the caller owns (15.9), so
/// the editor's presentation target never becomes something the renderer hands out.
pub struct RenderedScene {
    pub output: OutputDestination,
    pub generation: u64,
    pub extent: wgpu::Extent3d,
    /// What the graph actually ran, in order. Spec 15.12 asks for "command-log
    /// pass-order verification", and this is the artifact that claim is checked against.
    pub pass_log: Vec<graph::PassRecord>,
}

/// Where the clean output goes. Spec 15.9: "Export requests a readback-compatible
/// `OutputDestination` and cannot call `composite_editor_ui`."
pub enum OutputDestination {
    /// Readback into CPU bytes: the only form export can consume (15.9), and the form
    /// that makes a golden comparable at all.
    Readback(readback::OutputReadback),
    /// A texture the caller composits from. Editor-only: presenting this is what makes
    /// `composite_editor_ui` meaningful, and nothing hands one of these to a writer.
    View(wgpu::TextureView),
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// Selection found no discrete NVIDIA adapter after the bounded retries. Spec 9.7's
    /// `IdentityUnavailable`, and the reason it is a distinct variant rather than an
    /// `AdapterNotFound`-shaped guess: on this hybrid laptop the same message means
    /// either "no dGPU" or "the dGPU is power-gated" (BUILD_STATE hazard 2), and the
    /// difference decides whether a retry is the remedy.
    #[error("adapter identity unavailable: {detail}")]
    IdentityUnavailable { detail: String },

    /// The adapter that was selected is not the one G0 certified. Spec 9.12 will not
    /// carry a pass across a different adapter or driver without revalidation, and
    /// DECISIONS.md #18 keys that revalidation on PCI bus id, adapter name and driver.
    #[error("adapter does not match the certified G0 evidence: {field} expected {expected:?}, found {found:?}")]
    AdapterRevalidation {
        field: &'static str,
        expected: String,
        found: String,
    },

    /// `AdapterInfo::device_pci_bus_id` was empty. DECISIONS.md #18's residual says what
    /// not to do with this: "Require non-empty and hard-fail; do not treat a blank as
    /// 'unknown, proceed'." A blank anchor is not a missing check, it is a check that
    /// passed on nothing.
    #[error("no PCI bus id for adapter {adapter:?}; refusing to treat an absent bus id as a match")]
    PciBusIdUnavailable { adapter: String },

    /// A format or usage the graph is built around is not available. Spec 15.11's first
    /// sentence, and the reason it is an initialization error rather than a runtime one:
    /// discovering this mid-export would burn the whole run.
    #[error("required capability missing: {format} does not allow {usage} ({purpose})")]
    RequiredCapabilityMissing {
        format: String,
        usage: &'static str,
        purpose: String,
    },

    /// Extent zero in either axis. Spec 15.11 pairs the refusal with a promise — "without
    /// destroying the last valid targets" — so this variant is returned *by* `resize`
    /// before anything is reallocated, and the previous generation stays current.
    #[error("extent {width}x{height} is degenerate; the last valid targets are unchanged")]
    ZeroExtent { width: u32, height: u32 },

    /// A matrix or position that is not finite. Spec 15.7 validates finiteness during
    /// extraction, and 15.11 lists NaN transforms first among the extraction failures.
    #[error("non-finite transform on node {entity} draw_index {draw_index}: {detail}")]
    NonFiniteTransform {
        entity: NodeId,
        draw_index: u32,
        detail: String,
    },

    /// View depth that is not finite, so it cannot be quantized to the `i32` the sort
    /// key uses (15.7) and the item's position in the order would be arbitrary.
    #[error("non-finite view depth on node {entity} draw_index {draw_index}")]
    NonFiniteViewDepth { entity: NodeId, draw_index: u32 },

    /// Two items with the same `(entity_id, draw_index)`. Spec 15.7: "Duplicate keys fail
    /// extraction" — the key is the sort's last-resort tie-break, so a repeat makes the
    /// order undetermined by the data and the golden irreproducible.
    #[error("duplicate draw key (entity {entity}, draw_index {draw_index})")]
    DuplicateDrawKey { entity: NodeId, draw_index: u32 },

    /// An alpha-test cutoff outside the range the comparison means.
    #[error("invalid alpha-test cutoff {cutoff} on node {entity}: expected a finite value in [0, 1]")]
    InvalidAlphaCutoff { entity: NodeId, cutoff: f32 },

    /// An alpha mode no blend descriptor is written for. Refused rather than blended
    /// approximately, because an approximate blend is invisible in one frame and obvious
    /// in the finished export.
    #[error("unsupported alpha mode {0}")]
    UnsupportedAlphaMode(String),

    /// An image asset whose color space or alpha was never classified — the state a
    /// pre-WP06 project lands in after migration (DECISIONS.md #40). Spec 15.11 forbids a
    /// silent transfer-function fallback, so the alternative to asking is to refuse.
    #[error("asset {asset} carries unclassified color or alpha metadata; re-import it to classify")]
    UnclassifiedAssetMetadata { asset: String },

    /// The configured surface changed size or format. Spec 15.11: "Surface
    /// lost/outdated triggers reconfiguration" — a distinct outcome from device loss, and
    /// `anim_app` owns the reconfiguration (15.10), so this travels rather than acts.
    #[error("surface {outcome}: reconfigure before the next submission")]
    SurfaceUnavailable { outcome: String },

    /// The device went away. Spec 15.11: "Device loss invalidates history and GPU
    /// targets"; 21.5 adds that submission stops and recreation waits for the old
    /// resources to be unreferenced.
    #[error("device lost ({reason}): history and GPU targets are invalid")]
    DeviceLost { reason: String },

    /// An allocation the adapter refused. Spec 15.11 sends this to WP07's recovery, which
    /// does not exist yet, so the variant exists to stop the fallback WP07 would replace:
    /// without it, an out-of-memory would surface as a submission error.
    #[error("out of memory reserving {requested} bytes for {purpose}: {detail}")]
    OutOfMemory {
        purpose: String,
        requested: u64,
        detail: String,
    },

    /// A readback that never completed. Distinct from `DeviceLost` for the same reason
    /// the probe keeps them apart: on this machine a stalled map and a power-gated GPU
    /// look identical from `poll`, and mislabelling one hides the other.
    #[error("readback for {purpose} did not complete: {detail}")]
    ReadbackFailed { purpose: String, detail: String },

    /// WGSL that does not compile. Checked before submission so a shader defect stays a
    /// shader defect rather than arriving as device loss.
    ///
    /// The field is named `shader` and not `source`: `thiserror` reserves `source` for the
    /// error a variant wraps, and a plain `String` named `source` is a compile error rather
    /// than a wrong message.
    #[error("shader {shader} failed to compile: {detail}")]
    ShaderCompilation { shader: String, detail: String },

    /// A submission or pipeline-creation failure that is none of the above.
    #[error("submission failed in {stage}: {detail}")]
    Submission { stage: String, detail: String },

    /// A quality profile or output transfer function that is not one of the declared
    /// ones. Spec 15.11's last clause names three things that may not be substituted;
    /// this variant is the third and first, and there is no variant for "backend" because
    /// the backend is fixed at construction (spec 9.11) rather than chosen at runtime.
    #[error("no declared profile for {requested}: {detail}")]
    UnknownProfile { requested: String, detail: String },

    /// The graph asked for a pass sequence that is not internally consistent — an effect
    /// reading a target no earlier pass wrote, or a disabled pass claiming a binding.
    #[error("invalid pass graph at {stage}: {detail}")]
    GraphInvalid { stage: String, detail: String },

    /// Input that should be impossible: a state the crate's own invariants exclude.
    /// Returned, never panicked, because 15.11 gives a renderer no panic path and
    /// `clippy::unwrap_used` is denied workspace-wide.
    #[error("internal render invariant violated in {stage}: {detail}")]
    Internal { stage: String, detail: String },
}

impl RenderError {
    pub(crate) fn internal(stage: &str, detail: impl Into<String>) -> Self {
        Self::Internal {
            stage: stage.to_owned(),
            detail: detail.into(),
        }
    }
}
