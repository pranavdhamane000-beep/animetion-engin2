//! Executable verification, argument arrays and the FFmpeg child-process lifecycle
//! (spec 18.3, 18.5, 18.7, 18.11).
//!
//! Three responsibilities, each with a determinism or safety rule attached:
//!
//!   1. **Tool integrity** — resolve the FFmpeg/FFprobe paths from an operator-supplied explicit
//!      path (never a `PATH` search, spec 9.11), hash each at its path with SHA-256 and compare to
//!      `tools/ffmpeg/ffmpeg-manifest.json` *before* spawning (ADR-0014). A digest or size
//!      mismatch, a missing file, or the wrong binary under a role is [`ExportError::ToolIntegrity`]
//!      and there is no fallback to try.
//!   2. **Argument arrays** — 18.7 "Construct a versioned argument array. Raw input specifies
//!      rawvideo, rgba, exact width/height and fps_num/fps_den; input comes from `pipe:0` … Never
//!      use shell string interpolation." The builders return `Vec<String>` fed to
//!      `std::process::Command::args`; a shell is never involved (spec 9.4).
//!   3. **Child process** — spawn without a shell, feed raw frames on stdin from another thread,
//!      and drain both stdout and stderr on their own threads so a stalled or chatty FFmpeg can
//!      never fill a pipe (18.7 "Stderr is drained into a bounded tail buffer"). A deadline kills
//!      and reaps on overrun (18.11).
//!
//! The wall-clock (`Instant`) and bounded `sleep` here are operational only — a process deadline,
//! never a value that reaches canonical bytes — which is the carve-out the workspace lint comment
//! states for probe and preview wait loops. `StreamingChild::finish`'s exit code and stderr tail
//! go into typed errors, so a wrong FFmpeg can only ever prevent publication, never shift it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::settings::{AudioMode, QualityProfile, RateControl, ResolvedExport};
use crate::ExportError;

/// The manifest schema version this reader understands (`tools/ffmpeg/ffmpeg-manifest.json`
/// carries `schema_version: 1`, ADR-0014).
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Cap on the FFmpeg stderr retained per process (18.7's "bounded tail buffer"): the reader drains
/// everything so the pipe never fills, but keeps only the last `STDERR_TAIL_BYTES`. Matches the
/// WP00 probe's own bound so a long noisy encode cannot grow memory.
pub const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Poll interval of the bounded wait loop.
const WAIT_INTERVAL: Duration = Duration::from_millis(20);

/// One `executables[]` manifest entry. Unknown sibling fields are ignored; a missing required one
/// is a deserialization error surfaced as `ToolIntegrity`.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestEntry {
    pub role: String,
    pub relative_path: String,
    pub sha256: String,
    pub version: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub distribution: Option<String>,
}

/// The parsed pinned-manifest document (only the fields export needs).
#[derive(Debug, Clone, Deserialize)]
pub struct FfmpegManifest {
    pub schema_version: u32,
    pub executables: Vec<ManifestEntry>,
}

impl FfmpegManifest {
    /// Read and parse the manifest, refusing a schema version this build does not understand.
    pub fn load(path: &Path) -> Result<Self, ExportError> {
        let text = std::fs::read_to_string(path).map_err(|error| ExportError::ToolIntegrity {
            role: "manifest".to_owned(),
            detail: format!("cannot read {}: {error}", path.display()),
        })?;
        let manifest: Self =
            serde_json::from_str(&text).map_err(|error| ExportError::ToolIntegrity {
                role: "manifest".to_owned(),
                detail: format!("malformed {}: {error}", path.display()),
            })?;
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ExportError::ToolIntegrity {
                role: "manifest".to_owned(),
                detail: format!(
                    "manifest schema_version {} is not version {MANIFEST_SCHEMA_VERSION}",
                    manifest.schema_version
                ),
            });
        }
        Ok(manifest)
    }

    #[must_use]
    pub fn entry(&self, role: &str) -> Option<&ManifestEntry> {
        self.executables.iter().find(|e| e.role == role)
    }
}

/// The verified FFmpeg + FFprobe pair, with the manifest entries that proved them. Constructing
/// one *is* the pre-spawn integrity check, so holding it means the binaries are the pinned ones.
#[derive(Debug, Clone)]
pub struct FfmpegTools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub ffmpeg_entry: ManifestEntry,
    pub ffprobe_entry: ManifestEntry,
}

/// Streaming SHA-256 of a file as lowercase hex. 105 MB binaries are hashed in 64 KiB chunks,
/// never loaded whole. SHA-256 (not blake3) is deliberate: spec 5.1 names it for executable
/// integrity and an operator must reproduce the digest with a standard tool (ADR-0014).
pub fn sha256_hex(path: &Path) -> Result<String, ExportError> {
    let mut file = std::fs::File::open(path).map_err(|error| ExportError::ToolIntegrity {
        role: role_of(path),
        detail: format!("cannot open {}: {error}", path.display()),
    })?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|error| ExportError::ToolIntegrity {
                role: role_of(path),
                detail: format!("read error on {}: {error}", path.display()),
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(to_lower_hex(&hasher.finalize()))
}

/// Verify the file at `exe` against the manifest's `role` entry — digest, then size — before any
/// spawn. A mismatch is terminal; spec 9.11 forbids substituting another executable.
pub fn verify_executable(
    exe: &Path,
    manifest: &FfmpegManifest,
    role: &str,
) -> Result<ManifestEntry, ExportError> {
    let entry = manifest
        .entry(role)
        .ok_or_else(|| missing(role.to_owned()))?;
    let found = sha256_hex(exe)?;
    if !found.eq_ignore_ascii_case(&entry.sha256) {
        return Err(ExportError::ToolIntegrity {
            role: role.to_owned(),
            detail: format!(
                "digest mismatch at {}: expected {}, found {}",
                exe.display(),
                entry.sha256,
                found
            ),
        });
    }
    if let Some(expected) = entry.size_bytes {
        let actual = std::fs::metadata(exe).map(|m| m.len()).map_err(|error| {
            ExportError::ToolIntegrity {
                role: role.to_owned(),
                detail: format!("stat {}: {error}", exe.display()),
            }
        })?;
        if actual != expected {
            return Err(ExportError::ToolIntegrity {
                role: role.to_owned(),
                detail: format!(
                    "size mismatch at {}: expected {expected}, found {actual}",
                    exe.display()
                ),
            });
        }
    }
    Ok(entry.clone())
}

impl FfmpegTools {
    /// Resolve both executables relative to `workspace_root` (the manifest stores repo-relative
    /// paths), verify each against the manifest, and return the pair. This is the single pre-spawn
    /// integrity gate; nothing spawns without going through it.
    pub fn open(workspace_root: &Path) -> Result<Self, ExportError> {
        let manifest_path = workspace_root.join("tools/ffmpeg/ffmpeg-manifest.json");
        let manifest = FfmpegManifest::load(&manifest_path)?;
        let ffmpeg_rel = manifest
            .entry("ffmpeg")
            .ok_or_else(|| missing("ffmpeg".to_owned()))?
            .relative_path
            .clone();
        let ffprobe_rel = manifest
            .entry("ffprobe")
            .ok_or_else(|| missing("ffprobe".to_owned()))?
            .relative_path
            .clone();
        let ffmpeg = workspace_root.join(ffmpeg_rel);
        let ffprobe = workspace_root.join(ffprobe_rel);
        let ffmpeg_entry = verify_executable(&ffmpeg, &manifest, "ffmpeg")?;
        let ffprobe_entry = verify_executable(&ffprobe, &manifest, "ffprobe")?;
        Ok(Self {
            ffmpeg,
            ffprobe,
            ffmpeg_entry,
            ffprobe_entry,
        })
    }
}

fn missing(role: String) -> ExportError {
    ExportError::ToolIntegrity {
        role,
        detail: "role not present in ffmpeg-manifest.json".to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Argument arrays (18.7). Pure: given a resolved export they build the exact vector, and the unit
// tests assert the vectors element-for-element so the FFmpeg contract is pinned.
// ---------------------------------------------------------------------------------------------

/// FFmpeg codec name for a muxed audio mode. The MP4 row only reaches `Aac`/`None`, but the
/// mapping is total so a future certified row's `Pcm` cannot silently fall through to `aac`.
#[must_use]
pub fn audio_codec_name(mode: AudioMode) -> Option<&'static str> {
    match mode {
        AudioMode::None => None,
        AudioMode::Aac => Some("aac"),
        AudioMode::Pcm => Some("pcm_s16le"),
    }
}

/// The rate-control argument pair (`-crf N` or `-b:v Nk`), fixed by the resolved profile.
fn rate_control_args(q: &QualityProfile) -> [String; 2] {
    match q.rate {
        RateControl::Crf(crf) => ["-crf".to_owned(), crf.to_string()],
        RateControl::BitrateKbps(kbps) => ["-b:v".to_owned(), format!("{kbps}k")],
    }
}

/// Encode stage: packed RGBA8 frames on `pipe:0` → `libx264` in an MP4 video-only file. The raw
/// input declares the exact pixel format, dimensions and rational frame rate; the output fixes the
/// resolved quality and colour metadata with `+bitexact`/no-metadata, so two runs over the same
/// input frames produce the same file (18.7, "raw exact, container structural").
#[must_use]
pub fn encode_args(resolved: &ResolvedExport, video_only_out: &Path) -> Vec<String> {
    let s = &resolved.settings;
    let q = &s.quality;
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-v".into(),
        "error".into(),
        "-y".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgba".into(),
        "-s".into(),
        format!("{}x{}", s.width, s.height),
        "-framerate".into(),
        format!("{}/{}", s.fps.num, s.fps.den),
        "-i".into(),
        "pipe:0".into(),
        "-c:v".into(),
        resolved.codec_spec.video_encoder.into(),
        "-pix_fmt".into(),
        resolved.codec_spec.pixel_format.into(),
    ];
    args.extend(rate_control_args(q));
    args.extend([
        "-preset".into(),
        q.preset.clone(),
        "-g".into(),
        q.gop_frames.to_string(),
        "-profile:v".into(),
        q.codec_profile.clone(),
    ]);
    if let Some(level) = &q.codec_level {
        args.extend(["-level".into(), level.clone()]);
    }
    args.extend([
        "-color_primaries".into(),
        q.color.primaries.clone(),
        "-color_trc".into(),
        q.color.transfer.clone(),
        "-colorspace".into(),
        q.color.matrix.clone(),
        "-color_range".into(),
        q.color.range.clone(),
        "-fflags".into(),
        "+bitexact".into(),
        "-map_metadata".into(),
        "-1".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-an".into(),
        video_only_out.display().to_string(),
    ]);
    args
}

/// Mux stage: video-only MP4 (stream copy) + optional offline WAV → final MP4. 18.7 "Map video
/// and audio explicitly, copy video, select the declared audio codec and do not use -shortest."
#[must_use]
pub fn mux_args(
    resolved: &ResolvedExport,
    video_only: &Path,
    wav: Option<&Path>,
    final_out: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-v".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        video_only.display().to_string(),
        "-map".into(),
        "0:v:0".into(),
        "-c:v".into(),
        "copy".into(),
    ];
    if let Some(wav) = wav {
        // The pipeline passes Some(wav) only when the resolved audio mode names a codec, so the
        // `None` arm here is unreachable in a real export. Rather than panic inside a pure
        // argument builder, a `None` codec falls through to a video-only mux (defensive, and it
        // keeps `mux_args` total so it can never be a source of a hard abort).
        if let Some(audio_codec) = audio_codec_name(resolved.settings.audio) {
            args.extend([
                "-i".into(),
                wav.display().to_string(),
                "-map".into(),
                "1:a:0".into(),
            ]);
            let audio = &resolved.settings.quality.audio;
            args.extend([
                "-c:a".into(),
                audio_codec.to_owned(),
                "-b:a".into(),
                format!("{}k", audio.bitrate_kbps),
                "-ar".into(),
                audio.sample_rate.to_string(),
                "-ac".into(),
                "2".into(),
            ]);
        }
    }
    args.extend([
        "-fflags".into(),
        "+bitexact".into(),
        "-map_metadata".into(),
        "-1".into(),
        "-movflags".into(),
        "+faststart".into(),
        final_out.display().to_string(),
    ]);
    args
}

/// FFprobe JSON stream+format dump with frame counting, for the verifier's structural checks. The
/// verifier addresses streams by index (ADR-0014 f4: stable 0=video, 1=audio).
#[must_use]
pub fn ffprobe_args(path: &Path) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-print_format".into(),
        "json".into(),
        "-show_streams".into(),
        "-show_format".into(),
        "-count_frames".into(),
        path.display().to_string(),
    ]
}

/// The audio-count path (ADR-0014 f3): container audio counts are wrong, so demux the audio stream
/// to canonical PCM on stdout and count frames by size.
#[must_use]
pub fn audio_demux_args(path: &Path) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-v".into(),
        "error".into(),
        "-i".into(),
        path.display().to_string(),
        "-map".into(),
        "0:a:0".into(),
        "-c:a".into(),
        "pcm_s16le".into(),
        "-f".into(),
        "s16le".into(),
        "pipe:1".into(),
    ]
}

// ---------------------------------------------------------------------------------------------
// Child process. Spawn without a shell (spec 9.4); drain both pipes on threads (18.7); kill and
// reap on a deadline (18.11).
// ---------------------------------------------------------------------------------------------

/// The result of a finished child: exit code, drained stdout, and the bounded stderr tail.
pub struct ChildOutcome {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr_tail: String,
}

/// A running FFmpeg with `pipe:0` stdin, draining stdout/stderr on their own threads so neither
/// pipe can fill while the caller streams frames in.
pub struct StreamingChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    role: &'static str,
}

impl StreamingChild {
    /// Spawn an encoder process (FFmpeg) that reads raw frames on stdin. `exe` must already be
    /// integrity-verified by [`FfmpegTools::open`]. stdout/stderr are drained on threads so
    /// neither pipe can fill while frames stream in (18.7).
    pub fn spawn_encoder(
        exe: &Path,
        args: &[String],
        role: &'static str,
    ) -> Result<Self, ExportError> {
        let mut child = Command::new(exe)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ExportError::ToolIntegrity {
                role: role_of(exe),
                detail: format!("spawn of {} failed: {error}", exe.display()),
            })?;
        let stdin = child.stdin.take().ok_or_else(|| ExportError::Encoder {
            stage: "spawn",
            detail: "child stdin pipe was not created".to_owned(),
            status: None,
            stderr_tail: String::new(),
        })?;
        let stdout = child
            .stdout
            .take()
            .map(|pipe| std::thread::spawn(move || read_all(pipe)));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| std::thread::spawn(move || read_tail(pipe)));
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr,
            role,
        })
    }

    /// Write one packed frame to the encoder's stdin. A broken pipe here (FFmpeg died early) is
    /// reported as an encoder failure carrying the status and stderr tail (18.11).
    pub fn write_frame(&mut self, bytes: &[u8]) -> Result<(), ExportError> {
        let stdin = self.stdin.as_mut().ok_or_else(|| ExportError::Encoder {
            stage: "write",
            detail: "stdin already closed".to_owned(),
            status: None,
            stderr_tail: String::new(),
        })?;
        stdin.write_all(bytes).map_err(|error| {
            // Close stdin and reap so the diagnostic carries FFmpeg's real status/stderr.
            let _ = self.stdin.take();
            let outcome = self.reap(Duration::from_secs(5));
            let (status, tail) = match outcome {
                Ok(o) => (o.status, o.stderr_tail),
                Err(_) => (None, String::new()),
            };
            ExportError::Encoder {
                stage: "write",
                detail: format!("broken pipe writing frame: {error}"),
                status,
                stderr_tail: tail,
            }
        })
    }

    /// Close stdin (EOF to the encoder), reap with a deadline, and return the drained result.
    pub fn finish(mut self, deadline: Duration) -> Result<ChildOutcome, ExportError> {
        // Dropping stdin signals end-of-input to FFmpeg.
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.flush();
        }
        self.reap(deadline)
    }

    /// Reap the child within `deadline`, killing and reaping on overrun; join the drain threads.
    fn reap(&mut self, deadline: Duration) -> Result<ChildOutcome, ExportError> {
        let status = reap_with_deadline(&mut self.child, deadline, self.role)?;
        let stdout = self
            .stdout
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let stderr_tail = self
            .stderr
            .take()
            .and_then(|h| h.join().ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        Ok(ChildOutcome {
            status,
            stdout,
            stderr_tail,
        })
    }
}

// One-shot child that captures stdout (for `ffprobe`) with a deadline and a bounded stderr tail.
// No stdin payload.
pub fn run_capture(
    exe: &Path,
    args: &[String],
    role: &'static str,
    deadline: Duration,
) -> Result<ChildOutcome, ExportError> {
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ExportError::ToolIntegrity {
            role: role_of(exe),
            detail: format!("spawn of {} failed: {error}", exe.display()),
        })?;
    let out = child
        .stdout
        .take()
        .map(|pipe| std::thread::spawn(move || read_all(pipe)));
    let err = child
        .stderr
        .take()
        .map(|pipe| std::thread::spawn(move || read_tail(pipe)));
    let status = reap_with_deadline(&mut child, deadline, role)?;
    let stdout = out.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr_tail = err
        .and_then(|h| h.join().ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    Ok(ChildOutcome {
        status,
        stdout,
        stderr_tail,
    })
}

/// Spawn FFmpeg to demux/decode to raw PCM on stdout, and **count** the bytes streamed without
/// buffering the track (long-export memory stability, ADR-0014 f3 and BUILD_STATE risk 24): the
/// reader drains stdout accumulating only a running byte count, so a multi-minute export never
/// lands its whole decoded PCM in RAM. Returns the stdout byte count and the exit code.
pub fn run_counting_stdout(
    exe: &Path,
    args: &[String],
    role: &'static str,
    deadline: Duration,
) -> Result<(u64, Option<i32>, String), ExportError> {
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ExportError::ToolIntegrity {
            role: role_of(exe),
            detail: format!("spawn of {} failed: {error}", exe.display()),
        })?;
    let mut out_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ExportError::MalformedOutput {
            path: "<stdout>".to_owned(),
            detail: "stdout pipe was not created".to_owned(),
        })?;
    let err = child
        .stderr
        .take()
        .map(|pipe| std::thread::spawn(move || read_tail(pipe)));
    // Count on the calling thread so a stalled reader cannot outlive the reaping deadline.
    let mut total: u64 = 0;
    let mut buf = [0u8; 64 * 1024];
    loop {
        match out_pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n as u64,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExportError::MalformedOutput {
                    path: "<stdout>".to_owned(),
                    detail: format!("error reading decoded PCM: {error}"),
                });
            }
        }
    }
    drop(out_pipe);
    let status = reap_with_deadline(&mut child, deadline, role)?;
    let stderr_tail = err
        .and_then(|h| h.join().ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    Ok((total, status, stderr_tail))
}

/// Reap a child with a deadline: poll `try_wait`, and on overrun kill and reap (18.11). Returns the
/// exit code. `sleep`/`Instant` here are the operational wait-loop carve-out.
#[allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "a process deadline is operational, never canonical; spec 6.3 permits bounded \
               wait loops in probe/child code and the workspace lint comment names this carve-out"
)]
fn reap_with_deadline(
    child: &mut Child,
    deadline: Duration,
    role: &'static str,
) -> Result<Option<i32>, ExportError> {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = child.wait();
                return Ok(status.code());
            }
            Ok(None) => {
                if started.elapsed() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ExportError::Timeout { role });
                }
                std::thread::sleep(WAIT_INTERVAL);
            }
            Err(error) => {
                return Err(ExportError::Encoder {
                    stage: "wait",
                    detail: format!("try_wait failed: {error}"),
                    status: None,
                    stderr_tail: String::new(),
                });
            }
        }
    }
}

/// Drain a reader fully, returning everything (bounded by the caller's use, e.g. an ffprobe JSON).
fn read_all<R: Read>(mut reader: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

/// Drain a reader fully but retain only the last [`STDERR_TAIL_BYTES`] (18.7's bounded tail buffer).
fn read_tail<R: Read>(mut reader: R) -> Vec<u8> {
    let mut tail = Vec::with_capacity(STDERR_TAIL_BYTES);
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > STDERR_TAIL_BYTES {
                    let overflow = tail.len() - STDERR_TAIL_BYTES;
                    tail.drain(..overflow);
                }
            }
        }
    }
    tail
}

/// Best-effort role label for diagnostics, from the executable path's file stem.
fn role_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("ffmpeg")
        .to_owned()
}

/// Convert bytes to lowercase hex without a `hex` dependency.
fn to_lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only"
    )]

    use super::*;
    use crate::settings::{
        self, AudioQuality, CodecId, ColorMetadata, ExportAlphaMode, ExportSettings, QualityProfile,
    };
    use anim_core::time::{FrameRange, FrameRate};

    fn mp4_resolved() -> ResolvedExport {
        let settings = ExportSettings {
            schema_version: settings::EXPORT_SETTINGS_SCHEMA_VERSION,
            frames: FrameRange::new(0, 12).unwrap(),
            fps: FrameRate { num: 24, den: 1 },
            width: 320,
            height: 240,
            codec: CodecId::Mp4H264,
            quality: QualityProfile {
                name: "mp4-test".into(),
                rate: RateControl::Crf(20),
                preset: "slow".into(),
                gop_frames: 24,
                codec_profile: "high".into(),
                codec_level: Some("4.1".into()),
                color: ColorMetadata {
                    primaries: "bt709".into(),
                    transfer: "bt709".into(),
                    matrix: "bt709".into(),
                    range: "tv".into(),
                },
                audio: AudioQuality {
                    bitrate_kbps: 192,
                    sample_rate: 48_000,
                },
            },
            alpha: ExportAlphaMode::Off,
            audio: AudioMode::Aac,
            destination: "out/clip.mp4".into(),
            overwrite: false,
        };
        let spec = settings.codec.spec();
        ResolvedExport {
            settings,
            codec_spec: spec,
            frame_count: 12,
            packed_stride: 320 * 4,
            settings_hash: anim_assets::ContentHash::from_bytes([0; 32]),
        }
    }

    #[test]
    fn encode_args_describe_rawvideo_rgba_input_and_h264_output() {
        let r = mp4_resolved();
        let args = encode_args(&r, Path::new("v.mp4"));
        let joined = args.join(" ");
        assert!(
            joined.contains("-f rawvideo -pix_fmt rgba -s 320x240"),
            "{joined}"
        );
        assert!(joined.contains("-framerate 24/1 -i pipe:0"), "{joined}");
        assert!(joined.contains("-c:v libx264 -pix_fmt yuv420p"), "{joined}");
        assert!(joined.contains("-crf 20 -preset slow -g 24"), "{joined}");
        assert!(
            joined.contains("-color_primaries bt709 -color_trc bt709"),
            "{joined}"
        );
        assert!(
            joined.contains("-fflags +bitexact -map_metadata -1"),
            "{joined}"
        );
        assert!(
            joined.ends_with("-an v.mp4"),
            "video-only output last: {joined}"
        );
    }

    #[test]
    fn mux_args_copy_video_and_never_use_shortest() {
        let r = mp4_resolved();
        let args = mux_args(
            &r,
            Path::new("v.mp4"),
            Some(Path::new("a.wav")),
            Path::new("final.mp4"),
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:v copy"), "{joined}");
        assert!(
            joined.contains("-c:a aac -b:a 192k -ar 48000 -ac 2"),
            "{joined}"
        );
        assert!(joined.contains("-map 0:v:0"), "{joined}");
        assert!(joined.contains("-map 1:a:0"), "{joined}");
        assert!(
            !joined.contains("-shortest"),
            "18.7 forbids -shortest: {joined}"
        );
    }

    #[test]
    fn mux_without_audio_maps_only_video() {
        let mut r = mp4_resolved();
        r.settings.audio = AudioMode::None;
        let args = mux_args(&r, Path::new("v.mp4"), None, Path::new("final.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("-c:v copy"), "{joined}");
        assert!(
            !joined.contains("-c:a"),
            "no audio codec when audio is none: {joined}"
        );
        assert!(!joined.contains("-map 1:a:0"), "{joined}");
    }

    #[test]
    fn bitrate_rate_control_emits_negative_bitrate() {
        let mut r = mp4_resolved();
        r.settings.quality.rate = RateControl::BitrateKbps(6_000);
        let args = encode_args(&r, Path::new("v.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("-b:v 6000k"), "{joined}");
        assert!(!joined.contains("-crf"), "{joined}");
    }

    #[test]
    fn sha256_hex_matches_the_published_empty_input_vector() {
        // Independent oracle: SHA-256("") is a fixed, published constant.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.bin");
        std::fs::write(&p, b"").unwrap();
        assert_eq!(
            sha256_hex(&p).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn to_lower_hex_matches_known_bytes() {
        assert_eq!(to_lower_hex(&[0x00, 0xff, 0x0a, 0x9d]), "00ff0a9d");
    }

    #[test]
    fn audio_codec_names_are_total_and_match_the_matrix() {
        assert_eq!(audio_codec_name(AudioMode::None), None);
        assert_eq!(audio_codec_name(AudioMode::Aac), Some("aac"));
        assert_eq!(audio_codec_name(AudioMode::Pcm), Some("pcm_s16le"));
    }
}
