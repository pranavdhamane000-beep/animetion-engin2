//! The evaluated-snapshot scenario that runs in its own process (spec 13.12, G3).
//!
//! G3's criterion is not "the evaluation is deterministic in one process". It is
//! "identical canonical snapshots and event logs from **two fresh processes** for the same
//! document revision". A same-process check cannot answer that: inside one process the
//! binary's code, its loaded constants, its lazy statics and its allocator state are all
//! shared by construction, so agreement proves only that the code is the code. What the
//! clause actually rules out is anything that leaks into an evaluation from process state —
//! an address-ordered iteration, a table whose initialisation order depends on first touch,
//! a hash pre-image that picked up a pointer, an id namespace parsed differently on the
//! second call.
//!
//! So this is the process. It reads a `project.anim.json`, evaluates a fixed list of
//! positions, and prints one line of digest per position plus the ordered event log under
//! each. The parent is `crates/anim_test_support/tests/wp04_isolation.rs`, which starts it
//! twice and compares, and which also evaluates the same file in its own process to show
//! the two are not merely alike but equal to the library's own answer. Nothing here is a
//! shell pipeline (spec 9.4): the parent spawns this executable by path with an argument
//! array, the way `probe_tools` spawns FFmpeg and `wp03_save` is spawned for spec 12.12.
//!
//! It loads the document rather than building one in memory for the same reason
//! `wp03_save` writes real files. A process that reconstructs the authored state from the
//! saved form exercises `migrate::load_document`, the private-field `Deserialize` and the
//! canonical encoder on the way in — which is the path an operator's project actually
//! takes, and the one where a second spelling of one document (ADR-0005 Consequence 7)
//! would show up as two different snapshots.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anim_core::id::SceneId;
use anim_project::{schema, ProjectDocument};
use anim_scene::{snapshot_hash, SceneEvaluator};
use anim_test_support::wp04_fixture;

/// The exit code for a project file this scenario could not read or evaluate.
const FAILURE: u8 = 1;
/// The exit code for a command line this scenario was not given.
const USAGE: u8 = 2;

fn main() -> std::process::ExitCode {
    let path = match parse_args(std::env::args().skip(1)) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("wp04_snapshot: {message}");
            return std::process::ExitCode::from(USAGE);
        }
    };
    match report(&path) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("wp04_snapshot: {message}");
            std::process::ExitCode::from(FAILURE)
        }
    }
}

/// `--project <path>`, and nothing else.
fn parse_args(mut args: impl Iterator<Item = String>) -> std::result::Result<PathBuf, String> {
    let flag = args.next().ok_or("expected --project <path>")?;
    if flag != "--project" {
        return Err(format!(
            "unrecognised argument {flag:?}; expected --project <path>"
        ));
    }
    let path = args.next().ok_or("--project needs a path")?;
    if args.next().is_some() {
        return Err("too many arguments".to_string());
    }
    Ok(PathBuf::from(path))
}

/// Print the scenario's answer for one project file.
fn report(path: &Path) -> std::result::Result<(), String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let document =
        schema::deserialize_canonical(&bytes).map_err(|e| format!("load failed: {e}"))?;
    if document.revision.0 != 6 {
        // The parent writes the fixture itself, so a revision that is not the fixture's
        // means the file on disk is not the file the comparison claims to be about.
        return Err(format!(
            "the project on disk is at revision {}, which is not the fixture's 6",
            document.revision.0
        ));
    }
    let scene_id = only_scene(&document)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Named first, so a mismatch report can say what the two runs agreed on.
    writeln!(out, "project {}", document.project_id).map_err(|e| e.to_string())?;
    writeln!(out, "revision {}", document.revision.0).map_err(|e| e.to_string())?;

    for entry in wp04_fixture::POSITIONS {
        let position = wp04_fixture::position(&entry);
        let snapshot = SceneEvaluator::evaluate(&document, scene_id, position)
            .map_err(|e| format!("evaluation failed at {position:?}: {e}"))?;
        let hash = snapshot_hash(&snapshot).map_err(|e| format!("hash failed: {e}"))?;
        writeln!(
            out,
            "snapshot {} {} {hash}",
            position.frame,
            position.subframe()
        )
        .map_err(|e| e.to_string())?;
        // The event log spelled out rather than digested: G3's "identical event logs" is
        // about this sequence, and a digest of it would hide which of 13.7's five terms
        // moved between two runs.
        writeln!(out, "events {}", snapshot.events.len()).map_err(|e| e.to_string())?;
        for event in snapshot.events.iter() {
            writeln!(
                out,
                "event {} {} {} {} {} {}",
                event.occurrence_id,
                event.time.frame,
                event.time.subframe(),
                event.priority,
                event.source_id,
                event.local_sequence,
            )
            .map_err(|e| e.to_string())?;
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// The scene to evaluate, refusing anything but exactly one.
///
/// Refused rather than defaulted to the first, because a fixture with two scenes would
/// make the comparison depend on which key `BTreeMap` yields first — the class of thing
/// this scenario exists to detect, so it must not be the thing the scenario silently does.
fn only_scene(document: &ProjectDocument) -> std::result::Result<SceneId, String> {
    let mut ids = document.scenes.keys().copied();
    let first = ids.next().ok_or("the fixture project has no scenes")?;
    if ids.next().is_some() {
        return Err("the fixture project must have exactly one scene".to_string());
    }
    if first != wp04_fixture::scene_id() {
        return Err(format!(
            "the scene on disk is {first}, not the fixture's {}",
            wp04_fixture::scene_id()
        ));
    }
    Ok(first)
}
