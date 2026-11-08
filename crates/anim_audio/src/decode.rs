//! Source PCM decode and canonical channel/rate metadata (spec 17.3, 17.7).
//!
//! This is the consumer DECISIONS.md #26 named when it left `anim_assets::audio`
//! metadata-only: WP02 reads a WAV header, WP08 reads the samples. The decoder is the
//! pinned `symphonia` 0.6.1 with `default-features = false`, so `opt-simd` is off and its
//! DSP is scalar with no runtime dispatch (DECISIONS.md #46 ruling 4) — the decode half of
//! the portable bit-exact path. The enabled formats are exactly the workspace's five
//! (`wav`, `pcm`, `flac`, `ogg`, `vorbis`); an enabled decoder is a decoder an unexpected
//! stream can reach, so the set stays narrow.
//!
//! The output is interleaved `f32` at the source's *own* rate and channel count. Integer
//! samples are normalized by symphonia's built-in conversion (`i16 / 32768`, etc.), with no
//! dither. Resampling to the canonical 48 kHz is [`crate::resample`]'s job, kept separate so
//! the decode result is a pure function of the bytes and the resampler settings are versioned
//! on their own axis.
//!
//! The decoded frame count is authoritative: it is summed from the buffers the decoder
//! actually produced, never read from the container's `num_frames` hint (BUILD_STATE's WP00
//! finding — audio frame counts are unreadable from the container, so WP09 counts decoded
//! PCM). The content hash is not re-checked here; the resolver verified it under
//! `ResolutionPolicy::Content` before handing over the path, and `expected_hash` is carried
//! only as the decode-layer identity in errors.

use std::path::Path;

use anim_assets::ContentHash;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use crate::AudioError;

/// One decoded source as interleaved `f32` at its native rate and channel count.
///
/// The fields are public because [`crate::resample`] consumes them by value and the tests
/// hand-build sources; [`decode_source`] maintains `samples.len() == frames * channels`,
/// and the resampler re-checks it rather than trusting a hand-built value.
#[derive(Clone, Debug, PartialEq)]
pub struct NativePcm {
    /// The source's own sample rate in Hz, before canonical conversion.
    pub rate: u32,
    /// The source's channel count: 1 (mono) or 2 (stereo). Anything else is refused.
    pub channels: u16,
    /// Sample frames decoded, summed from the decoder's buffers (authoritative).
    pub frames: u64,
    /// Interleaved samples, `frames * channels` long, normalized to `[-1, 1]`.
    pub samples: Vec<f32>,
}

/// Decode one stored object to native interleaved `f32` PCM (spec 17.7's source mapping).
///
/// `expected_hash` is the content identity the resolver already verified; it names the
/// asset in any decode error (the resolver enriches it to the logical [`AssetId`] for the
/// operator). The path is trusted to be the verified canonical object path.
///
/// [`AssetId`]: anim_core::id::AssetId
///
/// # Errors
///
/// [`AudioError::DecodeFailure`] when the file cannot be opened, probed, demuxed or decoded;
/// [`AudioError::UnsupportedLayout`] when the source declares a channel count other than 1
/// or 2; [`AudioError::Overflow`] when a count exceeds its width. A malformed packet is
/// skipped (symphonia's `DecodeError` convention) rather than aborting the whole source, but
/// a demux or IO failure aborts — 17.11 forbids silently replacing a source with silence.
pub fn decode_source(path: &Path, expected_hash: ContentHash) -> Result<NativePcm, AudioError> {
    let fail = |detail: String| AudioError::DecodeFailure {
        asset_id: expected_hash.to_hex(),
        detail,
    };

    let file = std::fs::File::open(path).map_err(|e| fail(format!("open: {e}")))?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut hint = Hint::new();
    hint.with_extension(ext);
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| fail(format!("probe: {e}")))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| fail("no audio track".to_owned()))?
        .clone();
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone() else {
        return Err(fail(
            "audio track carries no audio codec parameters".to_owned(),
        ));
    };
    let mut dec = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| fail(format!("instantiate decoder: {e}")))?;

    let rate = params
        .sample_rate
        .ok_or_else(|| fail("no sample rate".to_owned()))?;
    if rate == 0 {
        return Err(fail("zero sample rate".to_owned()));
    }
    let channel_count = params
        .channels
        .ok_or_else(|| fail("no channel layout".to_owned()))?
        .count();
    let channels = u16::try_from(channel_count).map_err(|_| AudioError::Overflow {
        operation: "decoded channel count",
        value: channel_count.to_string(),
    })?;
    // 17.7 names a conversion rule for mono and stereo only; reject the rest until a rule
    // is explicitly added, rather than guessing a down-mix.
    if channels != 1 && channels != 2 {
        return Err(AudioError::UnsupportedLayout { channels });
    }

    let mut samples: Vec<f32> = Vec::new();
    let mut scratch: Vec<f32> = Vec::new();
    let mut frames = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymError::ResetRequired) => {
                // The demuxer changed codec parameters mid-stream; rebuild the decoder from
                // the current default audio track and continue. The declared rate/channel
                // count stay the stream's format — a change there is a malformed source.
                let reset_track = format
                    .default_track(TrackType::Audio)
                    .ok_or_else(|| fail("no audio track on reset".to_owned()))?
                    .clone();
                let Some(CodecParameters::Audio(reset_params)) = reset_track.codec_params.clone()
                else {
                    return Err(fail("no audio codec parameters on reset".to_owned()));
                };
                dec = symphonia::default::get_codecs()
                    .make_audio_decoder(&reset_params, &AudioDecoderOptions::default())
                    .map_err(|e| fail(format!("rebuild decoder: {e}")))?;
                continue;
            }
            Err(e) => return Err(fail(format!("demux: {e}"))),
        };
        if packet.track_id != track.id {
            continue;
        }
        match dec.decode(&packet) {
            Ok(buf) => {
                // `copy_to_vec_interleaved` resizes scratch to exactly this buffer's sample
                // count (it overwrites, never appends), so extend the accumulator from it.
                buf.copy_to_vec_interleaved::<f32>(&mut scratch);
                samples.extend_from_slice(&scratch);
                frames += buf.frames() as u64;
            }
            // A single malformed packet is skipped, symphonia's own convention; the rest of
            // the source still decodes. This is not the silent-silence substitution 17.11
            // forbids — that is replacing a whole missing/failed source, which aborts above.
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(fail(format!("decode: {e}"))),
        }
    }

    let expected_len = (frames as usize)
        .checked_mul(channels as usize)
        .ok_or_else(|| AudioError::Overflow {
            operation: "decoded sample count",
            value: format!("{frames} frames * {channels} channels"),
        })?;
    if samples.len() != expected_len {
        return Err(fail(format!(
            "decoded {} samples, expected {expected_len} ({frames} frames * {channels} channels)",
            samples.len()
        )));
    }
    Ok(NativePcm {
        rate,
        channels,
        frames,
        samples,
    })
}
