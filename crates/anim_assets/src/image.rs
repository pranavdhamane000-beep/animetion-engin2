//! Image metadata, color and alpha decode (spec 11.3, 11.6).
//!
//! Two readers live here. The metadata pass reads the header, not the pixels: the facts a
//! scene needs before it can reference a cel are its dimensions, its bit depth, whether it
//! carries alpha and whether it is interlaced — all of which live in the first few hundred
//! bytes, and none of which require a rasterizer. The pixel pass ([`ImageDecoder::decode_pixels`])
//! is the consumer DECISIONS.md #26 deferred until WP06 and #41 ruling 2 placed here: it
//! decodes PNG via the pinned `png` crate and JPEG via the pinned `zune-jpeg` crate, applies
//! the transfer function the *record* claims, and premultiplies in linear space when the
//! [`crate::decode::DecoderConfiguration`] asks for it. The result is a derived, cacheable
//! value (spec 11.10), never authoritative — the manifest record stays the one source of
//! colour truth.
//!
//! Chunk CRCs are *not* verified by the metadata pass. This engine's integrity model is the
//! content digest (spec 11.6), which proves the stored object is byte-for-byte what was
//! imported; PNG's per-chunk CRC would catch corruption introduced by the authoring tool
//! before import, which is a different failure with a different owner, and paying a second
//! full pass over every image to cover it is the wrong trade. (The `png` crate's pixel
//! decoder verifies critical-chunk CRCs itself; that is the library's own integrity check on
//! the bytes it inflates, not a second metadata reading.)
//!
//! Colour and alpha are classified here, from the same single header pass, and the
//! result is what [`crate::import`] records in the manifest. Spec 15.2 makes "image
//! assets carry color-space and alpha metadata" a prerequisite of the renderer, and the
//! ruling recorded with the schema bump is that the metadata is authored data fixed at
//! import rather than re-read from the file at render time: a second reading of one fact
//! beside the manifest's gives one project two render outputs, which is the failure
//! class this engine exists to prevent. That is why the rules below end in a *decision*
//! written into hashed bytes, and why the fallback for a profile this engine does not
//! model is a report rather than a guess.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::Path;

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
use serde::{Deserialize, Serialize};

use crate::decode::{AssetMetadata, Decoder, DecoderConfiguration, CURRENT_IMPORTER_VERSION};
use crate::manifest::{AssetKind, AssetManifestEntry};
use crate::AssetError;

/// The largest chunk or segment this crate will read or skip while looking for a
/// header, and the bound that makes the scan safe against a hostile file.
const MAX_HEADER_SCAN: u64 = 1024 * 1024;

/// The two image formats with a metadata reader in this build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
        }
    }

    /// The signature bytes that identify this format.
    #[must_use]
    pub const fn signature(self) -> &'static [u8] {
        match self {
            Self::Png => &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
            Self::Jpeg => &[0xFF, 0xD8],
        }
    }

    /// The format whose signature `probe` starts with, if any.
    #[must_use]
    pub fn sniff(probe: &[u8]) -> Option<Self> {
        [Self::Png, Self::Jpeg]
            .into_iter()
            .find(|format| probe.starts_with(format.signature()))
    }
}

impl std::fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the stored pixels mean, in terms a texture path can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PixelLayout {
    Grayscale,
    GrayscaleAlpha,
    Rgb,
    Rgba,
    /// PNG colour type 3: indices into a palette, with alpha only if `tRNS` says so.
    Palette,
    /// A layout no rule here names. Reported rather than guessed, because a wrong
    /// guess becomes a wrong colour, which is invisible in a golden and loud in a
    /// finished export.
    Unknown,
}

/// An embedded colour profile this engine does not model, as the header declared it.
///
/// Two shapes rather than one `Option<String>` because the formats differ in what they
/// state: PNG's `iCCP` carries the author's profile name as text, and JPEG's `APP2`
/// carries only the profile bytes inside a segment whose identifier is fixed. Recording a
/// name for the second case would be inventing one, and the whole point of this type is
/// that an un-modelled profile is *reported* — so the report says exactly what the file
/// says and no more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbeddedProfile {
    /// `iCCP`: the chunk's own profile-name field.
    Named(String),
    /// `c2PC`, or a JPEG `APP2` segment with the `ICC_PROFILE` identifier: a profile
    /// whose framing states no name.
    Unnamed,
}

impl EmbeddedProfile {
    /// How the diagnostic refers to this profile.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Named(name) => name.clone(),
            Self::Unnamed => "an unnamed embedded profile".to_owned(),
        }
    }
}

/// The colour-intent markers a header carried, kept as facts rather than as a conclusion.
///
/// The classifier below is the only place that turns these into an
/// [`ImageColorSpace`], so the parser stays free of policy and a rule change never
/// requires re-reading the file format.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageColorMarkers {
    /// PNG `sRGB`: the author declared the transfer function and the primaries.
    pub srgb: bool,
    /// PNG `gAMA`, in the format's 1/100 000 units, so 100 000 is gamma 1.0.
    pub gamma: Option<u32>,
    /// An embedded ICC profile, which this engine models and therefore refuses to name
    /// a colour space for.
    pub icc_profile: Option<EmbeddedProfile>,
}

/// The transfer function and primaries a stored image's pixels are in.
///
/// The lowercase spelling is the persisted and the canonical form, exactly as
/// [`AssetKind`] uses its own, so the string in a saved manifest and the string in a
/// hashed record are one value with no table between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageColorSpace {
    Srgb,
    Linear,
    /// A profile this engine does not model, or a claim it declined to guess.
    ///
    /// Produced by exactly two rules: an embedded profile at import, and the version 3 to
    /// version 4 migration of a file that recorded nothing. It is never a rendering-time
    /// approximation, and a renderer reading it must report rather than assume — spec
    /// 15.11's prohibition on a silent transfer-function fallback.
    Unclassified,
}

impl ImageColorSpace {
    /// The canonical spelling, identical to the JSON form.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Srgb => "srgb",
            Self::Linear => "linear",
            Self::Unclassified => "unclassified",
        }
    }
}

impl std::fmt::Display for ImageColorSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How an image's alpha channel combines with its colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageAlpha {
    /// No alpha channel, so every pixel is fully opaque — the whole of what a JPEG can be.
    Opaque,
    /// Alpha as authored, with the colour *not* multiplied by it.
    ///
    /// PNG states this directly: its alpha channel is independent of the colour channels,
    /// so a cel stored as `RGBA` is straight by definition of the format and not by
    /// inference from its contents. Premultiplication is a decode choice for the renderer
    /// to make against a [`crate::decode::DecoderConfiguration`], which is why it is a
    /// cache-key axis and not a metadata claim.
    Straight,
    /// Alpha mode not recorded, which only the version 3 to version 4 migration writes.
    ///
    /// Nothing that classifies a real file can produce this: PNG and JPEG both state
    /// whether an alpha channel exists, so an unknown alpha would be a parser that gave
    /// up rather than a header that did not say.
    Unclassified,
}

impl ImageAlpha {
    /// The canonical spelling, identical to the JSON form.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Opaque => "opaque",
            Self::Straight => "straight",
            Self::Unclassified => "unclassified",
        }
    }
}

impl std::fmt::Display for ImageAlpha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the manifest records about one image asset's pixels.
///
/// Two enums, no invariants to normalize, so the field is `pub` in
/// [`crate::manifest::AssetManifestEntry`] the way `kind` is and not private the way
/// `dependencies` is (DECISIONS.md #22's test: one value, one spelling).
///
/// There is no `as_str()` here because the pair has no single canonical spelling: the two
/// halves are encoded, persisted and hashed separately, in that order, which is what
/// keeps one claim change from disturbing the other's byte position. `Display` is the
/// readable form for a report and reaches nothing on a disk or in a digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAssetClaims {
    pub color_space: ImageColorSpace,
    pub alpha: ImageAlpha,
}

impl std::fmt::Display for ImageAssetClaims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.color_space, self.alpha)
    }
}

/// The format-level facts of one stored image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageMetadata {
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
    /// Bits per channel: 1/2/4/8/16 for PNG, 8 or 12 for JPEG.
    pub bit_depth: u8,
    pub layout: PixelLayout,
    pub has_alpha: bool,
    /// PNG Adam7 or JPEG progressive. Interlaced sources need a different decode
    /// strategy, so WP06 must know before it commits to a pass.
    pub interlaced: bool,
    /// The colour-intent chunks the header scan found on the way to the first `IDAT`.
    pub color_markers: ImageColorMarkers,
}

/// The diagnostic code an un-modelled profile is reported under (spec 7.4's stable
/// category, append-only like every other code in this crate).
pub const UNMODELLED_PROFILE_CODE: &str = "asset.unmodelled_colour_profile";

/// Classify one parsed header into the claims a manifest record carries.
///
/// Returns the claims and, when the colour space is `Unclassified` *because a profile was
/// found*, the finding that names it. The two are produced by one rule body rather than
/// two functions so that a record can never claim `Unclassified` with no report beside it,
/// nor report a profile it did not record.
///
/// `asset_id` is the record's own id, which `image_metadata` cannot know because it is
/// also called on a bare path; the caller that is building a record passes it here so the
/// diagnostic answers "which asset" as a field rather than as a substring (spec 7.4).
#[must_use]
pub fn classify_image(
    metadata: &ImageMetadata,
    asset_id: &str,
) -> (ImageAssetClaims, Option<Diagnostic>) {
    let markers = &metadata.color_markers;

    // An embedded profile wins over every other declaration, including a co-existing
    // `sRGB` chunk, and that ordering is the reason this is a rule and not a lookup.
    // Tools routinely write both, and the two can disagree; choosing one would be
    // guessing which the author meant, which is what [`PixelLayout::Unknown`]'s comment
    // refuses on the layout axis and this refuses on the colour axis.
    let (color_space, finding) = if let Some(profile) = &markers.icc_profile {
        let diagnostic = Diagnostic::new(
            DiagnosticCode::new(UNMODELLED_PROFILE_CODE)
                .unwrap_or_else(DiagnosticCode::unspecified),
            DiagnosticSeverity::Warning,
            format!(
                "asset {asset_id} carries {description}, which this engine does not model; \
                 its colour space is recorded as unclassified rather than approximated",
                description = profile.describe()
            ),
        )
        .with_context("asset_id", asset_id.to_owned())
        .with_context("profile", profile.describe())
        .with_context("format", metadata.format.to_string());
        (ImageColorSpace::Unclassified, Some(diagnostic))
    } else if markers.srgb {
        (ImageColorSpace::Srgb, None)
    } else if markers.gamma == Some(GAMMA_LINEAR) {
        // `gAMA` 100000 is the file saying "these values are the scene-linear ones",
        // which is a declaration of the transfer function and the only one `gAMA` alone
        // can support.
        (ImageColorSpace::Linear, None)
    } else {
        // The default rule, and it is a *decision written into hashed data at import*,
        // not an inference a renderer makes later. An unmarked cel in an animation
        // pipeline is an sRGB asset; that has been the format's de facto convention
        // since PNG and JPEG shipped no marker for the case, and choosing anything else
        // would be a guess in the other direction.
        //
        // Why the distinction matters: spec 15.11 forbids a renderer silently
        // substituting a transfer function, and a render-time guess is exactly that —
        // the same file would render differently under a changed rule, invisibly. Because
        // the answer is recorded in the record, a later package that revises this default
        // has to bump `MANIFEST_SCHEMA_VERSION` and re-import, so the change shows up in
        // every digest it affects. A `gAMA` that is neither 100000 nor absent lands here
        // too: gamma without primaries names a curve and not a colour space, so it is not
        // a claim this type can carry.
        (ImageColorSpace::Srgb, None)
    };

    // Nothing in this function emits `ImageAlpha::Unclassified`: both formats state
    // whether an alpha channel exists, so the variant's only producer is the migration.
    let alpha = if metadata.has_alpha {
        ImageAlpha::Straight
    } else {
        ImageAlpha::Opaque
    };

    (ImageAssetClaims { color_space, alpha }, finding)
}

/// `gAMA`'s value for a gamma of exactly 1.0, in the format's 1/100 000 units.
const GAMMA_LINEAR: u32 = 100_000;

/// Read one stored image's metadata, sniffing the format from its signature.
///
/// The signature is *peeked* by chaining the probe bytes back onto the stream, so
/// each format parser sees a file starting at byte zero and the rules in
/// [`png_metadata`] stay the rules the formats actually state.
pub fn image_metadata(path: &Path) -> Result<ImageMetadata, AssetError> {
    let file = File::open(path).map_err(|e| crate::io_error("open image", path, e.to_string()))?;
    let mut reader = BufReader::new(file);
    let mut probe = [0u8; 8];
    let probed = match reader.read(&mut probe) {
        Ok(read) => read,
        Err(e) => {
            return Err(crate::io_error("read image header", path, e.to_string()));
        }
    };
    let probe = &probe[..probed];
    let Some(format) = ImageFormat::sniff(probe) else {
        // A signature nothing here reads. Not "malformed" — the file may be a
        // perfectly good WebP — but there is no reader, and spec 11.11 names that
        // case.
        return Err(AssetError::UnsupportedDecoder {
            kind: AssetKind::Image.to_string(),
            importer_version: CURRENT_IMPORTER_VERSION,
        });
    };

    let malformed = |detail: String| AssetError::MalformedAsset {
        asset_id: String::new(),
        kind: AssetKind::Image.to_string(),
        detail: format!("{format} at {}: {detail}", path.display()),
    };
    // The probe bytes are chained back in front of the stream, so the format parser
    // below sees the file from byte zero.
    let mut source = probe.chain(&mut reader);
    match format {
        ImageFormat::Png => png_metadata(&mut source).map_err(malformed),
        ImageFormat::Jpeg => jpeg_metadata(&mut source).map_err(malformed),
    }
}

/// Parse a PNG stream from its signature: `IHDR`, then chunks up to the first `IDAT`.
///
/// `Err` carries the reason as a string so the caller can attach the path and the
/// asset id, which it knows and this function does not.
pub fn png_metadata<R: Read>(source: &mut R) -> Result<ImageMetadata, String> {
    let mut signature = [0u8; 8];
    read_exact_bounded(source, &mut signature)?;
    if signature != ImageFormat::Png.signature() {
        return Err("signature is not PNG".to_owned());
    }

    let (length, chunk_type) = chunk_header(source)?;
    if chunk_type != *b"IHDR" {
        return Err(format!("first chunk is {}", describe(&chunk_type)));
    }
    // IHDR: width, height, bit depth, color type, compression, filter, interlace.
    let header = read_chunk_payload(source, length, 13)?;
    // IHDR's own byte offsets: 10 is the compression method and 11 the filter
    // method, both of which the format fixes at zero.
    if header[10] != 0 || header[11] != 0 {
        return Err("unknown PNG compression or filter method".to_owned());
    }
    let width = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let height = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let bit_depth = header[8];
    let color_type = header[9];
    let interlaced = header[12] == 1;
    if width == 0 || height == 0 {
        return Err("zero-sized IHDR".to_owned());
    }
    let layout = match color_type {
        0 => PixelLayout::Grayscale,
        2 => PixelLayout::Rgb,
        3 => PixelLayout::Palette,
        4 => PixelLayout::GrayscaleAlpha,
        6 => PixelLayout::Rgba,
        other => return Err(format!("unknown PNG colour type {other}")),
    };
    let mut has_alpha = matches!(color_type, 4 | 6);
    let mut color_markers = ImageColorMarkers::default();

    // A palette or gray image with `tRNS` is semi-transparent. The chunk appears
    // before the first `IDAT`, so scanning until then is enough, and the scan is
    // bounded by chunk size rather than by file size. The colour chunks sit in the same
    // region — PNG puts `sRGB`, `gAMA`, `iCCP` and `c2PC` before the image data, before
    // any chunk that IDAT-compressed pixels could be confused with — so this one pass
    // reads layout, alpha and colour intent together and there is no second header
    // parser to keep in step with this one.
    while let Ok((length, chunk_type)) = chunk_header(source) {
        match chunk_type.as_slice() {
            b"IDAT" | b"IEND" => break,
            b"tRNS" => {
                if !read_chunk_payload(source, length, 0)?.is_empty() {
                    has_alpha = true;
                }
            }
            b"sRGB" => {
                // One byte: the rendering intent. The payload's exact size is part of
                // the format, so a chunk that claims another length is a corrupt file
                // rather than a marker to be interpreted loosely.
                read_chunk_payload(source, length, 1)?;
                color_markers.srgb = true;
            }
            b"gAMA" => {
                let payload = read_chunk_payload(source, length, 4)?;
                color_markers.gamma = Some(u32::from_be_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]));
            }
            b"iCCP" => {
                let payload = read_chunk_payload(source, length, 0)?;
                color_markers.icc_profile =
                    Some(EmbeddedProfile::Named(icc_profile_name(&payload)?));
            }
            b"c2PC" => {
                read_chunk_payload(source, length, 0)?;
                color_markers.icc_profile = Some(EmbeddedProfile::Unnamed);
            }
            _ => skip_remaining(source, length)?,
        }
    }

    Ok(ImageMetadata {
        format: ImageFormat::Png,
        width,
        height,
        bit_depth,
        layout,
        has_alpha,
        interlaced,
        color_markers,
    })
}

/// Parse a JPEG stream from its `SOI`, then marker segments until a start-of-frame.
pub fn jpeg_metadata<R: Read>(source: &mut R) -> Result<ImageMetadata, String> {
    let mut soi = [0u8; 2];
    read_exact_bounded(source, &mut soi)?;
    if soi != ImageFormat::Jpeg.signature() {
        return Err("signature is not JPEG".to_owned());
    }

    let mut color_markers = ImageColorMarkers::default();
    let mut scanned: u64 = 2;
    loop {
        let code = loop {
            let mut byte = [0u8; 1];
            read_exact_bounded(source, &mut byte)?;
            scanned = scanned.saturating_add(1);
            if byte[0] == 0xFF {
                let mut marker = [0u8; 1];
                read_exact_bounded(source, &mut marker)?;
                scanned = scanned.saturating_add(1);
                // 0xFF00 is a stuffed byte inside entropy data and 0xFFFF is padding;
                // every other 0xFFxx here is a marker we can act on.
                if marker[0] != 0x00 && marker[0] != 0xFF {
                    break marker[0];
                }
            }
            if scanned > MAX_HEADER_SCAN {
                return Err("no start-of-frame marker within the header budget".to_owned());
            }
        };

        // An image that ends without ever declaring a frame has no dimensions to
        // report, and guessing them from a thumbnail would invent content.
        if code == 0xD9 {
            return Err("no start-of-frame marker before end of image".to_owned());
        }
        // SOF0 baseline, SOF1 extended, SOF2 progressive. SOF3 and beyond are
        // lossless or arithmetic codings that no reader here claims.
        if matches!(code, 0xC0..=0xC2) {
            let segment = read_segment(source)?;
            if segment.len() < 6 {
                return Err("truncated start-of-frame segment".to_owned());
            }
            let bit_depth = segment[0];
            let height = u16::from_be_bytes([segment[1], segment[2]]) as u32;
            let width = u16::from_be_bytes([segment[3], segment[4]]) as u32;
            if width == 0 || height == 0 {
                return Err("zero-sized start-of-frame".to_owned());
            }
            let layout = match segment[5] {
                1 => PixelLayout::Grayscale,
                3 => PixelLayout::Rgb,
                _ => PixelLayout::Unknown,
            };
            return Ok(ImageMetadata {
                format: ImageFormat::Jpeg,
                width,
                height,
                bit_depth,
                layout,
                // JPEG has no alpha channel in any of its forms. Spec 14's H.264 row
                // states the same limit for video: alpha needs an explicit opaque
                // background, which is a decision above this file.
                has_alpha: false,
                interlaced: code == 0xC2,
                color_markers,
            });
        }
        // An `APP2` ICC profile is the one segment that carries a fact this file records
        // rather than skips, so its identifier is read and the rest of the profile is
        // discarded without being buffered — the profile is the largest segment an
        // ordinary photo carries and none of its bytes are needed here.
        if code == 0xE2 {
            let consumed = skip_app2_segment(source, &mut color_markers)?;
            scanned = scanned.saturating_add(consumed);
        } else if !matches!(code, 0x01 | 0xD0..=0xD9) {
            // Standalone markers (0x01, RSTn, SOI, EOI) carry no length; anything else
            // must be skipped rather than parsed, so that an APP2 ICC profile of any size
            // cannot be mistaken for a frame header.
            scanned = scanned.saturating_add(skip_segment(source)?);
        }
        if scanned > MAX_HEADER_SCAN {
            return Err("no start-of-frame marker within the header budget".to_owned());
        }
    }
}

/// The `ICC_PROFILE` identifier an `APP2` segment starts with, as the format fixes it.
const ICC_PROFILE_IDENTIFIER: [u8; 12] = *b"ICC_PROFILE\0";

/// Consume an `APP2` segment, recording it as an embedded profile if it claims to be one.
///
/// Returns the bytes consumed including the length field, so the caller's scan budget
/// accounts for a profile exactly as it accounts for a skipped segment.
fn skip_app2_segment<R: Read>(
    source: &mut R,
    markers: &mut ImageColorMarkers,
) -> Result<u64, String> {
    let payload = segment_length(source)?;
    let mut identifier = [0u8; ICC_PROFILE_IDENTIFIER.len()];
    let read = std::cmp::min(payload, identifier.len());
    if read > 0 {
        read_exact_bounded(source, &mut identifier[..read])?;
    }
    let remaining = payload - read;
    if remaining > 0 {
        discard(source, remaining as u64)?;
    }
    if read == identifier.len() && identifier == ICC_PROFILE_IDENTIFIER {
        markers.icc_profile = Some(EmbeddedProfile::Unnamed);
    }
    Ok(payload as u64 + 2)
}

/// The profile name an `iCCP` chunk declares: the payload is that name as text, a `NUL`,
/// a compression method, then the compressed profile.
///
/// PNG constrains the name to 1 to 79 bytes, and a chunk with no terminator inside that
/// window is not a profile the file can be trusted to describe. Latin-1 is the format's
/// own charset for this field, which for a name a report shows is close enough to be
/// carried as the bytes the author wrote rather than re-encoded into a guess.
fn icc_profile_name(payload: &[u8]) -> Result<String, String> {
    let terminator = payload
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| "iCCP chunk has no profile-name terminator".to_owned())?;
    if terminator == 0 {
        return Err("iCCP chunk declares an empty profile name".to_owned());
    }
    Ok(payload[..terminator]
        .iter()
        .map(|byte| char::from(*byte))
        .collect())
}

/// One PNG chunk's declared length and four-byte type, big-endian as the format says.
fn chunk_header<R: Read>(source: &mut R) -> Result<(u32, [u8; 4]), String> {
    let mut header = [0u8; 8];
    read_exact_bounded(source, &mut header)?;
    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    Ok((length, [header[4], header[5], header[6], header[7]]))
}

/// Read a chunk payload and its CRC.
///
/// `exact` asks for one specific payload size (`IHDR`'s thirteen bytes) and refuses
/// anything else; zero means "however long it claims to be, within the budget". The
/// claim is checked against the budget *before* allocating, because the length field
/// is file data rather than a promise.
fn read_chunk_payload<R: Read>(
    source: &mut R,
    length: u32,
    exact: usize,
) -> Result<Vec<u8>, String> {
    if u64::from(length) > MAX_HEADER_SCAN {
        return Err(format!(
            "chunk declares {length} bytes, above the {MAX_HEADER_SCAN} byte header budget"
        ));
    }
    if exact != 0 && length as usize != exact {
        return Err(format!("chunk declares {} bytes, expected {exact}", length));
    }
    let mut data = vec![0u8; length as usize];
    if length > 0 {
        read_exact_bounded(source, &mut data)?;
    }
    // The four-byte CRC is present; it is consumed here and not verified, per the
    // module header.
    let mut crc = [0u8; 4];
    read_exact_bounded(source, &mut crc)?;
    Ok(data)
}

fn skip_remaining<R: Read>(source: &mut R, length: u32) -> Result<(), String> {
    if u64::from(length) > MAX_HEADER_SCAN {
        return Err(format!(
            "chunk declares {length} bytes, above the {MAX_HEADER_SCAN} byte header budget"
        ));
    }
    // Payload plus CRC.
    discard(source, u64::from(length) + 4)
}

/// Read a JPEG segment's payload, returning the bytes consumed including its length
/// field.
fn read_segment<R: Read>(source: &mut R) -> Result<Vec<u8>, String> {
    let payload = segment_length(source)?;
    let mut data = vec![0u8; payload];
    if payload > 0 {
        read_exact_bounded(source, &mut data)?;
    }
    Ok(data)
}

fn skip_segment<R: Read>(source: &mut R) -> Result<u64, String> {
    let payload = segment_length(source)?;
    discard(source, payload as u64)?;
    Ok(payload as u64 + 2)
}

/// A JPEG segment's two-byte length, whose count includes itself.
fn segment_length<R: Read>(source: &mut R) -> Result<usize, String> {
    let mut length = [0u8; 2];
    read_exact_bounded(source, &mut length)?;
    let declared = u16::from_be_bytes([length[0], length[1]]);
    let payload = declared
        .checked_sub(2)
        .ok_or_else(|| "segment length shorter than its own header".to_owned())?;
    if u64::from(payload) > MAX_HEADER_SCAN {
        return Err(format!(
            "segment declares {payload} bytes, above the scan budget"
        ));
    }
    Ok(payload as usize)
}

/// Read and throw away `count` bytes.
fn discard<R: Read>(source: &mut R, count: u64) -> Result<(), String> {
    let mut buffer = [0u8; 4096];
    let mut left = count;
    while left > 0 {
        let want = std::cmp::min(left, buffer.len() as u64) as usize;
        let read = source
            .read(&mut buffer[..want])
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("stream ended inside a marker segment".to_owned());
        }
        left -= read as u64;
    }
    Ok(())
}

/// Fill `buffer` completely, reporting the byte count that could not be read.
fn read_exact_bounded<R: Read>(source: &mut R, buffer: &mut [u8]) -> Result<(), String> {
    let want = buffer.len();
    if want == 0 {
        return Ok(());
    }
    source
        .read_exact(buffer)
        .map_err(|e| format!("expected {want} bytes: {e}"))
}

fn describe(chunk_type: &[u8; 4]) -> String {
    match std::str::from_utf8(chunk_type) {
        Ok(text) => text.to_owned(),
        Err(_) => format!("{chunk_type:02X?}"),
    }
}

/// The image kind's metadata reader.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageDecoder;

/// One decoded image, in the linear working space WP06 samples from.
///
/// `pixels` is row-major with exactly `width * height` entries, four components per pixel.
/// The components are **linear** (the record's transfer function has been applied, never an
/// sRGB-encoded value) and are `f32`, not clamped to eight bits: the `Rgba8` in a
/// [`DecoderConfiguration`] name describes the *source* bit depth the configuration was
/// written against, not the width of this output. Alpha is straight unless the configuration
/// asked for premultiplication, in which case it has already been multiplied into the colour
/// channels in linear space (spec 15.7). This is a derived value (spec 11.10): cacheable by
/// `(content_hash, importer_version, configuration)`, never authoritative.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Row-major, 4 components per pixel, **linear** (not sRGB-encoded), not clamped to 8 bits.
    pub pixels: Vec<[f32; 4]>,
}

/// The allocation ceiling the pixel pass mirrors from the pinned `png` crate's own default
/// [`png::Limits`] (64 MiB). A crafted header can declare dimensions whose raw frame would
/// exhaust memory; bounding the output buffer here turns that into a typed refusal rather
/// than an allocation failure, and matches the budget the decoder already enforces internally.
const MAX_DECODE_BYTES: usize = 64 * 1024 * 1024;

impl ImageDecoder {
    /// The formats with a header reader in this build.
    #[must_use]
    pub const fn supported_formats() -> &'static [ImageFormat] {
        &[ImageFormat::Png, ImageFormat::Jpeg]
    }

    /// Decode one stored image's pixels into the linear working space (spec 11.3, 15.7).
    ///
    /// The transfer function comes from `record.image`, never from a guess: an `Srgb` claim
    /// applies the exact sRGB EOTF to RGB, a `Linear` claim passes the samples through, and an
    /// `Unclassified` claim on either axis is refused with [`AssetError::UnclassifiedImageClaims`]
    /// naming the asset and the remedy (spec 15.11's prohibition on a silent fallback). Alpha
    /// is never transferred. `ImageAlpha::Opaque` forces alpha to 1.0 regardless of what the
    /// file carries; `Straight` uses the file's alpha; `Unclassified` is refused.
    ///
    /// Premultiplication is the configuration's job and happens in **linear** space, after the
    /// transfer: `Rgba8Premultiplied` multiplies colour by alpha (spec 15.7's
    /// `rgb_premul = rgb_linear * alpha`), `Rgba8Unpremultiplied` leaves alpha straight. The
    /// `Rgba8` spelling names the eight-bit *source*, not this `f32` output; the variants are
    /// not renamed because their `as_str()` spellings feed [`crate::decode::derived_key`] and a
    /// rename would move every cache key for no behavioural change.
    ///
    /// Interlaced (Adam7) PNGs are decoded, not refused — the `png` crate expands the passes
    /// itself. Paletted, grayscale, grayscale+alpha, RGB and RGBA PNGs all expand to four
    /// components through the crate's transformation API; 16-bit sources are reduced to eight
    /// bits by the crate's `STRIP_16` transform *before* the transfer, so that reduction is
    /// lossy and is documented rather than passed off as lossless. JPEG has no alpha channel,
    /// so its alpha is 1.0. Malformed or truncated bytes produce [`AssetError::MalformedAsset`],
    /// never a panic.
    ///
    /// # Errors
    ///
    /// [`AssetError::IncompatibleDecodeConfiguration`] if `configuration` is not an image
    /// target, [`AssetError::MissingImageClaims`] if the record carries no claims,
    /// [`AssetError::UnclassifiedImageClaims`] if either claim is `Unclassified`,
    /// [`AssetError::UnsupportedDecoder`] if the object's signature has no reader,
    /// [`AssetError::Io`] if the object cannot be read, and [`AssetError::MalformedAsset`] if
    /// the bytes do not decode.
    pub fn decode_pixels(
        &self,
        record: &AssetManifestEntry,
        path: &Path,
        configuration: DecoderConfiguration,
    ) -> Result<DecodedImage, AssetError> {
        let asset_id = record.asset_id.to_canonical_string();

        // The configuration selects premultiplication; the audio PCM target is not an image
        // decode and is refused rather than silently treated as one of the RGBA targets.
        let premultiply = match configuration {
            DecoderConfiguration::Rgba8Unpremultiplied => false,
            DecoderConfiguration::Rgba8Premultiplied => true,
            DecoderConfiguration::PcmF32Interleaved => {
                return Err(AssetError::IncompatibleDecodeConfiguration {
                    asset_id,
                    kind: record.kind.to_string(),
                    configuration: configuration.as_str().to_owned(),
                });
            }
        };

        // The transfer and alpha mode are the record's claims, read here and nowhere else.
        // A valid `Image` record always carries claims (the constructor enforces it), so the
        // `None` branch is the defensive refusal for a record that is not an image at all.
        let claims = record.image.ok_or_else(|| AssetError::MissingImageClaims {
            asset_id: asset_id.clone(),
        })?;
        if claims.color_space == ImageColorSpace::Unclassified {
            return Err(AssetError::UnclassifiedImageClaims {
                asset_id,
                axis: "color_space".to_owned(),
            });
        }
        if claims.alpha == ImageAlpha::Unclassified {
            return Err(AssetError::UnclassifiedImageClaims {
                asset_id,
                axis: "alpha".to_owned(),
            });
        }
        let srgb = claims.color_space == ImageColorSpace::Srgb;
        let force_opaque = claims.alpha == ImageAlpha::Opaque;

        // Sniff the format from the signature, then rewind so the format decoder sees the
        // object from byte zero. The PNG decoder streams from the `BufRead + Seek` source;
        // the JPEG decoder, pinned without its `std` adapter, reads an in-memory cursor.
        let file =
            File::open(path).map_err(|e| crate::io_error("open image", path, e.to_string()))?;
        let mut reader = BufReader::new(file);
        let mut probe = [0u8; 8];
        let probed = reader
            .read(&mut probe)
            .map_err(|e| crate::io_error("read image header", path, e.to_string()))?;
        let Some(format) = ImageFormat::sniff(&probe[..probed]) else {
            return Err(AssetError::UnsupportedDecoder {
                kind: AssetKind::Image.to_string(),
                importer_version: CURRENT_IMPORTER_VERSION,
            });
        };
        reader
            .rewind()
            .map_err(|e| crate::io_error("rewind image", path, e.to_string()))?;

        match format {
            ImageFormat::Png => {
                decode_png(reader, &asset_id, path, srgb, force_opaque, premultiply)
            }
            ImageFormat::Jpeg => {
                // `zune-jpeg` is pinned with `default-features = false`, so its `std` reader
                // adapter is absent and it decodes from an in-memory cursor rather than a
                // `BufRead` stream. The compressed object is read whole for this path only.
                let mut bytes = Vec::new();
                reader
                    .read_to_end(&mut bytes)
                    .map_err(|e| crate::io_error("read image", path, e.to_string()))?;
                decode_jpeg(&bytes, &asset_id, path, srgb, force_opaque, premultiply)
            }
        }
    }
}

/// The exact sRGB EOTF (IEC 61966-2-1), applied to a normalized encoded sample.
///
/// The piecewise is the specification's own: a linear segment below 0.04045 and the 2.4-power
/// segment above it. This is the only transfer this engine applies, and it is applied solely
/// because a record claims `Srgb` — never as a fallback for an unclassified asset.
fn srgb_eotf(encoded: f32) -> f32 {
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// Turn one eight-bit source pixel into the linear, optionally premultiplied output.
///
/// The order is load-bearing and is the whole of spec 15.7's colour rule: normalize, apply the
/// transfer to RGB (alpha is never transferred), resolve alpha from the claim, then — only if
/// the configuration asked — multiply colour by alpha in linear space. Premultiplying before
/// the transfer would multiply encoded values and give a different, wrong result.
fn finish_pixel(
    r: u8,
    g: u8,
    b: u8,
    a: u8,
    srgb: bool,
    force_opaque: bool,
    premultiply: bool,
) -> [f32; 4] {
    let alpha = if force_opaque {
        1.0
    } else {
        f32::from(a) / 255.0
    };
    let mut rgb = [
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    ];
    if srgb {
        for channel in &mut rgb {
            *channel = srgb_eotf(*channel);
        }
    }
    if premultiply {
        for channel in &mut rgb {
            *channel *= alpha;
        }
    }
    [rgb[0], rgb[1], rgb[2], alpha]
}

/// A pixel-decode failure, named with the asset, the format and the object path.
fn decode_malformed(
    asset_id: &str,
    format: ImageFormat,
    path: &Path,
    detail: impl Into<String>,
) -> AssetError {
    AssetError::MalformedAsset {
        asset_id: asset_id.to_owned(),
        kind: AssetKind::Image.to_string(),
        detail: format!("{format} at {}: {}", path.display(), detail.into()),
    }
}

/// Decode a PNG through the pinned `png` crate, expanding every layout to four components.
///
/// `EXPAND | ALPHA | STRIP_16` is the crate's own expansion path: paletted images become RGBA
/// (palette and `tRNS` alpha resolved by the crate, not hand-rolled), grayscale and RGB gain an
/// alpha channel, sub-8-bit samples widen to eight, and 16-bit samples strip to eight. The
/// strip happens before [`finish_pixel`] applies the transfer, so a 16-bit source is reduced
/// to eight bits lossily — documented at the entry point rather than hidden. After the
/// transform the output is either `GrayscaleAlpha` (two samples) or `Rgba` (four); both map to
/// four linear components here. Adam7 interlacing is expanded by `next_frame` itself.
fn decode_png<R: BufRead + Seek>(
    source: R,
    asset_id: &str,
    path: &Path,
    srgb: bool,
    force_opaque: bool,
    premultiply: bool,
) -> Result<DecodedImage, AssetError> {
    let malformed = |detail: String| decode_malformed(asset_id, ImageFormat::Png, path, detail);

    let mut decoder = png::Decoder::new(source);
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder.read_info().map_err(|e| malformed(e.to_string()))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| malformed("image dimensions do not fit a decode buffer".to_owned()))?;
    if size > MAX_DECODE_BYTES {
        return Err(malformed(format!(
            "decoded frame is {size} bytes, above the {MAX_DECODE_BYTES} byte decode budget"
        )));
    }
    let mut buf = vec![0u8; size];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| malformed(e.to_string()))?;

    let width = frame.width;
    let height = frame.height;
    let pixels = match frame.color_type {
        png::ColorType::Rgba => buf
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| finish_pixel(c[0], c[1], c[2], c[3], srgb, force_opaque, premultiply))
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| finish_pixel(c[0], c[0], c[0], c[1], srgb, force_opaque, premultiply))
            .collect(),
        _ => {
            return Err(malformed(
                "unexpected decoded PNG colour type after expansion".to_owned(),
            ));
        }
    };
    Ok(DecodedImage {
        width,
        height,
        pixels,
    })
}

/// Decode a JPEG through the pinned `zune-jpeg` crate into RGB, then to four linear components.
///
/// The crate's default output colour space is RGB: YCbCr converts, grayscale luma replicates
/// across the three channels, and the result is three eight-bit samples per pixel. JPEG carries
/// no alpha channel in any form, so alpha is 1.0 before the claim resolves it (and `Opaque`
/// forces the same 1.0). Headers are decoded first so the output size is checked before it is
/// allocated, and the crate's own dimension limits turn an oversized header into a typed error.
fn decode_jpeg(
    source: &[u8],
    asset_id: &str,
    path: &Path,
    srgb: bool,
    force_opaque: bool,
    premultiply: bool,
) -> Result<DecodedImage, AssetError> {
    let malformed = |detail: String| decode_malformed(asset_id, ImageFormat::Jpeg, path, detail);

    let cursor = zune_jpeg::zune_core::bytestream::ZCursor::new(source);
    let mut decoder = zune_jpeg::JpegDecoder::new(cursor);
    decoder
        .decode_headers()
        .map_err(|e| malformed(e.to_string()))?;
    let size = decoder
        .output_buffer_size()
        .ok_or_else(|| malformed("image dimensions do not fit a decode buffer".to_owned()))?;
    let mut rgb = vec![0u8; size];
    decoder
        .decode_into(&mut rgb)
        .map_err(|e| malformed(e.to_string()))?;
    let info = decoder
        .info()
        .ok_or_else(|| malformed("jpeg decoder reported no image info".to_owned()))?;
    let width = u32::from(info.width);
    let height = u32::from(info.height);
    let count = width as usize * height as usize;
    if rgb.len() != count * 3 {
        return Err(malformed(format!(
            "decoded {} bytes, expected {} for a {width}x{height} RGB frame",
            rgb.len(),
            count * 3
        )));
    }
    let pixels = rgb
        .as_chunks::<3>()
        .0
        .iter()
        .map(|c| finish_pixel(c[0], c[1], c[2], 255, srgb, force_opaque, premultiply))
        .collect();
    Ok(DecodedImage {
        width,
        height,
        pixels,
    })
}

impl Decoder for ImageDecoder {
    fn kind(&self) -> AssetKind {
        AssetKind::Image
    }

    fn supported_versions(&self) -> &[u32] {
        &[CURRENT_IMPORTER_VERSION]
    }

    fn read_metadata(&self, path: &Path) -> Result<AssetMetadata, AssetError> {
        image_metadata(path).map(AssetMetadata::Image)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::excessive_precision,
        reason = "test assertions only; the production rule is enforced at the workspace level. \
                  The float literals are independent f64-computed specification oracles compared \
                  against f32 output with a tolerance, so their full precision is deliberate."
    )]

    use super::*;

    /// One PNG chunk as bytes: length, type, payload, and a zero CRC that nothing
    /// here reads.
    fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out.extend_from_slice(&[0u8; 4]);
        out
    }

    fn ihdr(width: u32, height: u32, bit_depth: u8, color_type: u8, interlaced: u8) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&width.to_be_bytes());
        payload.extend_from_slice(&height.to_be_bytes());
        payload.push(bit_depth);
        payload.push(color_type);
        payload.push(0); // compression
        payload.push(0); // filter
        payload.push(interlaced);
        payload
    }

    fn png(color_type: u8, trns: bool) -> Vec<u8> {
        let mut out = ImageFormat::Png.signature().to_vec();
        out.extend_from_slice(&chunk(b"IHDR", &ihdr(64, 32, 8, color_type, 0)));
        if trns {
            out.extend_from_slice(&chunk(b"tRNS", &[0, 0, 0]));
        }
        out.extend_from_slice(&chunk(b"IDAT", &[0u8; 16]));
        out.extend_from_slice(&chunk(b"IEND", &[]));
        out
    }

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn png_colour_types_map_to_alpha_and_layout() {
        let dir = tempfile::tempdir().unwrap();
        for (color_type, layout, alpha) in [
            (0u8, PixelLayout::Grayscale, false),
            (2, PixelLayout::Rgb, false),
            (3, PixelLayout::Palette, false),
            (4, PixelLayout::GrayscaleAlpha, true),
            (6, PixelLayout::Rgba, true),
        ] {
            let path = write(&dir, "a.png", &png(color_type, false));
            let AssetMetadata::Image(meta) = ImageDecoder.read_metadata(&path).unwrap() else {
                panic!("an image decoder must return image metadata");
            };
            assert_eq!(meta.format, ImageFormat::Png);
            assert_eq!(
                (meta.width, meta.height),
                (64, 32),
                "colour type {color_type}"
            );
            assert_eq!(meta.bit_depth, 8);
            assert_eq!(meta.layout, layout, "colour type {color_type}");
            assert_eq!(meta.has_alpha, alpha, "colour type {color_type}");
            assert!(!meta.interlaced);
        }
    }

    #[test]
    fn a_trns_chunk_makes_an_indexed_image_transparent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "b.png", &png(3, true));
        let meta = image_metadata(&path).unwrap();
        assert_eq!(meta.layout, PixelLayout::Palette);
        assert!(meta.has_alpha, "tRNS before IDAT is the palette's alpha");

        // After `IDAT` it is too late to be found, which is the PNG rule rather than
        // this parser's shortcut.
        let mut late = ImageFormat::Png.signature().to_vec();
        late.extend_from_slice(&chunk(b"IHDR", &ihdr(4, 4, 8, 3, 0)));
        late.extend_from_slice(&chunk(b"IDAT", &[0u8; 4]));
        late.extend_from_slice(&chunk(b"tRNS", &[0, 0, 0]));
        late.extend_from_slice(&chunk(b"IEND", &[]));
        assert!(
            !image_metadata(&write(&dir, "c.png", &late))
                .unwrap()
                .has_alpha
        );
    }

    #[test]
    fn interlaced_and_bit_depth_are_reported_not_assumed() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = ImageFormat::Png.signature().to_vec();
        out.extend_from_slice(&chunk(b"IHDR", &ihdr(1920, 1080, 16, 6, 1)));
        out.extend_from_slice(&chunk(b"IDAT", &[0u8; 4]));
        let meta = image_metadata(&write(&dir, "d.png", &out)).unwrap();
        assert!(meta.interlaced);
        assert_eq!(meta.bit_depth, 16);
        assert_eq!((meta.width, meta.height), (1920, 1080));
    }

    #[test]
    fn a_large_idat_is_skipped_rather_than_refused() {
        // The budget is a header-scan bound, not a file-size bound: an 8 MiB IDAT is
        // an ordinary cel, and the parser must stop at its header without reading it
        // and without claiming it is too big.
        let dir = tempfile::tempdir().unwrap();
        let mut out = ImageFormat::Png.signature().to_vec();
        out.extend_from_slice(&chunk(b"IHDR", &ihdr(8, 8, 8, 6, 0)));
        out.extend_from_slice(&(8u32 * 1024 * 1024).to_be_bytes());
        out.extend_from_slice(b"IDAT");
        let path = write(&dir, "big.png", &out);
        let meta = image_metadata(&path).unwrap();
        assert_eq!((meta.width, meta.height), (8, 8));
    }

    #[test]
    fn png_headers_that_cannot_be_trusted_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("wrong signature", b"NOTAPNGFILE".to_vec()),
            (
                "truncated signature",
                ImageFormat::Png.signature()[..4].to_vec(),
            ),
            (
                "first chunk is not IHDR",
                [
                    ImageFormat::Png.signature().to_vec(),
                    chunk(b"gABD", &ihdr(1, 1, 8, 6, 0)),
                ]
                .concat(),
            ),
            (
                "short IHDR",
                [
                    ImageFormat::Png.signature().to_vec(),
                    chunk(b"IHDR", &ihdr(1, 1, 8, 6, 0)[..9]),
                ]
                .concat(),
            ),
            (
                "IHDR claiming more than the budget",
                [
                    ImageFormat::Png.signature().to_vec(),
                    vec![0xFF, 0xFF, 0xFF, 0xFF],
                    b"IHDR".to_vec(),
                ]
                .concat(),
            ),
            (
                "zero dimensions",
                [
                    ImageFormat::Png.signature().to_vec(),
                    chunk(b"IHDR", &ihdr(0, 0, 8, 6, 0)),
                ]
                .concat(),
            ),
            (
                "unknown colour type",
                [
                    ImageFormat::Png.signature().to_vec(),
                    chunk(b"IHDR", &ihdr(8, 8, 8, 7, 0)),
                ]
                .concat(),
            ),
            (
                "ends mid-chunk",
                [
                    ImageFormat::Png.signature().to_vec(),
                    chunk(b"IHDR", &ihdr(8, 8, 8, 6, 0))[..10].to_vec(),
                ]
                .concat(),
            ),
        ];
        for (label, bytes) in cases {
            let path = write(&dir, "bad.png", &bytes);
            let e = image_metadata(&path).unwrap_err();
            // An unrecognized signature means no reader; a recognized one that does
            // not parse means a malformed asset. Both are typed and both name the kind.
            if label == "wrong signature" || label == "truncated signature" {
                assert!(
                    matches!(e, AssetError::UnsupportedDecoder { .. }),
                    "{label}: {e}"
                );
            } else {
                assert!(
                    matches!(e, AssetError::MalformedAsset { .. }),
                    "{label}: {e}"
                );
                assert!(e.to_string().contains("png"), "{label}: {e}");
            }
        }
    }

    #[test]
    fn jpeg_baseline_progressive_and_grayscale_are_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        // A start-of-frame payload is precision + height(2) + width(2) + component
        // count = 6 bytes, and the segment's length field counts itself, so it
        // declares 8.
        for (code, interlaced) in [(0xC0u8, false), (0xC1, false), (0xC2, true)] {
            let mut out = vec![0xFF, 0xD8];
            // APP0, which must be skipped rather than parsed as a frame.
            out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x05, b'J', b'F', 0]);
            out.extend_from_slice(&[0xFF, code, 0x00, 0x08, 8, 0x00, 0x10, 0x00, 0x20, 3]);
            out.extend_from_slice(&[0xFF, 0xD9]);
            let meta = image_metadata(&write(&dir, "e.jpg", &out)).unwrap();
            assert_eq!(meta.format, ImageFormat::Jpeg);
            assert_eq!((meta.width, meta.height), (32, 16), "SOF code {code:#04x}");
            assert_eq!(meta.bit_depth, 8);
            assert_eq!(meta.layout, PixelLayout::Rgb);
            assert!(!meta.has_alpha, "jpeg has no alpha channel");
            assert_eq!(meta.interlaced, interlaced, "SOF2 is progressive");
        }

        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x08, 8, 0x00, 0x04, 0x00, 0x08, 1]);
        assert_eq!(
            image_metadata(&write(&dir, "g.jpg", &out)).unwrap().layout,
            PixelLayout::Grayscale
        );

        // Four components (CMYK) is a layout no rule here names, reported as such.
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x08, 8, 0x00, 0x04, 0x00, 0x08, 4]);
        assert_eq!(
            image_metadata(&write(&dir, "k.jpg", &out)).unwrap().layout,
            PixelLayout::Unknown
        );
    }

    #[test]
    fn jpeg_without_a_frame_header_is_refused_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        // A thumbnail-only Exif file: SOI, one APP segment, EOI, no SOF.
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x06, b'e', b'x', b'i', b'f']);
        out.extend_from_slice(&[0xFF, 0xD9]);
        let e = image_metadata(&write(&dir, "h.jpg", &out)).unwrap_err();
        assert!(matches!(e, AssetError::MalformedAsset { .. }), "{e}");
        assert!(e.to_string().contains("start-of-frame"), "{e}");
    }

    #[test]
    fn a_large_icc_profile_does_not_become_a_frame_header() {
        let dir = tempfile::tempdir().unwrap();
        // A real APP2 ICC profile is split into 64 KiB pieces by its format's own
        // segment limit, so the fixture is eight of them: ~512 KiB of bytes that look
        // like markers, well past any fixed prefix, before the frame header arrives.
        let mut out = vec![0xFF, 0xD8];
        for index in 0..8u8 {
            let mut filler = vec![0xFFu8; 65_535 - 2];
            for (i, byte) in filler.iter_mut().enumerate() {
                // Never a valid marker pair, but always a byte that a fixed-prefix
                // scanner would have to be careful about.
                *byte = if i % 2 == 0 { 0x7F } else { 0xFF - index };
            }
            let declared = (filler.len() + 2) as u16;
            out.extend_from_slice(&[0xFF, 0xE2]);
            out.extend_from_slice(&declared.to_be_bytes());
            out.extend_from_slice(&filler);
        }
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x08, 8, 0x00, 0x02, 0x00, 0x02, 1]);
        let meta = image_metadata(&write(&dir, "i.jpg", &out)).unwrap();
        assert_eq!((meta.width, meta.height), (2, 2));
        assert_eq!(meta.format, ImageFormat::Jpeg);
    }

    #[test]
    fn sniffing_is_by_signature_not_extension() {
        let dir = tempfile::tempdir().unwrap();
        // Named .png, holding a JPEG. The extension is display metadata.
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x08, 8, 0x00, 0x08, 0x00, 0x08, 1]);
        assert_eq!(
            image_metadata(&write(&dir, "named-wrong.png", &out))
                .unwrap()
                .format,
            ImageFormat::Jpeg
        );
    }

    #[test]
    fn jpeg_payload_too_short_for_a_frame_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x03]); // declares one payload byte
        out.extend_from_slice(&[0x08]);
        let e = image_metadata(&write(&dir, "j.jpg", &out)).unwrap_err();
        assert!(matches!(e, AssetError::MalformedAsset { .. }), "{e}");
        assert!(e.to_string().contains("truncated"), "{e}");
    }

    #[test]
    fn decoder_identity_is_the_image_kind_at_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let decoder = ImageDecoder;
        assert_eq!(decoder.kind(), AssetKind::Image);
        assert_eq!(decoder.supported_versions(), &[CURRENT_IMPORTER_VERSION]);
        assert!(ImageDecoder::supported_formats().contains(&ImageFormat::Png));
        // A WebP signature is a missing reader, not a bad file.
        let e = decoder
            .read_metadata(&write(&dir, "w", b"RIFF....WEBPVP8 "))
            .unwrap_err();
        assert!(matches!(e, AssetError::UnsupportedDecoder { .. }), "{e}");
        assert!(e.to_string().contains("image"), "{e}");
    }

    #[test]
    fn a_missing_file_is_an_io_error_not_a_malformed_asset() {
        let dir = tempfile::tempdir().unwrap();
        let e = image_metadata(&dir.path().join("absent.png")).unwrap_err();
        assert!(matches!(e, AssetError::Io { .. }), "{e}");
    }

    /// A PNG with one extra chunk between `IHDR` and `IDAT`, which is where the format
    /// puts every colour and transparency marker.
    fn png_with(color_type: u8, extra: (&[u8; 4], &[u8])) -> Vec<u8> {
        let mut out = ImageFormat::Png.signature().to_vec();
        out.extend_from_slice(&chunk(b"IHDR", &ihdr(64, 32, 8, color_type, 0)));
        out.extend_from_slice(&chunk(extra.0, extra.1));
        out.extend_from_slice(&chunk(b"IDAT", &[0u8; 16]));
        out.extend_from_slice(&chunk(b"IEND", &[]));
        out
    }

    fn claims_of(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> ImageAssetClaims {
        let meta = image_metadata(&write(dir, name, bytes)).unwrap();
        let (claims, _) = classify_image(&meta, "asset-under-test");
        claims
    }

    #[test]
    fn a_declared_srgb_chunk_and_the_unmarked_default_are_the_same_answer_differently_earned() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            claims_of(&dir, "declared.png", &png_with(6, (b"sRGB", &[0]))),
            ImageAssetClaims {
                color_space: ImageColorSpace::Srgb,
                alpha: ImageAlpha::Straight,
            }
        );
        // The default rule: no colour chunk of any kind. Same *value*, and the point of
        // asserting both is that the value is recorded either way, so a later change to
        // this one rule is a schema event and not a re-reading of the same file.
        assert_eq!(
            claims_of(&dir, "unmarked.png", &png(6, false)),
            ImageAssetClaims {
                color_space: ImageColorSpace::Srgb,
                alpha: ImageAlpha::Straight,
            }
        );
    }

    #[test]
    fn gamma_one_is_linear_and_any_other_gamma_is_not_a_colour_space() {
        let dir = tempfile::tempdir().unwrap();
        let linear = png_with(2, (b"gAMA", &100_000u32.to_be_bytes()));
        assert_eq!(
            claims_of(&dir, "linear.png", &linear).color_space,
            ImageColorSpace::Linear
        );
        // 45 455 is the format's spelling of gamma 2.2. It names a curve, not primaries,
        // so no rule fires and the default answers — which is why `Linear` has exactly one
        // producer and a file cannot reach it by being merely nonlinear.
        assert_eq!(
            claims_of(
                &dir,
                "gamma22.png",
                &png_with(2, (b"gAMA", &45_455u32.to_be_bytes()))
            )
            .color_space,
            ImageColorSpace::Srgb
        );
        // Alpha is orthogonal: a linear image with no alpha channel is `Opaque`.
        assert_eq!(
            claims_of(&dir, "linear.png", &linear).alpha,
            ImageAlpha::Opaque
        );
    }

    #[test]
    fn an_embedded_profile_is_refused_and_named_rather_than_approximated() {
        let dir = tempfile::tempdir().unwrap();
        // `iCCP`'s payload is the author's profile name, a NUL, a compression method,
        // then the profile itself.
        let mut profile = b"Adobe RGB (1998)".to_vec();
        profile.push(0);
        profile.push(0);
        profile.extend_from_slice(&[0u8; 32]);
        let meta =
            image_metadata(&write(&dir, "wide.png", &png_with(6, (b"iCCP", &profile)))).unwrap();
        let (claims, finding) = classify_image(&meta, "asset-42");
        assert_eq!(claims.color_space, ImageColorSpace::Unclassified);
        let finding = finding.expect("a refusal to classify is a reportable finding");
        assert_eq!(
            finding.code.as_str(),
            "asset.unmodelled_colour_profile",
            "{finding:?}"
        );
        assert_eq!(
            finding.severity,
            anim_core::diagnostic::DiagnosticSeverity::Warning,
            "the import succeeded and said what it does not know"
        );
        assert!(
            finding
                .context
                .values()
                .any(|value| value == "Adobe RGB (1998)"),
            "the profile is named as the file named it: {finding:?}"
        );
        assert!(
            finding.context.values().any(|value| value == "asset-42"),
            "and so is the asset: {finding:?}"
        );

        // `c2PC` carries a profile with no name field, so the finding says so rather than
        // inventing one.
        let meta = image_metadata(&write(
            &dir,
            "wide2.png",
            &png_with(6, (b"c2PC", &[0u8; 8])),
        ))
        .unwrap();
        let (claims, finding) = classify_image(&meta, "asset-43");
        assert_eq!(claims.color_space, ImageColorSpace::Unclassified);
        let finding = finding.expect("an unnamed profile is still an un-modelled profile");
        assert!(
            finding.message.contains("unnamed embedded profile"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn a_profile_beats_a_co_existing_srgb_declaration() {
        // Tools write both. Choosing the chunk would be choosing which of two conflicting
        // claims to believe, and the answer this engine gives is that it does not choose:
        // the profile is the one it cannot model, so the file is reported.
        let dir = tempfile::tempdir().unwrap();
        let mut out = ImageFormat::Png.signature().to_vec();
        out.extend_from_slice(&chunk(b"IHDR", &ihdr(8, 8, 8, 6, 0)));
        out.extend_from_slice(&chunk(b"sRGB", &[0]));
        out.extend_from_slice(&chunk(
            b"iCCP",
            &[b"Display P3".to_vec(), vec![0, 0, 1, 2, 3]].concat(),
        ));
        out.extend_from_slice(&chunk(b"IDAT", &[0u8; 4]));
        let meta = image_metadata(&write(&dir, "both.png", &out)).unwrap();
        let (claims, finding) = classify_image(&meta, "both");
        assert_eq!(claims.color_space, ImageColorSpace::Unclassified);
        assert!(finding.is_some());
    }

    #[test]
    fn nothing_that_reads_a_file_invents_an_unknown_alpha() {
        // `Unclassified` has exactly two producers: an embedded profile on the colour axis,
        // and the document migration on both. These fixtures span every header shape this
        // parser can return that is not a profile, and neither answer is ever unknown,
        // because both formats state whether an alpha channel exists and the colour rule
        // ends in a default rather than in a shrug.
        let dir = tempfile::tempdir().unwrap();
        let cases: Vec<Vec<u8>> = vec![
            png(0, false),
            png(2, false),
            png(3, false),
            png(3, true),
            png(4, false),
            png(6, false),
            png_with(6, (b"sRGB", &[0])),
            png_with(2, (b"gAMA", &100_000u32.to_be_bytes())),
            {
                let mut out = vec![0xFF, 0xD8];
                out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x08, 8, 0x00, 0x10, 0x00, 0x10, 3]);
                out
            },
        ];
        for (index, bytes) in cases.iter().enumerate() {
            let name = if index >= 8 { "j.jpg" } else { "c.png" };
            let meta = image_metadata(&write(&dir, name, bytes)).unwrap();
            let (claims, finding) = classify_image(&meta, "asset");
            assert_ne!(
                claims.alpha,
                ImageAlpha::Unclassified,
                "case {index} was classified, so it is known"
            );
            assert_ne!(
                claims.color_space,
                ImageColorSpace::Unclassified,
                "case {index} carries no profile"
            );
            assert!(
                finding.is_none(),
                "and nothing here has a finding to report: case {index}"
            );
        }
    }

    #[test]
    fn alpha_follows_the_channel_the_header_declares() {
        let dir = tempfile::tempdir().unwrap();
        for (color_type, trns, alpha) in [
            (0u8, false, ImageAlpha::Opaque),
            (2, false, ImageAlpha::Opaque),
            (3, false, ImageAlpha::Opaque),
            (3, true, ImageAlpha::Straight),
            (4, false, ImageAlpha::Straight),
            (6, false, ImageAlpha::Straight),
        ] {
            assert_eq!(
                claims_of(&dir, "a.png", &png(color_type, trns)).alpha,
                alpha,
                "colour type {color_type} with tRNS={trns}"
            );
        }

        // JPEG: no alpha in any form, so `Opaque` is the format's answer and not a guess.
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x08, 8, 0x00, 0x10, 0x00, 0x10, 3]);
        assert_eq!(claims_of(&dir, "flat.jpg", &out).alpha, ImageAlpha::Opaque);
    }

    #[test]
    fn a_jpeg_app2_profile_is_unnamed_and_the_default_covers_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        // A real ICC-bearing export: APP2 with the format's fixed identifier, then the
        // profile bytes, then the frame.
        let mut out = vec![0xFF, 0xD8];
        let profile = [b"ICC_PROFILE\0".to_vec(), vec![1, 1, 0], vec![9u8; 40]].concat();
        let declared = (profile.len() + 2) as u16;
        out.extend_from_slice(&[0xFF, 0xE2]);
        out.extend_from_slice(&declared.to_be_bytes());
        out.extend_from_slice(&profile);
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x08, 8, 0x00, 0x10, 0x00, 0x10, 3]);
        let meta = image_metadata(&write(&dir, "c.jpg", &out)).unwrap();
        let (claims, finding) = classify_image(&meta, "jpeg-with-profile");
        assert_eq!(claims.color_space, ImageColorSpace::Unclassified);
        assert!(finding.is_some(), "and the report says which one: {claims}");

        // The same file with an Exif APP1 instead is an ordinary unmarked JPEG.
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x0A]);
        out.extend_from_slice(b"Exif_xx\0\0");
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x08, 8, 0x00, 0x10, 0x00, 0x10, 3]);
        let meta = image_metadata(&write(&dir, "d.jpg", &out)).unwrap();
        let (claims, finding) = classify_image(&meta, "jpeg-exif-only");
        assert_eq!(claims.color_space, ImageColorSpace::Srgb);
        assert!(finding.is_none());
    }

    #[test]
    fn claim_spellings_are_one_string_for_json_and_for_hashing() {
        for (space, spelling) in [
            (ImageColorSpace::Srgb, "srgb"),
            (ImageColorSpace::Linear, "linear"),
            (ImageColorSpace::Unclassified, "unclassified"),
        ] {
            assert_eq!(space.as_str(), spelling);
            assert_eq!(space.to_string(), spelling);
            let json = serde_json::to_string(&space).unwrap();
            assert_eq!(json, format!("\"{spelling}\""));
            assert_eq!(
                serde_json::from_str::<ImageColorSpace>(&json).unwrap(),
                space
            );
        }
        for (alpha, spelling) in [
            (ImageAlpha::Opaque, "opaque"),
            (ImageAlpha::Straight, "straight"),
            (ImageAlpha::Unclassified, "unclassified"),
        ] {
            assert_eq!(alpha.as_str(), spelling);
            assert_eq!(alpha.to_string(), spelling);
            let json = serde_json::to_string(&alpha).unwrap();
            assert_eq!(json, format!("\"{spelling}\""));
            assert_eq!(serde_json::from_str::<ImageAlpha>(&json).unwrap(), alpha);
        }

        // The pair serializes as the two fields and round-trips, which is what lets the
        // migration write it as JSON and the loader read it back through the constructor.
        let claims = ImageAssetClaims {
            color_space: ImageColorSpace::Unclassified,
            alpha: ImageAlpha::Unclassified,
        };
        let json = serde_json::to_string(&claims).unwrap();
        assert_eq!(
            json,
            r#"{"color_space":"unclassified","alpha":"unclassified"}"#
        );
        assert_eq!(
            serde_json::from_str::<ImageAssetClaims>(&json).unwrap(),
            claims
        );
    }

    #[test]
    fn a_profile_chunk_that_cannot_be_read_is_a_malformed_asset() {
        // `iCCP` with no name terminator is not a profile the file can be trusted to
        // describe; refusing it is the same rule that refuses an unknown colour type, and
        // it is why the record's claims are only ever about a header that parsed.
        let dir = tempfile::tempdir().unwrap();
        let e = image_metadata(&write(
            &dir,
            "e.png",
            &png_with(6, (b"iCCP", b"no terminator")),
        ))
        .unwrap_err();
        assert!(matches!(e, AssetError::MalformedAsset { .. }), "{e}");
        assert!(e.to_string().contains("iCCP"), "{e}");
    }

    // ---- pixel decode (WP06, DECISIONS.md #41 ruling 2) ----

    use crate::hash::ContentHash;
    use anim_core::id::AssetId;
    use uuid::Uuid;

    /// A record carrying exactly the claims under test, so the decode reads its transfer and
    /// alpha mode from the manifest the way the renderer will — never from the file.
    fn decode_record(claims: ImageAssetClaims) -> AssetManifestEntry {
        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        AssetManifestEntry::new(
            AssetId::derive(&namespace, b"decode-test"),
            ContentHash::from_bytes([0x11; 32]),
            0,
            AssetKind::Image,
            CURRENT_IMPORTER_VERSION,
            [],
            "cel",
            Some(claims),
        )
        .unwrap()
    }

    fn claims(color_space: ImageColorSpace, alpha: ImageAlpha) -> ImageAssetClaims {
        ImageAssetClaims { color_space, alpha }
    }

    /// Encode a real, decodable PNG with the pinned crate's own encoder.
    fn encode_png(
        width: u32,
        height: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(color);
            encoder.set_depth(depth);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        out
    }

    fn encode_indexed_png(palette: &[u8], indices: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, 1, 1);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(palette.to_vec());
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(indices).unwrap();
        }
        out
    }

    fn decode(
        dir: &tempfile::TempDir,
        name: &str,
        bytes: &[u8],
        claims: ImageAssetClaims,
        configuration: DecoderConfiguration,
    ) -> Result<DecodedImage, AssetError> {
        let path = write(dir, name, bytes);
        ImageDecoder.decode_pixels(&decode_record(claims), &path, configuration)
    }

    fn close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn srgb_eotf_matches_the_specification_at_known_points() {
        // The piecewise is the spec's own; these literals are computed from it independently
        // of `srgb_eotf`, never by calling it another way.
        assert_eq!(srgb_eotf(0.0), 0.0);
        assert_eq!(srgb_eotf(1.0), 1.0);
        close(srgb_eotf(0.5), 0.21404114);
        // The linear segment's endpoint: 0.04045 / 12.92.
        close(srgb_eotf(0.04045), 0.003130805);
    }

    #[test]
    fn a_linear_claim_passes_mid_grey_through_where_an_srgb_claim_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = encode_png(
            1,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            &[128],
        );
        let linear = decode(
            &dir,
            "grey.png",
            &bytes,
            claims(ImageColorSpace::Linear, ImageAlpha::Opaque),
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        // 128 / 255, unchanged: the authored sample normalized, no transfer.
        close(linear.pixels[0][0], 0.50196078);
        close(linear.pixels[0][1], 0.50196078);
        close(linear.pixels[0][2], 0.50196078);

        let srgb = decode(
            &dir,
            "grey.png",
            &bytes,
            claims(ImageColorSpace::Srgb, ImageAlpha::Opaque),
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        // ((128/255 + 0.055) / 1.055)^2.4, the sRGB EOTF at the same sample.
        close(srgb.pixels[0][0], 0.21586050);
        assert!(
            (linear.pixels[0][0] - srgb.pixels[0][0]).abs() > 0.2,
            "the two claims must diverge, or the transfer is hardcoded"
        );
    }

    #[test]
    fn unclassified_colour_and_alpha_each_refuse_and_name_the_asset() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = encode_png(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[10, 20, 30, 40],
        );
        for (claims, axis) in [
            (
                claims(ImageColorSpace::Unclassified, ImageAlpha::Straight),
                "color_space",
            ),
            (
                claims(ImageColorSpace::Srgb, ImageAlpha::Unclassified),
                "alpha",
            ),
        ] {
            let record = decode_record(claims);
            let id = record.asset_id.to_canonical_string();
            let path = write(&dir, "u.png", &bytes);
            let e = ImageDecoder
                .decode_pixels(&record, &path, DecoderConfiguration::Rgba8Unpremultiplied)
                .unwrap_err();
            assert!(
                matches!(e, AssetError::UnclassifiedImageClaims { .. }),
                "{e}"
            );
            assert_eq!(e.code().as_str(), "asset.unclassified_image_claims");
            assert!(e.to_string().contains(&id), "the asset is named: {e}");
            assert!(
                e.to_string().contains("re-import"),
                "the remedy is stated: {e}"
            );
            let d = e.to_diagnostic();
            assert!(
                d.context.values().any(|v| v == axis),
                "the failing axis is a field: {d:?}"
            );
        }
    }

    #[test]
    fn premultiplication_happens_after_the_transfer_in_linear_space() {
        let dir = tempfile::tempdir().unwrap();
        // Encoded colour 128 and alpha 128: doing the multiply in encoded space would give
        // eotf((128/255)^2) = 0.05166615, a different number from the correct 0.10835351.
        let bytes = encode_png(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[128, 128, 128, 128],
        );
        let srgb_straight = claims(ImageColorSpace::Srgb, ImageAlpha::Straight);

        let straight = decode(
            &dir,
            "p.png",
            &bytes,
            srgb_straight,
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        close(straight.pixels[0][0], 0.21586050);
        close(straight.pixels[0][3], 0.50196078);

        let premul = decode(
            &dir,
            "p.png",
            &bytes,
            srgb_straight,
            DecoderConfiguration::Rgba8Premultiplied,
        )
        .unwrap();
        // rgb_linear * alpha = 0.21586050 * 0.50196078.
        close(premul.pixels[0][0], 0.10835351);
        close(premul.pixels[0][3], 0.50196078);
        assert!(
            (premul.pixels[0][0] - 0.05166615).abs() > 1e-3,
            "premultiplying in encoded space would give 0.05166615, which is wrong"
        );
    }

    #[test]
    fn opaque_forces_alpha_one_even_when_the_png_carries_a_transparent_pixel() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = encode_png(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[200, 100, 50, 0],
        );
        let decoded = decode(
            &dir,
            "o.png",
            &bytes,
            claims(ImageColorSpace::Srgb, ImageAlpha::Opaque),
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        assert_eq!(
            decoded.pixels[0][3], 1.0,
            "the file's zero alpha is overridden"
        );
    }

    /// A real 8x8 Adam7 interlaced grayscale PNG, generated from the grid `value(x,y) =
    /// x*32 + y*4` with correct chunk CRCs. The `png` encoder cannot write interlaced data,
    /// so the fixture is built by hand from the format's own pass layout and embedded.
    const INTERLACED_PNG: [u8; 147] = [
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 8, 0, 0, 0, 8, 8, 0,
        0, 0, 1, 150, 99, 209, 193, 0, 0, 0, 90, 73, 68, 65, 84, 120, 218, 1, 79, 0, 176, 255, 0,
        0, 0, 128, 0, 16, 144, 0, 64, 192, 0, 80, 208, 0, 8, 72, 136, 200, 0, 24, 88, 152, 216, 0,
        32, 96, 160, 224, 0, 40, 104, 168, 232, 0, 48, 112, 176, 240, 0, 56, 120, 184, 248, 0, 4,
        36, 68, 100, 132, 164, 196, 228, 0, 12, 44, 76, 108, 140, 172, 204, 236, 0, 20, 52, 84,
        116, 148, 180, 212, 244, 0, 28, 60, 92, 124, 156, 188, 220, 252, 66, 171, 31, 129, 86, 229,
        79, 82, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    #[test]
    fn an_interlaced_png_decodes_to_the_same_pixels_as_its_progressive_equivalent() {
        let dir = tempfile::tempdir().unwrap();
        // The metadata pass must report the interlace flag WP06 is told to consult.
        let meta = image_metadata(&write(&dir, "i.png", &INTERLACED_PNG)).unwrap();
        assert!(meta.interlaced);

        // The progressive equivalent, encoded from the same grid row-major.
        let mut grid = Vec::new();
        for y in 0..8u32 {
            for x in 0..8u32 {
                grid.push((x * 32 + y * 4) as u8);
            }
        }
        let progressive = encode_png(8, 8, png::ColorType::Grayscale, png::BitDepth::Eight, &grid);
        let linear_opaque = claims(ImageColorSpace::Linear, ImageAlpha::Opaque);

        let interlaced = decode(
            &dir,
            "i.png",
            &INTERLACED_PNG,
            linear_opaque,
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        let flat = decode(
            &dir,
            "f.png",
            &progressive,
            linear_opaque,
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        assert_eq!((interlaced.width, interlaced.height), (8, 8));
        assert_eq!(
            interlaced.pixels, flat.pixels,
            "Adam7 expands to the same raster"
        );
        // Spot-check authored value at (x=3, y=2): 3*32 + 2*4 = 104, linear 104/255.
        close(interlaced.pixels[2 * 8 + 3][0], 104.0 / 255.0);
        close(interlaced.pixels[2 * 8 + 3][3], 1.0);
    }

    #[test]
    fn grayscale_and_paletted_pngs_expand_to_four_components() {
        let dir = tempfile::tempdir().unwrap();
        let linear_opaque = claims(ImageColorSpace::Linear, ImageAlpha::Opaque);

        let grey = encode_png(1, 1, png::ColorType::Grayscale, png::BitDepth::Eight, &[77]);
        let decoded = decode(
            &dir,
            "g.png",
            &grey,
            linear_opaque,
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        assert_eq!(decoded.pixels.len(), 1);
        close(decoded.pixels[0][0], 77.0 / 255.0);
        close(decoded.pixels[0][1], 77.0 / 255.0);
        close(decoded.pixels[0][2], 77.0 / 255.0);
        close(decoded.pixels[0][3], 1.0);

        // Palette entry 1 is (200, 150, 100); the crate resolves the index, not this code.
        let indexed = encode_indexed_png(&[10, 20, 30, 200, 150, 100], &[1]);
        let decoded = decode(
            &dir,
            "p.png",
            &indexed,
            linear_opaque,
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        close(decoded.pixels[0][0], 200.0 / 255.0);
        close(decoded.pixels[0][1], 150.0 / 255.0);
        close(decoded.pixels[0][2], 100.0 / 255.0);
        close(decoded.pixels[0][3], 1.0);
    }

    #[test]
    fn a_sixteen_bit_png_is_reduced_to_eight_bits_before_the_transfer() {
        let dir = tempfile::tempdir().unwrap();
        // 16-bit grayscale sample 0x8000 strips to its high byte 0x80 = 128 before the EOTF.
        let bytes = encode_png(
            1,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::Sixteen,
            &[0x80, 0x00],
        );
        let decoded = decode(
            &dir,
            "16.png",
            &bytes,
            claims(ImageColorSpace::Srgb, ImageAlpha::Opaque),
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        // eotf(128/255), the same value an 8-bit 128 sample gives: the low byte is dropped.
        close(decoded.pixels[0][0], 0.21586050);
    }

    /// A real 4x4 grayscale JPEG (mid-grey 128), encoded by Pillow at quality 95. JPEG is
    /// lossy, so the assertions are structural — dimensions, opaque alpha, luma replicated
    /// across RGB — plus the transfer divergence, not an exact colour oracle.
    const GREY_JPEG: &[u8; 331] = include_bytes!("../tests/fixtures/grey.jpg");

    #[test]
    fn a_jpeg_decodes_to_opaque_rgb_and_honours_the_records_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let linear = decode(
            &dir,
            "g.jpg",
            GREY_JPEG,
            claims(ImageColorSpace::Linear, ImageAlpha::Opaque),
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        assert_eq!((linear.width, linear.height), (4, 4));
        assert_eq!(linear.pixels.len(), 16);
        for pixel in &linear.pixels {
            // Grayscale luma replicates across the three channels; JPEG carries no alpha.
            close(pixel[0], pixel[1]);
            close(pixel[1], pixel[2]);
            assert_eq!(pixel[3], 1.0);
            assert!((0.0..=1.0).contains(&pixel[0]));
        }
        let srgb = decode(
            &dir,
            "g.jpg",
            GREY_JPEG,
            claims(ImageColorSpace::Srgb, ImageAlpha::Opaque),
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
        assert!(
            (linear.pixels[0][0] - srgb.pixels[0][0]).abs() > 0.1,
            "the record's transfer applies to JPEG exactly as to PNG"
        );
    }

    #[test]
    fn an_audio_configuration_is_refused_for_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = encode_png(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[1, 2, 3, 4],
        );
        let e = decode(
            &dir,
            "a.png",
            &bytes,
            claims(ImageColorSpace::Srgb, ImageAlpha::Straight),
            DecoderConfiguration::PcmF32Interleaved,
        )
        .unwrap_err();
        assert!(
            matches!(e, AssetError::IncompatibleDecodeConfiguration { .. }),
            "{e}"
        );
        assert_eq!(e.code().as_str(), "asset.incompatible_decode_configuration");
        assert!(e.to_string().contains("pcm-f32-interleaved"), "{e}");
    }

    #[test]
    fn truncated_and_garbage_bytes_are_typed_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let valid = encode_png(2, 2, png::ColorType::Rgba, png::BitDepth::Eight, &[0u8; 16]);
        let srgb_straight = claims(ImageColorSpace::Srgb, ImageAlpha::Straight);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("truncated png", valid[..20].to_vec()),
            ("png signature only", ImageFormat::Png.signature().to_vec()),
            (
                "garbage jpeg",
                vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x03, 0x08, 0x00, 0x00],
            ),
            ("jpeg signature only", vec![0xFF, 0xD8]),
        ];
        for (label, bytes) in cases {
            let name = if label.contains("jpeg") {
                "bad.jpg"
            } else {
                "bad.png"
            };
            let e = decode(
                &dir,
                name,
                &bytes,
                srgb_straight,
                DecoderConfiguration::Rgba8Unpremultiplied,
            )
            .unwrap_err();
            assert!(
                matches!(e, AssetError::MalformedAsset { .. }),
                "{label}: {e}"
            );
        }
    }
}
