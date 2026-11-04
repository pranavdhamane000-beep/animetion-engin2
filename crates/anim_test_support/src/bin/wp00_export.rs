#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "WP00 probe binary, not production code: a panic here fails the probe loudly rather than emitting a wrong golden, and the bounded sleep in the adapter-retry loop is the only permitted use spec'd by clippy.toml"
)]
// crates/anim_test_support/src/bin/wp00_export.rs
//! Raw-video encode, WAV mux and decoded-count verification
//! (spec 9.3, 9.5 step 8, 9.12 "Decoded export has exact expected video count and
//! declared audio behavior").
//!
//! The countable claim WP09 will make is "N frames in, N frames out". This probe
//! establishes that the claim can be verified from the *output* rather than trusted
//! from the exit status, because FFmpeg does not guarantee the two agree: a
//! truncated rawvideo feed exits 0, silently writes a short file, and prints
//! nothing to stderr.
//!
//! Three scenarios are run. Two must succeed and one must *fail loudly*, so a
//! count assertion that has no teeth cannot pass this probe.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
use anim_test_support::probe::{finish, ProbeError};
use anim_test_support::probe_tools;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;
const FRAME_RATE: u32 = 25;
const BYTES_PER_PIXEL: usize = 3;
/// Frames in the healthy scenario, and what the count check must report.
const EXPECTED_FRAMES: u64 = 10;
/// Frames fed in the truncated scenario.
const TRUNCATED_FRAMES: u64 = 6;
const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u16 = 2;
/// Exactly one second, so frame/audio counts are independently checkable.
const AUDIO_FRAMES: u32 = 48_000;

#[derive(Clone, Debug)]
struct Args {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    manifest: PathBuf,
    work: PathBuf,
    report_dir: PathBuf,
    timeout: Duration,
}

fn main() -> std::process::ExitCode {
    let root = workspace_root();
    let fallback = root.join("probe-out");
    let args = match Args::from_env(&root) {
        Ok(a) => a,
        Err(e) => return finish("wp00_export", vec![e.to_diagnostic()], &fallback),
    };
    match run(&args) {
        Ok(diagnostics) => finish("wp00_export", diagnostics, &args.report_dir),
        Err(e) => finish("wp00_export", vec![e.to_diagnostic()], &args.report_dir),
    }
}

impl Args {
    fn from_env(root: &Path) -> Result<Self, ProbeError> {
        let mut out = Self {
            ffmpeg: root.join("tools/ffmpeg/ffmpeg.exe"),
            ffprobe: root.join("tools/ffmpeg/ffprobe.exe"),
            manifest: root.join("tools/ffmpeg/ffmpeg-manifest.json"),
            work: root.join("probe-out/export-scenario"),
            report_dir: root.join("probe-out"),
            timeout: Duration::from_millis(60_000),
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
                "--work" => out.work = PathBuf::from(value()?),
                "--out" => out.report_dir = PathBuf::from(value()?),
                other => {
                    return Err(ProbeError::Config(format!(
                        "unrecognized argument {other}; expected --ffmpeg, --ffprobe, \
                         --manifest, --work or --out"
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

fn code(raw: &str) -> DiagnosticCode {
    DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
}

/// One decoded stream as counted out of the finished container.
#[derive(Clone, Debug)]
struct CountedStream {
    index: usize,
    codec_type: String,
    codec_name: String,
    nb_read_frames: Option<u64>,
    duration: Option<f64>,
}

fn run(args: &Args) -> Result<Vec<Diagnostic>, ProbeError> {
    let mut diagnostics = Vec::new();

    let ffmpeg_entry = probe_tools::verify_executable(&args.ffmpeg, &args.manifest, "ffmpeg")?;
    let ffprobe_entry = probe_tools::verify_executable(&args.ffprobe, &args.manifest, "ffprobe")?;
    // The capability the probe is about to rely on must be in the build, not
    // assumed from the distribution's name (spec 9.11).
    let capabilities = probe_tools::run(
        &args.ffmpeg,
        &["-hide_banner", "-encoders"],
        None,
        args.timeout,
    )?;
    for codec in ["ffv1", "flac", "pcm_s16le"] {
        if !String::from_utf8_lossy(&capabilities.stdout).contains(codec) {
            return Err(ProbeError::CapabilityAbsent(format!(
                "{codec} is not in the encoder list of {}",
                ffmpeg_entry.version
            )));
        }
    }
    diagnostics.push(Diagnostic::info(
        code("export.capabilities_present"),
        format!(
            "{} declares ffv1, flac and pcm_s16le encoders",
            ffmpeg_entry.version
        ),
    ));

    if args.work.exists() {
        std::fs::remove_dir_all(&args.work)
            .map_err(|e| ProbeError::io("clear export scenario", &args.work, &e))?;
    }
    std::fs::create_dir_all(&args.work)
        .map_err(|e| ProbeError::io("create export scenario", &args.work, &e))?;

    // The audio track is written here rather than vendored as a fixture, because the
    // probe needs to know its exact frame count from the writer, not infer it.
    let read_back = write_reference_wav(&args.work.join("audio.wav"))?;
    diagnostics.push(Diagnostic::info(
        code("export.audio_written"),
        format!(
            "reference WAV: {AUDIO_FRAMES} frames x {AUDIO_CHANNELS} channels at \
             {AUDIO_RATE} Hz, read back as {read_back} frames",
        ),
    ));

    // Scenario 1: the healthy path.
    let full = encode_and_count(
        args,
        &ffmpeg_entry,
        &ffprobe_entry,
        EXPECTED_FRAMES,
        "full",
        &mut diagnostics,
    )?;
    let video = find_stream(&full, "video")?;
    let got_video = video.nb_read_frames.ok_or(ProbeError::CountMismatch {
        expected: EXPECTED_FRAMES,
        got: 0,
    })?;
    if got_video != EXPECTED_FRAMES {
        return Err(ProbeError::CountMismatch {
            expected: EXPECTED_FRAMES,
            got: got_video,
        });
    }
    diagnostics.push(Diagnostic::info(
        code("export.video_count_exact"),
        format!(
            "{EXPECTED_FRAMES} rawvideo frames in, {} counted out of the {} stream in \
             matroska (-count_frames, not the container's absent nb_frames)",
            got_video, video.codec_name
        ),
    ));
    let audio = find_stream(&full, "audio")?;
    // Reference-track fact, measured: the container reports 48 000 audio frames, and `-count_frames` on
    // FLAC-in-Matroska reports 12 with no duration. Those are *packets* of
    // whole-frame granularity, not samples: the container cannot answer "how many
    // audio frames did the render produce". Stating this as a warning rather than
    // a pass is the point - WP09's audio verification has to decode to PCM and
    // count samples, because the count this line seems like it could give is wrong
    // by four orders of magnitude (spec 9.12 "declared audio behavior").
    diagnostics.push(Diagnostic::new(
        code("export.audio_not_container_countable"),
        DiagnosticSeverity::Warning,
        format!(
            "audio stream {} counted {} frames against a reference track of \
             {AUDIO_FRAMES}, and reports duration {}; container frame counting is \
             therefore NOT a usable audio check, and the export verifier must decode \
             to PCM and compare sample counts",
            audio.codec_name,
            audio.nb_read_frames.unwrap_or(0),
            audio
                .duration
                .map_or("absent".to_owned(), |d| d.to_string())
        ),
    ));
    if audio.nb_read_frames == Some(AUDIO_FRAMES as u64) {
        return Err(ProbeError::Config(
            "the audio container count matched the reference frame count exactly; that \
             contradicts the observed behaviour and means this check is now reading \
             something else"
                .to_owned(),
        ));
    }

    // The alternative the warning above points to, executed rather than described:
    // demux the audio out of the finished container into PCM and count it. This is
    // what proves the 48 000 frames survived the mux, and it is the shape WP09's
    // verifier should take.
    let extracted = args.work.join("audio_roundtrip.wav");
    let extract_argv: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-v".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        args.work.join("full.mkv").display().to_string(),
        "-vn".into(),
        "-map".into(),
        "0:a:0".into(),
        "-c:a".into(),
        "pcm_s16le".into(),
        "-fflags".into(),
        "+bitexact".into(),
        "-map_metadata".into(),
        "-1".into(),
        extracted.display().to_string(),
    ];
    let extracted_run = probe_tools::run(&args.ffmpeg, &extract_argv, None, args.timeout)?;
    probe_tools::record_tool_run(
        &mut diagnostics,
        "ffmpeg",
        &ffmpeg_entry,
        &extract_argv,
        &extracted_run,
    );
    if extracted_run.code != Some(0) {
        return Err(ProbeError::ToolNonzeroExit(
            extracted_run.code.unwrap_or(-1),
            extracted_run.last_stderr_line(),
        ));
    }
    let roundtrip = read_wav_frame_count(&extracted)?;
    diagnostics.push(Diagnostic::info(
        code("export.audio_pcm_roundtrip"),
        format!(
            "demuxing the audio out of the finished container and counting PCM frames \
             gives {roundtrip} against a reference of {AUDIO_FRAMES}; this is the \
             verification path that works, not container frame counting"
        ),
    ));
    if roundtrip != AUDIO_FRAMES as u64 {
        return Err(ProbeError::CountMismatch {
            expected: AUDIO_FRAMES as u64,
            got: roundtrip,
        });
    }
    // so the ONLY thing that can catch it is counting the output. If the count comes
    // back equal to what was actually fed, the assertion has teeth; if it comes back
    // as the requested 10, the probe would have been lying.
    // Scenario 2: the truncated feed. FFmpeg is expected to accept it and exit 0,
    // so the ONLY thing that can catch it is counting the output. If the count comes
    // back equal to what was actually fed, the assertion has teeth; if it comes back
    // as the requested 10, the probe would have been lying.
    let short = encode_and_count(
        args,
        &ffmpeg_entry,
        &ffprobe_entry,
        TRUNCATED_FRAMES,
        "truncated",
        &mut diagnostics,
    )?;
    let short_video = find_stream(&short, "video")?;
    match short_video.nb_read_frames {
        Some(n) if n == TRUNCATED_FRAMES => diagnostics.push(Diagnostic::info(
            code("export.truncation_detected"),
            format!(
                "feeding {TRUNCATED_FRAMES} frames while the header advertised a \
                 {WIDTH}x{HEIGHT} stream produced {} counted frames, so a count check \
                 on the output is what distinguishes a short export from a complete one",
                n
            ),
        )),
        Some(n) if n == EXPECTED_FRAMES => {
            return Err(ProbeError::Config(
                "the truncated feed still reported 10 frames; either the encoder \
                 padded to the declared count or the counting is not reading the file \
                 that was just written"
                    .to_owned(),
            ));
        }
        Some(n) => {
            diagnostics.push(Diagnostic::warning(
                code("export.truncation_unexpected_count"),
                format!(
                    "truncated feed produced {n} counted frames rather than \
                     {TRUNCATED_FRAMES}; still not the requested {EXPECTED_FRAMES}, so \
                     a count assertion would catch it"
                ),
            ));
        }
        None => {
            return Err(ProbeError::Config(
                "the truncated scenario reported no frame count at all, so the \
                 detection claim cannot be made"
                    .to_owned(),
            ));
        }
    }

    // Scenario 3: zero-length input. Documented to produce a corrupt file with a
    // clean exit and no stderr, so the only signal is what the output counts to.
    // "Absent" counts as a rejection here: a container whose frame count cannot be
    // read is not a container that holds the expected frames.
    let empty = encode_and_count(
        args,
        &ffmpeg_entry,
        &ffprobe_entry,
        0,
        "empty",
        &mut diagnostics,
    );
    match empty {
        Ok(streams) => {
            let video = find_stream(&streams, "video");
            let counted = video.as_ref().ok().and_then(|s| s.nb_read_frames);
            match counted {
                Some(EXPECTED_FRAMES) | Some(TRUNCATED_FRAMES) => {
                    return Err(ProbeError::Config(format!(
                        "a zero-byte feed was counted as {counted:?} frames; the count \
                         assertion is not sound"
                    )));
                }
                other => diagnostics.push(Diagnostic::info(
                    code("export.empty_rejected"),
                    format!(
                        "a zero-byte feed yields a container whose video frame count is \
                         {other:?} rather than {EXPECTED_FRAMES}{}; a count check on the \
                         output rejects it even though FFmpeg exited successfully",
                        if video.is_err() {
                            " because the container has no video stream at all"
                        } else {
                            ""
                        }
                    ),
                )),
            }
        }
        Err(e) => diagnostics.push(Diagnostic::info(
            code("export.empty_rejected"),
            format!("a zero-byte feed fails outright as {e}, which is also a valid outcome"),
        )),
    }

    // Stream order is part of what a count claim means: if the video and audio
    // indices can swap between runs, "stream 0 has N frames" is not a stable
    // statement (spec 7.1, ordered collections).
    let order: Vec<String> = full
        .iter()
        .map(|s| format!("{}={}", s.index, s.codec_type))
        .collect();
    diagnostics.push(Diagnostic::info(
        code("export.stream_order"),
        format!(
            "the verified output lists streams as {}; WP09 must address them by these \
             indices rather than by scanning",
            order.join(", ")
        ),
    ));

    Ok(diagnostics)
}

/// Deterministic RGB24 frame: a gradient plus a per-frame counter, so a duplicated
/// or dropped frame is detectable in the pixels and not only in the count.
fn frame_bytes(index: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((WIDTH as usize) * (HEIGHT as usize) * BYTES_PER_PIXEL);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            out.push(((x + index) % 256) as u8);
            out.push(((y * 3 + index) % 256) as u8);
            out.push(((x ^ y) % 256) as u8);
        }
    }
    out
}

/// Encode `frames` frames plus the reference WAV into a matroska container, then
/// count what the container actually holds.
fn encode_and_count(
    args: &Args,
    ffmpeg_entry: &probe_tools::ManifestEntry,
    ffprobe_entry: &probe_tools::ManifestEntry,
    frames: u64,
    label: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<CountedStream>, ProbeError> {
    let output = args.work.join(format!("{label}.mkv"));
    let mut payload =
        Vec::with_capacity(frames as usize * (WIDTH as usize * HEIGHT as usize * BYTES_PER_PIXEL));
    for i in 0..frames as u32 {
        payload.extend_from_slice(&frame_bytes(i));
    }

    let argv: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-v".into(),
        "error".into(),
        "-y".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgb24".into(),
        "-s".into(),
        format!("{WIDTH}x{HEIGHT}"),
        "-framerate".into(),
        FRAME_RATE.to_string(),
        "-i".into(),
        "-".into(),
        "-i".into(),
        args.work.join("audio.wav").display().to_string(),
        // Explicit maps: without them FFmpeg picks streams by its own rules and a
        // second run could bind a different pair.
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "1:a:0".into(),
        "-c:v".into(),
        "ffv1".into(),
        "-c:a".into(),
        "flac".into(),
        // Strip the encoder tag and any created timestamp, so two runs of the same
        // input are comparable. (Full byte-determinism is WP09's concern; here the
        // point is that the probe's own evidence does not drift.)
        "-fflags".into(),
        "+bitexact".into(),
        "-map_metadata".into(),
        "-1".into(),
        "-f".into(),
        "matroska".into(),
        output.display().to_string(),
    ];

    let completed = probe_tools::run(
        &args.ffmpeg,
        &argv,
        if payload.is_empty() {
            None
        } else {
            Some(&payload)
        },
        args.timeout,
    )?;
    // A zero-byte feed is given no stdin pipe at all: a closed pipe and an absent one
    // are different conditions and both are worth observing separately.
    probe_tools::record_tool_run(diagnostics, "ffmpeg", ffmpeg_entry, &argv, &completed);
    diagnostics.push(Diagnostic::info(
        code("export.encoded"),
        format!(
            "{label}: {} bytes of rawvideo fed, exit {:?}, output {} bytes, stderr '{}'",
            payload.len(),
            completed.code,
            std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0),
            completed.last_stderr_line()
        ),
    ));
    if !output.exists() {
        return Err(ProbeError::ToolNonzeroExit(
            completed.code.unwrap_or(-1),
            "no output file was created".to_owned(),
        ));
    }

    // Count from the finished file. `-count_frames` is mandatory: matroska carries
    // no nb_frames, so asking for it alone returns an empty object and exit 0, which
    // reads as "zero frames" to anything that treats a missing field as falsy.
    let count_argv: Vec<String> = vec![
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-print_format".into(),
        "json".into(),
        "-show_streams".into(),
        "-count_frames".into(),
        output.display().to_string(),
    ];
    let counted = probe_tools::run(&args.ffprobe, &count_argv, None, args.timeout)?;
    probe_tools::record_tool_run(diagnostics, "ffprobe", ffprobe_entry, &count_argv, &counted);
    parse_streams(&counted.stdout)
}

fn parse_streams(stdout: &[u8]) -> Result<Vec<CountedStream>, ProbeError> {
    let value: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|e| ProbeError::MalformedJson(e.to_string()))?;
    let streams = value
        .get("streams")
        .and_then(|s| s.as_array())
        .ok_or_else(|| ProbeError::MalformedJson("no streams array".to_owned()))?;
    let mut out = Vec::new();
    for s in streams {
        out.push(CountedStream {
            index: s.get("index").and_then(|i| i.as_u64()).unwrap_or(u64::MAX) as usize,
            codec_type: s
                .get("codec_type")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_owned(),
            codec_name: s
                .get("codec_name")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_owned(),
            nb_read_frames: s
                .get("nb_read_frames")
                .and_then(|n| n.as_str())
                .and_then(|n| n.parse().ok()),
            duration: s
                .get("duration")
                .and_then(|d| d.as_str())
                .and_then(|d| d.parse().ok()),
        });
    }
    Ok(out)
}

fn find_stream<'a>(
    streams: &'a [CountedStream],
    codec_type: &str,
) -> Result<&'a CountedStream, ProbeError> {
    streams
        .iter()
        .find(|s| s.codec_type == codec_type)
        .ok_or(ProbeError::MissingStream)
}

/// hound's error type is its own, so it is folded into a `Config` finding rather
/// than wrapped as an I/O error it did not come from.
fn hound_error(action: &str, path: &Path, e: hound::Error) -> ProbeError {
    ProbeError::Config(format!("{action} at {}: {e}", path.display()))
}

/// Write the reference track with hound and read the header back, so the frame count
/// is confirmed by a second reader rather than only asserted by the writer.
///
/// Returns the frame count as read back.
fn write_reference_wav(path: &Path) -> Result<u64, ProbeError> {
    use hound::{SampleFormat, WavReader, WavSpec, WavWriter};

    let spec = WavSpec {
        channels: AUDIO_CHANNELS,
        sample_rate: AUDIO_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer =
        WavWriter::create(path, spec).map_err(|e| hound_error("create reference WAV", path, e))?;
    for frame in 0..AUDIO_FRAMES {
        // A 220 Hz sine at modest amplitude, identical on both channels: channel
        // handling is not what this probe is testing, and a constant pair keeps the
        // expected sample count unambiguous.
        let t = frame as f32 / AUDIO_RATE as f32;
        let sample = ((2.0 * std::f32::consts::PI * 220.0 * t).sin() * 12_000.0) as i16;
        for _ in 0..AUDIO_CHANNELS {
            writer
                .write_sample(sample)
                .map_err(|e| hound_error("write WAV sample", path, e))?;
        }
    }
    writer
        .finalize()
        .map_err(|e| hound_error("finalize WAV", path, e))?;

    let reader = WavReader::open(path).map_err(|e| hound_error("open reference WAV", path, e))?;
    // duration() is frames; len() is total sample values. Reading the wrong one is a
    // factor-of-channels error that would look like a passing count.
    let read_back = u64::from(reader.duration());
    let read_spec = reader.spec();
    if read_spec.channels != AUDIO_CHANNELS || read_spec.sample_rate != AUDIO_RATE {
        return Err(ProbeError::Config(format!(
            "reference WAV header reads back as {} channels at {} Hz, expected \
             {AUDIO_CHANNELS} at {AUDIO_RATE}",
            read_spec.channels, read_spec.sample_rate
        )));
    }
    if u64::from(reader.len()) != read_back * u64::from(AUDIO_CHANNELS) {
        return Err(ProbeError::Config(format!(
            "reference WAV reports {} total samples for {read_back} frames at \
             {AUDIO_CHANNELS} channels",
            reader.len()
        )));
    }
    Ok(read_back)
}

/// Read only the frame count out of a WAV, without holding its samples.
fn read_wav_frame_count(path: &Path) -> Result<u64, ProbeError> {
    use hound::WavReader;
    let reader = WavReader::open(path).map_err(|e| hound_error("open round-trip WAV", path, e))?;
    Ok(u64::from(reader.duration()))
}
