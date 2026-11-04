#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "WP00 probe binary, not production code: a panic here fails the probe loudly rather than emitting a wrong golden, and the bounded sleep in the adapter-retry loop is the only permitted use spec'd by clippy.toml"
)]
// crates/anim_test_support/src/bin/wp00_replace.rs
//! Same-volume atomic replacement and failure-point verification
//! (spec 9.3, 9.5 step 9, 9.7 "Replacement probe").
//!
//! WP03 will persist the authoritative project document with this mechanism, so
//! the question this probe answers is narrow and serious: after a failure at any
//! point, is the file on disk always a complete old state or a complete new state?
//! "It is usually fine" is not an answer, and a half-written `project.anim.json` is
//! the one failure mode this engine cannot recover from by re-deriving anything.
//!
//! `MoveFileExW` is used rather than `ReplaceFileW`. `ReplaceFileW` is documented as
//! three steps - rename the target aside, rename the new file in, delete - and its
//! own page lists intermediate failures (`ERROR_UNABLE_TO_MOVE_REPLACEMENT` leaves
//! "the replaced file no longer exists") in which the destination path is *absent*.
//! That violates the requirement that a fault before commit leaves the old bytes
//! readable at the destination. `REPLACEFILE_WRITE_THROUGH` is additionally
//! documented as unsupported, so it cannot even offer a durable rename.

use std::io::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anim_core::diagnostic::{Diagnostic, DiagnosticCode};
use anim_test_support::probe::{finish, ProbeError};

/// Where the probe stops before reporting. Each value corresponds to a real
/// interruption point in the sequence WP03 will perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultPoint {
    /// Stop after writing and flushing the temporary, before the rename.
    BeforeCommit,
    /// Perform the rename, then stop before any further durability step.
    AfterCommit,
    /// Run the whole sequence.
    None,
}

impl FaultPoint {
    fn parse(raw: &str) -> Result<Self, ProbeError> {
        match raw {
            "before-commit" => Ok(Self::BeforeCommit),
            "after-commit" => Ok(Self::AfterCommit),
            "none" => Ok(Self::None),
            other => Err(ProbeError::Config(format!(
                "--fault wants before-commit, after-commit or none, got {other}"
            ))),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::BeforeCommit => "before_commit",
            Self::AfterCommit => "after_commit",
            Self::None => "none",
        }
    }
}

#[derive(Clone, Debug)]
struct Args {
    /// NTFS test directory (spec 9.7's input).
    dir: PathBuf,
    fault: FaultPoint,
    report_dir: PathBuf,
    /// Put the temporary somewhere else to prove the same-volume guard fires.
    temp_override: Option<PathBuf>,
}

fn main() -> std::process::ExitCode {
    let fallback = default_report_dir();
    let args = match Args::from_env() {
        Ok(a) => a,
        Err(e) => return finish("wp00_replace", vec![e.to_diagnostic()], &fallback),
    };
    let dir = args.dir.clone();
    match run(&args) {
        Ok(diagnostics) => finish("wp00_replace", diagnostics, &args.report_dir),
        Err(e) => {
            // Leave the scenario on disk. A cleanup that runs before the report is
            // written destroys the only evidence of what the destination contained.
            let mut d = vec![e.to_diagnostic()];
            d.push(Diagnostic::warning(
                code("replace.evidence_retained"),
                format!("scenario left in {} for inspection", dir.display()),
            ));
            finish("wp00_replace", d, &args.report_dir)
        }
    }
}

impl Args {
    fn from_env() -> Result<Self, ProbeError> {
        let workspace = workspace_root();
        let mut out = Self {
            dir: workspace.join("probe-out").join("replace-scenario"),
            fault: FaultPoint::None,
            report_dir: workspace.join("probe-out"),
            temp_override: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || -> Result<String, ProbeError> {
                it.next()
                    .ok_or_else(|| ProbeError::Config(format!("{flag} requires a value")))
            };
            match flag.as_str() {
                "--dir" => out.dir = PathBuf::from(value()?),
                "--fault" => out.fault = FaultPoint::parse(&value()?)?,
                "--out" => out.report_dir = PathBuf::from(value()?),
                "--temp-elsewhere" => out.temp_override = Some(PathBuf::from(value()?)),
                other => {
                    return Err(ProbeError::Config(format!(
                        "unrecognized argument {other}; expected --dir, --fault, --out \
                         or --temp-elsewhere"
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

fn default_report_dir() -> PathBuf {
    workspace_root().join("probe-out")
}

fn code(raw: &str) -> DiagnosticCode {
    DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
}

/// 64 KiB of patterned bytes, distinct per state, so a partially written file
/// cannot coincidentally hash like either complete state.
fn state_bytes(tag: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u64 * 31 + tag as u64) as u8)
        .collect()
}

fn run(args: &Args) -> Result<Vec<Diagnostic>, ProbeError> {
    let mut diagnostics = Vec::new();
    let len = 64 * 1024;
    let old = state_bytes(0xA1, len);
    let new = state_bytes(0x5C, len);
    if old == new {
        return Err(ProbeError::Config("test states must differ".into()));
    }

    let scenario = args.dir.join(format!("fault-{}", args.fault.label()));
    // A fresh scenario each run: a leftover file from an earlier attempt would make
    // "the destination holds the old bytes" true for the wrong reason.
    if scenario.exists() {
        std::fs::remove_dir_all(&scenario)
            .map_err(|e| ProbeError::io("clear prior scenario", &scenario, &e))?;
    }
    std::fs::create_dir_all(&scenario)
        .map_err(|e| ProbeError::io("create scenario", &scenario, &e))?;

    let destination = scenario.join("project.anim.json");
    let backup = scenario.join("project.anim.json.bak");
    let temp = args
        .temp_override
        .as_ref()
        .map(|d| d.join("project.anim.json.tmp"))
        .unwrap_or_else(|| scenario.join("project.anim.json.tmp"));

    // 1. The pre-existing authoritative file, written and flushed.
    write_and_flush(&destination, &old)?;
    // 2. The explicit backup. This is spec 4.3's `project.anim.json.bak`, not
    //    ReplaceFileW's internal one - which is why MoveFileExW is sufficient here.
    write_and_flush(&backup, &old)?;
    // 3. The new state, beside its destination so the rename stays on one volume.
    write_and_flush(&temp, &new)?;
    diagnostics.push(Diagnostic::info(
        code("replace.staged"),
        format!(
            "destination, backup and temporary staged in {} (fault point {})",
            scenario.display(),
            args.fault.label()
        ),
    ));

    // Same-volume guard, checked before anything is attempted. Cross-volume rename
    // is not atomic and on some paths is a copy, which is exactly the mixed-bytes
    // case the requirement forbids.
    let dest_volume = volume_serial(&destination)?;
    let temp_volume = volume_serial(&temp)?;
    if dest_volume != temp_volume {
        return Err(ProbeError::CrossVolume);
    }
    diagnostics.push(Diagnostic::info(
        code("replace.same_volume"),
        format!(
            "destination and temporary share volume serial {dest_volume:#010x}; rename \
             will be a directory-entry swap rather than a copy"
        ),
    ));

    if args.fault == FaultPoint::BeforeCommit {
        // Simulated interruption: nothing has been renamed yet. The destination must
        // still be the complete old state (spec 9.7).
        verify_destination(&destination, &old, &new, args.fault, &mut diagnostics)?;
        verify_backup(&backup, &old, args.fault, &mut diagnostics)?;
        if !temp.exists() {
            return Err(ProbeError::MixedBytesAfterFault(format!(
                "{}: the temporary vanished before any commit was attempted",
                args.fault.label()
            )));
        }
        diagnostics.push(Diagnostic::info(
            code("replace.old_state_survived"),
            "a fault before commit left the old bytes complete at the destination and \
             the temporary still present, which is the required outcome",
        ));
        return Ok(diagnostics);
    }

    // 4. The commit itself.
    move_file_replace(&temp, &destination)?;
    diagnostics.push(Diagnostic::info(
        code("replace.committed"),
        "MoveFileExW with REPLACE_EXISTING|WRITE_THROUGH returned success",
    ));

    verify_destination(&destination, &old, &new, args.fault, &mut diagnostics)?;
    verify_backup(&backup, &old, args.fault, &mut diagnostics)?;
    if args.fault == FaultPoint::AfterCommit {
        diagnostics.push(Diagnostic::info(
            code("replace.new_state_only"),
            "a fault after commit left the new bytes complete at the destination; the \
             old state survives only in the backup (spec 9.7 permits new, never mixed)",
        ));
    } else {
        // Full sequence: nothing may be left behind.
        if temp.exists() {
            return Err(ProbeError::Leak);
        }
        diagnostics.push(Diagnostic::info(
            code("replace.temporary_consumed"),
            "the temporary no longer exists after the rename, so a completed run leaves \
             no partial artifact beside the destination",
        ));
    }
    Ok(diagnostics)
}

/// Write, flush to the OS, then `FlushFileBuffers` to the device. Skipping the last
/// step is the common bug: `sync_all` on a closed file is not possible, so the
/// flush has to happen while the handle is still open.
fn write_and_flush(path: &Path, bytes: &[u8]) -> Result<(), ProbeError> {
    let mut file =
        std::fs::File::create(path).map_err(|e| ProbeError::io("create staged file", path, &e))?;
    file.write_all(bytes)
        .map_err(|e| ProbeError::io("write staged file", path, &e))?;
    file.flush()
        .map_err(|e| ProbeError::io("flush staged file", path, &e))?;
    flush_file_buffers(&file).map_err(|e| ProbeError::io("FlushFileBuffers", path, &e))?;
    drop(file);
    // Re-open and hash to prove what is on disk rather than what the OS still holds.
    let on_disk =
        std::fs::read(path).map_err(|e| ProbeError::io("re-read staged file", path, &e))?;
    if on_disk != bytes {
        return Err(ProbeError::MixedBytesAfterFault(format!(
            "staging {} did not produce the bytes written to it",
            path.display()
        )));
    }
    Ok(())
}

fn flush_file_buffers(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::FlushFileBuffers;

    // file.as_socket()/as_raw_handle() gives the HANDLE; FlushFileBuffers takes the
    // typed wrapper, which is constructible from the raw handle.
    let handle = windows::Win32::Foundation::HANDLE(file.as_raw_handle());
    if handle.is_invalid() {
        return Err(std::io::Error::other("invalid handle before device flush"));
    }
    match unsafe { FlushFileBuffers(handle) } {
        Ok(()) => Ok(()),
        Err(e) => Err(std::io::Error::other(format!(
            "FlushFileBuffers: {}",
            e.message()
        ))),
    }
}

/// Volume serial number of an open file, which is what makes "same volume" a checked
/// fact rather than an assumption about paths.
fn volume_serial(path: &Path) -> Result<u32, ProbeError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::GetFileInformationByHandle;

    let file =
        std::fs::File::open(path).map_err(|e| ProbeError::io("open for volume id", path, &e))?;
    let handle = windows::Win32::Foundation::HANDLE(file.as_raw_handle());
    let mut info = windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(|e| {
        ProbeError::io(
            "GetFileInformationByHandle",
            path,
            &std::io::Error::other(e.message()),
        )
    })?;
    Ok(info.dwVolumeSerialNumber)
}

fn move_file_replace(from: &Path, to: &Path) -> Result<(), ProbeError> {
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from_wide = to_wide(from)?;
    let to_wide = to_wide(to)?;
    unsafe {
        MoveFileExW(
            windows::core::PCWSTR::from_raw(from_wide.as_ptr()),
            windows::core::PCWSTR::from_raw(to_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|e| {
        ProbeError::io(
            "MoveFileExW",
            to,
            &std::io::Error::other(format!(
                "{} -> {}: {}",
                from.display(),
                to.display(),
                e.message()
            )),
        )
    })
}

/// Encode a path as a null-terminated UTF-16 buffer for a `PCWSTR`.
fn to_wide(path: &Path) -> Result<Vec<u16>, ProbeError> {
    let os = path.as_os_str().encode_wide().collect::<Vec<u16>>();
    // An embedded NUL would silently truncate the path handed to Win32, so the two
    // parts are concatenated rather than re-encoded.
    if os.contains(&0) {
        return Err(ProbeError::Config(format!(
            "path contains an interior NUL and cannot be passed to Win32: {}",
            path.display()
        )));
    }
    let mut v = os;
    v.push(0);
    Ok(v)
}

/// The core assertion: the destination is byte-identical to the old state or to the
/// new state, and never anything else.
fn verify_destination(
    path: &Path,
    old: &[u8],
    new: &[u8],
    fault: FaultPoint,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), ProbeError> {
    let bytes = std::fs::read(path).map_err(|e| {
        // A missing destination after any fault is itself a violation: ReplaceFileW
        // can produce exactly this, which is why it was not chosen.
        ProbeError::MixedBytesAfterFault(format!(
            "{}: destination {} could not be reopened: {e}",
            fault.label(),
            path.display()
        ))
    })?;
    let digest = blake3::hash(&bytes).to_hex().to_string();
    let (verdict, matches) = if bytes == *old {
        ("old", true)
    } else if bytes == *new {
        ("new", true)
    } else {
        ("neither", false)
    };
    if !matches {
        return Err(ProbeError::MixedBytesAfterFault(format!(
            "{}: destination is {} bytes with digest {}, which is neither the complete \
             old state nor the complete new state",
            fault.label(),
            bytes.len(),
            digest
        )));
    }
    diagnostics.push(Diagnostic::info(
        code("replace.destination_state"),
        format!(
            "destination holds the complete {verdict} state ({} bytes, blake3 {})",
            bytes.len(),
            &digest[..16]
        ),
    ));
    Ok(())
}

fn verify_backup(
    path: &Path,
    expected: &[u8],
    fault: FaultPoint,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), ProbeError> {
    let bytes = std::fs::read(path).map_err(|e| {
        ProbeError::BackupDisagreement(format!(
            "{}: backup {} could not be reopened: {e}",
            fault.label(),
            path.display()
        ))
    })?;
    if bytes != *expected {
        return Err(ProbeError::BackupDisagreement(format!(
            "{}: backup holds {} bytes that differ from the old state it was written \
             from",
            fault.label(),
            bytes.len()
        )));
    }
    diagnostics.push(Diagnostic::info(
        code("replace.backup_state"),
        format!(
            "backup still holds the complete old state ({} bytes), so recovery has a \
             readable fallback regardless of where the fault landed",
            bytes.len()
        ),
    ));
    Ok(())
}
