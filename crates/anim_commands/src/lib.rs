//! Envelopes, registry, validation, transaction processing, undo and redo
//! (spec 4.2, 12.3).
//!
//! This crate is the only way project state changes. Spec 12.6 says it in one sentence
//! — "The CommandProcessor is the single writer" — and the rest of the package is what
//! makes that sentence enforceable: a batch executes on a clone and either commits once
//! or is dropped whole, every command returns the inverse that undoes exactly its own
//! effect, and the revision advances by one per batch rather than one per command.
//!
//! Neither crate here depends on evaluation, rendering or editor code (spec 12.4), and
//! nothing here persists a document: `anim_commands` decides whether a mutation is
//! legal, `anim_project` decides what the state is and how it reaches disk.
//!
//! Spec 12.3 lists no `error.rs`, so [`CommandError`] lives at the crate root, as
//! `AssetError` and `ProjectError` do (DECISIONS.md #29).

pub mod command;
pub mod dry_run;
pub mod envelope;
pub mod history;
pub mod processor;
pub mod registry;

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
pub use anim_project::{ProjectDocument, ProjectError, Revision};

pub use command::{Command, CommandKind};
pub use dry_run::ValidationReport;
pub use envelope::{CommandEnvelope, ENVELOPE_SCHEMA_VERSION};
pub use history::AppliedTransaction;
pub use processor::{CommandOutcome, CommandProcessor};
pub use registry::{Registry, PAYLOAD_SCHEMA_VERSION};

/// The package's error taxonomy (spec 12.11).
///
/// Spec 12.11 lists eleven categories across both WP03 crates, and each name appears
/// exactly once, in the crate that raises it: `RevisionConflict` through `RedoEmpty`
/// are command-boundary failures and live here, while `UnsupportedSchema`,
/// `MigrationFailure` and `Persistence` belong to `anim_project`. The envelope's schema
/// check and a document version check surface through the same call, so this enum
/// reaches the other two by chaining [`ProjectError`] rather than by duplicating them —
/// a second `UnsupportedSchema` in a second type would give one fault two category
/// names, which is what spec 7.4's stable categories exist to prevent.
///
/// One variant is an addition, recorded the way DECISIONS.md #29 recorded `AssetError`'s
/// four: `ProjectMismatch`. Spec 12.6 requires "an envelope is accepted only when its
/// project ID … match", the list has no row for it, and it is deliberately *not* folded
/// into `Validation`, because an envelope built against another project is not a bad
/// command — it is a good command aimed at the wrong file, and the two send a reader to
/// different places.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// The envelope expected a revision the document does not hold: someone else saved,
    /// undid or applied first. Spec 12.11's "never commit a batch prefix" is what makes
    /// this refusal harmless rather than destructive.
    #[error(
        "revision conflict: envelope expected revision {expected}, the document is at {actual}"
    )]
    RevisionConflict { expected: u64, actual: u64 },

    /// A command id already accepted, offered again with different bytes. The result of
    /// the first submission is not repeated, because the two envelopes are not the same
    /// transaction (spec 12.7).
    #[error(
        "replay mismatch: command {command_id} was accepted with envelope hash {recorded}, \
         this one hashes to {offered}"
    )]
    CommandReplayMismatch {
        command_id: String,
        recorded: String,
        offered: String,
    },

    /// The envelope is addressed to another project.
    #[error("envelope is addressed to project {expected} and this document is {actual}")]
    ProjectMismatch { expected: String, actual: String },

    /// An empty batch, which spec 12.6 rejects outright: a commit with no effect still
    /// takes a revision, and a revision that means nothing is a snapshot key that means
    /// nothing.
    #[error("envelope {command_id} carries no commands")]
    EmptyBatch { command_id: String },

    /// No handler is registered for this kind at this payload schema version. The
    /// version is in the message because an unregistered kind and a kind whose payload
    /// moved to a second schema are different remedies.
    #[error("no command handler registered for {kind} at payload schema version {schema_version}")]
    UnknownCommand { kind: String, schema_version: u32 },

    /// A command's own preconditions, or a payload that will not decode. Names the
    /// command, because "which command failed" is the report a panel can act on.
    #[error("command {command} is not valid here: {detail}")]
    Validation { command: String, detail: String },

    /// The next commit would pass `u64::MAX`. Checked before any work is done (spec
    /// 12.7's "revision capacity"), and never wraps (spec 10.11).
    #[error("revision cannot advance past {at}")]
    RevisionOverflow { at: u64 },

    /// Nothing to undo. Not an error state — the document is intact — which is why it is
    /// reported as a warning.
    #[error("there is no transaction to undo")]
    UndoEmpty,

    /// Nothing to redo, and the same argument about severity as [`CommandError::UndoEmpty`].
    #[error("there is no transaction to redo")]
    RedoEmpty,

    /// A document-level refusal: schema, migration, validation or persistence, reached
    /// through the command that touched it. Chained rather than flattened, because the
    /// asset id or the offending field inside `ProjectError` is the answer to the only
    /// question a reader has.
    #[error(transparent)]
    Document(#[from] ProjectError),
}

impl CommandError {
    /// The stable category (spec 7.4). Append-only and dot-separated.
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        let raw = match self {
            Self::RevisionConflict { .. } => "command.revision_conflict",
            Self::CommandReplayMismatch { .. } => "command.replay_mismatch",
            Self::ProjectMismatch { .. } => "command.project_mismatch",
            Self::EmptyBatch { .. } => "command.empty_batch",
            Self::UnknownCommand { .. } => "command.unknown_command",
            Self::Validation { .. } => "command.validation",
            Self::RevisionOverflow { .. } => "command.revision_overflow",
            Self::UndoEmpty => "command.undo_empty",
            Self::RedoEmpty => "command.redo_empty",
            Self::Document(_) => "command.document",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    /// An empty history is a question with no answer, not a fault. Everything else here
    /// is a refusal the caller caused.
    #[must_use]
    pub fn severity(&self) -> DiagnosticSeverity {
        match self {
            Self::UndoEmpty | Self::RedoEmpty => DiagnosticSeverity::Warning,
            _ => DiagnosticSeverity::Error,
        }
    }

    /// The structured form spec 7.4 asks of every typed error: ids and revisions as
    /// fields, so "which envelope, which revision" is a filter rather than a parse.
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        let base = Diagnostic::new(self.code(), self.severity(), self.to_string());
        match self {
            Self::RevisionConflict { expected, actual } => base
                .with_context("expected_revision", expected.to_string())
                .with_context("actual_revision", actual.to_string()),
            Self::CommandReplayMismatch {
                command_id,
                recorded,
                offered,
            } => base
                .with_context("command_id", command_id.clone())
                .with_context("recorded_hash", recorded.clone())
                .with_context("offered_hash", offered.clone()),
            Self::ProjectMismatch { expected, actual } => base
                .with_context("expected_project", expected.clone())
                .with_context("actual_project", actual.clone()),
            Self::EmptyBatch { command_id } => base.with_context("command_id", command_id.clone()),
            Self::UnknownCommand {
                kind,
                schema_version,
            } => base
                .with_context("kind", kind.clone())
                .with_context("payload_schema_version", schema_version.to_string()),
            Self::Validation { command, .. } => base.with_context("command", command.clone()),
            Self::RevisionOverflow { at } => base.with_context("revision", at.to_string()),
            Self::UndoEmpty | Self::RedoEmpty => base,
            // The chained error's own code and context survive, which is the point of
            // chaining it: a report that says `command.document` and nothing else has
            // thrown away the diagnosis.
            Self::Document(error) => error.to_diagnostic(),
        }
    }
}

/// The result type every fallible function in this crate returns.
pub type Result<T, E = CommandError> = std::result::Result<T, E>;
