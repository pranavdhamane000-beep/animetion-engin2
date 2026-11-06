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
/// Empty, because version 1 is the first version this project has written: spec 12.5's
/// step 9 asks for "fixtures for every schema version", and there is exactly one
/// version, so there is exactly one fixture and no step. The machinery above it is
/// exercised by a test-only chain rather than left unproven until the first real
/// migration needs it — a chain function that has never run is not a tested feature,
/// and the first time it runs is the time a user's file depends on it.
pub const fn chain() -> &'static [MigrationStep] {
    &[]
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

/// A test-only chain: version 0, which never existed, rewritten into version 1.
///
/// Its purpose is to make `chain()`, `migrate_json` and the "the chain finished but the
/// document reports none" guard true statements rather than comments. If the machinery
/// is wrong, this test fails now instead of the first real migration failing in a
/// user's session.
#[cfg(test)]
pub(crate) const TEST_CHAIN: &[MigrationStep] = &[MigrationStep {
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
        object.insert(
            "schema_version".to_string(),
            Value::from(PROJECT_SCHEMA_VERSION),
        );
        Ok(())
    },
}];

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
        assert_eq!(found, 4);
        assert_eq!(which, "project document");
        assert_eq!(supported, PROJECT_SCHEMA_VERSION);
    }

    #[test]
    fn no_path_is_a_migration_failure_not_an_unsupported_schema() {
        // Version 0 is below this build and `chain()` has no step for it. The two
        // refusals name different remedies: one says "upgrade the tool", the other says
        // "write the step", and collapsing them would hide a missing migration behind a
        // message that tells the operator to do nothing.
        let bytes = bytes_at(0, "scenes_legacy");
        assert!(matches!(
            upgrade_to_current(&bytes),
            Err(ProjectError::MigrationFailure { from: 0, to: 1, .. })
        ));
    }

    #[test]
    fn a_step_renames_a_field_and_the_loader_accepts_the_result() {
        let bytes = bytes_at(0, "scenes_legacy");
        let loaded = load_document_with_chain(&bytes, TEST_CHAIN).expect("v0 migrates to v1");
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

    #[test]
    fn a_step_that_forgets_to_advance_the_version_is_caught() {
        const STUCK: &[MigrationStep] = &[MigrationStep {
            from: 0,
            to: 1,
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
