//! Device, graph, passes, GPU resources, memory policy and readback (spec 4.2).
//!
//! Owns the production GPU and DXGI code. Nothing in this crate imports a WP00
//! probe binary (spec 9.10); the probes prove capabilities this crate later uses.
//!
//! The crate is one forward pass sequence — opaque and alpha-tested, ordered 2D, sorted
//! transparent, the linear HDR post chain, one tone map, output transfer — over the
//! resize-owned full-resolution targets of spec 15.6. Two rules shape everything below:
//!
//!  * **Nothing falls back.** Spec 15.11 forbids a silent backend, transfer-function or
//!    quality substitute, so every capability the graph needs is validated against the
//!    adapter before the device is opened, and a missing one is an error rather than a
//!    degraded run. Spec 9.11 says the same about adapters.
//!  * **Order comes from data, not from hardware.** Spec 15.7 prohibits hash-map and GPU
//!    timing order in the draw list, so the transparent sort is a stable total comparator
//!    over authored fields and the pass sequence is recorded in a log the gate can read
//!    (15.12).
//!
//! # Error taxonomy
//!
//! [`RenderError`] lives here rather than in an `error.rs` because spec 15.3 lists no such
//! file for this crate — the same omission DECISIONS.md #29 recorded for `anim_assets`.
//!
//! # Two shapes spec 15.9 names but 15.8 does not define
//!
//! [`RenderQuality`] and `draw_list::PreviousFrameState` are named by 15.9's signatures and
//! given no fields anywhere in the spec. What is written here is only what 15.7's algorithms
//! need to run: an extent (the projection and the resize test both read it), a final alpha
//! mode (15.7's "only when the selected codec requires it"), and the previous submission's
//! transforms and history. `RenderQuality` is the resolved profile 15.8 asks for — it holds
//! no `Default`, so a caller cannot get one by accident.

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
use anim_core::id::NodeId;

pub mod alpha;
pub mod camera;
pub mod device;
pub mod draw_list;
pub mod graph;
pub mod memory;
pub mod passes;
pub mod readback;
pub mod resources;
pub mod targets;

pub use alpha::AlphaMode;
pub use camera::{CameraMatrices, HistoryState, Invalidation};
pub use device::{GpuContext, VramFloor};
pub use draw_list::{extract_frame, DrawItem, PreviousFrameState};
pub use graph::{
    create_renderer, PassKind, PassRecord, PostChainConfig, Renderer, POST_CHAIN_VERSION,
};
pub use memory::{
    AdapterIdentity, BudgetSnapshot, MemoryError, MemoryManager, MemoryMode, MemoryPolicy,
};
pub use targets::RenderTargets;

/// Crate result alias, matching `anim_scene` and `anim_sim`.
pub type Result<T, E = RenderError> = std::result::Result<T, E>;

/// The resolved quality profile a frame is extracted and drawn at (spec 15.8).
///
/// 15.8: "RenderQuality is explicit; export receives a fixed resolved profile." There is no
/// `Default` impl on purpose — a default extent would be a quality fallback, which 15.11
/// forbids by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderQuality {
    /// The full-resolution extent every 15.6 target is allocated at.
    pub extent: wgpu::Extent3d,
    /// The target generation this frame will be drawn into.
    ///
    /// 15.7 lists target resize as a history-invalidation condition, and 15.8's
    /// `HistoryState.target_generation` is what detects it. Extraction receives no targets,
    /// so the generation travels in the resolved profile instead — otherwise the check
    /// would compare the previous generation against itself and never fire.
    /// [`crate::graph::Renderer::quality`] fills it from the live targets.
    pub target_generation: u64,
    /// The alpha the clean output carries. 15.7: premultiplied output converts to straight
    /// "only when the selected codec requires it", so this is the codec's requirement
    /// resolved once by the caller, not a per-frame decision.
    pub final_alpha: FinalAlpha,
}

/// The alpha convention of the clean output target (spec 15.7's final alpha mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalAlpha {
    /// Leave the accumulated premultiplied result alone. The internal convention of every
    /// 15.6 target, and what a codec that accepts premultiplied input receives.
    Premultiplied,
    /// Divide RGB by alpha on the way out, with zero-alpha RGB defined as
    /// [`alpha::ZERO_ALPHA_RGB`]. Required by codecs that expect straight alpha.
    Straight,
}

impl FinalAlpha {
    /// Stable spelling for the pass log, which is a gate artifact rather than a debug dump.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premultiplied => "premultiplied",
            Self::Straight => "straight",
        }
    }
}

/// Initialization input for [`create_renderer`] (spec 15.9).
#[derive(Clone, Debug)]
pub struct RenderConfig {
    /// The extent the first target generation is allocated at. Zero is refused by
    /// [`Renderer::resize`] and by construction, before anything is allocated.
    pub extent: wgpu::Extent3d,
    /// Which post stages run. WP06 implements the chain and exactly one tone map; the
    /// effect kernels are declared and disabled (DECISIONS.md #40 ruling 4).
    pub post: PostChainConfig,
}

/// One rendered frame's outputs, with the generation they were drawn into.
///
/// Spec 15.9 makes this the owner of "the final clean output and generation metadata". It
/// deliberately carries no surface view: `composite_editor_ui` takes a `&TextureView` the
/// caller owns, so the editor's presentation target never becomes something the renderer
/// hands out and export can never reach it.
pub struct RenderedScene {
    /// Where the clean output went.
    pub output: OutputDestination,
    /// The target generation this frame was drawn into (15.8).
    pub generation: u64,
    /// The extent it was drawn at.
    pub extent: wgpu::Extent3d,
    /// What the graph actually ran, in order. Spec 15.12's "command-log pass-order
    /// verification" is checked against this artifact.
    pub pass_log: Vec<PassRecord>,
}

/// Where the clean output goes (spec 15.9).
///
/// "Export requests a readback-compatible `OutputDestination` and cannot call
/// `composite_editor_ui`." The enum is what makes that structural: the editor composite
/// takes a `&TextureView` the caller owns, and a readback destination has none to give it.
pub enum OutputDestination {
    /// CPU bytes. The only form export can consume, and the form that makes a golden
    /// comparable at all.
    Readback(readback::OutputReadback),
    /// A texture the caller composits from. Editor-only.
    View(wgpu::TextureView),
}

/// Every refusal this crate can produce (spec 15.11).
///
/// Additions beyond 15.11's prose, each recorded rather than smuggled:
/// [`Self::MissingAsset`] and [`Self::NotAnImageAsset`] are the two reachable outcomes of
/// the manifest lookup DECISIONS.md #41 ruling 1 added to `extract_frame`;
/// [`Self::UnclassifiedAssetMetadata`] is #40 ruling 2's one consumer rule;
/// [`Self::Internal`] exists because 15.11 gives a renderer no panic path and
/// `clippy::unwrap_used` is denied workspace-wide.
///
/// **`UnsupportedAlphaMode` is deliberately absent.** 15.7 says extraction validates
/// "supported alpha modes", but [`AlphaMode`] is a closed five-variant enum and all five
/// have blend descriptors, so no condition can produce that refusal. DECISIONS.md #40 §7
/// decides not to declare it, on the precedent of WP05's deleted
/// `RandomError::UnsupportedAlgorithm`: a declared-but-unreachable variant reads as covered
/// error handling in a later audit. The reachable half of the same sentence is
/// [`Self::InvalidAlphaCutoff`], which has a producer and a test.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RenderError {
    /// Selection found no discrete NVIDIA adapter after the bounded retries. Spec 9.7's
    /// `IdentityUnavailable`. Distinct from a generic "not found" because on this hybrid
    /// laptop the same message means either "no dGPU" or "the dGPU is power-gated"
    /// (BUILD_STATE hazard 2), and the difference decides whether a retry is the remedy.
    #[error("adapter identity unavailable: {detail}")]
    IdentityUnavailable { detail: String },

    /// The selected adapter is not the one G0 certified. Spec 9.12 will not carry a pass
    /// across a different adapter or driver without revalidation, and DECISIONS.md #18
    /// keys that on PCI bus id, adapter name and driver.
    #[error("adapter does not match the certified G0 evidence: {field} expected {expected:?}, found {found:?}")]
    AdapterRevalidation {
        field: &'static str,
        expected: String,
        found: String,
    },

    /// `AdapterInfo::device_pci_bus_id` was empty. #18's residual: "Require non-empty and
    /// hard-fail; do not treat a blank as 'unknown, proceed'." A blank anchor is not a
    /// missing check, it is a check that passed on nothing. #40 ruling 1 places the value
    /// on `wgpu::AdapterInfo`, so this is reachable without any raw-Vulkan dependency.
    #[error(
        "no PCI bus id for adapter {adapter:?}; refusing to treat an absent bus id as a match"
    )]
    PciBusIdUnavailable { adapter: String },

    /// A format or usage the graph is built around is unavailable. 15.11's first sentence,
    /// and an initialization error rather than a runtime one because discovering it
    /// mid-export would burn the whole run.
    #[error("required capability missing: {format} does not allow {usage} ({purpose})")]
    RequiredCapabilityMissing {
        format: String,
        usage: &'static str,
        purpose: String,
    },

    /// The adapter cannot provide a sample count of 1 with no multisampling, or refuses a
    /// limit the graph needs. #40 ruling 6 closes the ADR-0003 half as an explicit list
    /// checked on the adapter before `request_device`.
    #[error("adapter limit {limit} is {found}, which the render graph requires to be {required}")]
    LimitUnavailable {
        limit: &'static str,
        required: String,
        found: String,
    },

    /// Extent zero in either axis. 15.11 pairs the refusal with a promise — "without
    /// destroying the last valid targets" — so this is returned *by* `resize` before
    /// anything is reallocated and the previous generation stays current.
    #[error("extent {width}x{height} is degenerate; the last valid targets are unchanged")]
    ZeroExtent { width: u32, height: u32 },

    /// A matrix or position that is not finite. 15.7 validates finiteness during
    /// extraction; 15.11 lists NaN transforms first among the extraction failures.
    #[error("non-finite transform on node {entity} draw_index {draw_index}: {detail}")]
    NonFiniteTransform {
        entity: NodeId,
        draw_index: u32,
        detail: String,
    },

    /// View depth that is not finite, so it cannot be quantized to the `i32` the sort key
    /// uses (15.7) and the item's position in the order would be arbitrary.
    #[error("non-finite view depth on node {entity} draw_index {draw_index}")]
    NonFiniteViewDepth { entity: NodeId, draw_index: u32 },

    /// Two items with one `(entity_id, draw_index)`. 15.7: "Duplicate keys fail
    /// extraction" — the key is the sort's last-resort tie-break, so a repeat makes the
    /// order undetermined by the data and the golden irreproducible.
    #[error("duplicate draw key (entity {entity}, draw_index {draw_index})")]
    DuplicateDrawKey { entity: NodeId, draw_index: u32 },

    /// An alpha-test cutoff outside the range the comparison means. The reachable half of
    /// 15.7's "supported alpha modes" sentence (#40 §7).
    #[error(
        "invalid alpha-test cutoff {cutoff} on node {entity}: expected a finite value in [0, 1]"
    )]
    InvalidAlphaCutoff { entity: NodeId, cutoff: f32 },

    /// A node names an asset the manifest does not have. One of the two reachable outcomes
    /// of the lookup DECISIONS.md #41 ruling 1 added; 15.11's list has no row for it.
    #[error("node {entity} names asset {asset}, which the manifest does not contain")]
    MissingAsset { entity: NodeId, asset: String },

    /// A node names an asset that is not an image, so it has no colour or alpha claim and
    /// no pixels to sample. The second reachable outcome of #41's lookup. Refused rather
    /// than drawn untextured, because an untextured quad is a silent substitute for content
    /// the author named.
    #[error("node {entity} names asset {asset} of kind {kind}, which is not an image")]
    NotAnImageAsset {
        entity: NodeId,
        asset: String,
        kind: String,
    },

    /// A draw item's asset is in the manifest but was never uploaded to the GPU registry.
    /// Distinct from [`Self::MissingAsset`]: that one is a manifest lookup failure, this one
    /// is a missing upload, and the remedy differs (import versus register). 15.11 forbids
    /// the silent substitute a white placeholder would be.
    #[error("asset {asset} has no registered GPU texture; upload it before rendering")]
    TextureNotRegistered { asset: String },

    /// An image asset whose colour space or alpha was never classified — the state a
    /// pre-WP06 project lands in after `V3_TO_V4` (#40 ruling 2). 15.11 forbids a silent
    /// transfer-function fallback, so the alternative to asking is to refuse.
    #[error(
        "asset {asset} carries unclassified color or alpha metadata; re-import it to classify"
    )]
    UnclassifiedAssetMetadata { asset: String },

    /// A post stage was enabled whose kernel WP06 does not implement. #40 ruling 4 declares
    /// DOF, motion blur and bloom and leaves them disabled; enabling one is refused rather
    /// than approximated, because an invented kernel is a parameter WP14/WP17 will replace.
    #[error("post stage {stage} is declared but not implemented in WP06; {owner} owns its kernel")]
    PostStageUnavailable {
        stage: &'static str,
        owner: &'static str,
    },

    /// The configured surface changed size or format. 15.11: "Surface lost/outdated
    /// triggers reconfiguration" — distinct from device loss, and `anim_app` owns the
    /// reconfiguration (15.10), so this travels rather than acts.
    #[error("surface {outcome}: reconfigure before the next submission")]
    SurfaceUnavailable { outcome: String },

    /// The device went away. 15.11: "Device loss invalidates history and GPU targets".
    #[error("device lost ({reason}): history and GPU targets are invalid")]
    DeviceLost { reason: String },

    /// An allocation the adapter refused. 15.11 sends this to WP07's recovery, which does
    /// not exist yet, so the variant exists to stop the fallback WP07 would replace:
    /// without it an out-of-memory would surface as a submission error.
    #[error("out of memory reserving {requested} bytes for {purpose}: {detail}")]
    OutOfMemory {
        purpose: String,
        requested: u64,
        detail: String,
    },

    /// A readback that never completed. Distinct from `DeviceLost` for the reason the WP00
    /// probe keeps them apart: on this machine a stalled map and a power-gated GPU look
    /// identical from `poll`, and mislabelling one hides the other.
    #[error("readback for {purpose} did not complete: {detail}")]
    ReadbackFailed { purpose: String, detail: String },

    /// WGSL that does not compile. Checked before submission so a shader defect stays a
    /// shader defect rather than arriving as device loss.
    ///
    /// The field is `shader`, not `source`: `thiserror` reserves `source` for the wrapped
    /// error, and a plain `String` named `source` is a compile error rather than a wrong
    /// message.
    #[error("shader {shader} failed to compile: {detail}")]
    ShaderCompilation { shader: String, detail: String },

    /// A submission or pipeline-creation failure that is none of the above.
    #[error("submission failed in {stage}: {detail}")]
    Submission { stage: String, detail: String },

    /// A quality profile or output transfer that is not one of the declared ones. 15.11's
    /// last clause names three things that may not be substituted; there is no variant for
    /// "backend" because the backend is fixed at construction (spec 9.11).
    #[error("no declared profile for {requested}: {detail}")]
    UnknownProfile { requested: String, detail: String },

    /// The graph asked for a pass sequence that is not internally consistent — an effect
    /// reading a target no earlier pass wrote, or a disabled pass claiming a binding.
    #[error("invalid pass graph at {stage}: {detail}")]
    GraphInvalid { stage: String, detail: String },

    /// A decode or asset-resolution failure from `anim_assets`.
    #[error(transparent)]
    Asset(#[from] anim_assets::AssetError),

    /// Input that should be impossible: a state the crate's own invariants exclude.
    /// Returned, never panicked, because 15.11 gives a renderer no panic path.
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

    /// Stable diagnostic code, matching the `anim_scene`/`anim_sim` convention.
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        let raw = match self {
            Self::IdentityUnavailable { .. } => "render.identity_unavailable",
            Self::AdapterRevalidation { .. } => "render.adapter_revalidation",
            Self::PciBusIdUnavailable { .. } => "render.pci_bus_id_unavailable",
            Self::RequiredCapabilityMissing { .. } => "render.required_capability_missing",
            Self::LimitUnavailable { .. } => "render.limit_unavailable",
            Self::ZeroExtent { .. } => "render.zero_extent",
            Self::NonFiniteTransform { .. } => "render.non_finite_transform",
            Self::NonFiniteViewDepth { .. } => "render.non_finite_view_depth",
            Self::DuplicateDrawKey { .. } => "render.duplicate_draw_key",
            Self::InvalidAlphaCutoff { .. } => "render.invalid_alpha_cutoff",
            Self::MissingAsset { .. } => "render.missing_asset",
            Self::NotAnImageAsset { .. } => "render.not_an_image_asset",
            Self::TextureNotRegistered { .. } => "render.texture_not_registered",
            Self::UnclassifiedAssetMetadata { .. } => "render.unclassified_asset_metadata",
            Self::PostStageUnavailable { .. } => "render.post_stage_unavailable",
            Self::SurfaceUnavailable { .. } => "render.surface_unavailable",
            Self::DeviceLost { .. } => "render.device_lost",
            Self::OutOfMemory { .. } => "render.out_of_memory",
            Self::ReadbackFailed { .. } => "render.readback_failed",
            Self::ShaderCompilation { .. } => "render.shader_compilation",
            Self::Submission { .. } => "render.submission",
            Self::UnknownProfile { .. } => "render.unknown_profile",
            Self::GraphInvalid { .. } => "render.graph_invalid",
            Self::Asset(error) => return error.code(),
            Self::Internal { .. } => "render.internal",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    /// Whether the condition stops the frame or only asks for reconfiguration.
    #[must_use]
    pub fn severity(&self) -> DiagnosticSeverity {
        match self {
            // 15.11 says the surface outcome "triggers reconfiguration", and 15.10 gives
            // that job to anim_app. It is the one refusal here that is a request rather
            // than a stop.
            Self::SurfaceUnavailable { .. } => DiagnosticSeverity::Warning,
            Self::Asset(error) => error.severity(),
            _ => DiagnosticSeverity::Error,
        }
    }

    /// The diagnostic form, with the context spec 7.4 requires carried as fields.
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        let base = Diagnostic::new(self.code(), self.severity(), self.to_string());
        match self {
            Self::AdapterRevalidation {
                field,
                expected,
                found,
            } => base
                .with_context("field", *field)
                .with_context("expected", expected.clone())
                .with_context("found", found.clone()),
            Self::PciBusIdUnavailable { adapter } => base.with_context("adapter", adapter.clone()),
            Self::RequiredCapabilityMissing {
                format,
                usage,
                purpose,
            } => base
                .with_context("format", format.clone())
                .with_context("usage", *usage)
                .with_context("purpose", purpose.clone()),
            Self::LimitUnavailable {
                limit,
                required,
                found,
            } => base
                .with_context("limit", *limit)
                .with_context("required", required.clone())
                .with_context("found", found.clone()),
            Self::ZeroExtent { width, height } => base
                .with_context("width", width.to_string())
                .with_context("height", height.to_string()),
            Self::NonFiniteTransform {
                entity, draw_index, ..
            }
            | Self::NonFiniteViewDepth { entity, draw_index }
            | Self::DuplicateDrawKey { entity, draw_index } => base
                .with_context("entity", entity.to_canonical_string())
                .with_context("draw_index", draw_index.to_string()),
            Self::InvalidAlphaCutoff { entity, cutoff } => base
                .with_context("entity", entity.to_canonical_string())
                .with_context("cutoff", cutoff.to_string()),
            Self::MissingAsset { entity, asset } | Self::NotAnImageAsset { entity, asset, .. } => {
                base.with_context("entity", entity.to_canonical_string())
                    .with_context("asset", asset.clone())
            }
            Self::UnclassifiedAssetMetadata { asset } | Self::TextureNotRegistered { asset } => {
                base.with_context("asset", asset.clone())
            }
            Self::PostStageUnavailable { stage, owner } => base
                .with_context("stage", *stage)
                .with_context("owner", *owner),
            Self::OutOfMemory {
                purpose, requested, ..
            } => base
                .with_context("purpose", purpose.clone())
                .with_context("requested", requested.to_string()),
            Self::ShaderCompilation { shader, .. } => base.with_context("shader", shader.clone()),
            Self::Submission { stage, .. }
            | Self::GraphInvalid { stage, .. }
            | Self::Internal { stage, .. } => base.with_context("stage", stage.clone()),
            Self::ReadbackFailed { purpose, .. } => base.with_context("purpose", purpose.clone()),
            Self::UnknownProfile { requested, .. } => {
                base.with_context("requested", requested.clone())
            }
            Self::Asset(error) => error.to_diagnostic(),
            _ => base,
        }
    }
}
