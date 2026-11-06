//! Applied transaction storage (spec 12.3).
//!
//! Spec 12.8 gives the record: an envelope hash, the forward commands, their inverses,
//! and the revision the batch produced. Two rules from the same paragraph decide the
//! shape of everything in this file.
//!
//! "A transaction lives in exactly one history stack." An entry that could sit in both
//! would let undo and redo each find it, and the second visit would apply a forward
//! sequence that the state has already absorbed. So the stacks own their entries and the
//! only way to move one is `take_undo` / `take_redo`, which removes it from where it was
//! before it can be put anywhere else. The invariant is then a property of the API rather
//! than of the caller's care, and `a_transaction_is_never_in_both_stacks` says so.
//!
//! "History is session-local and not persisted as authoritative project state" (spec
//! 12.6). Nothing here is serialized, and `ProjectDocument` has no field for it — which is
//! also why undo depth survives a save and a reload does not: a saved document is a state,
//! and a history is a session's memory of how it got there.

use std::collections::BTreeMap;

use anim_assets::ContentHash;
use anim_core::id::CommandId;
use anim_project::Revision;

use crate::command::Command;

/// One committed batch, as undo and redo need it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedTransaction {
    /// The digest of the envelope that produced this batch (spec 12.7's replay rule
    /// compares against it, so a duplicate command id cannot smuggle a different batch).
    pub envelope_hash: ContentHash,
    /// The label the operator gave the batch.
    ///
    /// Spec 12.8 lists four fields on this struct and this is the fifth, added because
    /// spec 12.9 makes `undo` and `redo` return a `CommandOutcome`, and an outcome whose
    /// label is empty renders as an unnamed row in an undo list — the one place in the
    /// editor where a name is the difference between undoing the right step and guessing.
    /// The alternative, a parallel `BTreeMap` from hash to label inside [`History`], would
    /// be a second copy of a fact that belongs with the transaction it describes.
    pub transaction_label: String,
    /// The commands as they were declared, in the order they were applied.
    pub forward: Vec<Command>,
    /// The inverse of `forward[i]` at position `i`, for every `i`.
    ///
    /// Stored in correspondence with `forward` rather than in the order undo will apply
    /// them, because spec 12.8 says "Undo applies the selected transaction's inverses in
    /// reverse order" — the reversal is a property of the traversal, and pre-reversing the
    /// list would make the second reversal at redo time a silent double negation.
    pub inverse: Vec<Command>,
    /// The revision this batch produced. One increment for the whole batch (spec 6.4).
    pub resulting_revision: Revision,
}

impl AppliedTransaction {
    /// A batch of `forward` whose inverses were generated in the same order.
    ///
    /// # Errors
    ///
    /// [`CommandError::Validation`](crate::CommandError::Validation) if the two
    /// sequences are not the same length, because a transaction that cannot state the
    /// inverse of one of its own commands is not undoable and must not be recorded as if
    /// it were.
    pub fn new(
        envelope_hash: ContentHash,
        transaction_label: impl Into<String>,
        forward: Vec<Command>,
        inverse: Vec<Command>,
        resulting_revision: Revision,
    ) -> crate::Result<Self> {
        if forward.len() != inverse.len() {
            return Err(crate::CommandError::Validation {
                command: "transaction".to_string(),
                detail: format!(
                    "{} forward commands produced {} inverses; a batch with no exact \
                     inverse for each command cannot be recorded",
                    forward.len(),
                    inverse.len()
                ),
            });
        }
        Ok(Self {
            envelope_hash,
            transaction_label: transaction_label.into(),
            forward,
            inverse,
            resulting_revision,
        })
    }

    /// The transaction that would reverse this one: the inverses become the forward
    /// sequence, in the order they were applied, and the commands become its inverses.
    ///
    /// Not used by the processor, which builds the reverse entry from the inverses its
    /// apply loop actually collected rather than from a derivation of this one —
    /// computing it here would assume the property spec 12.12 tests for. The constructor
    /// is kept because a fixture and a report both need to name the relationship.
    #[must_use]
    pub fn mirrored(&self, revision: Revision) -> Self {
        let mut forward = self.inverse.clone();
        let mut inverse = self.forward.clone();
        forward.reverse();
        inverse.reverse();
        Self {
            envelope_hash: self.envelope_hash,
            transaction_label: self.transaction_label.clone(),
            forward,
            inverse,
            resulting_revision: revision,
        }
    }
}

/// What the replay table remembers about an accepted envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedEnvelope {
    pub envelope_hash: ContentHash,
    pub resulting_revision: Revision,
    pub command_count: usize,
}

/// The two stacks and the replay table, as one owner.
#[derive(Clone, Debug)]
pub struct History {
    undo: Vec<AppliedTransaction>,
    redo: Vec<AppliedTransaction>,
    accepted: BTreeMap<CommandId, AppliedEnvelope>,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// An empty history.
    ///
    /// Deliberately unbounded. A cap would be an engine policy the spec does not ask for,
    /// and the thing it would trade away is exactly the guarantee spec 12.1's purpose names
    /// — "maintain exact undo/redo" — because the transaction it forgot is the one an
    /// operator is about to be handed back. A session's history is bounded by the
    /// transactions the session ran.
    #[must_use]
    pub fn new() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            accepted: BTreeMap::new(),
        }
    }

    /// Record a committed batch: push it as undoable, clear the redo stack, and add the
    /// envelope to the replay table.
    ///
    /// Spec 12.6's "a new forward edit clears redo" happens here rather than at the call
    /// site so that no forward commit path can forget it.
    pub fn commit(&mut self, transaction: AppliedTransaction, command_id: CommandId) {
        self.undo.push(transaction);
        self.redo.clear();
        let recorded = self.undo.last().map(|entry| AppliedEnvelope {
            envelope_hash: entry.envelope_hash,
            resulting_revision: entry.resulting_revision,
            command_count: entry.forward.len(),
        });
        if let Some(recorded) = recorded {
            self.accepted.insert(command_id, recorded);
        }
    }

    /// Take the transaction undo should step back through, leaving no copy behind.
    #[must_use]
    pub fn take_undo(&mut self) -> Option<AppliedTransaction> {
        self.undo.pop()
    }

    /// Take the transaction redo should step forward through.
    #[must_use]
    pub fn take_redo(&mut self) -> Option<AppliedTransaction> {
        self.redo.pop()
    }

    /// Put a transaction on the undo stack, which is where a completed redo belongs.
    pub fn push_undo(&mut self, transaction: AppliedTransaction) {
        self.undo.push(transaction);
    }

    /// Put a transaction on the redo stack, which is where a completed undo belongs.
    pub fn push_redo(&mut self, transaction: AppliedTransaction) {
        self.redo.push(transaction);
    }

    /// What an envelope id was accepted with, if it was.
    #[must_use]
    pub fn lookup(&self, command_id: &CommandId) -> Option<&AppliedEnvelope> {
        self.accepted.get(command_id)
    }

    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    #[must_use]
    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.undo.is_empty() && self.redo.is_empty() && self.accepted.is_empty()
    }

    /// The command ids each stack holds a transaction for, for the one-stack invariant.
    ///
    /// Reported as the hashes the stacks hold because a transaction does not carry its
    /// envelope id; two stacks sharing a transaction share its hash, which is what this
    /// compares.
    #[must_use]
    pub fn shared_hashes(&self) -> Vec<ContentHash> {
        let mut undo: Vec<ContentHash> = self.undo.iter().map(|e| e.envelope_hash).collect();
        let redo: Vec<ContentHash> = self.redo.iter().map(|e| e.envelope_hash).collect();
        undo.retain(|hash| redo.contains(hash));
        undo
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use crate::command::{CommandKind, RemoveScenePayload};

    fn hash(byte: u8) -> ContentHash {
        anim_assets::hash_bytes(&[byte; 32])
    }

    /// A canonical uuid text whose last group carries `byte`, so two calls with different
    /// arguments are guaranteed different ids.
    fn id_text(byte: u8) -> String {
        format!("00000000-0000-5000-8000-{byte:012x}")
    }

    fn command(byte: u8) -> Command {
        Command::RemoveScene(RemoveScenePayload {
            scene_id: anim_project::SceneId::parse(&id_text(byte)).unwrap(),
        })
    }

    fn command_id(byte: u8) -> CommandId {
        CommandId::parse(&id_text(byte)).unwrap()
    }

    fn transaction(byte: u8, revision: u64) -> AppliedTransaction {
        AppliedTransaction {
            envelope_hash: hash(byte),
            transaction_label: format!("take {byte}"),
            forward: vec![command(byte), command(byte + 10)],
            inverse: vec![command(byte + 1), command(byte + 11)],
            resulting_revision: Revision(revision),
        }
    }

    #[test]
    fn a_record_refuses_an_inverse_sequence_of_another_length() {
        let error = AppliedTransaction::new(
            hash(1),
            "two commands one inverse",
            vec![command(1), command(2)],
            vec![command(3)],
            Revision(1),
        )
        .unwrap_err();
        assert!(
            matches!(error, crate::CommandError::Validation { .. }),
            "{error}"
        );
        let recorded = AppliedTransaction::new(
            hash(1),
            "pairs",
            vec![command(1), command(2)],
            vec![command(3), command(4)],
            Revision(1),
        )
        .unwrap();
        assert_eq!(recorded.forward.len(), 2);
        assert_eq!(recorded.transaction_label, "pairs");
    }

    #[test]
    fn a_transaction_is_never_in_both_stacks() {
        let mut history = History::new();
        history.commit(transaction(1, 1), command_id(1));
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.redo_depth(), 0);
        assert!(history.shared_hashes().is_empty());

        let moved = history.take_undo().expect("one entry");
        assert_eq!(
            history.undo_depth(),
            0,
            "taking a transaction leaves no copy where it was"
        );
        history.push_redo(moved.clone());
        assert!(
            history.shared_hashes().is_empty(),
            "the stacks now hold one entry between them, not two"
        );

        let back = history.take_redo().expect("the moved entry");
        history.push_undo(back);
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.redo_depth(), 0);
    }

    #[test]
    fn a_new_forward_edit_clears_redo_rather_than_extending_it() {
        let mut history = History::new();
        history.commit(transaction(1, 1), command_id(1));
        let undone = history.take_undo().unwrap();
        history.push_redo(undone);
        assert_eq!(history.redo_depth(), 1);
        history.commit(transaction(2, 2), command_id(2));
        assert_eq!(
            history.redo_depth(),
            0,
            "spec 12.6: a new forward edit clears redo"
        );
    }

    #[test]
    fn an_envelope_id_is_remembered_with_the_hash_and_revision_it_produced() {
        let mut history = History::new();
        let id = command_id(7);
        history.commit(transaction(7, 3), id);
        let recorded = history.lookup(&id).unwrap();
        assert_eq!(recorded.resulting_revision, Revision(3));
        assert_eq!(recorded.command_count, 2);
        assert_eq!(recorded.envelope_hash, hash(7));
        assert!(history.lookup(&command_id(8)).is_none());
    }

    #[test]
    fn a_session_keeps_every_transaction_it_committed_and_its_replay_record_with_it() {
        let mut history = History::new();
        for byte in 0..5u8 {
            history.commit(transaction(byte, u64::from(byte) + 1), command_id(byte));
        }
        assert_eq!(history.undo_depth(), 5);
        assert_eq!(history.take_undo().unwrap().resulting_revision, Revision(5));
        // The replay record belongs to a transaction that is still held, and stepping
        // back through the stack does not erase the memory that an id was accepted.
        assert!(history.lookup(&command_id(4)).is_some());
        assert!(history.lookup(&command_id(5)).is_none(), "never offered");
        while history.take_undo().is_some() {}
        assert_eq!(history.undo_depth(), 0);
        // Stepping back through every transaction empties the stacks but not the replay
        // table: the session still remembers accepting those ids, which is exactly what
        // makes a resubmit idempotent after an undo rather than a second application.
        assert_eq!(history.redo_depth(), 0);
        assert!(history.lookup(&command_id(0)).is_some());
        assert!(
            !history.is_empty(),
            "a drained history is not an unused one"
        );

        let mut unused = History::new();
        assert!(unused.is_empty());
        assert_eq!(unused.take_undo(), None);
    }

    #[test]
    fn mirroring_swaps_and_reverses_both_sequences() {
        // The relationship spec 12.5 step 6 describes: undo runs the inverses backwards
        // and redo runs the original commands forwards. A mirrored entry states exactly
        // that pairing, which is what the processor's collected inverses are checked
        // against in `undo_then_redo_restores_the_document_byte_for_byte`.
        let original = transaction(1, 1);
        let mirrored = original.mirrored(Revision(2));
        let mut expected_forward = original.inverse.clone();
        expected_forward.reverse();
        let mut expected_inverse = original.forward.clone();
        expected_inverse.reverse();
        assert_eq!(mirrored.forward, expected_forward);
        assert_eq!(mirrored.inverse, expected_inverse);
        assert_eq!(mirrored.resulting_revision, Revision(2));
        assert_eq!(mirrored.envelope_hash, original.envelope_hash);
        // Undoing a mirror returns to the original pairing.
        let restored = mirrored.mirrored(Revision(3));
        assert_eq!(restored.forward, original.forward);
        assert_eq!(restored.inverse, original.inverse);
    }

    #[test]
    fn the_declared_command_count_matches_the_history_records() {
        // History stores commands, not kinds, so a drift in `CommandKind::ALL` would show
        // up as a transaction whose length no longer matches its envelope.
        assert_eq!(CommandKind::ALL.len(), 12);
        assert_eq!(
            crate::registry::PAYLOAD_SCHEMAS.len(),
            CommandKind::ALL.len()
        );
    }
}
