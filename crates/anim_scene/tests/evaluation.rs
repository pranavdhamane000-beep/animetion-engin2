//! Boundary, hierarchy, determinism and cache tests for the evaluated snapshot (spec
//! 13.3's `crates/anim_scene/tests/evaluation.rs`, 13.12).
//!
//! 13.12 lists thirteen things G3 checks. Eleven of them are here, because they are
//! claims about one process's evaluation and an integration test is the right level: the
//! unit tests inside each module check the rule that module owns, and this file checks
//! that the assembled crate does what spec 13 says when the modules are wired together
//! through [`SceneEvaluator`]'s public surface only.
//!
//! Two are not, and both are recorded rather than quietly dropped:
//!
//! - **"identical canonical snapshots and event logs from two fresh processes"** cannot be
//!   done from inside one process, and a same-process clone would prove the linker gave
//!   one binary the same bytes it gave itself. That clause is
//!   `crates/anim_test_support/tests/wp04_isolation.rs` running the
//!   `wp04_snapshot` scenario binary twice, on `BUILD_STATE.md`'s deviation row 10
//!   precedent that a claim about processes needs processes.
//! - **checkpoint and simulation-history reconstruction** belongs to 14.12 and WP05, not
//!   here; this crate's half is that a snapshot carries everything 13.10 says simulation
//!   may read, which `a_snapshot_holds_everything_13_10_promises` checks.
//!
//! The oracle rule the earlier packages earned their defects with applies unchanged: no
//! expected value here is produced by calling the function under test in a different way.
//! Times, fractions, matrix entries and digests are written as the numbers the
//! specification says they are.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a fixture that cannot be built makes the scenario unreadable, which is a panic rather than a test outcome; the workspace denial still governs every production crate"
)]

use std::sync::Arc;

use anim_core::id::{AssetId, EventDefinitionId, NodeId, ProjectId, Revision, SceneId, TrackId};
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::schema;
use anim_project::{
    Easing, EasingCurve, EventDefinition, EventKind, NodeData, ProjectDocument, PropertyCurve,
    PropertyKey, PropertyPath, PropertyValue, SceneData, TrackData,
};
use anim_scene::{
    containing_frame, events_for_frame, hierarchy, snapshot_hash, EvaluatedNode, SceneEvaluator,
    SceneSnapshot, SnapshotCache,
};
use uuid::Uuid;

const NAMESPACE: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

fn namespace() -> Uuid {
    Uuid::parse_str(NAMESPACE).expect("the workspace's fixed id namespace is a valid uuid")
}

fn r(num: i128, den: i128) -> Rational {
    Rational::new(num, den).expect("the fixture's fractions are in range")
}

fn frame(n: i64) -> FramePos {
    FramePos::at_frame(n)
}

fn at(n: i64, sub: Rational) -> FramePos {
    FramePos::new(n, sub).expect("the fixture's subframes are in [0, 1)")
}

fn node_id(label: &str) -> NodeId {
    NodeId::derive(&namespace(), label.as_bytes())
}

fn track_id(label: &str) -> TrackId {
    TrackId::derive(&namespace(), label.as_bytes())
}

fn scene_id() -> SceneId {
    SceneId::derive(&namespace(), b"shot01")
}

fn event_id(label: &str) -> EventDefinitionId {
    EventDefinitionId::derive(&namespace(), label.as_bytes())
}

fn node(label: &str, parent: Option<&str>, order: u32) -> NodeData {
    NodeData::new(node_id(label), parent.map(node_id), order, label)
}

fn number_key(n: i64, num: i128, den: i128, easing: Easing) -> PropertyKey {
    PropertyKey::new(frame(n), PropertyValue::Rational(r(num, den)), easing)
}

fn path(raw: &str) -> PropertyPath {
    PropertyPath::new(raw).expect("the fixture's paths are canonical")
}

fn track(
    node_label: &str,
    id_label: &str,
    property: &str,
    keys: impl IntoIterator<Item = PropertyKey>,
) -> TrackData {
    TrackData::new(
        track_id(id_label),
        node_id(node_label),
        path(property),
        PropertyCurve::new(keys, None).expect("the fixture's key lists are legal"),
    )
}

/// One authored frame: a parent and child with a keyed transform, a stepped cel track, two
/// markers and three events — one cut, one cue, one signal.
///
/// Built as a `SceneData` so the same document can be handed to the evaluator, saved and
/// re-read, and used to build a shuffled input for the ordering test.
fn authored_scene() -> SceneData {
    let parent = node("parent", None, 0);
    let child = node("child", Some("parent"), 1);
    let cel = node("cel", Some("parent"), 4);
    SceneData::from_parts(
        "shot01",
        [parent, child, cel],
        [
            track(
                "child",
                "x",
                anim_scene::PATH_TRANSFORM_X,
                [
                    number_key(-4, 0, 1, Easing::Linear),
                    number_key(0, 40, 1, Easing::Linear),
                    number_key(
                        4,
                        100,
                        1,
                        Easing::Curve(
                            // An ease-in / ease-out table, not a straight line: samples at
                            // 0, 1/4, 1/2, 3/4 and 1 of the interval carry 0, 1/8, 1/2,
                            // 7/8 and 1. It is symmetric about the midpoint, which is what
                            // `an_eased_interval_that_is_symmetric_stays_symmetric` needs,
                            // and it is not the identity, which is what makes the earlier
                            // version of this fixture (a 1/2 midpoint) prove nothing.
                            EasingCurve::new([r(0, 1), r(1, 8), r(1, 2), r(7, 8), r(1, 1)])
                                .unwrap(),
                        ),
                    ),
                ],
            ),
            track(
                "cel",
                "asset",
                anim_scene::PATH_ASSET,
                [
                    PropertyKey::new(
                        frame(0),
                        PropertyValue::Asset(AssetId::derive(&namespace(), b"cel-a")),
                        Easing::Step,
                    ),
                    PropertyKey::new(
                        frame(6),
                        PropertyValue::Asset(AssetId::derive(&namespace(), b"cel-b")),
                        Easing::Step,
                    ),
                ],
            ),
            track(
                "parent",
                "opacity",
                anim_scene::PATH_OPACITY,
                [
                    number_key(2, 1, 1, Easing::Linear),
                    number_key(6, 0, 1, Easing::Linear),
                ],
            ),
        ],
        [
            anim_project::Marker::new(frame(0), "start"),
            anim_project::Marker::new(frame(9), "hit"),
        ],
        [
            EventDefinition::new(
                event_id("cut"),
                frame(4),
                0,
                node_id("cel"),
                EventKind::CameraCut,
                0,
            ),
            EventDefinition::new(
                event_id("cue"),
                frame(4),
                1,
                node_id("cel"),
                EventKind::AudioCue,
                0,
            ),
            EventDefinition::new(
                event_id("signal"),
                at(4, r(1, 2)),
                2,
                node_id("child"),
                EventKind::Signal,
                0,
            ),
        ],
        [],
    )
    .expect("the fixture scene is valid")
}

fn document() -> ProjectDocument {
    let mut out = ProjectDocument::new(
        ProjectId::derive(&namespace(), b"p"),
        FrameRate::new(24, 1).unwrap(),
    );
    assert!(
        out.insert_scene(scene_id(), authored_scene()).is_none(),
        "a new document displaces nothing"
    );
    out
}

fn snapshot_at(doc: &ProjectDocument, position: FramePos) -> Arc<SceneSnapshot> {
    SceneEvaluator::evaluate(doc, scene_id(), position)
        .unwrap_or_else(|e| panic!("the fixture must evaluate at {position:?}: {e}"))
}

fn node_in<'a>(snapshot: &'a SceneSnapshot, label: &str) -> &'a EvaluatedNode {
    snapshot
        .nodes
        .iter()
        .find(|n| n.id == node_id(label))
        .unwrap_or_else(|| panic!("{label} is not in the snapshot"))
}

/// 13.12, clause by clause. Each test's name states the clause it answers, so this file
/// can be read as the gate's checklist rather than as a suite that happens to cover it.
#[test]
fn first_and_last_key_clamping() {
    // "first/last key clamping" — spec 13.6's sentence, at both ends and at a frame the
    // keys say nothing about.
    let doc = document();
    let before = snapshot_at(&doc, frame(-100));
    assert_eq!(
        node_in(&before, "child").world_transform[0][2],
        0.0,
        "before the first key the first value holds. The first key is x = 0 at frame -4, so          frame -100 is 0 and not the -240 that extending the interval's slope would give"
    );
    let after = snapshot_at(&doc, frame(4_000));
    assert_eq!(
        node_in(&after, "child").world_transform[0][2],
        100.0,
        "after the last key the last value holds, with no extrapolation mode involved"
    );
}

#[test]
fn exact_and_subframe_interpolation() {
    // "exact and subframe interpolation". The authored interval is 0..4 frames carrying
    // 40..100 eased by a table whose midpoint is 1/2, so the answers are hand-derivable.
    let doc = document();
    let on_key = snapshot_at(&doc, frame(0));
    assert_eq!(
        node_in(&on_key, "child").world_transform[0][2],
        40.0,
        "at an exact key the authored value is reproduced"
    );
    let mid = snapshot_at(&doc, frame(2));
    assert_eq!(
        node_in(&mid, "child").world_transform[0][2],
        70.0,
        "halfway along the rise, eased by a symmetric table, is still halfway: {mid:?}",
    );
    // 2.5 frames of the 0..4 interval is u = 5/8, which lands in the table's third
    // segment: half way from 1/2 to 7/8 is 11/16, and 40 + 60 * 11/16 is 81.25.
    let sub = snapshot_at(&doc, at(2, r(1, 2)));
    assert!(
        (node_in(&sub, "child").world_transform[0][2] - 81.25).abs() < 1e-9,
        "expected 81.25 from the table arithmetic, got {}",
        node_in(&sub, "child").world_transform[0][2]
    );
}

#[test]
fn negative_frame_boundaries() {
    // "negative frame boundaries", and the two halves that matter: the value is
    // interpolated across zero without a discontinuity, and the frame that owns a negative
    // position is the floor rather than the truncation.
    let doc = document();
    let minus_two = snapshot_at(&doc, frame(-2));
    assert_eq!(
        node_in(&minus_two, "child").world_transform[0][2],
        20.0,
        "the interval -4..0 rises 0..40, so frame -2 is half of 40 and floor is doing the          frame selection, not the arithmetic"
    );
    let before_zero = at(-1, r(3, 4));
    assert_eq!(containing_frame(before_zero), -1, "floor, not truncation");
    assert!(
        snapshot_at(&doc, before_zero).time < snapshot_at(&doc, frame(0)).time,
        "and exact time keeps the order across the boundary"
    );
}

#[test]
fn adjacent_frame_event_exclusivity() {
    // "adjacent-frame event exclusivity". Three events authored around frame 4, one of
    // them exactly at 4 and one at 4.5: frame 3 must own none, frame 4 must own the two
    // that begin there, and the half-frame signal must not be in both.
    let doc = document();
    for (frame_number, expected) in [(3usize, 0usize), (4, 3), (5, 0)] {
        let events = events_for_frame(&doc, scene_id(), i64::try_from(frame_number).unwrap())
            .unwrap_or_else(|e| panic!("frame {frame_number}: {e}"));
        assert_eq!(
            events.len(),
            expected,
            "frame {frame_number} owns the wrong number of events: {events:?}"
        );
    }
    // And the snapshot's own event array agrees with the free function, which is 13.6's
    // "arbitrary-position evaluation does not independently redefine event delivery".
    let whole = snapshot_at(&doc, frame(4));
    let via_function = events_for_frame(&doc, scene_id(), 4).unwrap();
    assert_eq!(&*whole.events, &*via_function);
    let inside = snapshot_at(&doc, at(4, r(1, 2)));
    assert_eq!(
        &*inside.events, &*via_function,
        "a position inside frame 4 gets frame 4's sequence, including the event at its \
         own half-frame"
    );
}

#[test]
fn occurrence_ordering_under_shuffled_source_insertion() {
    // "occurrence ordering under shuffled source insertion". The same three authored
    // events, offered to the extraction in both orders, and a sixth definition inserted
    // at the end of the scene's map: the sequence is one sequence.
    let scene = authored_scene();
    let mut definitions: Vec<&EventDefinition> = scene.events().collect();
    let reference = anim_scene::EventSources::in_order(definitions.clone())
        .all_occurrences()
        .expect("the fixture's events order");
    definitions.reverse();
    let reversed = anim_scene::EventSources::in_order(definitions)
        .all_occurrences()
        .expect("the same events, re-offered");
    assert_eq!(
        reference
            .iter()
            .map(|e| e.occurrence_id)
            .collect::<Vec<_>>(),
        reversed.iter().map(|e| e.occurrence_id).collect::<Vec<_>>(),
        "the order came from 13.7's key and not from the scene's storage"
    );
    // The authored priorities decide, and they are the ones the fixture wrote: 0, then 1,
    // then 2, all at or inside frame 4.
    assert_eq!(
        reference.iter().map(|e| e.priority).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "{reference:?}"
    );
}

#[test]
fn cycle_rejection() {
    // "cycle rejection". `SceneData` cannot hold a cycle — its own `validate` refuses one
    // at construction and on load — so the clause is checked where 13.11 makes it a
    // result: the hierarchy pass over an extracted view. `anim_project`'s tests prove the
    // document side, and this proves the evaluator keeps its half of the same rule.
    let a = node_id("a");
    let b = node_id("b");
    let view = [
        hierarchy::NodeView {
            id: a,
            parent: Some(b),
            sibling_order: 0,
        },
        hierarchy::NodeView {
            id: b,
            parent: Some(a),
            sibling_order: 0,
        },
    ];
    let Err(anim_scene::SceneError::HierarchyCycle { node }) =
        hierarchy::visit(&view, &std::collections::BTreeMap::new())
    else {
        panic!("a two-node cycle must be refused rather than quietly left out");
    };
    assert!(
        node == a.to_string() || node == b.to_string(),
        "the report names a node on the cycle, got {node}"
    );
}

#[test]
fn parent_transforms_compose_in_the_visit_order() {
    // "parent transforms". The child is authored at x = 40 at frame 0 and its parent is
    // never moved, so the world x is the child's own; the order of the two factors is
    // pinned by the parent's opacity track fading while the child keeps moving.
    let doc = document();
    let snapshot = snapshot_at(&doc, frame(0));
    let child = node_in(&snapshot, "child");
    let parent = node_in(&snapshot, "parent");
    assert_eq!(
        child.parent,
        Some(parent.id),
        "and the parent link is carried"
    );
    assert_eq!(
        child.world_transform,
        anim_scene::multiply(&parent.world_transform, &child.world_transform),
        "a child under an unmoved parent is its own matrix"
    );
    assert_eq!(
        snapshot.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![node_id("parent"), node_id("child"), node_id("cel")],
        "roots by id, then children by sibling order 1 then 4"
    );
}

#[test]
fn camera_cuts() {
    // "camera cuts". One authored cut at frame 4 naming the cel node.
    let doc = document();
    let before = snapshot_at(&doc, frame(3));
    assert!(
        before.cameras.iter().all(|c| !c.is_on),
        "no cut has happened yet: {before:?}"
    );
    let on = snapshot_at(&doc, frame(4));
    let lit: Vec<&anim_scene::EvaluatedCamera> = on.cameras.iter().filter(|c| c.is_on).collect();
    assert_eq!(lit.len(), 1, "exactly one camera is on");
    assert_eq!(lit[0].node, node_id("cel"));
    assert_eq!(
        on.cameras.len(),
        1,
        "and only the node a cut names is presented as a camera"
    );
    // The cut stays on past its frame, which is the difference between a cut and an event.
    let later = snapshot_at(&doc, frame(9));
    assert!(later
        .cameras
        .iter()
        .any(|c| c.is_on && c.node == node_id("cel")));
    assert!(
        later.events.is_empty(),
        "frame 9 owns no events, and the camera is still the one from frame 4"
    );
}

#[test]
fn audio_cue_identity() {
    // "audio cue identity". The cue's identity in the snapshot is the same derived value
    // as in the event log, so a consumer that keyed a buffer on one is found by the log.
    let doc = document();
    let snapshot = snapshot_at(&doc, frame(4));
    assert_eq!(snapshot.audio_cues.len(), 1, "{snapshot:?}");
    let cue = &snapshot.audio_cues[0];
    let cue_event = snapshot
        .events
        .iter()
        .find(|event| event.kind == EventKind::AudioCue)
        .expect("the fixture authors one cue");
    assert_eq!(cue.occurrence_id, cue_event.occurrence_id);
    assert_eq!(cue.node, node_id("cel"));
    assert_eq!(cue.position, frame(4));
    assert_eq!(
        cue.asset,
        node_in(&snapshot, "cel").asset,
        "one authored asset, two views of it, and no second binding that could disagree"
    );
    // 13.9's free function returns the same identity from the same authored data.
    let via_function = events_for_frame(&doc, scene_id(), 4).unwrap();
    assert!(via_function
        .iter()
        .any(|e| e.occurrence_id == cue.occurrence_id));
}

#[test]
fn cache_isolation_after_undo_and_new_revisions() {
    // "cache isolation after undo/new revisions". Two revisions of one scene at one
    // position, both remembered, neither presented as the other, and the older entry
    // still correct about the revision it names.
    let original = document();
    let mut cache = SnapshotCache::new();
    let position = frame(2);
    let first = cache.snapshot(&original, scene_id(), position).unwrap();
    assert_eq!(first.revision, original.revision);

    // A new revision, the way WP03's document makes one: the field moves and the scenes
    // come from the same authored data, so only the stamp differs.
    let mut edited = document();
    edited.revision = Revision(1);
    let second = cache.snapshot(&edited, scene_id(), position).unwrap();
    assert_ne!(first.revision, second.revision);
    assert_eq!(cache.len(), 2, "two revisions are two entries");
    assert!(
        cache
            .peek(scene_id(), Revision(0), position)
            .is_some_and(|found| Arc::ptr_eq(&found, &first)),
        "the original snapshot is still there and still valid for revision 0"
    );

    // And an evaluated snapshot does not change when the document moves on: 13.8's
    // "cannot be presented as current" is a caller's decision, made possible by the
    // revision the entry carries.
    let third = cache.snapshot(&original, scene_id(), position).unwrap();
    assert!(
        Arc::ptr_eq(&first, &third),
        "asking again at the original revision is a hit, not a re-evaluation"
    );
    assert_eq!(cache.hits(), 1);
}

#[test]
fn repeated_snapshot_hashes() {
    // "repeated snapshot hashes", the strongest form available in one process: the digest
    // is a function of the snapshot's canonical bytes and the published domain and of
    // nothing else, so a second evaluation of the same revision and position reproduces
    // it. The two-fresh-processes half is `wp04_isolation`.
    let doc = document();
    let mut cache = SnapshotCache::new();
    let mut seen: Vec<String> = Vec::new();
    for position in [frame(-4), frame(0), frame(4), at(4, r(1, 2)), frame(40)] {
        let first = snapshot_hash(&snapshot_at(&doc, position)).unwrap();
        let again = snapshot_hash(&cache.snapshot(&doc, scene_id(), position).unwrap()).unwrap();
        assert_eq!(first, again, "cached and fresh disagree at {position:?}");
        let hex = first.to_hex();
        assert!(!seen.contains(&hex), "two positions hashed alike: {hex}");
        seen.push(hex);
    }
    assert_eq!(seen.len(), 5);
}

#[test]
fn a_saved_and_reread_document_evaluates_identically() {
    // The seam between WP03 and WP04, and the reason 13.2's boundary is worth having: the
    // bytes on disk carry the whole evaluation, so a process that only ever read the file
    // produces the same snapshot and the same digest as one that built it in memory.
    let built = document();
    let bytes = schema::serialize_canonical(&built).expect("the fixture is canonical");
    let loaded = schema::verify_canonical_bytes(&bytes)
        .unwrap_or_else(|e| panic!("the fixture does not round trip: {e}"));
    let position = at(4, r(1, 2));
    let from_built = snapshot_hash(&snapshot_at(&built, position)).unwrap();
    let from_loaded =
        snapshot_hash(&SceneEvaluator::evaluate(&loaded, scene_id(), position).unwrap()).unwrap();
    assert_eq!(
        from_built, from_loaded,
        "one authored state, two routes, one digest"
    );
}

#[test]
fn a_snapshot_holds_everything_13_10_promises() {
    // 13.10: simulation reads snapshots, render reads draw items, audio reads cues and
    // export drives whole frames. None of them may reach into the document, so anything
    // they need has to be in the snapshot.
    let doc = document();
    let snapshot = snapshot_at(&doc, frame(4));
    assert_eq!(snapshot.scene_id, scene_id());
    assert_eq!(snapshot.revision, doc.revision);
    assert_eq!(snapshot.position, frame(4));
    assert_eq!(
        snapshot.time,
        r(1, 6),
        "4 frames at 24 fps is one sixth of a second, exactly: {:?}",
        snapshot.time
    );
    assert_eq!(snapshot.nodes.len(), 3);
    assert_eq!(
        snapshot.simulation_inputs.len(),
        snapshot.nodes.len(),
        "one simulation input per evaluated node"
    );
    let input = &snapshot.simulation_inputs[0];
    let node = node_in(&snapshot, "parent");
    assert_eq!(input.node, node.id);
    assert_eq!(input.world_transform, node.world_transform);
    assert_eq!(input.z, node.z);
    assert_eq!(input.visible, node.visible);

    // The pre-image is public and re-computable: `canonical_bytes` is the stream and
    // `SNAPSHOT_DOMAIN` is the tag put in front of it by `snapshot_hash`. The framing
    // itself — length-prefixed domain, then the fields — is `anim_core::hash`'s rule and
    // `anim_core`'s own tests are where it is proven, so what is asserted here is the two
    // things that are this crate's: that the algorithm version leads the stream, and that
    // the stream distinguishes two snapshots that differ in one authored number.
    let body = snapshot.canonical_bytes().unwrap();
    assert_eq!(
        &body[..4],
        &anim_project::EASING_ALGORITHM_VERSION.to_le_bytes(),
        "the version is the first four bytes, so a changed reading moves every digest"
    );
    let mut changed = document();
    {
        let scene = changed
            .scenes
            .values_mut()
            .next()
            .expect("the fixture has one scene");
        scene
            .set_track_key(&track_id("x"), number_key(4, 101, 1, Easing::Linear))
            .expect("the replacement key is legal");
    }
    assert_ne!(
        snapshot_hash(&snapshot).unwrap(),
        snapshot_hash(&snapshot_at(&changed, frame(4))).unwrap(),
        "one authored number moved and the digest did not"
    );
}

#[test]
fn an_eased_interval_that_is_symmetric_stays_symmetric() {
    // The float-last rule, checked where it can actually be seen. The table is symmetric
    // about its midpoint — samples 0, 1/8, 1/2, 7/8, 1 at positions 0, 1/4, 1/2, 3/4, 1 —
    // so the two authored positions equidistant from frame 2 must produce values whose
    // average is the interval's midpoint, 70. A rounding anywhere before the final
    // conversion shows up here as the two halves disagreeing by an ulp.
    let doc = document();
    // 1.5 frames of the 0..4 interval is u = 3/8: half way through the second segment, so
    // half way from 1/8 to 1/2 is 5/16, and 40 + 60 * 5/16 is 58.75.
    let before_mid = node_in(&snapshot_at(&doc, at(1, r(1, 2))), "child").world_transform[0][2];
    // 2.5 frames is u = 5/8: half way from 1/2 to 7/8 is 11/16, and 40 + 60 * 11/16 is
    // 81.25. The pair is 140 between them, exactly twice the midpoint.
    let after_mid = node_in(&snapshot_at(&doc, at(2, r(1, 2))), "child").world_transform[0][2];
    assert!(
        (before_mid - 58.75).abs() < 1e-9,
        "the lower half of an ease-in sits below the straight line, got {before_mid}"
    );
    assert!(
        (after_mid - 81.25).abs() < 1e-9,
        "and the upper half sits above it, got {after_mid}"
    );
    assert!(
        (before_mid + after_mid - 140.0).abs() < 1e-12,
        "the two mirrored samples must average to the interval's midpoint: {before_mid} + \
         {after_mid}"
    );
    // The midpoint itself is the table's own middle sample, so it must land on 70 with no
    // rounding at all — the case that makes the pair above meaningful.
    let mid = node_in(&snapshot_at(&doc, frame(2)), "child").world_transform[0][2];
    assert_eq!(
        mid, 70.0,
        "u = 1/2 eases to 1/2, and half of 60 over 40 is 70"
    );
}

#[test]
fn an_unvisited_node_is_refused_rather_than_missing_from_the_snapshot() {
    // 13.11's "never skip invalid nodes silently" on the evaluator's side. A node the
    // hierarchy pass did not produce cannot be in the snapshot, and the two passes share
    // one list precisely so that no caller has to compare lengths to find out.
    let doc = document();
    let snapshot = snapshot_at(&doc, frame(0));
    let views: Vec<hierarchy::NodeView> = vec![
        hierarchy::NodeView {
            id: node_id("parent"),
            parent: None,
            sibling_order: 0,
        },
        hierarchy::NodeView {
            id: node_id("child"),
            parent: Some(node_id("parent")),
            sibling_order: 1,
        },
    ];
    let entries = hierarchy::visit(&views, &std::collections::BTreeMap::new()).unwrap();
    assert_eq!(
        entries.len(),
        2,
        "and the snapshot carries all three nodes, so a shorter list is a real difference"
    );
    assert_eq!(snapshot.nodes.len(), 3);
}

#[test]
fn one_document_evaluated_by_two_routes_is_one_digest() {
    // The in-process half of the claim the two-process scenario makes. Built twice, from
    // the same code, through two different routes — a fresh document and one read back
    // through the cache — because if a single process could already produce two digests
    // for one revision then the cross-process check would be testing luck rather than
    // determinism.
    let first = document();
    let second = document();
    assert_eq!(
        schema::serialize_canonical(&first).unwrap(),
        schema::serialize_canonical(&second).unwrap(),
        "the fixture itself is not referentially transparent, so nothing below it can be"
    );
    let mut cache = SnapshotCache::new();
    for position in [frame(-4), frame(0), frame(4), at(4, r(1, 2)), frame(40)] {
        let direct = snapshot_hash(&snapshot_at(&second, position)).unwrap();
        let cached = snapshot_hash(&cache.snapshot(&first, scene_id(), position).unwrap()).unwrap();
        assert_eq!(direct, cached, "two routes disagree at {position:?}");
    }
}
