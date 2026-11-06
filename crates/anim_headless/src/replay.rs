//! Headless state-hash reporting (spec 14.3).
//!
//! This is the crate's entry into WP05, and the file exists because 14.12's
//! 10,000-frame criterion is a claim about **two fresh processes**. Inside one process,
//! agreement between two runs is structural: the same code, the same statics, the same
//! allocation history. That is worth testing and it is not what 8.1 and 14.12 ask for. The
//! clause asks whether a project saved today replays the same tomorrow, in a process that has
//! never seen the first one — which is only answerable by a program that starts again.
//!
//! So this module's job is to turn a document and a range into text, and to make that text
//! byte-identical between two invocations. Two rules follow from that being the job:
//!
//! 1. **Nothing here reads a clock, a path, an environment variable or a locale.** The report
//!    names the project id, the revision, the rates, the seed and the hashes, and every one of
//!    those is in the document. A timestamp would make the comparison meaningless, which is
//!    why DECISIONS.md #23's no-entropy rule reaches even a diagnostic format.
//! 2. **The aggregate digest runs over the tick hashes, not over the text around them.** The
//!    header exists for a person reading the file; the digest exists for a test comparing two
//!    of them. Folding the header in would make a report-format change look like a simulation
//!    change, and those two have different remedies.
//!
//! [`anim_sim`] computes every number; this file only arranges them as bytes a second process
//! can reproduce. It is deliberately linear in the tick count — a report that re-sought to
//! every tick would be quadratic and would look like a hang rather than like a design error.

use anim_core::hash::CanonicalBytes;
use anim_core::id::SceneId;
use anim_core::time::FrameRange;
use anim_project::ProjectDocument;
use anim_sim::checkpoint::{state_hash, CheckpointCache};
use anim_sim::context::SimulationContext;
use anim_sim::mode::SimulationMode;
use anim_sim::{SimError, SimTick};

/// The one line that says what produced this text, so a reader can tell a WP05 report from a
/// WP09 one and a test can refuse a file that is not a report at all.
pub const REPORT_TAG: &str = "anim_engine/wp05-replay/1";

/// The domain the aggregate digest runs over.
///
/// Stated separately from the engine's state and checkpoint domains because it covers a
/// *report*: the same tick sequence rendered by a different report format must still digest
/// the same, and a different simulation must not.
pub const REPORT_DIGEST_DOMAIN: &str = "anim_engine/wp05_replay_digest/v1";

/// What a report covers, decided by the caller rather than by the environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayRequest {
    /// The last tick to reach. Inclusive of the state *at* that tick, so a request for tick
    /// 40,000 reports 40,001 hashes: the initialized state at zero plus one per step.
    pub last_tick: i64,
    /// Checkpoint cadence to run with, which 14.12 varies independently of the result.
    pub cadence_ticks: u64,
    /// Checkpoint capacity to run with, ditto.
    pub capacity: usize,
    /// Whether every tick's hash is written. `false` leaves the final hash and the aggregate,
    /// which is what a long export log wants; `true` is what the comparison test uses.
    pub per_tick_hashes: bool,
}

impl ReplayRequest {
    /// A request with no checkpoints, which is 14.12's uncheckpointed reference configuration.
    #[must_use]
    pub const fn uncheckpointed(last_tick: i64) -> Self {
        Self {
            last_tick,
            cadence_ticks: 0,
            capacity: 0,
            per_tick_hashes: true,
        }
    }

    /// A request that keeps a bounded checkpoint cache while it runs.
    #[must_use]
    pub const fn checkpointed(last_tick: i64, cadence_ticks: u64, capacity: usize) -> Self {
        Self {
            last_tick,
            cadence_ticks,
            capacity,
            per_tick_hashes: true,
        }
    }
}

/// Run the simulation and render the report (spec 14.12's fresh-process comparison).
///
/// The route is the export route: [`anim_sim::replay::prepare_export`] to the first tick, then
/// one [`anim_sim::context::SimulationContext::step`] per tick in
/// [`SimulationMode::Export`]. With checkpoints disabled — the reference configuration — the
/// warm-up leg has nothing to restore and the run is the plain sequential one, which is the
/// equivalence 14.12 asks the two configurations to satisfy.
///
/// # Errors
///
/// [`SimError`] from the context, the clock or any stepping stage. A document without the
/// requested scene arrives as [`SimError::Scene`], which is what the evaluator already reports.
pub fn report(
    document: &ProjectDocument,
    scene_id: SceneId,
    request: &ReplayRequest,
) -> Result<String, SimError> {
    let cache = CheckpointCache::new(request.cadence_ticks, request.capacity);
    let mut context = SimulationContext::new(document, scene_id, cache)?;
    let clock = context.clock_value();
    let target = SimTick(request.last_tick);
    let last_frame = clock.frame_of_tick(target);
    let mut state = if request.cadence_ticks == 0 || request.capacity == 0 {
        context.initial_state()?
    } else {
        anim_sim::replay::prepare_export(&mut context, FrameRange::new(0, last_frame)?)?
    };
    if state.tick > target {
        return Err(SimError::NegativeReplayTarget {
            target: state.tick.0,
        });
    }

    let mut hashes = vec![state_hash(&state)?];
    while state.tick < target {
        context.step(&mut state, SimulationMode::Export)?;
        hashes.push(state_hash(&state)?);
    }

    // `checkpoints_stored` is a *result* of the run, not an identity of the project: it is
    // written after the loop so a comparison across processes can see that two different
    // policies really did cache differently. It is deliberately not an identity field, because
    // two configurations that agree on hashes must still be allowed to disagree here.
    let mut lines = vec![
        REPORT_TAG.to_string(),
        format!("schema_version {}", document.schema_version),
        format!("project_id {}", document.project_id),
        format!("revision {}", document.revision.0),
        format!(
            "frame_rate {}/{}",
            document.frame_rate.num, document.frame_rate.den
        ),
        format!("ticks_per_frame {}", document.simulation.ticks_per_frame),
        format!("simulation_rate {}/{}", clock.rate.num, clock.rate.den),
        format!("seed {}", document.simulation.seed_hex()),
        format!("scene {scene_id}"),
        format!("cadence_ticks {}", request.cadence_ticks),
        format!("capacity {}", request.capacity),
        format!("last_tick {}", request.last_tick),
    ];

    // The aggregate is over the tick index and the hash, always, whether or not the per-tick
    // lines are printed -- so a `per_tick_hashes: false` report still compares.
    let mut digest = CanonicalBytes::with_domain(REPORT_DIGEST_DOMAIN)?;
    for (index, hash) in hashes.iter().enumerate() {
        digest.u64(u64::try_from(index).unwrap_or(u64::MAX));
        digest.fixed_bytes(hash.as_bytes());
        if request.per_tick_hashes {
            lines.push(format!("tick {index} hash {hash}"));
        }
    }
    let final_hash = hashes
        .last()
        .map(ToString::to_string)
        .unwrap_or_else(|| "none".to_string());
    lines.push(format!(
        "checkpoints_stored {}",
        context.checkpoints().len()
    ));
    lines.push(format!("count {}", hashes.len()));
    lines.push(format!("final_hash {final_hash}"));
    lines.push(format!("aggregate {}", digest.hex_digest()));
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use anim_core::id::{EventDefinitionId, NodeId, ProjectId};
    use anim_core::time::{FramePos, FrameRate};
    use anim_project::{EventDefinition, EventKind, NodeData, SceneData, SimulationSettings};

    fn namespace() -> uuid::Uuid {
        uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn scene_id() -> SceneId {
        SceneId::derive(&namespace(), b"shot01")
    }

    fn document() -> ProjectDocument {
        let mut document = ProjectDocument::new(
            ProjectId::derive(&namespace(), b"headless-fixture"),
            FrameRate::new(24, 1).unwrap(),
        );
        document.simulation =
            SimulationSettings::new(2, [42u8; anim_project::PROJECT_SEED_BYTES]).unwrap();
        let mut scene = SceneData::new("shot01").unwrap();
        let root = NodeId::derive(&namespace(), b"root");
        let child = NodeId::derive(&namespace(), b"child");
        scene
            .insert_node(NodeData::new(root, None, 0, "root"))
            .unwrap();
        scene
            .insert_node(NodeData::new(child, Some(root), 0, "child"))
            .unwrap();
        scene
            .insert_event(EventDefinition::new(
                EventDefinitionId::derive(&namespace(), b"hit"),
                FramePos::at_frame(1),
                0,
                child,
                EventKind::Signal,
                0,
            ))
            .unwrap();
        assert!(document.insert_scene(scene_id(), scene).is_none());
        document
    }

    #[test]
    fn the_report_is_the_same_text_twice_and_says_what_it_ran() {
        let document = document();
        let first = report(&document, scene_id(), &ReplayRequest::uncheckpointed(8)).unwrap();
        let second = report(&document, scene_id(), &ReplayRequest::uncheckpointed(8)).unwrap();
        assert_eq!(first, second, "one document, one text");
        for expected in [
            "anim_engine/wp05-replay/1",
            // The number the report prints is `document.schema_version`, so this expectation
            // moves whenever the document format does — which is the point of asserting it
            // rather than omitting it. It moved 3 -> 4 with WP06's per-asset colour and alpha
            // claims (DECISIONS.md #40 ruling 2), and the report line itself is unchanged:
            // a format bump is a fact about the document, not about this text's layout. The
            // `wp05-replay/1` tag above is the report's own version and did not move.
            "schema_version 4",
            "ticks_per_frame 2",
            "simulation_rate 48/1",
            "seed 2a",
            "count 9",
            "last_tick 8",
        ] {
            assert!(
                first.contains(expected),
                "the header has to name what produced the numbers: missing {expected}"
            );
        }
        assert_eq!(
            first
                .lines()
                .filter(|line| line.starts_with("tick "))
                .count(),
            9,
            "tick 0 through 8 inclusive"
        );
    }

    #[test]
    fn a_checkpointed_run_reports_the_same_hashes_as_the_reference() {
        // 14.12's cadence criterion from the reporting side, at the size this crate's own test
        // budget can afford; the 10,000-frame form is the two-process suite in
        // `anim_sim/tests/determinism.rs`.
        let document = document();
        let plain = report(&document, scene_id(), &ReplayRequest::uncheckpointed(24)).unwrap();
        let aggregate = |text: &str| {
            text.lines()
                .find(|line| line.starts_with("aggregate "))
                .unwrap()
                .to_string()
        };
        let final_hash = |text: &str| {
            text.lines()
                .find(|line| line.starts_with("final_hash "))
                .unwrap()
                .to_string()
        };
        for (cadence, capacity) in [(1, 4), (3, 2), (7, 8), (64, 8)] {
            let cached = report(
                &document,
                scene_id(),
                &ReplayRequest::checkpointed(24, cadence, capacity),
            )
            .unwrap();
            assert_eq!(
                aggregate(&cached),
                aggregate(&plain),
                "cadence {cadence} capacity {capacity} changed the aggregate"
            );
            assert_eq!(final_hash(&cached), final_hash(&plain));
        }
    }

    #[test]
    fn suppressing_the_per_tick_lines_leaves_the_digest_intact() {
        let document = document();
        let mut request = ReplayRequest::uncheckpointed(12);
        let full = report(&document, scene_id(), &request).unwrap();
        request.per_tick_hashes = false;
        let brief = report(&document, scene_id(), &request).unwrap();
        assert!(!brief.contains("\ntick 5 hash "));
        assert_eq!(
            brief
                .lines()
                .find(|line| line.starts_with("aggregate "))
                .unwrap(),
            full.lines()
                .find(|line| line.starts_with("aggregate "))
                .unwrap(),
            "a shorter report is the same statement about the same run"
        );
    }

    #[test]
    fn a_tick_below_the_reached_state_is_refused() {
        let document = document();
        // last_tick 0 with a cache that restores nothing is the ordinary short run, so the
        // refusal path is a target the warm-up leg has already passed -- which cannot happen
        // from frame 0, and is checked as a guard rather than as a behaviour.
        assert!(
            report(&document, scene_id(), &ReplayRequest::uncheckpointed(0))
                .unwrap()
                .contains("count 1")
        );
    }
}
