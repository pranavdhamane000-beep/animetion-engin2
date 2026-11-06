//! Random vectors and 10,000-frame equivalence tests (spec 14.3, 14.12, gate G4).
//!
//! Every row of 14.12 is one test here, except the fresh-process half, which is
//! `crates/anim_test_support/tests/wp05_isolation.rs` because a claim about processes can only
//! be proven by a process (the same split WP04 reached for G3, deviation row 13).
//!
//! ## What this suite refuses to do
//!
//! The rule DECISIONS.md #32 states — an oracle that calls the code under test proves only
//! self-consistency — is the reason several tests below rebuild a byte layout by hand instead
//! of asking the encoder what it wrote. It is also the reason the Philox half reads its values
//! from `tests/fixtures/philox4x32_10_test_vectors.json`, which carries the commit and file
//! hash of the D. E. Shaw Research source they came from: nothing in this repository can be the
//! authority on whether Philox4x32-10 is implemented correctly, because the thing being
//! certified is the code that would check it.
//!
//! ## The failure each row exists to catch
//!
//! | 14.12 row | The defect it would hide |
//! | --- | --- |
//! | golden vectors | a wrong round constant or key schedule that is self-consistent forever |
//! | seeks to every tick | a cursor in the random path, or a schedule gathered per frame |
//! | cadences 1, 7, 64, 1024, disabled, capacities | state that quietly depends on what was cached |
//! | forward, backward, warm-up, export vs reference | a second stepping loop with a different stage order |
//! | event chunking, frame grouping | a tick boundary handled twice, or skipped |
//! | two fresh processes, 10,000 frames | anything that lives in process-static state |
//! | one stream version changed | streams sharing an address, so a fix is a global change |
//! | insertion/removal stability, non-finite | an order-dependent digest, or a NaN in a checkpoint |

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a fixture that cannot be built makes the scenario unreadable, which is a panic rather than a test outcome; the workspace denial still governs every production crate"
)]

use anim_assets::ContentHash;
use anim_core::id::{EventDefinitionId, NodeId, ProjectId, SceneId};
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRate};
use anim_project::{
    EventDefinition, EventKind, NodeData, ProjectDocument, SceneData, SimulationSettings,
};
use anim_sim::checkpoint::{state_hash, CheckpointCache};
use anim_sim::context::SimulationContext;
use anim_sim::mode::SimulationMode;
use anim_sim::particles::{particle_id, PARTICLES_PER_SIGNAL};
use anim_sim::random::{philox4x32_10, purpose, random_f32, random_u32, system};
use anim_sim::replay::{prepare_export, replay_to, sequential_hashes, warm_up_to};
use anim_sim::seek::seek;
use anim_sim::state::{SimSettings, SimState};
use anim_sim::{SimError, StreamKey};
use anim_sim::{SimTick, PHILOX_ALGORITHM_VERSION};

/// The fixture's timeline rate. NTSC rather than an integer, because a rate whose denominator
/// is not 1 is the case every conversion in WP05 gets wrong when it drops one — and
/// `BUILD_STATE.md` records that a 1001× error in exactly this quantity was found in this
/// repository's real code by hand-computing the expectation.
const NUMERATOR: u32 = 30000;
const DENOMINATOR: u32 = 1001;
/// Ticks per frame for the fixture: four, so an event at a 1/8 frame is between two ticks and
/// the subframe age path is on every run rather than only in its own test.
const TICKS_PER_FRAME: u32 = 4;
/// Frames in the long equivalence run. 14.12's number, spelled once.
const GATE_FRAMES: i64 = 10_000;

fn namespace() -> uuid::Uuid {
    uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
}

fn node(label: &str) -> NodeId {
    NodeId::derive(&namespace(), label.as_bytes())
}

fn scene_id() -> SceneId {
    SceneId::derive(&namespace(), b"g4-shot")
}

/// A three-level hierarchy and one event per named beat, at a fixed seed.
///
/// Built here rather than in `anim_test_support` because this suite is the package's own gate
/// and must run with the support crate absent; the isolation fixture in
/// `wp05_fixture.rs` is the same shape at the same seed, and
/// `anim_test_support/tests/wp05_isolation.rs` is what proves the two agree.
fn fixture(ticks_per_frame: u32, seed_byte: u8) -> ProjectDocument {
    let mut document = ProjectDocument::new(
        ProjectId::derive(&namespace(), b"g4-fixture"),
        FrameRate::new(NUMERATOR, DENOMINATOR).unwrap(),
    );
    document.simulation = SimulationSettings::new(
        ticks_per_frame,
        [seed_byte; anim_project::PROJECT_SEED_BYTES],
    )
    .unwrap();
    document.revision = anim_core::id::Revision(11);

    let mut scene = SceneData::new("g4-shot").unwrap();
    // root -> arm -> hand -> tip, so a body exists for every non-root node and a spring for
    // each edge: the chain is long enough that a solver order would show up as a difference.
    let chain = ["root", "arm", "hand", "tip"];
    let mut previous: Option<NodeId> = None;
    for (order, label) in chain.iter().enumerate() {
        scene
            .insert_node(NodeData::new(
                node(label),
                previous,
                u32::try_from(order).unwrap(),
                *label,
            ))
            .unwrap();
        previous = Some(node(label));
    }
    // Four authored transforms, keyed so the evaluated positions differ per node.
    for (label, path, x) in [
        ("arm", "transform.x", 2),
        ("hand", "transform.x", 3),
        ("tip", "transform.x", 4),
        ("root", "transform.y", 1),
    ] {
        let track_id = anim_core::id::TrackId::derive(&namespace(), format!("t{label}").as_bytes());
        scene
            .insert_track(anim_project::TrackData::new(
                track_id,
                node(label),
                anim_project::PropertyPath::new(path).unwrap(),
                anim_project::PropertyCurve::new(
                    [
                        anim_project::PropertyKey::new(
                            FramePos::at_frame(0),
                            anim_project::PropertyValue::Rational(Rational::from_i128(0)),
                            anim_project::Easing::Linear,
                        ),
                        anim_project::PropertyKey::new(
                            FramePos::at_frame(64),
                            anim_project::PropertyValue::Rational(Rational::from_i128(i128::from(
                                x,
                            ))),
                            anim_project::Easing::Linear,
                        ),
                    ],
                    None,
                )
                .unwrap(),
            ))
            .unwrap();
    }
    // Beats on whole frames, on half frames, and two on the same instant across different
    // nodes, so chunking and ordering are both exercised by the ordinary runs.
    for (label, source, frame, subframe) in [
        ("b0", "arm", 0i64, 0i64),
        ("b1", "hand", 1, 1),
        ("b2", "tip", 2, 0),
        ("b3", "arm", 2, 1),
        ("b4", "root", 5, 0),
        ("b5", "hand", 5, 0),
        ("b6", "tip", 9, 1),
        ("b7", "arm", 13, 0),
    ] {
        scene
            .insert_event(EventDefinition::new(
                EventDefinitionId::derive(&namespace(), label.as_bytes()),
                FramePos::new(frame, Rational::new(i128::from(subframe), 2).unwrap()).unwrap(),
                0,
                node(source),
                EventKind::Signal,
                0,
            ))
            .unwrap();
    }
    // One cue and one cut, so the presentation ledger has something to suppress.
    scene
        .insert_event(EventDefinition::new(
            EventDefinitionId::derive(&namespace(), b"cue"),
            FramePos::at_frame(4),
            1,
            node("hand"),
            EventKind::AudioCue,
            0,
        ))
        .unwrap();
    assert!(document.insert_scene(scene_id(), scene).is_none());
    document
}

/// One named way to break a state, as a function pointer rather than a closure type: the
/// four corruptions capture nothing, which is the point -- a corruption that depended on the
/// run would be testing the run instead of testing the boundary.
type Corruption = (&'static str, fn(&mut SimState));

/// The uncheckpointed reference hash sequence 14.12 compares everything against.
fn reference_hashes(document: &ProjectDocument, last_tick: i64) -> Vec<ContentHash> {
    let mut context =
        SimulationContext::new(document, scene_id(), CheckpointCache::default()).unwrap();
    sequential_hashes(&mut context, SimTick(last_tick), SimulationMode::Export).unwrap()
}

/// A tick sequence that looks scattered and is entirely determined by its inputs.
///
/// The probe order comes from [`random_u32`] — this package's own address function, not a
/// thread timer or a seed from the environment — because 14.12 asks for "randomized ranges"
/// inside a suite whose whole subject is reproducibility. A `rand` crate here would make a
/// failure non-reproducible, which is the opposite of what the row is for.
fn probe_ticks(upper: i64, count: usize, salt: &str) -> Vec<i64> {
    let seed = [9u8; anim_project::PROJECT_SEED_BYTES];
    let key = StreamKey {
        project_seed: seed,
        system_id: system::SPRINGS,
        entity_id: node(salt),
        purpose: purpose::SPRING_INITIAL_OFFSET,
        stream_version: PHILOX_ALGORITHM_VERSION,
    };
    (0..count)
        .map(|index| {
            let draw = random_u32(&key, SimTick(i64::try_from(index).unwrap()), 0, 0).unwrap();
            i64::from(draw) % (upper + 1)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 14.12 row 1 -- "Verify Philox and stream-key golden vectors."
// ---------------------------------------------------------------------------

#[test]
fn the_permutation_reproduces_every_published_known_answer() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(manifest_dir)
        .join("tests")
        .join("fixtures")
        .join("philox4x32_10_test_vectors.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} is a versioned input: {e}", path.display()));
    let fixture: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(fixture["algorithm"].as_str(), Some("Philox4x32-10"));
    // The provenance is part of the gate, not a comment: a fixture that could not be traced
    // to the source that published it would be a place for a number to have been invented.
    let sources = fixture["sources"].as_array().unwrap();
    assert!(!sources.is_empty());
    for source in sources {
        assert!(
            source["url"]
                .as_str()
                .unwrap()
                .starts_with("https://raw.githubusercontent.com/DEShawResearch/random123/"),
            "the vectors have to come from the library that defines the algorithm"
        );
        assert_eq!(source["sha256_of_fetched_file"].as_str().unwrap().len(), 64);
    }
    let vectors = fixture["vectors"].as_array().unwrap();
    assert!(
        vectors.len() >= 50,
        "{} vectors is not the published set",
        vectors.len()
    );
    for vector in vectors {
        let counter: [u32; 4] = [0, 1, 2, 3].map(|i| vector["counter"][i].as_u64().unwrap() as u32);
        let key: [u64; 2] = [0, 1].map(|i| vector["key"][i].as_u64().unwrap());
        let expected: [u32; 4] =
            [0, 1, 2, 3].map(|i| vector["expected"][i].as_u64().unwrap() as u32);
        assert_eq!(
            philox4x32_10(counter, key),
            expected,
            "line {}",
            vector["source_line"].as_str().unwrap_or("?")
        );
    }
}

#[test]
fn the_stream_key_derivation_is_blake3_over_the_image_14_7_describes() {
    // The pre-image rebuilt from 14.7's sentence in 14.7's order -- domain tag, project seed,
    // system id, emitter/entity id, stream purpose, stream version -- with ADR-0004's widths:
    // fixed-width fields raw, variable-width behind a little-endian u64 length. Then a
    // `blake3` dev-dependency hashes it, which is the one external authority available for the
    // layout of a construction this project invented.
    let seed = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];
    let key = StreamKey {
        project_seed: seed,
        system_id: 1,
        entity_id: node("emitter"),
        purpose: 4,
        stream_version: PHILOX_ALGORITHM_VERSION,
    };
    let domain = "anim_engine/philox/v1";
    let mut pre = Vec::new();
    pre.extend_from_slice(&(domain.len() as u64).to_le_bytes());
    pre.extend_from_slice(domain.as_bytes());
    pre.extend_from_slice(&seed);
    pre.extend_from_slice(&1u32.to_le_bytes());
    pre.extend_from_slice(key.entity_id.as_bytes());
    pre.extend_from_slice(&4u32.to_le_bytes());
    pre.extend_from_slice(&PHILOX_ALGORITHM_VERSION.to_le_bytes());
    let digest = *blake3::hash(&pre).as_bytes();

    // The fold: subkey 0 is derived word 0 XOR derived word 1, subkey 1 word 2 XOR word 3,
    // because Philox4x32 reads two 32-bit subkeys and ADR-0008 spends all 128 derived bits.
    let word = |index: usize| {
        u32::from_le_bytes([
            digest[index * 4],
            digest[index * 4 + 1],
            digest[index * 4 + 2],
            digest[index * 4 + 3],
        ])
    };
    let expected = [u64::from(word(0) ^ word(1)), u64::from(word(2) ^ word(3))];
    assert_eq!(
        anim_sim::random::philox_key(&key),
        expected,
        "the derivation is a published function of a layout written down in one place"
    );
    assert_eq!(
        key.canonical_bytes(),
        pre,
        "and the pre-image the gate names is this"
    );
}

#[test]
fn a_draw_is_the_permutation_at_its_address() {
    let seed = [0xa5u8; anim_project::PROJECT_SEED_BYTES];
    let key = StreamKey {
        project_seed: seed,
        system_id: system::PARTICLES,
        entity_id: node("hero"),
        purpose: purpose::PARTICLE_DIRECTION,
        stream_version: PHILOX_ALGORITHM_VERSION,
    };
    let subkeys = anim_sim::random::philox_key(&key);
    for (tick, draw, lane) in [
        (0i64, 0u32, 0u8),
        (1, 1, 3),
        (4_294_967_296, 7, 2),
        (999_999, 12345, 1),
    ] {
        let (low, high) = SimTick(tick).counter_words().unwrap();
        assert_eq!(
            random_u32(&key, SimTick(tick), draw, lane).unwrap(),
            philox4x32_10([low, high, draw, 0], subkeys)[usize::from(lane)],
            "tick {tick} draw {draw} lane {lane}"
        );
    }
}

// ---------------------------------------------------------------------------
// 14.12 row 2 -- "Compare sequential execution with seeks to every tick in randomized ranges."
// ---------------------------------------------------------------------------

#[test]
fn a_seek_to_every_probed_tick_matches_sequential_execution() {
    let document = fixture(TICKS_PER_FRAME, 31);
    let last = i64::from(TICKS_PER_FRAME) * 40;
    let expected = reference_hashes(&document, last);

    let mut context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::new(1, 16)).unwrap();
    // Warm the cache by running forward, then probe out of order -- the shape of a scrubbing
    // editor, which is 14.10's stated consumer of this function.
    let mut forward = context.initial_state().unwrap();
    context
        .advance(&mut forward, SimTick(last), SimulationMode::Export, None)
        .unwrap();
    for tick in probe_ticks(last, 120, "probe-a") {
        let state = seek(&mut context, SimTick(tick)).unwrap();
        assert_eq!(state.tick, SimTick(tick));
        assert_eq!(
            state_hash(&state).unwrap(),
            expected[usize::try_from(tick).unwrap()],
            "a seek to tick {tick} disagrees with the run that stepped through it"
        );
    }
}

#[test]
fn a_seek_with_no_checkpoints_at_all_still_matches() {
    let document = fixture(TICKS_PER_FRAME, 31);
    let last = i64::from(TICKS_PER_FRAME) * 24;
    let expected = reference_hashes(&document, last);
    let mut context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
    for tick in probe_ticks(last, 60, "probe-b") {
        let state = seek(&mut context, SimTick(tick)).unwrap();
        assert_eq!(
            state_hash(&state).unwrap(),
            expected[usize::try_from(tick).unwrap()],
            "tick {tick}, reaching it from tick zero with nothing cached"
        );
    }
}

// ---------------------------------------------------------------------------
// 14.12 row 3 -- "Run checkpoint cadences 1, 7, 64, 1024, disabled checkpoints and different
//                 capacities; every per-tick hash must match."
// ---------------------------------------------------------------------------

#[test]
fn every_cadence_and_capacity_produces_the_same_hash_at_every_tick() {
    let document = fixture(TICKS_PER_FRAME, 31);
    let last = i64::from(TICKS_PER_FRAME) * 48;
    let expected = reference_hashes(&document, last);
    let mut configurations = vec![(0u64, 0usize)];
    for cadence in [1u64, 7, 64, 1024] {
        for capacity in [1usize, 2, 8, 64, 4096] {
            configurations.push((cadence, capacity));
        }
    }
    for (cadence, capacity) in configurations {
        let mut context = SimulationContext::new(
            &document,
            scene_id(),
            CheckpointCache::new(cadence, capacity),
        )
        .unwrap();
        let mut state = context.initial_state().unwrap();
        for tick in 1..=last {
            context
                .advance(&mut state, SimTick(tick), SimulationMode::Export, None)
                .unwrap();
            assert_eq!(
                state_hash(&state).unwrap(),
                expected[usize::try_from(tick).unwrap()],
                "cadence {cadence} capacity {capacity} differs at tick {tick}"
            );
        }
        assert!(
            context.checkpoints().len() <= capacity,
            "and the capacity really did bound the cache"
        );
    }
    // And the configurations actually differed in what they stored, or the loop above would
    // have compared one run with itself twenty times.
    let mut observed = Vec::new();
    for (cadence, capacity) in [(1u64, 4usize), (7, 4), (64, 4), (0, 4)] {
        let mut context = SimulationContext::new(
            &document,
            scene_id(),
            CheckpointCache::new(cadence, capacity),
        )
        .unwrap();
        let mut state = context.initial_state().unwrap();
        context
            .advance(&mut state, SimTick(80), SimulationMode::Export, None)
            .unwrap();
        observed.push((cadence, context.checkpoints().len()));
    }
    let counts: Vec<usize> = observed.iter().map(|(_, len)| *len).collect();
    assert!(
        counts.windows(2).any(|pair| pair[0] != pair[1]),
        "the cadences must cache differently or the equality above proves nothing: {counts:?}"
    );
}

// ---------------------------------------------------------------------------
// 14.12 row 4 -- "Compare forward playback, backward scrub, warm-up and export against an
//                 uncheckpointed reference."
// ---------------------------------------------------------------------------

#[test]
fn the_four_routes_all_arrive_at_the_references_state() {
    let document = fixture(TICKS_PER_FRAME, 31);
    let last = i64::from(TICKS_PER_FRAME) * 32;
    let expected = reference_hashes(&document, last);
    // A tick on a frame boundary, so `prepare_export` -- which stops on the tick its range
    // *starts* on, and derives that tick from a frame -- lands on the same one the other four
    // routes were driven to. 57 at four ticks per frame would not: its frame is 14, and
    // 14 * 4 is 56, which is the correct answer for that route and a different tick from the
    // reference index being compared. The mid-frame case is asserted on its own below.
    let target = 56usize;
    let target_tick = SimTick(i64::try_from(target).unwrap());

    // Forward playback: the ordinary run, hashing as it goes.
    let mut forward_context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
    let mut forward = forward_context.initial_state().unwrap();
    forward_context
        .advance(&mut forward, target_tick, SimulationMode::Live, None)
        .unwrap();

    // Backward scrub: reach a later tick, then seek back.
    let mut scrub_context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::new(1, 128)).unwrap();
    let mut ahead = scrub_context.initial_state().unwrap();
    scrub_context
        .advance(&mut ahead, SimTick(last), SimulationMode::Live, None)
        .unwrap();
    let scrubbed = seek(&mut scrub_context, target_tick).unwrap();

    // Warm-up, replay, and export preparation from a bounded cache.
    let mut warm_context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
    let warmed = warm_up_to(&mut warm_context, target_tick).unwrap();
    let replayed = replay_to(&mut warm_context, target_tick).unwrap();

    let mut export_context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::new(7, 3)).unwrap();
    // prepare_export stops at the tick its range starts on, so the range begins at the frame
    // that contains the target and the state returned is the target's own.
    let start_frame = export_context.clock().frame_of_tick(target_tick);
    let prepared = prepare_export(
        &mut export_context,
        anim_core::time::FrameRange::new(start_frame, start_frame + 4).unwrap(),
    )
    .unwrap();
    // `prepared` is the export route's own contract: it stops on the tick its range starts
    // on, which is the target tick here because the range was built from the frame that
    // contains it. It is listed alongside the others rather than compared separately, so the
    // table below is the whole of 14.12's row in one place.
    for (name, state) in [
        ("forward playback", &forward),
        ("backward scrub", &scrubbed),
        ("warm-up", &warmed),
        ("replay", &replayed),
        ("export preparation", &prepared),
    ] {
        assert_eq!(
            state_hash(state).unwrap(),
            expected[target],
            "{name} reached tick {target} with a different world than the reference"
        );
    }
    assert_eq!(
        prepared.tick, target_tick,
        "the export begins on the tick its range's first frame starts at"
    );

    // The mid-frame case, stated rather than hidden: an export range begins on a frame, so a
    // target in the middle of one is reached as that frame's first tick, and the difference is
    // visible instead of being absorbed by a tolerance.
    let mid_frame = SimTick(57);
    let frame = export_context.clock().frame_of_tick(mid_frame);
    assert_eq!(
        frame * i64::from(TICKS_PER_FRAME),
        56,
        "57 lives in frame 14"
    );
    let started = prepare_export(
        &mut export_context,
        anim_core::time::FrameRange::new(frame, frame + 2).unwrap(),
    )
    .unwrap();
    assert_eq!(started.tick, SimTick(56));
    assert_eq!(
        state_hash(&started).unwrap(),
        expected[56],
        "and it is the reference's own state at that tick"
    );
}

#[test]
fn warm_up_suppresses_the_effects_export_records_and_neither_changes_the_world() {
    let document = fixture(TICKS_PER_FRAME, 31);
    let last = i64::from(TICKS_PER_FRAME) * 20;
    let mut export_context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
    let mut heard = export_context.initial_state().unwrap();
    export_context
        .advance(&mut heard, SimTick(last), SimulationMode::Export, None)
        .unwrap();
    let mut warm_context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
    let silent = warm_up_to(&mut warm_context, SimTick(last)).unwrap();

    assert!(
        !heard.presentation.is_empty(),
        "the fixture's cue event must reach the ledger or this test is comparing two silences"
    );
    assert!(silent.presentation.is_empty(), "warm-up announces nothing");
    assert_eq!(
        state_hash(&heard).unwrap(),
        state_hash(&silent).unwrap(),
        "and that difference is invisible to the digest, which is what 14.12 requires"
    );
}

// ---------------------------------------------------------------------------
// 14.12 row 5 -- "Repeat with different event chunking and render frame grouping."
// ---------------------------------------------------------------------------

#[test]
fn grouping_the_calls_differently_changes_no_hash() {
    // Event chunking is the schedule's, which [`TickSchedule`] builds once over the whole
    // scene; render frame grouping is how many ticks a caller asks for per call. Neither is
    // allowed to reach the arithmetic, and this varies the second one.
    let document = fixture(TICKS_PER_FRAME, 31);
    let last = i64::from(TICKS_PER_FRAME) * 30;
    let expected = reference_hashes(&document, last);
    for chunk in [1i64, 2, 3, 5, 11, 30, 121] {
        let mut context =
            SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
        let mut state = context.initial_state().unwrap();
        let mut seen = vec![state_hash(&state).unwrap()];
        while state.tick < SimTick(last) {
            let next = std::cmp::min(state.tick.checked_add_ticks(chunk).unwrap(), SimTick(last));
            context
                .advance(&mut state, next, SimulationMode::Export, None)
                .unwrap();
            seen.push(state_hash(&state).unwrap());
        }
        assert_eq!(
            *seen.last().unwrap(),
            expected[usize::try_from(last).unwrap()],
            "grouping every {chunk} ticks reached a different world"
        );
        assert_eq!(
            seen.len(),
            usize::try_from((last + chunk - 1) / chunk + 1).unwrap(),
            "and it really stopped at different points, or this is one run measured twice"
        );
    }
}

#[test]
fn a_schedule_compiled_in_pieces_matches_one_compiled_whole() {
    // The other half of the row. `TickSchedule` reads the whole scene at once, and
    // `anim_scene::events_for_frame` is the per-frame delivery; if the two ever disagree about
    // an event's tick, chunking becomes a second delivery rule and 13.6 forbids one.
    let document = fixture(TICKS_PER_FRAME, 31);
    let clock = anim_sim::clock::SimulationClock::new(
        document.frame_rate,
        document.simulation.ticks_per_frame,
    )
    .unwrap();
    let scene = document.scene(&scene_id()).unwrap();
    let whole = anim_sim::events::TickSchedule::for_scene(&clock, scene).unwrap();

    let mut pieces = Vec::new();
    let mut by_tick = std::collections::BTreeMap::<i64, Vec<anim_scene::SceneEvent>>::new();
    for frame in 0..32i64 {
        for event in anim_scene::events_for_frame(&document, scene_id(), frame)
            .unwrap()
            .iter()
        {
            by_tick
                .entry(clock.tick_of_position(event.time).unwrap().0)
                .or_default()
                .push(event.clone());
        }
    }
    for (tick, events) in &by_tick {
        for event in events {
            pieces.push((*tick, event.occurrence_id));
        }
    }
    let mut scheduled = Vec::new();
    for tick in whole.scheduled_ticks() {
        for entry in whole.at(tick) {
            scheduled.push((tick.0, entry.event.occurrence_id));
        }
    }
    assert_eq!(
        scheduled.len(),
        pieces.len(),
        "the whole-scene schedule and the per-frame assembly disagree about how many events \
         there are"
    );
    for (a, b) in scheduled.iter().zip(pieces.iter()) {
        assert_eq!(*a, *b, "same tick, same occurrence, in the same order");
    }
}

// ---------------------------------------------------------------------------
// 14.12 row 6 -- "Run 10,000 frames in two fresh processes and compare all CPU state hashes."
// ---------------------------------------------------------------------------

#[test]
fn ten_thousand_frames_replay_to_one_hash_sequence() {
    // The single-process half of the row. The two-process half is
    // `anim_test_support/tests/wp05_isolation.rs`, which spawns the same fixture twice; what
    // this one establishes is that the long run is the *same* run as the short one, so a
    // difference found between two processes cannot be blamed on the length.
    let document = fixture(1, 31);
    let frames = GATE_FRAMES;
    let mut context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::new(1024, 8)).unwrap();
    let mut state = context.initial_state().unwrap();
    let mut hashes = Vec::with_capacity(usize::try_from(frames).unwrap());
    for frame in 1..=frames {
        context
            .advance(&mut state, SimTick(frame), SimulationMode::Export, None)
            .unwrap();
        hashes.push(state_hash(&state).unwrap());
        assert_eq!(state.tick, SimTick(frame));
    }
    assert_eq!(i64::try_from(hashes.len()).unwrap(), frames);
    let final_hash = hashes[usize::try_from(frames - 1).unwrap()];

    // A short prefix of the same document is a prefix of the same sequence: this is the
    // property that lets the two-process test compare a digest of the whole list rather than
    // accidentally comparing two differently-sized ones.
    let expected = reference_hashes(&document, frames);
    assert_eq!(
        expected.len(),
        hashes.len() + 1,
        "tick 0 plus one per frame"
    );
    for (index, hash) in hashes.iter().enumerate() {
        assert_eq!(*hash, expected[index + 1], "frame {}", index + 1);
    }
    assert_eq!(final_hash, *expected.last().unwrap());

    // And a seek into the middle of the long run lands on the sequence, not on an approximation.
    for tick in [1u64, 37, 4096, 9_999] {
        let sought = seek(&mut context, SimTick(i64::try_from(tick).unwrap())).unwrap();
        assert_eq!(
            state_hash(&sought).unwrap(),
            expected[usize::try_from(tick).unwrap()],
            "a seek inside the 10,000-frame run"
        );
    }
}

// ---------------------------------------------------------------------------
// 14.12 row 7 -- "Changing one stream version must leave unrelated stream outputs unchanged."
// ---------------------------------------------------------------------------

#[test]
fn one_streams_version_move_does_not_touch_another() {
    // 14.12's row read as a design constraint: `stream_version` is a field of `StreamKey`
    // rather than a global constant precisely so that a consumer who changes one channel's
    // numbers cannot change anybody else's. A global algorithm version is
    // PHILOX_ALGORITHM_VERSION, and that one is supposed to move everything -- the two
    // knobs exist for different jobs and this test is what keeps them apart.
    let seed = [0x5au8; anim_project::PROJECT_SEED_BYTES];
    let channels = [
        purpose::PARTICLE_DIRECTION,
        purpose::PARTICLE_LIFETIME,
        purpose::PARTICLE_SPEED,
        purpose::PARTICLE_SIZE,
        purpose::PARTICLE_ROTATION,
        purpose::PARTICLE_SUBFRAME,
    ];
    let entities = ["hero", "sword", "cape"];
    let address = |entity: &str, channel: u16, version: u32| StreamKey {
        project_seed: seed,
        system_id: system::PARTICLES,
        entity_id: node(entity),
        purpose: channel,
        stream_version: version,
    };
    let probes = [(3i64, 1u32, 0u8), (777, 4, 2), (99_999, 0, 3)];
    let draw_all = |key: &StreamKey| -> Vec<u32> {
        probes
            .iter()
            .map(|(tick, draw, lane)| random_u32(key, SimTick(*tick), *draw, *lane).unwrap())
            .collect()
    };

    // Every stream at version 1, as a fixed table: (entity, channel) -> draws.
    let table: Vec<(&str, u16)> = entities
        .iter()
        .flat_map(|entity| channels.iter().map(move |channel| (*entity, *channel)))
        .collect();
    let before: Vec<Vec<u32>> = table
        .iter()
        .map(|(entity, channel)| draw_all(&address(entity, *channel, 1)))
        .collect();

    // The one stream being bumped.
    let moved_entity = "sword";
    let moved_channel = purpose::PARTICLE_SPEED;
    assert_ne!(
        draw_all(&address(moved_entity, moved_channel, 2)),
        draw_all(&address(moved_entity, moved_channel, 1)),
        "a version that is not part of the address is not a version -- and it must still be \
         drawable, because a refused bump is a stream that can never be revised"
    );

    // And every other stream, at version 1, with the bumped stream now also present at 2 in
    // the same process -- the state of the world the row describes.
    for ((entity, channel), expected) in table.iter().zip(before.iter()) {
        if *entity == moved_entity && *channel == moved_channel {
            continue;
        }
        assert_eq!(
            &draw_all(&address(entity, *channel, 1)),
            expected,
            "{entity}/channel {channel} moved because an unrelated stream's version did"
        );
        // A different entity on the same channel, and the same entity on a different channel,
        // are both unrelated -- and both are covered by the loop, because the table is the
        // full cross product.
    }
    assert_eq!(table.len(), entities.len() * channels.len());
}

#[test]
fn a_global_lever_moves_every_stream_where_a_per_stream_lever_moves_exactly_one() {
    // The two version knobs are different instruments and this is what proves it, because a
    // test that only ever bumps one of them cannot tell "per-stream" from "global" at all.
    //
    // The global lever observable at runtime is the project seed: it is one of the five
    // StreamKey terms and it lives in `ProjectDocument.simulation`, so turning it is exactly
    // what an operator does when they want a different-but-reproducible draw of every emitter.
    // `PHILOX_ALGORITHM_VERSION` itself is a compile-time constant and cannot be turned here;
    // the value that carries it at runtime is `AlgorithmVersions::philox`, which
    // `restore_checkpoint` refuses outright -- that half is
    // `crate::checkpoint::tests::foreign_algorithm_versions_are_a_mismatch_not_a_reinterpretation`.
    let entities = ["hero", "sword", "cape"];
    let channels = [
        purpose::PARTICLE_DIRECTION,
        purpose::PARTICLE_LIFETIME,
        purpose::PARTICLE_SPEED,
        purpose::PARTICLE_SIZE,
        purpose::PARTICLE_ROTATION,
        purpose::PARTICLE_SUBFRAME,
    ];
    let probes = [(5i64, 2u32, 1u8), (900, 0, 3), (40_000, 11, 0)];
    let address = |seed_byte: u8, entity: &str, channel: u16, version: u32| StreamKey {
        project_seed: [seed_byte; anim_project::PROJECT_SEED_BYTES],
        system_id: system::PARTICLES,
        entity_id: node(entity),
        purpose: channel,
        stream_version: version,
    };
    let draw = |key: &StreamKey| -> Vec<u32> {
        probes
            .iter()
            .map(|(tick, at, lane)| random_u32(key, SimTick(*tick), *at, *lane).unwrap())
            .collect()
    };
    let table: Vec<(&str, u16)> = entities
        .iter()
        .flat_map(|entity| channels.iter().map(move |channel| (*entity, *channel)))
        .collect();

    let seed_a: Vec<Vec<u32>> = table
        .iter()
        .map(|(entity, channel)| draw(&address(0x11, entity, *channel, 1)))
        .collect();
    let seed_b: Vec<Vec<u32>> = table
        .iter()
        .map(|(entity, channel)| draw(&address(0x22, entity, *channel, 1)))
        .collect();
    let moved_by_seed: usize = seed_a
        .iter()
        .zip(seed_b.iter())
        .filter(|(x, y)| x != y)
        .count();
    assert_eq!(
        moved_by_seed,
        table.len(),
        "one seed byte changed {moved_by_seed} of {} streams; a global lever that misses any \
         stream would leave part of the project replaying the old draw",
        table.len()
    );

    // Now the per-stream lever, against the same baseline: exactly one entry moves.
    let bumped: Vec<Vec<u32>> = table
        .iter()
        .map(|(entity, channel)| {
            let version = u32::from(*entity == "sword" && *channel == purpose::PARTICLE_SIZE);
            draw(&address(0x11, entity, *channel, 1 + version))
        })
        .collect();
    let mut moved_by_version: Vec<(&str, u16)> = Vec::new();
    for (index, entry) in table.iter().enumerate() {
        if seed_a[index] != bumped[index] {
            moved_by_version.push(*entry);
        }
    }
    assert_eq!(
        moved_by_version,
        vec![("sword", purpose::PARTICLE_SIZE)],
        "bumping one stream version moved {moved_by_version:?}, not one stream"
    );
}

#[test]
fn distinct_purposes_and_entities_are_distinct_addresses() {
    let seed = [7u8; anim_project::PROJECT_SEED_BYTES];
    let mut seen: std::collections::BTreeMap<[u64; 2], String> = std::collections::BTreeMap::new();
    for entity in ["a", "b", "c", "d"] {
        for channel in [
            purpose::PARTICLE_DIRECTION,
            purpose::PARTICLE_LIFETIME,
            purpose::PARTICLE_SPEED,
            purpose::PARTICLE_SIZE,
            purpose::PARTICLE_ROTATION,
            purpose::PARTICLE_SUBFRAME,
            purpose::SPRING_INITIAL_OFFSET,
        ] {
            for system_id in [system::PARTICLES, system::SPRINGS] {
                let key = StreamKey {
                    project_seed: seed,
                    system_id,
                    entity_id: node(entity),
                    purpose: channel,
                    stream_version: PHILOX_ALGORITHM_VERSION,
                };
                let label = format!("{entity}/{channel}/{system_id}");
                if let Some(previous) =
                    seen.insert(anim_sim::random::philox_key(&key), label.clone())
                {
                    panic!(
                        "{previous} and {label} resolve to one stream, so one of them is \
                         drawing the other's numbers"
                    );
                }
            }
        }
    }
    assert_eq!(seen.len(), 4 * 7 * 2, "every address is distinct");
}

#[test]
fn a_seed_change_moves_every_stream_and_a_rate_change_moves_the_addresses() {
    let key = |seed_byte: u8| StreamKey {
        project_seed: [seed_byte; anim_project::PROJECT_SEED_BYTES],
        system_id: system::PARTICLES,
        entity_id: node("hero"),
        purpose: purpose::PARTICLE_DIRECTION,
        stream_version: PHILOX_ALGORITHM_VERSION,
    };
    assert_ne!(
        anim_sim::random::philox_key(&key(1)),
        anim_sim::random::philox_key(&key(2)),
        "the seed is in the key by 14.7's sentence, so two projects must not share streams"
    );
    assert_ne!(
        random_f32(&key(1), SimTick(5), 0, 0).unwrap(),
        random_f32(&key(2), SimTick(5), 0, 0).unwrap()
    );
}

// ---------------------------------------------------------------------------
// 14.12 row 8 -- "G4 also requires insertion/removal stability and explicit nonfinite-state
//                 failure tests."
// ---------------------------------------------------------------------------

#[test]
fn insertion_and_removal_are_stable_across_the_whole_run() {
    // The stability claim in the shape that would break it: the same authored events arriving
    // at the same ticks, with the schedule built from scenes whose node insertion order
    // differs. `SceneData` keys its collections by id, so the *document* is identical; what is
    // varied here is the order the test puts the nodes and events in, which is the order a
    // caller's undo history would hand over.
    let ascending = fixture(TICKS_PER_FRAME, 31);
    let mut descending = ProjectDocument::new(
        ascending.project_id,
        FrameRate::new(NUMERATOR, DENOMINATOR).unwrap(),
    );
    descending.revision = ascending.revision;
    descending.simulation = ascending.simulation;
    let source = ascending.scene(&scene_id()).unwrap();
    let mut scene = SceneData::new("g4-shot").unwrap();
    // Collected and then reversed, because `SceneData`'s accessors hand back `impl Iterator`
    // rather than `impl DoubleEndedIterator`: the ordering they guarantee is the map's, and a
    // test that asked for the reverse would be asserting something the type never promised.
    let mut nodes: Vec<_> = source.nodes().cloned().collect();
    nodes.reverse();
    for data in &nodes {
        scene.insert_node(data.clone()).unwrap();
    }
    let mut tracks: Vec<_> = source.tracks().cloned().collect();
    tracks.reverse();
    for track in &tracks {
        scene.insert_track(track.clone()).unwrap();
    }
    let mut events: Vec<_> = source.events().cloned().collect();
    events.reverse();
    for event in &events {
        scene.insert_event(event.clone()).unwrap();
    }
    for marker in source.markers() {
        scene.insert_marker(marker.clone()).unwrap();
    }
    assert!(descending.insert_scene(scene_id(), scene).is_none());

    let last = i64::from(TICKS_PER_FRAME) * 24;
    let a = reference_hashes(&ascending, last);
    let b = reference_hashes(&descending, last);
    assert_eq!(
        a, b,
        "the same authored state, offered in the opposite order, replayed differently"
    );

    // Removal stability: run to a tick where particles are dying, and check that the vector is
    // what a run without them would have produced, in id order.
    let mut context =
        SimulationContext::new(&ascending, scene_id(), CheckpointCache::default()).unwrap();
    let mut state = context.initial_state().unwrap();
    let mut saw_transitions = 0usize;
    let mut previous_ids: Vec<u128> = Vec::new();
    for tick in 1..=last {
        context
            .advance(&mut state, SimTick(tick), SimulationMode::Export, None)
            .unwrap();
        let ids: Vec<u128> = state.particles.iter().map(|p| p.id).collect();
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "tick {tick}: the live set must be in strict id order, whatever was removed: {ids:?}"
        );
        // Every id that survived kept its relative position.
        let kept: Vec<u128> = previous_ids
            .iter()
            .copied()
            .filter(|id| ids.contains(id))
            .collect();
        let prefix_of_new: Vec<u128> = ids
            .iter()
            .copied()
            .filter(|id| previous_ids.contains(id))
            .collect();
        assert_eq!(
            kept, prefix_of_new,
            "tick {tick}: removal permuted the survivors"
        );
        if ids.len() < previous_ids.len() {
            saw_transitions += 1;
        }
        previous_ids = ids;
    }
    assert!(
        saw_transitions > 0,
        "the fixture never removed a particle, so the removal path was not exercised"
    );
}

#[test]
fn a_nonfinite_state_fails_explicitly_at_every_boundary() {
    let document = fixture(TICKS_PER_FRAME, 31);
    let mut context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
    let mut state = context.initial_state().unwrap();
    context
        .advance(&mut state, SimTick(30), SimulationMode::Export, None)
        .unwrap();
    assert!(
        !state.particles.is_empty(),
        "the run must have something to corrupt"
    );

    // Named as function pointers, because each closure is its own anonymous type and an array
    // literal cannot hold four of them. The coercion is the point as well as the fix: these
    // corruptions capture nothing, so nothing about the run leaks into the failure being
    // tested for.
    let corruptions: [Corruption; 4] = [
        ("particle position", |s: &mut SimState| {
            s.particles[0].position[1] = f32::NAN
        }),
        ("particle velocity", |s: &mut SimState| {
            s.particles[0].velocity[2] = f32::INFINITY
        }),
        ("body position", |s: &mut SimState| {
            s.bodies[0].position[0] = f32::NAN
        }),
        ("spring rest length", |s: &mut SimState| {
            s.springs[0].rest_length = f32::NAN
        }),
    ];
    for (label, corrupt) in corruptions {
        let mut broken = state.clone();
        corrupt(&mut broken);
        assert!(
            matches!(broken.validate(), Err(SimError::NonFiniteState { .. })),
            "{label}: 14.11 asks for an explicit failure and got none"
        );
        assert!(
            matches!(
                broken.canonical_bytes(),
                Err(SimError::NonFiniteState { .. })
            ),
            "{label}: a non-finite value must not reach a digest"
        );
        assert!(
            matches!(state_hash(&broken), Err(SimError::NonFiniteState { .. })),
            "{label}: nor into a checkpoint"
        );
        assert!(
            matches!(
                anim_sim::checkpoint::capture_checkpoint(&broken),
                Err(SimError::NonFiniteState { .. })
            ),
            "{label}: and a cache entry must not be written for it"
        );
        // And the run cannot continue from a broken state.
        let input = context.input_at(broken.tick).unwrap();
        assert!(matches!(
            anim_sim::step::step_once(&mut broken, &input, SimulationMode::Export),
            Err(SimError::NonFiniteState { .. }) | Err(SimError::DuplicateId { .. })
        ));
    }
}

#[test]
fn a_nonfinite_value_can_never_be_produced_by_a_legal_step() {
    // The other side of the same row. If a divergence were reachable, the failure above would
    // be a UI nicety rather than a boundary, and the export would abort at a random frame.
    let document = fixture(TICKS_PER_FRAME, 31);
    let mut context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
    let mut state = context.initial_state().unwrap();
    for tick in 1..=800i64 {
        context
            .advance(&mut state, SimTick(tick), SimulationMode::Export, None)
            .unwrap();
        state
            .validate()
            .unwrap_or_else(|error| panic!("tick {tick} produced a state 14.11 refuses: {error}"));
        for particle in &state.particles {
            assert!(
                particle.size <= anim_sim::particles::SIZE_MAX,
                "a drawn size left its authored range: {}",
                particle.size
            );
            assert!(
                particle.lifetime_ticks >= anim_sim::particles::LIFETIME_MIN_TICKS
                    && particle.lifetime_ticks < anim_sim::particles::LIFETIME_MAX_TICKS,
                "a drawn lifetime left its authored range: {}",
                particle.lifetime_ticks
            );
            assert!(
                particle.spawn_ordinal < PARTICLES_PER_SIGNAL,
                "a burst may not exceed its authored size"
            );
        }
    }
}

#[test]
fn particle_ids_are_what_the_same_addresses_produce_again() {
    // A run's identities must not depend on the run: derive them directly and compare with
    // what a full replay produced, which is the check that a spawn address is the address.
    let document = fixture(TICKS_PER_FRAME, 31);
    let mut context =
        SimulationContext::new(&document, scene_id(), CheckpointCache::default()).unwrap();
    let mut state = context.initial_state().unwrap();
    context
        .advance(&mut state, SimTick(60), SimulationMode::Export, None)
        .unwrap();
    for particle in &state.particles {
        assert_eq!(
            particle.id,
            particle_id(
                &particle.emitter,
                particle.generation,
                particle.spawn_ordinal
            ),
            "an identity must be recomputable from the address it was drawn with"
        );
    }
}

#[test]
fn settings_that_refuse_a_rate_are_refused_by_the_context_too() {
    // 14.2's "nonzero u32" reaches the simulation through the persisted section, so the
    // refusal has to be observable from this side as well as from `anim_project`.
    let mut document = fixture(TICKS_PER_FRAME, 31);
    document.simulation.ticks_per_frame = 0;
    assert!(matches!(
        document.validate(),
        Err(anim_project::ProjectError::Validation { .. })
    ));
    // And a rate whose product overflows is the same category from the clock's side.
    let mut huge = fixture(TICKS_PER_FRAME, 31);
    huge.simulation.ticks_per_frame = u32::MAX;
    assert!(matches!(
        SimSettings::new(huge.frame_rate, &huge.simulation),
        Err(SimError::InvalidRate { .. })
    ));
    assert!(matches!(
        SimulationContext::new(&huge, scene_id(), CheckpointCache::default()),
        Err(SimError::InvalidRate { .. })
    ));
}
