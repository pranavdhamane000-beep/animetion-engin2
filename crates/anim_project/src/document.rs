//! The authoritative document and its revision (spec 12.3).
//!
//! `ProjectDocument` is spec 12.8's struct with its six fields and nothing else. The
//! two things it deliberately does not hold are the ones spec 12.8 names as forbidden —
//! "no editor, renderer, device or undo-stack references" — and the third is a `dirty`
//! flag: a document that recorded whether it had been saved would need a second writer
//! to clear it, and spec 12.7's rule is that the flag clears *after* a successful
//! replacement, which only the session that owns the save can sequence.
//! [`crate::save`] therefore signals success by returning a report at all, and the
//! caller clears its own state when it receives one.

use std::collections::BTreeMap;

use anim_assets::AssetManifest;
use anim_core::error::CoreError;
use anim_core::id::{ProjectId, SceneId};
use anim_core::time::FrameRate;
use serde::{Deserialize, Serialize};

use crate::migrate;
use crate::scene::SceneData;
use crate::{ProjectError, Result, Revision};

/// The document schema version this build writes (spec 12.8's `schema_version`).
///
/// A bump here is a data-format event: it needs a step in [`crate::migrate::chain`]
/// before the constant changes. It is independent of
/// [`anim_assets::MANIFEST_SCHEMA_VERSION`], which describes the record collection
/// embedded in `assets`, and of the envelope version in `anim_commands`, which
/// describes a transaction. ADR-0005 Decision 4 keeps those two numbers separate for
/// the manifest, and closing its document half keeps the third separate as well: one
/// version per thing that changes, because folding them would make an asset-schema bump
/// look like a byte-layout change and vice versa.
pub const PROJECT_SCHEMA_VERSION: u32 = 1;

/// The authoritative state of one project at one revision.
///
/// Field order is spec 12.8's listing order, and that order is load-bearing rather
/// than cosmetic: the derived `Serialize` writes JSON object members in declaration
/// order, so the declaration *is* the persisted field ordering that ADR-0005's
/// Consequence 5 left this package to decide. Reordering these fields would rewrite
/// every saved file's bytes without changing a value, which is why
/// [`crate::schema`] proves that a loaded document reproduces the bytes it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectDocument {
    /// The document schema this value follows.
    pub schema_version: u32,
    /// Identity, generated once and copied through save, load, undo and evaluation.
    pub project_id: ProjectId,
    /// The revision every snapshot and every envelope's `expected_scene_revision` is
    /// checked against. Zero is valid for a new document (spec 10.8).
    pub revision: Revision,
    /// The timeline's rate, exact and rational (spec 10.6).
    pub frame_rate: FrameRate,
    /// The content manifest, embedded as its own `{schema_version, records}` object
    /// rather than flattened into the document. That is ADR-0005 Consequence 5's
    /// "embedded rather than re-encoded", closed here: the record collection's
    /// encoding — and therefore its digest — is unchanged by living inside a document.
    pub assets: AssetManifest,
    /// The scenes, keyed by id so that iteration order is id-byte order.
    pub scenes: BTreeMap<SceneId, SceneData>,
}

impl ProjectDocument {
    /// A new empty document at [`Revision::ZERO`].
    #[must_use]
    pub fn new(project_id: ProjectId, frame_rate: FrameRate) -> Self {
        Self::with_schema_version(PROJECT_SCHEMA_VERSION, project_id, frame_rate)
    }

    /// A document at an explicit schema version, which is what a migration builds: the
    /// same reasoning as `AssetManifest::with_schema_version` (ADR-0005 Consequence 4)
    /// — a step migrating *to* a version must be able to construct it without this
    /// crate refusing it first.
    #[must_use]
    pub fn with_schema_version(
        schema_version: u32,
        project_id: ProjectId,
        frame_rate: FrameRate,
    ) -> Self {
        Self {
            schema_version,
            project_id,
            revision: Revision::ZERO,
            frame_rate,
            assets: AssetManifest::new(),
            scenes: BTreeMap::new(),
        }
    }

    /// Every rule that makes this value a legal project rather than a parseable one.
    ///
    /// Called before any write (spec 12.7: "Validate and serialize before touching
    /// disk"), on every load, and by `anim_commands` after each command's mutation.
    ///
    /// # Errors
    ///
    /// [`ProjectError::UnsupportedSchema`] for a version this build cannot read,
    /// [`ProjectError::Asset`] for a manifest whose dependency graph is broken, and
    /// [`ProjectError::Validation`] for an invalid scene.
    pub fn validate(&self) -> Result<()> {
        migrate::check_document_schema(self.schema_version)?;
        migrate::check_asset_schema(self.assets.schema_version())?;
        anim_assets::validate_manifest(&self.assets)?;
        for (scene_id, scene) in &self.scenes {
            scene.validate().map_err(|error| match error {
                // The scene's own message names the node or track; which *scene* it
                // belongs to is document-level information, so it is added here rather
                // than pushed into every scene message.
                ProjectError::Validation { location, detail } => ProjectError::Validation {
                    location: format!("scene {scene_id} / {location}"),
                    detail,
                },
                other => other,
            })?;
        }
        Ok(())
    }

    /// Insert or replace one scene, returning what it displaced.
    ///
    /// The displaced scene comes back rather than being dropped for the same reason
    /// `AssetManifest::insert` returns its displaced record: an exact inverse has to
    /// carry the state that existed before it (spec 12.6).
    #[must_use = "the displaced scene is what a command's inverse carries"]
    pub fn insert_scene(&mut self, scene_id: SceneId, scene: SceneData) -> Option<SceneData> {
        self.scenes.insert(scene_id, scene)
    }

    /// Remove one scene, returning it.
    #[must_use = "the removed scene is what a command's inverse carries"]
    pub fn remove_scene(&mut self, scene_id: &SceneId) -> Option<SceneData> {
        self.scenes.remove(scene_id)
    }

    #[must_use]
    pub fn scene(&self, scene_id: &SceneId) -> Option<&SceneData> {
        self.scenes.get(scene_id)
    }

    /// The mutable scene, which only a command under the writer boundary should hold.
    /// Spec 12.6 makes the `CommandProcessor` the single writer; this accessor exists
    /// so a command can mutate one scene without replacing the whole document.
    #[must_use]
    pub fn scene_mut(&mut self, scene_id: &SceneId) -> Option<&mut SceneData> {
        self.scenes.get_mut(scene_id)
    }

    /// The scenes in `SceneId` byte order.
    pub fn scenes(&self) -> impl Iterator<Item = (&SceneId, &SceneData)> {
        self.scenes.iter()
    }

    /// The revision this document would hold after the next commit, checked.
    ///
    /// Spec 12.7 lists "revision capacity" among the checks that run *before*
    /// execution, so a batch that cannot be recorded fails without doing work. This is
    /// the one place that rule is expressed; `CommandProcessor::apply` calls it before
    /// cloning, and no command ever advances the revision itself.
    ///
    /// # Errors
    ///
    /// [`CoreError::ArithmeticOverflow`] at `u64::MAX`, which `anim_commands` reports
    /// as `RevisionOverflow`. Never wraps (spec 10.11).
    pub fn next_revision(&self) -> std::result::Result<Revision, CoreError> {
        self.revision.checked_next()
    }
}

impl<'de> Deserialize<'de> for ProjectDocument {
    /// Hand-written so that a saved file passes the same gate a command's result does.
    ///
    /// A derived impl would fill `schema_version` and `revision` directly, and a
    /// hand-edited `project.anim.json` could then carry a version this build cannot
    /// interpret or a scene graph with a dangling parent — the failure DECISIONS.md #22
    /// names. [`migrate::load_document`] is the supported entry point; this is the
    /// gate that makes `serde_json::from_slice` directly no easier to misuse.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawDocument {
            schema_version: u32,
            project_id: ProjectId,
            revision: Revision,
            frame_rate: FrameRate,
            assets: AssetManifest,
            scenes: BTreeMap<SceneId, SceneData>,
        }
        let raw = RawDocument::deserialize(deserializer)?;
        let document = Self {
            schema_version: raw.schema_version,
            project_id: raw.project_id,
            revision: raw.revision,
            frame_rate: raw.frame_rate,
            assets: raw.assets,
            scenes: raw.scenes,
        };
        document.validate().map_err(serde::de::Error::custom)?;
        Ok(document)
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
    use crate::scene::{NodeData, TrackData};
    use anim_assets::{AssetKind, AssetManifestEntry};
    use anim_core::id::{AssetId, NodeId};
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn rate() -> FrameRate {
        FrameRate::new(24000, 1001).unwrap()
    }

    fn document() -> ProjectDocument {
        ProjectDocument::new(ProjectId::derive(&namespace(), b"fixture-project"), rate())
    }

    #[test]
    fn a_new_document_is_at_revision_zero_and_the_current_schema() {
        let document = document();
        assert_eq!(document.revision, Revision::ZERO);
        assert_eq!(document.schema_version, PROJECT_SCHEMA_VERSION);
        assert!(document.scenes.is_empty());
        assert!(document.assets.is_empty());
        document.validate().expect("an empty document is valid");
    }

    #[test]
    fn the_json_field_order_is_the_declaration_order_of_spec_12_8() {
        // Asserted on the bytes the save path writes, not on a `serde_json::Value`.
        // That distinction is the finding, not a detail: `to_value` puts every object's
        // keys in alphabetical order because `Value`'s map is a `BTreeMap<String, _>`,
        // so a claim about field ordering proven through `Value` would be proven about
        // the wrong thing. The canonical path is `to_vec` on the struct, which writes
        // members in the order the `Serialize` impl emits them.
        let text = String::from_utf8(crate::schema::serialize_canonical(&document()).unwrap())
            .expect("canonical bytes are UTF-8");
        let names = [
            "schema_version",
            "project_id",
            "revision",
            "frame_rate",
            "assets",
            "scenes",
        ];
        let positions: Vec<usize> = names
            .iter()
            .map(|name| text.find(name).unwrap_or(usize::MAX))
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "12.8's listing order is the persisted order, got {text}"
        );
    }

    #[test]
    fn the_assets_field_keeps_its_own_schema_version_inside_it() {
        // ADR-0005 Consequence 5, closed: the record collection is embedded rather than
        // re-encoded, so its version travels with it and the document's version stays
        // about the document.
        let document = document();
        let json = serde_json::to_value(&document).unwrap();
        assert!(json["assets"]["schema_version"].is_number());
        assert!(json["assets"]["records"].is_object());
        assert_eq!(json["schema_version"].as_u64(), Some(1));
    }

    #[test]
    fn next_revision_checks_capacity_rather_than_wrapping() {
        let mut document = document();
        assert_eq!(document.next_revision().unwrap(), Revision(1));
        document.revision = Revision(u64::MAX);
        assert!(
            document.next_revision().is_err(),
            "a revision that cannot advance must be reported, not wrapped (spec 10.11)"
        );
    }

    #[test]
    fn an_inserted_scene_is_reachable_and_its_displacement_comes_back() {
        let mut document = document();
        let scene_id = SceneId::derive(&namespace(), b"shot01");
        let first = SceneData::new("shot01").unwrap();
        let second = SceneData::new("shot01-take-2").unwrap();
        assert!(document.insert_scene(scene_id, first).is_none());
        assert_eq!(
            document
                .insert_scene(scene_id, second)
                .expect("the displaced scene is returned")
                .name,
            "shot01"
        );
        assert_eq!(
            document
                .remove_scene(&scene_id)
                .expect("the removed scene is returned")
                .name,
            "shot01-take-2"
        );
        assert!(document.scene(&scene_id).is_none());
    }

    #[test]
    fn a_broken_asset_dependency_makes_the_whole_document_invalid() {
        let mut document = document();
        let missing = AssetId::derive(&namespace(), b"missing");
        let holder = AssetId::derive(&namespace(), b"holder");
        let record = AssetManifestEntry::new(
            holder,
            anim_assets::hash_bytes(b"payload"),
            7,
            AssetKind::Image,
            1,
            [missing],
            "holder",
        )
        .unwrap();
        document.assets.insert(record);
        assert!(matches!(
            document.validate(),
            Err(ProjectError::Asset(
                anim_assets::AssetError::MissingAssetId { .. }
            ))
        ));
    }

    #[test]
    fn a_document_at_another_version_is_refused_with_the_value_that_says_so() {
        let future = ProjectDocument::with_schema_version(
            PROJECT_SCHEMA_VERSION + 1,
            ProjectId::derive(&namespace(), b"p"),
            rate(),
        );
        let Err(ProjectError::UnsupportedSchema {
            found,
            which,
            supported,
        }) = future.validate()
        else {
            panic!("a document from a later build must not validate");
        };
        assert_eq!(found, 2);
        assert_eq!(which, "project document");
        assert_eq!(supported, PROJECT_SCHEMA_VERSION);
    }

    #[test]
    fn an_ntsc_rate_and_a_full_scene_graph_survive_the_documents_json_form() {
        let mut document = document();
        let scene_id = SceneId::derive(&namespace(), b"shot01");
        let mut scene = SceneData::new("shot01").unwrap();
        let root_id = NodeId::derive(&namespace(), b"root");
        scene
            .insert_node(NodeData::new(root_id, None, 0, "root"))
            .unwrap();
        let path = crate::property::PropertyPath::new("transform.x").unwrap();
        scene
            .insert_track(TrackData::new(
                anim_core::id::TrackId::derive(&namespace(), b"track-x"),
                root_id,
                path.clone(),
                crate::property::PropertyCurve::new(
                    [crate::property::PropertyKey::new(
                        anim_core::time::FramePos::normalized(
                            24,
                            anim_core::rational::Rational::new(1001, 2002).unwrap(),
                        )
                        .unwrap(),
                        crate::property::PropertyValue::Rational(
                            anim_core::rational::Rational::new(1001, 24).unwrap(),
                        ),
                        crate::property::Easing::Linear,
                    )],
                    None,
                )
                .unwrap(),
            ))
            .unwrap();
        assert!(document.insert_scene(scene_id, scene).is_none());
        assert!(document.scene(&scene_id).is_some());

        let bytes = serde_json::to_vec(&document).unwrap();
        let loaded: ProjectDocument = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded, document);
        assert_eq!(loaded.frame_rate, rate());
        assert!(loaded
            .scene(&scene_id)
            .unwrap()
            .track_for(&root_id, &path)
            .is_some());
    }
}
