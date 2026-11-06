//! The WP05 two-process fixture (spec 14.12).
//!
//! G4 asks for 10,000 frames in two fresh processes with all CPU state hashes compared. The
//! thing that makes that comparison mean anything is that both processes are running the same
//! *authored project*, and the ordinary way for that to go wrong is for each process to build
//! its own fixture and for the two builders to drift — at which point the comparison proves
//! only that two different documents are each internally consistent.
//!
//! So the fixture is defined once, here, and used by the parent test
//! (`tests/wp05_isolation.rs`) and by the child process (`src/bin/wp05_replay.rs`) alike. The
//! child does not receive the document over a pipe or a file, which would make the
//! serialization the thing under test rather than a step on the way; it rebuilds it from this
//! module, which is the same compiled code the parent linked, in a process that shares no
//! memory with the first.
//!
//! `BUILD_STATE.md` risk 23 is the reason this file exists at all: what a fresh process proves
//! is the absence of process-static state, and nothing inside one process can show that.
//!
//! ## Why the rates are what they are
//!
//! 30000/1001 with four ticks per frame — the NTSC case — because an integer frame rate hides
//! every denominator bug at once. `WP01`'s own suite found a 1001× error in this quantity by
//! hand-computing the expectation (DECISIONS.md #32), and a fixture at 24/1 would not have
//! been able to show it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "this is a fixture, and every call below is a literal the type accepts. If one               ever failed, the scenario's report would be an unreadable error string from a               builder that produced nothing, and a panic that says which literal is wrong is               the diagnosis. The workspace denial still governs every production crate, and               none of this code is in one -- the same exemption `wp04_fixture.rs` carries."
)]

use anim_core::id::{EventDefinitionId, NodeId, ProjectId, SceneId, TrackId};
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::property::{Easing, PropertyCurve, PropertyKey, PropertyPath, PropertyValue};
use anim_project::{
    EventDefinition, EventKind, NodeData, ProjectDocument, SceneData, SimulationSettings, TrackData,
};

/// The project identity every fixture here derives from.
pub const PROJECT_LABEL: &[u8] = b"wp05-isolation-fixture";

/// The single scene, so a report says which one it is about.
pub const SCENE_LABEL: &[u8] = b"wp05-shot";

/// Ticks per frame: four, so an event at a half frame is between two ticks and the subframe
/// age arithmetic is on the ordinary path rather than only in its own test.
pub const TICKS_PER_FRAME: u32 = 4;

/// Frames in the gate run. 14.12's number.
pub const GATE_FRAMES: i64 = 10_000;

/// The seed byte every stream in the fixture is keyed by.
pub const SEED_BYTE: u8 = 0x5c;

fn namespace() -> uuid::Uuid {
    // The RFC 4122 DNS namespace, which is the one every fixture in this workspace derives
    // through (`anim_project::ID_NAMESPACE`, `anim_core::id`'s tests, `anim_assets`'s store).
    // A fixture-specific namespace would make the ids in a report unmatchable against any
    // other artifact this project produces.
    uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
}

/// One node identity.
#[must_use]
pub fn node(label: &str) -> NodeId {
    NodeId::derive(&namespace(), label.as_bytes())
}

/// The scene identity.
#[must_use]
pub fn scene_id() -> SceneId {
    SceneId::derive(&namespace(), SCENE_LABEL)
}

/// The document: a four-deep hierarchy, one animated track per non-root node, and nine
/// authored events placed on whole frames, half frames and one shared instant.
///
/// # Panics
///
/// Never, for this fixed input: every construction below is validated by the crate that owns
/// it and the fixture is committed, not generated, so a panic here is a defect in another
/// package rather than a state a caller can reach.
#[must_use]
pub fn document() -> ProjectDocument {
    let mut built = ProjectDocument::new(
        ProjectId::derive(&namespace(), PROJECT_LABEL),
        FrameRate::new(30000, 1001).unwrap(),
    );
    built.simulation = SimulationSettings::new(
        TICKS_PER_FRAME,
        [SEED_BYTE; anim_project::PROJECT_SEED_BYTES],
    )
    .unwrap_or_else(|error| panic!("the fixture's own settings must be legal: {error}"));
    built.revision = anim_core::id::Revision(23);

    let mut scene = SceneData::new("wp05-shot").unwrap_or_else(|error| panic!("{error}"));
    let chain = ["root", "arm", "hand", "tip"];
    let mut previous: Option<NodeId> = None;
    for (order, label) in chain.iter().enumerate() {
        let id = node(label);
        let data = NodeData::new(
            id,
            previous,
            u32::try_from(order).unwrap_or(u32::MAX),
            *label,
        );
        scene
            .insert_node(data)
            .unwrap_or_else(|_| panic!("{} is inserted once", *label));
        previous = Some(id);
    }
    // Authored motion on every non-root node, over 64 frames, so the world is different at
    // nearly every tick and a stale snapshot cannot pass as a correct one.
    for (label, path, magnitude) in [
        ("arm", "transform.x", 2i64),
        ("hand", "transform.x", 3),
        ("tip", "transform.x", 4),
        ("root", "transform.y", 1),
    ] {
        let curve = PropertyCurve::new(
            [
                PropertyKey::new(
                    FramePos::at_frame(0),
                    PropertyValue::Rational(Rational::from_i128(0)),
                    Easing::Linear,
                ),
                PropertyKey::new(
                    FramePos::at_frame(64),
                    PropertyValue::Rational(Rational::from_i128(magnitude as i128)),
                    Easing::Linear,
                ),
            ],
            None,
        )
        .unwrap_or_else(|error| panic!("curve for {label}: {error}"));
        scene
            .insert_track(TrackData::new(
                TrackId::derive(&namespace(), format!("t{label}").as_bytes()),
                node(label),
                PropertyPath::new(path).unwrap_or_else(|error| panic!("{path}: {error}")),
                curve,
            ))
            .unwrap_or_else(|_| panic!("track for {label}"));
    }
    // Beats: whole frames, half frames, two on the same instant on different nodes, and one
    // pair sharing an instant on the same node, which is the ordering case 13.7 exists for.
    for (label, source, frame, half, sequence) in [
        ("b0", "arm", 0i64, 0i64, 0u32),
        ("b1", "hand", 1, 1, 0),
        ("b2", "tip", 2, 0, 0),
        ("b3", "arm", 2, 1, 0),
        ("b4", "root", 5, 0, 0),
        ("b5", "hand", 5, 0, 1),
        ("b6", "tip", 9, 1, 0),
        ("b7", "arm", 13, 0, 0),
        ("b8", "arm", 13, 0, 1),
    ] {
        let position = FramePos::new(frame, Rational::new(i128::from(half), 2).unwrap())
            .unwrap_or_else(|error| panic!("position for {label}: {error}"));
        scene
            .insert_event(EventDefinition::new(
                EventDefinitionId::derive(&namespace(), label.as_bytes()),
                position,
                0,
                node(source),
                EventKind::Signal,
                sequence,
            ))
            .unwrap_or_else(|_| panic!("event {label}"));
    }
    // One cue and one cut, so the presentation ledger has something a warm-up can suppress and
    // an export must record.
    for (label, kind, frame, source) in [
        ("cue", EventKind::AudioCue, 4i64, "hand"),
        ("cut", EventKind::CameraCut, 7, "tip"),
    ] {
        scene
            .insert_event(EventDefinition::new(
                EventDefinitionId::derive(&namespace(), label.as_bytes()),
                FramePos::at_frame(frame),
                1,
                node(source),
                kind,
                0,
            ))
            .unwrap_or_else(|_| panic!("event {label}"));
    }
    assert!(
        built.insert_scene(scene_id(), scene).is_none(),
        "the fixture registers one scene under an id it has never used"
    );
    built
}

/// The last tick the gate run reaches, from [`GATE_FRAMES`] and [`TICKS_PER_FRAME`].
#[must_use]
pub const fn gate_ticks() -> i64 {
    GATE_FRAMES * TICKS_PER_FRAME as i64
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    #[test]
    fn the_fixture_is_valid_to_its_own_crates_rules() {
        let built = document();
        built
            .validate()
            .expect("a fixture that will not load is not a fixture");
        assert_eq!(
            built.schema_version,
            anim_project::document::PROJECT_SCHEMA_VERSION
        );
        assert_eq!(built.simulation.ticks_per_frame, TICKS_PER_FRAME);
        assert_eq!(built.simulation.project_seed, [SEED_BYTE; 32]);
    }

    #[test]
    fn the_fixture_is_the_same_document_every_time() {
        // Not a tautology: this is the property that lets the child process rebuild the
        // fixture instead of receiving it, and every id in it is a v5 derivation with no
        // entropy behind it (DECISIONS.md #23).
        let a = document();
        let b = document();
        assert_eq!(a, b);
        assert_eq!(
            anim_project::schema::serialize_canonical(&a).unwrap(),
            anim_project::schema::serialize_canonical(&b).unwrap(),
            "two builds of the fixture are one byte stream"
        );
    }

    #[test]
    fn the_fixture_has_the_shape_the_gate_needs() {
        let built = document();
        let scene = built.scene(&scene_id()).expect("one scene");
        assert_eq!(scene.nodes().count(), 4, "root, arm, hand, tip");
        assert_eq!(scene.tracks().count(), 4);
        assert_eq!(
            scene.events().count(),
            11,
            "nine signals plus a cue and a cut"
        );
        assert_eq!(gate_ticks(), 40_000);
        // And the subframe events are really subframe, or the age arithmetic is never reached.
        let halves: Vec<_> = scene
            .events()
            .filter(|event| !event.position.subframe().is_zero())
            .collect();
        assert_eq!(halves.len(), 3, "b1, b3 and b6 are the half-frame beats");
    }

    #[test]
    fn two_beats_share_an_instant_on_one_node() {
        // 13.7's `local_sequence` is the field that orders them, and the fixture must contain
        // the case or the ordering path in the simulation is untested.
        let built = document();
        let scene = built.scene(&scene_id()).unwrap();
        let at_13: Vec<_> = scene
            .events()
            .filter(|event| event.position == FramePos::at_frame(13) && event.source == node("arm"))
            .collect();
        assert_eq!(at_13.len(), 2, "b7 and b8");
        let mut sequences: Vec<u32> = at_13.iter().map(|event| event.local_sequence).collect();
        sequences.sort_unstable();
        assert_eq!(
            sequences,
            vec![0, 1],
            "and they are distinguished by sequence, not by luck"
        );
    }
}
