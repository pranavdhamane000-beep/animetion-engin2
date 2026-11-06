//! Conflict, rollback, replay and history tests (spec 12.3, 12.12).
//!
//! This is the functional half of G2's suite; the fault-injection half is
//! `crates/anim_project/tests/persistence.rs` and the killed-process scenarios under
//! `crates/anim_test_support`. Spec 12.12 names its clauses in one sentence each, and
//! every test below is named for the clause it answers, so the gate can be audited by
//! reading this file against 12.12 rather than by trusting a summary.
//!
//! The oracle rule from WP01 and WP02 holds: an expected state is never produced by
//! calling the code under test in the same way twice and comparing the two results. Where
//! a test needs to know what the document should look like, it compares against the
//! pre-edit bytes, against a document built by a different route, or against the schema
//! artifact committed in the repository.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a fixture that cannot be built makes the scenario unreadable, which is a panic rather than a test outcome; the workspace denial still governs every production crate"
)]

use std::path::{Path, PathBuf};

use anim_commands::command::{
    AddNodePayload, Command, PlaceMarkerPayload, RemoveAssetPayload, RemoveMarkerPayload,
    RemoveNodePayload, RemovePropertyKeyPayload, RemoveScenePayload, RemoveTrackPayload,
    SetPropertyKeyPayload, UpsertAssetPayload, UpsertScenePayload, UpsertTrackPayload,
};
use anim_commands::envelope::CommandEnvelope;
use anim_commands::registry::{Registry, PAYLOAD_SCHEMA_VERSION};
use anim_commands::{CommandError, CommandProcessor};
use anim_core::id::Revision;
use anim_core::id::{AssetId, CommandId, NodeId, ProjectId, SceneId, TrackId};
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::property::{Easing, PropertyCurve, PropertyKey, PropertyPath, PropertyValue};
use anim_project::scene::{Marker, NodeData, SceneData, TrackData};
use anim_project::{AssetKind, AssetManifestEntry, ProjectDocument};
use serde_json::Value;

fn project_id() -> ProjectId {
    ProjectId::parse("10000000-0000-5000-8000-000000000001").unwrap()
}

fn scene_id() -> SceneId {
    SceneId::parse("10000000-0000-5000-8000-000000000002").unwrap()
}

fn node_id(byte: u8) -> NodeId {
    NodeId::parse(&format!("10000000-0000-5000-8000-{byte:012x}")).unwrap()
}

fn track_id(byte: u8) -> TrackId {
    TrackId::parse(&format!("10000000-0000-5000-8000-{byte:012x}")).unwrap()
}

fn asset_id(byte: u8) -> AssetId {
    AssetId::parse(&format!("10000000-0000-5000-8000-{byte:012x}")).unwrap()
}

fn command_id(byte: u8) -> CommandId {
    CommandId::parse(&format!("10000000-0000-5000-8000-{byte:012x}")).unwrap()
}

fn document() -> ProjectDocument {
    ProjectDocument::new(project_id(), FrameRate::new(24000, 1001).unwrap())
}

/// The document's authored state, revision aside.
///
/// `revision` is one of the six persisted fields, so an undo that restores a state
/// legitimately differs from the bytes it started at by exactly the counter that moved
/// forward. Every equality below that means "the same project" compares this, and the
/// tests that also care about the counter assert it separately.
fn state(document: &ProjectDocument) -> Vec<u8> {
    let mut stripped = document.clone();
    stripped.revision = Revision::ZERO;
    anim_project::schema::serialize_canonical(&stripped).unwrap()
}

fn envelope(byte: u8, revision: u64, label: &str, commands: Vec<Command>) -> CommandEnvelope {
    CommandEnvelope::new(
        project_id(),
        command_id(byte),
        Revision(revision),
        label,
        commands,
    )
}

fn add_scene() -> Command {
    Command::UpsertScene(UpsertScenePayload {
        scene_id: scene_id(),
        scene: SceneData::new("shot01").unwrap(),
    })
}

/// Removal of a scene that is not in the document, which is the batch-ending failure the
/// rollback loop needs: one refusal that lands at every position.
fn remove_absent_scene() -> Command {
    Command::RemoveScene(RemoveScenePayload {
        scene_id: SceneId::parse("10000000-0000-5000-8000-00000000ee00").unwrap(),
    })
}

fn add_node(byte: u8) -> Command {
    Command::AddNode(AddNodePayload {
        scene_id: scene_id(),
        node: NodeData::new(node_id(byte), None, u32::from(byte), "cel"),
    })
}

fn remove_node(byte: u8) -> Command {
    Command::RemoveNode(RemoveNodePayload {
        scene_id: scene_id(),
        node_id: node_id(byte),
    })
}

fn record(byte: u8, dependencies: Vec<AssetId>) -> AssetManifestEntry {
    AssetManifestEntry::new(
        asset_id(byte),
        anim_assets::hash_bytes(&[byte; 8]),
        8,
        AssetKind::Image,
        1,
        dependencies,
        format!("cel-{byte:03}"),
    )
    .unwrap()
}

fn upsert_asset(byte: u8) -> Command {
    Command::UpsertAsset(UpsertAssetPayload {
        record: record(byte, vec![]),
    })
}

fn track(byte: u8) -> TrackData {
    TrackData::new(
        track_id(byte),
        node_id(0xa0),
        PropertyPath::new("transform.x").unwrap(),
        PropertyCurve::with_default(Some(PropertyValue::Rational(Rational::from_i128(0)))),
    )
}

fn key(frame: i64, value: i128) -> PropertyKey {
    PropertyKey::new(
        FramePos::at_frame(frame),
        PropertyValue::Rational(Rational::from_i128(value)),
        Easing::Linear,
    )
}

fn marker(frame: i64, label: &str) -> Marker {
    Marker::new(FramePos::at_frame(frame), label.to_string())
}

fn place(byte: u8) -> Command {
    Command::PlaceMarker(PlaceMarkerPayload {
        scene_id: scene_id(),
        marker: marker(i64::from(byte), "cue"),
    })
}

fn up_track(byte: u8) -> Command {
    Command::UpsertTrack(UpsertTrackPayload {
        scene_id: scene_id(),
        track: track(byte),
    })
}

fn set_key(byte: u8, frame: i64, value: i128) -> Command {
    Command::SetPropertyKey(SetPropertyKeyPayload {
        scene_id: scene_id(),
        track_id: track_id(byte),
        key: key(frame, value),
    })
}

/// A processor with one scene already committed, which is where most scenarios start.
fn processor_with_scene() -> CommandProcessor {
    let mut processor = CommandProcessor::new(document());
    processor
        .apply(envelope(0x01, 0, "open shot01", vec![add_scene()]))
        .unwrap();
    processor
}

#[test]
fn a_stale_envelope_is_rejected_and_the_document_is_untouched() {
    // 12.12: "stale-envelope rejection".
    let mut processor = processor_with_scene();
    let before = state(processor.document());
    let revision = processor.document().revision;
    assert_eq!(revision, Revision(1));
    let error = processor
        .apply(envelope(
            0x02,
            0,
            "built against the previous revision",
            vec![add_node(0xa1)],
        ))
        .unwrap_err();
    assert_eq!(
        error,
        CommandError::RevisionConflict {
            expected: 0,
            actual: 1
        }
    );
    assert_eq!(state(processor.document()), before);
    assert_eq!(processor.document().revision, revision);
    assert_eq!(
        processor.history().undo_depth(),
        1,
        "the refused batch added no history entry"
    );

    // The same id may be offered again with a current revision: a refused envelope is not
    // recorded as accepted, so idempotency does not lock the caller out of retrying.
    let retried = processor
        .apply(envelope(0x02, 1, "same id, current", vec![add_node(0xa1)]))
        .unwrap();
    assert_eq!(retried.revision, Revision(2));
    assert!(!retried.replayed, "a refused replay is not a replay");
}

#[test]
fn a_duplicate_id_replays_the_original_result_and_a_different_batch_is_a_mismatch() {
    // 12.12: "exact duplicate handling", which is spec 12.7's sentence about the complete
    // canonical envelope hash: the id alone is not enough in either direction.
    let mut processor = processor_with_scene();
    let expected_first = envelope(0x03, 1, "add cel", vec![add_node(0xa2)])
        .hash()
        .unwrap();
    let first = processor
        .apply(envelope(0x03, 1, "add cel", vec![add_node(0xa2)]))
        .unwrap();
    let after_first = state(processor.document());

    let replay = processor
        .apply(envelope(0x03, 1, "add cel", vec![add_node(0xa2)]))
        .unwrap();
    assert!(
        replay.replayed,
        "the same envelope is recognized as the same work"
    );
    assert_eq!(replay.revision, first.revision);
    assert_eq!(state(processor.document()), after_first);
    assert_eq!(
        processor.document().revision,
        Revision(2),
        "and no third revision was taken"
    );

    // Same id, same label, one different byte inside the payload: not the same work.
    let error = processor
        .apply(envelope(0x03, 1, "add cel", vec![add_node(0xa3)]))
        .unwrap_err();
    match error {
        CommandError::CommandReplayMismatch {
            command_id: id,
            recorded,
            offered,
        } => {
            assert_eq!(id, command_id(0x03).to_canonical_string());
            assert_eq!(
                recorded,
                expected_first.to_hex(),
                "the refusal names the hash that was actually accepted"
            );
            assert_ne!(recorded, offered, "both digests are named and they differ");
        }
        other => panic!("expected a replay mismatch, got {other}"),
    }
    assert_eq!(state(processor.document()), after_first);
}

#[test]
fn one_batch_of_many_commands_takes_exactly_one_revision() {
    // 12.12: "one revision increment per batch", counted rather than inferred from a
    // final value.
    let mut processor = CommandProcessor::new(document());
    let commands = vec![
        add_scene(),
        add_node(0xa0),
        add_node(0xa1),
        up_track(0xb0),
        set_key(0xb0, 0, 1),
        set_key(0xb0, 24, 2),
        place(0x0c),
        upsert_asset(0xc0),
    ];
    let count = commands.len();
    let outcome = processor
        .apply(envelope(0x04, 0, "build a shot", commands))
        .unwrap();
    assert_eq!(count, 8);
    assert_eq!(outcome.command_count, count);
    assert_eq!(
        outcome.revision,
        Revision(1),
        "{count} commands, one revision"
    );
    assert_eq!(processor.document().revision, Revision(1));
    assert_eq!(processor.history().undo_depth(), 1, "and one history entry");
}

#[test]
fn rollback_is_exact_from_every_command_index_of_the_longest_batch() {
    // 12.12: "rollback from every command index". The prefixes are all legal and the
    // appended command fails against every one of them, so the refusal lands at a
    // different index each time round and a committed prefix would show as a changed
    // document at exactly that index.
    let legal = [
        add_scene(),
        add_node(0xa0),
        add_node(0xa1),
        up_track(0xb0),
        place(0x0c),
        upsert_asset(0xc0),
    ];
    for index in 0..legal.len() {
        let mut commands: Vec<Command> = legal[..=index].to_vec();
        commands.push(remove_absent_scene());
        let mut processor = CommandProcessor::new(document());
        let error = processor
            .apply(envelope(0x05, 0, "doomed batch", commands))
            .expect_err("the appended command must fail");
        assert!(
            matches!(error, CommandError::Validation { .. }),
            "index {index}: {error}"
        );
        assert_eq!(
            state(processor.document()),
            state(&document()),
            "index {index} left a committed prefix behind"
        );
        assert_eq!(processor.document().revision, Revision::ZERO);
        assert_eq!(
            processor.history().undo_depth(),
            0,
            "index {index} recorded a batch"
        );
    }
}

#[test]
fn undo_and_redo_are_exact_over_the_whole_command_set() {
    // 12.12: "undo/redo equality". Every command family is applied in one batch, then the
    // batch is undone in one step and redone: the state must return to the pre-batch bytes
    // after the undo and to the post-batch bytes after the redo.
    let mut processor = CommandProcessor::new(document());
    let before = state(processor.document());
    processor
        .apply(envelope(
            0x06,
            0,
            "every kind",
            vec![
                add_scene(),
                add_node(0xa0),
                up_track(0xb0),
                set_key(0xb0, 0, 1),
                set_key(0xb0, 12, 2),
                place(0x0c),
                upsert_asset(0xc0),
                upsert_asset(0xc1),
                Command::RemoveAsset(RemoveAssetPayload {
                    asset_id: asset_id(0xc1),
                }),
            ],
        ))
        .unwrap();
    let after = state(processor.document());
    assert_ne!(before, after, "the batch changed the project");

    let undone = processor.undo().unwrap();
    assert_eq!(
        state(processor.document()),
        before,
        "a nine-command batch must step back to exactly where it started"
    );
    assert_eq!(undone.command_count, 9);
    assert_eq!(
        undone.command_id, None,
        "an undo is not an envelope submission"
    );

    let redone = processor.redo().unwrap();
    assert_eq!(
        state(processor.document()),
        after,
        "and step forward to exactly where it was"
    );
    assert_eq!(redone.command_count, 9);
    assert_eq!(
        processor.document().revision,
        Revision(3),
        "the revision moved twice while the state returned to itself"
    );
    assert_eq!(processor.history().undo_depth(), 1);
    assert_eq!(processor.history().redo_depth(), 0);
}

#[test]
fn repeated_undo_and_redo_cycles_do_not_drift() {
    let mut processor = processor_with_scene();
    for round in 0..8u8 {
        // The state this round starts from, which is the previous round's state plus
        // whatever it left behind: undo returns to *here*, not to the opened shot, and
        // asserting the opened shot from round two onward would be a wrong expectation
        // rather than a caught defect.
        let before = state(processor.document());
        let revision = processor.document().revision.0;
        // A different node each round: the redo at the end of a round leaves its node in
        // the scene, and re-offering the same id would test the refusal rather than the
        // cycle.
        processor
            .apply(envelope(
                0x10 + round,
                revision,
                "nudge a cel",
                vec![add_node(0xa0 + round)],
            ))
            .unwrap();
        let changed = state(processor.document());
        processor.undo().unwrap();
        assert_eq!(
            state(processor.document()),
            before,
            "round {round}: undo did not return to the state this round started from"
        );
        processor.redo().unwrap();
        assert_eq!(
            state(processor.document()),
            changed,
            "round {round}: redo drifted"
        );
        // Eight cycles, and one node after each redo rather than one per round: an
        // inverse that added as well as removed would show up here as a growing scene.
        assert_eq!(
            processor
                .document()
                .scene(&scene_id())
                .unwrap()
                .nodes()
                .count(),
            usize::from(round) + 1,
            "each round leaves exactly the node it added"
        );
    }
    // Counted, not observed: one scene open, then three commits per round (the edit, the
    // undo and the redo each take a revision, which is what spec 12.6's "commits a new
    // revision" for undo means), so 1 + 8 * 3.
    assert_eq!(
        processor.document().revision,
        Revision(1 + 8 * 3),
        "a revision that moved differently would mean an undo or a redo committed twice          or not at all"
    );
}

#[test]
fn a_new_forward_edit_clears_redo_and_the_retired_ids_stay_idempotent() {
    // 12.12: "redo clearing".
    let mut processor = processor_with_scene();
    processor
        .apply(envelope(0x07, 1, "add a cel", vec![add_node(0xa0)]))
        .unwrap();
    processor
        .apply(envelope(0x08, 2, "add another", vec![add_node(0xa1)]))
        .unwrap();
    processor.undo().unwrap();
    processor.undo().unwrap();
    assert_eq!(processor.history().redo_depth(), 2);

    let revision = processor.document().revision.0;
    processor
        .apply(envelope(
            0x09,
            revision,
            "a different branch",
            vec![place(0x03)],
        ))
        .unwrap();
    assert_eq!(processor.history().redo_depth(), 0);
    assert_eq!(
        processor.redo().unwrap_err(),
        CommandError::RedoEmpty,
        "the abandoned branch is gone from the stacks"
    );

    // Its envelope id is still remembered, which is correct rather than sloppy: a retried
    // batch must not apply twice just because the transaction it created was stepped back.
    // Byte-for-byte the envelope that was accepted the first time, revision included:
    // 12.7's replay rule is about the complete canonical hash, so offering the same id with
    // a current-but-different revision must be a mismatch rather than a replay.
    let replay = processor
        .apply(envelope(0x08, 2, "add another", vec![add_node(0xa1)]))
        .unwrap();
    assert!(
        replay.replayed,
        "a retired transaction's id is still recognized"
    );
    assert_eq!(
        processor.document().scene(&scene_id()).unwrap().nodes().count(),
        0,
        "recognizing the retired id must not re-apply its node: both nodes were undone,          the forward edit that cleared redo added a marker instead, and a replay that          applied would put a node back that nothing asked for"
    );
}

#[test]
fn dry_run_and_apply_agree_at_one_revision() {
    // 12.12: "dry-run/apply equivalence at one revision", over three shapes: a batch that
    // passes, a batch addressed to a stale revision, and a batch whose second command
    // fails after the first would have committed.
    for (label, revision, commands) in [
        ("passes", 1u64, vec![add_node(0xa0)]),
        ("stale", 0u64, vec![add_node(0xa1)]),
        ("fails halfway", 1u64, vec![add_node(0xa2), add_node(0xa2)]),
    ] {
        let checking = processor_with_scene();
        let mut applying = processor_with_scene();
        let offered = envelope(0x0a, revision, label, commands);
        let dry = checking.dry_run(&offered);
        let wet = applying.apply(offered);
        match (&dry, &wet) {
            (Ok(report), Ok(outcome)) => {
                assert_eq!(report.would_be_revision, Some(outcome.revision));
                assert_eq!(report.command_count, outcome.command_count);
                assert_eq!(report.transaction_label, label);
                assert_eq!(
                    report.would_be_revision,
                    Some(applying.document().revision),
                    "{label}: the revision dry_run promised is not the one the document holds"
                );
                assert_eq!(
                    state(checking.document()),
                    state(processor_with_scene().document()),
                    "{label}: dry_run mutated the document it was only asked about"
                );
            }
            (Err(dry_error), Err(wet_error)) => assert_eq!(
                dry_error, wet_error,
                "{label}: the two paths refused differently"
            ),
            (dry, wet) => panic!(
                "{label}: dry_ok={} wet_ok={} disagree",
                dry.is_ok(),
                wet.is_ok()
            ),
        }
        assert_eq!(
            state(checking.document()),
            state(processor_with_scene().document()),
            "{label}: dry_run mutated the document"
        );
    }
}

#[test]
fn an_envelope_from_bytes_and_one_built_in_code_dispatch_to_the_same_commands() {
    // 12.10: "UI and headless callers submit identical envelopes". The registry is the only
    // path from bytes to a command, so the two callers cannot diverge by construction
    // rather than by agreement.
    let built = envelope(
        0x0b,
        0,
        "from code",
        vec![add_node(0xa0), place(0x03), upsert_asset(0xc0)],
    );
    let text = serde_json::to_vec(&built).unwrap();
    let submitted: CommandEnvelope = serde_json::from_slice(&text).unwrap();
    assert_eq!(submitted, built);
    assert_eq!(
        submitted.hash().unwrap(),
        built.hash().unwrap(),
        "and the replay identity survives the round trip"
    );
    assert_eq!(submitted.commands.len(), built.commands.len());

    // A command the registry does not dispatch is refused during the read, not silently
    // dropped from the batch -- a batch that lost a command would commit a prefix.
    let mut value: Value = serde_json::to_value(&built).unwrap();
    value["commands"][0]["kind"] = Value::String("set_camera_params".to_string());
    let error = serde_json::from_value::<CommandEnvelope>(value).unwrap_err();
    assert!(
        error.to_string().contains("set_camera_params"),
        "the refusal names what arrived: {error}"
    );

    // An out-of-range payload version is a refusal that names both halves of the pair.
    let mut versioned: Value = serde_json::to_value(&built).unwrap();
    versioned["commands"][0]["schema_version"] = Value::from(7u64);
    let error = serde_json::from_value::<CommandEnvelope>(versioned).unwrap_err();
    assert!(
        error.to_string().contains("payload schema version 7"),
        "{error}"
    );
}

#[test]
fn a_record_committed_by_a_command_resolves_against_the_store_that_produced_it() {
    // 12.2: "Asset payloads are persisted before their manifest commands", and spec 11.5
    // step 8's connection from WP02's record to a WP03 command. The ordering cannot be
    // checked by `Command::apply` alone, because 12.9 gives a command the document and not
    // the store -- so the check is import, commit, save, reload, verify.
    let dir = tempfile::tempdir().unwrap();
    let store = anim_assets::AssetStore::init(dir.path()).unwrap();
    let id = asset_id(0xd0);
    let imported = anim_assets::import(
        &store,
        id,
        "cel-001.png".to_string(),
        AssetKind::Image,
        anim_assets::CURRENT_IMPORTER_VERSION,
        std::io::Cursor::new(b"an imported payload".to_vec()),
    )
    .unwrap();

    let mut processor = processor_with_scene();
    let outcome = processor
        .apply(envelope(
            0x0c,
            1,
            "import cel-001",
            vec![Command::UpsertAsset(UpsertAssetPayload {
                record: imported.clone(),
            })],
        ))
        .unwrap();
    assert!(!outcome.replayed);

    // The object was on disk before the record was committed, so the record verifies.
    let committed = processor
        .document()
        .assets
        .get(&id)
        .expect("the record committed");
    store
        .verify(committed)
        .expect("a committed record resolves");

    let destination = dir.path().join("project.anim.json");
    anim_project::save::save_atomic(processor.document(), &destination).unwrap();
    let loaded = anim_project::schema::deserialize_canonical(
        &std::fs::read(&destination).expect("a file was written"),
    )
    .expect("the saved document reloads");
    assert_eq!(loaded.assets.get(&id).unwrap(), committed);

    // Reimporting the same id displaces the record, and undoing restores exactly what was
    // displaced: 11.6's reimport path is an ordinary command, not a special case.
    let replacement = anim_assets::import(
        &store,
        id,
        "cel-001.png".to_string(),
        AssetKind::Image,
        anim_assets::CURRENT_IMPORTER_VERSION,
        std::io::Cursor::new(b"a corrected payload".to_vec()),
    )
    .unwrap();
    assert_ne!(
        replacement.content_hash, imported.content_hash,
        "different bytes are a different asset revision under the same id"
    );
    let revision = processor.document().revision.0;
    processor
        .apply(envelope(
            0x0d,
            revision,
            "reimport cel-001",
            vec![Command::UpsertAsset(UpsertAssetPayload {
                record: replacement.clone(),
            })],
        ))
        .unwrap();
    assert_eq!(
        processor.document().assets.get(&id).unwrap().content_hash,
        replacement.content_hash
    );
    processor.undo().unwrap();
    assert_eq!(
        processor.document().assets.get(&id).unwrap().content_hash,
        imported.content_hash,
        "the undo restored the record the reimport displaced"
    );
    store
        .verify(processor.document().assets.get(&id).unwrap())
        .unwrap();
}

#[test]
fn removing_a_record_another_record_depends_on_is_refused_not_dangling() {
    let mut processor = processor_with_scene();
    processor
        .apply(envelope(
            0x0e,
            1,
            "two linked cels",
            vec![
                upsert_asset(0xd1),
                Command::UpsertAsset(UpsertAssetPayload {
                    record: record(0xd2, vec![asset_id(0xd1)]),
                }),
            ],
        ))
        .unwrap();
    let before = state(processor.document());
    let error = processor
        .apply(envelope(
            0x0f,
            processor.document().revision.0,
            "remove the dependency",
            vec![Command::RemoveAsset(RemoveAssetPayload {
                asset_id: asset_id(0xd1),
            })],
        ))
        .unwrap_err();
    assert!(
        matches!(
            error,
            CommandError::Document(anim_project::ProjectError::Asset(
                anim_assets::AssetError::MissingAssetId { .. }
            ))
        ),
        "11.7's missing-dependency refusal reaches the caller typed, not flattened: {error}"
    );
    assert_eq!(state(processor.document()), before);

    // Removing the dependent first is legal, and then the dependency can go too.
    let revision = processor.document().revision.0;
    processor
        .apply(envelope(
            0x11,
            revision,
            "remove in order",
            vec![
                Command::RemoveAsset(RemoveAssetPayload {
                    asset_id: asset_id(0xd2),
                }),
                Command::RemoveAsset(RemoveAssetPayload {
                    asset_id: asset_id(0xd1),
                }),
            ],
        ))
        .unwrap();
    assert!(processor.document().assets.is_empty());
}

#[test]
fn the_committed_command_schema_matches_the_encoder() {
    // The commands half of 12.12's canonical-serialization clause: the versioned external
    // schema under `schemas/commands/` and the payload bytes the registry writes agree on
    // every field name. Spec 12.2 requires the schemas; spec 5.1 names no validator
    // dependency, so this comparison is what makes the artifact mean something.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path: PathBuf = Path::new(manifest_dir)
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("schemas")
        .join("commands")
        .join("v1.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} is versioned and must be readable: {e}", path.display()));
    let schema_document: Value = serde_json::from_str(&text).unwrap();
    let declared = schema_document["commands"].as_object().unwrap();
    let registry = Registry::with_default_handlers();

    let samples = vec![
        upsert_asset(0xc0),
        Command::RemoveAsset(RemoveAssetPayload {
            asset_id: asset_id(0xc0),
        }),
        add_scene(),
        remove_absent_scene(),
        add_node(0xa0),
        remove_node(0xa0),
        up_track(0xb0),
        Command::RemoveTrack(RemoveTrackPayload {
            scene_id: scene_id(),
            track_id: track_id(0xb0),
        }),
        set_key(0xb0, 0, 1),
        Command::RemovePropertyKey(RemovePropertyKeyPayload {
            scene_id: scene_id(),
            track_id: track_id(0xb0),
            position: FramePos::at_frame(0),
        }),
        place(0x03),
        Command::RemoveMarker(RemoveMarkerPayload {
            scene_id: scene_id(),
            marker: marker(3, "cue"),
        }),
    ];
    assert_eq!(samples.len(), 12);
    assert_eq!(
        declared.len(),
        samples.len(),
        "the schema file and the command set hold different numbers of kinds"
    );

    for command in &samples {
        let kind = command.kind().as_str();
        let wire = registry.encode(command).unwrap();
        let entry = &declared[kind];
        let mut required: Vec<&str> = Vec::new();
        for name in entry["required"].as_array().unwrap() {
            required.push(name.as_str().unwrap());
        }
        required.sort_unstable();
        let mut actual: Vec<&str> = Vec::new();
        for name in wire.payload.as_object().unwrap().keys() {
            actual.push(name.as_str());
        }
        actual.sort_unstable();
        assert_eq!(
            actual, required,
            "{kind}: the encoder writes a different field set than the schema declares"
        );
        assert_eq!(
            entry["additionalProperties"].as_bool(),
            Some(false),
            "{kind}: a payload schema that allows extra members is not a schema"
        );
        assert_eq!(wire.schema_version, PAYLOAD_SCHEMA_VERSION);
        assert_eq!(
            wire.kind, kind,
            "the tagged form carries the declared kind text"
        );
        assert!(
            registry
                .handler(command.kind(), PAYLOAD_SCHEMA_VERSION)
                .is_ok(),
            "{kind} is declared but not dispatched"
        );
        // Every encoded payload decodes back to the command it came from.
        assert_eq!(
            registry.decode(&wire).unwrap(),
            *command,
            "{kind}: the registry's two directions disagree"
        );
    }

    // The envelope's own six fields, declared once in the schema and once in the
    // serializer.
    let envelope = envelope(0x12, 0, "schema check", samples.clone());
    let json: Value = serde_json::to_value(&envelope).unwrap();
    let mut actual: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut expected: Vec<&str> = Vec::new();
    for name in schema_document["required"].as_array().unwrap() {
        expected.push(name.as_str().unwrap());
    }
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        actual, expected,
        "the envelope's fields, spelled the same twice"
    );
}

#[test]
fn a_saved_document_reloads_to_the_state_the_processor_holds() {
    // 12.10: "Save serializes document and manifest together", and 12.10's "export resolves
    // an immutable saved revision" starts here: the file is the processor's state, not a
    // rendering of it.
    let dir = tempfile::tempdir().unwrap();
    let mut processor = processor_with_scene();
    processor
        .apply(envelope(
            0x13,
            1,
            "build a shot",
            vec![
                add_node(0xa0),
                up_track(0xb0),
                upsert_asset(0xc0),
                place(0x06),
            ],
        ))
        .unwrap();
    let destination = dir.path().join("project.anim.json");
    let report = anim_project::save::save_atomic(processor.document(), &destination).unwrap();
    assert_eq!(report.revision, processor.document().revision);

    let loaded = anim_project::schema::deserialize_canonical(
        &std::fs::read(&destination).expect("a file was written"),
    )
    .expect("the saved document reloads");
    assert_eq!(state(&loaded), state(processor.document()));
    assert_eq!(loaded.revision, processor.document().revision);
    assert_eq!(
        anim_assets::manifest_digest(&loaded.assets).unwrap(),
        anim_assets::manifest_digest(&processor.document().assets).unwrap(),
        "the ordered manifest digest is one value in memory and on disk (spec 11.10)"
    );
}

#[test]
fn every_command_kind_has_an_inverse_that_closes() {
    // The property the two tests above lean on, run over all twelve kinds rather than the
    // ones that were convenient: for each command, applying it and then stepping back
    // returns the exact bytes it started from. An inverse that is exact for `add_node` and
    // inexact for `place_marker` is the bug this whole file exists to find, and it would
    // surface two packages later as an editor whose undo button no longer matches the
    // document it claims to restore.
    let cases: Vec<(&'static str, Vec<Command>)> = vec![
        ("upsert_asset", vec![upsert_asset(0xc0)]),
        ("remove_asset", vec![upsert_asset(0xc2), remove_asset(0xc2)]),
        (
            "upsert_scene",
            vec![Command::UpsertScene(UpsertScenePayload {
                scene_id: scene_id(),
                scene: SceneData::new("second").unwrap(),
            })],
        ),
        ("remove_scene", vec![remove_scene_of(scene_id())]),
        ("add_node", vec![add_node(0xa0)]),
        ("remove_node", vec![add_node(0xa1), remove_node(0xa1)]),
        ("upsert_track", vec![add_node(0xa0), up_track(0xb0)]),
        (
            "remove_track",
            vec![add_node(0xa0), up_track(0xb1), rm_track(0xb1)],
        ),
        (
            "set_property_key",
            vec![add_node(0xa0), up_track(0xb2), set_key(0xb2, 0, 1)],
        ),
        (
            "remove_property_key",
            vec![
                add_node(0xa0),
                up_track(0xb3),
                set_key(0xb3, 0, 1),
                Command::RemovePropertyKey(RemovePropertyKeyPayload {
                    scene_id: scene_id(),
                    track_id: track_id(0xb3),
                    position: FramePos::at_frame(0),
                }),
            ],
        ),
        ("place_marker", vec![place(0x03)]),
        (
            "remove_marker",
            vec![
                place(0x04),
                Command::RemoveMarker(RemoveMarkerPayload {
                    scene_id: scene_id(),
                    marker: marker(4, "cue"),
                }),
            ],
        ),
    ];
    assert_eq!(cases.len(), 12);

    for (name, batch) in cases {
        // Every batch starts from the same opened shot and ends its own setup before the
        // command under test, so the final command in each batch is the one whose inverse
        // is being checked.
        let mut processor = processor_with_scene();
        let (setup, last) = batch.split_at(batch.len() - 1);
        if !setup.is_empty() {
            let revision = processor.document().revision.0;
            processor
                .apply(envelope(0x20, revision, "setup", setup.to_vec()))
                .unwrap_or_else(|error| panic!("{name}: the setup batch was refused: {error}"));
        }
        let before = state(processor.document());
        let revision = processor.document().revision.0;
        processor
            .apply(envelope(0x21, revision, name, last.to_vec()))
            .unwrap_or_else(|error| panic!("{name}: the command under test was refused: {error}"));
        let after = state(processor.document());
        assert_ne!(
            before, after,
            "{name}: the command changed nothing, so its inverse proves nothing"
        );
        processor.undo().unwrap();
        assert_eq!(
            state(processor.document()),
            before,
            "{name}: its inverse did not restore the state exactly"
        );
        processor.redo().unwrap();
        assert_eq!(state(processor.document()), after, "{name}: redo drifted");
    }
}

fn remove_scene_of(id: SceneId) -> Command {
    Command::RemoveScene(RemoveScenePayload { scene_id: id })
}

fn rm_track(byte: u8) -> Command {
    Command::RemoveTrack(RemoveTrackPayload {
        scene_id: scene_id(),
        track_id: track_id(byte),
    })
}

fn remove_asset(byte: u8) -> Command {
    Command::RemoveAsset(RemoveAssetPayload {
        asset_id: asset_id(byte),
    })
}
