//! Sequential schema migrations (spec 12.3).
//!
//! Spec 11.11's manifest and spec 12.8's document each carry their own version
//! integer, and ADR-0005 Consequence 4 assigns the range question to this file:
//! `anim_assets` accepts any `u32` in `AssetManifest::with_schema_version` precisely
//! so that a migration can build a manifest at the version it is migrating *to*, which
//! means refusing a version has to happen here, where the supported set is known.
//!
//! A migration is a step on raw `serde_json::Value`, not on `ProjectDocument`. That is
//! the whole design: by the time a step runs, the target type may not exist in this
//! build's shape yet, and a step that had to deserialize into the current struct first
//! could only migrate files it could already parse — which is every file that needed no
//! migration.

use serde_json::Value;

use crate::document::{ProjectDocument, PROJECT_SCHEMA_VERSION};
use crate::{ProjectError, Result};

/// The asset manifest version this build reads.
///
/// Equal to [`anim_assets::MANIFEST_SCHEMA_VERSION`] today, and stated separately
/// because they can disagree legitimately: `anim_assets` writes version 2 records
/// before this crate knows how to migrate them *from* 1, and the direction that must
/// never happen silently is reading a file whose manifest version is outside this list.
pub const SUPPORTED_MANIFEST_SCHEMA_VERSIONS: &[u32] = &[anim_assets::MANIFEST_SCHEMA_VERSION];

// The document version this build reads is `PROJECT_SCHEMA_VERSION` exactly, and there
// is no list for it: a supported range would be a promise to keep a migration path for
// versions that do not exist yet, which is the opposite of spec 12.5's step 9 — the
// chain grows one step at a time, per version, with a fixture per version.

/// One step of the chain: it advances exactly one version, and it is total — every
/// field it does not rewrite must survive byte-identically.
pub struct MigrationStep {
    /// The version this step reads.
    pub from: u32,
    /// The version this step produces.
    pub to: u32,
    /// What changes, for the report and for a reader deciding whether to trust it.
    pub description: &'static str,
    pub apply: fn(&mut Value) -> Result<()>,
}

/// The migration chain from any supported version to
/// [`PROJECT_SCHEMA_VERSION`].
///
/// One step. Version 1 is WP03's shape and is on disk in every project saved before
/// WP04, which added the event section spec 13.5 step 1 validates and 13.7 orders. Spec
/// 12.5's step 9 asks for a step per version pair and a fixture per version, and both now
/// exist: the step is `V1_TO_V2` and the fixture is
/// `tests/fixtures/project_v1.anim.json`. What was a test-only mechanism is a real one.
pub const fn chain() -> &'static [MigrationStep] {
    CHAIN
}

const CHAIN: &[MigrationStep] = &[V1_TO_V2];

/// The version 1 to version 2 step.
///
/// The two ends are literals and not `PROJECT_SCHEMA_VERSION - 1` on purpose. A step is
/// frozen history: when a version 3 arrives, this one must still read exactly the version
/// 1 shape and must still produce exactly the version 2 shape, and a step written against
/// the current number would silently change what it claims to read the moment the constant
/// moves.
const V1_TO_V2: MigrationStep = MigrationStep {
    from: 1,
    to: 2,
    description: "v1 to v2: give every scene the `events` section version 1 did not have",
    apply: add_events_section,
};

/// Give every scene in a version 1 document the empty event section version 2 requires.
///
/// Total in the sense [`MigrationStep`] promises: it writes `schema_version` and adds one
/// member per scene, and rewrites nothing else. It is also idempotent — a scene that
/// already has the section keeps it, because the chain may legitimately run over a file a
/// person edited toward version 2 by hand, and discarding the events they added would be
/// data loss dressed up as a migration.
///
/// A document with no `scenes` member is left alone here rather than refused: it is not a
/// version problem, and the typed parse that follows reports it as the malformed document
/// it is.
fn add_events_section(value: &mut Value) -> Result<()> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| ProjectError::MigrationFailure {
            from: 1,
            to: 2,
            detail: "a version 1 document is not a JSON object, so there is no scene map to \
                 add a section to"
                .to_string(),
        })?;
    if let Some(Value::Object(scenes)) = object.get_mut("scenes") {
        for scene in scenes.values_mut() {
            if let Some(members) = scene.as_object_mut() {
                members
                    .entry("events".to_string())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
            }
        }
    }
    object.insert(
        "schema_version".to_string(),
        Value::from(crate::document::PROJECT_SCHEMA_VERSION),
    );
    Ok(())
}

/// Refuse a document version this build cannot interpret.
///
/// # Errors
///
/// [`ProjectError::UnsupportedSchema`].
pub fn check_document_schema(version: u32) -> Result<()> {
    if version == PROJECT_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ProjectError::UnsupportedSchema {
            which: "project document",
            found: version,
            supported: PROJECT_SCHEMA_VERSION,
        })
    }
}

/// Refuse a manifest version this build cannot interpret, which is the rule ADR-0005
/// Consequence 4 hands to this package.
///
/// # Errors
///
/// [`ProjectError::UnsupportedSchema`].
pub fn check_asset_schema(version: u32) -> Result<()> {
    if SUPPORTED_MANIFEST_SCHEMA_VERSIONS.contains(&version) {
        Ok(())
    } else {
        Err(ProjectError::UnsupportedSchema {
            which: "asset manifest",
            found: version,
            supported: anim_assets::MANIFEST_SCHEMA_VERSION,
        })
    }
}

/// The version a file claims, read from the JSON without parsing the document.
///
/// # Errors
///
/// [`ProjectError::Validation`] if the bytes are not a JSON object or the field is
/// missing or not a non-negative integer. A file that fails here is not an old version
/// — it is not a version at all, and reporting it as `UnsupportedSchema` would send the
/// reader to the migration chain to look for a path that cannot exist.
pub fn read_document_version(bytes: &[u8]) -> Result<u32> {
    version_of(&parse(bytes)?)
}

fn parse(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|e| ProjectError::Validation {
        location: "project document".to_string(),
        detail: format!("is not parseable JSON: {e}"),
    })
}

fn version_of(value: &Value) -> Result<u32> {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| ProjectError::Validation {
            location: "project document".to_string(),
            detail: "has no non-negative integer schema_version field".to_string(),
        })
}

/// Walk the chain from `from` to `to`, applying each step in order.
///
/// # Errors
///
/// [`ProjectError::MigrationFailure`] if no step advances the version at some point, or
/// a step fails, or the result does not report `to`. The last check is what stops a
/// step from reporting success while leaving the file unreadable by the loader that
/// follows it.
pub fn migrate_json(value: &mut Value, from: u32, to: u32, steps: &[MigrationStep]) -> Result<()> {
    let mut current = from;
    while current < to {
        let step = steps
            .iter()
            .find(|step| step.from == current && step.to > current)
            .ok_or_else(|| ProjectError::MigrationFailure {
                from,
                to,
                detail: format!("no migration advances version {current}"),
            })?;
        (step.apply)(value).map_err(|e| ProjectError::MigrationFailure {
            from: current,
            to: step.to,
            detail: format!("{}: {e}", step.description),
        })?;
        current = step.to;
    }
    let written = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());
    if written != Some(to) {
        return Err(ProjectError::MigrationFailure {
            from,
            to,
            detail: format!(
                "the chain finished but the document reports {:?}, not {to}",
                written
            ),
        });
    }
    Ok(())
}

/// Bring a file's bytes to the current version, using the real chain.
///
/// A file already at the current version passes through unchanged, which matters: a
/// migration that reformatted a current document would rewrite bytes on every load and
/// break the canonical-serialization proof spec 12.12 asks for.
///
/// # Errors
///
/// [`ProjectError::UnsupportedSchema`] for a version above this build,
/// [`ProjectError::MigrationFailure`] for a version below it with no path, and
/// [`ProjectError::Validation`] for bytes that are not a document.
pub fn upgrade_to_current(bytes: &[u8]) -> Result<Vec<u8>> {
    upgrade_with(bytes, chain())
}

/// The same upgrade against a caller's chain, which is how a migration is tested before
/// there is a second schema version to need one.
///
/// # Errors
///
/// As [`upgrade_to_current`].
pub fn upgrade_with(bytes: &[u8], steps: &[MigrationStep]) -> Result<Vec<u8>> {
    let mut value = parse(bytes)?;
    let found = version_of(&value)?;
    if found > PROJECT_SCHEMA_VERSION {
        return Err(ProjectError::UnsupportedSchema {
            which: "project document",
            found,
            supported: PROJECT_SCHEMA_VERSION,
        });
    }
    if found == PROJECT_SCHEMA_VERSION {
        // A current file is returned as the bytes it arrived as. Round-tripping it
        // through `serde_json::Value` would sort every object's keys alphabetically and
        // rewrite a document that needed nothing done to it, which would break the
        // byte-for-byte proof spec 12.12 asks for on a file this build wrote.
        return Ok(bytes.to_vec());
    }
    migrate_json(&mut value, found, PROJECT_SCHEMA_VERSION, steps)?;
    serde_json::to_vec(&value).map_err(|e| ProjectError::MigrationFailure {
        from: found,
        to: PROJECT_SCHEMA_VERSION,
        detail: format!("the migrated document could not be re-serialized: {e}"),
    })
}

/// Read bytes into a document: version check, chain, typed parse, full validation.
///
/// This is the only supported way to turn a file into a `ProjectDocument`. The typed
/// parse re-validates (see `document.rs`'s `Deserialize`), and the version numbers of
/// both the document and the manifest it embeds are checked here rather than assumed.
///
/// # Errors
///
/// Every category above, and [`ProjectError::Asset`] or
/// [`ProjectError::Validation`] from the document's own rules.
pub fn load_document(bytes: &[u8]) -> Result<ProjectDocument> {
    load_document_with_chain(bytes, chain())
}

/// [`load_document`] against a caller's chain.
///
/// # Errors
///
/// As [`load_document`].
pub fn load_document_with_chain(bytes: &[u8], steps: &[MigrationStep]) -> Result<ProjectDocument> {
    let upgraded = upgrade_with(bytes, steps)?;
    let value: Value = parse(&upgraded)?;

    // Both version numbers are checked from the parsed JSON *before* the typed read, and
    // the order is not fussiness. `ProjectDocument`'s `Deserialize` re-validates, so an
    // out-of-range embedded manifest version surfaces there as a boxed serde message, and
    // `load_document` would report it as a generic parse failure with the real reason
    // inside a string. ADR-0005 Consequence 4 assigns this range to WP03, and a rule
    // whose violation arrives untypeable is a rule a report cannot filter on (spec 7.4).
    check_document_schema(version_of(&value)?)?;
    if let Some(found) = value
        .get("assets")
        .and_then(|assets| assets.get("schema_version"))
        .and_then(Value::as_u64)
    {
        check_asset_schema(
            u32::try_from(found).map_err(|_| ProjectError::UnsupportedSchema {
                which: "asset manifest",
                found: u32::MAX,
                supported: anim_assets::MANIFEST_SCHEMA_VERSION,
            })?,
        )?;
    }

    let document: ProjectDocument = serde_json::from_value(value).map_err(|e| {
        // After a successful migration and a version check, a parse failure is a document
        // that claims a version it does not satisfy.
        ProjectError::Validation {
            location: "project document".to_string(),
            detail: format!("is not a valid version {PROJECT_SCHEMA_VERSION} document: {e}"),
        }
    })?;
    Ok(document)
}

/// A test-only chain, from a version 0 that never existed all the way to
/// [`PROJECT_SCHEMA_VERSION`].
///
/// Two steps now, and the second one is the real one: `V1_TO_V2`. That is deliberate.
/// While version 1 was the only version this project wrote, the migration machinery
/// could only be exercised by a synthetic step, and its own doc comment said so. It no
/// longer has to be: a test that starts at the synthetic version 0 walks the synthetic
/// rename and then the production step, so `migrate_json`'s multi-step loop, the
/// "chain finished but reports another version" guard and the real `events` rewrite are
/// all on the same path a user's version 1 file will take.
#[cfg(test)]
pub(crate) const TEST_CHAIN: &[MigrationStep] = &[V0_TO_V1, V1_TO_V2];

/// The synthetic first step. Version 0 is not a shape this project ever wrote; it exists
/// so that a chain of length two is what the tests run.
#[cfg(test)]
const V0_TO_V1: MigrationStep = MigrationStep {
    from: 0,
    to: 1,
    description: "test-only v0 to v1: rename `scenes_legacy` to `scenes`",
    apply: |value| {
        let object = value
            .as_object_mut()
            .ok_or_else(|| ProjectError::MigrationFailure {
                from: 0,
                to: 1,
                detail: "document is not an object".to_string(),
            })?;
        let scenes = object.remove("scenes_legacy").unwrap_or(Value::Null);
        object.insert("scenes".to_string(), scenes);
        // The version this step produces, and no more. Writing
        // `PROJECT_SCHEMA_VERSION` here would let a two-step chain report a document
        // that never passed through step two's rules.
        object.insert("schema_version".to_string(), Value::from(1u32));
        Ok(())
    },
};

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
    use uuid::Uuid;

    fn document() -> ProjectDocument {
        ProjectDocument::new(
            ProjectId::derive(
                &Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
                b"fixture",
            ),
            FrameRate::new(24, 1).unwrap(),
        )
    }

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    /// A hand-written document at `version`, with one scene and no assets.
    ///
    /// The scene is deliberately the **version 1** shape — no `events` member — because
    /// that is what makes this fixture useful now rather than merely convenient. Every
    /// file a person actually has on disk at a version below this build predates the event
    /// section, so a fixture that already carried one would let the production step look
    /// total while never having rewritten anything.
    fn bytes_at(version: u32, scenes_key: &str) -> Vec<u8> {
        // `scenes` is a map keyed by `SceneId`, so the fixture has to spell the id and
        // not just the scene: a scene object filed under `"name"` would be read as an
        // identifier, which is the failure this version of the fixture used to have.
        let scene_id =
            anim_core::id::SceneId::derive(&namespace(), b"shot01").to_canonical_string();
        let scene = serde_json::json!({
            "name": "shot01",
            "nodes": {},
            "tracks": {},
            "markers": [],
        });
        serde_json::to_vec(&serde_json::json!({
            "schema_version": version,
            "project_id": document().project_id,
            "revision": 0u64,
            "frame_rate": { "num": 24u32, "den": 1u32 },
            "assets": { "schema_version": 1u32, "records": {} },
            scenes_key: { scene_id: scene },
        }))
        .unwrap()
    }

    #[test]
    fn the_current_version_passes_through_byte_for_byte() {
        let bytes = serde_json::to_vec(&document()).unwrap();
        assert_eq!(
            upgrade_to_current(&bytes).unwrap(),
            bytes,
            "a document already at the current version must not be rewritten"
        );
    }

    #[test]
    fn a_version_above_this_build_is_unsupported_rather_than_migratable() {
        let bytes = bytes_at(PROJECT_SCHEMA_VERSION + 3, "scenes");
        let Err(ProjectError::UnsupportedSchema {
            found,
            which,
            supported,
        }) = upgrade_to_current(&bytes)
        else {
            panic!("reading a future schema must refuse");
        };
        assert_eq!(
            found,
            PROJECT_SCHEMA_VERSION + 3,
            "a refusal that reports the wrong version number is a refusal an operator \
             cannot act on"
        );
        assert_eq!(which, "project document");
        assert_eq!(supported, PROJECT_SCHEMA_VERSION);
    }

    #[test]
    fn no_path_is_a_migration_failure_not_an_unsupported_schema() {
        // Version 0 is below this build and the real chain starts at version 1, so there
        // is no way in. The two refusals name different remedies: one says "upgrade the
        // tool", the other says "write the step", and collapsing them would hide a
        // missing migration behind a message that tells the operator to do nothing.
        let bytes = bytes_at(0, "scenes_legacy");
        assert!(matches!(
            upgrade_to_current(&bytes),
            Err(ProjectError::MigrationFailure {
                from: 0,
                to: PROJECT_SCHEMA_VERSION,
                ..
            })
        ));
    }

    #[test]
    fn a_step_renames_a_field_and_the_loader_accepts_the_result() {
        // The synthetic version 0 still earns its place: it is the only way to run
        // `migrate_json`'s loop over more than one step, which is the shape a version 1
        // file will take when a version 3 exists and this build has to walk two steps to
        // reach it.
        let bytes = bytes_at(0, "scenes_legacy");
        let loaded = load_document_with_chain(&bytes, TEST_CHAIN).expect("v0 walks to this build");
        assert_eq!(loaded.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(
            loaded
                .scenes
                .values()
                .map(|scene| scene.name.as_str())
                .collect::<Vec<_>>(),
            vec!["shot01"],
            "the migrated document keeps the scene it had"
        );
    }

    /// The committed version 1 file, read from the repository.
    ///
    /// Spec 12.5's step 9 asks for a fixture per schema version, and a fixture generated
    /// inside the test that consumes it proves the round trip of the generator rather than
    /// that a file a person could have on disk is readable. This one is versioned text.
    fn version_one_fixture() -> Vec<u8> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir)
            .join("tests")
            .join("fixtures")
            .join("project_v1.anim.json");
        std::fs::read(&path).unwrap_or_else(|e| panic!("{} is versioned: {e}", path.display()))
    }

    #[test]
    fn a_committed_version_1_file_loads_through_the_real_chain() {
        let document = load_document(&version_one_fixture()).expect("a v1 file migrates to v2");
        assert_eq!(document.schema_version, PROJECT_SCHEMA_VERSION);
        // Everything the operator had survives, which is the only reason a migration is
        // allowed to rewrite a file at all.
        assert_eq!(document.revision, anim_core::id::Revision(4));
        let scene = document
            .scenes
            .values()
            .next()
            .expect("the fixture has one scene");
        assert_eq!(scene.name, "shot01");
        assert_eq!(scene.nodes().count(), 2);
        assert_eq!(scene.tracks().count(), 1);
        assert_eq!(scene.markers().len(), 2);
        assert_eq!(
            scene.events().count(),
            0,
            "a version 1 file had no events, and adding the section must not add data"
        );
        let track = scene.tracks().next().unwrap();
        assert_eq!(
            track.curve.keys().len(),
            2,
            "the keyed curve survives with both keys"
        );
        assert_eq!(
            scene
                .node(
                    &anim_core::id::NodeId::parse("22222222-2222-5222-8222-222222222222").unwrap()
                )
                .expect("the child node is still there")
                .sibling_order,
            3,
            "and so is its authored position among its siblings"
        );
    }

    #[test]
    fn the_step_adds_the_section_and_rewrites_nothing_else() {
        // The totality [`MigrationStep`] promises, checked against the committed file
        // rather than against a fixture this test wrote: every member that existed before
        // is byte-for-byte the same value afterwards, and the only additions are the
        // document's version number and one empty `events` object per scene.
        let before: serde_json::Value = serde_json::from_slice(&version_one_fixture()).unwrap();
        let after: serde_json::Value =
            serde_json::from_slice(&upgrade_to_current(&version_one_fixture()).unwrap()).unwrap();

        assert_eq!(before["schema_version"], serde_json::json!(1));
        assert_eq!(after["schema_version"], serde_json::json!(2));
        for key in ["project_id", "revision", "frame_rate", "assets"] {
            assert_eq!(
                before[key], after[key],
                "{key} moved, and it should not have"
            );
        }

        let scenes_before = before["scenes"].as_object().unwrap();
        let scenes_after = after["scenes"].as_object().unwrap();
        assert_eq!(scenes_before.len(), scenes_after.len());
        for (id, scene_before) in scenes_before {
            let scene_after = &scenes_after[id];
            let events = scene_after
                .get("events")
                .expect("the step added the section to this scene");
            assert_eq!(events, &serde_json::json!({}));
            for (member, value) in scene_before.as_object().unwrap() {
                assert_eq!(
                    &scene_after[member], value,
                    "scene member {member} was rewritten by a step that should not touch it"
                );
            }
            assert_eq!(
                scene_after.as_object().unwrap().len(),
                scene_before.as_object().unwrap().len() + 1,
                "the section is the only thing this step adds"
            );
        }
    }

    #[test]
    fn a_version_1_file_is_not_readable_as_version_2_without_the_step() {
        // The strictness rule, proven from the other side. `SceneData`'s `Deserialize` has
        // no `serde(default)` on `events`, so an unmigrated version 1 file is refused
        // rather than silently read as an empty event section — which is what forces every
        // real load of an old file through `chain()` and its recorded description.
        let bytes = version_one_fixture();
        assert!(matches!(
            load_document_with_chain(&bytes, &[]),
            Err(ProjectError::MigrationFailure { .. })
        ));
    }

    #[test]
    fn a_step_that_forgets_to_advance_the_version_is_caught() {
        // One step that goes all the way to this build's version and writes nothing, so
        // what fails is the chain's own last check rather than a missing step. An earlier
        // spelling of this test stopped at version 1 and was caught by the *missing-step*
        // branch, which is a different rule; the constant makes that impossible to
        // reintroduce by accident.
        const STUCK: &[MigrationStep] = &[MigrationStep {
            from: 0,
            to: PROJECT_SCHEMA_VERSION,
            description: "test-only: rewrites nothing",
            apply: |_| Ok(()),
        }];
        let bytes = bytes_at(0, "scenes");
        assert!(matches!(
            load_document_with_chain(&bytes, STUCK),
            Err(ProjectError::MigrationFailure { .. })
        ));
    }

    #[test]
    fn a_failing_step_reports_which_step_failed() {
        const FAILING: &[MigrationStep] = &[MigrationStep {
            from: 0,
            to: 1,
            description: "test-only: always fails",
            apply: |_| {
                Err(ProjectError::Validation {
                    location: "test".to_string(),
                    detail: "refused".to_string(),
                })
            },
        }];
        let Err(ProjectError::MigrationFailure { detail, .. }) =
            load_document_with_chain(&bytes_at(0, "scenes"), FAILING)
        else {
            panic!("a failing step must report");
        };
        assert!(
            detail.contains("always fails"),
            "the failure names the step: {detail}"
        );
    }

    #[test]
    fn bytes_that_are_not_a_document_are_refused_before_any_migration() {
        for bad in [
            b"not json".to_vec(),
            br#"{"schema_version":"one"}"#.to_vec(),
            br#"[]"#.to_vec(),
        ] {
            let error = upgrade_to_current(&bad).unwrap_err();
            assert!(
                matches!(error, ProjectError::Validation { .. }),
                "not a version problem, and reported that way: {error}"
            );
        }
    }

    #[test]
    fn an_unreadable_manifest_version_inside_a_current_document_is_refused() {
        let mut document = document();
        document.assets = anim_assets::AssetManifest::with_schema_version(99);
        let bytes = serde_json::to_vec(&document).unwrap();
        assert!(
            load_document(&bytes).is_err(),
            "ADR-0005 Consequence 4 hands the manifest range to this file, so a
             document whose embedded manifest is out of range must not load"
        );
    }
}
