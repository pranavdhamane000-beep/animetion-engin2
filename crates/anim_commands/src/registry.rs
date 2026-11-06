//! Payload handlers and schema dispatch (spec 12.3).
//!
//! Spec 12.2 requires that "all command payload types have versioned JSON schemas" and
//! 12.5 step 4 that an envelope carry a schema check. Two things follow, and both live
//! here rather than being spread across the command file: a payload's wire form names
//! itself (`kind` plus a `schema_version` beside the payload bytes), and one table says
//! what fields each kind has — which is also what `schemas/commands/v1.json` must agree
//! with, so the committed artifact and the encoder have a single source to be checked
//! against.
//!
//! The dispatch is keyed on `(kind, payload_schema_version)` rather than on kind alone.
//! A second version of one payload is a different handler, so an old envelope keeps
//! arriving at the code that understands it while a new one is refused rather than
//! silently reinterpreted — which is the same argument ADR-0005 gives for hashing both
//! version numbers of a manifest.

use std::collections::BTreeMap;

use anim_core::hash::CanonicalBytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::command::{Command, CommandKind};
use crate::{CommandError, Result};

/// The payload schema version every kind in this build declares.
///
/// One constant today because there is one version of anything; the registry keys on
/// `(kind, version)` regardless, so the first payload to need a second version adds a
/// handler rather than restructuring this file.
pub const PAYLOAD_SCHEMA_VERSION: u32 = 1;

/// The domain separator for one payload's canonical stream.
///
/// Unused by the envelope hash, which nests payload bytes under the envelope's own tag
/// (spec 12.7's "complete canonical envelope hash" is one digest over the whole batch).
/// It exists for a caller that wants to hash a single payload on its own and must not
/// invent a second tag to do it.
pub const PAYLOAD_DOMAIN: &str = "anim_engine/payload/v1";

/// The tagged wire form of one command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCommand {
    pub kind: String,
    pub schema_version: u32,
    pub payload: Value,
}

/// One field of a payload: its name and its JSON type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayloadField {
    pub name: &'static str,
    /// `string`, `object`, `integer`, `boolean` — the JSON type, with typed ids spelled
    /// `string` and documented as canonical uuid text by the schema file.
    pub json_type: &'static str,
}

/// The declared shape of one payload kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayloadSchema {
    pub kind: CommandKind,
    pub version: u32,
    pub fields: &'static [PayloadField],
}

/// Every payload's field table, in the order the enum declares its variants.
///
/// This table is what `schemas/commands/v1.json` is generated from and what the drift
/// test compares against the bytes the encoder actually writes. Writing the schema file by
/// hand and this table by hand, with a test between them, is the arrangement chosen over a
/// schema validator dependency: spec 5.1 names no JSON Schema crate, and the failure worth
/// preventing is not a malformed document but the artifact and the encoder disagreeing.
pub const PAYLOAD_SCHEMAS: &[PayloadSchema] = &[
    PayloadSchema {
        kind: CommandKind::UpsertAsset,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[PayloadField {
            name: "record",
            json_type: "object",
        }],
    },
    PayloadSchema {
        kind: CommandKind::RemoveAsset,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[PayloadField {
            name: "asset_id",
            json_type: "string",
        }],
    },
    PayloadSchema {
        kind: CommandKind::UpsertScene,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[
            PayloadField {
                name: "scene_id",
                json_type: "string",
            },
            PayloadField {
                name: "scene",
                json_type: "object",
            },
        ],
    },
    PayloadSchema {
        kind: CommandKind::RemoveScene,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[PayloadField {
            name: "scene_id",
            json_type: "string",
        }],
    },
    PayloadSchema {
        kind: CommandKind::AddNode,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[
            PayloadField {
                name: "scene_id",
                json_type: "string",
            },
            PayloadField {
                name: "node",
                json_type: "object",
            },
        ],
    },
    PayloadSchema {
        kind: CommandKind::RemoveNode,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[
            PayloadField {
                name: "scene_id",
                json_type: "string",
            },
            PayloadField {
                name: "node_id",
                json_type: "string",
            },
        ],
    },
    PayloadSchema {
        kind: CommandKind::UpsertTrack,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[
            PayloadField {
                name: "scene_id",
                json_type: "string",
            },
            PayloadField {
                name: "track",
                json_type: "object",
            },
        ],
    },
    PayloadSchema {
        kind: CommandKind::RemoveTrack,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[
            PayloadField {
                name: "scene_id",
                json_type: "string",
            },
            PayloadField {
                name: "track_id",
                json_type: "string",
            },
        ],
    },
    PayloadSchema {
        kind: CommandKind::SetPropertyKey,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[
            PayloadField {
                name: "scene_id",
                json_type: "string",
            },
            PayloadField {
                name: "track_id",
                json_type: "string",
            },
            PayloadField {
                name: "key",
                json_type: "object",
            },
        ],
    },
    PayloadSchema {
        kind: CommandKind::RemovePropertyKey,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[
            PayloadField {
                name: "scene_id",
                json_type: "string",
            },
            PayloadField {
                name: "track_id",
                json_type: "string",
            },
            PayloadField {
                name: "position",
                json_type: "object",
            },
        ],
    },
    PayloadSchema {
        kind: CommandKind::PlaceMarker,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[
            PayloadField {
                name: "scene_id",
                json_type: "string",
            },
            PayloadField {
                name: "marker",
                json_type: "object",
            },
        ],
    },
    PayloadSchema {
        kind: CommandKind::RemoveMarker,
        version: PAYLOAD_SCHEMA_VERSION,
        fields: &[
            PayloadField {
                name: "scene_id",
                json_type: "string",
            },
            PayloadField {
                name: "marker",
                json_type: "object",
            },
        ],
    },
];

/// The declared field names of one kind, for a report that must not repeat the table.
#[must_use]
pub fn field_names(kind: CommandKind) -> Vec<&'static str> {
    PAYLOAD_SCHEMAS
        .iter()
        .find(|schema| schema.kind == kind)
        .map(|schema| schema.fields.iter().map(|field| field.name).collect())
        .unwrap_or_default()
}

/// One kind's handler at one payload version.
#[derive(Clone, Copy)]
pub struct Handler {
    kind: CommandKind,
    version: u32,
    decode: fn(Value) -> Result<Command>,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler")
            .field("kind", &self.kind)
            .field("version", &self.version)
            .finish()
    }
}

impl Handler {
    /// A handler for a kind at this build's payload version.
    #[must_use]
    pub const fn new(kind: CommandKind, decode: fn(Value) -> Result<Command>) -> Self {
        Self {
            kind,
            version: PAYLOAD_SCHEMA_VERSION,
            decode,
        }
    }

    /// Decode a payload into the typed command, refusing a shape mismatch as a
    /// validation failure that names the kind.
    ///
    /// # Errors
    ///
    /// [`CommandError::Validation`] when the payload does not match this kind's schema.
    pub fn decode(&self, payload: Value) -> Result<Command> {
        (self.decode)(payload)
    }

    #[must_use]
    pub const fn kind(&self) -> CommandKind {
        self.kind
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// What this kind's payload is declared to hold, looked up in the one table.
    ///
    /// A lookup rather than a stored reference because `PAYLOAD_SCHEMAS` and `Command`
    /// are two declarations of the same set, and a handler that carried a pointer into the
    /// table would make an undeclared kind a construction failure at runtime. The
    /// `every_kind_has_exactly_one_declaration_at_the_current_version` test is what makes
    /// the answer never `None` in practice, and `field_names` returning an empty list is
    /// what keeps it honest if that ever stops being true.
    #[must_use]
    pub fn schema(&self) -> Option<&'static PayloadSchema> {
        PAYLOAD_SCHEMAS
            .iter()
            .find(|schema| schema.kind == self.kind && schema.version == self.version)
    }
}

/// A registry that does not exist: an out-of-range payload version reports the pair it
/// was keyed on, which is the difference between "no such command" and "no such version
/// of a command you do have".
fn unregistered(kind: &str, version: u32) -> CommandError {
    CommandError::UnknownCommand {
        kind: kind.to_string(),
        schema_version: version,
    }
}

fn decode_as<T>(kind: CommandKind, payload: Value) -> std::result::Result<T, CommandError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(payload).map_err(|error| CommandError::Validation {
        command: kind.as_str().to_string(),
        detail: format!("payload does not match version {PAYLOAD_SCHEMA_VERSION}: {error}"),
    })
}

/// The handlers and payload versions this build accepts.
///
/// `BTreeMap` keyed on `(kind, version)` rather than a match over kinds, because the
/// spec's later packages register commands into this table (WP12 through WP21 add the
/// rig, action and hand commands) and a registry that is a data structure can be extended
/// without editing a dispatch chain in three places.
pub struct Registry {
    handlers: BTreeMap<(CommandKind, u32), Handler>,
}

impl Registry {
    /// An empty registry, which is what a migration test or a restricted caller builds
    /// from.
    #[must_use]
    pub fn new() -> Self {
        Self {
            handlers: BTreeMap::new(),
        }
    }

    /// Every command this package implements, at its current payload version.
    #[must_use]
    pub fn with_default_handlers() -> Self {
        let mut registry = Self::new();
        for handler in [
            Handler::new(CommandKind::UpsertAsset, |payload| {
                decode_as(CommandKind::UpsertAsset, payload).map(Command::UpsertAsset)
            }),
            Handler::new(CommandKind::RemoveAsset, |payload| {
                decode_as(CommandKind::RemoveAsset, payload).map(Command::RemoveAsset)
            }),
            Handler::new(CommandKind::UpsertScene, |payload| {
                decode_as(CommandKind::UpsertScene, payload).map(Command::UpsertScene)
            }),
            Handler::new(CommandKind::RemoveScene, |payload| {
                decode_as(CommandKind::RemoveScene, payload).map(Command::RemoveScene)
            }),
            Handler::new(CommandKind::AddNode, |payload| {
                decode_as(CommandKind::AddNode, payload).map(Command::AddNode)
            }),
            Handler::new(CommandKind::RemoveNode, |payload| {
                decode_as(CommandKind::RemoveNode, payload).map(Command::RemoveNode)
            }),
            Handler::new(CommandKind::UpsertTrack, |payload| {
                decode_as(CommandKind::UpsertTrack, payload).map(Command::UpsertTrack)
            }),
            Handler::new(CommandKind::RemoveTrack, |payload| {
                decode_as(CommandKind::RemoveTrack, payload).map(Command::RemoveTrack)
            }),
            Handler::new(CommandKind::SetPropertyKey, |payload| {
                decode_as(CommandKind::SetPropertyKey, payload).map(Command::SetPropertyKey)
            }),
            Handler::new(CommandKind::RemovePropertyKey, |payload| {
                decode_as(CommandKind::RemovePropertyKey, payload).map(Command::RemovePropertyKey)
            }),
            Handler::new(CommandKind::PlaceMarker, |payload| {
                decode_as(CommandKind::PlaceMarker, payload).map(Command::PlaceMarker)
            }),
            Handler::new(CommandKind::RemoveMarker, |payload| {
                decode_as(CommandKind::RemoveMarker, payload).map(Command::RemoveMarker)
            }),
        ] {
            registry.register(handler);
        }
        registry
    }

    /// Add or replace one handler.
    pub fn register(&mut self, handler: Handler) -> Option<Handler> {
        self.handlers
            .insert((handler.kind(), handler.version()), handler)
    }

    /// Look a handler up by the pair an envelope declares.
    ///
    /// # Errors
    ///
    /// [`CommandError::UnknownCommand`] naming both halves of the miss.
    pub fn handler(&self, kind: CommandKind, version: u32) -> Result<&Handler> {
        self.handlers
            .get(&(kind, version))
            .ok_or_else(|| unregistered(kind.as_str(), version))
    }

    /// The kinds accepted at the current payload version, in registry order.
    #[must_use]
    pub fn accepted(&self) -> Vec<(CommandKind, u32)> {
        self.handlers.keys().copied().collect()
    }

    /// Decode a tagged command against this registry's handlers. This is the only path
    /// from JSON to a `Command`, so a caller holding an untrusted envelope and the
    /// processor share one implementation of the schema dispatch.
    ///
    /// # Errors
    ///
    /// [`CommandError::UnknownCommand`] for a kind or version this registry does not
    /// carry, and [`CommandError::Validation`] for a payload that does not match.
    pub fn decode(&self, wire: &WireCommand) -> Result<Command> {
        let kind: CommandKind = wire.kind.as_str().parse()?;
        self.handler(kind, wire.schema_version)?
            .decode(wire.payload.clone())
    }

    /// Encode a typed command back into its tagged form.
    ///
    /// # Errors
    ///
    /// A `serde_json` failure, reported as a validation error naming the kind.
    pub fn encode(&self, command: &Command) -> Result<WireCommand> {
        encode_command(command)
    }

    /// The payload schemas this registry can dispatch, for `schemas/commands/`.
    #[must_use]
    pub fn payload_schemas(&self) -> Vec<&'static PayloadSchema> {
        self.handlers.values().filter_map(Handler::schema).collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_default_handlers()
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("handlers", &self.handlers.len())
            .finish()
    }
}

/// Decode through the default registry.
///
/// Free function rather than a method because `CommandEnvelope`'s `Deserialize` needs it
/// and has no registry to be given: an envelope read from a file is read against what this
/// build implements, which is the same answer the processor would use.
///
/// # Errors
///
/// As [`Registry::decode`].
pub fn decode_wire_command(wire: &WireCommand) -> Result<Command> {
    let kind: CommandKind = wire.kind.as_str().parse()?;
    let registry = Registry::default();
    registry
        .handler(kind, wire.schema_version)?
        .decode(wire.payload.clone())
}

/// Encode through the default registry's table.
///
/// # Errors
///
/// [`CommandError::Validation`] if the payload cannot be written as JSON, or if the kind
/// has no declaration — which would mean `PAYLOAD_SCHEMAS` and `Command` drifted apart.
pub fn encode_command(command: &Command) -> Result<WireCommand> {
    let kind = command.kind();
    let payload = match command {
        Command::UpsertAsset(payload) => serde_json::to_value(payload),
        Command::RemoveAsset(payload) => serde_json::to_value(payload),
        Command::UpsertScene(payload) => serde_json::to_value(payload),
        Command::RemoveScene(payload) => serde_json::to_value(payload),
        Command::AddNode(payload) => serde_json::to_value(payload),
        Command::RemoveNode(payload) => serde_json::to_value(payload),
        Command::UpsertTrack(payload) => serde_json::to_value(payload),
        Command::RemoveTrack(payload) => serde_json::to_value(payload),
        Command::SetPropertyKey(payload) => serde_json::to_value(payload),
        Command::RemovePropertyKey(payload) => serde_json::to_value(payload),
        Command::PlaceMarker(payload) => serde_json::to_value(payload),
        Command::RemoveMarker(payload) => serde_json::to_value(payload),
    }
    .map_err(|error| CommandError::Validation {
        command: kind.as_str().to_string(),
        detail: format!("payload could not be written as JSON: {error}"),
    })?;
    Ok(WireCommand {
        kind: kind.as_str().to_string(),
        schema_version: command.payload_schema_version(),
        payload,
    })
}

/// The JSON Schema document for the command payloads, as `schemas/commands/v1.json`.
///
/// Generated from [`PAYLOAD_SCHEMAS`] so that the committed file and the dispatch table
/// have one source, and the test that compares them is comparing a file to a table rather
/// than a file to a hope. The file is nevertheless versioned in the repository, so it can
/// be read, diffed and cited without running anything.
#[must_use]
pub fn command_schema_document() -> Value {
    let mut schemas = Map::new();
    for schema in PAYLOAD_SCHEMAS {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for field in schema.fields {
            let mut entry = Map::new();
            entry.insert(
                "type".to_string(),
                Value::String(field.json_type.to_string()),
            );
            if field.json_type == "string" && field.name.ends_with("_id") {
                entry.insert("format".to_string(), Value::String("uuid".to_string()));
            }
            properties.insert(field.name.to_string(), Value::Object(entry));
            required.push(Value::String(field.name.to_string()));
        }
        let mut object = Map::new();
        object.insert("type".to_string(), Value::String("object".to_string()));
        object.insert("additionalProperties".to_string(), Value::Bool(false));
        object.insert("properties".to_string(), Value::Object(properties));
        object.insert("required".to_string(), Value::Array(required));
        schemas.insert(schema.kind.as_str().to_string(), Value::Object(object));
    }

    let mut envelope = Map::new();
    envelope.insert(
        "$schema".to_string(),
        Value::String(JSON_SCHEMA_DRAFT.into()),
    );
    envelope.insert(
        "title".to_string(),
        Value::String("anim_engine command payloads".into()),
    );
    envelope.insert(
        "payload_schema_version".to_string(),
        Value::from(PAYLOAD_SCHEMA_VERSION),
    );
    envelope.insert("commands".to_string(), Value::Object(schemas));
    Value::Object(envelope)
}

/// The dialect the two schema files declare. Draft 2020-12 is named because it is what a
/// reader would validate these against; no validator is a dependency (spec 5.1 names none),
/// and acceptance happens at the typed boundary, where a non-canonical rational or a
/// malformed uuid is refused by its own constructor rather than by a schema engine.
pub const JSON_SCHEMA_DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// The payload's canonical stream under its own domain, for a caller that hashes one
/// command outside an envelope.
///
/// # Errors
///
/// As [`Command::canonical_bytes`].
pub fn payload_digest(command: &Command) -> Result<anim_assets::ContentHash> {
    let mut cp =
        CanonicalBytes::with_domain(PAYLOAD_DOMAIN).map_err(|error| CommandError::Validation {
            command: command.kind().as_str().to_string(),
            detail: error.to_string(),
        })?;
    cp.bytes(&command.canonical_bytes()?);
    Ok(anim_assets::hash_bytes(cp.as_slice()))
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
        AddNodePayload, RemoveAssetPayload, RemoveNodePayload, RemoveScenePayload,
    };
    use anim_core::id::{AssetId, NodeId, SceneId};

    fn scene_id() -> SceneId {
        SceneId::parse("00000000-0000-5000-8000-000000000001").unwrap()
    }

    #[test]
    fn every_kind_has_exactly_one_declaration_at_the_current_version() {
        for kind in CommandKind::ALL {
            let declared: Vec<&PayloadSchema> = PAYLOAD_SCHEMAS
                .iter()
                .filter(|schema| schema.kind == kind)
                .collect();
            assert_eq!(declared.len(), 1, "{kind} declared {declared:?}");
            assert_eq!(declared[0].version, PAYLOAD_SCHEMA_VERSION);
            assert!(!declared[0].fields.is_empty(), "{kind} declares no fields");
        }
        assert_eq!(PAYLOAD_SCHEMAS.len(), CommandKind::ALL.len());
    }

    #[test]
    fn a_payload_round_trips_through_the_registry_that_declares_it() {
        let registry = Registry::with_default_handlers();
        let commands = [
            Command::RemoveAsset(RemoveAssetPayload {
                asset_id: AssetId::parse("00000000-0000-5000-8000-000000000002").unwrap(),
            }),
            Command::RemoveScene(RemoveScenePayload {
                scene_id: scene_id(),
            }),
            Command::RemoveNode(RemoveNodePayload {
                scene_id: scene_id(),
                node_id: NodeId::parse("00000000-0000-5000-8000-000000000003").unwrap(),
            }),
        ];
        for command in &commands {
            let wire = registry.encode(command).unwrap();
            assert_eq!(wire.kind, command.kind().as_str());
            assert_eq!(
                registry.decode(&wire).unwrap(),
                *command,
                "decode must return the command encode was given"
            );
            let text = serde_json::to_string(&wire).unwrap();
            assert!(
                text.contains(&format!("\"kind\":\"{}\"", command.kind().as_str())),
                "the tagged form carries its own kind: {text}"
            );
        }
    }

    #[test]
    fn a_payload_that_does_not_match_its_kind_is_refused_not_partially_read() {
        let registry = Registry::with_default_handlers();
        let wire = WireCommand {
            kind: "remove_scene".to_string(),
            schema_version: PAYLOAD_SCHEMA_VERSION,
            // `remove_scene` takes a scene id; this is an asset id under the wrong name.
            payload: serde_json::json!({ "node_id": "not-a-uuid" }),
        };
        let error = registry.decode(&wire).unwrap_err();
        assert!(
            matches!(error, CommandError::Validation { .. }),
            "a shape mismatch is a validation failure, got {error}"
        );
    }

    #[test]
    fn an_unregistered_version_names_the_pair_that_was_asked_for() {
        let registry = Registry::with_default_handlers();
        let error = registry
            .handler(CommandKind::AddNode, PAYLOAD_SCHEMA_VERSION + 1)
            .unwrap_err();
        assert_eq!(
            error,
            CommandError::UnknownCommand {
                kind: "add_node".to_string(),
                schema_version: 2
            }
        );
        // The same kind at the version that is registered resolves, which is what makes
        // the above a version refusal rather than a kind refusal.
        assert!(registry.handler(CommandKind::AddNode, 1).is_ok());
    }

    #[test]
    fn an_unknown_kind_text_is_refused_with_the_spelling_that_arrived() {
        let registry = Registry::with_default_handlers();
        let wire = WireCommand {
            kind: "set_camera_params".to_string(),
            schema_version: PAYLOAD_SCHEMA_VERSION,
            payload: serde_json::json!({}),
        };
        assert_eq!(
            registry.decode(&wire).unwrap_err(),
            CommandError::UnknownCommand {
                kind: "set_camera_params".to_string(),
                schema_version: PAYLOAD_SCHEMA_VERSION
            },
            "WP15's command must not look like a malformed WP03 payload"
        );
    }

    #[test]
    fn the_generated_schema_document_lists_every_field_of_every_kind() {
        let document = command_schema_document();
        let commands = document["commands"]
            .as_object()
            .expect("commands is an object");
        assert_eq!(commands.len(), CommandKind::ALL.len());
        for kind in CommandKind::ALL {
            let entry = &commands[kind.as_str()];
            let properties = entry["properties"].as_object().unwrap();
            let required = entry["required"].as_array().unwrap();
            // Compared as sets on purpose: this document is a `serde_json::Value`, whose
            // members come out alphabetical, and the canonical ordering rule belongs to the
            // engine's own serializers rather than to a schema artifact.
            let mut declared: Vec<&str> = properties.keys().map(String::as_str).collect();
            declared.sort_unstable();
            let mut wanted: Vec<&str> = Vec::new();
            for value in required {
                wanted.push(value.as_str().expect("required names are strings"));
            }
            wanted.sort_unstable();
            assert_eq!(
                declared, wanted,
                "{kind}: every declared field is required, and none is extra"
            );
            assert_eq!(properties.len(), field_names(kind).len());
        }
        assert_eq!(document["payload_schema_version"], 1);
    }

    #[test]
    fn a_payload_digest_is_domain_separated_from_the_same_bytes_under_another_tag() {
        let command = Command::RemoveScene(RemoveScenePayload {
            scene_id: scene_id(),
        });
        let digest = payload_digest(&command).unwrap();
        let mut bare = CanonicalBytes::new();
        bare.bytes(&command.canonical_bytes().unwrap());
        assert_ne!(
            digest,
            anim_assets::hash_bytes(bare.as_slice()),
            "the payload domain is what keeps a single-command hash from colliding with \
             the same bytes under another tag"
        );
    }

    #[test]
    fn a_registry_can_be_restricted_to_the_commands_a_caller_is_allowed() {
        // Spec 12.10: future automation "receives no privileged mutation route", which
        // only works if the registry is a data structure a caller can narrow rather than a
        // hardcoded match the caller must re-implement.
        let mut restricted = Registry::new();
        restricted.register(Handler::new(CommandKind::RemoveScene, |payload| {
            decode_as(CommandKind::RemoveScene, payload).map(Command::RemoveScene)
        }));
        assert_eq!(restricted.accepted().len(), 1);
        assert!(restricted
            .handler(CommandKind::AddNode, PAYLOAD_SCHEMA_VERSION)
            .is_err());
        assert!(restricted
            .handler(CommandKind::RemoveScene, PAYLOAD_SCHEMA_VERSION)
            .is_ok());
        // A restricted registry refuses even a well-formed command it does not carry,
        // which is the whole point of narrowing it.
        let node = anim_project::NodeData::new(
            anim_core::id::NodeId::parse("00000000-0000-5000-8000-0000000000c0").unwrap(),
            None,
            0,
            "cel",
        );
        let wire = WireCommand {
            kind: "add_node".to_string(),
            schema_version: PAYLOAD_SCHEMA_VERSION,
            payload: serde_json::json!({
                "scene_id": scene_id().to_canonical_string(),
                "node": serde_json::to_value(&node).unwrap(),
            }),
        };
        assert!(restricted.decode(&wire).is_err());
        assert_eq!(
            Registry::with_default_handlers().decode(&wire).unwrap(),
            Command::AddNode(AddNodePayload {
                scene_id: scene_id(),
                node,
            }),
            "the same bytes a narrowed registry refuses are legal to the full one"
        );
    }
}
