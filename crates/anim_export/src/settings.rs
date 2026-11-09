//! Export settings, the immutable codec matrix and `validate` (spec 18.3, 18.6, 18.8).
//!
//! Two objects live here. `ExportSettings` is the *input* a caller (the CLI in
//! `anim_headless`, or the editor in WP10) supplies: a frame range, dimensions, a codec row,
//! a quality profile, and the audio/alpha axes. `ResolvedExport` is the *output* of
//! [`validate`]: an immutable bundle with every delivery parameter the encoder will actually
//! use fixed in place, plus the deterministic `settings_hash`. 18.5 step 1 ("resolve one
//! immutable codec/quality profile") and step 2 ("validate … frame range, dimensions, alpha,
//! audio, output and FFmpeg capabilities") are both this module; FFmpeg's own capability probe
//! is run-time and lives in `ffmpeg.rs`, because 18.9 gives `validate` only a settings and a
//! project to look at.
//!
//! The codec matrix below is the one 18.6 draws. Per the operator's WP09 ruling (DECISIONS
//! #50, MP4-only), only the MP4/H.264 row is *certified*: the pinned `tools/ffmpeg/
//! ffmpeg-manifest.json` declares `libx264`/`aac`/`mp4` and WP00 probed a raw-video→matroska
//! round trip, but it never probed `prores_ks`, the `.mov` container or the alpha pix_fmt, so
//! the MOV and PNG-sequence rows are refused at `validate` with `CapabilityNotCertified`
//! rather than silently attempted against an unprobed capability (spec 9.11).

use std::collections::BTreeMap;
use std::path::Path;

use anim_assets::ContentHash;
use anim_core::hash::CanonicalBytes;
use anim_core::time::{FrameRange, FrameRate};
use anim_project::ProjectDocument;
use serde::Deserialize;

use crate::{ExportError, ValidationCheck};

/// `ExportSettings.schema_version`. Bumped only when the resolved-bytes meaning changes, on the
/// WP04 `EASING_ALGORITHM_VERSION` reasoning: a field addition is data, a changed *reading*
/// must move the `settings_hash`.
pub const EXPORT_SETTINGS_SCHEMA_VERSION: u32 = 1;

/// The domain tag for `settings_hash`. It names the canonical framing of the resolved delivery
/// parameters, so an unrelated re-framing cannot collide with a past export's hash.
pub const SETTINGS_DOMAIN: &str = "anim_engine/export_settings/v1";

/// Upper bound on either output dimension. Beyond this a single RGBA frame's byte length
/// approaches the u32 stride/offset space the readback path assumes, and it is far past any
/// real delivery target; 18.7 lists "excessive dimensions" among the pre-publication failures.
pub const MAX_DIMENSION: u32 = 16_384;

/// Upper bound on `width × height`. `MAX_DIMENSION²` is ~268 megapixels — generous for any
/// fixed-quality offline frame yet small enough that `packed_stride × height` cannot overflow.
pub const MAX_AREA: u64 = 16_384 * 16_384;

/// The three delivery rows of the 18.6 matrix, as the container/format a caller selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecId {
    /// MP4 / H.264 (`libx264`) / `yuv420p` / no alpha / AAC or none.
    Mp4H264,
    /// MOV / ProRes 4444 (`prores_ks`, profile 4444) / `yuva444p10le` / alpha / PCM or none.
    MovProres4444,
    /// PNG sequence / `png` / `rgba` / alpha / separate WAV only.
    PngSequence,
}

/// Whether a codec row accepts an alpha channel, per 18.6's "Alpha" column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlphaSupport {
    /// The row never carries alpha (MP4).
    Never,
    /// The row requires alpha to be meaningful (MOV ProRes 4444, PNG).
    Optional,
}

/// Whether a row's video encoder requires even frame dimensions. H.264's chroma subsampling
/// (`yuv420p`, 4:2:0) cannot represent an odd width or height, and 18.6 states it explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DimensionParity {
    Even,
    Any,
}

/// A single row of the codec matrix. The values are FFmpeg vocabulary spelled the way
/// `ffmpeg.rs` will pass them, so `validate` fixes the contract and the encoder just reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecSpec {
    pub container: &'static str,
    pub file_extension: &'static str,
    pub video_encoder: &'static str,
    /// The bitstream codec name FFprobe reports on the decoded stream (e.g. `h264`), which is
    /// what the verifier compares — distinct from `video_encoder`, the FFmpeg *encoder* name
    /// (`libx264`) that `ffmpeg.rs` passes to `-c:v`.
    pub stream_codec: &'static str,
    pub pixel_format: &'static str,
    pub alpha: AlphaSupport,
    pub parity: DimensionParity,
    /// The only row this build certifies. `false` makes `validate` refuse with
    /// `CapabilityNotCertified` (WP09 MP4-only ruling); the field documents *why* the row is
    /// in the matrix at all rather than pretending it does not exist.
    pub certified: bool,
}

impl CodecId {
    /// The matrix row for this codec.
    #[must_use]
    pub const fn spec(self) -> CodecSpec {
        match self {
            // 18.6: MP4 | libx264 | yuv420p | Never | AAC or none.
            Self::Mp4H264 => CodecSpec {
                container: "mp4",
                file_extension: "mp4",
                video_encoder: "libx264",
                stream_codec: "h264",
                pixel_format: "yuv420p",
                alpha: AlphaSupport::Never,
                parity: DimensionParity::Even,
                certified: true,
            },
            // 18.6: MOV | prores_ks profile 4444 | yuva444p10le | Supported | pcm_s16le or none.
            Self::MovProres4444 => CodecSpec {
                container: "mov",
                file_extension: "mov",
                video_encoder: "prores_ks",
                stream_codec: "prores",
                pixel_format: "yuva444p10le",
                alpha: AlphaSupport::Optional,
                parity: DimensionParity::Any,
                certified: false,
            },
            // 18.6: PNG sequence | png | rgba | Supported | separate WAV only.
            Self::PngSequence => CodecSpec {
                container: "image2",
                file_extension: "png",
                video_encoder: "png",
                stream_codec: "png",
                pixel_format: "rgba",
                alpha: AlphaSupport::Optional,
                parity: DimensionParity::Any,
                certified: false,
            },
        }
    }

    /// The codec's stable discriminant for canonical hashing. Not `as u32`: the numeric spelling
    /// is frozen here so reordering the variants cannot silently move every past `settings_hash`.
    const fn hash_tag(self) -> u32 {
        match self {
            Self::Mp4H264 => 0,
            Self::MovProres4444 => 1,
            Self::PngSequence => 2,
        }
    }

    /// Whether this row's audio column admits `mode` (18.6's "Audio" cell).
    fn audio_allowed(self, mode: AudioMode) -> bool {
        match self {
            // MP4: "AAC or none".
            Self::Mp4H264 => matches!(mode, AudioMode::None | AudioMode::Aac),
            // MOV: "pcm_s16le or none".
            Self::MovProres4444 => matches!(mode, AudioMode::None | AudioMode::Pcm),
            // PNG sequence: "Separate WAV only" — no muxed audio at all.
            Self::PngSequence => mode == AudioMode::None,
        }
    }
}

/// The alpha axis of `ExportSettings`. MP4 rejects `On` (H.264 has no alpha); the alpha rows
/// (`MOV`, `PNG`) would honour it, and both are refused for `certified == false` before this
/// matters. `Composite` is the "explicit opaque background" 18.6 names as the only way to feed
/// alpha-bearing content into H.264 — a background colour flattens alpha onto an opaque plate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportAlphaMode {
    /// No alpha; the renderer's straight/premultiplied output is composited onto black
    /// (the engine's clear colour) and dropped. This is the normal MP4 path.
    Off,
    /// Keep an alpha channel. Illegal for MP4 (18.12: "H.264 alpha … fail before rendering").
    On,
}

impl ExportAlphaMode {
    const fn hash_tag(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::On => 1,
        }
    }
}

/// The audio axis of `ExportSettings` (18.8's `audio: AudioMode`). Which modes a row allows is
/// the codec matrix's decision (`CodecId::audio_allowed`), not this enum's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioMode {
    /// No audio track; the offline WAV is not produced for the container.
    None,
    /// Lossy AAC (`aac`), the MP4 row's audio codec.
    Aac,
    /// Lossless PCM (`pcm_s16le`), the MOV row's audio codec.
    Pcm,
}

impl AudioMode {
    const fn hash_tag(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Aac => 1,
            Self::Pcm => 2,
        }
    }
}

/// Video rate control. 18.6 puts "Quality, CRF/bitrate, profile, color matrix/range and
/// rational frame rate … fixed in resolved settings" — so exactly one of these is set and the
/// choice is part of the `settings_hash`, never inferred at encode time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateControl {
    /// Constant rate factor (libx264 `-crf`), lower is better quality.
    Crf(u32),
    /// Target average bitrate in kbit/s (libx264 `-b:v`).
    BitrateKbps(u32),
}

/// Colour metadata written into the stream (18.6 "color matrix/range"; ADR-0014 defers the exact
/// primaries/transfer/matrix/range selection to these resolved settings). These are FFmpeg
/// `-color_*` option spellings, fixed here so two runs of one profile emit identical metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorMetadata {
    pub primaries: String,
    pub transfer: String,
    pub matrix: String,
    /// `tv` (limited) or `pc` (full) range.
    pub range: String,
}

/// Audio quality knobs for the row's audio codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioQuality {
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
}

/// A resolved, immutable quality profile (18.8's `quality: QualityProfile`): every encoder
/// parameter the delivery row needs, already flattened from whatever named profile the caller
/// picked (the `config/export_profiles.json` loader in `settings`'s companion turns a tier into
/// one of these). This is *input-shaped*: `validate` copies it into `ResolvedExport` unchanged
/// after checking it is coherent with the codec row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualityProfile {
    /// Traceability name carried into the report; never hashed as a semantic field beyond its
    /// string (renaming a profile with identical knobs intentionally moves the hash, because
    /// the name is operator-facing provenance).
    pub name: String,
    pub rate: RateControl,
    pub preset: String,
    pub gop_frames: u32,
    /// Codec profile/level spelled for FFmpeg (`-profile:v`, `-level`).
    pub codec_profile: String,
    pub codec_level: Option<String>,
    pub color: ColorMetadata,
    pub audio: AudioQuality,
}

/// The caller-supplied input to an export (18.8's `ExportSettings`, verbatim fields).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportSettings {
    pub schema_version: u32,
    pub frames: FrameRange,
    pub fps: FrameRate,
    pub width: u32,
    pub height: u32,
    pub codec: CodecId,
    pub quality: QualityProfile,
    pub alpha: ExportAlphaMode,
    pub audio: AudioMode,
    pub destination: std::path::PathBuf,
    pub overwrite: bool,
}

/// The immutable resolved export (18.9's `ResolvedExport`). Everything the pipeline and encoder
/// read comes from here; there are no setters, so "resolved settings are immutable" (18.8) is
/// structural rather than a convention the loop must remember.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedExport {
    pub settings: ExportSettings,
    pub codec_spec: CodecSpec,
    /// `N = end − first`, the frame count the encoder must emit and the verifier must count.
    pub frame_count: u64,
    /// Packed row length the readback strips wgpu's 256-byte padding down to: `width × 4`.
    pub packed_stride: usize,
    /// Blake3 over the canonical resolved delivery parameters (18.8's `settings_hash`).
    pub settings_hash: ContentHash,
}

impl ResolvedExport {
    /// Exact packed byte length of one RGBA8 frame (`width × height × 4`), which is what 18.7's
    /// "pack exactly width*height*4 bytes" writes per frame to FFmpeg.
    #[must_use]
    pub fn packed_bytes_per_frame(&self) -> usize {
        self.packed_stride * self.settings.height as usize
    }
}

/// Validate a settings object against a project document and the codec matrix, resolving it to
/// an immutable [`ResolvedExport`] (spec 18.9).
///
/// This performs 18.5 steps 1–2. Every failure it can raise is a `Validation` that happens
/// *before* any frame is produced or any child process is spawned, which is what makes 18.12's
/// "H.264 alpha and odd dimensions fail before rendering" and 18.11's "an existing destination
/// without overwrite fails before rendering" observable and cheap.
///
/// Note it does not probe FFmpeg (18.9 gives it no tool handles); the capability check that
/// complements the `certified` gate is run-time in `ffmpeg.rs`.
///
/// # Errors
/// [`ExportError::Validation`] for any rejected axis or combination, and
/// [`ExportError::Project`] if the document itself fails its own `validate`.
pub fn validate(
    settings: &ExportSettings,
    project: &ProjectDocument,
) -> Result<ResolvedExport, ExportError> {
    if settings.schema_version != EXPORT_SETTINGS_SCHEMA_VERSION {
        return Err(ExportError::validation(
            ValidationCheck::SchemaVersion,
            format!(
                "export settings schema_version {} is not the version this build resolves ({})",
                settings.schema_version, EXPORT_SETTINGS_SCHEMA_VERSION
            ),
        ));
    }

    // The document must be internally valid before we bind an export to its revision.
    project.validate()?;

    let spec = settings.codec.spec();

    // WP09 MP4-only ruling: a non-certified row is refused rather than attempted against an
    // FFmpeg capability that was never probed (spec 9.11 forbids the silent-unverified path).
    if !spec.certified {
        return Err(ExportError::validation(
            ValidationCheck::CapabilityNotCertified,
            format!(
                "codec row {:?} ({} / {} / {}) is defined in the 18.6 matrix but its FFmpeg \
                 capability is not certified on this pinned build; WP09 certifies MP4/H.264 only",
                settings.codec, spec.container, spec.video_encoder, spec.pixel_format
            ),
        ));
    }

    // 18.6/18.12: H.264 carries no alpha, so an On alpha axis is refused here, before rendering.
    if matches!(spec.alpha, AlphaSupport::Never) && settings.alpha == ExportAlphaMode::On {
        return Err(ExportError::validation(
            ValidationCheck::AlphaNotSupported,
            format!("{:?} rejects an alpha channel", settings.codec),
        ));
    }

    // 18.6 audio column: the row decides which audio modes exist. (MP4: AAC or none.)
    if !settings.codec.audio_allowed(settings.audio) {
        return Err(ExportError::validation(
            ValidationCheck::AudioNotSupported,
            format!(
                "codec row {:?} does not allow audio mode {:?}",
                settings.codec, settings.audio
            ),
        ));
    }

    // Dimensions: nonzero, bounded, and (H.264) even. Checked before the stride multiply so a
    // zero or absurd size fails as a size error, not an overflow one.
    if settings.width == 0 || settings.height == 0 {
        return Err(ExportError::validation(
            ValidationCheck::ZeroDimension,
            format!(
                "output extent {}×{} has a zero side",
                settings.width, settings.height
            ),
        ));
    }
    if settings.width > MAX_DIMENSION || settings.height > MAX_DIMENSION {
        return Err(ExportError::validation(
            ValidationCheck::ExcessiveDimensions,
            format!(
                "output extent {}×{} exceeds MAX_DIMENSION {}",
                settings.width, settings.height, MAX_DIMENSION
            ),
        ));
    }
    let area = u64::from(settings.width) * u64::from(settings.height);
    if area > MAX_AREA {
        return Err(ExportError::validation(
            ValidationCheck::ExcessiveDimensions,
            format!("output area {area} px exceeds MAX_AREA {MAX_AREA}"),
        ));
    }
    if matches!(spec.parity, DimensionParity::Even)
        && (!settings.width.is_multiple_of(2) || !settings.height.is_multiple_of(2))
    {
        return Err(ExportError::validation(
            ValidationCheck::OddDimension,
            format!(
                "{:?} ({}) requires even dimensions, got {}×{}",
                settings.codec, spec.pixel_format, settings.width, settings.height
            ),
        ));
    }

    // Stride/packed-size arithmetic must fit the usize the readback ring and the FFmpeg stdin
    // write both use. width*4 can't overflow u32 for width ≤ MAX_DIMENSION, but the full frame
    // length is the quantity the ring slots are sized by, so validate it in usize arithmetic.
    let packed_stride = usize::try_from(settings.width)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or_else(|| {
            ExportError::validation(
                ValidationCheck::StrideOverflow,
                format!("width {} overflows the packed stride", settings.width),
            )
        })?;
    packed_stride
        .checked_mul(usize::try_from(settings.height).unwrap_or(usize::MAX))
        .ok_or_else(|| {
            ExportError::validation(
                ValidationCheck::StrideOverflow,
                format!(
                    "packed frame {}×{} overflows usize",
                    settings.width, settings.height
                ),
            )
        })?;

    // 18.6 fixes "rational frame rate" in resolved settings, and the engine binds one timeline
    // to one rate (WP05 / DECISIONS #35): an export that stamps a different rate than the
    // project plays at would desync the muxed audio count from `sample_range`. So the fps must
    // equal the project's frame rate exactly.
    if settings.fps.num != project.frame_rate.num || settings.fps.den != project.frame_rate.den {
        return Err(ExportError::validation(
            ValidationCheck::FrameRateMismatch,
            format!(
                "export fps {}/{} does not match the project frame rate {}/{}; the offline audio \
                 sample count is derived at the project rate and would not survive a re-stamp",
                settings.fps.num, settings.fps.den, project.frame_rate.num, project.frame_rate.den
            ),
        ));
    }

    // 18.7: zero frames fails before publication. FrameRange already refuses reversed at
    // construction; an empty range is legal as data but not as an export.
    let frame_count = settings.frames.len()?;
    if settings.frames.is_empty() || frame_count == 0 {
        return Err(ExportError::validation(
            ValidationCheck::EmptyRange,
            format!("export range {:?} carries zero frames", settings.frames),
        ));
    }

    let settings_hash = hash_resolved(settings, spec, frame_count, packed_stride)?;

    Ok(ResolvedExport {
        settings: settings.clone(),
        codec_spec: spec,
        frame_count,
        packed_stride,
        settings_hash,
    })
}

/// Blake3 over the canonical resolved delivery parameters. Length-prefixed fields in a fixed
/// order (spec 11.7 / ADR-0004 canonical-bytes rule), domain tag first.
fn hash_resolved(
    settings: &ExportSettings,
    spec: CodecSpec,
    frame_count: u64,
    packed_stride: usize,
) -> Result<ContentHash, ExportError> {
    let mut cp = CanonicalBytes::with_domain(SETTINGS_DOMAIN)?;
    cp.u32(EXPORT_SETTINGS_SCHEMA_VERSION);
    cp.i64(settings.frames.start);
    cp.i64(settings.frames.end);
    cp.u32(frame_count.try_into().unwrap_or(u32::MAX));
    cp.u32(settings.fps.num);
    cp.u32(settings.fps.den);
    cp.u32(settings.width);
    cp.u32(settings.height);
    cp.u32(packed_stride.try_into().unwrap_or(u32::MAX));
    cp.u32(settings.codec.hash_tag());
    cp.text(spec.container);
    cp.text(spec.video_encoder);
    cp.text(spec.pixel_format);
    cp.u32(settings.alpha.hash_tag());
    cp.u32(settings.audio.hash_tag());

    // The resolved profile's concrete knobs — 18.6 "Quality, CRF/bitrate, profile, color
    // matrix/range … fixed in resolved settings."
    let q = &settings.quality;
    cp.text(&q.name);
    match q.rate {
        RateControl::Crf(v) => {
            cp.u32(0);
            cp.u32(v);
        }
        RateControl::BitrateKbps(v) => {
            cp.u32(1);
            cp.u32(v);
        }
    }
    cp.text(&q.preset);
    cp.u32(q.gop_frames);
    cp.text(&q.codec_profile);
    match &q.codec_level {
        None => cp.bool(false),
        Some(level) => {
            cp.bool(true);
            cp.text(level);
        }
    }
    cp.text(&q.color.primaries);
    cp.text(&q.color.transfer);
    cp.text(&q.color.matrix);
    cp.text(&q.color.range);
    cp.u32(q.audio.bitrate_kbps);
    cp.u32(q.audio.sample_rate);

    Ok(ContentHash::from_bytes(cp.digest()))
}

// ---------------------------------------------------------------------------------------------
// The profile file (18.3: `config/export_profiles.json`, "versioned immutable codec/quality
// profiles"). A named profile is the concrete `QualityProfile` plus the codec row it belongs to;
// the caller selects one by name and the CLI/`run` combines it with the frame range, dimensions,
// alpha and audio axes to form `ExportSettings`, which `validate` then resolves. Loading is
// deliberately a pure `parse` over text (tested without disk) wrapped by a one-line `load`.
// ---------------------------------------------------------------------------------------------

/// Schema version of `config/export_profiles.json`.
pub const PROFILE_FILE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
struct ProfileFile {
    schema_version: u32,
    profiles: Vec<ProfileDef>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProfileDef {
    name: String,
    codec: String,
    quality: QualityDef,
}

#[derive(Debug, Clone, Deserialize)]
struct QualityDef {
    /// `"crf"` or `"bitrate"`; selects which of the two optional values below is required.
    rate: String,
    #[serde(default)]
    crf: Option<u32>,
    #[serde(default)]
    bitrate_kbps: Option<u32>,
    preset: String,
    gop_frames: u32,
    #[serde(default)]
    codec_profile: String,
    #[serde(default)]
    codec_level: Option<String>,
    color: ColorDef,
    audio: AudioDef,
}

#[derive(Debug, Clone, Deserialize)]
struct ColorDef {
    primaries: String,
    transfer: String,
    matrix: String,
    range: String,
}

#[derive(Debug, Clone, Deserialize)]
struct AudioDef {
    bitrate_kbps: u32,
    sample_rate: u32,
}

/// One resolved named profile: the codec row and its fixed quality knobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub codec: CodecId,
    pub quality: QualityProfile,
}

/// An immutable, loaded profile table keyed by name (`BTreeMap`, never `HashMap` — the workspace
/// forbids it and a stable iteration order keeps any future hashing canonical).
#[derive(Debug, Clone)]
pub struct ExportProfiles {
    by_name: BTreeMap<String, Profile>,
}

impl ExportProfiles {
    /// Read and parse the profile file at `path`.
    pub fn load(path: &Path) -> Result<Self, ExportError> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            ExportError::validation(
                ValidationCheck::SchemaVersion,
                format!("cannot read profile file {}: {error}", path.display()),
            )
        })?;
        Self::parse(&text)
    }

    /// Parse and validate the profile JSON. Every codec name and rate-control shape is checked
    /// here so a typo is a load-time error, not a surprise at `validate`.
    pub fn parse(text: &str) -> Result<Self, ExportError> {
        let file: ProfileFile = serde_json::from_str(text).map_err(|error| {
            ExportError::validation(
                ValidationCheck::SchemaVersion,
                format!("malformed export profile JSON: {error}"),
            )
        })?;
        if file.schema_version != PROFILE_FILE_SCHEMA_VERSION {
            return Err(ExportError::validation(
                ValidationCheck::SchemaVersion,
                format!(
                    "export profile schema_version {} is not {PROFILE_FILE_SCHEMA_VERSION}",
                    file.schema_version
                ),
            ));
        }
        let mut by_name = BTreeMap::new();
        for def in file.profiles {
            let name = def.name.clone();
            if by_name.contains_key(&name) {
                return Err(ExportError::validation(
                    ValidationCheck::SchemaVersion,
                    format!("duplicate profile name {name:?}"),
                ));
            }
            let codec = parse_codec(&def.codec)?;
            let quality = to_quality(codec, def)?;
            by_name.insert(
                name.clone(),
                Profile {
                    name,
                    codec,
                    quality,
                },
            );
        }
        Ok(Self { by_name })
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.by_name.get(name)
    }

    /// Look a profile up by name, or fail naming the known profiles.
    pub fn resolve(&self, name: &str) -> Result<Profile, ExportError> {
        self.by_name.get(name).cloned().ok_or_else(|| {
            ExportError::validation(
                ValidationCheck::SchemaVersion,
                format!(
                    "unknown export profile {name:?}; known: {}",
                    self.names().collect::<Vec<_>>().join(", ")
                ),
            )
        })
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }
}

fn parse_codec(s: &str) -> Result<CodecId, ExportError> {
    match s {
        "mp4_h264" => Ok(CodecId::Mp4H264),
        "mov_prores4444" => Ok(CodecId::MovProres4444),
        "png_sequence" => Ok(CodecId::PngSequence),
        other => Err(ExportError::validation(
            ValidationCheck::SchemaVersion,
            format!("unknown codec name {other:?} in export profile"),
        )),
    }
}

fn to_quality(_codec: CodecId, def: ProfileDef) -> Result<QualityProfile, ExportError> {
    let q = def.quality;
    let rate = match q.rate.as_str() {
        "crf" => RateControl::Crf(q.crf.ok_or_else(|| {
            ExportError::validation(
                ValidationCheck::SchemaVersion,
                format!("profile {:?} uses rate 'crf' but omits crf", def.name),
            )
        })?),
        "bitrate" => RateControl::BitrateKbps(q.bitrate_kbps.ok_or_else(|| {
            ExportError::validation(
                ValidationCheck::SchemaVersion,
                format!(
                    "profile {:?} uses rate 'bitrate' but omits bitrate_kbps",
                    def.name
                ),
            )
        })?),
        other => {
            return Err(ExportError::validation(
                ValidationCheck::SchemaVersion,
                format!("profile {:?} has unknown rate control {other:?}", def.name),
            ));
        }
    };
    Ok(QualityProfile {
        name: def.name,
        rate,
        preset: q.preset,
        gop_frames: q.gop_frames,
        codec_profile: q.codec_profile,
        codec_level: q.codec_level,
        color: ColorMetadata {
            primaries: q.color.primaries,
            transfer: q.color.transfer,
            matrix: q.color.matrix,
            range: q.color.range,
        },
        audio: AudioQuality {
            bitrate_kbps: q.audio.bitrate_kbps,
            sample_rate: q.audio.sample_rate,
        },
    })
}

#[cfg(test)]
mod profiles_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only"
    )]

    use super::*;

    const SAMPLE: &str = r#"{
      "schema_version": 1,
      "profiles": [
        {"name":"mp4-h264-standard","codec":"mp4_h264","quality":{
            "rate":"crf","crf":18,"preset":"medium","gop_frames":48,
            "codec_profile":"high","codec_level":"4.1",
            "color":{"primaries":"bt709","transfer":"bt709","matrix":"bt709","range":"tv"},
            "audio":{"bitrate_kbps":192,"sample_rate":48000}}},
        {"name":"mov-prores4444","codec":"mov_prores4444","quality":{
            "rate":"bitrate","bitrate_kbps":0,"preset":"0","gop_frames":1,
            "codec_profile":"4444",
            "color":{"primaries":"bt709","transfer":"bt709","matrix":"bt709","range":"pc"},
            "audio":{"bitrate_kbps":0,"sample_rate":48000}}}
      ]
    }"#;

    #[test]
    fn profiles_parse_and_resolve_the_two_rate_shapes() {
        let profiles = ExportProfiles::parse(SAMPLE).unwrap();
        let standard = profiles.resolve("mp4-h264-standard").unwrap();
        assert_eq!(standard.codec, CodecId::Mp4H264);
        assert_eq!(standard.quality.rate, RateControl::Crf(18));
        assert_eq!(standard.quality.audio.sample_rate, 48_000);

        let prores = profiles.resolve("mov-prores4444").unwrap();
        assert_eq!(prores.codec, CodecId::MovProres4444);
        assert_eq!(prores.quality.rate, RateControl::BitrateKbps(0));
    }

    #[test]
    fn an_unknown_codec_or_rate_or_missing_value_is_a_load_time_error() {
        // Each of these was a real foot-gun class found in WP02/WP04: a typo that would otherwise
        // surface much later as a wrong output. Assert the loader refuses them by name.
        let bad_codec = SAMPLE.replace("\"mp4_h264\"", "\"mp4_h265\"");
        assert!(ExportProfiles::parse(&bad_codec).is_err());

        let crf_missing = SAMPLE.replace("\"crf\":18,", "");
        assert!(ExportProfiles::parse(&crf_missing).is_err());

        let bad_rate = SAMPLE.replace("\"rate\":\"crf\"", "\"rate\":\"constant\"");
        assert!(ExportProfiles::parse(&bad_rate).is_err());
    }

    #[test]
    fn a_wrong_schema_version_is_refused() {
        let old = SAMPLE.replace("\"schema_version\": 1", "\"schema_version\": 2");
        assert!(ExportProfiles::parse(&old).is_err());
    }

    #[test]
    fn resolving_an_unknown_profile_names_the_known_ones() {
        let profiles = ExportProfiles::parse(SAMPLE).unwrap();
        let err = profiles.resolve("nope").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mp4-h264-standard") && msg.contains("mov-prores4444"),
            "{msg}"
        );
    }

    #[test]
    fn the_shipped_profile_file_parses_and_has_the_certified_mp4_rows() {
        // The real `config/export_profiles.json` is a versioned artifact the gate must not be able
        // to pass while broken (precedent: WP03/WP05 schema fixtures read through the crate root).
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/export_profiles.json");
        let profiles = ExportProfiles::load(&path)
            .unwrap_or_else(|error| panic!("{} failed to parse: {error}", path.display()));
        for name in ["mp4-h264-draft", "mp4-h264-standard", "mp4-h264-master"] {
            let profile = profiles.resolve(name).expect(name);
            assert_eq!(
                profile.codec,
                CodecId::Mp4H264,
                "{name} must be the certified row"
            );
        }
        // The refused rows load (they are valid profiles) but are not certified — validate() is
        // what refuses them; the loader only has to parse them without error.
        assert_eq!(
            profiles.resolve("mov-prores4444").unwrap().codec,
            CodecId::MovProres4444
        );
        assert_eq!(
            profiles.resolve("png-sequence").unwrap().codec,
            CodecId::PngSequence
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only"
    )]

    use super::*;

    // A minimal settings object exercising only what settings.rs can decide without a full
    // project (the project-backed tests live in tests/export.rs, which builds a real v5
    // document). Here we drive the pure matrix/geometry logic through the fields validate reads.
    fn base_settings() -> ExportSettings {
        ExportSettings {
            schema_version: EXPORT_SETTINGS_SCHEMA_VERSION,
            frames: FrameRange::new(0, 12).unwrap(),
            fps: FrameRate { num: 24, den: 1 },
            width: 320,
            height: 240,
            codec: CodecId::Mp4H264,
            quality: QualityProfile {
                name: "mp4-test".to_owned(),
                rate: RateControl::Crf(20),
                preset: "slow".to_owned(),
                gop_frames: 24,
                codec_profile: "high".to_owned(),
                codec_level: Some("4.1".to_owned()),
                color: ColorMetadata {
                    primaries: "bt709".to_owned(),
                    transfer: "bt709".to_owned(),
                    matrix: "bt709".to_owned(),
                    range: "tv".to_owned(),
                },
                audio: AudioQuality {
                    bitrate_kbps: 192,
                    sample_rate: 48_000,
                },
            },
            alpha: ExportAlphaMode::Off,
            audio: AudioMode::Aac,
            destination: std::path::PathBuf::from("out/clip.mp4"),
            overwrite: false,
        }
    }

    #[test]
    fn only_mp4_is_certified_and_the_other_rows_are_named_as_not_certified() {
        assert!(CodecId::Mp4H264.spec().certified);
        assert!(!CodecId::MovProres4444.spec().certified);
        assert!(!CodecId::PngSequence.spec().certified);
    }

    #[test]
    fn the_audio_column_matches_the_matrix() {
        // MP4 admits AAC and none, never PCM.
        assert!(CodecId::Mp4H264.audio_allowed(AudioMode::Aac));
        assert!(CodecId::Mp4H264.audio_allowed(AudioMode::None));
        assert!(!CodecId::Mp4H264.audio_allowed(AudioMode::Pcm));
        // MOV admits PCM, not AAC.
        assert!(CodecId::MovProres4444.audio_allowed(AudioMode::Pcm));
        assert!(!CodecId::MovProres4444.audio_allowed(AudioMode::Aac));
        // PNG sequence carries no muxed audio.
        assert!(CodecId::PngSequence.audio_allowed(AudioMode::None));
        assert!(!CodecId::PngSequence.audio_allowed(AudioMode::Aac));
    }

    #[test]
    fn h264_is_the_even_dimension_row() {
        assert_eq!(CodecId::Mp4H264.spec().parity, DimensionParity::Even);
        assert_eq!(CodecId::MovProres4444.spec().parity, DimensionParity::Any);
    }

    #[test]
    fn settings_hash_is_stable_and_moves_when_a_delivery_parameter_moves() {
        let a = base_settings();
        let mut changed_crf = a.clone();
        changed_crf.quality.rate = RateControl::Crf(23);
        let mut changed_audio = a.clone();
        changed_audio.audio = AudioMode::None;

        let spec = a.codec.spec();
        let n = a.frames.len().unwrap();
        let stride = (a.width as usize) * 4;

        let h0 = hash_resolved(&a, spec, n, stride).unwrap();
        let h0_again = hash_resolved(&a, spec, n, stride).unwrap();
        assert_eq!(
            h0, h0_again,
            "same resolved parameters must hash identically"
        );

        let h_crf = hash_resolved(&changed_crf, spec, n, stride).unwrap();
        let h_audio = hash_resolved(&changed_audio, spec, n, stride).unwrap();
        assert_ne!(h0, h_crf, "a CRF change is a different delivery");
        assert_ne!(h0, h_audio, "dropping audio is a different delivery");
    }
}
