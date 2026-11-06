//! Autosave selection and validation (spec 12.3).
//!
//! Spec 12.7's recovery paragraph is three rules and one silence: validate current,
//! backup and autosave *independently*; select only a schema-valid complete document
//! with a matching project id and an explicitly recorded revision; never silently
//! overwrite a newer manual save with an older autosave; present choices when lineage
//! is ambiguous. The silence is what this file is for — spec 12.11's "recovery parse
//! errors preserve all candidate files for diagnosis" means nothing here deletes, and
//! the test that proves it compares the directory before and after.
//!
//! "Generation" in spec 12.7 names [`crate::save::RecoveryMetadata::revision`]. Every
//! other generation counter in the spec is a particle pool or a GPU target, and no file
//! in a project directory carries one, so the recorded revision is the only ordering
//! that exists to compare — which is also why no mtime is read anywhere in this file.

use std::path::{Path, PathBuf};

use anim_core::id::ProjectId;

use crate::document::ProjectDocument;
use crate::save::{ProjectPaths, RecoveryMetadata, BACKUP_SUFFIX};
use crate::schema;

/// Which of the three names on disk a candidate arrived under (spec 4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateKind {
    /// The authoritative record.
    Current,
    /// The previous state, written by the last successful replacement.
    Backup,
    /// The most recent automatic save.
    Autosave,
}

impl CandidateKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Backup => "backup",
            Self::Autosave => "autosave",
        }
    }

    /// The order candidates are reported and tie-broken in: authoritative first, so a
    /// choice between two identical files prefers the one the project already names.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Current => 0,
            Self::Backup => 1,
            Self::Autosave => 2,
        }
    }
}

/// What was found at one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateState {
    /// Nothing is at the path. Distinct from unreadable: an absent file is expected on
    /// a first save, and an operator should not be told a backup is corrupt when it was
    /// never written.
    Absent,
    /// Bytes exist but are not a usable document. The message is the load failure, kept
    /// verbatim, and the file is left exactly where it is.
    Unreadable(String),
    /// A complete document that validates, with the metadata that ranks it.
    Complete(CompleteCandidate),
}

/// A candidate that can be selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompleteCandidate {
    pub document: ProjectDocument,
    pub metadata: RecoveryMetadata,
    /// Whether the file's bytes are what this build would write for that document.
    ///
    /// A file that needed migration is complete and selectable but not canonical, and
    /// saying so is the difference between "recovered" and "recovered and already
    /// rewritten", which is a claim a report must not make on an operator's behalf.
    pub canonical: bool,
}

/// One of the three files, as found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub path: PathBuf,
    pub state: CandidateState,
}

impl Candidate {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self.state, CandidateState::Complete(_))
    }

    /// The revision the file records, if it records one.
    #[must_use]
    pub fn revision(&self) -> Option<crate::Revision> {
        match &self.state {
            CandidateState::Complete(complete) => Some(complete.metadata.revision),
            _ => None,
        }
    }
}

/// Read and validate each recovery input independently, writing nothing.
///
/// The order is [`CandidateKind::rank`]'s, so a report lists current, backup, autosave
/// every time rather than in whatever order the paths were built.
///
/// A file that fails to parse is reported and preserved; a file that fails *validation*
/// after parsing is the same, and neither one is repaired here, because a repair is a
/// write and spec 12.7 makes selection the only action recovery is allowed to take
/// before a human agrees to one.
#[must_use]
pub fn inspect(paths: &ProjectPaths) -> Vec<Candidate> {
    [
        (CandidateKind::Current, paths.document()),
        (CandidateKind::Backup, paths.backup()),
        (CandidateKind::Autosave, paths.autosave()),
    ]
    .into_iter()
    .map(|(kind, path)| Candidate {
        state: read_candidate(&path).state,
        kind,
        path,
    })
    .collect()
}

/// Inspect one path by itself, which is what a report or a fixture check needs.
#[must_use]
pub fn read_candidate(path: &Path) -> Candidate {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Candidate {
                kind: kind_of(path),
                path: path.to_path_buf(),
                state: CandidateState::Absent,
            };
        }
        Err(e) => {
            return Candidate {
                kind: kind_of(path),
                path: path.to_path_buf(),
                state: CandidateState::Unreadable(format!("could not be read: {e}")),
            };
        }
    };
    let state = match schema::deserialize_canonical(&bytes) {
        Ok(document) => {
            let metadata = match RecoveryMetadata::for_document(&document) {
                Ok(metadata) => metadata,
                Err(e) => {
                    return Candidate {
                        kind: kind_of(path),
                        path: path.to_path_buf(),
                        state: CandidateState::Unreadable(e.to_string()),
                    }
                }
            };
            // `canonical` is the round trip spec 12.12 names, applied to a recovery
            // input: the bytes on disk against the bytes this build writes for the
            // document they decoded into.
            let canonical =
                schema::serialize_canonical(&document).is_ok_and(|written| written == bytes);
            CandidateState::Complete(CompleteCandidate {
                document,
                metadata,
                canonical,
            })
        }
        Err(e) => CandidateState::Unreadable(e.to_string()),
    };
    Candidate {
        kind: kind_of(path),
        path: path.to_path_buf(),
        state,
    }
}

/// The kind a path names, from spec 4.3's layout.
///
/// Textual, because a candidate's identity is which rule of 12.7 applies to it — the
/// authoritative file, an immutable backup, a disposable autosave — and that is decided
/// by where it lives, not by anything inside it.
#[must_use]
pub fn kind_of(path: &Path) -> CandidateKind {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.ends_with(BACKUP_SUFFIX) {
        CandidateKind::Backup
    } else if name.starts_with("autosave.") {
        CandidateKind::Autosave
    } else {
        CandidateKind::Current
    }
}

/// What recovery may do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// The authoritative file is the best complete record. Nothing to restore, and the
    /// other inputs stay where they are.
    UseCurrent,
    /// One candidate is usable and the authoritative file is not. Restoring it is a
    /// repair rather than a choice, so it names the single candidate to copy from — and
    /// copying is the caller's write, never this function's. Boxed because a candidate
    /// carries a whole document, and an enum whose common case is a unit variant should
    /// not pay for the uncommon one on the stack.
    Restore(Box<Candidate>),
    /// Two or more complete records disagree about the project's lineage, or a recovery
    /// input is ahead of the authoritative file. Spec 12.7 requires a choice to be
    /// *presented*; every candidate in the list is still on disk, untouched.
    Choose(Vec<Candidate>),
    /// Nothing usable was found. The list is what was seen, with each file's failure, so
    /// the operator can diagnose instead of guessing which of three files is intact.
    NothingUsable(Vec<Candidate>),
}

impl RecoveryDecision {
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::UseCurrent => "use-current",
            Self::Restore(_) => "restore",
            Self::Choose(_) => "choose",
            Self::NothingUsable(_) => "nothing-usable",
        }
    }
}

/// Apply spec 12.7's selection rules to what [`inspect`] found.
///
/// A candidate whose `project_id` is not `expected` is excluded from selection but kept
/// in every reported list: pointing the operator at a file from another project is worse
/// than saying the current one is unreadable, and the file itself is the diagnosis.
#[must_use]
pub fn select(candidates: &[Candidate], expected: ProjectId) -> RecoveryDecision {
    let eligible: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| {
            candidate.is_complete()
                && matches!(&candidate.state, CandidateState::Complete(complete)
                     if complete.document.project_id == expected)
        })
        .cloned()
        .collect();

    let highest = eligible
        .iter()
        .filter_map(|candidate| match &candidate.state {
            CandidateState::Complete(complete) => Some(complete.metadata.revision),
            CandidateState::Absent | CandidateState::Unreadable(_) => None,
        })
        .max();

    let Some(highest) = highest else {
        return RecoveryDecision::NothingUsable(candidates.to_vec());
    };

    let mut top: Vec<Candidate> = eligible
        .iter()
        .filter(|candidate| candidate.revision() == Some(highest))
        .cloned()
        .collect();
    top.sort_by_key(|candidate| candidate.kind.rank());

    // Same revision, same content: one state written under two names, which a
    // successful save produces on its first backup. Not ambiguous by any reading of
    // spec 12.7, so prefer the authoritative name.
    let all_agree = top
        .windows(2)
        .all(|pair| digest_of(&pair[0]) == digest_of(&pair[1]));
    if !all_agree {
        return RecoveryDecision::Choose(top);
    }

    let current_is_complete = candidates
        .iter()
        .any(|candidate| candidate.kind == CandidateKind::Current && candidate.is_complete());

    if top[0].kind == CandidateKind::Current {
        // Either the current file is the highest state there is, or it ties with an
        // identical copy of itself. Both mean: use it, and do not touch the others.
        RecoveryDecision::UseCurrent
    } else if current_is_complete {
        // The authoritative file is readable and *behind*. An autosave ahead of a manual
        // save is the case spec 12.7 refuses to resolve silently, and the converse
        // mistake — preferring an autosave because it is the only candidate that is not
        // the current file — is the same mistake in the other direction.
        RecoveryDecision::Choose(eligible)
    } else {
        RecoveryDecision::Restore(Box::new(top.remove(0)))
    }
}

fn digest_of(candidate: &Candidate) -> Option<anim_assets::ContentHash> {
    match &candidate.state {
        CandidateState::Complete(complete) => Some(complete.metadata.digest),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]
    #![allow(
        clippy::disallowed_methods,
        reason = "removing a file here simulates the loss a recovery scan is supposed to \
                  survive; the rule the lint protects is about the save path, and this \
                  module writes nothing except through these fixtures"
    )]

    use super::*;
    use anim_core::id::Revision;
    use anim_core::time::FrameRate;
    use uuid::Uuid;

    fn project_id() -> ProjectId {
        ProjectId::derive(
            &Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            b"recovery-fixture",
        )
    }

    fn at(revision: u64) -> ProjectDocument {
        let mut document = ProjectDocument::new(project_id(), FrameRate::new(24, 1).unwrap());
        document.revision = Revision(revision);
        document
    }

    fn write(paths: &ProjectPaths, document: &ProjectDocument, autosave: bool) {
        if autosave {
            crate::save::save_autosave(document, &paths.root).unwrap();
        } else {
            crate::save::save_atomic(document, &paths.document()).unwrap();
        }
    }

    #[test]
    fn a_missing_everything_is_reported_as_nothing_usable_not_as_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        let candidates = inspect(&paths);
        assert_eq!(candidates.len(), 3);
        assert!(candidates
            .iter()
            .all(|c| matches!(c.state, CandidateState::Absent)));
        assert_eq!(
            select(&candidates, project_id()),
            RecoveryDecision::NothingUsable(candidates)
        );
    }

    #[test]
    fn a_current_file_is_used_and_nothing_else_is_consulted_for_action() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        write(&paths, &at(0), false);
        let candidates = inspect(&paths);
        assert_eq!(
            select(&candidates, project_id()),
            RecoveryDecision::UseCurrent
        );
    }

    #[test]
    fn a_backup_written_by_the_first_replacement_ties_with_a_identical_current_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        let first = at(1);
        write(&paths, &first, false);
        let second = at(2);
        write(&paths, &second, false);

        let candidates = inspect(&paths);
        let backup = candidates
            .iter()
            .find(|c| c.kind == CandidateKind::Backup)
            .expect("the second save made a backup");
        assert_eq!(backup.revision(), Some(Revision(1)));
        assert!(backup.is_complete());
        // Current is ahead, so it wins outright.
        assert_eq!(
            select(&candidates, project_id()),
            RecoveryDecision::UseCurrent
        );
    }

    #[test]
    fn an_unreadable_current_file_repairs_from_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        write(&paths, &at(1), false);
        write(&paths, &at(2), false);
        std::fs::write(paths.document(), b"{ truncated").unwrap();

        let candidates = inspect(&paths);
        let current = &candidates[0];
        assert!(
            matches!(&current.state, CandidateState::Unreadable(message) if !message.is_empty()),
            "{current:?}"
        );
        let decision = select(&candidates, project_id());
        match decision {
            RecoveryDecision::Restore(candidate) => {
                assert_eq!(candidate.kind, CandidateKind::Backup);
                assert_eq!(candidate.revision(), Some(Revision(1)));
            }
            other => panic!("expected a repair from the backup, got {other:?}"),
        }
        // Spec 12.11: the broken file survives for diagnosis.
        assert_eq!(std::fs::read(paths.document()).unwrap(), b"{ truncated");
    }

    #[test]
    fn an_autosave_ahead_of_the_manual_save_is_a_choice_not_a_restore() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        write(&paths, &at(5), false);
        write(&paths, &at(9), true);

        let candidates = inspect(&paths);
        let decision = select(&candidates, project_id());
        let RecoveryDecision::Choose(list) = &decision else {
            panic!(
                "12.7 forbids resolving this silently, got {:?}",
                decision.kind_name()
            );
        };
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|c| c.kind == CandidateKind::Current));
        assert!(list.iter().any(|c| c.kind == CandidateKind::Autosave));
    }

    #[test]
    fn an_older_autosave_never_displaces_a_newer_manual_save() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        write(&paths, &at(9), false);
        write(&paths, &at(3), true);

        let candidates = inspect(&paths);
        assert_eq!(
            select(&candidates, project_id()),
            RecoveryDecision::UseCurrent,
            "spec 12.7's rule, and the reason no code path here compares file times"
        );
    }

    #[test]
    fn a_document_from_another_project_is_never_a_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        write(&paths, &at(4), false);
        let other = ProjectDocument::new(
            ProjectId::derive(
                &Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
                b"a-different-project",
            ),
            FrameRate::new(24, 1).unwrap(),
        );
        std::fs::write(paths.backup(), serde_json::to_vec(&other).unwrap()).unwrap();

        let candidates = inspect(&paths);
        let backup = candidates
            .iter()
            .find(|c| c.kind == CandidateKind::Backup)
            .unwrap();
        assert!(backup.is_complete(), "it parses and validates");
        // Current is rev 4, the foreign backup is rev 0, and only current is eligible.
        assert_eq!(
            select(&candidates, project_id()),
            RecoveryDecision::UseCurrent
        );

        // With the current file gone, the foreign backup is still not a repair.
        std::fs::remove_file(paths.document()).unwrap();
        let without_current = inspect(&paths);
        assert!(matches!(
            select(&without_current, project_id()),
            RecoveryDecision::NothingUsable(_)
        ));
        assert!(
            std::fs::read(paths.backup()).is_ok(),
            "and it is preserved rather than deleted for being foreign"
        );
    }

    #[test]
    fn inspection_and_selection_write_nothing_anywhere() {
        // The structural half of "never silently overwrite": this module's read path has
        // no write in it, and this test makes that a fact about the directory rather
        // than a claim about the code.
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        write(&paths, &at(1), false);
        write(&paths, &at(2), false);
        write(&paths, &at(1), true);

        let before = listing_with_digests(dir.path());
        let candidates = inspect(&paths);
        let _ = select(&candidates, project_id());
        let after = listing_with_digests(dir.path());
        assert_eq!(
            before, after,
            "recovery selection must not create, delete, touch or rewrite a file"
        );
    }

    fn listing_with_digests(root: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        collect(root, &mut out);
        out.sort();
        out
    }

    fn collect(dir: &Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                collect(&path, out);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.push((
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    anim_assets::hash_bytes(&bytes).to_hex(),
                ));
            }
        }
    }

    #[test]
    fn a_file_with_the_right_values_in_the_wrong_field_order_is_usable_and_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        let document = at(7);
        // Written through `serde_json::Value`, whose map is a `BTreeMap<String, _>`, so
        // every object's members come out alphabetically instead of in the order this
        // build's serializers emit. The file parses, validates and is fully usable; it
        // is simply not the byte stream `anim_project` writes — which is the state a
        // migrated file arrives in too, and the reason `canonical` exists as a reported
        // field rather than as an assumption.
        let unsorted = serde_json::to_vec(&serde_json::to_value(&document).unwrap()).unwrap();
        assert_ne!(
            unsorted,
            schema::serialize_canonical(&document).unwrap(),
            "the two spellings really do differ, or this test proves nothing"
        );
        std::fs::write(paths.document(), &unsorted).unwrap();

        let candidates = inspect(&paths);
        // The authoritative file is readable and complete, so it is used: recovery does
        // not prefer a newer *spelling* of a document over the document itself.
        assert_eq!(
            select(&candidates, project_id()),
            RecoveryDecision::UseCurrent
        );
        let CandidateState::Complete(complete) = &candidates[0].state else {
            panic!(
                "the current file is the complete one: {:?}",
                candidates[0].state
            );
        };
        assert_eq!(complete.metadata.revision, Revision(7));
        assert!(
            !complete.canonical,
            "and the report says so, rather than implying a rewrite that did not happen"
        );
        // The metadata is the digest of the canonical bytes, so the same state in either
        // spelling compares equal to itself.
        assert_eq!(
            complete.metadata.digest,
            RecoveryMetadata::for_document(&document).unwrap().digest
        );
    }
}
