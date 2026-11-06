//! PCM metadata and source decoding (spec 11.3, 11.6).
//!
//! The WAV container is read with `hound`, which spec 5.1 already pins for
//! "Canonical WAV I/O", so this file adds no dependency and states the facts WP08's
//! resampler needs before it decodes anything: rate, channel count, sample width,
//! frame count and whether the payload is PCM at all.
//!
//! Sample *values* are not decoded here. Spec 11.12's WP02 checks are all about the
//! store and the manifest, and the first consumer of PCM is WP08's offline mix, which
//! also owns the resampler settings — decoding to f32 now would produce an array
//! nothing reads this package, and spec 5.1 pins `symphonia` for the OGG and FLAC
//! paths WP08 has to take anyway (DECISIONS.md #26).
//!
//! `hound`'s own units are a trap worth naming once: `WavReader::duration()` is
//! *frames* (it divides by the channel count) while `len()` is the total number of
//! sample values. This file reads `len()` and divides itself, so that a container
//! whose sample count is not a whole number of frames is a reported defect rather
//! than a silently truncated division.

use std::path::Path;

use hound::{SampleFormat as HoundFormat, WavReader, WavSpec};

use crate::decode::{AssetMetadata, Decoder, CURRENT_IMPORTER_VERSION};
use crate::manifest::AssetKind;
use crate::AssetError;

/// The sample encoding one WAV file states, from its format tag and its width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SampleFormat {
    /// Integer PCM of `bits` per sample: 8, 16, 24 and 32 are the widths a WAV
    /// header normally carries.
    Pcm { bits: u16 },
    /// IEEE float of `bits` per sample, which `WAVE_FORMAT_IEEE_FLOAT` means.
    Float { bits: u16 },
    /// A width the container states but no consumer here names — 20-bit PCM, or a
    /// float size that is not 32. Reported rather than rounded to its nearest known
    /// width, because rounding changes what a sample means.
    Other { bits: u16, float: bool },
}

impl SampleFormat {
    #[must_use]
    pub const fn bits_per_sample(self) -> u16 {
        match self {
            Self::Pcm { bits } | Self::Float { bits } | Self::Other { bits, .. } => bits,
        }
    }

    /// Whether samples of this format can reach WP08's mixer without a codec in the
    /// way. `Other` counts as decodable in principle (it is still linear PCM or
    /// float) but is reported distinctly so that a surprise width is visible in the
    /// metadata rather than discovered in a mix.
    #[must_use]
    pub const fn is_linear(self) -> bool {
        matches!(self, Self::Pcm { .. } | Self::Float { .. })
    }

    /// The spelling a diagnostic, a cache key or a report uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pcm { bits: 8 } => "pcm-s8",
            Self::Pcm { bits: 16 } => "pcm-s16",
            Self::Pcm { bits: 24 } => "pcm-s24",
            Self::Pcm { bits: 32 } => "pcm-s32",
            Self::Float { bits: 32 } => "f32",
            Self::Float { .. } | Self::Pcm { .. } | Self::Other { .. } => "other",
        }
    }

    /// Derive the encoding from one `hound` spec.
    #[must_use]
    const fn from_spec(spec: &WavSpec) -> Self {
        match (spec.sample_format, spec.bits_per_sample) {
            (HoundFormat::Int, bits @ (8 | 16 | 24 | 32)) => Self::Pcm { bits },
            (HoundFormat::Float, 32) => Self::Float { bits: 32 },
            (HoundFormat::Int, bits) => Self::Other { bits, float: false },
            (HoundFormat::Float, bits) => Self::Other { bits, float: true },
        }
    }
}

impl std::fmt::Display for SampleFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pcm { bits } => write!(f, "pcm-s{bits}"),
            Self::Float { bits } => write!(f, "float{bits}"),
            Self::Other { bits, float } => {
                write!(f, "other-{}{bits}", if *float { "f" } else { "s" })
            }
        }
    }
}

/// The format-level facts of one stored WAV object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavMetadata {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
    /// Sample frames: one sample per channel.
    pub frames: u64,
    /// Sample values across all channels, which is `frames × channels` and is what a
    /// byte length is computed from.
    pub total_samples: u64,
}

impl WavMetadata {
    /// The interleaved byte length the PCM payload occupies.
    ///
    /// WP09's expected-PCM-length check (spec 18.5) wants this figure derived from
    /// the header rather than from a decode, which is also what makes it checkable
    /// against the object's `byte_length` minus its chunk framing.
    #[must_use]
    pub const fn pcm_byte_length(&self) -> Option<u64> {
        // Only a byte-aligned width has a byte length: 12-bit PCM stores two samples
        // in three bytes, so claiming `bits / 8` would state a figure that no read
        // offset can reproduce.
        let bits = self.format.bits_per_sample();
        if bits == 0 || !bits.is_multiple_of(8) {
            return None;
        }
        self.total_samples.checked_mul((bits / 8) as u64)
    }
}

/// Read one stored WAV object's metadata.
///
/// The path is the store's canonical object path; a file that is not a WAV, or is a
/// WAV of a shape no reader here claims, is reported with the reason.
pub fn wav_metadata(path: &Path) -> Result<WavMetadata, AssetError> {
    let reader = WavReader::open(path).map_err(|e| match e {
        hound::Error::IoError(io) => crate::io_error("read wav", path, io.to_string()),
        hound::Error::Unsupported => AssetError::UnsupportedDecoder {
            kind: AssetKind::Audio.to_string(),
            importer_version: CURRENT_IMPORTER_VERSION,
        },
        other => AssetError::MalformedAsset {
            asset_id: String::new(),
            kind: AssetKind::Audio.to_string(),
            detail: format!("{}: {other}", path.display()),
        },
    })?;
    let spec = reader.spec();
    if spec.channels == 0 {
        return Err(malformed(path, "zero channels".to_owned()));
    }
    let total_samples = u64::from(reader.len());
    let channels = u64::from(spec.channels);
    // `frames` is exact or the container is corrupt: a half frame is not a thing a
    // planar or interleaved decode can produce.
    if !total_samples.is_multiple_of(channels) {
        return Err(malformed(
            path,
            format!("{total_samples} samples is not a whole number of {channels}-channel frames"),
        ));
    }
    Ok(WavMetadata {
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        format: SampleFormat::from_spec(&spec),
        frames: total_samples / channels,
        total_samples,
    })
}

fn malformed(path: &Path, detail: String) -> AssetError {
    AssetError::MalformedAsset {
        asset_id: String::new(),
        kind: AssetKind::Audio.to_string(),
        detail: format!("{}: {detail}", path.display()),
    }
}

/// The audio kind's metadata reader.
#[derive(Clone, Copy, Debug, Default)]
pub struct WavDecoder;

impl WavDecoder {
    /// The widths this build can describe and whose samples WP08 can reach directly.
    ///
    /// Listed here so that a capability question has one answer instead of a match
    /// arm copied into each caller.
    #[must_use]
    pub const fn supported_formats() -> &'static [SampleFormat] {
        &[
            SampleFormat::Pcm { bits: 8 },
            SampleFormat::Pcm { bits: 16 },
            SampleFormat::Pcm { bits: 24 },
            SampleFormat::Pcm { bits: 32 },
            SampleFormat::Float { bits: 32 },
        ]
    }
}

impl Decoder for WavDecoder {
    fn kind(&self) -> AssetKind {
        AssetKind::Audio
    }

    fn supported_versions(&self) -> &[u32] {
        &[CURRENT_IMPORTER_VERSION]
    }

    fn read_metadata(&self, path: &Path) -> Result<AssetMetadata, AssetError> {
        Ok(AssetMetadata::Audio(wav_metadata(path)?))
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
    use hound::{WavSpec, WavWriter};

    /// Write a real WAV file with `frames` frames of interleaved integers.
    fn wav(
        dir: &Path,
        name: &str,
        sample_rate: u32,
        channels: u16,
        frames: usize,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut writer = WavWriter::create(
            &path,
            WavSpec {
                sample_rate,
                channels,
                bits_per_sample: 16,
                sample_format: HoundFormat::Int,
            },
        )
        .unwrap();
        for frame in 0..frames {
            for channel in 0..channels {
                writer
                    .write_sample((frame * channel as usize) as i16)
                    .unwrap();
            }
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn a_real_wav_reports_the_facts_wp08_plans_on() {
        let dir = tempfile::tempdir().unwrap();
        let meta = wav_metadata(&wav(dir.path(), "cue.wav", 48_000, 2, 1000)).unwrap();
        assert_eq!(meta.sample_rate, 48_000);
        assert_eq!(meta.channels, 2);
        assert_eq!(meta.format, SampleFormat::Pcm { bits: 16 });
        assert_eq!(meta.frames, 1000, "one sample per channel per frame");
        assert_eq!(meta.total_samples, 2000);
        assert_eq!(meta.pcm_byte_length(), Some(2000 * 2));
        assert_eq!(meta.format.bits_per_sample(), 16);
        assert!(meta.format.is_linear());
    }

    #[test]
    fn mono_and_multi_channel_frames_are_divided_by_the_channel_count() {
        let dir = tempfile::tempdir().unwrap();
        for channels in [1u16, 2, 6] {
            let meta = wav_metadata(&wav(dir.path(), "c.wav", 44_100, channels, 480)).unwrap();
            assert_eq!(meta.frames, 480, "{channels} channels");
            assert_eq!(meta.total_samples, 480 * u64::from(channels));
            assert_eq!(meta.pcm_byte_length(), Some(480 * u64::from(channels) * 2));
        }
    }

    #[test]
    fn a_float_wav_is_not_mistaken_for_integer_pcm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.wav");
        let mut writer = WavWriter::create(
            &path,
            WavSpec {
                sample_rate: 48_000,
                channels: 1,
                bits_per_sample: 32,
                sample_format: HoundFormat::Float,
            },
        )
        .unwrap();
        for i in 0..10u8 {
            writer.write_sample(f32::from(i) / 20.0).unwrap();
        }
        writer.finalize().unwrap();
        let meta = wav_metadata(&path).unwrap();
        assert_eq!(meta.format, SampleFormat::Float { bits: 32 });
        assert_eq!(meta.frames, 10);
        assert_eq!(meta.pcm_byte_length(), Some(40));
    }

    #[test]
    fn an_unusual_width_is_reported_not_rounded() {
        // 24-bit is a normal width; a 12-bit integer stream is not, and the mapping
        // must say so instead of calling it 16. Tested on the spec struct directly,
        // because hand-writing a 12-bit container would test `hound`'s writer rather
        // than this rule.
        let spec = WavSpec {
            sample_rate: 8_000,
            channels: 1,
            bits_per_sample: 12,
            sample_format: HoundFormat::Int,
        };
        let format = SampleFormat::from_spec(&spec);
        assert_eq!(
            format,
            SampleFormat::Other {
                bits: 12,
                float: false
            }
        );
        assert!(!format.is_linear());
        assert_eq!(format.bits_per_sample(), 12);
        assert_eq!(format.as_str(), "other");
        assert_eq!(format.to_string(), "other-s12");
        // 12 bits is not byte-aligned, so no byte length is claimed at all.
        assert_eq!(
            WavMetadata {
                sample_rate: 8_000,
                channels: 1,
                format,
                frames: 4,
                total_samples: 4,
            }
            .pcm_byte_length(),
            None
        );
        // A float width that is not 32 is likewise reported, not rounded.
        assert_eq!(
            SampleFormat::from_spec(&WavSpec {
                sample_rate: 8_000,
                channels: 1,
                bits_per_sample: 64,
                sample_format: HoundFormat::Float,
            }),
            SampleFormat::Other {
                bits: 64,
                float: true
            }
        );
        // And the four widths WP08 expects are named by themselves.
        for bits in [8u16, 16, 24, 32] {
            assert_eq!(
                SampleFormat::from_spec(&WavSpec {
                    sample_rate: 8_000,
                    channels: 1,
                    bits_per_sample: bits,
                    sample_format: HoundFormat::Int,
                }),
                SampleFormat::Pcm { bits }
            );
        }
    }

    #[test]
    fn an_empty_payload_is_zero_frames_not_an_error() {
        // A silent clip is a legitimate asset; refusing it would make an empty take
        // unimportable.
        let dir = tempfile::tempdir().unwrap();
        let meta = wav_metadata(&wav(dir.path(), "empty.wav", 48_000, 2, 0)).unwrap();
        assert_eq!(meta.frames, 0);
        assert_eq!(meta.total_samples, 0);
        assert_eq!(meta.pcm_byte_length(), Some(0));
    }

    #[test]
    fn format_spellings_are_unique_per_width() {
        let mut names: Vec<String> = WavDecoder::supported_formats()
            .iter()
            .map(|f| f.to_string())
            .collect();
        names.push(
            SampleFormat::Other {
                bits: 20,
                float: false,
            }
            .to_string(),
        );
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "one spelling per format: {names:?}");
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn a_non_wav_file_is_a_malformed_asset_not_a_missing_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.wav");
        std::fs::write(&path, b"not a riff container at all").unwrap();
        let e = wav_metadata(&path).unwrap_err();
        assert!(matches!(e, AssetError::MalformedAsset { .. }), "{e}");
        assert!(e.to_string().contains("audio"), "{e}");
        assert_eq!(e.code().as_str(), "asset.malformed_asset");
    }

    #[test]
    fn a_truncated_header_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let full = wav(dir.path(), "t.wav", 48_000, 2, 100);
        let mut bytes = std::fs::read(&full).unwrap();
        bytes.truncate(20);
        let path = dir.path().join("trunc.wav");
        std::fs::write(&path, &bytes).unwrap();
        let e = wav_metadata(&path).unwrap_err();
        assert!(
            matches!(e, AssetError::MalformedAsset { .. } | AssetError::Io { .. }),
            "{e}"
        );
    }

    #[test]
    fn a_missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let e = wav_metadata(&dir.path().join("absent.wav")).unwrap_err();
        assert!(matches!(e, AssetError::Io { .. }), "{e}");
    }

    #[test]
    fn decoder_identity_is_the_audio_kind_at_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let decoder = WavDecoder;
        assert_eq!(decoder.kind(), AssetKind::Audio);
        assert_eq!(decoder.supported_versions(), &[CURRENT_IMPORTER_VERSION]);
        let meta = decoder
            .read_metadata(&wav(dir.path(), "d.wav", 48_000, 1, 10))
            .unwrap();
        assert_eq!(meta.kind(), AssetKind::Audio);
        match meta {
            AssetMetadata::Audio(wav) => assert_eq!(wav.frames, 10),
            other => panic!("expected audio metadata, got {other:?}"),
        }
    }
}
