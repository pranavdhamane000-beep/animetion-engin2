//! Stable clip selection and immutable mix-plan construction (spec 17.3, 17.7, 17.8, 17.9).
//!
//! The plan is the offline path's immutable spine: [`compile_plan`] selects every authored
//! clip intersecting the output range, clips its timeline exactly while preserving the
//! corresponding source offset, resolves and decodes each required source once, validates
//! the loop/gain/fade/layout rules, and sorts by `(timeline_start, clip_id)`. After
//! construction a [`MixPlan`] has no setter and no interior mutability, so the mixer adds
//! terms in one canonical order on every run (17.7).
//!
//! Two shape decisions are recorded rather than silent:
//!
//! - **`compile_plan` takes a fourth parameter, `fps: FrameRate`.** Spec 17.9's signature
//!   has three, but converting a clip's [`FramePos`] timeline bounds to samples needs the
//!   project frame rate, and neither [`OfflineAudioSpec`] (17.8 fixes its five fields) nor
//!   [`SceneSnapshot`] (it carries `time` and `position`, not a rate) supplies one. The rate
//!   is appended as a pure suffix, the same deviation shape WP06's fifth `extract_frame`
//!   parameter took (DECISIONS.md #41). See [`compile_plan`]'s own doc.
//! - **channel conversion is driven by the decoded PCM, not the authored layout.** The
//!   authored [`ChannelLayout`] is a closed enum (mono/stereo), so both are supported by
//!   construction; 17.7's "reject unsupported layouts" is enforced where an unsupported
//!   count can actually arrive — [`crate::decode`] and [`crate::mix`], against the decoded
//!   channel count. The mixer reads [`CanonicalPcm::channels`], the truth of the bytes, so a
//!   layout/asset disagreement converts the actual samples rather than mis-indexing them.

use std::cell::RefCell;
use std::collections::BTreeMap;

use anim_assets::resolver::{resolve_with, ResolutionPolicy, ResolvedAsset};
use anim_assets::{AssetError, AssetKind, AssetManifest, AssetStore, ContentHash};
use anim_core::id::{AssetId, TrackId};
use anim_core::rational::Rational;
use anim_core::time::{FrameRange, FrameRate};
use anim_scene::SceneSnapshot;

use crate::time::{sample_range, SampleOffset, SampleRange};
use crate::{
    from_core, AudioClip, AudioError, EXPORT_CHANNELS, EXPORT_SAMPLE_RATE,
    OFFLINE_AUDIO_ALGORITHM_VERSION,
};

/// The validated parameters of one offline render (spec 17.8).
///
/// Initializes to 48 kHz, stereo, 1,024-sample chunks and the current algorithm version.
/// The default `frames` is the empty range `[0, 0)`: a caller must set the export range
/// before [`OfflineAudioSpec::validate`] accepts it, because export refuses zero frames
/// (17.7). The fields are public exactly as 17.8 spells them; validation is a separate gate
/// rather than a constructor, so a spec can be built, inspected and corrected before use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfflineAudioSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: FrameRange,
    pub chunk_samples: u32,
    pub algorithm_version: u32,
}

impl Default for OfflineAudioSpec {
    fn default() -> Self {
        Self {
            sample_rate: EXPORT_SAMPLE_RATE,
            channels: EXPORT_CHANNELS,
            frames: FrameRange { start: 0, end: 0 },
            chunk_samples: crate::DEFAULT_CHUNK_SAMPLES,
            algorithm_version: OFFLINE_AUDIO_ALGORITHM_VERSION,
        }
    }
}

impl OfflineAudioSpec {
    /// Every rule 17.8's "unless validated otherwise" gates.
    ///
    /// # Errors
    ///
    /// [`AudioError::InvalidRate`] unless the rate is exactly 48 kHz; [`AudioError::InvalidSpec`]
    /// for a non-stereo channel count, a zero chunk size or an unsupported algorithm version;
    /// [`AudioError::EmptyRange`] / [`AudioError::NegativeRange`] / [`AudioError::Core`] for an
    /// empty, negative or reversed frame range. The full sample conversion (which needs the
    /// frame rate) is [`compile_plan`]'s job via [`sample_range`]; this checks the frame range
    /// structurally so a spec is refused before any asset is touched.
    pub fn validate(&self) -> Result<(), AudioError> {
        if self.sample_rate != EXPORT_SAMPLE_RATE {
            return Err(AudioError::InvalidRate {
                rate: self.sample_rate.to_string(),
            });
        }
        if self.channels != EXPORT_CHANNELS {
            return Err(AudioError::InvalidSpec {
                detail: format!(
                    "channels must be {EXPORT_CHANNELS} (stereo), got {}",
                    self.channels
                ),
            });
        }
        if self.chunk_samples == 0 {
            return Err(AudioError::InvalidSpec {
                detail: "chunk_samples must be nonzero".to_owned(),
            });
        }
        if self.algorithm_version != OFFLINE_AUDIO_ALGORITHM_VERSION {
            return Err(AudioError::InvalidSpec {
                detail: format!(
                    "algorithm_version {} is unsupported (this build is {OFFLINE_AUDIO_ALGORITHM_VERSION})",
                    self.algorithm_version
                ),
            });
        }
        if self.frames.is_empty() {
            return Err(AudioError::EmptyRange {
                start: self.frames.start,
                end: self.frames.end,
            });
        }
        if self.frames.start > self.frames.end {
            return Err(AudioError::Core(
                anim_core::error::CoreError::InvalidRange {
                    kind: "frame",
                    start: self.frames.start,
                    end: self.frames.end,
                },
            ));
        }
        if self.frames.start < 0 {
            return Err(AudioError::NegativeRange {
                sample: self.frames.start,
            });
        }
        Ok(())
    }
}

/// One clip resolved into exact sample coordinates (spec 17.8's field list, verbatim).
///
/// `gain` is the single `f64` in the plan: the authored [`Rational`] gain converted once via
/// `to_f64` (zero when muted), because 17.6 forbids a float from deciding placement but the
/// mix accumulator is `f64`. Every coordinate — timeline, source start, source length, fade
/// lengths — is an integer sample count, so placement never touches floating point.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedClip {
    pub clip_id: TrackId,
    pub asset_id: AssetId,
    pub timeline: SampleRange,
    pub source_start: SampleOffset,
    pub source_len: u64,
    pub looped: bool,
    pub gain: f64,
    pub fade_in_samples: u64,
    pub fade_out_samples: u64,
}

/// An immutable, canonically ordered mix plan (spec 17.8's "plans are immutable after
/// construction").
///
/// The fields are private and the only constructor sorts, so the canonical order
/// `(timeline.start, clip_id)` is a property of the type rather than a step a caller could
/// forget. There is no `&mut` accessor; the mixer reads [`MixPlan::clips`] and nothing else.
#[derive(Clone, Debug, PartialEq)]
pub struct MixPlan {
    clips: Vec<PlannedClip>,
    range: SampleRange,
    spec: OfflineAudioSpec,
}

impl MixPlan {
    /// Build a plan, sorting clips into canonical order.
    ///
    /// Public so a synthetic source (a test, a future preview planner) can construct a plan
    /// from already-resolved [`PlannedClip`]s; [`compile_plan`] is the canonical producer for
    /// the offline path. The fields stay private and there is no setter, so the plan is
    /// immutable after construction however it was built. The constructor enforces the
    /// order; the mixer enforces the per-clip source invariants (channel count, length, and
    /// a nonzero loop modulus) so a hand-built plan cannot index out of bounds or divide by
    /// zero.
    #[must_use]
    pub fn new(mut clips: Vec<PlannedClip>, range: SampleRange, spec: OfflineAudioSpec) -> Self {
        // Stable sort by start sample then clip id (17.5/17.7). `sort_by` is stable, and the
        // clip id tie-break makes the order total even for two clips sharing a start.
        clips.sort_by(|a, b| {
            a.timeline
                .start
                .0
                .cmp(&b.timeline.start.0)
                .then_with(|| a.clip_id.cmp(&b.clip_id))
        });
        Self { clips, range, spec }
    }

    /// The planned clips in canonical order.
    #[must_use]
    pub fn clips(&self) -> &[PlannedClip] {
        &self.clips
    }

    /// The exact output sample range this plan was compiled for.
    #[must_use]
    pub fn range(&self) -> SampleRange {
        self.range
    }

    /// The validated spec this plan was compiled against.
    #[must_use]
    pub fn spec(&self) -> &OfflineAudioSpec {
        &self.spec
    }
}

/// One asset decoded and resampled to canonical 48 kHz interleaved `f32` (spec 17.9's
/// "canonical rate/channel/sample access and exact asset revision metadata").
///
/// `content_hash` is the revision: the verified digest of the source bytes, so a cache entry
/// names the content it was built from. The fields are private behind a validating
/// constructor because `samples.len() == frames * channels` is an invariant a reader relies
/// on — the mixer indexes `source_pos * channels` without a second bounds derivation.
#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalPcm {
    content_hash: ContentHash,
    channels: u16,
    frames: u64,
    samples: Vec<f32>,
}

impl CanonicalPcm {
    /// Build canonical PCM, checking the sample buffer against the declared shape.
    ///
    /// # Errors
    ///
    /// [`AudioError::Overflow`] when `frames * channels` exceeds the address width;
    /// [`AudioError::InvalidSpec`] when the buffer length disagrees with that product.
    pub fn new(
        content_hash: ContentHash,
        channels: u16,
        frames: u64,
        samples: Vec<f32>,
    ) -> Result<Self, AudioError> {
        let expected = (frames as usize)
            .checked_mul(usize::from(channels))
            .ok_or_else(|| AudioError::Overflow {
                operation: "canonical PCM sample count",
                value: format!("{frames} frames * {channels} channels"),
            })?;
        if samples.len() != expected {
            return Err(AudioError::InvalidSpec {
                detail: format!(
                    "canonical PCM holds {} samples, expected {expected} ({frames} frames * {channels} channels)",
                    samples.len()
                ),
            });
        }
        Ok(Self {
            content_hash,
            channels,
            frames,
            samples,
        })
    }

    /// The verified content digest of the source — the exact revision metadata 17.9 names.
    #[must_use]
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// The canonical channel count (1 or 2); the rate is always [`EXPORT_SAMPLE_RATE`].
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// The canonical frame count at 48 kHz.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The interleaved samples, `frames * channels` long.
    #[must_use]
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }
}

/// Canonical PCM access for the mixer (spec 17.9).
///
/// The mixer depends on this trait, not on the resolver, so a test can hand it synthetic
/// PCM and the offline path can be exercised without a filesystem. The revision metadata
/// 17.9 asks for is [`CanonicalPcm::content_hash`].
pub trait PcmAssetSource {
    /// The canonical 48 kHz PCM for one asset, or the typed reason it is unavailable.
    ///
    /// # Errors
    ///
    /// Whatever the implementor reports for a missing, changed, undecodable or unsupported
    /// asset — never a silent silence substitution (17.11).
    fn canonical_pcm(&self, asset_id: AssetId) -> Result<CanonicalPcm, AudioError>;
}

/// A read-only asset resolver with a single-threaded, content-keyed offline PCM cache.
///
/// The cache is a [`RefCell`]`<`[`BTreeMap`]`>` rather than a lock: `clippy.toml` denies
/// `Mutex`/`RwLock` because concurrent access order must never reach exported bytes, and the
/// offline path is single-threaded, so a `RefCell` is the honest interior-mutability tool.
/// It cannot change output (17.8's "evictable without output changes"): every entry is a
/// pure function of the verified source bytes and the locked resampler settings, the map is
/// ordered, and a miss re-resolves under [`ResolutionPolicy::Content`] and re-decodes, so a
/// cold cache and a warm one produce identical PCM. The manifest is borrowed immutably for
/// the resolver's lifetime, so one [`AssetId`] names one content hash throughout.
pub struct AssetResolver<'a> {
    store: &'a AssetStore,
    manifest: &'a AssetManifest,
    cache: RefCell<BTreeMap<AssetId, CanonicalPcm>>,
}

impl<'a> AssetResolver<'a> {
    /// A resolver over a store and its manifest, with an empty cache.
    #[must_use]
    pub fn new(store: &'a AssetStore, manifest: &'a AssetManifest) -> Self {
        Self {
            store,
            manifest,
            cache: RefCell::new(BTreeMap::new()),
        }
    }

    /// Resolve one asset under export's full content verification (spec 11.7, 17.7).
    ///
    /// # Errors
    ///
    /// [`AudioError::MissingAsset`] for an absent record or object, [`AudioError::ChangedAsset`]
    /// for a hash or length mismatch, and [`AudioError::Asset`] for any other asset-layer fault.
    pub fn resolve(&self, asset_id: AssetId) -> Result<ResolvedAsset, AudioError> {
        resolve_with(
            self.store,
            self.manifest,
            asset_id,
            ResolutionPolicy::Content,
        )
        .map_err(map_asset_error)
    }
}

impl PcmAssetSource for AssetResolver<'_> {
    fn canonical_pcm(&self, asset_id: AssetId) -> Result<CanonicalPcm, AudioError> {
        if let Some(pcm) = self.cache.borrow().get(&asset_id) {
            return Ok(pcm.clone());
        }
        let resolved = self.resolve(asset_id)?;
        // A record whose kind is not Audio cannot be decoded to PCM; report the asset-layer
        // incompatibility rather than a generic decode failure (the configuration axis is
        // PcmF32Interleaved, spec 11.6's cache-key spelling).
        if resolved.entry.kind != AssetKind::Audio {
            return Err(AudioError::Asset(
                AssetError::IncompatibleDecodeConfiguration {
                    asset_id: asset_id.to_canonical_string(),
                    kind: resolved.entry.kind.to_string(),
                    configuration: anim_assets::DecoderConfiguration::PcmF32Interleaved.to_string(),
                },
            ));
        }
        let native =
            crate::decode::decode_source(&resolved.canonical_path, resolved.entry.content_hash)
                .map_err(|e| enrich_identity(e, asset_id))?;
        let canonical = crate::resample::resample_to_48k(native, resolved.entry.content_hash)
            .map_err(|e| enrich_identity(e, asset_id))?;
        self.cache.borrow_mut().insert(asset_id, canonical.clone());
        Ok(canonical)
    }
}

/// Compile the authored clips intersecting the export range into an immutable [`MixPlan`]
/// (spec 17.7's plan construction, 17.9's interface).
///
/// # The `fps` parameter (deviation from 17.9)
///
/// 17.9 writes `compile_plan(scene, assets, spec)` with three parameters, but a clip's
/// timeline bounds are [`anim_core::time::FramePos`] (project time) and converting them to
/// sample indices needs the project frame rate. Neither [`OfflineAudioSpec`] — 17.8 fixes
/// its five fields — nor [`SceneSnapshot`] — which carries `time` and `position` but no rate
/// — supplies one. `fps` is therefore appended as a fourth, pure-suffix parameter, the same
/// shape WP06's `extract_frame` took for its manifest argument (DECISIONS.md #41). The
/// caller (WP09's export, the editor) owns the document's frame rate and passes it here.
///
/// # Errors
///
/// The spec's own validation errors; [`AudioError::MalformedClip`] for a reversed/empty
/// timeline, a negative offset/fade or a non-finite gain; [`AudioError::InvalidLoop`] for a
/// looping clip over a zero-length source; and whatever the resolver reports for a missing,
/// changed or undecodable source. Missing sources fail here, before rendering (17.7).
pub fn compile_plan(
    scene: &SceneSnapshot,
    assets: &AssetResolver<'_>,
    spec: &OfflineAudioSpec,
    fps: FrameRate,
) -> Result<MixPlan, AudioError> {
    spec.validate()?;
    let range = sample_range(spec.frames, fps)?;
    // The range comes from sample_range, so its start is a nonnegative i64-valued sample;
    // the end is checked. Intersecting clip bounds against it stays in i64 until the final
    // checked conversion, so a clip before timeline zero clips correctly.
    let range_start = i64::try_from(range.start.0).map_err(|_| AudioError::Overflow {
        operation: "export range start",
        value: range.start.0.to_string(),
    })?;
    let range_end_i64 = range.end()?;
    let range_end = i64::try_from(range_end_i64).map_err(|_| AudioError::Overflow {
        operation: "export range end",
        value: range_end_i64.to_string(),
    })?;

    let mut clips = Vec::new();
    for clip in scene.audio_clips.iter() {
        validate_authored_clip(clip)?;

        let start_sample = fps
            .sample_at(clip.timeline_start, EXPORT_SAMPLE_RATE)
            .map_err(from_core)?;
        let end_sample = fps
            .sample_at(clip.timeline_end, EXPORT_SAMPLE_RATE)
            .map_err(from_core)?;
        // Intersect [start_sample, end_sample) with the output range. A clip wholly outside
        // contributes nothing and is not decoded — only required sources are resolved.
        let isect_start = start_sample.max(range_start);
        let isect_end = end_sample.min(range_end);
        if isect_start >= isect_end {
            continue;
        }

        let timeline_start = SampleOffset::from_i64(isect_start)?;
        let timeline_len =
            u64::try_from(isect_end - isect_start).map_err(|_| AudioError::Overflow {
                operation: "clipped timeline length",
                value: format!("{isect_end} - {isect_start}"),
            })?;
        // Clipping the start forward by d samples advances the source offset by the same d,
        // so the clip plays the part of the source that belongs at the output range's start.
        let advance =
            u64::try_from(isect_start - start_sample).map_err(|_| AudioError::Overflow {
                operation: "clipped source advance",
                value: format!("{isect_start} - {start_sample}"),
            })?;
        let source_base = duration_samples(clip.source_offset)?;
        let source_start =
            source_base
                .checked_add(advance)
                .ok_or_else(|| AudioError::Overflow {
                    operation: "clip source start",
                    value: format!("{source_base} + {advance}"),
                })?;

        let pcm = assets.canonical_pcm(clip.asset_id)?;
        let source_len = pcm.frames();
        if clip.looped && source_len == 0 {
            return Err(AudioError::InvalidLoop {
                clip_id: clip.clip_id.to_canonical_string(),
                source_len,
            });
        }

        // The single authored-value-to-f64 conversion. A Rational's f64 image is in fact
        // always finite today — its numerator and denominator are i128-bounded (|num| <=
        // ~1.7e38, den >= 1), far inside f64's range — so 17.7's "finite gains" is guaranteed
        // by the type. This guard is defensive: it documents the rule at the conversion site
        // and refuses a non-finite gain if Rational is ever widened, rather than mixing an
        // infinity into the accumulator.
        let gain = if clip.mute { 0.0 } else { clip.gain.to_f64() };
        if !gain.is_finite() {
            return Err(AudioError::MalformedClip {
                clip_id: clip.clip_id.to_canonical_string(),
                detail: format!("gain {} s is not finite as f64", clip.gain),
            });
        }

        clips.push(PlannedClip {
            clip_id: clip.clip_id,
            asset_id: clip.asset_id,
            timeline: SampleRange::new(timeline_start, timeline_len),
            source_start: SampleOffset::new(source_start),
            source_len,
            looped: clip.looped,
            gain,
            fade_in_samples: duration_samples(clip.fade_in)?,
            fade_out_samples: duration_samples(clip.fade_out)?,
        });
    }
    Ok(MixPlan::new(clips, range, *spec))
}

/// The authored-clip rules `compile_plan` enforces before any sample arithmetic.
///
/// The channel layout needs no check here: [`crate::ChannelLayout`] is closed at mono/stereo,
/// so both are supported by construction, and the decoded-count refusal lives in decode/mix.
fn validate_authored_clip(clip: &AudioClip) -> Result<(), AudioError> {
    let id = clip.clip_id.to_canonical_string();
    if clip.timeline_start >= clip.timeline_end {
        return Err(AudioError::MalformedClip {
            clip_id: id,
            detail: format!(
                "timeline range [{}, {}) is empty or reversed",
                clip.timeline_start.frame, clip.timeline_end.frame
            ),
        });
    }
    let zero = Rational::from_i128(0);
    for (field, value) in [
        ("source_offset", &clip.source_offset),
        ("fade_in", &clip.fade_in),
        ("fade_out", &clip.fade_out),
    ] {
        if *value < zero {
            return Err(AudioError::MalformedClip {
                clip_id: id,
                detail: format!(
                    "negative {field} ({value}); offsets and durations are nonnegative"
                ),
            });
        }
    }
    Ok(())
}

/// Convert an exact nonnegative duration in seconds to a floored sample count at 48 kHz.
///
/// The multiply and floor are exact rational arithmetic; only the final width check is a
/// conversion. Placement never uses a floating-point second (17.6).
fn duration_samples(seconds: Rational) -> Result<u64, AudioError> {
    let scaled = seconds
        .checked_mul(Rational::from_i128(i128::from(EXPORT_SAMPLE_RATE)))
        .map_err(from_core)?;
    let whole = scaled.floor_to_i64().map_err(from_core)?;
    Ok(SampleOffset::from_i64(whole)?.get())
}

/// Map an asset-layer resolution fault to the audio category 17.11 names, preserving the
/// rest as the asset's own diagnosis.
fn map_asset_error(e: AssetError) -> AudioError {
    match e {
        AssetError::MissingAssetId { asset_id } | AssetError::MissingObject { asset_id, .. } => {
            AudioError::MissingAsset { asset_id }
        }
        AssetError::HashMismatch {
            asset_id,
            expected,
            actual,
        } => AudioError::ChangedAsset {
            asset_id,
            detail: format!("content hashes to {actual}, record states {expected}"),
        },
        AssetError::SizeMismatch {
            asset_id,
            expected,
            actual,
        } => AudioError::ChangedAsset {
            asset_id,
            detail: format!("{actual} bytes on disk, record states {expected}"),
        },
        other => AudioError::Asset(other),
    }
}

/// Replace the decode/resample-layer content identity with the logical asset id, so an
/// operator-facing error names the asset they referenced rather than its digest.
fn enrich_identity(e: AudioError, asset_id: AssetId) -> AudioError {
    let id = asset_id.to_canonical_string();
    match e {
        AudioError::DecodeFailure { detail, .. } => AudioError::DecodeFailure {
            asset_id: id,
            detail,
        },
        AudioError::ResampleFailure { detail, .. } => AudioError::ResampleFailure {
            asset_id: id,
            detail,
        },
        other => other,
    }
}
