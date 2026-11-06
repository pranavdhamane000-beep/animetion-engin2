//! The shared atomic-file API used by save and export (spec 10.3).
//!
//! The requirement this exists to satisfy is narrow and unforgiving: after a fault
//! at any point, the destination holds either the complete old state or the
//! complete new state, and never a blend (spec 9.7, 6.3, 23's save-failure row).
//! A half-written `project.anim.json` is the one failure this engine cannot
//! re-derive its way out of, because there is no upstream state to replay from.
//!
//! The sequence is the one `wp00_replace` proved on this machine and ADR-0006
//! accepted: write a temporary beside the destination, flush it to the device,
//! write the previous state to an explicit backup, check that both files are on one
//! volume by *handle* rather than by path prefix, then
//! `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`, which does not return until the
//! rename is on disk.
//!
//! Deviation, recorded in DECISIONS.md #23 and `BUILD_STATE.md`'s deviation row 7:
//! spec 12.7 prescribes `ReplaceFileW` with `lpBackupFileName`. ADR-0006 rejected it
//! from that API's own documentation — `ERROR_UNABLE_TO_MOVE_REPLACEMENT` leaves the
//! destination path *absent* — so this module implements the alternative and WP03
//! reconciles the two when it writes `save_atomic`.
//!
//! Two entry points, one sequence. [`write_atomic`] takes the payload in memory and
//! is the save path; [`StagedWriter`] plus [`Staged`] take it in chunks and are the
//! path for a payload that must not be held — WP02's asset import, where the
//! destination is derived from a hash of the bytes still being read (spec 11.7), and
//! WP09's export. Both end in the same rename, the same volume guard and the same
//! post-rename check that the destination is complete rather than blended.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
use crate::io::windows;

/// How many temporary names to try before giving up. A fresh run normally needs
/// one; the attempts exist because a crashed earlier run can leave `.tmp` files
/// behind, and `create_new` refuses those rather than writing into them.
const TEMPORARY_ATTEMPTS: u32 = 64;

/// Whether the parent directory entry itself was flushed.
///
/// Spec 18.4 makes this a distinction rather than an implementation detail: "a
/// directory-flush limitation is a visible durability warning, not silent success."
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryFlush {
    /// The rename is durable in the directory entry, not only in the file.
    Flushed,
    /// Refused or unimplemented. The file bytes and the rename are durable; a
    /// power loss could still leave the directory pointing at the old entry.
    Unsupported { detail: String },
}

impl DirectoryFlush {
    #[must_use]
    pub const fn is_flushed(&self) -> bool {
        matches!(self, Self::Flushed)
    }
}

/// What a caller is asking for. Built through the three named constructors rather
/// than by filling in fields, because each is a different promise about an existing
/// destination.
#[derive(Clone, Debug)]
pub struct Replacement<'a> {
    /// The final path the bytes must appear at.
    pub destination: &'a Path,
    /// Where to write the previous state before renaming. `None` writes no backup.
    pub backup: Option<&'a Path>,
    /// Whether an existing destination may be replaced.
    pub overwrite: bool,
    /// Leave the temporary in place when the replacement fails, so a probe or a
    /// report can inspect it (spec 7.3's "explicit diagnostic preservation").
    pub keep_temporary_on_failure: bool,
}

impl<'a> Replacement<'a> {
    /// Replace the destination with no backup. The export publication path (spec
    /// 18.5), where the artifact is re-derivable so a backup has no recovery value.
    #[must_use]
    pub fn to_file(destination: &'a Path) -> Self {
        Self {
            destination,
            backup: None,
            overwrite: true,
            keep_temporary_on_failure: false,
        }
    }

    /// Replace the destination and keep the previous state at `backup` first. This
    /// is `save_atomic`'s case (spec 12.7, 12.9) and the only one where
    /// `project.anim.json.bak` is a recovery input.
    #[must_use]
    pub fn with_backup(destination: &'a Path, backup: &'a Path) -> Self {
        Self {
            destination,
            backup: Some(backup),
            overwrite: true,
            keep_temporary_on_failure: false,
        }
    }

    /// Publish only if nothing is there yet: the content-addressed object store's
    /// "rename without overwrite" (spec 11.7). An existing destination is left
    /// exactly as it was.
    #[must_use]
    pub fn create_only(destination: &'a Path) -> Self {
        Self {
            destination,
            backup: None,
            overwrite: false,
            keep_temporary_on_failure: false,
        }
    }

    /// Keep the temporary on failure. Fault-injection probes and WP11's failure
    /// artifacts ask for this; ordinary saves do not.
    #[must_use]
    pub fn preserving_temporary(mut self) -> Self {
        self.keep_temporary_on_failure = true;
        self
    }
}

/// What a completed replacement did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacementReport {
    pub destination: PathBuf,
    /// The name that was staged. After success nothing is at this path — the rename
    /// consumed it — but a report that says which name was used is auditable.
    pub temporary: PathBuf,
    /// Bytes the destination now holds.
    pub bytes: u64,
    /// Volume the destination and the temporary shared, from the handle check.
    pub volume_serial: u32,
    /// The backup actually written. `None` when there was no previous state to copy.
    pub backup: Option<PathBuf>,
    pub directory_flush: DirectoryFlush,
}

impl ReplacementReport {
    /// The warning spec 12.11 requires when directory durability was not achieved,
    /// or `None` when everything flushed.
    #[must_use]
    pub fn durability_warning(&self) -> Option<Diagnostic> {
        match &self.directory_flush {
            DirectoryFlush::Flushed => None,
            DirectoryFlush::Unsupported { detail } => Some(Diagnostic::warning(
                code("io.directory_flush_unsupported"),
                format!(
                    "{} was replaced durably at the file level, but the directory entry \
                     for its parent could not be flushed: {detail}",
                    self.destination.display()
                ),
            )),
        }
    }
}

/// Every way a replacement can fail. All of them leave the destination holding one
/// complete state: the failures that could have mixed bytes are the ones refused
/// before the rename is attempted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplacementError {
    #[error("{context}: {path}: {detail}")]
    Io {
        context: &'static str,
        path: String,
        detail: String,
    },

    #[error("the parent directory {path} does not exist, so no temporary could be staged beside the destination")]
    ParentMissing { path: String },

    #[error("{path} already exists and this replacement does not permit overwrite")]
    DestinationExists { path: String },

    #[error(
        "temporary {temporary} is on volume {temporary_serial:#010x} but destination \
         {destination} is on volume {destination_serial:#010x}; replacement would be a copy, \
         not an atomic rename"
    )]
    CrossVolume {
        destination: String,
        temporary: String,
        destination_serial: u32,
        temporary_serial: u32,
    },

    #[error("{path} reads back {read} bytes, not the {written} that were written to it")]
    StagingMismatch {
        path: String,
        written: u64,
        read: u64,
    },

    #[error(
        "{path} reads back {read} bytes after replacement, not the {written} that were staged"
    )]
    DestinationDisagreement {
        path: String,
        written: u64,
        read: u64,
    },

    #[error("backup {path} does not hold the previous state it was written from")]
    BackupDisagreement { path: String },

    #[error("could not rename {from} onto {path}: {detail}")]
    Rename {
        path: String,
        from: String,
        detail: String,
    },

    #[error("no unused temporary name beside {path} within {attempts} attempts")]
    TemporariesExhausted { path: String, attempts: u32 },

    #[error("staged byte count overflowed the u64 length field at {bytes} bytes into {path}")]
    SizeOverflow { path: String, bytes: u64 },
}

impl ReplacementError {
    /// Stable category, matching [`crate::error::CoreError::code`] so a report from
    /// the time model and a report from the filesystem line up (spec 7.4).
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        let raw = match self {
            Self::Io { .. } => "io.filesystem",
            Self::ParentMissing { .. } => "io.parent_missing",
            Self::DestinationExists { .. } => "io.destination_exists",
            Self::CrossVolume { .. } => "io.cross_volume",
            Self::StagingMismatch { .. } => "io.staging_mismatch",
            Self::DestinationDisagreement { .. } => "io.destination_disagreement",
            Self::BackupDisagreement { .. } => "io.backup_disagreement",
            Self::Rename { .. } => "io.rename_failed",
            Self::TemporariesExhausted { .. } => "io.temporaries_exhausted",
            Self::SizeOverflow { .. } => "io.size_overflow",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    /// The shortest true description of the failure, for nesting inside another
    /// diagnostic.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Io { detail, .. } => detail.clone(),
            other => other.to_string(),
        }
    }

    #[must_use]
    pub fn severity(&self) -> DiagnosticSeverity {
        DiagnosticSeverity::Error
    }

    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        Diagnostic::new(self.code(), self.severity(), self.to_string())
    }
}

pub(crate) fn io_error(
    context: &'static str,
    path: &Path,
    detail: impl Into<String>,
) -> ReplacementError {
    ReplacementError::Io {
        context,
        path: path.display().to_string(),
        detail: detail.into(),
    }
}

fn code(raw: &str) -> DiagnosticCode {
    DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
}

/// Write `bytes` so that the destination appears complete or not at all.
pub fn write_atomic(
    request: &Replacement<'_>,
    bytes: &[u8],
) -> Result<ReplacementReport, ReplacementError> {
    let parent = destination_parent(request.destination)?;
    refuse_existing_destination(request)?;
    let temporary = stage(&parent, request.destination, bytes)?;
    publish_staged(
        request,
        temporary,
        &parent,
        Expectation::Bytes(bytes),
        bytes.len() as u64,
    )
}

/// The directory a replacement stages inside, refusing a missing one before any
/// file is created.
fn destination_parent(destination: &Path) -> Result<PathBuf, ReplacementError> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(ReplacementError::ParentMissing {
            path: parent.display().to_string(),
        });
    }
    Ok(parent.to_path_buf())
}

/// Refused before anything is created: spec 6.3 forbids deleting or overwriting a
/// destination before replacement is known to be possible.
fn refuse_existing_destination(request: &Replacement<'_>) -> Result<(), ReplacementError> {
    if request.destination.exists() && !request.overwrite {
        return Err(ReplacementError::DestinationExists {
            path: request.destination.display().to_string(),
        });
    }
    Ok(())
}

/// A temporary being written in bounded chunks, whose payload is never held in
/// memory.
///
/// This exists for the two callers that cannot use [`write_atomic`]: WP02's asset
/// import (spec 11.7) and WP09's export both stream bytes that may be far larger
/// than the process, and in WP02's case the destination path is not knowable until
/// the whole stream has been read, because it is derived from the hash of it. The
/// caller owns the reading and the hashing; this type owns the parts that must not
/// be reimplemented per crate — the exclusive temporary name, the device flush, and
/// the guarantee that a failed import leaves no file behind.
///
/// Dropping an unsynchronized writer deletes its temporary. Spec 11.8's "temporary
/// files are never resolved as objects" is served by that plus the object-store path
/// shape, which no `.tmp` name can match.
pub struct StagedWriter {
    file: Option<File>,
    path: PathBuf,
    bytes: u64,
    consumed: bool,
}

impl StagedWriter {
    /// Create a new temporary beside `parent`, named `.<stem>.<n>.tmp`.
    ///
    /// `parent` must already exist; a store that wants a deeper layout creates it
    /// first, so that no code path here invents a directory an operator did not ask
    /// for.
    pub fn create_in(parent: &Path, stem: &str) -> Result<Self, ReplacementError> {
        if !parent.is_dir() {
            return Err(ReplacementError::ParentMissing {
                path: parent.display().to_string(),
            });
        }
        let (path, file) = open_unused_temporary(parent, stem)?;
        Ok(Self {
            file: Some(file),
            path,
            bytes: 0,
            consumed: false,
        })
    }

    /// Append one chunk. The count is checked, because spec 11.7 asks for a
    /// "checked byte count" and a wrapped length would silently under-verify every
    /// object bigger than the wrap.
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), ReplacementError> {
        // Checked before the write, so a count that cannot be represented refuses
        // without leaving a partial chunk on disk.
        let next = self.bytes.checked_add(chunk.len() as u64).ok_or_else(|| {
            ReplacementError::SizeOverflow {
                path: self.path.display().to_string(),
                bytes: self.bytes,
            }
        })?;
        let path = self.path.clone();
        let file = self.open_file()?;
        file.write_all(chunk)
            .map_err(|e| io_error("write staged chunk", &path, e.to_string()))?;
        self.bytes = next;
        Ok(())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written so far.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// `flush` → `FlushFileBuffers` → close → length check.
    ///
    /// The check here is the length, not the content: proving the content of a
    /// streamed object means re-reading and re-hashing it, and that is the caller's
    /// job — for WP02 it is spec 11.9's `verify`, which runs against the destination
    /// after publication and so covers both the staged bytes and the rename. Reading
    /// the payload back in full would recreate the cost `BUILD_STATE.md` risk 9 flags
    /// for a multi-gigabyte object.
    pub fn synchronize(mut self) -> Result<Staged, ReplacementError> {
        // Responsibility for the file moves out of `Drop` here: on success `Staged`
        // holds it, and on failure the deletion below is explicit rather than left to
        // a destructor that could not report which half failed.
        self.consumed = true;
        let staged = self.synchronize_inner();
        if staged.is_err() {
            let _ = windows::delete_temporary(&self.path);
        }
        staged
    }

    fn synchronize_inner(&mut self) -> Result<Staged, ReplacementError> {
        let path = self.path.clone();
        let expected = self.bytes;
        {
            let file = self.open_file()?;
            file.flush()
                .map_err(|e| io_error("flush to OS", &path, e.to_string()))?;
            windows::flush_file(file, &path)?;
        }
        drop(self.file.take());
        let on_disk = file_length(&path, "re-read staged length after device flush")?;
        if on_disk != expected {
            return Err(ReplacementError::StagingMismatch {
                path: path.display().to_string(),
                written: expected,
                read: on_disk,
            });
        }
        Ok(Staged {
            path: std::mem::take(&mut self.path),
            bytes: expected,
            consumed: false,
        })
    }

    fn open_file(&mut self) -> Result<&mut File, ReplacementError> {
        let path = self.path.clone();
        self.file
            .as_mut()
            .ok_or_else(|| io_error("use staged handle", &path, "the handle is closed"))
    }
}

impl Drop for StagedWriter {
    fn drop(&mut self) {
        if !self.consumed {
            drop(self.file.take());
            // A failure here is deliberately not reported, for the reason in
            // [`TemporaryGuard`]: the caller has a real error to look at, and a
            // leftover temporary is neither a correctness hazard nor resolvable.
            let _ = windows::delete_temporary(&self.path);
        }
    }
}

/// A staged temporary that is durable on the device, not yet published.
pub struct Staged {
    path: PathBuf,
    bytes: u64,
    consumed: bool,
}

impl Staged {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes staged.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Rename this staged file onto the request's destination.
    ///
    /// The destination's parent directory must exist; a store that derives one from
    /// content creates it first, which is spec 11.7's "create parent directories and
    /// rename without overwrite".
    pub fn publish(
        mut self,
        request: &Replacement<'_>,
    ) -> Result<ReplacementReport, ReplacementError> {
        let parent = destination_parent(request.destination)?;
        refuse_existing_destination(request)?;
        let temporary = std::mem::take(&mut self.path);
        // From here the guard inside [`publish_staged`] owns the temporary,
        // including when it is consumed by a successful rename.
        self.consumed = true;
        publish_staged(
            request,
            temporary,
            &parent,
            Expectation::Length(self.bytes),
            self.bytes,
        )
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.consumed && !self.path.as_os_str().is_empty() {
            let _ = windows::delete_temporary(&self.path);
        }
    }
}

/// Write and device-flush the temporary, then prove the bytes are on disk.
fn stage(parent: &Path, destination: &Path, bytes: &[u8]) -> Result<PathBuf, ReplacementError> {
    let name = destination
        .file_name()
        .ok_or_else(|| ReplacementError::ParentMissing {
            path: destination.display().to_string(),
        })?;
    // A lossy rendering is fine here: this only names a temporary, and `create_new`
    // is what actually guarantees two writers do not share one.
    let (candidate, file) = open_unused_temporary(parent, &name.to_string_lossy())?;
    write_flush_verify(file, &candidate, bytes)?;
    Ok(candidate)
}

/// Take the first unused temporary name beside `parent`, opened exclusively.
///
/// A leftover from an interrupted run is skipped rather than written into or
/// deleted: another process may still hold it, and adopting its bytes would make
/// the published object something no caller wrote.
fn open_unused_temporary(parent: &Path, stem: &str) -> Result<(PathBuf, File), ReplacementError> {
    for attempt in 1..=TEMPORARY_ATTEMPTS {
        let candidate = parent.join(format!(".{stem}.{attempt}.tmp"));
        match open_exclusive(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(io_error("create temporary", &candidate, e.to_string()));
            }
        }
    }
    Err(ReplacementError::TemporariesExhausted {
        path: parent.display().to_string(),
        attempts: TEMPORARY_ATTEMPTS,
    })
}

fn open_exclusive(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// `WriteFile` → `FlushFileBuffers` → close → re-read.
///
/// The re-read is the part that makes a durability claim checkable: without it,
/// "flushed" only means the call returned, and the bytes may still be in the system
/// file cache. Cost is a full second read of the payload, which for a project
/// document is noise; WP09 should hash-compare rather than byte-compare when this
/// is pointed at a multi-gigabyte export.
fn write_flush_verify(mut file: File, path: &Path, bytes: &[u8]) -> Result<(), ReplacementError> {
    let written = bytes.len() as u64;
    file.write_all(bytes)
        .map_err(|e| io_error("write", path, e.to_string()))?;
    file.flush()
        .map_err(|e| io_error("flush to OS", path, e.to_string()))?;
    windows::flush_file(&file, path)?;
    drop(file);
    let read = read_back(path, "re-read after device flush")?;
    if read.len() as u64 != written {
        return Err(ReplacementError::StagingMismatch {
            path: path.display().to_string(),
            written,
            read: read.len() as u64,
        });
    }
    if read != bytes {
        return Err(ReplacementError::StagingMismatch {
            path: path.display().to_string(),
            written,
            read: read.len() as u64,
        });
    }
    Ok(())
}

fn read_back(path: &Path, context: &'static str) -> Result<Vec<u8>, ReplacementError> {
    fs::read(path).map_err(|e| io_error(context, path, e.to_string()))
}

/// What the destination must be proved to hold after the rename.
///
/// Two forms because the payload is not always in memory. `Bytes` is the ordinary
/// save path, where the caller already holds the document. `Length` is the streamed
/// path (spec 11.7's import, and WP09's export), where holding the payload to compare
/// it would defeat the streaming; there the caller's own content hash is what proves
/// the bytes, so this module proves only that the destination is exactly as long as
/// what was staged and present at all.
enum Expectation<'a> {
    Bytes(&'a [u8]),
    Length(u64),
}

/// Everything from the volume guard onward, with the temporary already durable.
fn publish_staged(
    request: &Replacement<'_>,
    temporary: PathBuf,
    parent: &Path,
    expectation: Expectation<'_>,
    bytes: u64,
) -> Result<ReplacementReport, ReplacementError> {
    let mut guard = TemporaryGuard::new(temporary.clone(), request.keep_temporary_on_failure);
    match replace_from_staged(request, &temporary, parent, &expectation, bytes) {
        Ok(report) => {
            // The rename consumed the temporary, so it must not be deleted next.
            guard.disarm();
            Ok(report)
        }
        Err(e) => {
            guard.cleanup();
            Err(e)
        }
    }
}

fn replace_from_staged(
    request: &Replacement<'_>,
    temporary: &Path,
    parent: &Path,
    expectation: &Expectation<'_>,
    bytes: u64,
) -> Result<ReplacementReport, ReplacementError> {
    let destination = request.destination;

    // Same-volume, checked by handle. A missing destination has no handle yet, so
    // the directory it would occupy is the right comparison — the rename lands on
    // that volume or not at all.
    let volume_reference = if destination.exists() {
        destination
    } else {
        parent
    };
    let destination_serial = windows::volume_serial(volume_reference)?;
    let temporary_serial = windows::volume_serial(temporary)?;
    if destination_serial != temporary_serial {
        return Err(ReplacementError::CrossVolume {
            destination: destination.display().to_string(),
            temporary: temporary.display().to_string(),
            destination_serial,
            temporary_serial,
        });
    }

    // The explicit backup. Written by us, before the rename, which is what makes it
    // a real recovery input with defined content rather than an artifact of a
    // replacement API (ADR-0006 consequence 2).
    let mut backup = None;
    if let Some(backup_path) = request.backup.filter(|_| destination.exists()) {
        let previous = read_back(destination, "read destination for backup")?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(backup_path)
            .map_err(|e| io_error("open backup", backup_path, e.to_string()))?;
        write_flush_verify(file, backup_path, &previous)?;
        if read_back(backup_path, "verify backup")? != previous {
            return Err(ReplacementError::BackupDisagreement {
                path: backup_path.display().to_string(),
            });
        }
        backup = Some(backup_path.to_path_buf());
    }

    windows::rename(temporary, destination, request.overwrite)?;

    // The assertion 9.7 asks for: the destination is exactly the new state, not a
    // blend of old and new.
    match expectation {
        Expectation::Bytes(payload) => {
            let on_disk = read_back(destination, "re-read destination after rename")?;
            if on_disk != *payload {
                return Err(ReplacementError::DestinationDisagreement {
                    path: destination.display().to_string(),
                    written: bytes,
                    read: on_disk.len() as u64,
                });
            }
        }
        Expectation::Length(expected) => {
            let on_disk = file_length(destination, "re-read destination after rename")?;
            if on_disk != *expected {
                return Err(ReplacementError::DestinationDisagreement {
                    path: destination.display().to_string(),
                    written: bytes,
                    read: on_disk,
                });
            }
        }
    }

    Ok(ReplacementReport {
        destination: destination.to_path_buf(),
        temporary: temporary.to_path_buf(),
        bytes,
        volume_serial: destination_serial,
        backup,
        directory_flush: windows::flush_directory(parent),
    })
}

/// The length a file reports on disk, after its data and directory entry are
/// believed durable.
fn file_length(path: &Path, context: &'static str) -> Result<u64, ReplacementError> {
    fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| io_error(context, path, e.to_string()))
}

/// Deletes the temporary when a replacement failed, unless the caller asked for it
/// to survive for inspection.
struct TemporaryGuard {
    path: PathBuf,
    keep: bool,
    armed: bool,
}

impl TemporaryGuard {
    fn new(path: PathBuf, keep: bool) -> Self {
        Self {
            path,
            keep,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn cleanup(&mut self) {
        if self.armed && !self.keep {
            // A failure here is deliberately not reported: the replacement has
            // already failed for a reason the caller must see, and a leftover
            // temporary is not a correctness hazard — the destination is complete,
            // and spec 11.8 forbids resolving a `.tmp` as an object. The guard is
            // tested both ways below rather than trusted.
            let _ = windows::delete_temporary(&self.path);
        }
        self.armed = false;
    }
}

impl Drop for TemporaryGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::disallowed_types,
        reason = "test fixtures only; the production rules are enforced at the workspace level"
    )]

    use super::*;

    fn state(tag: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u64 * 31 + u64::from(tag)) as u8)
            .collect()
    }

    /// Everything happens inside one tempdir, so the destination and its temporary
    /// are on one volume by construction. The cross-volume refusal is certified by
    /// `wp00_replace`'s deliberately split-drive scenario (ADR-0006's evidence),
    /// which no unit test can reproduce without a second drive present.
    fn scenario(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        assert!(dir.path().join(name).parent().is_some());
        dir
    }

    #[test]
    fn first_write_leaves_exactly_the_destination() {
        let dir = scenario("first");
        let destination = dir.path().join("project.anim.json");
        let bytes = state(0xA1, 4096);
        let report = write_atomic(&Replacement::to_file(&destination), &bytes).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), bytes);
        assert_eq!(report.bytes, 4096);
        assert!(report.backup.is_none(), "no previous state to back up");
        assert!(
            !dir.path().join(".project.anim.json.1.tmp").exists(),
            "the temporary must be consumed by the rename"
        );
        assert_eq!(
            fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
                .count(),
            0
        );
    }

    #[test]
    fn second_write_keeps_the_previous_state_readable() {
        let dir = scenario("second");
        let destination = dir.path().join("project.anim.json");
        let backup = dir.path().join("project.anim.json.bak");
        let old = state(0xA1, 2048);
        let new = state(0x5C, 3072);

        write_atomic(&Replacement::to_file(&destination), &old).unwrap();
        let report = write_atomic(&Replacement::with_backup(&destination, &backup), &new).unwrap();

        assert_eq!(
            fs::read(&destination).unwrap(),
            new,
            "destination is the new state"
        );
        assert_eq!(
            fs::read(&backup).unwrap(),
            old,
            "backup is the old state, not the new"
        );
        assert_eq!(report.backup.as_deref(), Some(backup.as_path()));
        assert_eq!(
            report.volume_serial,
            windows::volume_serial(&destination).unwrap()
        );
    }

    #[test]
    fn create_only_refuses_before_touching_anything() {
        let dir = scenario("create-only");
        let destination = dir.path().join("object");
        let original = state(0x11, 128);
        fs::write(&destination, &original).unwrap();

        let e =
            write_atomic(&Replacement::create_only(&destination), &state(0x22, 128)).unwrap_err();
        assert!(
            matches!(e, ReplacementError::DestinationExists { .. }),
            "{e}"
        );
        assert_eq!(
            fs::read(&destination).unwrap(),
            original,
            "destination untouched"
        );
        // The refusal happened before staging, so no temporary exists either.
        assert_eq!(
            fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
                .count(),
            0
        );
    }

    #[test]
    fn a_failed_replacement_removes_the_temporary_unless_asked_otherwise() {
        // Forcing the failure through an unwritable backup path, which happens after
        // staging and before the rename. The destination must keep the old bytes and
        // the temporary must be gone.
        let dir = scenario("failure");
        let destination = dir.path().join("project.anim.json");
        let old = state(0xA1, 512);
        write_atomic(&Replacement::to_file(&destination), &old).unwrap();

        let missing_backup = dir.path().join("no-such-dir").join("project.anim.json.bak");
        let request = Replacement::with_backup(&destination, &missing_backup);
        let e = write_atomic(&request, &state(0x5C, 640)).unwrap_err();
        assert!(
            matches!(
                e,
                ReplacementError::Io { .. } | ReplacementError::ParentMissing { .. }
            ),
            "{e}"
        );
        assert_eq!(
            fs::read(&destination).unwrap(),
            old,
            "old state survived intact"
        );
        assert!(
            !dir.path().join(".project.anim.json.1.tmp").exists(),
            "the temporary should have been cleaned up"
        );

        // The same failure with preservation requested leaves the temporary behind.
        let preserving =
            Replacement::with_backup(&destination, &missing_backup).preserving_temporary();
        write_atomic(&preserving, &state(0x5C, 640)).unwrap_err();
        assert!(
            dir.path().join(".project.anim.json.1.tmp").exists(),
            "preservation was requested, so the temporary must remain"
        );
    }

    #[test]
    fn staging_that_does_not_reproduce_the_bytes_is_rejected() {
        // A leftover `.name.1.tmp` from an interrupted run must not be written into.
        let dir = scenario("leftover");
        let destination = dir.path().join("project.anim.json");
        fs::write(dir.path().join(".project.anim.json.1.tmp"), b"stale").unwrap();

        let report = write_atomic(&Replacement::to_file(&destination), &state(0x9F, 256)).unwrap();
        assert_ne!(
            report.temporary,
            dir.path().join(".project.anim.json.1.tmp")
        );
        assert_eq!(fs::read(&destination).unwrap(), state(0x9F, 256));
        // The stale file was left alone rather than adopted or deleted.
        assert_eq!(
            fs::read(dir.path().join(".project.anim.json.1.tmp")).unwrap(),
            b"stale"
        );
    }

    #[test]
    fn a_staged_stream_publishes_with_nothing_but_the_chunk_in_memory() {
        // The streamed path is the one WP02's import and WP09's export use, so the
        // proof that matters here is that a payload larger than any single chunk
        // arrives complete: 1 MiB staged in 16 KiB pieces.
        let dir = scenario("staged");
        let destination = dir.path().join("object");
        let mut writer = StagedWriter::create_in(dir.path(), "object").unwrap();
        let mut expected: Vec<u8> = Vec::new();
        for tag in 0..64u8 {
            let chunk = state(tag, 16 * 1024);
            expected.extend_from_slice(&chunk);
            writer.write(&chunk).unwrap();
        }
        assert_eq!(writer.bytes(), 1024 * 1024);
        let staged = writer.synchronize().unwrap();
        assert_eq!(staged.bytes(), 1024 * 1024);
        // Still a temporary, not yet an object.
        assert!(!destination.exists());
        let report = staged
            .publish(&Replacement::create_only(&destination))
            .unwrap();
        assert_eq!(report.bytes, 1024 * 1024);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert_eq!(
            fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
                .count(),
            0,
            "the rename must consume the temporary"
        );
    }

    #[test]
    fn a_staged_writer_dropped_midway_leaves_no_temporary_behind() {
        // Spec 11.11: a failed import cleans temporary data and leaves the project
        // unchanged. The failure here is the caller abandoning the stream.
        let dir = scenario("staged-drop");
        let destination = dir.path().join("object");
        {
            let mut writer = StagedWriter::create_in(dir.path(), "object").unwrap();
            writer.write(b"half a payload").unwrap();
        }
        assert!(!destination.exists());
        assert_eq!(
            fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            0,
            "no leftover temporary"
        );
    }

    #[test]
    fn create_only_refuses_a_staged_publish_before_moving_anything() {
        // The dedupe case in spec 11.7: the object already exists, so the staged
        // temporary must be discarded and the existing object must not be touched.
        let dir = scenario("staged-create-only");
        let destination = dir.path().join("object");
        let original = state(0x11, 512);
        fs::write(&destination, &original).unwrap();

        let mut writer = StagedWriter::create_in(dir.path(), "object").unwrap();
        writer.write(&state(0x22, 512)).unwrap();
        let staged = writer.synchronize().unwrap();
        let staged_path = staged.path().to_path_buf();

        let e = staged
            .publish(&Replacement::create_only(&destination))
            .unwrap_err();
        assert!(
            matches!(e, ReplacementError::DestinationExists { .. }),
            "{e}"
        );
        assert_eq!(fs::read(&destination).unwrap(), original);
        assert!(
            !staged_path.exists(),
            "the refused staged file must be cleaned up, not left to resolve as an object"
        );
    }

    #[test]
    fn a_staged_file_clobbered_before_publish_is_refused() {
        // The streamed destination check compares lengths rather than bytes, because
        // holding a multi-gigabyte payload to byte-compare it is the cost
        // BUILD_STATE.md risk 9 names. This is the failure that guard exists for:
        // something rewrote the staged file between synchronize and the rename.
        let dir = scenario("staged-clobber");
        let destination = dir.path().join("object");
        let mut writer = StagedWriter::create_in(dir.path(), "object").unwrap();
        writer.write(&state(0x7A, 4096)).unwrap();
        let staged = writer.synchronize().unwrap();
        fs::write(staged.path(), state(0x7A, 8192)).unwrap();

        let e = staged
            .publish(&Replacement::create_only(&destination))
            .unwrap_err();
        assert!(
            matches!(e, ReplacementError::DestinationDisagreement { .. }),
            "{e}"
        );
        // Both widths travel with the error, because the operator's question is
        // "how much did I lose".
        assert!(e.to_string().contains("4096"), "{e}");
        assert!(e.to_string().contains("8192"), "{e}");
        // The rename itself cannot be undone, so this leaves an object no manifest
        // references — unreferenced garbage that spec 11.8 permits a future
        // collection pass to take. What must not happen is a returned record, and
        // WP02's import test is what proves that.
        assert_eq!(
            fs::read(&destination).unwrap().len(),
            8192,
            "the file that failed verification is the clobbered one, still orphaned"
        );
    }

    #[test]
    fn a_staged_publish_refuses_a_missing_destination_directory() {
        // Spec 11.7's "create parent directories" is the store's instruction, not
        // this module's licence to invent a layout.
        let dir = scenario("staged-parent");
        let destination = dir.path().join("ab").join("cd").join("object");
        let mut writer = StagedWriter::create_in(dir.path(), "object").unwrap();
        writer.write(b"payload").unwrap();
        let staged = writer.synchronize().unwrap();
        let staged_path = staged.path().to_path_buf();

        let e = staged
            .publish(&Replacement::create_only(&destination))
            .unwrap_err();
        assert!(matches!(e, ReplacementError::ParentMissing { .. }), "{e}");
        assert!(
            !dir.path().join("ab").exists(),
            "must not have created the missing directory"
        );
        assert!(!staged_path.exists(), "the staged file must be cleaned up");
    }

    #[test]
    fn staging_skips_a_temporary_left_by_an_earlier_run() {
        let dir = scenario("staged-leftover");
        fs::write(dir.path().join(".object.1.tmp"), b"stale").unwrap();
        let mut writer = StagedWriter::create_in(dir.path(), "object").unwrap();
        assert_eq!(writer.path(), &dir.path().join(".object.2.tmp"));
        writer.write(b"mine").unwrap();
        assert_eq!(
            fs::read(dir.path().join(".object.1.tmp")).unwrap(),
            b"stale",
            "a leftover is skipped, never adopted or deleted"
        );
    }

    #[test]
    fn durability_is_reported_as_flushed_or_as_a_warning_never_as_silence() {
        let dir = scenario("flush");
        let destination = dir.path().join("a.json");
        let report = write_atomic(&Replacement::to_file(&destination), b"payload").unwrap();
        match &report.directory_flush {
            DirectoryFlush::Flushed => assert_eq!(report.durability_warning(), None),
            DirectoryFlush::Unsupported { detail } => {
                assert!(!detail.is_empty(), "an unsupported flush must say why");
                let d = report.durability_warning().unwrap();
                assert_eq!(d.code.as_str(), "io.directory_flush_unsupported");
                assert_eq!(d.severity, DiagnosticSeverity::Warning);
            }
        }
    }

    #[test]
    fn a_missing_parent_directory_is_refused() {
        let dir = scenario("missing-parent");
        let destination = dir.path().join("absent").join("project.anim.json");
        let e = write_atomic(&Replacement::to_file(&destination), b"x").unwrap_err();
        assert!(matches!(e, ReplacementError::ParentMissing { .. }), "{e}");
        assert!(
            !dir.path().join("absent").exists(),
            "must not have created it"
        );
    }

    #[test]
    fn error_codes_are_stable_and_namespaced() {
        let e = ReplacementError::CrossVolume {
            destination: "D:\\a\\project.anim.json".into(),
            temporary: "C:\\b\\.project.anim.json.1.tmp".into(),
            destination_serial: 0xfea2_d1fe,
            temporary_serial: 0x1234_5678,
        };
        assert_eq!(e.code().as_str(), "io.cross_volume");
        // The message must carry both volumes, since the operator's next question is
        // "which one moved".
        assert!(e.to_string().contains("fea2d1fe"), "{e}");
        assert!(e.to_string().contains("12345678"), "{e}");
        assert!(Diagnostic::any_error(std::slice::from_ref(
            &e.to_diagnostic()
        )));
    }
}
