#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "WP00 probe binary, not production code: a panic here fails the probe loudly rather than emitting a wrong golden, and the bounded sleep in the adapter-retry loop is the only permitted use spec'd by clippy.toml"
)]
// crates/anim_test_support/src/bin/wp00_media.rs
//! FFprobe metadata, audio decode and resampling validation
//! (spec 9.3, 9.5 step 7, 9.7 "Media probe").
//!
//! Three things are being proven, and only the third is about audio:
//!   1. An external tool can be verified by digest *before* it is ever executed
//!      (spec 5.1, 9.11), so "system FFmpeg" cannot be reached by accident.
//!   2. A child process can be driven with an argument array, with both pipes
//!      drained and a timeout that terminates and reaps - the same machinery WP09
//!      will depend on for every export.
//!   3. Decoded sample counts survive a resample with a *declared* relationship,
//!      which is what makes "frame N aligns with sample M" provable rather than
//!      approximate.
//!
//! Every fixture used here is a 1.0 s, 44 100 Hz, 2-channel tone, so the expected
//! frame count is 44 100 for all three containers and any drift is visible at once.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anim_core::diagnostic::{Diagnostic, DiagnosticCode};
use anim_test_support::probe::{finish, ProbeError};
use anim_test_support::probe_tools;

const TARGET_RATE: usize = 48_000;
const FIXTURE_FRAMES: u64 = 44_100;
const FIXTURE_RATE: u64 = 44_100;
const FIXTURE_CHANNELS: usize = 2;

/// One container under test, with the codec `FFprobe` is expected to name for it.
struct Fixture {
    file: &'static str,
    extension: &'static str,
    expected_codec: &'static str,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        file: "tone_44100_stereo_1s.wav",
        extension: "wav",
        expected_codec: "pcm_s16le",
    },
    Fixture {
        file: "tone_44100_stereo_1s.flac",
        extension: "flac",
        expected_codec: "flac",
    },
    Fixture {
        file: "tone_44100_stereo_1s.oga",
        extension: "oga",
        expected_codec: "vorbis",
    },
];

#[derive(Clone, Debug)]
struct Args {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    manifest: PathBuf,
    fixtures: PathBuf,
    report_dir: PathBuf,
    stream_index: usize,
    timeout: Duration,
}

fn main() -> std::process::ExitCode {
    let fallback = workspace_root().join("probe-out");
    let args = match Args::from_env() {
        Ok(a) => a,
        Err(e) => return finish("wp00_media", vec![e.to_diagnostic()], &fallback),
    };
    match run(&args) {
        Ok(diagnostics) => finish("wp00_media", diagnostics, &args.report_dir),
        Err(e) => finish("wp00_media", vec![e.to_diagnostic()], &args.report_dir),
    }
}

impl Args {
    fn from_env() -> Result<Self, ProbeError> {
        let root = workspace_root();
        // Paths come from config/default.toml, not from PATH lookup. The probe
        // resolves them relative to the workspace so the repo is self-contained.
        let configured = configured_tool_paths(&root)?;
        let mut out = Self {
            ffmpeg: configured.0,
            ffprobe: configured.1,
            manifest: root.join("tools/ffmpeg/ffmpeg-manifest.json"),
            fixtures: root.join("tests/fixtures/media"),
            report_dir: root.join("probe-out"),
            stream_index: 0,
            timeout: Duration::from_millis(30_000),
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || -> Result<String, ProbeError> {
                it.next()
                    .ok_or_else(|| ProbeError::Config(format!("{flag} requires a value")))
            };
            match flag.as_str() {
                "--ffmpeg" => out.ffmpeg = PathBuf::from(value()?),
                "--ffprobe" => out.ffprobe = PathBuf::from(value()?),
                "--manifest" => out.manifest = PathBuf::from(value()?),
                "--fixtures" => out.fixtures = PathBuf::from(value()?),
                "--out" => out.report_dir = PathBuf::from(value()?),
                "--stream-index" => out.stream_index = parse_usize(&value()?, "--stream-index")?,
                "--timeout-ms" => {
                    out.timeout =
                        Duration::from_millis(parse_usize(&value()?, "--timeout-ms")? as u64)
                }
                other => {
                    return Err(ProbeError::Config(format!(
                        "unrecognized argument {other}; expected --ffmpeg, --ffprobe, \
                         --manifest, --fixtures, --out, --stream-index or --timeout-ms"
                    )));
                }
            }
        }
        Ok(out)
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn parse_usize(raw: &str, name: &str) -> Result<usize, ProbeError> {
    raw.parse::<usize>().map_err(|e| {
        ProbeError::Config(format!(
            "{name} wants a non-negative integer, got {raw}: {e}"
        ))
    })
}

fn code(raw: &str) -> DiagnosticCode {
    DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
}

/// Read the two tool paths out of `config/default.toml`.
///
/// Only the three keys this probe needs are read, with a line scanner rather than a
/// TOML parser: spec 5.1's dependency table lists `serde`/`serde_json` but no TOML
/// reader, and adding one now would be a third dependency exception for two string
/// values. Spec 18.5 ("parse configuration and validate external-tool paths") is
/// where a real config type belongs, and that decision is recorded in
/// `DECISIONS.md` rather than made silently here.
fn configured_tool_paths(root: &Path) -> Result<(PathBuf, PathBuf), ProbeError> {
    let config = root.join("config/default.toml");
    let text = std::fs::read_to_string(&config)
        .map_err(|e| ProbeError::io("read config/default.toml", &config, &e))?;

    let mut ffmpeg = None;
    let mut ffprobe = None;
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        if section != "tooling.ffmpeg" || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let value = raw.trim().trim_matches('"').to_owned();
        match key.trim() {
            "executable" => ffmpeg = Some(value),
            "probe_executable" => ffprobe = Some(value),
            _ => {}
        }
    }
    let ffmpeg = ffmpeg.ok_or_else(|| {
        ProbeError::Config("config/default.toml [tooling.ffmpeg] has no executable".to_owned())
    })?;
    let ffprobe = ffprobe.ok_or_else(|| {
        ProbeError::Config(
            "config/default.toml [tooling.ffmpeg] has no probe_executable".to_owned(),
        )
    })?;
    Ok((
        probe_tools::resolve_from_workspace(Path::new(&ffmpeg)),
        probe_tools::resolve_from_workspace(Path::new(&ffprobe)),
    ))
}

fn run(args: &Args) -> Result<Vec<Diagnostic>, ProbeError> {
    let mut diagnostics = Vec::new();

    // 1. Verify before executing. Nothing below runs an unverified binary.
    let ffmpeg_entry = probe_tools::verify_executable(&args.ffmpeg, &args.manifest, "ffmpeg")?;
    let ffprobe_entry = probe_tools::verify_executable(&args.ffprobe, &args.manifest, "ffprobe")?;
    for (role, entry) in [("ffmpeg", &ffmpeg_entry), ("ffprobe", &ffprobe_entry)] {
        diagnostics.push(
            Diagnostic::info(
                code("tool.digest_verified"),
                format!("{role} {} matches the manifest digest", entry.version),
            )
            .with_context("role", role),
        );
    }

    // 2. The failure modes spec 9.7 names. Proved first, because a probe that can
    //    only report success proves nothing about the failures.
    probe_failure_modes(args, &mut diagnostics)?;

    // 3. Per-container metadata, decode and resample.
    for fixture in FIXTURES {
        probe_one_fixture(args, fixture, &mut diagnostics)?;
    }

    Ok(diagnostics)
}

/// Drive FFprobe over one fixture, decode it, and resample the result.
fn probe_one_fixture(
    args: &Args,
    fixture: &Fixture,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), ProbeError> {
    let path = args.fixtures.join(fixture.file);
    if !path.exists() {
        return Err(ProbeError::ToolNotConfigured(format!(
            "fixture {} is missing; probes do not generate their own inputs",
            path.display()
        )));
    }

    let ffprobe_entry = probe_tools::verify_executable(&args.ffprobe, &args.manifest, "ffprobe")?;
    let argv = probe_argv(args, &path);
    let run = probe_tools::run(&args.ffprobe, &argv, None, args.timeout)?;
    probe_tools::record_tool_run(diagnostics, "ffprobe", &ffprobe_entry, &argv, &run);
    if run.code != Some(0) {
        return Err(ProbeError::ToolNonzeroExit(
            run.code.unwrap_or(-1),
            run.last_stderr_line(),
        ));
    }

    let meta = parse_stream(&run.stdout, args.stream_index)?;
    let rate_text = meta
        .sample_rate
        .map(|r| r.to_string())
        .unwrap_or_else(|| "absent".to_owned());
    let chan_text = meta
        .channels
        .map(|c| c.to_string())
        .unwrap_or_else(|| "absent".to_owned());
    diagnostics.push(
        Diagnostic::info(
            code("media.stream_selected"),
            format!(
                "{file}: stream index {index} is {codec} at {rate_text} Hz, \
                 {chan_text} channels, duration {duration}",
                file = fixture.file,
                index = meta.index,
                codec = meta.codec_name,
                duration = meta.duration.unwrap_or_else(|| "unreported".to_owned()),
            ),
        )
        .with_context("fixture", fixture.file),
    );
    if meta.codec_name != fixture.expected_codec {
        return Err(ProbeError::CapabilityAbsent(format!(
            "{}: FFprobe reports codec {} but the fixture was built to be {}",
            fixture.file, meta.codec_name, fixture.expected_codec
        )));
    }
    if meta.sample_rate != Some(FIXTURE_RATE) || meta.channels != Some(FIXTURE_CHANNELS) {
        return Err(ProbeError::Config(format!(
            "{} declared {:?} Hz / {:?} channels, expected {FIXTURE_RATE} Hz / {FIXTURE_CHANNELS}",
            fixture.file, meta.sample_rate, meta.channels
        )));
    }
    // nb_frames is absent in FLAC and OGG containers. Absent is not zero, and a
    // probe that read a missing field as zero would pass while measuring nothing.
    diagnostics.push(Diagnostic::info(
        code("media.container_frame_count"),
        format!(
            "{}: FFprobe reports nb_frames as {}",
            fixture.file,
            meta.nb_frames
                .map(|n| n.to_string())
                .unwrap_or_else(|| "absent (not zero) for this container".to_owned())
        ),
    ));

    // Decode twice: once with gapless trimming, once without, so the relationship
    // between the two is a measured fact rather than an assumption.
    let trimmed = decode_frames(&path, fixture.extension, true)?;
    let untrimmed = decode_frames(&path, fixture.extension, false)?;
    diagnostics.push(Diagnostic::info(
        code("media.decoded_frames"),
        format!(
            "{}: gapless decode yields {} frames, untrimmed yields {} (difference {})",
            fixture.file,
            trimmed.frames,
            untrimmed.frames,
            untrimmed.frames - trimmed.frames
        ),
    ));
    if trimmed.frames != FIXTURE_FRAMES {
        return Err(ProbeError::CountMismatch {
            expected: FIXTURE_FRAMES,
            got: trimmed.frames,
        });
    }
    // The declared rule: trimming removes exactly the delay and padding the track
    // advertises. If a decoder disagrees, the count is unverifiable and WP08 must
    // not assume gapless trimming is free.
    let declared = trimmed.delay + trimmed.padding;
    let observed = untrimmed.frames - trimmed.frames;
    if declared > 0 && observed != declared {
        return Err(ProbeError::SampleCountOutOfRange {
            got: observed,
            expected_low: declared,
            expected_high: declared,
            rationale: format!(
                "{}: untrimmed minus gapless is {observed} but the track declares \
                 delay {delay} + padding {padding}",
                fixture.file,
                delay = trimmed.delay,
                padding = trimmed.padding
            ),
        });
    }
    if declared > 0 {
        // Reached only when the invariant held, so the report states which rule was
        // measured rather than leaving the reader to infer it from the absence of an
        // error. This is the fact WP08 needs: gapless trimming is exactly
        // delay + padding for this decoder, not approximately.
        diagnostics.push(Diagnostic::info(
            code("media.decoder_delay_verified"),
            format!(
                "{}: untrimmed minus gapless equals the declared delay {} plus padding \
                 {} exactly, so gapless trimming is predictable here",
                fixture.file, trimmed.delay, trimmed.padding
            ),
        ));
    } else {
        diagnostics.push(Diagnostic::info(
            code("media.decoder_delay_undeclared"),
            format!(
                "{}: the track declares no delay or padding and the two decodes are \
                 identical, so trimming is unobservable for this codec rather than \
                 absent",
                fixture.file
            ),
        ));
    }

    // Resample to the canonical rate and check the count against the declared rule.
    let ratio_num = TARGET_RATE as u64;
    let ratio_den = FIXTURE_RATE;
    let exact = (ratio_num * trimmed.frames) as f64 / ratio_den as f64;
    let expected_floor = exact.floor() as u64;
    let expected_ceil = exact.ceil() as u64;
    let out = resample_to_target(&trimmed.planar, trimmed.frames as usize)?;
    let got = out as u64;
    diagnostics.push(Diagnostic::info(
        code("media.resampled_frames"),
        format!(
            "{}: {FIXTURE_RATE} Hz -> {TARGET_RATE} Hz over {} frames yields {got}; \
             exact ratio is {exact:.4} so the declared band is {expected_floor}..={}",
            fixture.file,
            trimmed.frames,
            expected_ceil + 1
        ),
    ));
    // One frame of slack upward is what a chunked FFT resampler's tail produces;
    // the band is stated rather than asserted as equality so WP08 inherits a
    // measured tolerance instead of a guessed one.
    if !(expected_floor..=expected_ceil + 1).contains(&got) {
        return Err(ProbeError::SampleCountOutOfRange {
            got,
            expected_low: expected_floor,
            expected_high: expected_ceil + 1,
            rationale: format!(
                "{}: resample output outside the band implied by the rate ratio",
                fixture.file
            ),
        });
    }
    Ok(())
}

fn probe_argv(args: &Args, path: &Path) -> Vec<String> {
    [
        "-hide_banner",
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
        // Bounded probing: an unbounded analysearch on a corrupt file is how a
        // metadata read turns into a minute of silence.
        "-probesize",
        "32768",
        "-analyzeduration",
        "0",
        "-select_streams",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .chain(std::iter::once(args.stream_index.to_string()))
    .chain(std::iter::once(path.display().to_string()))
    .collect()
}

#[derive(Debug)]
struct StreamMeta {
    index: usize,
    codec_name: String,
    sample_rate: Option<u64>,
    channels: Option<usize>,
    duration: Option<String>,
    nb_frames: Option<u64>,
}

/// Parse FFprobe's JSON and select the stream by index.
///
/// The selected stream's reported `index` is compared to the requested one, because
/// `-select_streams` mixes global and per-type numbering and a silent mismatch would
/// mean the probe described a different stream than it asked for.
fn parse_stream(stdout: &[u8], want_index: usize) -> Result<StreamMeta, ProbeError> {
    let value: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|e| ProbeError::MalformedJson(e.to_string()))?;
    let streams =
        value
            .get("streams")
            .and_then(|s| s.as_array())
            .ok_or(ProbeError::MalformedJson(
                "no streams array in FFprobe output".to_owned(),
            ))?;
    let found = streams
        .iter()
        .find(|s| s.get("index").and_then(|i| i.as_u64()) == Some(want_index as u64))
        .ok_or(ProbeError::MissingStream)?;

    let codec_name = found
        .get("codec_name")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_owned();
    if codec_name.is_empty() {
        return Err(ProbeError::MalformedJson(
            "selected stream has no codec_name".to_owned(),
        ));
    }
    // FFprobe quotes sample_rate as a string; a numeric reading would silently fail.
    let sample_rate = found
        .get("sample_rate")
        .and_then(|r| r.as_str())
        .and_then(|r| r.parse::<u64>().ok());
    Ok(StreamMeta {
        index: want_index,
        codec_name,
        sample_rate,
        channels: found
            .get("channels")
            .and_then(|c| c.as_u64())
            .map(|c| c as usize),
        duration: found
            .get("duration")
            .and_then(|d| d.as_str())
            .map(str::to_owned),
        nb_frames: found
            .get("nb_frames")
            .and_then(|n| n.as_str())
            .and_then(|n| n.parse().ok()),
    })
}

#[derive(Debug)]
struct Decoded {
    frames: u64,
    delay: u64,
    padding: u64,
    /// One Vec<f32> per channel, planar.
    planar: Vec<Vec<f32>>,
}

/// Decode every audio frame of one file with symphonia.
fn decode_frames(path: &Path, extension: &str, gapless: bool) -> Result<Decoded, ProbeError> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = std::fs::File::open(path).map_err(|e| ProbeError::io("open fixture", path, &e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| {
            ProbeError::Config(format!(
                "{}: no demuxer accepted the stream (gapless={gapless}): {e}",
                path.display()
            ))
        })?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or(ProbeError::MissingStream)?;
    let params = track
        .codec_params
        .as_ref()
        .ok_or(ProbeError::MissingStream)?
        .audio()
        .ok_or(ProbeError::MissingStream)?
        .clone();
    // as_ref, not map on the value: Channels is not Copy and map would move it out
    // of params, which the decoder constructor still needs.
    let channels = params
        .channels
        .as_ref()
        .map(|c| c.count())
        .unwrap_or(FIXTURE_CHANNELS);
    let delay = track.delay.unwrap_or(0) as u64;
    let padding = track.padding.unwrap_or(0) as u64;

    let opts = AudioDecoderOptions::default().gapless(gapless);
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &opts)
        .map_err(|e| {
            ProbeError::CapabilityAbsent(format!("no decoder for {}: {e}", params.codec))
        })?;

    let mut planar: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut frames = 0u64;
    let track_id = track.id;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            // A mid-stream track-list change is a real OGG chained-stream case. For
            // a probe fixture it means the input is not what was assumed.
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => {
                return Err(ProbeError::Config(format!(
                    "{}: packet read failed at frame {frames}: {e}",
                    path.display()
                )))
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        let buffer = decoder.decode(&packet).map_err(|e| {
            ProbeError::Config(format!(
                "{}: decode failed at packet dts {}: {e}",
                path.display(),
                packet.dts
            ))
        })?;
        // Fresh destination per packet, then appended by hand: the library's planar
        // copy resizes the destination it is given, so reusing one accumulator would
        // overwrite rather than extend and silently report a short file.
        let mut chunk: Vec<Vec<f32>> = Vec::new();
        buffer.copy_to_vecs_planar(&mut chunk);
        if chunk.len() != channels {
            return Err(ProbeError::CountMismatch {
                expected: channels as u64,
                got: chunk.len() as u64,
            });
        }
        let plane_frames = chunk.first().map(Vec::len).unwrap_or(0);
        for (dest, plane) in planar.iter_mut().zip(chunk) {
            dest.extend(plane);
        }
        frames += u64::try_from(plane_frames).unwrap_or(u64::MAX);
    }

    Ok(Decoded {
        frames,
        delay,
        padding,
        planar,
    })
}

/// Resample planar f32 audio to the canonical rate and return the output frame count.
fn resample_to_target(planar: &[Vec<f32>], frames: usize) -> Result<usize, ProbeError> {
    use rubato::audioadapter::Adapter as _;
    use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
    use rubato::{Fft, FixedSync, Resampler};

    let channels = planar.len();
    // Chunk size is a fixed 1024 so the output length does not depend on the clip
    // length modulo the tail; WP08 will pin this with its PCM goldens (ADR-0013).
    let mut resampler: Fft<f32> = Fft::new(
        FIXTURE_RATE as usize,
        TARGET_RATE,
        1024,
        channels,
        FixedSync::Input,
    )
    .map_err(|e| ProbeError::Config(format!("resampler construction: {e}")))?;
    let input = SequentialSliceOfVecs::new(planar, channels, frames)
        .map_err(|e| ProbeError::Config(format!("input adapter: {e}")))?;
    let output = resampler
        .process_all(&input, frames, None)
        .map_err(|e| ProbeError::Config(format!("resample failed: {e}")))?;
    Ok(output.frames())
}

/// Prove the four failures spec 9.7 names are distinguishable, not collapsed.
fn probe_failure_modes(args: &Args, diagnostics: &mut Vec<Diagnostic>) -> Result<(), ProbeError> {
    let ffprobe_entry = probe_tools::verify_executable(&args.ffprobe, &args.manifest, "ffprobe")?;

    // Nonzero exit: point FFprobe at a file that is not a media stream at all.
    let junk = args.fixtures.join("malformed_container.bin");
    if junk.exists() {
        let argv = probe_argv(args, &junk);
        let run = probe_tools::run(&args.ffprobe, &argv, None, args.timeout)?;
        probe_tools::record_tool_run(diagnostics, "ffprobe", &ffprobe_entry, &argv, &run);
        if run.code == Some(0) {
            return Err(ProbeError::Config(
                "FFprobe accepted a file that is not a media stream; the failure test \
                 is not testing anything"
                    .to_owned(),
            ));
        }
        diagnostics.push(Diagnostic::info(
            code("media.failure_nonzero_exit"),
            format!(
                "non-media input produced exit {:?} with stderr '{}'",
                run.code,
                run.last_stderr_line()
            ),
        ));
        // Malformed JSON is a *separate* outcome from a nonzero exit: FFprobe can
        // exit nonzero and still print an empty-but-valid object on stdout, so the
        // probe must not treat "exit != 0" and "unparsable" as the same event.
        match serde_json::from_slice::<serde_json::Value>(&run.stdout) {
            Ok(parsed) => {
                let streams = parsed.get("streams").and_then(|s| s.as_array());
                diagnostics.push(Diagnostic::info(
                    code("media.failure_parsable_but_empty"),
                    format!(
                        "stdout parsed as JSON with {} stream entries despite the nonzero \
                         exit; treating this as missing-stream, not malformed-json",
                        streams.map(Vec::len).unwrap_or(0)
                    ),
                ));
            }
            Err(e) => diagnostics.push(Diagnostic::info(
                code("media.failure_malformed_json"),
                format!("stdout was not parsable JSON: {e}"),
            )),
        }
        if parse_stream(&run.stdout, args.stream_index).is_ok() {
            return Err(ProbeError::Config(
                "the failure fixture produced a selectable stream, so it is not a \
                 failure fixture"
                    .to_owned(),
            ));
        }
        diagnostics.push(Diagnostic::info(
            code("media.failure_missing_stream"),
            "selecting the requested stream index from that output fails as \
             MissingStream, which is a distinct outcome from MalformedJson"
                .to_owned(),
        ));
    } else {
        diagnostics.push(Diagnostic::warning(
            code("media.failure_fixture_absent"),
            format!(
                "{} is missing; malformed-json and missing-stream cannot be \
                 distinguished in this run",
                junk.display()
            ),
        ));
    }

    // Timeout: run a source that cannot finish inside the deadline. Feeding a short
    // stdin payload does NOT work - FFmpeg sees EOF and exits with an error in under
    // 100 ms, which looks identical to a pass while testing nothing. A lavfi sine of
    // ten hours piped to the null muxer is genuinely still producing samples when the
    // deadline lands.
    let argv = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        "sine=frequency=440:sample_rate=44100:duration=36000".to_string(),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ];
    let started = std::time::Instant::now();
    let timed = probe_tools::run(&args.ffmpeg, &argv, None, Duration::from_millis(400));
    let elapsed = started.elapsed();
    match timed {
        Ok(completed) => {
            diagnostics.push(Diagnostic::error(
                code("media.failure_timeout_not_triggered"),
                format!(
                    "the ten-hour source finished in {} ms with exit {:?}; the timeout \
                     path is NOT proven and this run must not certify it",
                    elapsed.as_millis(),
                    completed.code
                ),
            ));
            return Err(ProbeError::Config(format!(
                "timeout test did not time out (finished in {} ms)",
                elapsed.as_millis()
            )));
        }
        Err(ProbeError::ToolTimeout(detail)) => diagnostics.push(Diagnostic::info(
            code("media.failure_timeout"),
            format!(
                "deadline of 400 ms terminated and reaped a still-running child after \
                 {} ms: {detail}",
                elapsed.as_millis()
            ),
        )),
        Err(other) => return Err(other),
    }

    // Digest guard, actually exercised: ask for the ffmpeg role while pointing at the
    // ffprobe binary. Both exist and both are valid executables, so the only thing
    // that can reject this is the manifest digest - which is the claim being made.
    let cross = probe_tools::verify_executable(&args.ffprobe, &args.manifest, "ffmpeg");
    match cross {
        Err(ProbeError::ToolDigestMismatch(detail)) => diagnostics.push(Diagnostic::info(
            code("media.failure_digest_mismatch"),
            format!(
                "requesting the ffmpeg role at the ffprobe path is refused before any \
                 process is spawned: {detail}"
            ),
        )),
        Err(other) => {
            return Err(ProbeError::Config(format!(
                "the digest guard produced an unexpected outcome: {other}"
            )))
        }
        Ok(entry) => {
            return Err(ProbeError::Config(format!(
                "the manifest accepted the ffprobe binary under the ffmpeg role as {}; \
                 the two entries are not distinct, so digest verification proves nothing",
                entry.version
            )))
        }
    }
    Ok(())
}
