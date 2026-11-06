//! Atomic save, backup, autosave, recovery, migration and the schema artifact (spec 12.12).
//!
//! This is the persistence half of G2's "save faults and migrations pass". It is an
//! integration test rather than unit tests for one reason: spec 12.11's claim is about the
//! *file system* ("preserve the original destination on serialization, write, flush or
//! replacement failure"), and the only way to test a claim about files is to make them.
//!
//! Spec 12.3 lists one test file for WP03, `crates/anim_commands/tests/atomicity.rs`. This
//! second file is an addition, recorded in DECISIONS.md #31 and in `BUILD_STATE.md`'s
//! deviation table: 12.12 requires "both functional and fault-injection suites", the
//! functional half is every clause of the atomicity file, and the persistence clauses have
//! no owning test file in the listing. Placing them inside `atomicity.rs` would mean an
//! `anim_commands` test that depends on `anim_project`'s private save orchestration, and
//! 12.4 puts the save path on the other side of that boundary.
//!
//! Every assertion about durability here is about bytes on disk, not about return values:
//! `write_atomic` and `Staged::publish` already report their own failures, and the question
//! G2 asks is what the directory looks like afterwards.
//!
//! The oracle rule from WP01 and WP02 applies unchanged: no expectation here is computed
//! by calling the code under test in the same way twice. The comparisons are a file
//! against the serializer's output for a *different* document (the previous revision), a
//! file against the schema artifact committed in the repository, or a file against bytes
//! this file wrote by hand.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a fixture that cannot be built makes the scenario unreadable, which is a panic rather than a test outcome; the workspace denial still governs every production crate"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "these scenarios damage files from outside the engine to simulate what an               interrupted run leaves behind; the rule the lint protects is about the save               path, which this file tests rather than bypasses"
)]

use std::path::{Path, PathBuf};

use anim_core::id::Revision;
use anim_core::id::{AssetId, EventDefinitionId, NodeId, ProjectId, SceneId, TrackId};
use anim_core::io::atomic_replace::DirectoryFlush;
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::event::{EventDefinition, EventKind};
use anim_project::property::{Easing, PropertyCurve, PropertyKey, PropertyPath, PropertyValue};
use anim_project::recovery::{CandidateState, RecoveryDecision};
use anim_project::save::{Boundary, ProjectPaths, RecoveryMetadata};
use anim_project::scene::{Marker, NodeData, SceneData, TrackData};
use anim_project::schema;
use anim_project::{AssetKind, AssetManifest, AssetManifestEntry, ProjectDocument};
use serde_json::Value;
use uuid::Uuid;

fn namespace() -> Uuid {
    Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
}

fn project_id() -> ProjectId {
    ProjectId::derive(&namespace(), b"persistence-fixture")
}

fn scene_id() -> SceneId {
    SceneId::derive(&namespace(), b"shot01")
}

fn rate() -> FrameRate {
    FrameRate::new(24000, 1001).unwrap()
}

/// A document with every persisted shape represented: an NTSC rate, one asset record with
/// a dependency, a two-level hierarchy, a keyed property with an authored default, and two
/// markers.
fn rich_document(revision: u64) -> ProjectDocument {
    let mut document = ProjectDocument::new(project_id(), rate());
    document.revision = Revision(revision);

    let cel = AssetId::derive(&namespace(), b"cel-001");
    let sheet = AssetId::derive(&namespace(), b"sheet-001");
    document.assets.insert(
        AssetManifestEntry::new(
            sheet,
            anim_assets::hash_bytes(b"exposure sheet"),
            14,
            AssetKind::Other,
            1,
            [],
            "sheet-001",
        )
        .unwrap(),
    );
    document.assets.insert(
        AssetManifestEntry::new(
            cel,
            anim_assets::hash_bytes(b"cel bytes"),
            9,
            AssetKind::Image,
            1,
            [sheet],
            "cel-001",
        )
        .unwrap(),
    );

    let root = NodeData::new(NodeId::derive(&namespace(), b"root"), None, 0, "root");
    let root_id = root.node_id;
    let mut scene = SceneData::new("shot01").unwrap();
    scene.insert_node(root).unwrap();
    let cel_id = NodeId::derive(&namespace(), b"cel");
    scene
        .insert_node(NodeData::new(cel_id, Some(root_id), 0, "cel"))
        .unwrap();
    scene
        .insert_track(TrackData::new(
            TrackId::derive(&namespace(), b"x-track"),
            root_id,
            PropertyPath::new("transform.x").unwrap(),
            PropertyCurve::new(
                [
                    PropertyKey::new(
                        FramePos::at_frame(24),
                        PropertyValue::Rational(Rational::new(1001, 24).unwrap()),
                        Easing::Linear,
                    ),
                    PropertyKey::new(
                        FramePos::at_frame(0),
                        PropertyValue::Rational(Rational::from_i128(0)),
                        Easing::Step,
                    ),
                    // WP04's third easing spelling, in the fixture so that the curve's
                    // canonical encoding and its save/load round trip are covered by every
                    // persistence test below rather than by one new one.
                    PropertyKey::new(
                        FramePos::at_frame(12),
                        PropertyValue::Rational(Rational::new(251, 10).unwrap()),
                        Easing::Curve(
                            anim_project::EasingCurve::new([
                                Rational::from_i128(0),
                                Rational::new(1, 4).unwrap(),
                                Rational::new(3, 4).unwrap(),
                                Rational::from_i128(1),
                            ])
                            .unwrap(),
                        ),
                    ),
                ],
                Some(PropertyValue::Rational(Rational::from_i128(0))),
            )
            .unwrap(),
        ))
        .unwrap();
    scene
        .insert_marker(Marker::new(FramePos::at_frame(12), "contact"))
        .unwrap();
    scene
        .insert_marker(Marker::new(FramePos::at_frame(0), "start"))
        .unwrap();
    // Two events, on two different nodes and with priorities that do not follow the
    // frames they sit on, so that a fixture consumer of spec 13.7's ordering has
    // something to order rather than an already-sorted list.
    scene
        .insert_event(EventDefinition::new(
            EventDefinitionId::derive(&namespace(), b"cut-to-cel"),
            FramePos::at_frame(12),
            3,
            root_id,
            EventKind::CameraCut,
            0,
        ))
        .unwrap();
    scene
        .insert_event(EventDefinition::new(
            EventDefinitionId::derive(&namespace(), b"foley-contact"),
            FramePos::new(12, Rational::new(1, 2).unwrap()).unwrap(),
            -1,
            cel_id,
            EventKind::AudioCue,
            1,
        ))
        .unwrap();
    assert!(
        document.insert_scene(scene_id(), scene).is_none(),
        "a fixture builds a new document, so nothing is displaced"
    );
    document
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn listing(root: &Path) -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("directory") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.push((
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    hex(&std::fs::read(&path).unwrap()),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn hex(bytes: &[u8]) -> String {
    anim_assets::hash_bytes(bytes).to_hex()
}

/// Every `.`-prefixed temporary still sitting in the directory. Spec 12.8 says these live
/// until replacement or failure cleanup, so a save that finished — successfully or not —
/// must leave none.
fn strays(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = listing(root)
        .into_iter()
        .filter(|(name, _)| name.contains(".tmp"))
        .map(|(name, _)| name)
        .collect();
    out.sort();
    out
}

fn failing_at(target: Boundary) -> impl Fn(Boundary) -> anim_project::Result<()> {
    move |reached| {
        if reached == target {
            Err(anim_project::ProjectError::Persistence {
                context: "injected fault",
                path: String::new(),
                detail: format!("{target} failed as instructed"),
            })
        } else {
            Ok(())
        }
    }
}

#[test]
fn a_saved_file_is_the_documents_canonical_bytes_and_loads_back_identical() {
    // Spec 12.12's "canonical serialization", stated the way 12.12 asks for it: against
    // a saved file, not against an in-memory value.
    let dir = tempfile::tempdir().unwrap();
    let paths = ProjectPaths::from_root(dir.path());
    let document = rich_document(7);

    anim_project::save::save_atomic(&document, &paths.document()).unwrap();
    let on_disk = read(&paths.document());
    assert_eq!(
        on_disk,
        schema::serialize_canonical(&document).unwrap(),
        "what reached the disk is what the serializer produced"
    );

    let loaded = schema::deserialize_canonical(&on_disk).unwrap();
    assert_eq!(loaded, document);
    assert_eq!(
        schema::serialize_canonical(&loaded).unwrap(),
        on_disk,
        "and re-serializing the loaded document reproduces the file byte for byte"
    );
    assert!(schema::verify_canonical_bytes(&on_disk).is_ok());
    assert_eq!(strays(dir.path()), Vec::<String>::new());
}

#[test]
fn the_second_save_backs_up_the_previous_state_before_replacing_it() {
    // Spec 4.3's layout, and the reason the backup is an input to recovery rather than a
    // copy of the current file: it is written from the destination's bytes, before the
    // rename, by us (ADR-0006 consequence 2).
    let dir = tempfile::tempdir().unwrap();
    let paths = ProjectPaths::from_root(dir.path());
    let first = rich_document(1);
    let second = rich_document(2);

    anim_project::save::save_atomic(&first, &paths.document()).unwrap();
    assert!(
        !paths.backup().exists(),
        "there is nothing to back up before a first replacement"
    );
    let report = anim_project::save::save_atomic(&second, &paths.document()).unwrap();
    assert_eq!(
        report.replacement.backup.as_deref(),
        Some(paths.backup().as_path())
    );
    assert_eq!(
        read(&paths.backup()),
        schema::serialize_canonical(&first).unwrap()
    );
    assert_eq!(
        read(&paths.document()),
        schema::serialize_canonical(&second).unwrap()
    );
    assert_eq!(report.revision, Revision(2));
    assert_eq!(report.bytes, read(&paths.document()).len() as u64);
}

#[test]
fn a_save_reports_the_directory_flush_it_achieved_rather_than_assuming_one() {
    // BUILD_STATE risk 10: which branch this machine takes was never measured, and spec
    // 12.11 asks only that the limitation be visible. So the assertion is the
    // equivalence, not one outcome or the other.
    let dir = tempfile::tempdir().unwrap();
    let paths = ProjectPaths::from_root(dir.path());
    let report = anim_project::save::save_atomic(&rich_document(1), &paths.document()).unwrap();
    let unsupported = matches!(
        report.replacement.directory_flush,
        DirectoryFlush::Unsupported { .. }
    );
    assert_eq!(
        report.durability_warning().is_some(),
        unsupported,
        "a warning exactly when the flush was not achieved, and not before"
    );
    if let Some(warning) = report.durability_warning() {
        assert_eq!(
            warning.severity,
            anim_core::diagnostic::DiagnosticSeverity::Warning
        );
        assert!(
            warning.message.contains("directory entry"),
            "{}",
            warning.message
        );
    }
}

#[test]
fn every_fault_boundary_leaves_the_destination_holding_one_complete_state() {
    // Spec 12.11's list — "serialization, write, flush or replacement failure" — with the
    // boundary names from 12.7's algorithm, and the file system as the judge.
    let original = rich_document(3);
    let replacement = rich_document(4);
    for boundary in Boundary::ALL {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path());
        anim_project::save::save_atomic(&original, &paths.document()).unwrap();
        let before = read(&paths.document());
        let backup_before = read(&paths.backup());

        let error = anim_project::save::save_atomic_observed(
            &replacement,
            &paths.document(),
            &mut failing_at(boundary),
        )
        .unwrap_err();
        assert!(
            matches!(error, anim_project::ProjectError::Persistence { .. }),
            "{boundary} reports as a persistence failure: {error}"
        );

        let after = read(&paths.document());
        if boundary == Boundary::AfterReplacement {
            // The replacement had already succeeded when the fault fired, so the honest
            // statement is that the new state is in place. 12.7 says the dirty flag clears
            // only after a successful replacement; this is the one boundary where a
            // reported failure and a completed publication coincide, and the destination
            // is still exactly one of the two whole documents rather than a blend.
            assert_eq!(after, schema::serialize_canonical(&replacement).unwrap());
            assert_eq!(read(&paths.backup()), before);
        } else {
            assert_eq!(after, before, "{boundary} altered the destination");
            assert_eq!(
                read(&paths.backup()),
                backup_before,
                "{boundary} touched the backup"
            );
        }
        assert_eq!(
            strays(dir.path()),
            Vec::<String>::new(),
            "{boundary} left a temporary behind"
        );
        // Whatever else happened, the file that survives is readable as a whole document.
        schema::verify_canonical_bytes(&after).expect("{boundary} left a damaged file");
    }
}

#[test]
fn a_save_into_a_missing_directory_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent").join("project.anim.json");
    let error = anim_project::save::save_atomic(&rich_document(0), &missing).unwrap_err();
    assert!(
        matches!(error, anim_project::ProjectError::Persistence { .. }),
        "{error}"
    );
    assert!(
        !dir.path().join("absent").exists(),
        "a save never invents a project root"
    );
}

#[test]
fn an_invalid_document_is_refused_before_the_file_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ProjectPaths::from_root(dir.path());
    let good = rich_document(1);
    anim_project::save::save_atomic(&good, &paths.document()).unwrap();
    let before = read(&paths.document());

    // The only invalid state a `ProjectDocument` can be put into through its public
    // fields: a schema version this build does not write.
    let mut future = good.clone();
    future.schema_version = 99;
    assert!(anim_project::save::save_atomic(&future, &paths.document()).is_err());
    assert_eq!(read(&paths.document()), before, "spec 12.7 validates first");
}

#[test]
fn an_autosave_lands_in_the_recovery_directory_and_is_a_complete_document() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ProjectPaths::from_root(dir.path());
    let report = anim_project::save::save_autosave(&rich_document(11), dir.path()).unwrap();
    assert_eq!(report.destination, paths.autosave());
    assert!(
        paths.recovery_dir().is_dir(),
        "the one place a save may create a directory is the recovery directory 4.3 names"
    );
    let loaded = schema::deserialize_canonical(&read(&paths.autosave())).unwrap();
    assert_eq!(loaded.revision, Revision(11));
    assert_eq!(
        loaded.project_id,
        project_id(),
        "an autosave is a real document, which is what makes it selectable"
    );
    assert!(
        !paths.backup().exists(),
        "an autosave is replaceable by definition and gets no backup of its own"
    );
}

#[test]
fn recovery_reads_the_three_inputs_and_never_writes_any_of_them() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ProjectPaths::from_root(dir.path());
    anim_project::save::save_atomic(&rich_document(1), &paths.document()).unwrap();
    anim_project::save::save_atomic(&rich_document(2), &paths.document()).unwrap();
    anim_project::save::save_autosave(&rich_document(3), dir.path()).unwrap();

    let before = listing(dir.path());
    let candidates = anim_project::recovery::inspect(&paths);
    let decision = anim_project::recovery::select(&candidates, project_id());
    // An autosave ahead of the manual save is the case 12.7 refuses to resolve silently.
    assert!(
        matches!(decision, RecoveryDecision::Choose(_)),
        "{decision:?}"
    );
    assert_eq!(
        listing(dir.path()),
        before,
        "inspection and selection create, touch and rewrite nothing"
    );

    // The same files, with the autosave behind the manual save, resolve to using current.
    anim_project::save::save_autosave(&rich_document(2), dir.path()).unwrap();
    let candidates = anim_project::recovery::inspect(&paths);
    assert_eq!(
        anim_project::recovery::select(&candidates, project_id()),
        RecoveryDecision::UseCurrent
    );
}

#[test]
fn a_recovery_parse_error_preserves_every_candidate_for_diagnosis() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ProjectPaths::from_root(dir.path());
    anim_project::save::save_atomic(&rich_document(5), &paths.document()).unwrap();
    let intact = read(&paths.document());
    std::fs::write(paths.backup(), b"{\"schema_version\":1,").unwrap();
    std::fs::write(prepare_autosave(dir.path()), b"not json at all").unwrap();

    let before = listing(dir.path());
    let candidates = anim_project::recovery::inspect(&paths);
    let states: Vec<&str> = candidates
        .iter()
        .map(|c| match &c.state {
            CandidateState::Complete(_) => "complete",
            CandidateState::Unreadable(_) => "unreadable",
            CandidateState::Absent => "absent",
        })
        .collect();
    assert_eq!(states, vec!["complete", "unreadable", "unreadable"]);
    let decision = anim_project::recovery::select(&candidates, project_id());
    assert_eq!(decision, RecoveryDecision::UseCurrent);
    assert_eq!(listing(dir.path()), before);
    assert_eq!(read(&paths.backup()), b"{\"schema_version\":1,".to_vec());
    assert_eq!(read(&paths.document()), intact);
}

/// The autosave path with `recovery/` created first. An inherent impl on `ProjectPaths`
/// is not available from a test, and this scenario has to write a damaged autosave by
/// hand, because the engine's own autosave path cannot produce one.
fn prepare_autosave(root: &Path) -> PathBuf {
    let paths = ProjectPaths::from_root(root);
    std::fs::create_dir_all(paths.recovery_dir()).unwrap();
    paths.autosave()
}

#[test]
fn an_old_schema_version_migrates_and_a_newer_one_is_refused_with_its_number() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ProjectPaths::from_root(dir.path());
    let document = rich_document(2);
    anim_project::save::save_atomic(&document, &paths.document()).unwrap();

    let bytes = read(&paths.document());
    assert_eq!(
        anim_project::migrate::read_document_version(&bytes).unwrap(),
        anim_project::document::PROJECT_SCHEMA_VERSION,
        "a file this build wrote reports the version this build writes"
    );
    assert_eq!(schema::deserialize_canonical(&bytes).unwrap(), document);

    // The other half of the same rule, in the direction that used to be hypothetical:
    // spec 12.5 step 9 asks for a fixture per version, and `project_v1.anim.json` is the
    // version below this one. Reading it is what proves the bump came with a way in
    // rather than only a way to refuse.
    let v1 = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("project_v1.anim.json"),
    )
    .unwrap();
    assert_eq!(
        anim_project::migrate::read_document_version(&v1).unwrap(),
        1
    );
    let migrated = schema::deserialize_canonical(&v1).expect("a v1 file is migratable");
    assert_eq!(
        migrated.schema_version,
        anim_project::document::PROJECT_SCHEMA_VERSION,
        "and it arrives at this build's version"
    );
    assert!(
        migrated
            .scenes
            .values()
            .all(|scene| scene.events().count() == 0),
        "the step adds the section and does not invent events to put in it"
    );

    let mut future = document.clone();
    future.schema_version = anim_project::document::PROJECT_SCHEMA_VERSION + 1;
    let future_bytes = serde_json::to_vec(&future).unwrap();
    let error = schema::deserialize_canonical(&future_bytes).unwrap_err();
    match error {
        anim_project::ProjectError::UnsupportedSchema {
            which,
            found,
            supported,
        } => {
            assert_eq!(which, "project document");
            assert_eq!(found, anim_project::document::PROJECT_SCHEMA_VERSION + 1);
            assert_eq!(supported, anim_project::document::PROJECT_SCHEMA_VERSION);
        }
        other => panic!("expected an unsupported-schema refusal, got {other}"),
    }
    // A future file is never rewritten by a save either: it cannot even be loaded.
    assert!(std::fs::write(paths.document(), &future_bytes).is_ok());
    assert!(schema::deserialize_canonical(&read(&paths.document())).is_err());
}

#[test]
fn a_manifest_version_the_document_cannot_migrate_is_refused_at_load() {
    // ADR-0005 Consequence 4 hands this range question to WP03, and `anim_assets`
    // deliberately does not answer it.
    let mut document = rich_document(1);
    document.assets = AssetManifest::with_schema_version(41);
    let bytes = serde_json::to_vec(&document).unwrap();
    let error = schema::deserialize_canonical(&bytes).unwrap_err();
    assert!(
        matches!(
            error,
            anim_project::ProjectError::UnsupportedSchema { which, .. }
                if which == "asset manifest"
        ),
        "{error}"
    );
}

#[test]
fn recovery_metadata_names_the_revision_that_makes_a_file_selectable() {
    let document = rich_document(6);
    let metadata = RecoveryMetadata::for_document(&document).unwrap();
    assert_eq!(metadata.revision, Revision(6));
    assert_eq!(metadata.project_id, project_id());
    assert_eq!(
        metadata.schema_version,
        anim_project::document::PROJECT_SCHEMA_VERSION
    );
    assert_eq!(
        metadata.manifest_digest,
        anim_assets::manifest_digest(&document.assets).unwrap(),
        "spec 11.10's ordered manifest digest is the same value here, so a report can \
         compare a save to an export"
    );

    let dir = tempfile::tempdir().unwrap();
    let paths = ProjectPaths::from_root(dir.path());
    anim_project::save::save_atomic(&document, &paths.document()).unwrap();
    let report = anim_project::recovery::read_candidate(&paths.document());
    let CandidateState::Complete(complete) = &report.state else {
        panic!("the file we just wrote is complete: {:?}", report.state)
    };
    assert_eq!(complete.metadata.digest, metadata.digest);
    assert!(complete.canonical);
    assert_eq!(
        report.revision(),
        Some(Revision(6)),
        "12.7's 'explicitly recorded revision' is this field, and no mtime is consulted"
    );
}

#[test]
fn the_committed_schema_matches_what_the_serializers_write() {
    // The clause G2 would otherwise not cover: an external artifact describing the
    // persisted shape, and the encoder that produces it, agreeing. Read from the
    // repository, not from a build-time generation step, so this file is what a person
    // can open and disagree with.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = Path::new(manifest_dir)
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("schemas")
        .join("project")
        .join("v2.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} is versioned and must be readable: {e}", path.display()));
    let schema_document: Value = serde_json::from_str(&text).unwrap();
    let defs = schema_document["$defs"].as_object().unwrap();

    let document = rich_document(1);
    let document_json: Value =
        serde_json::from_slice(&schema::serialize_canonical(&document).unwrap()).unwrap();
    check(&schema_document, "document", &document_json);
    let scene = document.scene(&scene_id()).unwrap();
    check(
        &schema_document,
        "scene",
        &serde_json::to_value(scene).unwrap(),
    );
    let (first_node, second_node) = {
        let mut nodes = scene.nodes();
        (nodes.next().unwrap(), nodes.next().unwrap())
    };
    for node in [first_node, second_node] {
        check(
            &schema_document,
            "node",
            &serde_json::to_value(node).unwrap(),
        );
    }
    let track = scene.tracks().next().unwrap();
    check(
        &schema_document,
        "track",
        &serde_json::to_value(track).unwrap(),
    );
    check(
        &schema_document,
        "property_curve",
        &serde_json::to_value(&track.curve).unwrap(),
    );
    for key in track.curve.keys() {
        check(
            &schema_document,
            "property_key",
            &serde_json::to_value(key).unwrap(),
        );
    }
    for marker in scene.markers() {
        check(
            &schema_document,
            "marker",
            &serde_json::to_value(marker).unwrap(),
        );
    }
    for event in scene.events() {
        check(
            &schema_document,
            "event_definition",
            &serde_json::to_value(event).unwrap(),
        );
    }
    // The two new definitions the version bump exists for. `check` compares a required
    // list against a serialized object, and an `Easing` is a string or an object
    // depending on the variant, so the easing spelling is asserted directly instead.
    assert!(
        defs.contains_key("easing_curve"),
        "the schema describes the curve the Rust type can now hold"
    );
    assert!(defs.contains_key("event_kind"));
    assert!(
        serde_json::to_value(Easing::Linear).unwrap() == "linear",
        "a unit variant stays a bare name, which is the spelling its canonical tag uses"
    );
    let curve = serde_json::to_value(Easing::Curve(
        anim_project::EasingCurve::new([
            anim_core::rational::Rational::from_i128(0),
            anim_core::rational::Rational::new(1, 4).unwrap(),
            anim_core::rational::Rational::from_i128(1),
        ])
        .unwrap(),
    ))
    .unwrap();
    check(&schema_document, "easing_curve", &curve["curve"]);
    assert!(curve["curve"]["points"].is_array());
    let record = document.assets.iter().next().unwrap();
    check(
        &schema_document,
        "manifest_record",
        &serde_json::to_value(record).unwrap(),
    );
    check(
        &schema_document,
        "manifest",
        &serde_json::to_value(&document.assets).unwrap(),
    );
    check(
        &schema_document,
        "frame_rate",
        &serde_json::to_value(document.frame_rate).unwrap(),
    );
    check(
        &schema_document,
        "frame_pos",
        &serde_json::to_value(FramePos::at_frame(3)).unwrap(),
    );
    assert!(defs.contains_key("property_value"));

    fn check(root: &Value, name: &str, value: &Value) {
        let definition = if name == "document" {
            root.clone()
        } else {
            root["$defs"][name].clone()
        };
        let required: Vec<String> = definition["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} declares no required list"))
            .iter()
            .map(|v| v.as_str().expect("names are strings").to_string())
            .collect();
        let object = value
            .as_object()
            .unwrap_or_else(|| panic!("{name}: the serializer wrote a non-object"));
        let mut actual: Vec<String> = object.keys().cloned().collect();
        let mut expected = required.clone();
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "{name}: the file on disk and the committed schema disagree about the field set"
        );
        assert_eq!(
            definition["additionalProperties"].as_bool(),
            Some(false),
            "{name}: a schema that allows extra members is not describing this document"
        );
    }
}

#[test]
fn the_schema_patterns_describe_refusals_the_loaders_already_make() {
    // Not a new rule: this is the property that makes the schema's patterns worth
    // writing at all, so it is checked against the loaders rather than asserted of the
    // file. A pattern nothing refuses is decoration.
    for bad in [r#""0/2""#, r#""-1/-2""#, r#""3/0""#, r#""1.5""#, r#""0""#] {
        let json = format!("{{\"frame\":3,\"subframe\":{bad}}}");
        assert!(
            serde_json::from_str::<FramePos>(&json).is_err(),
            "{json} must not load: the schema's `rational` pattern allows one spelling"
        );
    }
    let uppercase = "00000000-0000-5000-8000-0000000000AB";
    assert!(
        ProjectId::parse(uppercase).is_err(),
        "the `uuid` pattern is lowercase, and the parser already refuses this"
    );
    assert!(
        PropertyPath::new("transform x").is_err(),
        "the path pattern is enforced on load, not only declared"
    );
}
