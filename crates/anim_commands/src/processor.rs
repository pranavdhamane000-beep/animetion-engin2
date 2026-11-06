//! Revision checks, atomic apply, undo and redo (spec 12.3).
//!
//! Spec 12.6's one-sentence design — "The CommandProcessor is the single writer" — is
//! implemented here as one private function that no other path reaches:
//! [`CommandProcessor::execute`]. `apply`, `undo` and `redo` each assemble a sequence of
//! commands and hand it to that function, which clones the document, runs the sequence, and
//! swaps the clone in only if the whole sequence completed and the result validates. So
//! "never commit a batch prefix" (spec 12.11) is not a rule three methods each remember; it
//! is a property of the only code that can change a document at all.
//!
//! The acceptance order is spec 12.7's sentence, in its order: schema, command-id replay,
//! project id, expected revision, non-empty batch, revision capacity. Two of those are
//! deliberate. Replay is checked *before* the revision, because spec 12.7 promises a
//! duplicate accepted command id "the original result" — and after an undo the document is
//! legitimately at a different revision than the one that result names, so checking the
//! revision first would turn an idempotent replay into a conflict. Capacity is checked
//! before any command runs, because a batch that cannot record its own increment should not
//! have spent the work.

use anim_core::id::CommandId;
use anim_project::{ProjectDocument, Revision};

use crate::command::Command;
use crate::dry_run::{self, ValidationReport};
use crate::envelope::CommandEnvelope;
use crate::history::{AppliedTransaction, History};
use crate::registry::Registry;
use crate::{CommandError, Result};

/// What a committed mutation produced.
///
/// Spec 12.10's "successful outcomes provide the revision used to invalidate snapshots"
/// names this type's most important field: `revision` is what every WP04 cache entry is
/// stamped against, and `replayed` tells a caller whether that revision is news.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutcome {
    /// The envelope's id, or `None` for an undo or a redo.
    ///
    /// Spec 12.8's `AppliedTransaction` carries no envelope id — a hash and a label
    /// describe the batch, and inventing an id by truncating one of those would be a
    /// second identity nothing else can produce. Undo and redo are not envelope
    /// submissions, so they have none to report, and saying `None` is truer than a value
    /// that looks like one.
    pub command_id: Option<CommandId>,
    /// The transaction's label, so a report and an undo list can name what happened.
    pub transaction_label: String,
    /// The revision the document holds now.
    pub revision: Revision,
    /// Commands this outcome ran — the size of the batch, not of the history.
    pub command_count: usize,
    /// True when this envelope id had already been accepted and nothing was changed: spec
    /// 12.7's idempotency, reported rather than hidden behind an identical-looking success.
    pub replayed: bool,
}

/// The single writer.
pub struct CommandProcessor {
    document: ProjectDocument,
    registry: Registry,
    history: History,
}

impl CommandProcessor {
    /// A processor over a document, with every command this package registers.
    #[must_use]
    pub fn new(document: ProjectDocument) -> Self {
        Self::with_registry(document, Registry::with_default_handlers())
    }

    /// A processor whose commands a caller has narrowed, which is how spec 12.10's
    /// "future LLM automation … receives no privileged mutation route" is enforced rather
    /// than promised: [`CommandProcessor::check_registered`] refuses a command the
    /// registry does not carry before it reaches a document, so a narrowed registry is a
    /// boundary and not a suggestion.
    #[must_use]
    pub fn with_registry(document: ProjectDocument, registry: Registry) -> Self {
        Self {
            document,
            registry,
            history: History::default(),
        }
    }

    /// The authoritative state. Spec 13.2 forbids downstream crates from mutating what they
    /// read through this, and the borrow is the enforcement: there is no `document_mut`.
    #[must_use]
    pub fn document(&self) -> &ProjectDocument {
        &self.document
    }

    #[must_use]
    pub const fn registry(&self) -> &Registry {
        &self.registry
    }

    #[must_use]
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Check an envelope without changing anything, using the same handlers `apply` will
    /// use (spec 12.9).
    ///
    /// # Errors
    ///
    /// Exactly the error `apply` would return for the same envelope against the same
    /// document, except for a replayed id, which is not reported here: a dry-run asks
    /// "would this be accepted", and an already-accepted envelope would be accepted again
    /// as itself.
    pub fn dry_run(&self, envelope: &CommandEnvelope) -> Result<ValidationReport> {
        self.check_registered(envelope)?;
        dry_run::validate(&self.document, envelope)
    }

    /// Accept or refuse an envelope, and commit it as one unit (spec 12.9).
    ///
    /// # Errors
    ///
    /// Spec 12.11's categories, in spec 12.7's order. A failure anywhere in the sequence
    /// leaves the document byte-for-byte as it was and the history untouched: no partial
    /// batch, no consumed undo entry, no incremented revision.
    pub fn apply(&mut self, envelope: CommandEnvelope) -> Result<CommandOutcome> {
        // 1. Schema first, then the replay table, project id, expected revision, batch
        //    shape and capacity: 12.7's list in 12.7's order.
        envelope.check_schema()?;
        let hash = envelope.hash()?;
        if let Some(recorded) = self.history.lookup(&envelope.command_id) {
            return if recorded.envelope_hash == hash {
                Ok(CommandOutcome {
                    command_id: Some(envelope.command_id),
                    transaction_label: envelope.transaction_label.clone(),
                    revision: recorded.resulting_revision,
                    command_count: recorded.command_count,
                    replayed: true,
                })
            } else {
                Err(CommandError::CommandReplayMismatch {
                    command_id: envelope.command_id.to_canonical_string(),
                    recorded: recorded.envelope_hash.to_hex(),
                    offered: hash.to_hex(),
                })
            };
        }
        self.check_registered(&envelope)?;
        if envelope.project_id != self.document.project_id {
            return Err(CommandError::ProjectMismatch {
                expected: self.document.project_id.to_canonical_string(),
                actual: envelope.project_id.to_canonical_string(),
            });
        }
        if envelope.expected_scene_revision != self.document.revision {
            return Err(CommandError::RevisionConflict {
                expected: envelope.expected_scene_revision.0,
                actual: self.document.revision.0,
            });
        }
        envelope.check_batch_is_nonempty()?;

        // 2. Execute against a clone. `execute` does the capacity check itself, so a batch
        //    that cannot record its own increment refuses before running anything.
        let command_count = envelope.commands.len();
        let (generated, next) = self.execute(&envelope.commands)?;

        // 3. Commit once: swap, one increment, record. The notification spec 12.6 wants
        //    "after commit" is the value returned from here, and spec 12.10's snapshot
        //    invalidation reads `outcome.revision`.
        let transaction = AppliedTransaction::new(
            hash,
            envelope.transaction_label.clone(),
            envelope.commands.clone(),
            generated,
            next,
        )?;
        self.history.commit(transaction, envelope.command_id);

        Ok(CommandOutcome {
            command_id: Some(envelope.command_id),
            transaction_label: envelope.transaction_label,
            revision: next,
            command_count,
            replayed: false,
        })
    }

    /// Step back one transaction and commit the step as a new revision (spec 12.6).
    ///
    /// An undo does not rewind the revision counter: the document moves forward to a state
    /// that happens to equal an earlier one. That is what makes the undo safe to publish —
    /// every snapshot keyed on the previous revision stays distinguishable from this one —
    /// and it is why `resulting_revision` is recorded rather than derived.
    ///
    /// # Errors
    ///
    /// [`CommandError::UndoEmpty`] when there is nothing to step back through, and any
    /// command failure from the inverse sequence. A failure restores the transaction to the
    /// undo stack, so a refused undo leaves the history exactly as it was.
    pub fn undo(&mut self) -> Result<CommandOutcome> {
        let taken = self.history.take_undo();
        self.step_back_through(taken, CommandError::UndoEmpty, true)
    }

    /// Re-apply the last undone transaction, forward, as a new revision.
    ///
    /// # Errors
    ///
    /// [`CommandError::RedoEmpty`], or a command failure that restores the transaction to
    /// the redo stack.
    pub fn redo(&mut self) -> Result<CommandOutcome> {
        let taken = self.history.take_redo();
        self.step_back_through(taken, CommandError::RedoEmpty, false)
    }

    /// The one body undo and redo share: run a transaction's sequence, and move the entry
    /// to the other stack with the revision this step produced.
    ///
    /// `undo` picks which half of the stored transaction runs: undo walks the inverses
    /// backwards, redo runs the commands in their declared order (spec 12.5 step 6). An
    /// entry means the same thing on either stack — `forward` is what redo will apply and
    /// `inverse` is what undo applies in reverse — so stepping back does not mirror the
    /// transaction, it moves it, and only `resulting_revision` changes. That is what
    /// spec 12.8's "a transaction lives in exactly one history stack" is describing, and
    /// building the moved entry from the inverses this step *actually returned* rather than
    /// from the stored pair is what keeps the invariant `inverse[i]` is the inverse of
    /// `forward[i]` a fact about the run instead of a copy of an assumption.
    ///
    /// # Errors
    ///
    /// `empty` when there is no transaction to step through, and any command failure from
    /// the sequence. A failure puts the transaction back where it came from, because a
    /// refused undo that consumed a history entry would be a second kind of damage.
    fn step_back_through(
        &mut self,
        transaction: Option<AppliedTransaction>,
        empty: CommandError,
        undo: bool,
    ) -> Result<CommandOutcome> {
        let transaction = transaction.ok_or(empty)?;
        let sequence: Vec<Command> = if undo {
            transaction.inverse.iter().rev().cloned().collect()
        } else {
            transaction.forward.clone()
        };
        let command_count = sequence.len();
        let label = transaction.transaction_label.clone();

        match self.execute(&sequence) {
            Ok((generated, next)) => {
                let mut inverse = generated;
                if undo {
                    // The step applied the inverses backwards, so what came back is the
                    // forward sequence backwards; reversing it restores the pairing.
                    inverse.reverse();
                }
                let moved = AppliedTransaction::new(
                    transaction.envelope_hash,
                    label.clone(),
                    transaction.forward.clone(),
                    inverse,
                    next,
                )?;
                if undo {
                    self.history.push_redo(moved);
                } else {
                    self.history.push_undo(moved);
                }
                Ok(CommandOutcome {
                    command_id: None,
                    transaction_label: label,
                    revision: next,
                    command_count,
                    replayed: false,
                })
            }
            Err(error) => {
                if undo {
                    self.history.push_undo(transaction);
                } else {
                    self.history.push_redo(transaction);
                }
                Err(error)
            }
        }
    }

    /// Refuse a batch whose commands this registry does not dispatch, before any of them
    /// reaches a document.
    ///
    /// # Errors
    ///
    /// [`CommandError::UnknownCommand`] for the first unregistered kind or version.
    fn check_registered(&self, envelope: &CommandEnvelope) -> Result<()> {
        for command in &envelope.commands {
            self.registry
                .handler(command.kind(), command.payload_schema_version())?;
        }
        Ok(())
    }

    /// The only code in this crate that changes a document.
    ///
    /// Checks capacity, clones, runs the sequence in order, validates, and commits exactly
    /// one increment. Every failure path returns before `self.document` is replaced, which
    /// is why a batch that died at command three of five leaves the caller's document
    /// untouched.
    fn execute(&mut self, sequence: &[Command]) -> Result<(Vec<Command>, Revision)> {
        let next = self
            .document
            .next_revision()
            .map_err(|_| CommandError::RevisionOverflow {
                at: self.document.revision.0,
            })?;

        let mut working = self.document.clone();
        let mut generated: Vec<Command> = Vec::with_capacity(sequence.len());
        for command in sequence {
            generated.push(command.apply(&mut working)?);
        }
        working.validate()?;
        working.revision = next;
        self.document = working;
        Ok((generated, next))
    }
}

/// The transactional helper `anim_project`'s commands call: run a mutation against a clone
/// of the document, validate the result, and commit only if it holds.
///
/// This is spec 12.6's clone-and-swap expressed once, at the level a single command works
/// at, so that a refusal inside a command cannot leave the document half-changed even when
/// the command is being applied outside a batch — which is what `dry_run` and every unit
/// test do.
///
/// # Errors
///
/// Whatever the closure reports, or a document validation failure chained as
/// [`CommandError::Document`].
pub(crate) fn commit_if_valid<T>(
    document: &mut ProjectDocument,
    change: impl FnOnce(&mut ProjectDocument) -> Result<T>,
) -> Result<T> {
    let mut candidate = document.clone();
    let outcome = change(&mut candidate)?;
    candidate.validate().map_err(CommandError::Document)?;
    *document = candidate;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use crate::command::{
        AddNodePayload, PlaceMarkerPayload, RemoveScenePayload, UpsertScenePayload,
    };
    use anim_assets::ContentHash;
    use anim_core::id::{ProjectId, SceneId};
    use anim_core::time::{FramePos, FrameRate};
    use anim_project::{Marker, NodeData, SceneData};

    fn project_id() -> ProjectId {
        ProjectId::parse("00000000-0000-5000-8000-0000000000aa").unwrap()
    }

    fn scene_id() -> SceneId {
        SceneId::parse("00000000-0000-5000-8000-0000000000bb").unwrap()
    }

    fn command_id(byte: u8) -> CommandId {
        CommandId::parse(&format!("00000000-0000-5000-8000-{byte:012x}")).unwrap()
    }

    fn add_scene() -> Command {
        Command::UpsertScene(UpsertScenePayload {
            scene_id: scene_id(),
            scene: SceneData::new("shot01").unwrap(),
        })
    }

    fn remove_scene() -> Command {
        Command::RemoveScene(RemoveScenePayload {
            scene_id: scene_id(),
        })
    }

    fn node_id(byte: u8) -> anim_core::id::NodeId {
        anim_core::id::NodeId::parse(&format!("00000000-0000-5000-8000-{byte:012x}")).unwrap()
    }

    /// A root whose sibling order comes from its id, because two children of one parent
    /// cannot share an order — the rule a fixture that always passed `0` would trip for
    /// the wrong reason.
    fn add_node(byte: u8) -> Command {
        Command::AddNode(AddNodePayload {
            scene_id: scene_id(),
            node: NodeData::new(node_id(byte), None, u32::from(byte), "cel"),
        })
    }

    /// A command that cannot pass against any prefix, which is what makes the rollback
    /// loop below a test of position rather than of one particular refusal.
    fn remove_absent_scene() -> Command {
        Command::RemoveScene(RemoveScenePayload {
            scene_id: SceneId::parse("00000000-0000-5000-8000-0000000000ee").unwrap(),
        })
    }

    fn place_marker(frame: i64, label: &str) -> Command {
        Command::PlaceMarker(PlaceMarkerPayload {
            scene_id: scene_id(),
            marker: Marker::new(FramePos::at_frame(frame), label.to_string()),
        })
    }

    fn fresh() -> ProjectDocument {
        ProjectDocument::new(project_id(), FrameRate::new(24, 1).unwrap())
    }

    fn bytes(document: &ProjectDocument) -> Vec<u8> {
        anim_project::schema::serialize_canonical(document).unwrap()
    }

    /// The authored state, with the revision counter removed.
    ///
    /// `revision` is one of the six persisted fields, so an undo that restores a state
    /// must still differ from the bytes it started at by exactly the counter that moved
    /// forward. Comparing whole documents across an undo would assert that the revision
    /// went backwards, which spec 12.6 and 12.8 both forbid; this is what spec 12.12's
    /// "undo/redo equality" is a claim about.
    fn state_bytes(document: &ProjectDocument) -> Vec<u8> {
        let mut stripped = document.clone();
        stripped.revision = Revision::ZERO;
        bytes(&stripped)
    }

    fn processor() -> CommandProcessor {
        CommandProcessor::new(fresh())
    }

    fn envelope(revision: u64, commands: Vec<Command>) -> CommandEnvelope {
        CommandEnvelope::new(
            project_id(),
            command_id(1),
            Revision(revision),
            "add shot01",
            commands,
        )
    }

    #[test]
    fn one_batch_takes_one_revision_however_many_commands_it_carries() {
        let mut processor = processor();
        let outcome = processor
            .apply(envelope(0, vec![add_scene(), remove_scene()]))
            .unwrap();
        assert_eq!(outcome.revision, Revision(1));
        assert_eq!(outcome.command_count, 2);
        assert_eq!(processor.document().revision, Revision(1));
        assert!(processor.document().scenes.is_empty(), "both ran");
    }

    #[test]
    fn an_empty_batch_is_refused_and_costs_no_revision() {
        let mut processor = processor();
        let error = processor.apply(envelope(0, vec![])).unwrap_err();
        assert_eq!(
            error,
            CommandError::EmptyBatch {
                command_id: command_id(1).to_canonical_string()
            }
        );
        assert_eq!(processor.document().revision, Revision::ZERO);
    }

    #[test]
    fn a_stale_envelope_is_refused_and_leaves_the_document_untouched() {
        let mut processor = processor();
        processor.apply(envelope(0, vec![add_scene()])).unwrap();
        let before = bytes(processor.document());
        let stale = CommandEnvelope::new(
            project_id(),
            command_id(2),
            Revision(0),
            "built against revision 0",
            vec![remove_scene()],
        );
        let error = processor.apply(stale).unwrap_err();
        assert_eq!(
            error,
            CommandError::RevisionConflict {
                expected: 0,
                actual: 1
            },
            "spec 12.6's optimistic-concurrency check"
        );
        assert_eq!(bytes(processor.document()), before);
    }

    #[test]
    fn an_envelope_for_another_project_is_a_separate_refusal() {
        let mut processor = processor();
        let foreign = CommandEnvelope::new(
            ProjectId::parse("00000000-0000-5000-8000-0000000000ff").unwrap(),
            command_id(2),
            Revision::ZERO,
            "wrong project",
            vec![add_scene()],
        );
        let error = processor.apply(foreign).unwrap_err();
        assert!(
            matches!(error, CommandError::ProjectMismatch { .. }),
            "not a validation failure, because nothing in the batch is wrong: {error}"
        );
    }

    #[test]
    fn a_replayed_envelope_returns_the_original_result_and_changes_nothing() {
        let mut processor = processor();
        let first = processor.apply(envelope(0, vec![add_scene()])).unwrap();
        let before = bytes(processor.document());
        let again = processor
            .apply(envelope(0, vec![add_scene()]))
            .expect("a replay is not a failure");
        assert!(again.replayed);
        assert_eq!(again.revision, first.revision);
        assert_eq!(again.command_count, first.command_count);
        assert_eq!(processor.document().revision, Revision(1));
        assert_eq!(bytes(processor.document()), before);
    }

    #[test]
    fn the_same_command_id_with_different_bytes_is_a_replay_mismatch() {
        let mut processor = processor();
        processor.apply(envelope(0, vec![add_scene()])).unwrap();
        let conflicting = CommandEnvelope::new(
            project_id(),
            command_id(1),
            Revision(1),
            "a different batch",
            vec![remove_scene()],
        );
        match processor.apply(conflicting).unwrap_err() {
            CommandError::CommandReplayMismatch {
                command_id: id,
                recorded,
                offered,
            } => {
                assert_eq!(id, command_id(1).to_canonical_string());
                assert_ne!(recorded, offered, "both hashes are named");
            }
            other => panic!("expected a replay mismatch, got {other}"),
        }
        assert_eq!(processor.document().revision, Revision(1));
        assert!(!processor.document().scenes.is_empty(), "nothing applied");
    }

    #[test]
    fn a_batch_that_fails_at_the_second_command_commits_nothing_from_the_first() {
        let mut processor = processor();
        let error = processor
            .apply(envelope(
                0,
                vec![add_scene(), remove_scene(), remove_scene()],
            ))
            .unwrap_err();
        assert!(matches!(error, CommandError::Validation { .. }), "{error}");
        assert_eq!(processor.document().revision, Revision::ZERO);
        assert!(
            processor.document().scenes.is_empty(),
            "command 1 rolled back"
        );
        assert_eq!(
            processor.history().undo_depth(),
            0,
            "and was never recorded"
        );
    }

    #[test]
    fn rollback_is_complete_from_every_command_index_of_a_full_batch() {
        // Spec 12.12's "rollback from every command index". The longest legal batch this
        // package can express is truncated after each position and closed with a command
        // that cannot pass, so the failure lands at a different index every time and each
        // prefix that *would* have committed is checked to have committed nothing.
        let legal = [
            add_scene(),
            add_node(0xc0),
            add_node(0xc1),
            place_marker(1, "start"),
            place_marker(9, "contact"),
        ];
        for index in 0..legal.len() {
            let mut commands: Vec<Command> = legal[..=index].to_vec();
            // Every prefix above is legal on its own and this last command fails against
            // all of them, so the refusal lands at a different index each time round.
            commands.push(remove_absent_scene());
            let mut processor = processor();
            let error = processor
                .apply(envelope(0, commands))
                .expect_err("a scene that is not in the document cannot be removed");
            assert!(
                matches!(error, CommandError::Validation { .. }),
                "position {index}: {error}"
            );
            assert_eq!(
                processor.document(),
                &fresh(),
                "the batch failed at position {} and left a changed document behind",
                index + 1
            );
            assert_eq!(processor.document().revision, Revision::ZERO);
            assert_eq!(processor.history().undo_depth(), 0);
        }
    }

    #[test]
    fn undo_then_redo_restores_the_document_byte_for_byte() {
        let mut processor = processor();
        let before = state_bytes(processor.document());
        processor.apply(envelope(0, vec![add_scene()])).unwrap();
        let applied = state_bytes(processor.document());
        assert_ne!(applied, before);

        let undone = processor.undo().unwrap();
        assert_eq!(undone.revision, Revision(2), "undo commits a new revision");
        assert_eq!(undone.command_id, None, "an undo is not an envelope");
        assert_eq!(state_bytes(processor.document()), before);
        assert_eq!(
            processor.document().revision,
            Revision(2),
            "the state is the earlier one; the counter is not"
        );

        let redone = processor.redo().unwrap();
        assert_eq!(redone.revision, Revision(3));
        assert_eq!(state_bytes(processor.document()), applied);
        assert_eq!(processor.history().undo_depth(), 1);
        assert_eq!(processor.history().redo_depth(), 0);
    }

    #[test]
    fn a_new_forward_edit_clears_the_redo_stack() {
        let mut processor = processor();
        processor.apply(envelope(0, vec![add_scene()])).unwrap();
        processor.undo().unwrap();
        assert_eq!(processor.history().redo_depth(), 1);
        processor
            .apply(CommandEnvelope::new(
                project_id(),
                command_id(4),
                Revision(2),
                "a different edit",
                vec![add_scene()],
            ))
            .unwrap();
        assert_eq!(processor.history().redo_depth(), 0);
        assert_eq!(
            processor.redo().unwrap_err(),
            CommandError::RedoEmpty,
            "and the cleared stack stays cleared"
        );
    }

    #[test]
    fn undoing_a_multi_command_batch_reverses_it_and_redo_reapplies_it() {
        let mut processor = processor();
        processor
            .apply(envelope(
                0,
                vec![add_scene(), add_node(0xc0), add_node(0xc1)],
            ))
            .unwrap();
        let applied = state_bytes(processor.document());
        assert_eq!(processor.document().revision, Revision(1));

        processor.undo().unwrap();
        assert_eq!(
            state_bytes(processor.document()),
            state_bytes(&fresh()),
            "three commands, one step back"
        );

        processor.redo().unwrap();
        assert_eq!(state_bytes(processor.document()), applied);
        assert_eq!(processor.document().revision, Revision(3));
    }

    #[test]
    fn undo_and_redo_on_an_empty_history_are_warnings_not_failures() {
        let mut processor = processor();
        let undo = processor.undo().unwrap_err();
        assert_eq!(undo, CommandError::UndoEmpty);
        assert_eq!(
            undo.severity(),
            anim_core::diagnostic::DiagnosticSeverity::Warning
        );
        assert_eq!(processor.redo().unwrap_err(), CommandError::RedoEmpty);
        assert_eq!(processor.document().revision, Revision::ZERO);
    }

    #[test]
    fn dry_run_and_apply_agree_at_one_revision() {
        // Spec 12.12's equivalence clause, over three envelopes: one that passes, one that
        // is stale, one whose second command fails after the first would have succeeded.
        let mut processor = processor();
        let passing = envelope(0, vec![add_scene()]);
        let report = processor.dry_run(&passing).unwrap();
        assert!(report.would_commit());
        assert_eq!(report.would_be_revision, Some(Revision(1)));
        assert_eq!(report.command_count, 1);
        assert_eq!(report.transaction_label, "add shot01");
        let outcome = processor.apply(passing).unwrap();
        assert_eq!(outcome.revision, report.would_be_revision.unwrap());

        let mut checking = CommandProcessor::new(fresh());
        checking.apply(envelope(0, vec![add_scene()])).unwrap();
        let stale = CommandEnvelope::new(
            project_id(),
            command_id(5),
            Revision(0),
            "stale",
            vec![remove_scene()],
        );
        assert_eq!(
            checking.dry_run(&stale).unwrap_err(),
            checking.apply(stale).unwrap_err(),
            "the same refusal from both paths"
        );

        let mut checking = CommandProcessor::new(fresh());
        checking.apply(envelope(0, vec![add_scene()])).unwrap();
        let broken = CommandEnvelope::new(
            project_id(),
            command_id(6),
            Revision(1),
            "fails halfway",
            vec![remove_scene(), remove_scene()],
        );
        let dry = checking.dry_run(&broken).unwrap_err();
        let wet = checking.apply(broken).unwrap_err();
        assert_eq!(dry, wet, "one implementation, two entry points");
        assert_eq!(checking.document().revision, Revision(1));
    }

    #[test]
    fn a_registry_narrowed_by_the_caller_refuses_commands_it_does_not_carry() {
        // Spec 12.10's "no privileged mutation route", enforced at the boundary rather than
        // asserted in a comment.
        let mut restricted = CommandProcessor::with_registry(fresh(), Registry::new());
        let error = restricted
            .apply(envelope(0, vec![add_scene()]))
            .unwrap_err();
        assert_eq!(
            error,
            CommandError::UnknownCommand {
                kind: "upsert_scene".to_string(),
                schema_version: crate::registry::PAYLOAD_SCHEMA_VERSION
            },
            "an empty registry must refuse before the batch reaches a document"
        );
        assert_eq!(restricted.document().revision, Revision::ZERO);
        // The same batch through a default registry is accepted, which is what makes the
        // above a dispatch refusal rather than a broken command.
        assert!(processor().apply(envelope(0, vec![add_scene()])).is_ok());
    }

    #[test]
    fn a_revision_at_the_width_limit_refuses_before_running_anything() {
        let mut processor = CommandProcessor::new(fresh());
        processor.document.revision = Revision(u64::MAX);
        let error = processor
            .apply(CommandEnvelope::new(
                project_id(),
                command_id(9),
                Revision(u64::MAX),
                "at the limit",
                vec![add_scene()],
            ))
            .unwrap_err();
        assert_eq!(error, CommandError::RevisionOverflow { at: u64::MAX });
        assert_eq!(processor.document().revision, Revision(u64::MAX));
        assert!(processor.document().scenes.is_empty(), "no command ran");
    }

    #[test]
    fn the_hash_of_a_batch_covers_every_field_and_command() {
        let base = envelope(0, vec![add_scene()]);
        let a = base.hash().unwrap();
        let mut renamed = base.clone();
        renamed.transaction_label = "renamed".to_string();
        assert_ne!(a, renamed.hash().unwrap(), "the label is in the pre-image");
        let mut bumped = base.clone();
        bumped.expected_scene_revision = Revision(1);
        assert_ne!(a, bumped.hash().unwrap(), "the expected revision is too");
        let mut longer = base.clone();
        longer.commands.push(remove_scene());
        assert_ne!(
            a,
            longer.hash().unwrap(),
            "and so is the count, which is why the count is written at all"
        );
        let mut reordered = base.clone();
        reordered.commands = vec![remove_scene(), add_scene()];
        let two = CommandEnvelope {
            commands: vec![add_scene(), remove_scene()],
            ..base.clone()
        };
        assert_ne!(
            reordered.hash().unwrap(),
            two.hash().unwrap(),
            "declared order is execution order, so a permutation is a different batch"
        );
    }

    #[test]
    fn a_hash_is_stable_across_rebuilds_of_the_same_envelope() {
        let first = envelope(0, vec![add_scene()]).hash().unwrap();
        let second = envelope(0, vec![add_scene()]).hash().unwrap();
        assert_eq!(first, second);
        assert_eq!(first, ContentHash::from_hex(&first.to_hex()).unwrap());
    }
}
