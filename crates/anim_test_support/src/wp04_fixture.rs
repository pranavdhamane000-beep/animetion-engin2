//! The authored state WP04's two-process scenario evaluates (spec 13.12, G3).
//!
//! In the library rather than in `src/bin/wp04_snapshot.rs` because both the process that
//! prints the digests and the process that compares them need *one* definition of the
//! fixture. Two copies would be two builders, and a scenario whose parent holds its own
//! version of the same code proves only that the code agrees with itself — the exact
//! failure DECISIONS.md #32 tells every test in this project to avoid, and the reason
//! `wp03_save` writes its own fixtures rather than having the parent re-derive them.
//!
//! What the parent therefore compares is one in-process evaluation against two separate
//! processes that each read the same `project.anim.json` from disk. That is G3's sentence:
//! identical canonical snapshots and event logs from two fresh processes for the same
//! document revision.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "this is a fixture, and every call below is a literal the type accepts: a               negative denominator, an out-of-range subframe, a duplicate key time. If one               of them ever failed, the scenario's report would be an unreadable error string               from a builder that produced nothing, and a panic that says which literal is               wrong is the diagnosis. The workspace denial still governs every production               crate, and none of this code is in one"
)]

use anim_core::id::{EventDefinitionId, NodeId, ProjectId, Revision, SceneId, TrackId};
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::{
    Easing, EasingCurve, EventDefinition, EventKind, Marker, NodeData, ProjectDocument,
    PropertyCurve, PropertyKey, PropertyPath, PropertyValue, SceneData, TrackData,
};

/// The id namespace every derivation in the fixture is anchored to.
///
/// The RFC 4122 DNS namespace, which is what `anim_core::id`'s tests,
/// `anim_assets/tests/store.rs` and `anim_project::ID_NAMESPACE` all use. It matters here
/// because the fixture's identities must be the same in every process, and there is no
/// entropy source anywhere in this workspace to make them differ (DECISIONS.md #23).
pub const NAMESPACE: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

/// The positions evaluated, in the order reported.
///
/// A fixed list rather than an argument, so the two runs cannot be handed different work:
/// the comparison is only meaningful if both processes answered the same questions. It is
/// deliberately not all whole frames — a subframe position and a negative one are the two
/// cases where an implementation can agree with itself and still be wrong, and 13.12 names
/// both.
pub const POSITIONS: [(i64, (i128, i128)); 6] = [
    (-40, (0, 1)),
    (-1, (3, 4)),
    (0, (0, 1)),
    (2, (1, 2)),
    (4, (1, 2)),
    (40, (0, 1)),
];

/// One position as the type the evaluator takes.
///
/// # Panics
///
/// Never for a pair from [`POSITIONS`]; the widths are literals chosen to be legal, and a
/// fixture that cannot be built is a panic rather than a test outcome.
#[must_use]
pub fn position(entry: &(i64, (i128, i128))) -> FramePos {
    let (frame, (num, den)) = *entry;
    FramePos::new(
        frame,
        Rational::new(num, den).expect("the fixture's subframes are legal"),
    )
    .expect("the fixture's subframes are in [0, 1)")
}

/// The authored state.
///
/// Three nodes, three tracks (one linear across a negative interval, one stepped cel
/// switch, one fade with an authored default), two markers and three events on two source
/// nodes. Every field the evaluator reads has a track behind it and every event kind
/// 13.5's step 5 names appears once, because the scenario's point is that the whole
/// evaluation is reproduced — not one field of it.
#[must_use]
pub fn fixture() -> ProjectDocument {
    let namespace = uuid::Uuid::parse_str(NAMESPACE).expect("the fixed namespace is a uuid");
    let node = |label: &str| NodeId::derive(&namespace, label.as_bytes());
    let rational = |num: i128, den: i128| Rational::new(num, den).expect("fixture fractions");
    let path = |raw: &str| PropertyPath::new(raw).expect("the fixture's paths are canonical");

    let parent = NodeData::new(node("parent"), None, 0, "parent");
    let parent_id = parent.node_id;
    let child = NodeData::new(node("child"), Some(parent_id), 1, "child");
    let cel = NodeData::new(node("cel"), Some(parent_id), 4, "cel");

    let x_track = TrackData::new(
        TrackId::derive(&namespace, b"x"),
        node("child"),
        path(anim_scene::PATH_TRANSFORM_X),
        PropertyCurve::new(
            [
                PropertyKey::new(
                    FramePos::at_frame(-4),
                    PropertyValue::Rational(rational(0, 1)),
                    Easing::Linear,
                ),
                PropertyKey::new(
                    FramePos::at_frame(0),
                    PropertyValue::Rational(rational(40, 1)),
                    Easing::Linear,
                ),
                PropertyKey::new(
                    FramePos::at_frame(4),
                    PropertyValue::Rational(rational(100, 1)),
                    // An ease-in / ease-out table: samples 0, 1/8, 1/2, 7/8, 1 at equal
                    // positions. Symmetric about the midpoint, which is what makes
                    // `wp04_isolation`'s hand-computed values a check on the arithmetic
                    // rather than on the fixture.
                    Easing::Curve(
                        EasingCurve::new([
                            rational(0, 1),
                            rational(1, 8),
                            rational(1, 2),
                            rational(7, 8),
                            rational(1, 1),
                        ])
                        .expect("the fixture's easing table is legal"),
                    ),
                ),
            ],
            None,
        )
        .expect("the fixture's transform keys are unique"),
    );

    let cel_track = TrackData::new(
        TrackId::derive(&namespace, b"cel-asset"),
        node("cel"),
        path(anim_scene::PATH_ASSET),
        PropertyCurve::new(
            [
                PropertyKey::new(
                    FramePos::at_frame(0),
                    PropertyValue::Asset(anim_core::id::AssetId::derive(&namespace, b"cel-a")),
                    Easing::Step,
                ),
                PropertyKey::new(
                    FramePos::at_frame(6),
                    PropertyValue::Asset(anim_core::id::AssetId::derive(&namespace, b"cel-b")),
                    Easing::Step,
                ),
            ],
            None,
        )
        .expect("the fixture's cel keys are unique"),
    );

    let opacity_track = TrackData::new(
        TrackId::derive(&namespace, b"opacity"),
        parent_id,
        path(anim_scene::PATH_OPACITY),
        PropertyCurve::new(
            [
                PropertyKey::new(
                    FramePos::at_frame(2),
                    PropertyValue::Rational(rational(1, 1)),
                    Easing::Linear,
                ),
                PropertyKey::new(
                    FramePos::at_frame(6),
                    PropertyValue::Rational(rational(0, 1)),
                    Easing::Linear,
                ),
            ],
            // An authored default as well as a key list, so the two halves of 13.6's
            // sentence are both in the fixture and both reach the digest.
            Some(PropertyValue::Rational(rational(1, 1))),
        )
        .expect("the fixture's fade keys are unique"),
    );

    let scene = SceneData::from_parts(
        "shot01",
        [parent, child, cel],
        [x_track, cel_track, opacity_track],
        [
            Marker::new(FramePos::at_frame(0), "start"),
            Marker::new(FramePos::at_frame(9), "hit"),
        ],
        [
            EventDefinition::new(
                EventDefinitionId::derive(&namespace, b"cut"),
                FramePos::at_frame(4),
                0,
                node("cel"),
                EventKind::CameraCut,
                0,
            ),
            EventDefinition::new(
                EventDefinitionId::derive(&namespace, b"cue"),
                FramePos::at_frame(4),
                1,
                node("cel"),
                EventKind::AudioCue,
                0,
            ),
            EventDefinition::new(
                EventDefinitionId::derive(&namespace, b"signal"),
                FramePos::new(4, rational(1, 2)).expect("a half frame is in range"),
                2,
                node("child"),
                EventKind::Signal,
                0,
            ),
        ],
        [],
    )
    .expect("the fixture scene is valid");

    let mut document = ProjectDocument::new(
        ProjectId::derive(&namespace, b"wp04-isolation"),
        FrameRate::new(24, 1).expect("24 fps is a legal rate"),
    );
    // A revision above zero, because revision 0 is the case a cache or a report could
    // confuse with "unset" and the scenario is comparing exactly those numbers.
    document.revision = Revision(6);
    assert!(
        document
            .insert_scene(SceneId::derive(&namespace, b"shot01"), scene)
            .is_none(),
        "a fresh document displaces nothing"
    );
    document
}

/// The one scene id in the fixture, which the scenario must name rather than discover.
///
/// `BTreeMap` iteration would give it, but a report whose subject depended on iteration
/// order is the thing this scenario is trying to detect, so it is derived from the same
/// label the builder used.
#[must_use]
pub fn scene_id() -> SceneId {
    SceneId::derive(
        &uuid::Uuid::parse_str(NAMESPACE).expect("the fixed namespace is a uuid"),
        b"shot01",
    )
}
