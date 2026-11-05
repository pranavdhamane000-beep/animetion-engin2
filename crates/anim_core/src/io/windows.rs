//! The Win32 half of atomic replacement (spec 10.3).
//!
//! `pub(crate)` on purpose: spec 5.1's rule for the `windows` dependency is
//! "target-gated features, no cross-platform API leakage", so no `HANDLE`,
//! `PCWSTR` or Win32 error type may appear in anything a later crate calls. The
//! portable surface is [`super::atomic_replace`]; this file is its only
//! implementation.
//!
//! The workspace is Windows-only — `rust-toolchain.toml` pins
//! `x86_64-pc-windows-msvc` and spec 2.1 names no other platform — so these
//! functions are not cfg-gated behind a target that is never built. Adding a
//! second platform would mean adding a second implementation, which is the case
//! spec 9.11 forbids silently falling back through.

use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, FlushFileBuffers, GetFileInformationByHandle, MoveFileExW,
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MOVE_FILE_FLAGS,
    OPEN_EXISTING,
};

use super::atomic_replace::{io_error, DirectoryFlush, ReplacementError};

/// Encode a path as the null-terminated UTF-16 buffer a `PCWSTR` points at.
///
/// An interior NUL would silently truncate the path handed to Win32, and a
/// truncated path is a *different file* rather than an error, so it is refused
/// here rather than discovered by whichever file got overwritten.
fn to_wide(path: &Path) -> Result<Vec<u16>, ReplacementError> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<u16>>();
    if wide.contains(&0) {
        return Err(io_error(
            "path contains an interior NUL",
            path,
            "cannot be passed to Win32",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// `FlushFileBuffers` on an open file handle.
///
/// This has to happen while the handle is still open — there is no way to flush a
/// closed file, which is the common bug in a "we used `sync_all`" claim.
pub(crate) fn flush_file(file: &File, path: &Path) -> Result<(), ReplacementError> {
    let handle = HANDLE(file.as_raw_handle());
    if handle.is_invalid() {
        return Err(io_error("invalid handle before device flush", path, ""));
    }
    // SAFETY: `handle` is borrowed from `file`, which outlives this call, and
    // FlushFileBuffers takes no ownership of it.
    unsafe { FlushFileBuffers(handle) }.map_err(|e| io_error("FlushFileBuffers", path, e.message()))
}

/// The volume a path lives on, by handle rather than by path prefix.
///
/// A prefix proves nothing about mount points or junctions, and a rename across
/// volumes is a copy, which is the mixed-bytes case ADR-0006 exists to prevent.
/// Directories need the backup-semantics open, because an ordinary create never
/// succeeds on one.
pub(crate) fn volume_serial(path: &Path) -> Result<u32, ReplacementError> {
    if path.is_dir() {
        let handle = open_directory(path)?;
        let info = query_handle(handle, path);
        // SAFETY: the handle came from CreateFileW above and is not used again.
        let closed = unsafe { CloseHandle(handle) };
        let serial = info?;
        closed.map_err(|e| io_error("CloseHandle", path, e.message()))?;
        return Ok(serial);
    }
    let file = File::open(path).map_err(|e| io_error("open for volume id", path, e.to_string()))?;
    let handle = HANDLE(file.as_raw_handle());
    query_handle(handle, path)
}

fn query_handle(handle: HANDLE, path: &Path) -> Result<u32, ReplacementError> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `info` is a validly sized struct written only by this call, and the
    // handle is live for the duration.
    unsafe { GetFileInformationByHandle(handle, &mut info) }
        .map_err(|e| io_error("GetFileInformationByHandle", path, e.message()))?;
    Ok(info.dwVolumeSerialNumber)
}

/// Open a path as a directory for metadata work, with no access rights requested.
///
/// `FILE_FLAG_BACKUP_SEMANTICS` is what lets the open succeed on a directory at
/// all, and it is the same handle form spec 12.7 names for a directory
/// flush.
fn open_directory(path: &Path) -> Result<HANDLE, ReplacementError> {
    let wide = to_wide(path)?;
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    // SAFETY: `wide` outlives the call and is a valid null-terminated buffer.
    unsafe {
        CreateFileW(
            windows::core::PCWSTR::from_raw(wide.as_ptr()),
            0,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(|e| io_error("CreateFileW(backup semantics)", path, e.message()))
}

/// Rename `from` onto `to`, durable on return.
///
/// `MoveFileExW` with `WRITE_THROUGH` does not return until the rename is on disk
/// (ADR-0006). With `overwrite` false the destination must not exist, and the call
/// fails rather than replacing it, which is the create-only behaviour the asset
/// store needs (spec 11.7: "rename without overwrite").
pub(crate) fn rename(from: &Path, to: &Path, overwrite: bool) -> Result<(), ReplacementError> {
    let from_wide = to_wide(from)?;
    let to_wide = to_wide(to)?;
    let flags: MOVE_FILE_FLAGS = if overwrite {
        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
    } else {
        MOVEFILE_WRITE_THROUGH
    };
    // SAFETY: both buffers are live and null-terminated for the duration of the
    // call, and the strings are read-only inputs.
    unsafe {
        MoveFileExW(
            windows::core::PCWSTR::from_raw(from_wide.as_ptr()),
            windows::core::PCWSTR::from_raw(to_wide.as_ptr()),
            flags,
        )
    }
    .map_err(|e| ReplacementError::Rename {
        path: to.display().to_string(),
        from: from.display().to_string(),
        detail: e.message().to_string(),
    })
}

/// Remove a temporary this crate created. Never used on a destination: spec 6.3
/// forbids deleting a destination before replacement is known possible, and
/// `std::fs::remove_file` is denied workspace-wide for that reason (`clippy.toml`).
pub(crate) fn delete_temporary(path: &Path) -> Result<(), ReplacementError> {
    let wide = to_wide(path)?;
    // SAFETY: `wide` is a live null-terminated buffer used only for this call.
    unsafe { DeleteFileW(windows::core::PCWSTR::from_raw(wide.as_ptr())) }
        .map_err(|e| io_error("DeleteFileW", path, e.message()))
}

/// Attempt to flush the directory entry. Spec 18.4 requires that a directory-flush
/// limitation be a visible durability warning rather than silent success, so the
/// failure path returns the reason instead of an error.
pub(crate) fn flush_directory(path: &Path) -> DirectoryFlush {
    let handle = match open_directory(path) {
        Ok(handle) => handle,
        Err(e) => return DirectoryFlush::Unsupported { detail: e.detail() },
    };
    // SAFETY: the handle is open and not used by any other thread.
    let flushed = unsafe { FlushFileBuffers(handle) };
    // SAFETY: closing the handle this function opened.
    let closed = unsafe { CloseHandle(handle) };
    match (flushed, closed) {
        (Ok(()), Ok(())) => DirectoryFlush::Flushed,
        (Err(e), _) => DirectoryFlush::Unsupported {
            detail: e.message().to_string(),
        },
        (Ok(()), Err(e)) => DirectoryFlush::Unsupported {
            detail: format!("flushed but CloseHandle failed: {}", e.message()),
        },
    }
}
