//! Commands, validation and inverse generation (spec 12.3).
//!
//! Spec 12.9 gives one signature for the whole file:
//!
//! ```text
//! impl Command {
//!     pub fn apply(&self, txn: &mut ProjectDocument) -> Result<Command, CommandError>;
//!     }
//! ```
//!
//! The return value is the inverse, and that one choice decides the shape of the set.
//! Spec 12.6 says a command "validates current transactional state and returns an
//! inverse", and an inverse has to be expressible as another command in this same set —
//! which is why the twelve below are *closed under inversion* rather than a list of
//! convenient edits. `UpsertAsset` and `RemoveAsset` are one pair, `AddNode` and
//! `RemoveNode` another, and every `Upsert`/`Set` returns either the displaced value
//! wrapped in its own kind or the matching `Remove` when there was nothing to displace.
//! A set that closes is what makes spec 12.12's "undo/redo equality" and "rollback from
//! every command index" provable; a set where an inverse needs a third mechanism is not.
//!
//! The command names a later package will test are absent on purpose. `AddKeyframe`,
//! `AddMotionLayer` and `SetCameraParams` are WP15's (spec 15's own list), the rig
//! commands are WP12's, the hand and face commands are WP18's and WP19's. This package
//! owns the boundary those commands will be added *to*, and forward-loading their
//! semantics would author camera and motion models that spec 15 and 21 assign elsewhere.

use anim_core::hash::CanonicalBytes;
use anim_core::id::NodeId;
use anim_project::{
    AssetId, AssetManifestEntry, FramePos, Marker, NodeData, ProjectDocument, PropertyKey,
    SceneData, SceneId, TrackData, TrackId,
};
use serde::{Deserialize, Serialize};

use crate::processor::commit_if_valid;
use crate::registry::PAYLOAD_SCHEMA_VERSION;
use crate::{CommandError, Result};

/// A content-manifest record. Spec 11.5 step 8's "WP03 command" is this one:
/// `anim_assets::import` returns a record and touches no document, so a record becomes
/// project state only through an envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsertAssetPayload {
    pub record: AssetManifestEntry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveAssetPayload {
    pub asset_id: AssetId,
}

/// Create or replace a whole scene.
///
/// The payload carries the scene rather than a name, because the inverse of replacing a
/// scene has to carry what was there (spec 12.6) and a name cannot rebuild an authored
/// graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsertScenePayload {
    pub scene_id: SceneId,
    pub scene: SceneData,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveScenePayload {
    pub scene_id: SceneId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddNodePayload {
    pub scene_id: SceneId,
    pub node: NodeData,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveNodePayload {
    pub scene_id: SceneId,
    pub node_id: NodeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsertTrackPayload {
    pub scene_id: SceneId,
    pub track: TrackData,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveTrackPayload {
    pub scene_id: SceneId,
    pub track_id: TrackId,
}

/// Insert or replace one key on an existing track.
///
/// A track is never created as a side effect of setting a key. If it were, the inverse
/// of the first key on a new property would have to delete the track as well as the key,
/// and one command would need two different inverses depending on state that the caller
/// did not choose — so track creation is [`CommandKind::UpsertTrack`], and this command
/// refuses a track id that does not exist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetPropertyKeyPayload {
    pub scene_id: SceneId,
    pub track_id: TrackId,
    pub key: PropertyKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovePropertyKeyPayload {
    pub scene_id: SceneId,
    pub track_id: TrackId,
    pub position: FramePos,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceMarkerPayload {
    pub scene_id: SceneId,
    pub marker: Marker,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveMarkerPayload {
    pub scene_id: SceneId,
    pub marker: Marker,
}

/// The command's kind, as the stable lowercase text used in JSON, in a canonical stream
/// and in the registry's dispatch table.
///
/// Text rather than an array position, for ADR-0004's reason: reordering these variants
/// would silently redefine every envelope ever recorded if a discriminant were the
/// spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    UpsertAsset,
    RemoveAsset,
    UpsertScene,
    RemoveScene,
    AddNode,
    RemoveNode,
    UpsertTrack,
    RemoveTrack,
    SetPropertyKey,
    RemovePropertyKey,
    PlaceMarker,
    RemoveMarker,
}

impl CommandKind {
    /// Every kind, in the registry's and the schema's declared order.
    pub const ALL: [Self; 12] = [
        Self::UpsertAsset,
        Self::RemoveAsset,
        Self::UpsertScene,
        Self::RemoveScene,
        Self::AddNode,
        Self::RemoveNode,
        Self::UpsertTrack,
        Self::RemoveTrack,
        Self::SetPropertyKey,
        Self::RemovePropertyKey,
        Self::PlaceMarker,
        Self::RemoveMarker,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpsertAsset => "upsert_asset",
            Self::RemoveAsset => "remove_asset",
            Self::UpsertScene => "upsert_scene",
            Self::RemoveScene => "remove_scene",
            Self::AddNode => "add_node",
            Self::RemoveNode => "remove_node",
            Self::UpsertTrack => "upsert_track",
            Self::RemoveTrack => "remove_track",
            Self::SetPropertyKey => "set_property_key",
            Self::RemovePropertyKey => "remove_property_key",
            Self::PlaceMarker => "place_marker",
            Self::RemoveMarker => "remove_marker",
        }
    }

    /// The kind this one's inverse is drawn from.
    ///
    /// A pair rather than a table because it is the property the set is built to hold:
    /// `inverse()` of `inverse()` is not required to return to the start — an
    /// `UpsertAsset` that displaced nothing reverses to `RemoveAsset`, and removing an
    /// absent record is refused rather than being a no-op — but every command's inverse
    /// must land inside this set, which `closed_set_is_the_inverse_of_itself` asserts.
    #[must_use]
    pub const fn inverse(self) -> Self {
        match self {
            Self::UpsertAsset | Self::RemoveAsset => Self::UpsertAsset,
            Self::UpsertScene | Self::RemoveScene => Self::UpsertScene,
            Self::AddNode | Self::RemoveNode => Self::AddNode,
            Self::UpsertTrack | Self::RemoveTrack => Self::UpsertTrack,
            Self::SetPropertyKey | Self::RemovePropertyKey => Self::SetPropertyKey,
            Self::PlaceMarker | Self::RemoveMarker => Self::PlaceMarker,
        }
    }
}

impl std::fmt::Display for CommandKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for CommandKind {
    type Err = CommandError;

    /// Parse the canonical spelling, and only it.
    ///
    /// `FromStr` over the `as_str` table rather than `serde`'s name mapping, so the
    /// spelling the registry dispatches on and the spelling the serializer writes are
    /// one string with no second list to keep in step.
    fn from_str(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| CommandError::UnknownCommand {
                kind: s.to_string(),
                schema_version: PAYLOAD_SCHEMA_VERSION,
            })
    }
}

/// One mutation, with its payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    UpsertAsset(UpsertAssetPayload),
    RemoveAsset(RemoveAssetPayload),
    UpsertScene(UpsertScenePayload),
    RemoveScene(RemoveScenePayload),
    AddNode(AddNodePayload),
    RemoveNode(RemoveNodePayload),
    UpsertTrack(UpsertTrackPayload),
    RemoveTrack(RemoveTrackPayload),
    SetPropertyKey(SetPropertyKeyPayload),
    RemovePropertyKey(RemovePropertyKeyPayload),
    PlaceMarker(PlaceMarkerPayload),
    RemoveMarker(RemoveMarkerPayload),
}

impl Command {
    #[must_use]
    pub fn kind(&self) -> CommandKind {
        match self {
            Self::UpsertAsset(_) => CommandKind::UpsertAsset,
            Self::RemoveAsset(_) => CommandKind::RemoveAsset,
            Self::UpsertScene(_) => CommandKind::UpsertScene,
            Self::RemoveScene(_) => CommandKind::RemoveScene,
            Self::AddNode(_) => CommandKind::AddNode,
            Self::RemoveNode(_) => CommandKind::RemoveNode,
            Self::UpsertTrack(_) => CommandKind::UpsertTrack,
            Self::RemoveTrack(_) => CommandKind::RemoveTrack,
            Self::SetPropertyKey(_) => CommandKind::SetPropertyKey,
            Self::RemovePropertyKey(_) => CommandKind::RemovePropertyKey,
            Self::PlaceMarker(_) => CommandKind::PlaceMarker,
            Self::RemoveMarker(_) => CommandKind::RemoveMarker,
        }
    }

    /// The version of *this payload's* JSON schema (spec 12.2: "All command payload
    /// types have versioned JSON schemas").
    ///
    /// Per kind rather than global, so that one payload changing shape does not bump the
    /// version of eleven commands that did not change.
    #[must_use]
    pub const fn payload_schema_version(&self) -> u32 {
        PAYLOAD_SCHEMA_VERSION
    }

    /// Check this command against the state it would be applied to, changing nothing.
    ///
    /// This is the same code [`Command::apply`] runs rather than a second reading of the
    /// rules: spec 12.6 says dry-run "checks schema, references and invariants, but apply
    /// repeats all checks under the writer boundary", and the way to honour a sentence
    /// like that is one implementation with two entry points, not two implementations
    /// kept equivalent by discipline.
    ///
    /// # Errors
    ///
    /// [`CommandError::Validation`] for a precondition this command cannot meet.
    pub fn validate(&self, document: &ProjectDocument) -> Result<()> {
        let mut candidate = document.clone();
        self.mutate(&mut candidate)?;
        Ok(())
    }

    /// Apply the mutation and return the command that exactly reverses it (spec 12.9).
    ///
    /// # Errors
    ///
    /// Whatever [`Command::validate`] refuses. A failure leaves `txn` byte-for-byte
    /// unchanged, because the mutation lands only after the document re-validates.
    pub fn apply(&self, txn: &mut ProjectDocument) -> Result<Command> {
        self.mutate(txn)
    }

    /// The single implementation behind both entry points.
    fn mutate(&self, txn: &mut ProjectDocument) -> Result<Command> {
        match self {
            Self::UpsertAsset(UpsertAssetPayload { record }) => {
                let asset_id = record.asset_id;
                let inserted = record.clone();
                let displaced =
                    commit_if_valid(txn, move |document| Ok(document.assets.insert(inserted)))?;
                Ok(match displaced {
                    Some(previous) => Self::UpsertAsset(UpsertAssetPayload { record: previous }),
                    None => Self::RemoveAsset(RemoveAssetPayload { asset_id }),
                })
            }
            Self::RemoveAsset(RemoveAssetPayload { asset_id }) => {
                if !txn.assets.contains(asset_id) {
                    return Err(missing(
                        "remove_asset",
                        &asset_id.to_string(),
                        "no record in the manifest",
                    ));
                }
                let removed =
                    commit_if_valid(txn, |document| Ok(document.assets.remove(asset_id)))?
                        .ok_or_else(|| {
                            missing("remove_asset", &asset_id.to_string(), "no record")
                        })?;
                Ok(Self::UpsertAsset(UpsertAssetPayload { record: removed }))
            }
            Self::UpsertScene(UpsertScenePayload { scene_id, scene }) => {
                let inserted = scene.clone();
                let displaced = commit_if_valid(txn, move |document| {
                    Ok(document.insert_scene(*scene_id, inserted))
                })?;
                Ok(match displaced {
                    Some(previous) => Self::UpsertScene(UpsertScenePayload {
                        scene_id: *scene_id,
                        scene: previous,
                    }),
                    None => Self::RemoveScene(RemoveScenePayload {
                        scene_id: *scene_id,
                    }),
                })
            }
            Self::RemoveScene(RemoveScenePayload { scene_id }) => {
                if txn.scene(scene_id).is_none() {
                    return Err(missing_scene("remove_scene", scene_id));
                }
                let removed = commit_if_valid(txn, |document| Ok(document.remove_scene(scene_id)))?
                    .ok_or_else(|| missing("remove_scene", &scene_id.to_string(), "no scene"))?;
                Ok(Self::UpsertScene(UpsertScenePayload {
                    scene_id: *scene_id,
                    scene: removed,
                }))
            }
            Self::AddNode(AddNodePayload { scene_id, node }) => {
                let node_id = node.node_id;
                // Refused rather than replaced: spec 10.8 says an id is generated once at
                // creation, so a second node claiming one is a defect, not an edit.
                if txn
                    .scene(scene_id)
                    .ok_or_else(|| missing_scene("add_node", scene_id))?
                    .contains_node(&node_id)
                {
                    return Err(missing(
                        "add_node",
                        &node_id.to_string(),
                        "a node with this id is already in the scene",
                    ));
                }
                let added = node.clone();
                commit_if_valid(txn, move |document| {
                    project(scene_mut(document, "add_node", scene_id)?.insert_node(added))
                })?;
                Ok(Self::RemoveNode(RemoveNodePayload {
                    scene_id: *scene_id,
                    node_id,
                }))
            }
            Self::RemoveNode(RemoveNodePayload { scene_id, node_id }) => {
                if !txn
                    .scene(scene_id)
                    .ok_or_else(|| missing_scene("remove_node", scene_id))?
                    .contains_node(node_id)
                {
                    return Err(missing("remove_node", &node_id.to_string(), "no node"));
                }
                let removed = commit_if_valid(txn, |document| {
                    project(scene_mut(document, "remove_node", scene_id)?.remove_node(node_id))
                })?
                .ok_or_else(|| missing("remove_node", &node_id.to_string(), "no node"))?;
                Ok(Self::AddNode(AddNodePayload {
                    scene_id: *scene_id,
                    node: removed,
                }))
            }
            Self::UpsertTrack(UpsertTrackPayload { scene_id, track }) => {
                let inserted = track.clone();
                let displaced = commit_if_valid(txn, move |document| {
                    project(scene_mut(document, "upsert_track", scene_id)?.insert_track(inserted))
                })?;
                Ok(match displaced {
                    Some(previous) => Self::UpsertTrack(UpsertTrackPayload {
                        scene_id: *scene_id,
                        track: previous,
                    }),
                    None => Self::RemoveTrack(RemoveTrackPayload {
                        scene_id: *scene_id,
                        track_id: track.track_id,
                    }),
                })
            }
            Self::RemoveTrack(RemoveTrackPayload { scene_id, track_id }) => {
                let removed = commit_if_valid(txn, |document| {
                    project(scene_mut(document, "remove_track", scene_id)?.remove_track(track_id))
                })?
                .ok_or_else(|| {
                    missing(
                        "remove_track",
                        &track_id.to_string(),
                        "no track in this scene",
                    )
                })?;
                Ok(Self::UpsertTrack(UpsertTrackPayload {
                    scene_id: *scene_id,
                    track: removed,
                }))
            }
            Self::SetPropertyKey(SetPropertyKeyPayload {
                scene_id,
                track_id,
                key,
            }) => {
                let position = key.position;
                let inserted = key.clone();
                let displaced = commit_if_valid(txn, move |document| {
                    project(
                        scene_mut(document, "set_property_key", scene_id)?
                            .set_track_key(track_id, inserted),
                    )
                })?;
                Ok(match displaced {
                    Some(previous) => Self::SetPropertyKey(SetPropertyKeyPayload {
                        scene_id: *scene_id,
                        track_id: *track_id,
                        key: previous,
                    }),
                    None => Self::RemovePropertyKey(RemovePropertyKeyPayload {
                        scene_id: *scene_id,
                        track_id: *track_id,
                        position,
                    }),
                })
            }
            Self::RemovePropertyKey(RemovePropertyKeyPayload {
                scene_id,
                track_id,
                position,
            }) => {
                let removed = commit_if_valid(txn, |document| {
                    project(
                        scene_mut(document, "remove_property_key", scene_id)?
                            .remove_track_key(track_id, *position),
                    )
                })?
                .ok_or_else(|| {
                    missing(
                        "remove_property_key",
                        &format!("track {track_id} at frame {}", position.frame),
                        "no key",
                    )
                })?;
                Ok(Self::SetPropertyKey(SetPropertyKeyPayload {
                    scene_id: *scene_id,
                    track_id: *track_id,
                    key: removed,
                }))
            }
            Self::PlaceMarker(PlaceMarkerPayload { scene_id, marker }) => {
                let placed = marker.clone();
                let added = commit_if_valid(txn, move |document| {
                    project(scene_mut(document, "place_marker", scene_id)?.insert_marker(placed))
                })?;
                if !added {
                    return Err(missing(
                        "place_marker",
                        &format!("{:?}", marker.label),
                        &format!("is already at frame {}", marker.position.frame),
                    ));
                }
                Ok(Self::RemoveMarker(RemoveMarkerPayload {
                    scene_id: *scene_id,
                    marker: marker.clone(),
                }))
            }
            Self::RemoveMarker(RemoveMarkerPayload { scene_id, marker }) => {
                let target = marker.clone();
                let removed = commit_if_valid(txn, move |document| {
                    project(scene_mut(document, "remove_marker", scene_id)?.remove_marker(&target))
                })?;
                if !removed {
                    return Err(missing(
                        "remove_marker",
                        &format!("{:?}", marker.label),
                        &format!("is not at frame {}", marker.position.frame),
                    ));
                }
                Ok(Self::PlaceMarker(PlaceMarkerPayload {
                    scene_id: *scene_id,
                    marker: marker.clone(),
                }))
            }
        }
    }

    /// The command's canonical bytes: kind text, payload schema version, then the
    /// payload's fields in declaration order.
    ///
    /// ADR-0005 Decision 8 exists for the asset case — a payload that references a
    /// record hashes `AssetManifestEntry::canonical_bytes()` rather than inventing a
    /// second rendering — and the same rule extends to the rest: fixed widths raw,
    /// variable-length items `u64`-prefixed, including a nested object's own canonical
    /// stream. Nothing here uses JSON, because a JSON byte stream inside a hash pre-image
    /// would make an envelope digest depend on a serializer's key order rather than on the
    /// command.
    ///
    /// # Errors
    ///
    /// [`CommandError::Document`] when a nested record's canonical encoding fails, which
    /// is a manifest that disagrees with itself rather than a bad command.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut cp = CanonicalBytes::new();
        cp.text(self.kind().as_str());
        cp.u32(self.payload_schema_version());
        match self {
            Self::UpsertAsset(payload) => {
                cp.bytes(&payload.record.canonical_bytes().map_err(asset)?)
            }
            Self::RemoveAsset(payload) => cp.fixed_bytes(payload.asset_id.as_bytes()),
            Self::UpsertScene(payload) => {
                cp.fixed_bytes(payload.scene_id.as_bytes());
                cp.bytes(&payload.scene.canonical_bytes());
            }
            Self::RemoveScene(payload) => cp.fixed_bytes(payload.scene_id.as_bytes()),
            Self::AddNode(payload) => {
                cp.fixed_bytes(payload.scene_id.as_bytes());
                cp.bytes(&payload.node.canonical_bytes());
            }
            Self::RemoveNode(payload) => {
                cp.fixed_bytes(payload.scene_id.as_bytes());
                cp.fixed_bytes(payload.node_id.as_bytes());
            }
            Self::UpsertTrack(payload) => {
                cp.fixed_bytes(payload.scene_id.as_bytes());
                cp.bytes(&payload.track.canonical_bytes());
            }
            Self::RemoveTrack(payload) => {
                cp.fixed_bytes(payload.scene_id.as_bytes());
                cp.fixed_bytes(payload.track_id.as_bytes());
            }
            Self::SetPropertyKey(payload) => {
                cp.fixed_bytes(payload.scene_id.as_bytes());
                cp.fixed_bytes(payload.track_id.as_bytes());
                cp.bytes(&payload.key.canonical_bytes());
            }
            Self::RemovePropertyKey(payload) => {
                cp.fixed_bytes(payload.scene_id.as_bytes());
                cp.fixed_bytes(payload.track_id.as_bytes());
                cp.i64(payload.position.frame);
                cp.rational(&payload.position.subframe());
            }
            Self::PlaceMarker(payload) => {
                cp.fixed_bytes(payload.scene_id.as_bytes());
                cp.bytes(&payload.marker.canonical_bytes());
            }
            Self::RemoveMarker(payload) => {
                cp.fixed_bytes(payload.scene_id.as_bytes());
                cp.bytes(&payload.marker.canonical_bytes());
            }
        }
        Ok(cp.into_vec())
    }
}

/// Map a `ProjectError` into this crate's taxonomy.
///
/// `anim_project`'s own `Result` alias appears in every scene and manifest method, and
/// `?` cannot join the two types inside a closure whose return type is still being
/// inferred. Naming the conversion once here is what keeps the commands below from each
/// spelling `map_err` a little differently.
fn project<T>(result: std::result::Result<T, anim_project::ProjectError>) -> Result<T> {
    result.map_err(CommandError::Document)
}

/// The same for a record's canonical encoding, which fails as an `AssetError`.
fn asset(error: anim_assets::AssetError) -> CommandError {
    CommandError::Document(anim_project::ProjectError::Asset(error))
}

/// The scene a command mutates, or the refusal that names which id did not resolve.
///
/// `scene_mut` returns an `Option`, and every caller here needs the same error with a
/// different command name — which is why the name is a parameter rather than why each
/// call site repeats the message.
fn scene_mut<'a>(
    document: &'a mut ProjectDocument,
    command: &str,
    scene_id: &SceneId,
) -> Result<&'a mut SceneData> {
    document
        .scene_mut(scene_id)
        .ok_or_else(|| CommandError::Validation {
            command: command.to_string(),
            detail: format!("scene {scene_id} is not in the document"),
        })
}

fn missing_scene(command: &str, scene_id: &SceneId) -> CommandError {
    CommandError::Validation {
        command: command.to_string(),
        detail: format!("scene {scene_id} is not in the document"),
    }
}

fn missing(command: &str, what: &str, why: &str) -> CommandError {
    CommandError::Validation {
        command: command.to_string(),
        detail: format!("{what} {why}"),
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
    use anim_core::id::ProjectId;
    use anim_core::time::FrameRate;
    use anim_project::{PropertyPath, Revision};
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn scene_id() -> SceneId {
        SceneId::derive(&namespace(), b"shot01")
    }

    fn document_with_scene() -> ProjectDocument {
        let mut document = ProjectDocument::new(
            ProjectId::derive(&namespace(), b"commands-fixture"),
            FrameRate::new(24, 1).unwrap(),
        );
        assert!(
            document
                .insert_scene(scene_id(), SceneData::new("shot01").unwrap())
                .is_none(),
            "the fixture builds a new document, so nothing is displaced"
        );
        document
    }

    fn node(label: &str, parent: Option<&str>) -> NodeData {
        NodeData::new(
            NodeId::derive(&namespace(), label.as_bytes()),
            parent.map(|p| NodeId::derive(&namespace(), p.as_bytes())),
            0,
            label.to_string(),
        )
    }

    #[test]
    fn kind_spellings_are_unique_and_survive_parsing() {
        let mut seen = Vec::new();
        for kind in CommandKind::ALL {
            assert!(
                !seen.contains(&kind.as_str()),
                "{} spelled twice",
                kind.as_str()
            );
            seen.push(kind.as_str());
            assert_eq!(kind.to_string(), kind.as_str());
            assert_eq!(kind.as_str().parse::<CommandKind>().unwrap(), kind);
        }
        assert_eq!(
            "set_keyframe".parse::<CommandKind>().unwrap_err(),
            CommandError::UnknownCommand {
                kind: "set_keyframe".to_string(),
                schema_version: 1
            }
        );
    }

    #[test]
    fn every_command_inverses_into_a_command_of_the_same_set() {
        // The structural property the whole file is organized around: an inverse is
        // another `Command`, never a side effect or a second mechanism.
        for kind in CommandKind::ALL {
            let inverse = kind.inverse();
            assert!(
                CommandKind::ALL.contains(&inverse),
                "{kind} would inverse outside the set"
            );
        }
    }

    #[test]
    fn an_added_node_reverses_to_removing_that_node() {
        let mut document = document_with_scene();
        let before = serde_json::to_string(&document).unwrap();
        let added = node("root", None);
        let inverse = Command::AddNode(AddNodePayload {
            scene_id: scene_id(),
            node: added.clone(),
        })
        .apply(&mut document)
        .unwrap();
        assert_ne!(serde_json::to_string(&document).unwrap(), before);
        assert_eq!(
            inverse,
            Command::RemoveNode(RemoveNodePayload {
                scene_id: scene_id(),
                node_id: added.node_id,
            })
        );
        inverse.apply(&mut document).unwrap();
        assert_eq!(
            serde_json::to_string(&document).unwrap(),
            before,
            "apply then its inverse is the identity on the document"
        );
    }

    #[test]
    fn a_replaced_key_reverses_to_the_key_it_displaced_not_to_a_removal() {
        let mut document = document_with_scene();
        let root = node("root", None);
        let root_id = root.node_id;
        Command::AddNode(AddNodePayload {
            scene_id: scene_id(),
            node: root.clone(),
        })
        .apply(&mut document)
        .unwrap();
        let track_id = TrackId::derive(&namespace(), b"opacity");
        let track = TrackData::new(
            track_id,
            root_id,
            PropertyPath::new("opacity").unwrap(),
            anim_project::PropertyCurve::with_default(None),
        );
        Command::UpsertTrack(UpsertTrackPayload {
            scene_id: scene_id(),
            track,
        })
        .apply(&mut document)
        .unwrap();

        let key_at_4 = anim_project::PropertyKey::new(
            anim_core::time::FramePos::at_frame(4),
            anim_project::PropertyValue::Bool(false),
            anim_project::Easing::Linear,
        );
        let first = Command::SetPropertyKey(SetPropertyKeyPayload {
            scene_id: scene_id(),
            track_id,
            key: key_at_4.clone(),
        });
        // A first key has no displaced value, so it reverses to a removal.
        assert_eq!(
            first.apply(&mut document).unwrap(),
            Command::RemovePropertyKey(RemovePropertyKeyPayload {
                scene_id: scene_id(),
                track_id,
                position: key_at_4.position,
            })
        );

        let replacement = anim_project::PropertyKey::new(
            key_at_4.position,
            anim_project::PropertyValue::Bool(true),
            anim_project::Easing::Step,
        );
        let second = Command::SetPropertyKey(SetPropertyKeyPayload {
            scene_id: scene_id(),
            track_id,
            key: replacement.clone(),
        });
        // The same command on an occupied position reverses to restoring the old key.
        assert_eq!(
            second.apply(&mut document).unwrap(),
            Command::SetPropertyKey(SetPropertyKeyPayload {
                scene_id: scene_id(),
                track_id,
                key: key_at_4,
            })
        );
    }

    #[test]
    fn a_rejected_command_leaves_the_document_byte_identical() {
        let mut document = document_with_scene();
        let before = serde_json::to_vec(&document).unwrap();
        // A node whose parent is not in the scene: the scene refuses, and the document
        // must be exactly what it was rather than half-changed.
        let orphan = node("orphan", Some("absent"));
        let error = Command::AddNode(AddNodePayload {
            scene_id: scene_id(),
            node: orphan,
        })
        .apply(&mut document)
        .unwrap_err();
        assert!(matches!(error, CommandError::Document(_)), "{error}");
        assert_eq!(serde_json::to_vec(&document).unwrap(), before);
        assert_eq!(document.revision, Revision::ZERO);
    }

    #[test]
    fn canonical_bytes_separate_two_commands_that_differ_only_in_kind() {
        let removal = Command::RemoveScene(RemoveScenePayload {
            scene_id: scene_id(),
        });
        let addition = Command::AddNode(AddNodePayload {
            scene_id: scene_id(),
            node: node("x", None),
        });
        let removal_bytes = removal.canonical_bytes().unwrap();
        assert_ne!(removal_bytes, addition.canonical_bytes().unwrap());
        assert!(
            String::from_utf8_lossy(&removal_bytes).contains("remove_scene"),
            "the kind text is in the pre-image, not a discriminant"
        );
    }

    #[test]
    fn a_marker_and_its_removal_carry_the_same_payload_and_inverse_to_each_other() {
        let mut document = document_with_scene();
        let marker = Marker::new(anim_core::time::FramePos::at_frame(12), "contact");
        let placed = Command::PlaceMarker(PlaceMarkerPayload {
            scene_id: scene_id(),
            marker: marker.clone(),
        });
        let inverse = placed.apply(&mut document).unwrap();
        assert_eq!(
            inverse,
            Command::RemoveMarker(RemoveMarkerPayload {
                scene_id: scene_id(),
                marker: marker.clone(),
            })
        );
        // Placing the same marker twice is a defect, not a no-op.
        assert!(placed.apply(&mut document).is_err());
    }
}
