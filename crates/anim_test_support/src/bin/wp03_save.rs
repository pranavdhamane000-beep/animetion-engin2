//! A save that stops at one persistence boundary and waits to be killed (spec 12.12).
//!
//! Spec 12.12 asks for the persistence scenarios to be proven by killing processes, not by
//! simulating them: "Kill test processes at each save stage and verify current, backup and
//! autosave recovery behavior." An in-process fault seam can prove what *this code* does
//! when a step returns an error; it cannot prove what the file system looks like when the
//! code stops running in the middle. Only a process that dies mid-save can answer that,
//! and the answer is a property of NTFS and the pinned sequence rather than of a mock.
//!
//! So this binary is the process that dies. The parent is
//! `crates/anim_test_support/tests/wp03_fault_injection.rs`, which starts it, waits for the
//! stage marker, and terminates it. Nothing here is a shell pipeline (spec 9.4): the parent
//! spawns this executable directly by path with an argument array, the way `probe_tools`
//! spawns FFmpeg.
//!
//! Two things it writes before the faulted save, so the parent never has to reproduce the
//! fixture: `fixtures/old.bin` and `fixtures/new.bin`, the canonical bytes of the two
//! documents. A parent that re-derived them with its own copy of the builder would be
//! comparing one implementation against a copy of itself.
#![allow(
    clippy::disallowed_methods,
    reason = "the bounded wait for the parent's kill is the scenario: this process parks \
              inside one save boundary until it is terminated, and clippy.toml permits \
              sleep in bounded wait loops of probes precisely for this"
)]
#![allow(
    clippy::disallowed_types,
    reason = "a wall-clock deadline on a wait loop is not deterministic state; it bounds a \
              probe that must not hang, and nothing here writes a duration into a document \
              or a hash"
)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use anim_assets::{AssetKind, AssetManifestEntry};
use anim_core::id::Revision;
use anim_core::id::{AssetId, NodeId, ProjectId, SceneId, TrackId};
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::property::{Easing, PropertyCurve, PropertyKey, PropertyPath, PropertyValue};
use anim_project::save::{self, Boundary, ProjectPaths};
use anim_project::scene::{Marker, NodeData, SceneData, TrackData};
use anim_project::{schema, ProjectDocument};

/// How long this process is willing to wait to be killed.
///
/// A deadline rather than an infinite park, so a parent that dies or forgets leaves a
/// nonzero exit rather than a hung build. The save is then completed and the parent's
/// assertion that the child never returned is what fails, which is the visible outcome.
const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

struct Args {
    root: PathBuf,
    stage: Option<Boundary>,
}

fn main() -> std::process::ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("wp03_save: {message}");
            return std::process::ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wp03_save: {error}");
            std::process::ExitCode::from(1)
        }
    }
}

fn parse_args(mut iter: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut root = None;
    let mut stage = None;
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--dir" => {
                root = Some(PathBuf::from(iter.next().ok_or("--dir needs a path")?));
            }
            "--stage" => {
                let name = iter.next().ok_or("--stage needs a boundary name")?;
                if name != "none" {
                    stage = Some(
                        Boundary::ALL
                            .into_iter()
                            .find(|boundary| boundary.as_str() == name)
                            .ok_or_else(|| format!("unknown stage {name:?}"))?,
                    );
                }
            }
            other => return Err(format!("unrecognized argument {other:?}")),
        }
    }
    Ok(Args {
        root: root.ok_or("--dir is required")?,
        stage,
    })
}

fn run(args: &Args) -> Result<(), String> {
    std::fs::create_dir_all(&args.root).map_err(|e| format!("create project root: {e}"))?;
    let paths = ProjectPaths::from_root(&args.root);
    let fixtures = args.root.join("fixtures");
    std::fs::create_dir_all(&fixtures).map_err(|e| format!("create fixtures: {e}"))?;

    let original = document(3)?;
    let replacement = document(4)?;
    // The two byte streams the parent compares against, written by the same builder that
    // produced them.
    std::fs::write(
        fixtures.join("old.bin"),
        schema::serialize_canonical(&original).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("write old fixture: {e}"))?;
    std::fs::write(
        fixtures.join("new.bin"),
        schema::serialize_canonical(&replacement).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("write new fixture: {e}"))?;

    // A destination to replace, and a backup: the interesting case is the second save of a
    // project that already has state, because a first save has no previous bytes to mix
    // with.
    save::save_atomic(&original, &paths.document()).map_err(|e| format!("seed save: {e}"))?;
    save::save_autosave(&original, &args.root).map_err(|e| format!("seed autosave: {e}"))?;

    let marker = args.root.join("stage.marker");
    let signal = args.root.join("kill.signal");
    let reached = args.stage;

    let report = save::save_atomic_observed(&replacement, &paths.document(), &mut |boundary| {
        if reached == Some(boundary) {
            std::fs::write(&marker, boundary.as_str()).map_err(|e| {
                anim_project::ProjectError::Persistence {
                    context: "write stage marker",
                    path: marker.display().to_string(),
                    detail: e.to_string(),
                }
            })?;
            wait_for_signal(&signal);
            // Reached only if the parent forgot to kill us. Reporting the fault makes
            // the run observable rather than silently successful.
            return Err(anim_project::ProjectError::Persistence {
                context: "waited for kill",
                path: boundary.as_str().to_string(),
                detail: format!("{boundary} parked for {WAIT_LIMIT:?} and was not killed"),
            });
        }
        Ok(())
    });

    match (args.stage, report) {
        // Killed before returning, which is the normal outcome of a stage run.
        (Some(_), _) => Ok(()),
        (None, Ok(_)) => Ok(()),
        (None, Err(error)) => Err(format!("unstageable save failed: {error}")),
    }
}

fn wait_for_signal(signal: &Path) {
    let deadline = Instant::now() + WAIT_LIMIT;
    while Instant::now() < deadline {
        if signal.exists() {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One document, parameterized only by revision, so `old.bin` and `new.bin` differ in
/// exactly the state a save is supposed to make atomic.
///
/// Every construction failure propagates as a message rather than a panic: this binary is
/// the process a parent kills, so an unwrapped fixture error would look to the parent like
/// a crash at an interesting moment and be indistinguishable from the scenario.
fn document(revision: u64) -> Result<ProjectDocument, String> {
    let namespace = namespace()?;
    let mut doc = ProjectDocument::new(project_id(&namespace), rate()?);
    doc.revision = Revision(revision);

    let cel = AssetId::derive(&namespace, b"cel-001");
    doc.assets.insert(
        AssetManifestEntry::new(
            cel,
            anim_assets::hash_bytes(revision.to_le_bytes().as_slice()),
            8,
            AssetKind::Image,
            1,
            [],
            "cel-001",
            claims(),
        )
        .map_err(|e| format!("asset record: {e}"))?,
    );

    let root_id = NodeId::derive(&namespace, b"root");
    let mut scene = SceneData::new("shot01").map_err(|e| format!("scene: {e}"))?;
    scene
        .insert_node(NodeData::new(root_id, None, 0, "root"))
        .map_err(|e| format!("root node: {e}"))?;
    scene
        .insert_node(NodeData::new(
            NodeId::derive(&namespace, b"cel"),
            Some(root_id),
            0,
            "cel",
        ))
        .map_err(|e| format!("cel node: {e}"))?;
    scene
        .insert_track(TrackData::new(
            TrackId::derive(&namespace, b"x"),
            root_id,
            PropertyPath::new("transform.x").map_err(|e| format!("property path: {e}"))?,
            PropertyCurve::new(
                [PropertyKey::new(
                    FramePos::at_frame(3),
                    PropertyValue::Rational(Rational::from_i128(i128::from(revision))),
                    Easing::Linear,
                )],
                None,
            )
            .map_err(|e| format!("curve: {e}"))?,
        ))
        .map_err(|e| format!("track: {e}"))?;
    scene
        .insert_marker(Marker::new(FramePos::at_frame(0), "start"))
        .map_err(|e| format!("marker: {e}"))?;
    assert!(
        doc.insert_scene(scene_id(&namespace), scene).is_none(),
        "a freshly built document displaces nothing"
    );
    Ok(doc)
}

fn project_id(namespace: &uuid::Uuid) -> ProjectId {
    ProjectId::derive(namespace, b"wp03-save")
}

/// The claims a hand-built `Image` record must carry to be legal at all (spec 15.2,
/// DECISIONS.md #40 ruling 2).
///
/// Fixed here rather than derived, and that choice matters for what this binary proves: the
/// kill scenarios compare a save's *bytes*, so a claims value that moved between the two
/// revisions would make `old.bin` and `new.bin` differ by something other than the revision.
/// One constant keeps the fault-injection assertions about the thing they are about.
fn claims() -> Option<anim_assets::ImageAssetClaims> {
    Some(anim_assets::ImageAssetClaims {
        color_space: anim_assets::ImageColorSpace::Srgb,
        alpha: anim_assets::ImageAlpha::Straight,
    })
}

fn scene_id(namespace: &uuid::Uuid) -> SceneId {
    SceneId::derive(namespace, b"shot01")
}

/// The RFC 4122 DNS namespace, the same one `anim_core::id`'s and `anim_assets`'s
/// fixtures derive through. Parsed rather than spelled as bytes so a reader can compare it
/// with the constant in `anim_project::ID_NAMESPACE`, and the failure is reported because
/// a fixture id that silently became `nil` would make two scenarios look identical.
fn namespace() -> Result<uuid::Uuid, String> {
    uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").map_err(|e| e.to_string())
}

fn rate() -> Result<FrameRate, String> {
    FrameRate::new(24000, 1001).map_err(|e| e.to_string())
}
