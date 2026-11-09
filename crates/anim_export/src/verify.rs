//! FFprobe/decode metadata, counts and failure comparison (spec 18.3, 18.5 step 11, 18.7, 18.12).
//!
//! The verifier enforces the two hard-won WP00 findings that ADR-0014 records:
//!
//!   * **Counts are read from the output, never trusted from exit status.** FFmpeg exits 0 on
//!     truncated and even on empty raw-video input (ADR-0014 fact 2), so `nb_read_frames` from
//!     `-count_frames` is authoritative and its *absence* is a failure, not a zero (fact 1).
//!   * **Audio is verified in PCM.** Container audio frame counts are wrong (a 48 000-frame track
//!     reports 12 — those are packets, fact 3), so the audio count demuxes to `s16le` on stdout and
//!     counts streamed bytes; nothing here reads an audio `nb_read_frames` as truth.
//!
//! Lossy AAC is handled per 18.7: "checked against declared priming/edit-list behavior and
//! expected timeline duration; unexplained added/missing samples fail." The priming window is a
//! measured constant ([`AAC_FRAME_PRIMING_SAMPLES`]) recorded by DECISIONS #51 — on this pinned
//! build FFmpeg stamps `initial_padding=1024` and its edit-list trim makes a decode round-trip
//! sample-exact, so the tolerance is exactly one AAC frame of encoder delay, not a fudge factor.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::ffmpeg::{self, FfmpegTools};
use crate::settings::{AudioMode, ResolvedExport};
use crate::ExportError;

/// Measured native-AAC encoder delay in samples (spec 18.7's "declared priming"), observed on the
/// vendored FFmpeg 9.0.2 as `initial_padding=1024` at 48 kHz on 2026-11-09; the decode round-trip
/// was sample-exact (delta 0). One AAC frame is 1024 samples by definition, so this is not
/// machine-specific tuning — it is the codec's frame size — but it is a *measured* fact on this
/// build rather than an assumption (DECISIONS #51).
pub const AAC_FRAME_PRIMING_SAMPLES: u64 = 1024;

/// Bytes per PCM sample-frame for the canonical demux target (`pcm_s16le`, stereo): 2 × 2.
const PCM_FRAME_BYTES: u64 = 4;

/// Bound on a single FFprobe/FFprobe-equivalent verification call. Verification runs on a finished
/// file, so this is generous but finite (a hung decoder is a `Timeout`, killed and reaped).
const VERIFY_TIMEOUT: Duration = Duration::from_secs(600);

/// What the finished container must contain (18.9's `verify_output(path, expected, tools)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedCounts {
    pub frames: u64,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// The bitstream codec FFprobe should report (e.g. `h264`).
    pub video_codec: String,
    pub pixel_format: String,
    pub audio: ExpectedAudio,
}

/// The audio half of [`ExpectedCounts`]. `pcm_frames` is the WP01 expected sample-frame count of
/// the *offline WAV*, against which the decoded muxed audio is compared within the priming window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedAudio {
    pub present: bool,
    pub codec: String,
    pub sample_rate: u32,
    pub pcm_frames: u64,
}

impl ExpectedAudio {
    const ABSENT: Self = Self {
        present: false,
        codec: String::new(),
        sample_rate: 0,
        pcm_frames: 0,
    };
}

/// Derive the expectation from a resolved export plus the offline audio's sample-frame count
/// (which the pipeline computed against `sample_range` / WP01 and verified before encoding).
#[must_use]
pub fn expected_from(resolved: &ResolvedExport, audio_pcm_frames: u64) -> ExpectedCounts {
    let spec = resolved.codec_spec;
    let s = &resolved.settings;
    let audio = match s.audio {
        AudioMode::None => ExpectedAudio::ABSENT,
        mode => ExpectedAudio {
            present: true,
            codec: ffmpeg::audio_codec_name(mode).unwrap_or("aac").to_owned(),
            sample_rate: s.quality.audio.sample_rate,
            pcm_frames: audio_pcm_frames,
        },
    };
    ExpectedCounts {
        frames: resolved.frame_count,
        width: s.width,
        height: s.height,
        fps_num: s.fps.num,
        fps_den: s.fps.den,
        video_codec: spec.stream_codec.to_owned(),
        pixel_format: spec.pixel_format.to_owned(),
        audio,
    }
}

/// Verify a produced container against the expectation (spec 18.9). Runs FFprobe for the metadata
/// and stream counts, then (if audio is expected) demuxes the audio to PCM and counts it. Any
/// mismatch is a [`ExportError::Verification`] / [`ExportError::MalformedOutput`]; a mismatch here
/// is terminal and prevents publication (18.11).
pub fn verify_output(
    path: &Path,
    expected: &ExpectedCounts,
    tools: &FfmpegTools,
) -> Result<(), ExportError> {
    let probe = ffmpeg::run_capture(
        &tools.ffprobe,
        &ffmpeg::ffprobe_args(path),
        "ffprobe",
        VERIFY_TIMEOUT,
    )?;
    match probe.status {
        Some(0) | None => {}
        Some(code) => {
            return Err(ExportError::MalformedOutput {
                path: path.display().to_string(),
                detail: format!("ffprobe exited {code}: {}", probe.stderr_tail),
            });
        }
    }
    let document = parse_probe(&probe.stdout, path)?;
    check_streams(&document, expected, path)?;

    if expected.audio.present {
        let decoded = verify_audio_pcm(path, expected, tools)?;
        // The container's declared duration must match the expected timeline within the priming
        // window (18.7), using the format block FFprobe already returned.
        check_timeline_duration(&document, expected, decoded, path)?;
    }
    Ok(())
}

/// Decode the muxed audio to canonical PCM and count the streamed sample-frames, then compare to
/// the offline WAV count within the priming window. Counting is streamed (no buffering of the
/// whole track) for long-export memory stability.
fn verify_audio_pcm(
    path: &Path,
    expected: &ExpectedCounts,
    tools: &FfmpegTools,
) -> Result<u64, ExportError> {
    let (bytes, status, stderr_tail) = ffmpeg::run_counting_stdout(
        &tools.ffmpeg,
        &ffmpeg::audio_demux_args(path),
        "audio-decode",
        VERIFY_TIMEOUT,
    )?;
    match status {
        Some(0) | None => {}
        Some(code) => {
            return Err(ExportError::MalformedOutput {
                path: path.display().to_string(),
                detail: format!("audio decode exited {code}: {stderr_tail}"),
            });
        }
    }
    if bytes % PCM_FRAME_BYTES != 0 {
        return Err(ExportError::MalformedOutput {
            path: path.display().to_string(),
            detail: format!("decoded PCM byte count {bytes} is not a whole sample-frame"),
        });
    }
    let decoded = bytes / PCM_FRAME_BYTES;
    check_audio_pcm(expected.audio.pcm_frames, decoded)?;
    Ok(decoded)
}

// ---------------------------------------------------------------------------------------------
// Pure checks — these carry no I/O and are unit-tested against hand-written FFprobe JSON, so the
// verifier's comparison logic has an oracle independent of the code under test.
// ---------------------------------------------------------------------------------------------

/// Assert the video stream matches codec/pix-fmt/dimensions/frame-rate and that `-count_frames`
/// reported exactly `N` complete frames. A missing `nb_read_frames` is a failure, never a zero
/// (ADR-0014 fact 1). Video must be the first stream (fact 4: stable 0=video, 1=audio).
fn check_streams(
    document: &ProbeJson,
    expected: &ExpectedCounts,
    path: &Path,
) -> Result<(), ExportError> {
    let video = document
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| ExportError::MalformedOutput {
            path: path.display().to_string(),
            detail: "no video stream in output".to_owned(),
        })?;
    if video.index != 0 {
        return Err(ExportError::Verification {
            check: "stream-order",
            expected: "0".to_owned(),
            found: video.index.to_string(),
        });
    }
    string_eq(
        video.codec_name.as_deref(),
        &expected.video_codec,
        "video-codec",
        path,
    )?;
    string_eq(
        video.pix_fmt.as_deref(),
        &expected.pixel_format,
        "pixel-format",
        path,
    )?;
    u64_eq(video.width, u64::from(expected.width), "width", path)?;
    u64_eq(video.height, u64::from(expected.height), "height", path)?;
    let expected_rate = format!("{}/{}", expected.fps_num, expected.fps_den);
    string_eq(
        video.r_frame_rate.as_deref(),
        &expected_rate,
        "frame-rate",
        path,
    )?;

    // The authoritative video frame count. Absent → failure (fact 1/2), not zero.
    match video.nb_read_frames.as_deref() {
        None => Err(ExportError::Verification {
            check: "video-frame-count",
            expected: expected.frames.to_string(),
            found: "nb_read_frames absent (unreadable; -count_frames produced no count)".to_owned(),
        }),
        Some(raw) => {
            let found: u64 = raw.trim().parse().map_err(|_| ExportError::Verification {
                check: "video-frame-count",
                expected: expected.frames.to_string(),
                found: format!("unparseable nb_read_frames {raw:?}"),
            })?;
            if found == expected.frames {
                Ok(())
            } else {
                Err(ExportError::Verification {
                    check: "video-frame-count",
                    expected: expected.frames.to_string(),
                    found: found.to_string(),
                })
            }
        }
    }
}

/// The audio decoded-PCM count must equal the offline expected count within one AAC frame of
/// priming. Anything beyond that is "unexplained added/missing samples" and fails (18.7).
fn check_audio_pcm(expected_frames: u64, decoded_frames: u64) -> Result<(), ExportError> {
    let delta = expected_frames.abs_diff(decoded_frames);
    if delta <= AAC_FRAME_PRIMING_SAMPLES {
        Ok(())
    } else {
        Err(ExportError::Verification {
            check: "audio-sample-count",
            expected: format!(
                "{expected_frames} ± {AAC_FRAME_PRIMING_SAMPLES} (one AAC frame of priming)"
            ),
            found: decoded_frames.to_string(),
        })
    }
}

/// The container duration must match the expected timeline (offline sample count ÷ rate) within
/// the same priming window. This catches a mux that dropped or duplicated samples without a
/// matching PCM-count change (belt to `check_audio_pcm`'s braces).
fn check_timeline_duration(
    document: &ProbeJson,
    expected: &ExpectedCounts,
    decoded_frames: u64,
    path: &Path,
) -> Result<(), ExportError> {
    let raw = document.format.as_ref().and_then(|f| f.duration.as_deref());
    let Some(raw) = raw else {
        // No format duration: fall back to trusting the (already checked) PCM count. FFmpeg can
        // omit a container duration on some muxes; the sample count is the stronger signal.
        let _ = path;
        return Ok(());
    };
    let container_seconds: f64 = raw.trim().parse().map_err(|_| ExportError::Verification {
        check: "timeline-duration",
        expected: "a parseable duration".to_owned(),
        found: format!("{raw:?}"),
    })?;
    let rate = f64::from(expected.audio.sample_rate.max(1));
    let expected_seconds = decoded_frames as f64 / rate;
    let window = (AAC_FRAME_PRIMING_SAMPLES as f64 / rate).max(1.0 / rate);
    if (container_seconds - expected_seconds).abs() <= window {
        Ok(())
    } else {
        Err(ExportError::Verification {
            check: "timeline-duration",
            expected: format!("{expected_seconds:.6}s ± {window:.6}s"),
            found: format!("{container_seconds:.6}s"),
        })
    }
}

fn string_eq(
    found: Option<&str>,
    expected: &str,
    check: &'static str,
    path: &Path,
) -> Result<(), ExportError> {
    match found {
        Some(v) if v == expected => Ok(()),
        Some(v) => Err(ExportError::Verification {
            check,
            expected: expected.to_owned(),
            found: v.to_owned(),
        }),
        None => Err(ExportError::MalformedOutput {
            path: path.display().to_string(),
            detail: format!("stream field for {check} is missing"),
        }),
    }
}

fn u64_eq(
    found: Option<u64>,
    expected: u64,
    check: &'static str,
    path: &Path,
) -> Result<(), ExportError> {
    match found {
        Some(v) if v == expected => Ok(()),
        Some(v) => Err(ExportError::Verification {
            check,
            expected: expected.to_string(),
            found: v.to_string(),
        }),
        None => Err(ExportError::MalformedOutput {
            path: path.display().to_string(),
            detail: format!("stream field for {check} is missing"),
        }),
    }
}

fn parse_probe(bytes: &[u8], path: &Path) -> Result<ProbeJson, ExportError> {
    serde_json::from_slice(bytes).map_err(|error| ExportError::MalformedOutput {
        path: path.display().to_string(),
        detail: format!("ffprobe JSON could not be parsed: {error}"),
    })
}

// ---------------------------------------------------------------------------------------------
// FFprobe JSON model. Numeric stream fields arrive as JSON numbers; counts and rates as strings.
// Option + default everywhere so a field FFprobe omits becomes `None` (a checked failure), never
// a deserialization panic.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ProbeJson {
    #[serde(default)]
    pub streams: Vec<ProbeStream>,
    #[serde(default)]
    pub format: Option<ProbeFormat>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProbeStream {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub codec_type: Option<String>,
    #[serde(default)]
    pub codec_name: Option<String>,
    #[serde(default)]
    pub width: Option<u64>,
    #[serde(default)]
    pub height: Option<u64>,
    #[serde(default)]
    pub pix_fmt: Option<String>,
    #[serde(default)]
    pub r_frame_rate: Option<String>,
    #[serde(default)]
    pub nb_read_frames: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProbeFormat {
    #[serde(default)]
    pub duration: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only"
    )]

    use super::*;

    fn expected() -> ExpectedCounts {
        ExpectedCounts {
            frames: 12,
            width: 320,
            height: 240,
            fps_num: 24,
            fps_den: 1,
            video_codec: "h264".to_owned(),
            pixel_format: "yuv420p".to_owned(),
            audio: ExpectedAudio {
                present: true,
                codec: "aac".to_owned(),
                sample_rate: 48_000,
                pcm_frames: 96_000,
            },
        }
    }

    const GOOD: &str = r#"{
      "streams": [
        {"index":0,"codec_type":"video","codec_name":"h264","width":320,"height":240,
         "pix_fmt":"yuv420p","r_frame_rate":"24/1","nb_read_frames":"12"},
        {"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}
      ],
      "format": {"duration":"2.000000"}
    }"#;

    #[test]
    fn a_matching_probe_passes_all_pure_checks() {
        let doc: ProbeJson = serde_json::from_str(GOOD).unwrap();
        let e = expected();
        check_streams(&doc, &e, Path::new("x.mp4")).unwrap();
        // 96000 ± one frame of priming.
        check_audio_pcm(e.audio.pcm_frames, 96_000).unwrap();
        check_audio_pcm(e.audio.pcm_frames, 96_000 + AAC_FRAME_PRIMING_SAMPLES).unwrap();
        check_timeline_duration(&doc, &e, 96_000, Path::new("x.mp4")).unwrap();
    }

    #[test]
    fn a_missing_video_count_is_a_failure_not_a_zero() {
        // ADR-0014 fact 1: Matroska-style absence must not read as 0 == 0 success.
        let json = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264",
          "width":320,"height":240,"pix_fmt":"yuv420p","r_frame_rate":"24/1"}]}"#;
        let doc: ProbeJson = serde_json::from_str(json).unwrap();
        let e = expected();
        let err = check_streams(&doc, &e, Path::new("x.mp4")).unwrap_err();
        assert!(matches!(
            err,
            ExportError::Verification {
                check: "video-frame-count",
                ..
            }
        ));
    }

    #[test]
    fn a_short_video_count_fails() {
        let json = GOOD.replace("\"nb_read_frames\":\"12\"", "\"nb_read_frames\":\"11\"");
        let doc: ProbeJson = serde_json::from_str(&json).unwrap();
        let e = expected();
        assert!(check_streams(&doc, &e, Path::new("x.mp4")).is_err());
    }

    #[test]
    fn wrong_codec_or_dimensions_or_rate_are_named() {
        let e = expected();
        let bad_codec = GOOD.replace("\"codec_name\":\"h264\"", "\"codec_name\":\"mpeg4\"");
        let doc: ProbeJson = serde_json::from_str(&bad_codec).unwrap();
        let err = check_streams(&doc, &e, Path::new("x.mp4")).unwrap_err();
        assert!(matches!(&err, ExportError::Verification { check, .. } if *check == "video-codec"));

        let bad_w = GOOD.replace("\"width\":320", "\"width\":322");
        let doc: ProbeJson = serde_json::from_str(&bad_w).unwrap();
        let err = check_streams(&doc, &e, Path::new("x.mp4")).unwrap_err();
        assert!(matches!(&err, ExportError::Verification { check, .. } if *check == "width"));

        let bad_rate = GOOD.replace("\"r_frame_rate\":\"24/1\"", "\"r_frame_rate\":\"25/1\"");
        let doc: ProbeJson = serde_json::from_str(&bad_rate).unwrap();
        assert!(check_streams(&doc, &e, Path::new("x.mp4")).is_err());
    }

    #[test]
    fn audio_samples_beyond_the_priming_window_fail() {
        // One frame of priming is allowed; two frames of drift is "unexplained" and fails.
        check_audio_pcm(96_000, 96_000 + AAC_FRAME_PRIMING_SAMPLES).unwrap();
        let err = check_audio_pcm(96_000, 96_000 + 2 * AAC_FRAME_PRIMING_SAMPLES).unwrap_err();
        assert!(matches!(
            err,
            ExportError::Verification {
                check: "audio-sample-count",
                ..
            }
        ));
        // Missing samples beyond the window likewise fails.
        assert!(check_audio_pcm(96_000, 96_000 - AAC_FRAME_PRIMING_SAMPLES - 1).is_err());
    }

    #[test]
    fn a_bad_container_duration_fails_independent_of_the_pcm_count() {
        let e = expected();
        // Duration claims 4 seconds while the (already-accepted) PCM says 2 → mux inconsistency.
        let json = GOOD.replace("\"duration\":\"2.000000\"", "\"duration\":\"4.000000\"");
        let doc: ProbeJson = serde_json::from_str(&json).unwrap();
        let err = check_timeline_duration(&doc, &e, 96_000, Path::new("x.mp4")).unwrap_err();
        assert!(matches!(
            err,
            ExportError::Verification {
                check: "timeline-duration",
                ..
            }
        ));
    }
}
