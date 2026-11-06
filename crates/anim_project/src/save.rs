//! Save orchestration and recovery metadata (spec 12.3).
//!
//! Spec 12.7 prescribes an algorithm — a UUID-named temporary, `ReplaceFileW`, a
//! backup handle — and deviation row 7 replaces two parts of it: `MoveFileExW` with
//! `REPLACE_EXISTING | WRITE_THROUGH` and an explicit `.bak` written by us, plus a
//! `create_new` counter sequence for the temporary name. ADR-0006 rejected
//! `ReplaceFileW` from that API's own documentation (`ERROR_UNABLE_TO_MOVE_REPLACEMENT`
//! leaves the destination path *absent*, which violates spec 9.7's promise that a fault
//! before commit leaves the old bytes readable), and `wp00_replace` proved the
//! replacement on this machine. This file is where that reconciliation lands, and it is
//! the last half of ADR-0006's status line: the recovery *policy* closes here.
//!
//! The sequence uses [`StagedWriter`] rather than [`write_atomic`] for one reason:
//! spec 12.12 requires failures and process kills *at every persistence boundary*, and
//! `write_atomic` is opaque — its boundaries are inside the call. Each step here is a
//! [`Boundary`] the observer is told about, and the byte-for-byte destination check
//! that `write_atomic`'s in-memory path performs is repeated here after publication,
//! because a project document is a few kilobytes and the stronger check is the one
//! G2's clause is about (DECISIONS.md #28's streamed length-only asymmetry does not
//! apply to a payload that is already in memory).

use std::path::{Path, PathBuf};

use anim_assets::ContentHash;
use anim_core::diagnostic::Diagnostic;
use anim_core::id::ProjectId;
use anim_core::io::atomic_replace::{write_atomic, Replacement, ReplacementReport, StagedWriter};

use crate::document::ProjectDocument;
use crate::schema;
use crate::{ProjectError, Result, Revision};

/// The authoritative file's name (spec 4.3).
pub const DOCUMENT_FILE_NAME: &str = "project.anim.json";

/// The suffix of the backup this build writes beside it (spec 4.3:
/// `project.anim.json.bak`).
pub const BACKUP_SUFFIX: &str = ".bak";

/// The autosave's path relative to a project root (spec 4.3: `recovery/`).
pub const AUTOSAVE_RELATIVE: &str = "recovery/autosave.anim.json";

/// The names a project directory is made of (spec 4.3).
///
/// Derived from one root rather than passed in piecewise, because the three files must
/// stay on one volume for the rename to be atomic at all — and a caller that could name
/// an autosave on another drive would produce a `CrossVolume` failure at the point where
/// the damage is least recoverable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPaths {
    pub root: PathBuf,
}

impl ProjectPaths {
    #[must_use]
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The authoritative document (spec 4.3).
    #[must_use]
    pub fn document(&self) -> PathBuf {
        self.root.join(DOCUMENT_FILE_NAME)
    }

    /// The previous state, written by us before the rename. An immutable recovery
    /// input, never a second current project (spec 12.8).
    #[must_use]
    pub fn backup(&self) -> PathBuf {
        let name = format!("{DOCUMENT_FILE_NAME}{BACKUP_SUFFIX}");
        self.root.join(name)
    }

    /// The directory recovery inputs other than the backup live in.
    #[must_use]
    pub fn recovery_dir(&self) -> PathBuf {
        self.root.join("recovery")
    }

    /// The autosave.
    #[must_use]
    pub fn autosave(&self) -> PathBuf {
        self.root.join(AUTOSAVE_RELATIVE)
    }
}

/// A point in the save sequence that can be observed or made to fail.
///
/// The five names cover spec 12.11's four words — "serialization, write, flush or
/// replacement failure" — with `BeforeReplacement` and `AfterReplacement` split because
/// they have opposite outcomes: a failure before the rename must leave the old
/// destination, and a failure after it means the destination is already the new state
/// and the only honest report is that the operation did not complete cleanly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// Before validating and serializing. Nothing on disk has been touched.
    Serialization,
    /// After the bytes exist, before the temporary is created.
    BeforeStaging,
    /// After the temporary holds every byte, before the device flush.
    AfterWrite,
    /// After the device flush, before the rename. The destination still holds the
    /// previous state.
    BeforeReplacement,
    /// After the rename and the destination check.
    AfterReplacement,
}

impl Boundary {
    /// The stable name used in a report, a command line and a test's assertion.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Serialization => "serialization",
            Self::BeforeStaging => "before-staging",
            Self::AfterWrite => "after-write",
            Self::BeforeReplacement => "before-replacement",
            Self::AfterReplacement => "after-replacement",
        }
    }

    /// Every boundary, in the order a save reaches them.
    pub const ALL: [Self; 5] = [
        Self::Serialization,
        Self::BeforeStaging,
        Self::AfterWrite,
        Self::BeforeReplacement,
        Self::AfterReplacement,
    ];
}

impl std::fmt::Display for Boundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Called at each [`Boundary`] while a save runs.
///
/// One mechanism serves both things this package has to prove. Returning `Err` is
/// spec 12.12's injected failure; blocking is spec 12.12's "kill test processes at each
/// save stage", because a save that is parked inside a boundary can be terminated from
/// outside, and the state of the directory afterwards is a fact about the file system
/// rather than about this code's good intentions.
pub type Observer<'a> = &'a mut dyn FnMut(Boundary) -> Result<()>;

/// What a completed save established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveReport {
    /// The authoritative file.
    pub destination: PathBuf,
    pub project_id: ProjectId,
    /// The revision whose bytes are now durable.
    pub revision: Revision,
    /// Bytes the destination holds.
    pub bytes: u64,
    /// The digest of exactly those bytes, which is what [`crate::recovery`] compares
    /// candidates on without re-reading them twice.
    pub digest: ContentHash,
    /// The replacement's own record, including the temporary name that was consumed.
    pub replacement: ReplacementReport,
}

impl SaveReport {
    /// Spec 12.11's visible durability limitation, if there is one.
    ///
    /// A directory-entry flush that the platform does not support is a warning that
    /// travels with the save rather than a silent success, which is what BUILD_STATE
    /// risk 10 asks to stay visible.
    #[must_use]
    pub fn durability_warning(&self) -> Option<Diagnostic> {
        self.replacement.durability_warning()
    }
}

/// The metadata that makes a file a recovery input rather than a guess (spec 12.3's
/// "recovery metadata", spec 12.7's "explicitly recorded revision/generation").
///
/// There is no timestamp in it. Spec 7.1 keeps wall-clock values out of deterministic
/// state and out of hashes, and spec 12.7's rule — never prefer a *newer* manual save to
/// an *older* autosave — is a rule about revisions, which are recorded, ordered and
/// comparable inside the document. A file's mtime could not support the rule on a
/// machine whose clock moved, and it is not what "generation" names here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryMetadata {
    pub project_id: ProjectId,
    pub revision: Revision,
    pub schema_version: u32,
    /// The manifest digest at this revision, so that two files at the same revision but
    /// over different assets are not read as the same state.
    pub manifest_digest: ContentHash,
    /// The digest of the file bytes this metadata was read from.
    pub digest: ContentHash,
}

impl RecoveryMetadata {
    /// Describe a document in memory, at the bytes this build would write for it.
    ///
    /// # Errors
    ///
    /// Whatever [`schema::serialize_canonical`] refuses.
    pub fn for_document(document: &ProjectDocument) -> Result<Self> {
        let bytes = schema::serialize_canonical(document)?;
        Self::for_bytes(document, &bytes)
    }

    /// Describe a document together with the bytes it was read from, so that recovery
    /// can compare a candidate file to the state it claims.
    ///
    /// # Errors
    ///
    /// [`crate::manifest_digest` failures](anim_assets::manifest_digest) and a
    /// serialization failure.
    pub fn for_bytes(document: &ProjectDocument, bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            project_id: document.project_id,
            revision: document.revision,
            schema_version: document.schema_version,
            manifest_digest: anim_assets::manifest_digest(&document.assets)?,
            digest: anim_assets::hash_bytes(bytes),
        })
    }
}

/// Validate, serialize and replace `destination` so it is never partially the new
/// state.
///
/// # Errors
///
/// [`ProjectError::Validation`] or [`ProjectError::UnsupportedSchema`] before anything
/// is created, and [`ProjectError::Persistence`] for anything the file system refuses.
/// In every one of those cases the destination keeps the bytes it had.
pub fn save_atomic(document: &ProjectDocument, destination: &Path) -> Result<SaveReport> {
    save_atomic_observed(document, destination, &mut |_| Ok(()))
}

/// [`save_atomic`] with the sequence observable, which is how both of spec 12.12's
/// persistence clauses are tested.
///
/// # Errors
///
/// As [`save_atomic`], plus whatever the observer returns — which is how an injected
/// failure is reported as if the file system had produced it.
pub fn save_atomic_observed(
    document: &ProjectDocument,
    destination: &Path,
    observer: Observer<'_>,
) -> Result<SaveReport> {
    // 1. Serialization, before disk is involved at all.
    observer(Boundary::Serialization)?;
    let bytes = schema::serialize_canonical(document)?;

    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    if !parent.is_dir() {
        return Err(ProjectError::Persistence {
            context: "save into",
            path: parent.display().to_string(),
            detail: "the directory does not exist; a save never creates a project root".to_string(),
        });
    }

    let backup = backup_path(destination);
    observer(Boundary::BeforeStaging)?;

    // 2. The temporary, created exclusively, named from the destination's own file
    //    name — deviation row 7's counter sequence rather than spec 12.7's UUID,
    //    because `uuid` is v5-only and there is no entropy source in this graph.
    let stem = destination
        .file_name()
        .ok_or_else(|| ProjectError::Persistence {
            context: "save to",
            path: destination.display().to_string(),
            detail: "has no file name to name a temporary from".to_string(),
        })?
        .to_string_lossy()
        .into_owned();
    let mut writer = StagedWriter::create_in(&parent, &stem)
        .map_err(|e| persistence(&parent, "create temporary", &e))?;
    writer
        .write(&bytes)
        .map_err(|e| persistence(writer.path(), "write temporary", &e))?;
    observer(Boundary::AfterWrite)?;

    // 3. Device flush, then the destination is still the previous state.
    let staged = writer
        .synchronize()
        .map_err(|e| persistence(&parent, "flush temporary", &e))?;
    observer(Boundary::BeforeReplacement)?;

    // 4. The replacement. `with_backup` is what writes `.bak` from the destination's
    //    current bytes before the rename, which is what makes the backup a recovery
    //    input with defined content rather than an artifact of a Windows call. The
    //    request is passed unconditionally rather than only when a destination exists:
    //    `anim_core` skips the backup when there is no previous state to copy, and
    //    deciding it here as well would be a second copy of a rule with one owner.
    let request = Replacement::with_backup(destination, &backup);
    let replacement = staged
        .publish(&request)
        .map_err(|e| persistence(destination, "replace destination", &e))?;

    // 5. The check `write_atomic` does for an in-memory payload and `Staged::publish`
    //    deliberately does not: the destination is byte-identical to what was hashed,
    //    not merely the right length. Spec 12.11's "never mixed bytes" is about the
    //    content, and a project file is small enough that reading it back is the honest
    //    way to say so.
    let on_disk = std::fs::read(destination).map_err(|e| ProjectError::Persistence {
        context: "re-read destination",
        path: destination.display().to_string(),
        detail: e.to_string(),
    })?;
    if on_disk != bytes {
        return Err(ProjectError::Persistence {
            context: "verify destination",
            path: destination.display().to_string(),
            detail: format!(
                "holds {} bytes that are not the {} it was published with",
                on_disk.len(),
                bytes.len()
            ),
        });
    }
    observer(Boundary::AfterReplacement)?;

    Ok(SaveReport {
        destination: destination.to_path_buf(),
        project_id: document.project_id,
        revision: document.revision,
        bytes: bytes.len() as u64,
        digest: anim_assets::hash_bytes(&bytes),
        replacement,
    })
}

/// Write the autosave, creating `recovery/` if this is the first one.
///
/// The autosave is a recovery input and never the authoritative record, so it gets no
/// backup of its own and is replaced in place. The directory is created here because
/// [`StagedWriter`] and [`write_atomic`] both refuse a missing parent rather than
/// inventing one an operator did not ask for; this is the one place the `recovery/`
/// directory is legitimately created by a save.
///
/// # Errors
///
/// As [`save_atomic`].
pub fn save_autosave(document: &ProjectDocument, root: &Path) -> Result<SaveReport> {
    let paths = ProjectPaths::from_root(root);
    let destination = paths.autosave();
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ProjectError::Persistence {
            context: "create recovery directory",
            path: parent.display().to_string(),
            detail: e.to_string(),
        })?;
    }
    // No backup of its own and no observer: an autosave is replaceable by definition,
    // and the kill-at-each-stage proof belongs to the authoritative file.
    let bytes = schema::serialize_canonical(document)?;
    let replacement = write_atomic(&Replacement::to_file(&destination), &bytes)
        .map_err(|e| persistence(&destination, "write autosave", &e))?;
    Ok(report(document, &bytes, replacement))
}

/// The backup path for a destination: `project.anim.json` becomes
/// `project.anim.json.bak` (spec 4.3), and any other file gets the same suffix.
#[must_use]
pub fn backup_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    destination.with_file_name(format!("{name}{BACKUP_SUFFIX}"))
}

fn report(document: &ProjectDocument, bytes: &[u8], replacement: ReplacementReport) -> SaveReport {
    SaveReport {
        destination: replacement.destination.clone(),
        project_id: document.project_id,
        revision: document.revision,
        bytes: bytes.len() as u64,
        digest: anim_assets::hash_bytes(bytes),
        replacement,
    }
}

fn persistence(
    path: &Path,
    context: &'static str,
    error: &anim_core::io::atomic_replace::ReplacementError,
) -> ProjectError {
    ProjectError::Persistence {
        context,
        path: path.display().to_string(),
        detail: error.detail(),
    }
}
