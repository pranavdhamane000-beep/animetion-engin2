//! Shared validation without commit (spec 12.3).
//!
//! Spec 12.6 is unusually specific about what a dry-run is and what it is not: "Dry-run
//! checks schema, references and invariants, but apply repeats all checks under the writer
//! boundary. This prevents a validate-then-apply race." The sentence rules out the obvious
//! implementation, in which `apply` accepts a `ValidationReport` and skips the work. Here
//! `apply` never looks at a report, and the two paths share one function that runs
//! commands — so what is common between them is the code, not a checklist someone keeps in
//! step.
//!
//! A batch validates *chained*: command `k` sees the state command `k-1` produced, on a
//! clone. Validating each command against the untouched document instead would pass a pair
//! like "remove the only node, then add a child under it" — a batch that must fail exactly
//! the way it fails on apply, which is the clause spec 12.12 calls dry-run/apply
//! equivalence.

use anim_project::ProjectDocument;
use anim_project::Revision;

use crate::envelope::CommandEnvelope;
use crate::{CommandError, Result};

/// What a dry-run established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationReport {
    /// The label from the envelope, so a panel can name the batch it just checked.
    pub transaction_label: String,
    /// Commands checked, in declared order.
    pub command_count: usize,
    /// The revision `apply` would produce, or `None` when the capacity check already
    /// refused. Reporting it lets a caller pre-invalidate the snapshot key it is about to
    /// make dirty (spec 12.10).
    pub would_be_revision: Option<Revision>,
}

impl ValidationReport {
    /// True when the batch would commit.
    #[must_use]
    pub const fn would_commit(&self) -> bool {
        self.would_be_revision.is_some()
    }
}

/// Check an envelope against a document and change nothing.
///
/// Every refusal is returned as the same [`CommandError`] `apply` would return for the
/// same envelope against the same document, which is what makes
/// `dry_run_and_apply_agree_at_one_revision` a comparison of two results rather than of two
/// intentions.
///
/// # Errors
///
/// Whatever [`CommandEnvelope::check_schema`], the document-independent half of spec
/// 12.7's acceptance checks, [`crate::command::Command::apply`] and
/// [`anim_project::ProjectDocument::validate`] report.
pub fn validate(
    document: &ProjectDocument,
    envelope: &CommandEnvelope,
) -> Result<ValidationReport> {
    envelope.check_schema()?;
    if envelope.project_id != document.project_id {
        return Err(CommandError::ProjectMismatch {
            expected: document.project_id.to_canonical_string(),
            actual: envelope.project_id.to_canonical_string(),
        });
    }
    if envelope.expected_scene_revision != document.revision {
        return Err(CommandError::RevisionConflict {
            expected: envelope.expected_scene_revision.0,
            actual: document.revision.0,
        });
    }
    envelope.check_batch_is_nonempty()?;

    let next = document
        .next_revision()
        .map_err(|_| CommandError::RevisionOverflow {
            at: document.revision.0,
        })?;

    // The chain runs on a clone whose result is dropped. `Command::apply` is the shared
    // step, so a divergence between this path and the commit path would have to be a
    // divergence inside one function rather than between two files.
    let mut working = document.clone();
    for command in &envelope.commands {
        command.apply(&mut working)?;
    }
    Ok(ValidationReport {
        transaction_label: envelope.transaction_label.clone(),
        command_count: envelope.commands.len(),
        would_be_revision: Some(next),
    })
}
