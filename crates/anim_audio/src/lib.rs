//! Deterministic offline audio: exact sample planning, decode, locked resampling, ordered
//! mixing and canonical WAV output (spec 4.2, 17), the offline half of work package WP08.
//!
//! The claim this crate exists to make is 17.6's: offline rendering is a **pure function**
//! of the immutable scene snapshot, the verified assets and an [`OfflineAudioSpec`]. Four
//! decisions keep it pure, and each has a 17.12 test rather than a comment:
//!
//! - **integer placement** — every timeline position becomes a sample index through WP01's
//!   exact rational conversion ([`time::sample_range`], `FrameRate::sample_at`). No
//!   floating-point second ever decides where a clip starts; `f64` appears only at the
//!   final mix gain, accumulation and rounding ([`mix`]).
//! - **canonical order** — [`plan::MixPlan`] sorts clips by `(timeline_start, clip_id)` and
//!   is immutable after construction, so the accumulator adds terms in one order on every
//!   run. There is no `HashMap`, no lock and no wall clock (`clippy.toml` denies them).
//! - **locked resampling** — [`resample`] forces rubato's *scalar* interpolator
//!   (DECISIONS.md #46 ruling 4) so the decode -> resample -> mix path is bit-exact on every
//!   CPU, not just the reference machine, and symphonia's `opt-simd` stays off.
//! - **chunk independence** — mixing is per-sample with no cross-chunk state, so
//!   [`offline::render_offline`] produces identical bytes at any chunk size (17.12).
//!
//! Spec 17.4 lets this crate depend on `anim_core`, `anim_assets` and `anim_scene` only.
//! The authored clip reaches it through `SceneSnapshot.audio_clips` (DECISIONS.md #46
//! ruling 1), re-exported as [`AudioClip`] / [`ChannelLayout`] from `anim_scene`. The
//! preview half (`preview.rs`, `output.rs`, cpal/ringbuf) is **parked** (ruling 3): the
//! `preview-audio` feature is declared but no offline module names a device, a clock, the
//! renderer or the editor.
//!
//! The error taxonomy lives here rather than in an `error.rs` because 17.3 lists no such
//! file — the resolution `anim_assets` reached for `AssetError` (DECISIONS.md #29) and
//! `anim_sim`/`anim_scene` for theirs. Every variant has an offline producer; the
//! device-loss and output-stream categories 17.11 also names belong to preview and are
//! deliberately absent (risk 19: no unreachable variant left to look covered).

pub mod decode;
pub mod mix;
pub mod offline;
pub mod plan;
pub mod resample;
pub mod time;
pub mod wav;

pub use decode::{decode_source, NativePcm};
pub use mix::mix_range;
pub use offline::render_offline;
pub use plan::{
    compile_plan, AssetResolver, CanonicalPcm, MixPlan, OfflineAudioSpec, PcmAssetSource,
    PlannedClip,
};
pub use resample::{resample_to_48k, resampler_settings_hash};
pub use time::{sample_range, SampleOffset, SampleRange};
pub use wav::write_wav;

// The clip vocabulary is authored in `anim_project` and exposed through `anim_scene`; name
// it here so a consumer of this crate reads one type (17.4 forbids the anim_project edge).
pub use anim_scene::{AudioClip, ChannelLayout};

use anim_core::error::CoreError;

/// The canonical export sample rate (spec 17.7). Every conversion targets this rate, and
/// [`OfflineAudioSpec::validate`] refuses any other, so "48 kHz" is one constant rather
/// than a literal repeated at each boundary.
pub const EXPORT_SAMPLE_RATE: u32 = 48_000;

/// The canonical export channel count: stereo (spec 17.8).
pub const EXPORT_CHANNELS: u16 = 2;

/// The default mix chunk in sample frames (spec 17.8's "1,024-sample chunks").
pub const DEFAULT_CHUNK_SAMPLES: u32 = 1024;

/// The version of the offline planning/scheduling algorithm: frame-to-sample conversion,
/// clip selection, source-offset preservation and the plan order (spec 17.6).
///
/// Bumping this is a statement that every plan this crate ever compiled came from a
/// different function. It is carried in [`OfflineAudioSpec`] and validated, so a spec built
/// against another version is refused rather than silently re-read.
pub const OFFLINE_AUDIO_ALGORITHM_VERSION: u32 = 1;

/// The version of the locked resampler configuration (spec 17.6, DECISIONS.md #46 ruling
/// 4). A change to any constant in [`resample`] is a bump of this number, not a quiet edit;
/// it is folded into [`resampler_settings_hash`] so a derived cache keyed on it cannot
/// serve yesterday's coefficients.
pub const RESAMPLER_ALGORITHM_VERSION: u32 = 1;

/// The version of the mix algorithm: the canonical accumulation order, the mono-to-stereo
/// conversion, the linear sample-indexed fades and the symmetric rounding rule (spec 17.7).
pub const MIX_ALGORITHM_VERSION: u32 = 1;

/// The domain separator for a canonical PCM digest (ADR-0004 consequence 5's rule, the one
/// the document, manifest, snapshot and simulation state already follow). A PCM hash is
/// only reproducible from a named domain and the sample bytes, so the tag is applied here
/// once rather than remembered by every caller.
pub const PCM_DOMAIN: &str = "anim_engine/audio_pcm/v1";

/// The typed errors of the offline audio path (spec 17.11).
///
/// `Clone + PartialEq` so a test can assert an exact category and an integration report can
/// carry the finding without re-deriving it. The two `#[from]` wrappers keep the underlying
/// `anim_assets` / `anim_core` diagnosis intact when the fault is genuinely theirs (an IO
/// failure, an incompatible decode configuration, a reversed range) rather than inventing a
/// second spelling of it.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum AudioError {
    /// A sample rate or frame rate that is not the canonical export rate, or is zero.
    #[error("invalid audio rate {rate}")]
    InvalidRate { rate: String },

    /// An empty frame range: zero frames selected is a real state WP01 allows but export
    /// (17.7) refuses, because it would produce a zero-sample WAV with no identity.
    #[error("empty frame range [{start}, {end}) selects no samples")]
    EmptyRange { start: i64, end: i64 },

    /// A negative export sample. WP01's signed calculation can land below zero; the export
    /// offset wrapper refuses it rather than wrapping (spec 17.5, 17.7).
    #[error("negative export sample {sample}; export offsets are nonnegative")]
    NegativeRange { sample: i64 },

    /// A checked conversion or addition that does not fit its width. Never wraps, never
    /// clamps (spec 17.7's overflow refusal).
    #[error("arithmetic overflow in {operation}: {value}")]
    Overflow {
        operation: &'static str,
        value: String,
    },

    /// A declared audio shape that fails validation: an [`OfflineAudioSpec`] with a channel
    /// count that is not stereo, a zero chunk size or an unsupported algorithm version, or a
    /// [`CanonicalPcm`](crate::CanonicalPcm) whose sample buffer length disagrees with its
    /// declared `frames * channels`.
    #[error("invalid offline audio spec: {detail}")]
    InvalidSpec { detail: String },

    /// A clip references an asset the manifest does not hold or whose object is absent.
    /// Missing sources fail before rendering (spec 17.7), never replaced with silence.
    #[error("audio asset {asset_id} is missing")]
    MissingAsset { asset_id: String },

    /// A referenced asset's bytes or length disagree with its manifest record. The export
    /// verification policy (`Content`) caught a changed asset; 17.11 names it distinctly
    /// from a missing one because the remedy differs.
    #[error("audio asset {asset_id} changed: {detail}")]
    ChangedAsset { asset_id: String, detail: String },

    /// A clip whose authored semantics are illegal: a reversed or empty timeline range, a
    /// negative source offset or fade, or a gain that is not finite once converted.
    #[error("audio clip {clip_id} is malformed: {detail}")]
    MalformedClip { clip_id: String, detail: String },

    /// A looping clip whose canonical source has zero length: the checked modulo 17.7
    /// requires would divide by zero, so the plan refuses it.
    #[error("audio clip {clip_id} loops a source of length {source_len}")]
    InvalidLoop { clip_id: String, source_len: u64 },

    /// A channel count the mixer has no conversion rule for. 17.7 names mono and stereo
    /// only and says to reject the rest "until explicitly added".
    #[error("unsupported audio channel count {channels}")]
    UnsupportedLayout { channels: u16 },

    /// The pinned decoder could not read a source. `asset_id` is the content identity at
    /// the decode layer; the resolver enriches it to the logical asset id for the operator.
    #[error("decode of audio asset {asset_id} failed: {detail}")]
    DecodeFailure { asset_id: String, detail: String },

    /// The locked resampler failed, or its output count disagreed with the exact integer
    /// expectation. Carries the same identity convention as [`AudioError::DecodeFailure`].
    #[error("resample of audio asset {asset_id} failed: {detail}")]
    ResampleFailure { asset_id: String, detail: String },

    /// The WAV writer or the underlying writer failed (spec 17.11's output failure). The
    /// offline path aborts before publication, so no partial WAV is presented as complete.
    #[error("audio output failed: {detail}")]
    OutputFailure { detail: String },

    /// An asset-layer fault that is genuinely `anim_assets`' to report (an IO error, an
    /// incompatible decode configuration), preserved rather than re-spelled.
    #[error(transparent)]
    Asset(#[from] anim_assets::AssetError),

    /// A core arithmetic or range fault that is not an overflow or a rate (a reversed frame
    /// range, a subframe out of range), preserved from `anim_core`.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Reinterpret a [`CoreError`] at the audio boundary: overflow and rate faults become the
/// audio categories 17.11 names, everything else stays a core fault. Used by [`time`] and
/// [`plan`] so a WP01 overflow reads as [`AudioError::Overflow`] rather than a wrapped core
/// error, while a reversed range (which `anim_core` owns) keeps its diagnosis.
pub(crate) fn from_core(e: CoreError) -> AudioError {
    match e {
        CoreError::ArithmeticOverflow { operation, value } => {
            AudioError::Overflow { operation, value }
        }
        CoreError::NonPositiveRate {
            numerator,
            denominator,
        } => AudioError::InvalidRate {
            rate: format!("{numerator}/{denominator}"),
        },
        other => AudioError::Core(other),
    }
}
