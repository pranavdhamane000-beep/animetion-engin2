//! Live, replay, warm-up and export side-effect policies (spec 14.3, 14.9).
//!
//! 14.9's sentence is the whole contract, and it is a prohibition rather than a
//! description: "SimulationMode controls external side effects only; it never changes
//! solver parameters." So this type is the only place in the crate where a caller's intent
//! reaches a step, and it reaches exactly one thing — whether the tick's presentation
//! effects are recorded — and nothing else.
//!
//! Making that structural rather than a promise is the point of the file, because the
//! failure mode is the expensive kind: a mode that quietly skipped a solver pass would make
//! warm-up and export disagree about the *world* rather than about whether a sound played,
//! and 14.12's "compare forward playback, backward scrub, warm-up and export against an
//! uncheckpointed reference" is the test that would catch it. That test is
//! [`crate::step::tests`]'s responsibility, and it asserts identical state hashes across
//! all four modes for the same ticks.

/// What the caller intends to do with the state this step produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SimulationMode {
    /// Interactive preview. Presentation effects are recorded; frames may be skipped
    /// between steps, but 14.6 forbids skipping *ticks*, so a preview that cannot catch up
    /// pauses and reconstructs rather than taking a larger step.
    Live,
    /// Reaching a state by replaying from a checkpoint for a scrub or a seek. Not watched,
    /// not heard: reconstruction is not a performance.
    Replay,
    /// Reaching the export start point from a checkpoint (14.7). "Warm-up performs all
    /// internal spawns, random draws, events and solver work but suppresses external one-shot
    /// presentation effects such as device audio."
    WarmUp,
    /// Producing the deliverable. Every effect is recorded.
    Export,
}

impl SimulationMode {
    /// Whether this tick's external presentation effects are withheld (spec 14.7, 14.10).
    ///
    /// 14.10's reason is concrete — "warm-up suppression prevents reconstructed history from
    /// playing device audio" — and replay earns the same answer for the same reason: a
    /// backward scrub reconstructs history that already played the first time. Live and
    /// export are the two modes where someone is watching.
    #[must_use]
    pub const fn suppresses_presentation(self) -> bool {
        matches!(self, Self::Replay | Self::WarmUp)
    }

    /// The mode's own name, for a report that must say which pass produced a frame without
    /// the report changing if a variant is renamed.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Replay => "replay",
            Self::WarmUp => "warm-up",
            Self::Export => "export",
        }
    }
}

impl std::fmt::Display for SimulationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One externally visible effect announced by one tick.
///
/// A presentation effect rather than a simulation event: the sim's job is to decide that a
/// cue occurred and when, and 14.4 forbids this crate from owning an audio device, so the
/// effect ends up in a consumer's hands through
/// [`crate::state::SimState::presentation`]. Carrying the occurrence identity and not a
/// sample index is 13.10's split — `anim_audio` owns the conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimulationEffect {
    /// The tick that announced it, so a consumer can tell a cue from a cue's replay.
    pub tick: crate::SimTick,
    /// Which authored occurrence this is, unchanged from the snapshot that carried it.
    pub occurrence: anim_core::id::EventOccurrenceId,
    /// The node the cue belongs to.
    pub source: anim_core::id::NodeId,
    /// What kind of cue, kept as 13.7's own kind rather than as a new taxonomy: an effect
    /// this crate does not interpret.
    pub kind: anim_project::EventKind,
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
    fn only_the_two_unwatched_modes_suppress() {
        for (mode, suppressed) in [
            (SimulationMode::Live, false),
            (SimulationMode::Export, false),
            (SimulationMode::Replay, true),
            (SimulationMode::WarmUp, true),
        ] {
            assert_eq!(
                mode.suppresses_presentation(),
                suppressed,
                "{mode} is a presentation question with a documented answer"
            );
        }
    }

    #[test]
    fn the_mode_names_are_distinct() {
        let names: Vec<&str> = [
            SimulationMode::Live,
            SimulationMode::Replay,
            SimulationMode::WarmUp,
            SimulationMode::Export,
        ]
        .iter()
        .map(|mode| mode.as_str())
        .collect();
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "a report that cannot tell warm-up from replay is a report about nothing: {names:?}"
        );
    }
}
