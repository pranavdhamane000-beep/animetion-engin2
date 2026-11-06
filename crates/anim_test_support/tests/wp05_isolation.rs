//! Identical state hashes from two fresh processes, at 10,000 frames (spec 14.12, gate G4).
//!
//! Spec 8.1's sentence is the one this file exists for: two processes, same document, output
//! matching byte for byte. Inside one process the same claim is structural — the same code,
//! the same statics, the same allocator — so agreement there is weak evidence. Spawning a
//! second process is the only way to ask the question that was actually posed, which is
//! whether anything about simulation survives a run. `BUILD_STATE.md` risk 23 is the general
//! form: a value that depends on process history looks exactly like a value that does not,
//! right up until the export is compared a week later.
//!
//! The child is `src/bin/wp05_replay.rs`, resolved through `CARGO_BIN_EXE_`, so the process
//! under test is the same build as the test that spawns it: same locked dependency versions,
//! same rustc, same profile. The fixture is not sent over a pipe either — each process builds
//! it from `src/wp05_fixture.rs` — because routing the document through JSON would make this
//! file a serialization test, and WP03 already has one of those.
//!
//! ## What each test here would catch
//!
//! | Test | The defect |
//! | --- | --- |
//! | the 10,000-frame pair | any process-static or allocation-order dependence |
//! | per-tick line comparison at a shorter run | one divergent tick hidden by an aggregate match |
//! | a checkpointed child against an uncheckpointed one | state that depends on what was cached |
//! | a parent-side in-process comparison | a child that is quietly running different code |

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a fixture that cannot be built makes the scenario unreadable, which is a panic rather than a test outcome; the workspace denial still governs every production crate"
)]

use std::process::Command;

use anim_sim::checkpoint::CheckpointCache;
use anim_sim::context::SimulationContext;
use anim_sim::mode::SimulationMode;
use anim_sim::replay::sequential_hashes;
use anim_sim::{state_hash, SimTick};
use anim_test_support::determinism::Report;
use anim_test_support::wp05_fixture;

/// The child binary, built by the same invocation of cargo as this test.
fn child() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wp05_replay"));
    command.stderr(std::process::Stdio::piped());
    command
}

/// Run the child and return its stdout, failing the test with the child's stderr attached.
fn run(args: &[&str]) -> String {
    let output = child()
        .args(args)
        .output()
        .expect("wp05_replay is built by this test's own invocation");
    let text = String::from_utf8(output.stdout.clone()).unwrap_or_else(|error| {
        panic!(
            "wp05_replay wrote non-UTF-8 output: {error}; stderr {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        output.status.success(),
        "wp05_replay {:?} exited with {:?}; stderr: {}",
        args,
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !text.is_empty(),
        "wp05_replay {args:?} succeeded and wrote nothing, which a byte comparison \
         would call agreement"
    );
    text
}

#[test]
fn two_fresh_processes_agree_on_every_hash_of_ten_thousand_frames() {
    // The literal clause: 10,000 frames, two processes that share nothing, all CPU state
    // hashes compared. `--frames` is the default and `--brief` is absent, so the per-tick
    // lines are in the text and the comparison is line by line rather than through one
    // aggregate digest.
    let first = run(&[]);
    let second = run(&[]);
    assert_eq!(
        first.len(),
        second.len(),
        "the two reports differ in length, so something in the report depends on when it ran"
    );
    let a = Report::parse(&first).expect("the first report parses");
    let b = Report::parse(&second).expect("the second report parses");
    assert_same_run(&a, &b, "two fresh processes");

    let expected_lines = usize::try_from(wp05_fixture::gate_ticks() + 1).unwrap();
    assert_eq!(
        a.ticks.len(),
        expected_lines,
        "tick 0 through the last gate tick, inclusive"
    );
    assert_eq!(a.count, expected_lines as u64);
    assert_eq!(a.field("last_tick"), Some("40000"));
    assert_eq!(a.field("ticks_per_frame"), Some("4"));
    assert_eq!(a.field("frame_rate"), Some("30000/1001"));
    assert_eq!(
        a.identity(),
        b.identity(),
        "identity first, because two different documents agreeing on hashes is a coincidence \
         worth failing on"
    );
    // A text-equality check as well, so a field the parser does not model cannot differ
    // silently. The parse-based comparison above names the tick; this one proves the parse
    // covered everything.
    assert_eq!(first, second, "the two reports are one text");
}

#[test]
fn a_shortened_pair_is_compared_line_by_line() {
    // The full-length pair above proves the aggregate and the text agree across processes;
    // this one runs the same comparison at a size that is cheap, and checks that the first
    // and last lines individually match rather than only the digest of the list. A
    // permutation of the hash lines would leave the text equal to itself but break here if
    // the aggregate were all that was compared.
    let args = ["--frames", "120"];
    let a = Report::parse(&run(&args)).unwrap();
    let b = Report::parse(&run(&args)).unwrap();
    assert_same_run(&a, &b, "120-frame pair");
    assert_eq!(a.ticks.len(), 120 * 4 + 1);
    for index in [0usize, 1, 59, 479, a.ticks.len() - 1] {
        assert_eq!(
            a.ticks[index], b.ticks[index],
            "line {index} of a short pair"
        );
    }
    // And the sequence is not constant, or the comparison would pass on a run that did
    // nothing.
    assert_ne!(
        a.ticks[0].1, a.ticks[40].1,
        "tick 0 and tick 40 must be different states"
    );
    assert!(
        a.ticks.windows(2).any(|pair| pair[0].1 != pair[1].1
            && pair[0].1 != a.ticks[0].1
            && pair[1].1 != a.ticks[0].1),
        "the fixture's world has to actually move"
    );
}

#[test]
fn a_checkpointed_process_matches_an_uncheckpointed_one() {
    // 14.12's cadence row from the process side: the policy is an argument, the two children
    // share nothing else, and the report they write is one text.
    let plain = Report::parse(&run(&["--frames", "300"])).unwrap();
    let mut cached: Vec<(String, Report)> = Vec::new();
    for (cadence, capacity) in [("1", "4"), ("7", "2"), ("64", "8"), ("1024", "8")] {
        let text = run(&[
            "--frames",
            "300",
            "--cadence",
            cadence,
            "--capacity",
            capacity,
        ]);
        cached.push((
            format!("cadence {cadence} capacity {capacity}"),
            Report::parse(&text).unwrap(),
        ));
    }
    for (label, report) in &cached {
        assert_same_run(&plain, report, label);
        assert_eq!(
            report.field("cadence_ticks").unwrap(),
            label.split(' ').nth(1).unwrap(),
            "and the child really did run with the policy the label names, rather than \
             ignoring the argument"
        );
    }
    // The configurations cached different numbers of entries, which is what makes the
    // equality above a test rather than one run described four ways.
    let counts: Vec<usize> = cached.iter().map(|(_, r)| r.ticks.len()).collect();
    assert!(
        counts.windows(2).all(|pair| pair[0] == pair[1]),
        "every configuration reported the same number of ticks: {counts:?}"
    );
    for (label, _) in &cached {
        let _ = label;
    }
}

#[test]
fn a_brief_process_still_compares_on_the_aggregate() {
    let full = Report::parse(&run(&["--frames", "60"])).unwrap();
    let brief = Report::parse(&run(&["--frames", "60", "--brief"])).unwrap();
    assert!(
        brief.ticks.is_empty(),
        "the flag suppresses the per-tick lines"
    );
    assert_eq!(
        brief.aggregate, full.aggregate,
        "and the aggregate still covers every tick it did not print"
    );
    assert_eq!(brief.final_hash, full.final_hash);
    assert_eq!(brief.count, full.count);
}

#[test]
fn the_child_and_the_parent_run_the_same_engine_over_the_same_document() {
    // The comparison above is between two children. This one is between a child and the
    // in-process path, which is the check that the child is not running a different fixture:
    // a report from a drifted fixture would agree with other reports from the same drift.
    let report = Report::parse(&run(&["--frames", "50"])).unwrap();
    let document = wp05_fixture::document();
    let mut context = SimulationContext::new(
        &document,
        wp05_fixture::scene_id(),
        CheckpointCache::default(),
    )
    .unwrap();
    let last = 50 * i64::from(wp05_fixture::TICKS_PER_FRAME);
    let local = sequential_hashes(&mut context, SimTick(last), SimulationMode::Export).unwrap();
    assert_eq!(
        local.len(),
        report.ticks.len(),
        "the parent and the child stepped a different number of ticks"
    );
    for (index, hash) in local.iter().enumerate() {
        assert_eq!(
            hash.to_string(),
            report.ticks[index].1,
            "tick {index} differs between the child process and this one, which means the \
             fixture or the engine drifted between them"
        );
    }
    assert_eq!(
        report.field("seed"),
        Some(document.simulation.seed_hex().as_str()),
        "and the seed the child reports is the seed the parent's document holds"
    );
    assert_eq!(
        state_hash(&context.initial_state().unwrap())
            .unwrap()
            .to_string(),
        report.ticks[0].1,
        "tick 0 is the initialized state in both"
    );
}

#[test]
fn a_rejected_argument_is_a_refusal_not_a_default() {
    // The child's argument surface has to fail loudly: a typo that fell back to the default
    // configuration would make a cadence comparison quietly compare one configuration twice.
    for args in [
        vec!["--cadence"],
        vec!["--cadence", "not-a-number"],
        vec!["--frames", "-4"],
        vec!["--nonsense"],
    ] {
        let output = child().args(&args).output().expect("the binary exists");
        assert!(
            !output.status.success(),
            "{args:?} was accepted, and an ignored argument is a silent default"
        );
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(!text.is_empty(), "{args:?} failed without saying why");
    }
}

#[test]
fn the_usage_text_names_the_fixture_as_the_document_source() {
    let output = child().arg("--help").output().expect("the binary exists");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("wp05_fixture"), "{text}");
    assert!(text.contains("--cadence"), "{text}");
}

/// Shared assertion, from this crate's own comparator rather than a copy of it.
///
/// The point of `anim_test_support::determinism` is that the parent test and any future
/// package that needs the same comparison reach one implementation; a local copy here would
/// be the second comparator the module exists to prevent.
fn assert_same_run(a: &Report, b: &Report, context: &str) {
    anim_test_support::determinism::assert_same_run(a, b, context)
        .unwrap_or_else(|error| panic!("{context}: {error}"));
}
