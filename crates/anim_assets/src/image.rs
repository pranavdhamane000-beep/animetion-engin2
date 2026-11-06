//! Image metadata, color and alpha decode (spec 11.3, 11.6).
//!
//! Reads the header, not the pixels. The facts a scene needs before it can reference
//! a cel are its dimensions, its bit depth, whether it carries alpha and whether it is
//! interlaced — all of which live in the first few hundred bytes, and none of which
//! require a rasterizer. Spec 5.1's dependency table names no image decoder, and
//! DECISIONS.md #26 records the ruling that pixel decode arrives with its consumer in
//! WP06 rather than as a dependency bought a stage early.
//!
//! Chunk CRCs are *not* verified. This engine's integrity model is the content digest
//! (spec 11.6), which proves the stored object is byte-for-byte what was imported;
//! PNG's per-chunk CRC would catch corruption introduced by the authoring tool before
//! import, which is a different failure with a different owner, and paying a second
//! full pass over every image to cover it is the wrong trade.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::decode::{AssetMetadata, Decoder, CURRENT_IMPORTER_VERSION};
use crate::manifest::AssetKind;
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
}

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

    // A palette or gray image with `tRNS` is semi-transparent. The chunk appears
    // before the first `IDAT`, so scanning until then is enough, and the scan is
    // bounded by chunk size rather than by file size.
    while let Ok((length, chunk_type)) = chunk_header(source) {
        match chunk_type.as_slice() {
            b"IDAT" | b"IEND" => break,
            b"tRNS" => {
                if !read_chunk_payload(source, length, 0)?.is_empty() {
                    has_alpha = true;
                }
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
    })
}

/// Parse a JPEG stream from its `SOI`, then marker segments until a start-of-frame.
pub fn jpeg_metadata<R: Read>(source: &mut R) -> Result<ImageMetadata, String> {
    let mut soi = [0u8; 2];
    read_exact_bounded(source, &mut soi)?;
    if soi != ImageFormat::Jpeg.signature() {
        return Err("signature is not JPEG".to_owned());
    }

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
            });
        }
        // Standalone markers (0x01, RSTn, SOI, EOI) carry no length; anything else
        // must be skipped rather than parsed, so that an APP2 ICC profile of any size
        // cannot be mistaken for a frame header.
        if !matches!(code, 0x01 | 0xD0..=0xD9) {
            scanned = scanned.saturating_add(skip_segment(source)?);
        }
        if scanned > MAX_HEADER_SCAN {
            return Err("no start-of-frame marker within the header budget".to_owned());
        }
    }
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

impl ImageDecoder {
    /// The formats with a header reader in this build.
    #[must_use]
    pub const fn supported_formats() -> &'static [ImageFormat] {
        &[ImageFormat::Png, ImageFormat::Jpeg]
    }
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
        reason = "test assertions only; the production rule is enforced at the workspace level"
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
}
