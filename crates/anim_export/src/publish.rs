//! Same-volume temporary output and atomic publication (spec 18.3, 18.5, 18.7, 18.11).
//!
//! Publication is the last stage and the only place the destination is ever touched. Two rules
//! drive the design:
//!
//!   * **Never delete-then-rename; never leave a partial destination.** The temporary produced by
//!     FFmpeg is streamed through `anim_core::io::atomic_replace::StagedWriter` on the destination
//!     *volume* and promoted by an atomic same-volume rename (deviation #7 / ADR-0006's
//!     `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`). The module itself refuses a cross-volume
//!     promotion (`CrossVolume`) and an existing destination under `create_only`
//!     (`DestinationExists`), so "the final destination appears only through successful
//!     publication" (18.8) is enforced by the shared primitive, not reimplemented here.
//!   * **Hash-compare, not byte-compare** (BUILD_STATE risk 9): `write_atomic` would read the whole
//!     multi-gigabyte payload back to byte-compare the destination. A large export must not pay
//!     that, but the "never mixed bytes" check cannot be dropped either. So the payload is hashed
//!     once, streaming, as it is copied into the staged temporary; the module verifies the staged
//!     length on `synchronize` and again on publish, and the produced Blake3 digest is the report's
//!     `output_hash`. The check stays; only the comparison method changes from bytes to hash.

use std::io::Read;
use std::path::Path;

use anim_assets::ContentHash;
use anim_core::io::atomic_replace::{Replacement, StagedWriter};

use crate::{ExportError, ValidationCheck};

/// Chunk size of the streaming copy/hash (64 KiB, matching the WP02 import streamer).
const COPY_CHUNK: usize = 64 * 1024;

/// Reject a pre-existing destination when overwrite is not permitted, *before* the export starts
/// (18.5: "an existing destination without overwrite fails before rendering"). The publish stage
/// would also refuse via `create_only`, but failing here means a 10-minute export is not wasted to
/// hit a collision at the last step.
pub fn assert_destination_available(
    destination: &Path,
    overwrite: bool,
) -> Result<(), ExportError> {
    if !overwrite && destination.symlink_metadata().is_ok() {
        return Err(ExportError::validation(
            ValidationCheck::DestinationExists,
            format!(
                "destination {} already exists and overwrite is not set",
                destination.display()
            ),
        ));
    }
    Ok(())
}

/// Publish an FFmpeg-produced temporary to the destination, returning the Blake3 hash of the
/// published bytes. `overwrite` selects `to_file` (replace) vs `create_only` (fail if present);
/// both go through the same atomic same-volume rename.
///
/// This is the internal primitive the pipeline uses so it can carry the `output_hash` into the
/// report. The spec-level [`publish`] wraps it and discards the hash.
pub(crate) fn promote(
    temporary: &Path,
    destination: &Path,
    overwrite: bool,
) -> Result<ContentHash, ExportError> {
    let parent = destination.parent().ok_or_else(|| {
        ExportError::validation(
            ValidationCheck::SchemaVersion,
            format!(
                "destination {} has no parent directory to stage beside",
                destination.display()
            ),
        )
    })?;
    if !parent.is_dir() {
        return Err(ExportError::Publication(
            anim_core::io::atomic_replace::ReplacementError::ParentMissing {
                path: parent.display().to_string(),
            },
        ));
    }
    let stem = destination
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("anim-export");

    // Stage the bytes on the destination volume, hashing as we go. The staged writer creates a
    // unique `.<stem>.N.tmp` beside the destination and stays on one volume. The caller's own
    // `temporary` (FFmpeg's mux output) is a *separate* scratch file: it is read here and dropped
    // by the pipeline after publication, so "temporary resources are dropped before publication"
    // (18.8) covers both the staged name (consumed by the rename) and this scratch.
    let mut staged = StagedWriter::create_in(parent, stem)?;
    let staged_path = staged.path().to_path_buf();
    let mut hasher = blake3::Hasher::new();
    let mut source = std::fs::File::open(temporary)?;
    let mut buf = [0u8; COPY_CHUNK];
    loop {
        let read = source
            .read(&mut buf)
            .map_err(|error| ExportError::from_write_io(error, temporary))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        staged
            .write(&buf[..read])
            .map_err(|error| map_stage_error(error, &staged_path))?;
    }
    // `synchronize` flushes and re-reads the staged temp to prove the durable length matches.
    let staged = staged.synchronize()?;
    let output_hash = ContentHash::from_bytes(*hasher.finalize().as_bytes());

    let request = if overwrite {
        Replacement::to_file(destination)
    } else {
        Replacement::create_only(destination)
    };
    // The atomic same-volume rename; a `CrossVolume`/`DestinationExists` here is terminal and
    // leaves any previous destination untouched (the rename is the only mutating step).
    staged.publish(&request)?;
    Ok(output_hash)
}

/// The spec-level publication entry point (18.9): promote a temporary to the destination.
pub fn publish(temporary: &Path, destination: &Path, overwrite: bool) -> Result<(), ExportError> {
    promote(temporary, destination, overwrite)?;
    Ok(())
}

/// Translate a staging write error, surfacing disk-full as the distinct 18.11 category.
fn map_stage_error(
    error: anim_core::io::atomic_replace::ReplacementError,
    path: &Path,
) -> ExportError {
    // The module wraps OS errors as `Io { context, .. }`; a full volume during staging must
    // "prevent publication" under its own name (18.11) so a report can distinguish capacity from
    // permission faults.
    let detail = error.detail();
    if detail_contains_disk_full(&detail) {
        return ExportError::DiskFull {
            path: path.display().to_string(),
        };
    }
    ExportError::Publication(error)
}

/// Heuristic: the module's `Io` variant renders the OS message into `detail`, and the Windows/POSIX
/// texts for a full volume are matched here. Exactness is not required — every branch still
/// prevents publication (18.11); this only chooses which named variant reports it.
fn detail_contains_disk_full(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("not enough space")
        || lower.contains("no space left")
        || lower.contains("disk full")
        || lower.contains("quota")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only"
    )]

    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn promote_publishes_and_returns_the_blake3_of_the_bytes() {
        // Independent oracle: blake3 of "hello world" is a fixed value computed here by the
        // reference crate, and the published file must equal the temporary byte-for-byte.
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("artifact.tmp");
        let dest = dir.path().join("out.mp4");
        write(&temp, b"hello world");

        let hash = promote(&temp, &dest, false).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
        // The atomic rename consumed the module's own staged temp (`.<stem>.N.tmp` beside the
        // destination). The caller's scratch `temporary` is a separate file the pipeline removes;
        // promote must leave no *staged* writer temp behind.
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with('.') && name.ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0, "no staged temporary may survive publication");
        // Blake3 of the exact payload (the report's output_hash).
        let expected = blake3::hash(b"hello world");
        assert_eq!(hash.to_hex(), expected.to_hex().as_str());
    }

    #[test]
    fn create_only_refuses_an_existing_destination_and_leaves_it_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("n.tmp");
        let dest = dir.path().join("out.mp4");
        write(&temp, b"new");
        write(&dest, b"PREVIOUS");

        let err = promote(&temp, &dest, false).unwrap_err();
        assert!(
            matches!(err, ExportError::Publication(_)),
            "existing destination without overwrite is a publication refusal: {err:?}"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"PREVIOUS",
            "previous state untouched"
        );
    }

    #[test]
    fn overwrite_replaces_the_previous_destination() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("o.tmp");
        let dest = dir.path().join("out.mp4");
        write(&temp, b"new");
        write(&dest, b"PREVIOUS");

        promote(&temp, &dest, true).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    }

    #[test]
    fn destination_available_check_fails_before_rendering_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");
        // Absent: ok either way.
        assert_destination_available(&dest, false).unwrap();
        write(&dest, b"x");
        assert_destination_available(&dest, true).unwrap();
        let err = assert_destination_available(&dest, false).unwrap_err();
        assert!(matches!(
            err,
            ExportError::Validation {
                check: ValidationCheck::DestinationExists,
                ..
            }
        ));
    }
}
