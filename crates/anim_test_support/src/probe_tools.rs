//! Launching and verifying the external media tools (spec 9.4, 9.6, 9.11).
//!
//! Shared by `wp00_media` and `wp00_export`, and not a file the spec's 9.3 table
//! names. It lives in this crate rather than in each binary because duplicating a
//! process-timeout path across two probes means two chances to get the half that
//! matters wrong: leaving a child alive on timeout. The alternative - a shell - is
//! prohibited outright (spec 9.4).
//!
//! Three rules the rest of the crate depends on:
//!   * Processes are spawned with an argument array and never through a shell, so
//!     no path or filename can be reinterpreted as a command.
//!   * stdout and stderr are drained on reader threads. A child whose pipe buffer
//!     fills blocks, and a blocked child looks exactly like a slow one.
//!   * On timeout the child is terminated *and reaped*. `kill` without `wait`
//!     leaves a zombie handle behind, which is the leak spec 9.8 calls out.

// This module is WP00 probe support, not a production API, and it is where the
// workspace's determinism rules intentionally do not apply:
//   * `Instant` and a bounded sleep are how a child-process deadline is measured.
//     `clippy.toml` disallows `std::time::Instant` because wall-clock must never
//     reach canonical state; a process timeout is operational, not canonical, and
//     nothing here feeds a hash.
//   * `expect` guards handles the code two lines above just piped. A failure there
//     is a bug in this module, and a panic is the honest report.
//   * Per-function `# Errors` rustdoc is omitted: the error surface is the shared
//     `ProbeError` taxonomy, documented once, and every failure reaches a report.
#![allow(
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    reason = "WP00 probe-support module; see the comment above"
)]

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anim_core::diagnostic::{Diagnostic, DiagnosticCode};

use crate::probe::ProbeError;

/// Bytes of stderr retained. Enough to hold a FFmpeg failure banner and the last
/// real error line; bounded so a chatty encoder cannot grow the report without
/// limit.
const STDERR_TAIL_BYTES: usize = 64 * 1024;
/// Poll interval while waiting for a child. Only ever a wait *loop* - it never
/// paces canonical work (spec 6.3).
const WAIT_INTERVAL: Duration = Duration::from_millis(25);

/// One verified executable from `tools/ffmpeg/ffmpeg-manifest.json`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct ManifestEntry {
    pub role: String,
    pub relative_path: String,
    pub sha256: String,
    pub version: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
struct ManifestFile {
    executables: Vec<ManifestEntry>,
}

/// A finished child process: exit status, stdout, and the retained stderr tail.
#[derive(Clone, Debug)]
pub struct Completed {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr_tail: String,
}

impl Completed {
    /// Last non-empty stderr line, which is where FFmpeg puts the reason.
    pub fn last_stderr_line(&self) -> String {
        self.stderr_tail
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_owned()
    }
}

/// Read the manifest and find the entry for `role`.
pub fn manifest_entry(manifest: &Path, role: &str) -> Result<ManifestEntry, ProbeError> {
    let text = std::fs::read_to_string(manifest)
        .map_err(|e| ProbeError::io("read tool manifest", manifest, &e))?;
    let parsed: ManifestFile = serde_json::from_str(&text)
        .map_err(|e| ProbeError::MalformedJson(format!("{}: {e}", manifest.display())))?;
    parsed
        .executables
        .into_iter()
        .find(|entry| entry.role == role)
        .ok_or_else(|| {
            ProbeError::ToolNotConfigured(format!(
                "{role} is not declared in {}",
                manifest.display()
            ))
        })
}

/// SHA-256 of a file, lowercase hex.
///
/// This is the one place blake3 is *not* used: spec 5.1 names SHA-256 for
/// executable verification, and an operator must be able to reproduce the digest
/// with any standard tool rather than only with this engine.
pub fn sha256_hex(path: &Path) -> Result<String, ProbeError> {
    use sha2::Digest as _;

    let mut file = std::fs::File::open(path)
        .map_err(|e| ProbeError::io("open executable for hashing", path, &e))?;
    let mut hasher = sha2::Sha256::new();
    let mut chunk = vec![0u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|e| ProbeError::io("read executable for hashing", path, &e))?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Check the file at `exe` against the manifest entry for `role`.
///
/// The digest is compared before the process is spawned, so an unverified binary
/// never runs. Spec 9.11 forbids falling back to a system FFmpeg, and the only way
/// to honour that is to fail here rather than try something else.
pub fn verify_executable(
    exe: &Path,
    manifest: &Path,
    role: &str,
) -> Result<ManifestEntry, ProbeError> {
    if !exe.exists() {
        return Err(ProbeError::ToolNotConfigured(format!(
            "{role} executable {} does not exist",
            exe.display()
        )));
    }
    let entry = manifest_entry(manifest, role)?;
    let actual = sha256_hex(exe)?;
    let want = entry.sha256.to_ascii_lowercase();
    if actual != want {
        return Err(ProbeError::ToolDigestMismatch(format!(
            "{role}: {} is {actual}, manifest expects {want}",
            exe.display()
        )));
    }
    if let Some(expected_size) = entry.size_bytes {
        let len = std::fs::metadata(exe)
            .map_err(|e| ProbeError::io("stat executable", exe, &e))?
            .len();
        if len != expected_size {
            return Err(ProbeError::ToolDigestMismatch(format!(
                "{role}: size {len} does not match the manifest's {expected_size} even \
                 though the digest matched, so the manifest is internally inconsistent"
            )));
        }
    }
    Ok(entry)
}

/// Launch a verified tool with an argument array, drain both pipes, and enforce a
/// timeout with termination and reap.
///
/// `stdin_payload` is written and the pipe closed, which is how a raw video stream
/// reaches FFmpeg without a shell pipeline.
pub fn run<S: AsRef<std::ffi::OsStr>>(
    exe: &Path,
    args: &[S],
    stdin_payload: Option<&[u8]>,
    timeout: Duration,
) -> Result<Completed, ProbeError> {
    if !exe.exists() {
        return Err(ProbeError::ToolNotConfigured(exe.display().to_string()));
    }

    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            // Explicitly null so a tool cannot inherit and block on the parent's
            // console stdin.
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // No shell: Command::new with a path and an argument array. Nothing here can be
    // reinterpreted as a command line (spec 9.4).
    let mut child = command.spawn().map_err(|e| {
        ProbeError::ToolSpawn(
            exe.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            e,
        )
    })?;

    // Feed stdin on its own thread: a payload larger than the OS pipe buffer would
    // otherwise deadlock the parent against the child.
    //
    // The payload is copied because a thread needs `'static` data while the caller
    // owns a borrow. Probe inputs are frames of raw video, not archives, so the copy
    // is cheap here; WP09's production writer should use a scoped thread or a channel
    // instead of inheriting this shortcut.
    let writer = stdin_payload.map(|payload| {
        let mut pipe = child.stdin.take().expect("stdin was piped above");
        let owned = payload.to_vec();
        std::thread::spawn(move || {
            // A closed-or-full pipe is not an error worth failing the probe over:
            // FFmpeg exiting early is reported by its exit status instead.
            let _ = pipe.write_all(&owned);
            let _ = pipe.flush();
            drop(pipe);
        })
    });

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped above");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped above");
    let stdout_join = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_join = std::thread::spawn(move || {
        // Bounded drain: keep only the tail, but read everything so the child is
        // never blocked by a pipe nobody empties.
        let mut ring: Vec<u8> = Vec::with_capacity(STDERR_TAIL_BYTES);
        let mut chunk = [0u8; 8 * 1024];
        loop {
            match stderr_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    ring.extend_from_slice(&chunk[..n]);
                    if ring.len() > STDERR_TAIL_BYTES {
                        let drop_from = ring.len() - STDERR_TAIL_BYTES;
                        ring.drain(..drop_from);
                    }
                }
            }
        }
        String::from_utf8_lossy(&ring).into_owned()
    });

    let deadline = Instant::now() + timeout;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break Err(());
                }
                #[allow(clippy::disallowed_methods)]
                std::thread::sleep(WAIT_INTERVAL);
            }
            Err(e) => {
                // Cannot even ask for the status. Terminate and reap before giving
                // up, so an error path does not orphan the process.
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::io("try_wait", exe, &e));
            }
        }
    };

    let Ok(status) = outcome else {
        // Timeout: terminate, then reap. Killing without waiting leaves the
        // child's handle and any locked output file behind.
        let _ = child.kill();
        let _ = child.wait();
        // The reader threads can still be blocked on pipes belonging to a
        // process we just killed. Dropping our handles unblocks them, and the
        // joins are not required to report the timeout.
        drop(child.stdin.take());
        return Err(ProbeError::ToolTimeout(format!(
            "{} exceeded {} ms",
            exe.display(),
            timeout.as_millis()
        )));
    };

    if let Some(join) = writer {
        let _ = join.join();
    }
    let stdout = stdout_join.join().map_err(|_| ProbeError::Leak)?;
    let stderr_tail = stderr_join.join().map_err(|_| ProbeError::Leak)?;

    Ok(Completed {
        code: status.code(),
        stdout,
        stderr_tail,
    })
}

/// Record which tool ran and what it reported, so a G0 evidence file can be
/// reproduced from the report alone (spec 9.6).
pub fn record_tool_run<S: AsRef<std::ffi::OsStr>>(
    diagnostics: &mut Vec<Diagnostic>,
    role: &str,
    entry: &ManifestEntry,
    args: &[S],
    completed: &Completed,
) {
    let joined = args
        .iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let d = Diagnostic::info(
        DiagnosticCode::new("tool.run").unwrap_or_else(DiagnosticCode::unspecified),
        format!(
            "{role} {} exited {:?}; argv [{joined}]",
            entry.version, completed.code
        ),
    )
    .with_context("role", role)
    .with_context(
        "process_exit",
        completed
            .code
            .map_or("signal".to_owned(), |c| c.to_string()),
    );
    diagnostics.push(d);
}

/// Resolve a path that may be relative to the workspace root.
pub fn resolve_from_workspace(candidate: &Path) -> PathBuf {
    if candidate.is_absolute() {
        return candidate.to_path_buf();
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    root.join(candidate)
}
