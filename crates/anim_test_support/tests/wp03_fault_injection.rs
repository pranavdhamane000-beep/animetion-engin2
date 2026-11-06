//! The save fault-injection suite, run by killing a process at each save stage
//! (spec 12.12, gate G2).
//!
//! Spec 12.12's sentence is unusually literal: "Kill test processes at each save stage and
//! verify current, backup and autosave recovery behavior." The in-process seam in
//! `crates/anim_project/tests/persistence.rs` proves what the save code *does* when a step
//! returns an error. This file proves the stronger and less comfortable claim — that the
//! outcome does not depend on this process ever getting another turn. A killed process runs
//! no destructors, so the guard that makes an injected failure tidy is not on the table,
//! and what remains is only what a same-volume `MoveFileExW` with `WRITE_THROUGH` actually
//! leaves on NTFS (ADR-0006).
//!
//! Spawned by path with an argument array, never through a shell (spec 9.4).
//! `CARGO_BIN_EXE_wp03_save` is the path cargo built for this package's own binary, which
//! is what makes the child the same code as the parent rather than whatever happens to be
//! sitting in `target/debug`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a fixture or a child that cannot be started makes the scenario unreadable, which is a panic rather than a test outcome; the workspace denial still governs every production crate"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "bounded wait loops for an external process, which clippy.toml permits for \
              probes; every sleep below is inside a deadline that terminates the child when \
              it is exceeded"
)]
#![allow(
    clippy::disallowed_types,
    reason = "Instant bounds a wait on another process; no duration reaches a document, a \
              hash or canonical output"
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use anim_project::save::{Boundary, ProjectPaths};
use serde_json::Value;

/// How long to wait for the child to announce it has reached the boundary.
const STAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long to wait for a child that is expected to finish on its own.
const EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const POLL: std::time::Duration = std::time::Duration::from_millis(10);
/// The child's own patience, mirrored here so a scenario that waits past it is reported as
/// the test's failure rather than silently becoming an in-process fault injection.
const CHILD_PARK_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

struct Scenario {
    root: PathBuf,
    paths: ProjectPaths,
    old: Vec<u8>,
    new: Vec<u8>,
}

impl Scenario {
    /// A project directory in a known state: the child has seeded `project.anim.json` and
    /// `recovery/autosave.anim.json` with the *previous* document and is about to replace
    /// the authoritative file with the new one.
    ///
    /// The two expected byte streams are read from the child rather than rebuilt here. A
    /// parent that constructed its own copy of the fixture would be comparing one
    /// implementation of the builder against a duplicate of itself, which is the oracle
    /// mistake DECISIONS.md #25 and #30 were written to prevent.
    fn seeded(label: &str, stage: Option<Boundary>) -> (Self, Child) {
        // One directory per label, and every label in this file is distinct, so parallel
        // test threads cannot collide. A killed process leaves its directory behind by
        // design, hence the remove-before-create.
        let root = std::env::temp_dir().join(format!("wp03-save-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let scenario = Self {
            old: Vec::new(),
            new: Vec::new(),
            paths: ProjectPaths::from_root(&root),
            root,
        };
        let child = scenario.spawn(stage);
        (scenario, child)
    }

    fn spawn(&self, stage: Option<Boundary>) -> Child {
        let exe = env!("CARGO_BIN_EXE_wp03_save");
        let mut child = Command::new(exe)
            .arg("--dir")
            .arg(&self.root)
            .arg("--stage")
            .arg(stage.map_or("none", Boundary::as_str))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {exe}: {error}"));

        let deadline = Instant::now() + STAGE_TIMEOUT;
        loop {
            let staged = self.root.join("fixtures").join("new.bin").exists()
                && (stage.is_none() || self.root.join("stage.marker").exists());
            if staged {
                return child;
            }
            if let Some(status) = child.try_wait().expect("try_wait") {
                panic!(
                    "the child exited with {status} before reaching {:?}; its stderr is the \
                     diagnosis",
                    stage.map(Boundary::as_str)
                );
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the child never reached {:?}", stage.map(Boundary::as_str));
            }
            std::thread::sleep(POLL);
        }
    }

    fn load_fixtures(&mut self) {
        self.old = std::fs::read(self.root.join("fixtures").join("old.bin")).unwrap();
        self.new = std::fs::read(self.root.join("fixtures").join("new.bin")).unwrap();
        assert!(!self.old.is_empty() && !self.new.is_empty());
        assert_ne!(self.old, self.new, "the two documents must differ");
    }

    /// The child is killed only while it is provably still parked. A child that ran out of
    /// its own patience would return from the observer with an injected error, and the
    /// guard would clean up — an in-process fault, which the other suite already covers and
    /// this one must not mistake for a kill.
    fn kill_while_parked(child: &mut Child, stage: Boundary) {
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!(
                "the child at {stage} exited with {status} before the kill, so this is no \
                 longer a killed-process scenario (child park limit is {CHILD_PARK_LIMIT:?})"
            );
        }
        // Terminate rather than signal: Windows has no polite kill, and the whole point is
        // that nothing in the process gets to clean up after itself.
        child
            .kill()
            .unwrap_or_else(|error| panic!("{stage}: kill: {error}"));
        let status = child.wait().expect("reap the killed child");
        assert!(
            !status.success(),
            "{stage}: a killed child reported success: {status}"
        );
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn temporaries(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().into_owned();
            name.ends_with(".tmp").then_some(name)
        })
        .collect();
    out.sort();
    out
}

/// Every clause 12.12 owns about a save, evaluated on whatever is in the directory.
///
/// A verdict rather than a set of assertions, so the same code can be pointed at a
/// deliberately corrupted file by the negative controls at the end of this file. A checker
/// that has only ever been shown inputs it likes has not been tested, which is the same
/// argument DECISIONS.md #19 and #20 make about the gate and the hook — and every rule here
/// has a control that makes it fire, because a rule nothing can violate is not a rule.
///
/// The "is it a whole document, and is it canonical" checks are deliberately *not* repeated
/// here: the membership rule below subsumes them (both accepted byte streams are canonical
/// documents by construction, and `persistence.rs` proves that for the serializer), and a
/// check that cannot be reached is a check that cannot be tested.
fn classify(scenario: &Scenario, stage: Boundary) -> Result<(), String> {
    let on_disk = read(&scenario.paths.document());

    // Spec 9.7's requirement, and the reason ADR-0006 rejected `ReplaceFileW`: the
    // destination path is never merely absent.
    if on_disk.is_empty() {
        return Err(format!("{stage}: the destination is missing or empty"));
    }
    if on_disk != scenario.old && on_disk != scenario.new {
        return Err(format!(
            "{stage}: the destination holds {} bytes that are neither the previous document \
             ({} bytes) nor the new one ({} bytes)",
            on_disk.len(),
            scenario.old.len(),
            scenario.new.len(),
        ));
    }

    // A backup is a recovery input with defined content: either there is not one yet, or it
    // is exactly the previous document. Half a backup is the same defect in a file a
    // recovery scan will read.
    let backup = read(&scenario.paths.backup());
    if !backup.is_empty() && backup != scenario.old {
        return Err(format!(
            "{stage}: the backup is neither absent nor the previous document"
        ));
    }

    // The residue a killed process is allowed to leave. `Drop` never ran, so a staged
    // temporary may survive; what must not survive is the idea that one is a project.
    // Recovery reads the three named inputs and nothing else, and the next save names a
    // fresh temporary rather than adopting the abandoned bytes.
    let strays = temporaries(&scenario.root);
    if let Some(stray) = strays.iter().find(|name| !name.starts_with('.')) {
        return Err(format!(
            "{stage}: a temporary escaped the dotted-name convention: {stray}"
        ));
    }
    let candidates = anim_project::recovery::inspect(&scenario.paths);
    if candidates.len() != 3 {
        return Err(format!(
            "{stage}: recovery did not read exactly three inputs"
        ));
    }
    let project_id = match &candidates[0].state {
        anim_project::recovery::CandidateState::Complete(complete) => complete.document.project_id,
        _ => {
            return Err(format!(
                "{stage}: the destination is not a complete candidate"
            ))
        }
    };
    let decision = anim_project::recovery::select(&candidates, project_id);
    if !matches!(
        decision,
        anim_project::recovery::RecoveryDecision::UseCurrent
    ) {
        return Err(format!(
            "{stage}: with strays {strays:?}, recovery did not settle on the current file: {decision:?}"
        ));
    }
    Ok(())
}

/// Which strays each stage must leave.
///
/// The boundaries after the temporary exists and before its rename are the two a `Drop`
/// would have cleaned up, so a surviving temporary there is positive evidence that the
/// process really was terminated mid-sequence. Without this check, a kill that landed after
/// the rename would still satisfy `classify`, and the suite would be reporting a completed
/// save as an interrupted one.
fn expect_residue(scenario: &Scenario, stage: Boundary) {
    let strays = temporaries(&scenario.root);
    match stage {
        Boundary::AfterWrite | Boundary::BeforeReplacement => assert_eq!(
            strays.len(),
            1,
            "{stage} parks after the temporary exists, so the kill must leave it behind; \
             finding none means the child finished the rename and this is not a \
             killed-process scenario"
        ),
        Boundary::Serialization | Boundary::BeforeStaging => assert_eq!(
            strays,
            Vec::<String>::new(),
            "{stage} precedes the temporary, so nothing exists yet to abandon"
        ),
        Boundary::AfterReplacement => assert_eq!(
            strays,
            Vec::<String>::new(),
            "the rename consumed the temporary"
        ),
    }
}

#[test]
fn a_process_killed_at_each_save_stage_leaves_old_or_new_bytes_never_a_blend() {
    // 12.12 in full: "old-or-new file contents after injected write/flush/replacement
    // failures" and "kill test processes at each save stage and verify current, backup and
    // autosave recovery behavior".
    for stage in Boundary::ALL {
        let (mut scenario, mut child) =
            Scenario::seeded(&stage.as_str().replace('-', "_"), Some(stage));
        Scenario::kill_while_parked(&mut child, stage);
        // The fixtures were loaded while the child was parked; reload in case the child's
        // final write raced the kill.
        scenario.load_fixtures();

        if let Err(verdict) = classify(&scenario, stage) {
            panic!("{verdict}");
        }
        expect_residue(&scenario, stage);

        // The project remains usable in the strongest sense 12.12 names: the same save
        // completes afterwards over whatever the corpse left, including a stray temporary.
        let mut resumed = scenario.spawn(None);
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let status = loop {
            match resumed.try_wait().expect("try_wait") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = resumed.kill();
                    let _ = resumed.wait();
                    panic!("{stage}: a save could not be completed after the kill");
                }
                None => std::thread::sleep(POLL),
            }
        };
        assert!(
            status.success(),
            "{stage}: the resumed save failed: {status}"
        );
        assert_eq!(
            read(&scenario.paths.document()),
            scenario.new,
            "{stage}: the resumed save did not publish the new document"
        );
    }
}

#[test]
fn a_save_that_completes_in_a_child_process_leaves_no_temporary_behind() {
    // The control for the whole file: if an unstaged run also left stray temporaries, the
    // killed runs would be proving nothing about being killed.
    let (mut scenario, mut child) = Scenario::seeded("clean", None);
    let status = child.wait().expect("the clean save finishes");
    assert!(status.success(), "the unstaged child failed: {status}");
    scenario.load_fixtures();
    assert_eq!(
        read(&scenario.paths.document()),
        scenario.new,
        "the child's save did not publish the new document"
    );
    assert_eq!(
        temporaries(&scenario.root),
        Vec::<String>::new(),
        "a completed save left a temporary behind"
    );
    assert_eq!(
        read(&scenario.paths.backup()),
        scenario.old,
        "and wrote the backup from the previous bytes"
    );
    assert_eq!(
        read(&scenario.paths.autosave()),
        scenario.old,
        "the seeded autosave is untouched by a manual save"
    );
}

#[test]
fn the_destination_is_a_readable_document_while_the_save_is_parked() {
    // Spec 9.7's promise stated as the observable one: at every stage there is a file at
    // the authoritative path that a reader can load, *during* the save rather than only
    // after it. This is the property that makes an interrupted save survivable rather than
    // merely recoverable, and it is exactly what ADR-0006 says `ReplaceFileW` cannot
    // guarantee.
    for stage in Boundary::ALL {
        let (mut scenario, mut child) = Scenario::seeded(
            &format!("readable_{}", stage.as_str().replace('-', "_")),
            Some(stage),
        );
        scenario.load_fixtures();
        // Read the destination while the child is still inside the sequence.
        let mid = read(&scenario.paths.document());
        assert!(
            !mid.is_empty(),
            "{stage}: nothing at the authoritative path during the save"
        );
        anim_project::schema::deserialize_canonical(&mid).unwrap_or_else(|error| {
            panic!("{stage}: the destination was unreadable mid-save: {error}")
        });
        Scenario::kill_while_parked(&mut child, stage);
    }
}

#[test]
fn the_classifier_rejects_every_corruption_it_claims_to_catch() {
    // The negative controls. Without these, "old or new, never mixed" is a statement about
    // a checker that has only ever been handed files it liked. Each corruption below is the
    // artifact a specific failure mode would leave, and each must produce a refusal naming
    // the rule it broke.
    let (mut scenario, mut child) = Scenario::seeded("corruptions", Some(Boundary::Serialization));
    Scenario::kill_while_parked(&mut child, Boundary::Serialization);
    scenario.load_fixtures();
    assert!(
        classify(&scenario, Boundary::Serialization).is_ok(),
        "the untouched scenario has to pass before the corrupted ones mean anything"
    );

    // A blend: the new document with the old one appended. This is what a replacement that
    // wrote into the destination in place would leave behind.
    let mut blended = scenario.new.clone();
    blended.extend_from_slice(&scenario.old);
    std::fs::write(scenario.paths.document(), &blended).unwrap();
    let verdict = classify(&scenario, Boundary::Serialization)
        .expect_err("a blended destination must be rejected, or the suite proves nothing");
    assert!(
        verdict.contains("neither the previous document"),
        "and it must name the rule it broke: {verdict}"
    );

    // Truncation, which is what a power loss during a non-atomic write produces.
    std::fs::write(
        scenario.paths.document(),
        &scenario.new[..scenario.new.len() / 2],
    )
    .unwrap();
    assert!(classify(&scenario, Boundary::Serialization)
        .unwrap_err()
        .contains("neither the previous document"));

    // An absent destination, the failure ADR-0006 says `ReplaceFileW` can cause.
    std::fs::write(scenario.paths.document(), b"").unwrap();
    assert!(classify(&scenario, Boundary::Serialization)
        .unwrap_err()
        .contains("missing or empty"));

    // A complete, valid document in a non-canonical spelling: same values, outer members
    // re-ordered by a `serde_json::Value` round trip. It loads, and it is still a second
    // byte stream for one state, so it is a second digest.
    let value: Value = serde_json::from_slice(&scenario.new).unwrap();
    let reordered = serde_json::json!({
        "scenes": value["scenes"],
        "assets": value["assets"],
        "frame_rate": value["frame_rate"],
        "revision": value["revision"],
        "project_id": value["project_id"],
        "schema_version": value["schema_version"],
    });
    let reordered_bytes = serde_json::to_vec(&reordered).unwrap();
    assert_ne!(reordered_bytes, scenario.new);
    std::fs::write(scenario.paths.document(), &reordered_bytes).unwrap();
    assert!(classify(&scenario, Boundary::Serialization)
        .unwrap_err()
        .contains("neither the previous document"));

    // A stray temporary named like an object rather than like a temporary.
    std::fs::write(scenario.paths.document(), &scenario.new).unwrap();
    assert!(classify(&scenario, Boundary::Serialization).is_ok());
    std::fs::write(scenario.root.join("leaked.tmp"), b"not a project").unwrap();
    assert!(classify(&scenario, Boundary::Serialization)
        .unwrap_err()
        .contains("dotted-name convention"));
    std::fs::remove_file(scenario.root.join("leaked.tmp")).unwrap();

    // An autosave ahead of the manual save. 12.7 refuses to resolve that silently, so the
    // classifier must refuse it too -- and this is the control that proves the recovery
    // rule inside `classify` can actually fire rather than always agreeing.
    let mut ahead: anim_project::ProjectDocument =
        anim_project::schema::deserialize_canonical(&scenario.new).unwrap();
    ahead.revision = anim_core::id::Revision(ahead.revision.0 + 1);
    std::fs::write(
        scenario.paths.autosave(),
        anim_project::schema::serialize_canonical(&ahead).unwrap(),
    )
    .unwrap();
    let verdict = classify(&scenario, Boundary::Serialization)
        .expect_err("a newer autosave must not be silently ignored");
    assert!(verdict.contains("recovery did not settle"), "{verdict}");
}

#[test]
fn a_half_written_backup_is_rejected() {
    // The backup clause needs its own control: a truncated `.bak` is a recovery input that
    // lies about the previous state, which is worse than having no backup at all.
    //
    // Staged after the replacement, because that is the first boundary at which a backup
    // exists: `anim_core` writes it from the destination's previous bytes immediately
    // before the rename, so a child parked at `before-replacement` has produced no backup
    // for anything to corrupt.
    let (mut scenario, mut child) =
        Scenario::seeded("backup-control", Some(Boundary::AfterReplacement));
    Scenario::kill_while_parked(&mut child, Boundary::AfterReplacement);
    scenario.load_fixtures();
    assert!(
        scenario.paths.backup().exists(),
        "the backup is written before the rename, so a child parked after it must have one"
    );
    assert_eq!(read(&scenario.paths.document()), scenario.new);
    assert!(classify(&scenario, Boundary::AfterReplacement).is_ok());
    let intact = std::fs::read(scenario.paths.backup()).unwrap();
    std::fs::write(scenario.paths.backup(), &intact[..intact.len() / 2]).unwrap();
    let verdict = classify(&scenario, Boundary::AfterReplacement)
        .expect_err("a truncated backup must be refused");
    assert!(verdict.contains("backup"), "{verdict}");
}
