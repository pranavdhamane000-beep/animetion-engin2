//! Fixed-step clock, random streams, state, replay, checkpoints, particles and
//! springs (spec 4.2), and the whole of work package WP05 (spec 14).
//!
//! The claim this crate exists to make is in 14.1: results must not depend on cache
//! cadence, rendering rate, thread scheduling or random-call history. Four of those are
//! defeated by four separate decisions here, and each has a 14.12 test rather than a
//! comment:
//!
//! - **cadence** — checkpoints are a cache, and [`CheckpointCache`] may only ever drop
//!   copies. Every tick's state hash is computed from the state, so a run with cadence 1
//!   and a run with cadence 1024 or with caching disabled must produce the same sequence.
//! - **render rate** — the clock converts frames to ticks exactly
//!   (`checked_frame_to_ticks`, 14.7), and a snapshot is sampled by the tick being stepped
//!   rather than by whatever frame the caller last looked at.
//! - **thread scheduling** — there is nothing here to schedule. Every collection whose
//!   order affects a result is a sorted `Vec` or a `BTreeMap` (`clippy.toml` rules out
//!   `HashMap` workspace-wide, and no lock is used), so iteration order is a property of
//!   the data.
//! - **random-call history** — [`random_u32`] and [`random_f32`] are *address functions*:
//!   `StreamKey` plus absolute tick plus semantic draw index plus lane. There is no cursor
//!   to be in a different place, which is what makes a seek and a sequential run agree.
//!
//! Spec 14.4 says this crate depends on `anim_core`, `anim_scene` and read-only
//! `anim_assets`, and must not depend on `anim_commands`, the renderer, an audio device,
//! the editor or wall-clock APIs. It also depends on `anim_project`, which 14.4 does not
//! list: 14.9's own `SimulationContext` "owns immutable project/evaluation inputs", and
//! `anim_scene` re-exports no `anim_core`, `anim_project` or `anim_assets` type at all, so
//! a crate that evaluates a snapshot has to name `ProjectDocument`, `FrameRate` and
//! `SimulationSettings` through their owning crates. That is the same reasoning that made
//! `anim_commands` depend on `anim_assets` directly (deviation row 11) rather than receive
//! another crate's types by forwarding. The prohibition 14.4 actually states — no
//! `anim_commands`, no renderer, no audio device, no editor, no wall clock — holds.
//!
//! The error taxonomy lives here rather than in an `error.rs` because 14.3 lists no such
//! file, which is the resolution `anim_assets` reached for `AssetError` (DECISIONS.md #29)
//! and `anim_scene` for `SceneError`.

pub mod checkpoint;
pub mod clock;
pub mod context;
pub mod events;
pub mod mode;
pub mod particles;
pub mod random;
pub mod replay;
pub mod seek;
pub mod springs;
pub mod state;
pub mod step;

pub use checkpoint::{
    capture_checkpoint, restore_checkpoint, state_hash, Checkpoint, CheckpointCache,
};
pub use clock::{SimTick, SimulationClock};
pub use context::{CacheReport, SimulationContext, SimulationInputFrame};
pub use events::{ScheduledEvent, TickSchedule};
pub use mode::SimulationMode;
pub use particles::{extract_frame, Particle, SimulationFrame};
pub use random::{philox4x32_10, random_f32, random_u32};
pub use replay::{
    export_ticks, prepare_export, replay_to, sequential_hashes, sequential_trace, warm_up_to,
};
pub use seek::{has_checkpoint_before, seek, seek_with_budget};
pub use springs::{Spring, SpringBody};
pub use state::{initial_state, AlgorithmVersions, PresentationLedger, SimSettings, SimState};
pub use step::step_once;

use anim_core::error::CoreError;
use anim_core::hash::CanonicalBytes;
use anim_core::id::NodeId;
use anim_core::time::FrameRate;
use anim_scene::SceneError;

/// The domain separator for a canonical simulation state stream (ADR-0004 consequence 5's
/// rule, the one the document, the manifest and the snapshot already follow).
///
/// 14.7's checkpoint algorithm says "hash with domain separation", and the state is what
/// the checkpoint wraps, so the state gets the tag and the checkpoint wraps that digest.
pub const STATE_DOMAIN: &str = "anim_engine/sim_state/v1";

/// The domain separator for a [`crate::checkpoint::Checkpoint`]'s own stream.
pub const CHECKPOINT_DOMAIN: &str = "anim_engine/checkpoint/v1";

/// The domain separator for the BLAKE3 pre-image of a stream key (spec 14.7).
///
/// The spec names this string literally, so it is repeated nowhere else in this crate.
pub const PHILOX_DOMAIN: &str = "anim_engine/philox/v1";

/// The domain separator for a derived particle identity.
pub const PARTICLE_ID_DOMAIN: &str = "anim_engine/particle_id/v1";

/// The domain separator for a derived spring identity.
pub const SPRING_ID_DOMAIN: &str = "anim_engine/spring_id/v1";

/// The version of the Philox4x32-10 permutation, its key split and its counter layout.
///
/// Bumping this is a statement that every random value this crate has ever produced came
/// from a different function. It is hashed into state digests rather than only living in
/// the code because that is the only way a changed *reading* becomes visible in a saved
/// checkpoint's comparison, and it is the number ADR-0008 exists to keep honest.
pub const PHILOX_ALGORITHM_VERSION: u32 = 1;

/// The version of the stepping model: the order of the stages in
/// [`crate::step::step_once`], the spawn arithmetic, the force functions and the constants
/// in [`crate::particles`] and [`crate::springs`].
///
/// One number rather than one per stage because the stages are not independently
/// substitutable: changing the gravity constant or moving a lifetime update after removal
/// changes what the other stages see, so they move together or the digest is lying about
/// which combination produced this state.
pub const SIMULATION_MODEL_VERSION: u32 = 1;

/// The version of the checkpoint byte layout.
pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;

/// The solver iteration count a state this build creates starts with.
///
/// An engine constant and not a persisted setting, on 14.6's wording: it runs "a **fixed**
/// number of solver iterations", where the same section says the tick rate is the thing
/// that comes from exact configuration. 14.11's `InvalidSolverCount` still has a producer,
/// because the count is carried in the state and re-validated on restore — a checkpoint
/// from a build with a different count is a mismatch, not a value to adopt. When a package
/// wants authors to choose it, it belongs in the schema section
/// [`anim_project::SimulationSettings`] already occupies, with a migration step, and this
/// line is where that change is argued.
pub const DEFAULT_SOLVER_ITERATIONS: u16 = 4;

/// The largest solver iteration count this crate will step with.
///
/// A bound rather than an infinity because the count is inside the per-tick work and is
/// carried in a checkpoint: without a ceiling, a corrupt `u16::MAX` restores as a state
/// that is legal, hashable, and takes a day per tick.
pub const MAX_SOLVER_ITERATIONS: u16 = 1024;

/// The package's error taxonomy (spec 14.11).
///
/// Every variant 14.11 names is present and has a call site that can produce it, with the
/// one exception recorded in `BUILD_STATE.md` rather than here. The variant list is
/// deliberately not flattened: "invalid rates" and "tick overflow" are different remedies
/// for an operator, and 7.4 wants a report that can be filtered on the difference.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum SimError {
    /// A tick rate or simulation rate 14.2 refuses: zero ticks per frame, a rate whose
    /// multiplied numerator does not fit, or a zero-rate frame rate.
    #[error("invalid simulation rate for {which}: {detail}")]
    InvalidRate { which: &'static str, detail: String },

    /// An arithmetic step on the clock passed its exact bound.
    #[error("tick arithmetic overflow computing {operation} for {value}")]
    TickOverflow {
        operation: &'static str,
        value: String,
    },

    /// A replay or seek target below tick zero. 14.7: "SimTick is signed for timeline
    /// arithmetic, but stateful replay supports ticks at or above zero."
    #[error("replay target tick {target} is negative; stateful replay begins at zero")]
    NegativeReplayTarget { target: i64 },

    /// Two objects claiming one identity: a repeated particle id or a repeated spring.
    #[error("duplicate simulation id {kind} {id}")]
    DuplicateId { kind: &'static str, id: String },

    /// A state value that is not finite, found where 14.11 says to look for it.
    ///
    /// The check is on the *canonical encoding* path as well as on the integrator, because
    /// a non-finite value that reaches a digest is a checkpoint that restores differently
    /// on a different platform, and 14.7 insists on "finite validation" before bit
    /// patterns are written.
    #[error("non-finite simulation state in {subject} field {field}")]
    NonFiniteState {
        subject: String,
        field: &'static str,
    },

    /// A solver iteration count of zero, or above [`MAX_SOLVER_ITERATIONS`].
    #[error("invalid solver iteration count {count}")]
    InvalidSolverCount { count: u16 },

    /// An event whose scheduled tick could not be computed, or whose spawn age came out
    /// negative — 14.7 calls that "an internal error", and this is the type that says so.
    #[error("event {event} could not be scheduled: {detail}")]
    UnknownEventTarget { event: String, detail: String },

    /// A checkpoint whose bytes do not decode as this build's checkpoint shape.
    #[error("malformed checkpoint at tick {tick}: {detail}")]
    MalformedCheckpoint { tick: i64, detail: String },

    /// A checkpoint that decoded but whose recomputed digest disagrees with the one it
    /// carries, or whose clock does not match the context stepping it.
    #[error("checkpoint hash mismatch at tick {tick}: {detail}")]
    CheckpointHashMismatch { tick: i64, detail: String },

    /// A random draw this build cannot serve: an unknown algorithm version, or a stream
    /// key built from a configuration no longer supported.
    #[error("unsupported random algorithm version {version}")]
    UnsupportedRandomAlgorithm { version: u32 },

    /// A replay that reached its configured step budget before its target.
    ///
    /// 14.7 is explicit that this is an error rather than a reason to approximate:
    /// "Exhaustion of a replay budget triggers continued replay with a higher explicit
    /// limit or a clear error, never approximate state."
    #[error("replay budget of {budget} ticks exhausted before target tick {target}")]
    ReplayBudgetExhausted { budget: u64, target: i64 },

    /// A tick whose snapshot could not be evaluated, or an event identity the scene does
    /// not have.
    #[error(transparent)]
    Scene(#[from] SceneError),

    /// A checked-arithmetic or canonical-text failure from `anim_core`.
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl SimError {
    /// The stable category for this error (spec 7.4), append-only and dot-separated like
    /// every other crate's.
    #[must_use]
    pub fn code(&self) -> anim_core::diagnostic::DiagnosticCode {
        use anim_core::diagnostic::DiagnosticCode;
        let raw = match self {
            Self::InvalidRate { .. } => "sim.invalid_rate",
            Self::TickOverflow { .. } => "sim.tick_overflow",
            Self::NegativeReplayTarget { .. } => "sim.negative_replay_target",
            Self::DuplicateId { .. } => "sim.duplicate_id",
            Self::NonFiniteState { .. } => "sim.non_finite_state",
            Self::InvalidSolverCount { .. } => "sim.invalid_solver_count",
            Self::UnknownEventTarget { .. } => "sim.unknown_event_target",
            Self::MalformedCheckpoint { .. } => "sim.malformed_checkpoint",
            Self::CheckpointHashMismatch { .. } => "sim.checkpoint_hash_mismatch",
            Self::UnsupportedRandomAlgorithm { .. } => "sim.unsupported_random_algorithm",
            Self::ReplayBudgetExhausted { .. } => "sim.replay_budget_exhausted",
            Self::Scene(_) => "sim.scene",
            Self::Core(_) => "sim.core",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    #[must_use]
    pub fn severity(&self) -> anim_core::diagnostic::DiagnosticSeverity {
        anim_core::diagnostic::DiagnosticSeverity::Error
    }

    /// The structured form a report renders (spec 7.4).
    #[must_use]
    pub fn to_diagnostic(&self) -> anim_core::diagnostic::Diagnostic {
        use anim_core::diagnostic::Diagnostic;
        let base = Diagnostic::new(self.code(), self.severity(), self.to_string());
        match self {
            Self::InvalidRate { which, .. } => base.with_context("which", (*which).to_string()),
            Self::TickOverflow { operation, .. } => {
                base.with_context("operation", (*operation).to_string())
            }
            Self::NegativeReplayTarget { target } => {
                base.with_context("target", target.to_string())
            }
            Self::DuplicateId { kind, id } => base
                .with_context("kind", (*kind).to_string())
                .with_context("id", id.clone()),
            Self::NonFiniteState { field, .. } => base.with_context("field", (*field).to_string()),
            Self::InvalidSolverCount { count } => base.with_context("count", count.to_string()),
            Self::UnknownEventTarget { event, .. } => base.with_context("event", event.clone()),
            Self::MalformedCheckpoint { tick, .. } | Self::CheckpointHashMismatch { tick, .. } => {
                base.with_context("tick", tick.to_string())
            }
            Self::UnsupportedRandomAlgorithm { version } => {
                base.with_context("version", version.to_string())
            }
            Self::ReplayBudgetExhausted { budget, target } => base
                .with_context("budget", budget.to_string())
                .with_context("target", target.to_string()),
            Self::Scene(error) => error.to_diagnostic(),
            Self::Core(error) => error.to_diagnostic(),
        }
    }
}

/// The random-draw errors 14.9's signature names separately.
///
/// Separate rather than folded into [`SimError`] because 14.9 types the two draw
/// functions this way, and because a bad lane or a bad tick is a *call-site* bug in a
/// consumer's draw table while every [`SimError`] variant is a state or configuration bug;
/// a caller that can retry one cannot retry the other. [`SimError::UnsupportedRandomAlgorithm`]
/// is reached through [`RandomError::UnsupportedAlgorithm`] by `?` at the boundary.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RandomError {
    /// A lane outside 0..=3 (spec 14.7: "Lane above three, negative tick or overflow
    /// returns an error").
    #[error("random lane {lane} is outside 0..=3")]
    LaneOutOfRange { lane: u8 },

    /// A draw at a tick below zero, which has no counter representation and no replay.
    #[error("random draw at negative tick {tick}")]
    NegativeTick { tick: i64 },

    /// A counter word that could not be formed: the tick or draw index exceeded the
    /// address space of the four 32-bit counter words.
    #[error("random address overflow for {which} value {value}")]
    AddressOverflow { which: &'static str, value: String },
}

impl RandomError {
    /// The stable category for this error (spec 7.4).
    #[must_use]
    pub fn code(&self) -> anim_core::diagnostic::DiagnosticCode {
        use anim_core::diagnostic::DiagnosticCode;
        let raw = match self {
            Self::LaneOutOfRange { .. } => "random.lane_out_of_range",
            Self::NegativeTick { .. } => "random.negative_tick",
            Self::AddressOverflow { .. } => "random.address_overflow",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    /// The structured form a report renders.
    #[must_use]
    pub fn to_diagnostic(&self) -> anim_core::diagnostic::Diagnostic {
        use anim_core::diagnostic::{Diagnostic, DiagnosticSeverity};
        let base = Diagnostic::new(self.code(), DiagnosticSeverity::Error, self.to_string());
        match self {
            Self::LaneOutOfRange { lane } => base.with_context("lane", lane.to_string()),
            Self::NegativeTick { tick } => base.with_context("tick", tick.to_string()),
            Self::AddressOverflow { which, value } => base
                .with_context("which", (*which).to_string())
                .with_context("value", value.clone()),
        }
    }
}

impl From<RandomError> for SimError {
    /// A bad address is a draw that could not be placed on the timeline, which is the same
    /// category 14.11 already names for an event that could not be placed on it.
    ///
    /// The variant this used to carry — "unsupported random algorithm" — lives on
    /// [`SimError`] instead, because the value that identifies a permutation is
    /// [`crate::state::AlgorithmVersions::philox`], and the only place that value arrives from
    /// data rather than from this build's own constants is
    /// [`crate::checkpoint::restore_checkpoint`]. A `StreamKey` has no algorithm field: 14.8
    /// gives it a `stream_version`, which is a per-stream dial and 14.12's "changing one
    /// stream version" criterion requires it to be free to differ.
    fn from(error: RandomError) -> Self {
        Self::UnknownEventTarget {
            event: "random draw".to_string(),
            detail: error.to_string(),
        }
    }
}

/// The result type every fallible function in this crate returns.
pub type Result<T, E = SimError> = std::result::Result<T, E>;

/// The identity of one simulation stream, exactly as spec 14.8 declares it.
///
/// Held by value and cheap to copy, because it is an address and not an object: 14.7
/// derives a 128-bit Philox key from these five terms on every draw, so a stream has no
/// state to own and two consumers that name the same five terms get the same numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamKey {
    /// The project's seed, from the persisted [`anim_project::SimulationSettings`].
    pub project_seed: [u8; anim_project::PROJECT_SEED_BYTES],
    /// Which subsystem is drawing: 14.7's "system ID".
    pub system_id: u16,
    /// The node the draw belongs to — 14.7's "emitter/entity ID", and the only identity a
    /// snapshot can hand a consumer.
    pub entity_id: NodeId,
    /// The semantic channel, from this crate's fixed tables.
    pub purpose: u16,
    /// The stream's version, so a consumer's own algorithm change moves its numbers
    /// without moving anyone else's (14.12's last clause).
    pub stream_version: u32,
}

impl StreamKey {
    /// The five terms 14.7 hashes, in the order it lists them, with the domain tag the
    /// same section names literally.
    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.fixed_bytes(&self.project_seed);
        cp.u32(u32::from(self.system_id));
        cp.fixed_bytes(self.entity_id.as_bytes());
        cp.u32(u32::from(self.purpose));
        cp.u32(self.stream_version);
    }

    /// The unhashed BLAKE3 pre-image 14.7's derivation runs over.
    ///
    /// Returned as the encoder rather than as bytes so a caller can take
    /// [`anim_core::hash::CanonicalBytes::digest`] without a second hash crate in this one;
    /// spec 14.4 says `anim_sim` uses blake3, and it does, through the canonical encoder
    /// every other crate in this workspace already reaches it through.
    pub(crate) fn pre_image(&self) -> CanonicalBytes {
        // `with_domain` is `new()` plus a length-prefixed `text`, so an id derivation
        // whose signature is infallible can spell it directly instead of unwrapping a
        // constant it already knows is valid ASCII. The bytes are identical; see
        // anim_core::hash::CanonicalBytes::with_domain.
        let mut cp = CanonicalBytes::new();
        cp.text(PHILOX_DOMAIN);
        self.encode(&mut cp);
        cp
    }

    /// The canonical pre-image this key is hashed from.
    ///
    /// Public because ADR-0008's golden vectors are stated over it: a vector for the
    /// permutation is not a vector for the *derivation*, and the derivation is the half
    /// that can change what every stream returns without changing Philox at all.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.pre_image().into_vec()
    }
}

/// The simulation rate: `frame_rate * ticks_per_frame` (spec 14.2), checked.
///
/// Kept as a free function rather than inlined in [`crate::clock::SimulationClock::new`]
/// because the refusal has to be the same on every route into a clock, and 14.2 states it
/// as a property of the settings rather than of the clock. The numerator widens through
/// `u64` before narrowing, because `30000 * 200_000` fits neither `u32` nor an unchecked
/// multiply, and an NTSC rate reduced here would silently rescale every scheduled tick.
fn simulation_rate(frame_rate: FrameRate, ticks_per_frame: u32) -> Result<FrameRate> {
    if ticks_per_frame == 0 {
        return Err(SimError::InvalidRate {
            which: "ticks_per_frame",
            detail: "is zero; spec 14.2 requires a nonzero u32".to_string(),
        });
    }
    let num = u64::from(frame_rate.num)
        .checked_mul(u64::from(ticks_per_frame))
        .and_then(|product| u32::try_from(product).ok())
        .ok_or_else(|| SimError::InvalidRate {
            which: "simulation rate",
            detail: format!(
                "{} * {ticks_per_frame} ticks per frame does not fit a u32 numerator",
                frame_rate.num
            ),
        })?;
    FrameRate::new(num, frame_rate.den).map_err(|e| SimError::InvalidRate {
        which: "simulation rate",
        detail: e.to_string(),
    })
}
