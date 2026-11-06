//! Versioned envelopes and batch hashes (spec 12.3).
//!
//! An envelope is the unit of mutation: one transaction label, one batch of commands,
//! one expected revision, one id that says whether it has been accepted before. Spec
//! 12.7's atomic-apply sequence is a list of checks against exactly these six fields,
//! and the `AppliedTransaction.envelope_hash` that spec 12.8 stores is the digest of the
//! canonical byte stream this file defines — which is what makes the replay rule
//! checkable rather than a matter of trusting that two ids that match are the same work.

use anim_assets::ContentHash;
use anim_core::hash::CanonicalBytes;
use anim_core::id::{CommandId, ProjectId};
use anim_project::Revision;
use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::registry::{decode_wire_command, encode_command, WireCommand, PAYLOAD_SCHEMA_VERSION};
use crate::{CommandError, Result};

/// The envelope schema version this build writes and accepts.
///
/// A third version number beside the document's and the manifest's, and the third
/// distinct meaning: this one describes a *transaction's* shape, so an envelope from an
/// older editor can be accepted against a current document without either side
/// pretending the other's schema moved (ADR-0005 Consequence 5, closed).
pub const ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// The domain separator for an envelope's canonical byte stream (ADR-0004 consequence 5).
pub const ENVELOPE_DOMAIN: &str = "anim_engine/envelope/v1";

/// One transaction submitted to the processor.
///
/// Spec 12.8's field list and order, and the order is the persisted order: the derived
/// serializer for the nested payloads writes members as declared, and the hand-written
/// impl below repeats 12.8's sequence rather than an alphabetical one so that an envelope
/// inside a saved record has one spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandEnvelope {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub command_id: CommandId,
    /// The revision the submitter believed the document held. Spec 12.6's
    /// optimistic-concurrency check, named as 12.8 names it even though the revision is
    /// project-wide: an envelope is built against one scene's state, and the field says
    /// which state the sender had in mind.
    pub expected_scene_revision: Revision,
    pub transaction_label: String,
    pub commands: Vec<Command>,
}

impl CommandEnvelope {
    /// Build an envelope at the current schema version.
    #[must_use]
    pub fn new(
        project_id: ProjectId,
        command_id: CommandId,
        expected_scene_revision: Revision,
        transaction_label: impl Into<String>,
        commands: Vec<Command>,
    ) -> Self {
        Self {
            schema_version: ENVELOPE_SCHEMA_VERSION,
            project_id,
            command_id,
            expected_scene_revision,
            transaction_label: transaction_label.into(),
            commands,
        }
    }

    /// The schema check, which spec 12.7 puts first in the acceptance sequence.
    ///
    /// Split from [`CommandEnvelope::check_batch_is_nonempty`] because 12.7's order is
    /// "schema, command ID replay, project ID, expected revision, nonempty batch and
    /// revision capacity" — a batch that is both empty and addressed to the wrong project
    /// has to be refused for the reason the spec lists first, or the two rules would
    /// report different faults in different callers depending on how the checks happened
    /// to be written.
    ///
    /// # Errors
    ///
    /// [`CommandError::Document`] wrapping
    /// [`anim_project::ProjectError::UnsupportedSchema`].
    pub fn check_schema(&self) -> Result<()> {
        if self.schema_version == ENVELOPE_SCHEMA_VERSION {
            Ok(())
        } else {
            Err(CommandError::Document(
                anim_project::ProjectError::UnsupportedSchema {
                    which: "command envelope",
                    found: self.schema_version,
                    supported: ENVELOPE_SCHEMA_VERSION,
                },
            ))
        }
    }

    /// The non-empty batch rule (spec 12.6: "Empty batches are rejected").
    ///
    /// # Errors
    ///
    /// [`CommandError::EmptyBatch`].
    pub fn check_batch_is_nonempty(&self) -> Result<()> {
        if self.commands.is_empty() {
            return Err(CommandError::EmptyBatch {
                command_id: self.command_id.to_canonical_string(),
            });
        }
        Ok(())
    }

    /// Both shape checks, for a caller that has no ordering to honour.
    ///
    /// # Errors
    ///
    /// As [`CommandEnvelope::check_schema`] then [`CommandEnvelope::check_batch_is_nonempty`].
    pub fn check_form(&self) -> Result<()> {
        self.check_schema()?;
        self.check_batch_is_nonempty()
    }

    /// The full canonical byte stream the batch hash is taken over.
    ///
    /// Layout, in the order ADR-0004 fixes: domain tag, then fixed-width values written
    /// raw (`schema_version` as `u32`, the two ids as 16 bytes, the expected revision as
    /// `u64`), then the label length-prefixed, then a command count and each command's
    /// own canonical stream length-prefixed.
    ///
    /// The count is not decoration. A batch of one command whose payload is a prefix of a
    /// batch of two would otherwise hash the same as the longer batch truncated, which is
    /// the framing error ADR-0005 Decision 1 rejected for records and for the same reason.
    ///
    /// # Errors
    ///
    /// Whatever [`Command::canonical_bytes`] reports for a payload that cannot be encoded.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut cp = CanonicalBytes::with_domain(ENVELOPE_DOMAIN).map_err(|error| {
            CommandError::Validation {
                command: "envelope".to_string(),
                detail: error.to_string(),
            }
        })?;
        cp.u32(self.schema_version);
        cp.fixed_bytes(self.project_id.as_bytes());
        cp.fixed_bytes(self.command_id.as_bytes());
        cp.u64(self.expected_scene_revision.0);
        cp.text(&self.transaction_label);
        cp.u64(self.commands.len() as u64);
        for command in &self.commands {
            cp.bytes(&command.canonical_bytes()?);
        }
        Ok(cp.into_vec())
    }

    /// The digest stored as `AppliedTransaction.envelope_hash` (spec 12.8) and compared
    /// on replay (spec 12.7).
    ///
    /// It is `anim_assets::ContentHash` and not a second hash type: spec 12.8 names that
    /// field by that type, and ADR-0005 Decision 3 fixed what a digest looks like inside a
    /// canonical stream.
    ///
    /// # Errors
    ///
    /// As [`CommandEnvelope::canonical_bytes`].
    pub fn hash(&self) -> Result<ContentHash> {
        let bytes = self.canonical_bytes()?;
        Ok(anim_assets::hash_bytes(&bytes))
    }

    /// The kinds this batch declares, in order, for a report or a schema check.
    #[must_use]
    pub fn kinds(&self) -> Vec<crate::command::CommandKind> {
        self.commands.iter().map(Command::kind).collect()
    }
}

impl Serialize for CommandEnvelope {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("CommandEnvelope", 6)?;
        state.serialize_field("schema_version", &self.schema_version)?;
        state.serialize_field("project_id", &self.project_id)?;
        state.serialize_field("command_id", &self.command_id)?;
        state.serialize_field("expected_scene_revision", &self.expected_scene_revision)?;
        state.serialize_field("transaction_label", &self.transaction_label)?;
        // Each command goes out through the registry's tagged form, so the payload schema
        // version travels with the payload it describes rather than with the envelope.
        let mut wire: Vec<WireCommand> = Vec::with_capacity(self.commands.len());
        for command in &self.commands {
            wire.push(encode_command(command).map_err(serde::ser::Error::custom)?);
        }
        state.serialize_field("commands", &wire)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CommandEnvelope {
    /// Hand-written so that reading a batch goes through the same registry dispatch a
    /// submission from JSON would, and so an envelope whose commands the registry does not
    /// know is a refusal rather than a partially-populated batch.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawEnvelope {
            schema_version: u32,
            project_id: ProjectId,
            command_id: CommandId,
            expected_scene_revision: Revision,
            transaction_label: String,
            commands: Vec<WireCommand>,
        }
        let raw = RawEnvelope::deserialize(deserializer)?;
        let mut commands = Vec::with_capacity(raw.commands.len());
        for wire in &raw.commands {
            commands.push(decode_wire_command(wire).map_err(serde::de::Error::custom)?);
        }
        Ok(Self {
            schema_version: raw.schema_version,
            project_id: raw.project_id,
            command_id: raw.command_id,
            expected_scene_revision: raw.expected_scene_revision,
            transaction_label: raw.transaction_label,
            commands,
        })
    }
}

/// The version every payload in a freshly built envelope declares.
///
/// Re-exported for callers that assemble a [`WireCommand`] by hand, so the number is not
/// written as a literal outside the registry's table.
pub const fn current_payload_version() -> u32 {
    PAYLOAD_SCHEMA_VERSION
}
