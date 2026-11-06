//! The two-process determinism scenario for gate G3 (spec 13.12).
//!
//! G3's sentence is the reason this file exists and the reason an in-process test cannot
//! replace it: "G3 requires identical canonical snapshots and event logs from **two fresh
//! processes** for the same document revision." Inside one process, agreement is
//! structural — the same loaded constants, the same lazily initialised tables, the same
//! allocator, the same parsed namespace — so a second evaluation matching the first proves
//! only that the code is the code. The things the clause can actually catch are all leaks
//! from process state: an address-ordered iteration reaching a digest, a hash pre-image
//! picking up a pointer, an initialisation order that depends on which question was asked
//! first.
//!
//! So the evaluation happens in a child process, twice, and this file compares what it
//! printed. Spawned by path with an argument array, never through a shell (spec 9.4), and
//! via `CARGO_BIN_EXE_wp04_snapshot` so the child is the build the parent was compiled
//! against rather than whatever is sitting in `target/debug`.
//!
//! Three things this suite deliberately does not do:
//!
//! - **It does not rebuild the fixture in the parent.** `wp04_fixture` is shared with the
//!   binary for the same reason `wp03_save` writes its own expected bytes: a parent holding
//!   a second copy of the builder would compare one implementation against a duplicate of
//!   itself (DECISIONS.md #25, #30, #32).
//! - **It does not accept agreement as proof on its own.**
//!   `a_changed_document_changes_the_report` runs the same comparison machinery against a
//!   different authored state and asserts the digests move. Two identical outputs from two
//!   processes would be worth nothing if the scenario could not tell two different
//!   documents apart, and a checker that has never failed is not yet a checker.
//! - **It does not compare digests only.** The event log is printed as its ordered terms,
//!   so a reordering shows up as which of 13.7's five keys moved instead of as an opaque
//!   hash that says nothing further.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a fixture or a child that cannot be started makes the scenario unreadable, which is a panic rather than a test outcome; the workspace denial still governs every production crate"
)]

use std::path::Path;
use std::process::{Command, Stdio};

use anim_project::save::ProjectPaths;
use anim_project::schema;
use anim_scene::{snapshot_hash, SceneEvaluator};
use anim_test_support::wp04_fixture;

/// One run of the scenario: write the project file, start a fresh process against it, and
/// return what it printed.
///
/// Each label gets its own directory and its own file, so the runs are not merely separate
/// processes but separate readers of separate bytes — the case an operator's two exports
/// from the same project actually are.
fn run(label: &str, document: &anim_project::ProjectDocument) -> String {
    let root = std::env::temp_dir().join(format!("wp04-isolation-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("create {}: {e}", root.display()));
    let paths = ProjectPaths::from_root(&root);
    // Saved through the engine's own path, so the file the child reads is the canonical
    // bytes this build writes and not a re-serialisation of a `serde_json::Value`. That
    // distinction is ADR-0005 Consequence 7's, and it is the difference between testing
    // determinism and testing one document against two spellings of it.
    anim_project::save::save_atomic(document, &paths.document())
        .unwrap_or_else(|e| panic!("save: {e}"));

    let output = spawn(&paths.document());
    String::from_utf8(output).unwrap_or_else(|e| panic!("the child printed non-utf8: {e}"))
}

fn spawn(project: &Path) -> Vec<u8> {
    let exe = env!("CARGO_BIN_EXE_wp04_snapshot");
    let status = Command::new(exe)
        .arg("--project")
        .arg(project)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("spawn {exe}: {e}"));
    assert!(
        status.status.success(),
        "the scenario exited with {:?}; stderr:\n{}",
        status.status.code(),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        status.stderr.is_empty(),
        "a successful run must also be a quiet one:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
    status.stdout
}

/// Every `snapshot` line as `(position label, hex digest)`.
fn snapshots(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("snapshot ")?;
            let (position, digest) = rest.rsplit_once(' ')?;
            Some((position.trim().to_string(), digest.to_string()))
        })
        .collect()
}

/// Every `event` line, in the order printed.
fn events(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.starts_with("event "))
        .map(|line| line["event ".len()..].to_string())
        .collect()
}

#[test]
fn two_fresh_processes_report_identical_snapshots_and_event_logs() {
    let document = wp04_fixture::fixture();
    let first = run("first", &document);
    let second = run("second", &document);

    // The whole report, not just the digests: a difference in the header lines means the
    // two processes read different documents, which is a broken scenario rather than a
    // determinism failure, and the assertion below reports it as the difference it is.
    assert_eq!(
        first, second,
        "two fresh processes disagreed about the same revision"
    );

    let listed = snapshots(&first);
    assert_eq!(
        listed.len(),
        wp04_fixture::POSITIONS.len(),
        "the scenario reported {} positions, expected {}",
        listed.len(),
        wp04_fixture::POSITIONS.len()
    );
    for (position, digest) in &listed {
        assert_eq!(
            digest.len(),
            64,
            "a blake3 digest is 64 hex at {position}: {digest}"
        );
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "the digest must be lowercase hex at {position}: {digest}"
        );
    }

    // And the report is not vacuously identical because nothing was evaluated: frame 4 is
    // where the fixture authors its events.
    let log = events(&first);
    assert!(
        !log.is_empty(),
        "an empty event log would make the comparison trivially true"
    );
    assert!(
        log.iter().any(|line| line.split(' ').nth(2) == Some("1/2")),
        "the half-frame signal must appear in the log: {log:?}"
    );
}

#[test]
fn the_child_agrees_with_this_process_reading_the_same_bytes() {
    // The third route. If the child were self-consistent but different from the library —
    // a stale build, a feature difference between the bin and the test target — the
    // two-process check above would still pass. This is the assertion that ties the
    // scenario's numbers to the code the rest of the suite tests.
    let document = wp04_fixture::fixture();
    let report = run("in-process", &document);
    let listed = snapshots(&report);
    assert_eq!(listed.len(), wp04_fixture::POSITIONS.len());
    for (entry, (position, digest)) in wp04_fixture::POSITIONS.iter().zip(&listed) {
        let frame_position = wp04_fixture::position(entry);
        let snapshot =
            SceneEvaluator::evaluate(&document, wp04_fixture::scene_id(), frame_position)
                .unwrap_or_else(|e| panic!("evaluate {frame_position:?}: {e}"));
        let expected = snapshot_hash(&snapshot).unwrap().to_hex();
        assert_eq!(
            expected, *digest,
            "the parent and the child disagree at {position}"
        );
        assert_eq!(
            position,
            &format!("{} {}", frame_position.frame, frame_position.subframe()),
            "and the position it was labelled with"
        );
    }
}

#[test]
fn a_changed_document_changes_the_report() {
    // The negative control, and the reason the equality above means anything. One authored
    // number moves — a key value on the child's transform track — and every digest at a
    // position whose interval contains it must move with it. A scenario that printed the
    // same text for two different documents would pass the determinism test while
    // detecting nothing, which is the failure mode DECISIONS.md #31.13 records for the
    // kill suite and #32.4 records for expected-failure assertions.
    let unchanged = wp04_fixture::fixture();
    let changed = {
        let mut document = wp04_fixture::fixture();
        let scene = document
            .scenes
            .values_mut()
            .next()
            .expect("the fixture has one scene");
        let track = scene
            .tracks()
            .find(|track| track.node_id == scene.nodes().nth(1).unwrap().node_id)
            .expect("the child node has a track")
            .track_id;
        scene
            .set_track_key(
                &track,
                anim_project::PropertyKey::new(
                    anim_core::time::FramePos::at_frame(4),
                    anim_project::PropertyValue::Rational(
                        anim_core::rational::Rational::from_i128(101),
                    ),
                    anim_project::Easing::Linear,
                ),
            )
            .expect("the replacement key is legal");
        document
    };

    let before = run("control-before", &unchanged);
    let after = run("control-after", &changed);
    assert_ne!(
        before, after,
        "the scenario could not tell two documents apart, so its agreement proves nothing"
    );
    let before_map = snapshots(&before);
    let after_map = snapshots(&after);
    assert_eq!(
        before_map[0].1, after_map[0].1,
        "frame -40 sits before the first key, where the changed value cannot reach: the \
         digest must NOT move"
    );
    assert_ne!(
        before_map[4].1, after_map[4].1,
        "and the changed frame must move"
    );
}

#[test]
fn a_misshapen_command_line_is_refused_rather_than_ignored() {
    // The child's own argument handling, checked because the parent depends on it: a
    // scenario that ran with no project argument and printed an empty report would make
    // every comparison above pass.
    let exe = env!("CARGO_BIN_EXE_wp04_snapshot");
    let no_arguments = Command::new(exe)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("spawn {exe}: {e}"));
    assert!(!no_arguments.status.success(), "no --project must fail");
    assert!(
        String::from_utf8_lossy(&no_arguments.stderr).contains("--project"),
        "and say what it wanted"
    );

    let missing_file = Command::new(exe)
        .arg("--project")
        .arg(Path::new("this-file-does-not-exist.anim.json"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !missing_file.status.success(),
        "an unreadable file must fail"
    );
    // A `tempfile` directory rather than a hand-named path, because the alternative is a
    // `remove_file` at the end of the test and clippy.toml denies that method for the save
    // path's sake — a correct instinct that has nothing to do with a scenario tidying up
    // after itself, which is exactly the case for letting the RAII guard do it instead.
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let root = scratch.path().join("not-a-project.anim.json");
    std::fs::write(&root, b"{\"schema_version\":99}").unwrap();
    let wrong_version = Command::new(exe)
        .arg("--project")
        .arg(&root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !wrong_version.status.success(),
        "a future schema version must be refused rather than evaluated"
    );
}

#[test]
fn the_report_is_per_position_and_not_one_digest_for_the_run() {
    // 13.12 says "identical canonical snapshots", plural, and the clause is only worth
    // something if the comparison is per position. If the scenario collapsed every
    // position into one line, two processes could agree on one digest while disagreeing
    // about the other five.
    let report = run("granularity", &wp04_fixture::fixture());
    let listed = snapshots(&report);
    let distinct: std::collections::BTreeSet<&str> =
        listed.iter().map(|(_, d)| d.as_str()).collect();
    assert_eq!(
        distinct.len(),
        listed.len(),
        "six positions produced {} distinct digests",
        distinct.len()
    );
    let positions: std::collections::BTreeSet<&str> =
        listed.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(positions.len(), listed.len(), "one position reported twice");
}

#[test]
fn every_reported_position_is_the_normalized_spelling_of_its_instant() {
    // DECISIONS.md #22's rule, arriving at the scenario's text rather than at the type.
    // `FramePos` keeps its subframe in `[0, 1)` precisely so that `2 + 1/2` and `3 + 0/1`
    // cannot both exist as cache keys and event owners, and the child's report is a third
    // place a key is spelled. A position printed as `-1 -1/4` would be a legal-looking
    // line describing an instant the type says does not exist, and the parent's positional
    // comparison would then be matching strings rather than moments.
    let report = run("normalization", &wp04_fixture::fixture());
    let listed = snapshots(&report);
    assert_eq!(listed.len(), wp04_fixture::POSITIONS.len());
    for (position, _) in &listed {
        let (frame, subframe) = position
            .split_once(' ')
            .unwrap_or_else(|| panic!("the position {position:?} is not `frame fraction`"));
        let frame: i64 = frame.parse().expect("a frame number");
        let (num, den) = subframe
            .split_once('/')
            .unwrap_or_else(|| panic!("{subframe} is not a fraction"));
        let num: i128 = num.parse().expect("a numerator");
        let den: i128 = den.parse().expect("a denominator");
        assert!(
            den > 0,
            "{position}: a canonical rational has a positive denominator"
        );
        assert!(
            num == 0 || (num > 0 && num < den),
            "{position}: a negative frame of {frame} with subframe {subframe} would be a              second spelling of one instant, which is the failure that splits a cache key"
        );
        // And the printed form must be the reduced one, or `2/8` and `1/4` would be two
        // labels for one position in the very report the two processes are compared on.
        let rebuilt = anim_core::rational::Rational::new(num, den).unwrap();
        assert_eq!(
            rebuilt.to_string(),
            subframe,
            "{position}: the child printed an unreduced fraction"
        );
    }
}

#[test]
fn the_project_file_the_child_reads_is_the_one_the_engine_wrote() {
    // The last link in the chain, and the one a scenario can quietly miss: the child must
    // read the bytes `save_atomic` wrote rather than a re-serialisation the parent made.
    // If they differed, the two processes would agree while the file on disk described a
    // state the engine never produced.
    let document = wp04_fixture::fixture();
    let canonical = schema::serialize_canonical(&document).unwrap();
    let root = std::env::temp_dir().join(format!("wp04-bytes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let paths = ProjectPaths::from_root(&root);
    anim_project::save::save_atomic(&document, &paths.document()).unwrap();
    let on_disk = std::fs::read(paths.document()).unwrap();
    assert_eq!(
        on_disk, canonical,
        "the save path and the canonical serializer do not agree, so every digest in this \
         file is a digest of whichever one the reader happened to use"
    );
    schema::verify_canonical_bytes(&on_disk).expect("the file is canonical");
}
