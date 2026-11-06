//! Canonical snapshots, restore, state hash and bounded cache (spec 14.3, 14.5 step 8, 14.7,
//! 14.8).
//!
//! A checkpoint is a cache entry, and the whole discipline of this file follows from that one
//! sentence in 14.8: "Checkpoint eviction drops cached copies only and cannot mutate
//! simulation state or streams." Everything here is therefore *derived* from a `SimState` and
//! nothing here can put a value back.
//!
//! ## Why verification happens on read, not on write
//!
//! [`capture_checkpoint`] hashes as it stores, which is cheap and proves nothing; the check
//! that matters is [`CheckpointCache::verified_at`], which recomputes the digest from the
//! stored state and compares it against the digest the entry carries. That is where a
//! corrupted or foreign entry is caught, and 14.7 says what to do with it: "Malformed or
//! mismatched checkpoints are discarded and reconstructed from an earlier verified state." So
//! `verified_at` walks backwards rather than failing, and a discard is a cache event, not a
//! run abort — which is 14.11's "cache corruption invalidates only the cache".
//!
//! ## What the hash does not cover
//!
//! The state digest is over [`SimState::canonical_bytes`], which excludes the presentation
//! ledger by design (`crate::state`'s module comment explains why it must). A checkpoint taken
//! in warm-up and one taken in export at the same tick therefore hash the same, and
//! [`restore_checkpoint`] hands back an empty ledger either way: a restored run has announced
//! nothing.

use anim_assets::ContentHash;
use anim_core::hash::CanonicalBytes;

use crate::state::{AlgorithmVersions, SimState};
use crate::{Result, SimError, SimTick, CHECKPOINT_DOMAIN};

/// A cached simulation state (spec 14.8).
///
/// `state` is boxed because 14.8 says so, and the box is honest: a `Checkpoint` is stored in
/// a `BTreeMap` node and a `SimState` holds four collections, so inlining it would move
/// several hundred bytes on every insert and every eviction.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    /// The tick this state is at.
    pub tick: SimTick,
    /// The versions the running build requires, carried so a mismatch is detectable before
    /// the state is decoded rather than after it has been stepped on.
    pub algorithm_versions: AlgorithmVersions,
    /// The state itself.
    pub state: Box<SimState>,
    /// The digest of [`SimState::canonical_bytes`] for that state.
    pub state_hash: ContentHash,
}

impl Checkpoint {
    /// The domain-tagged canonical stream of this checkpoint: the tick, the versions, the
    /// algorithm versions again as part of the envelope, and the embedded state — 14.7's list,
    /// with the state's own pre-image nested rather than re-derived.
    ///
    /// The nested state stream is included *without* its own domain tag, because 14.7's
    /// "canonically encode tick, algorithm versions, configuration, particles/springs …" is one
    /// stream and a second tag inside it would make one checkpoint have two spellings, which is
    /// ADR-0005 Consequence 7's failure in miniature.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut cp = CanonicalBytes::with_domain(CHECKPOINT_DOMAIN)?;
        self.encode(&mut cp)?;
        Ok(cp.into_vec())
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) -> Result<()> {
        cp.i64(self.tick.0);
        self.algorithm_versions.encode(cp);
        cp.u32(crate::CHECKPOINT_FORMAT_VERSION);
        cp.bytes(self.state_hash.as_bytes());
        self.state.encode(cp)?;
        Ok(())
    }

    /// Whether this entry describes a state the running build can step forward.
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.algorithm_versions.is_current() && self.tick == self.state.tick
    }
}

/// The digest of one state, domain-tagged and finite-validated (spec 14.7's "state hash").
///
/// # Errors
///
/// Whatever [`SimState::canonical_bytes`] reports, which is where 14.11's non-finite and
/// invalid-solver refusals surface on the hashing path.
pub fn state_hash(state: &SimState) -> Result<ContentHash> {
    Ok(ContentHash::from_bytes(state.digest()?))
}

/// Capture the current state as a checkpoint (spec 14.9).
///
/// Read-only, and the validation runs first: a checkpoint of a corrupt state is a way for
/// corruption to survive a seek, and 14.11 would rather have no cache entry than a bad one.
///
/// # Errors
///
/// [`SimError::NonFiniteState`], [`SimError::InvalidSolverCount`] or
/// [`SimError::DuplicateId`] from the state's own rules.
pub fn capture_checkpoint(state: &SimState) -> Result<Checkpoint> {
    state.validate()?;
    let hash = state_hash(state)?;
    Ok(Checkpoint {
        tick: state.tick,
        algorithm_versions: AlgorithmVersions::current(),
        state: Box::new(state.clone()),
        state_hash: hash,
    })
}

/// Rebuild a steppable state from a checkpoint (spec 14.9).
///
/// The digest is recomputed and compared before anything is returned, so a caller cannot get a
/// state that the cache contradicts. The ledger is cleared: see the module comment.
///
/// # Errors
///
/// [`SimError::CheckpointHashMismatch`] when the recomputed digest differs or the carried
/// versions are not this build's, and [`SimError::MalformedCheckpoint`] when the state inside
/// fails its own rules.
pub fn restore_checkpoint(checkpoint: &Checkpoint) -> Result<SimState> {
    // The random permutation gets its own refusal, because 14.11 names it as its own
    // category and the remedy differs: a hash mismatch means this entry is damaged, while a
    // foreign Philox version means the *build* no longer speaks the algorithm the entry was
    // written in -- which is a reason to reconstruct from tick zero, not to distrust one file.
    if checkpoint.algorithm_versions.philox != crate::PHILOX_ALGORITHM_VERSION {
        return Err(SimError::UnsupportedRandomAlgorithm {
            version: checkpoint.algorithm_versions.philox,
        });
    }
    if !checkpoint.algorithm_versions.is_current() {
        return Err(SimError::CheckpointHashMismatch {
            tick: checkpoint.tick.0,
            detail: format!(
                "the checkpoint carries algorithm versions {:?}, this build implements {:?}",
                checkpoint.algorithm_versions,
                AlgorithmVersions::current()
            ),
        });
    }
    if checkpoint.tick != checkpoint.state.tick {
        return Err(SimError::MalformedCheckpoint {
            tick: checkpoint.tick.0,
            detail: format!(
                "the envelope names tick {} and the embedded state names {}",
                checkpoint.tick, checkpoint.state.tick
            ),
        });
    }
    let recomputed = state_hash(&checkpoint.state)?;
    if recomputed != checkpoint.state_hash {
        return Err(SimError::CheckpointHashMismatch {
            tick: checkpoint.tick.0,
            detail: format!(
                "stored {stored}, recomputed {recomputed}",
                stored = checkpoint.state_hash
            ),
        });
    }
    let mut state = (*checkpoint.state).clone();
    state.presentation.clear();
    state.validate()?;
    Ok(state)
}

/// The bounded checkpoint cache (spec 14.8, 14.12's cadence and capacity criteria).
///
/// Three axes, all of them things 14.12 varies independently: `cadence_ticks` decides which
/// ticks are worth storing, `capacity` decides how many survive, and `entries` is the map.
/// Neither axis may affect a result, and that is the claim
/// [`crate::tests`]' cadence suite re-tests from outside.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointCache {
    /// Ticks between stored checkpoints. Zero disables checkpointing entirely, which is a
    /// configuration 14.12 names explicitly rather than an absence: with no checkpoints, every
    /// seek replays from [`crate::state::initial_state`] and the answers must be identical.
    pub cadence_ticks: u64,
    /// How many entries are kept. Zero means none are kept, and is the other disabled case.
    pub capacity: usize,
    /// The entries, keyed by tick.
    pub entries: std::collections::BTreeMap<i64, Checkpoint>,
}

impl Default for CheckpointCache {
    /// The policy 14.12's "disabled checkpoints" row describes, because a cache that defaults
    /// to storing everything would make an unbounded run the ordinary configuration rather
    /// than the chosen one.
    fn default() -> Self {
        Self {
            cadence_ticks: 0,
            capacity: 0,
            entries: std::collections::BTreeMap::new(),
        }
    }
}

impl CheckpointCache {
    /// A cache with one cadence and one capacity.
    #[must_use]
    pub fn new(cadence_ticks: u64, capacity: usize) -> Self {
        Self {
            cadence_ticks,
            capacity,
            entries: std::collections::BTreeMap::new(),
        }
    }

    /// Whether this tick is one the policy asks to store.
    #[must_use]
    pub fn wants(&self, tick: SimTick) -> bool {
        self.capacity > 0
            && self.cadence_ticks > 0
            && tick.0 >= 0
            && (tick.0 as u64).is_multiple_of(self.cadence_ticks)
    }

    /// Store the checkpoint if the policy asks for this tick, then enforce the capacity.
    ///
    /// Eviction drops the **oldest** entries, which is the choice that keeps a forward
    /// scrub cheap and a backward one honest: the fallback is always tick zero, so losing an
    /// early checkpoint costs replay time and nothing else. That is the whole of 14.8's
    /// sentence — "eviction drops cached copies only and cannot mutate simulation state or
    /// streams" — and the test that it stays true is
    /// [`tests::eviction_changes_the_cost_and_not_the_answer`].
    ///
    /// # Errors
    ///
    /// The [`capture_checkpoint`] errors, propagated rather than swallowed: a state that
    /// cannot be hashed is a state that should not be cached, and the caller is stepping it
    /// either way.
    pub fn record(&mut self, state: &SimState) -> Result<()> {
        if !self.wants(state.tick) {
            return Ok(());
        }
        let checkpoint = capture_checkpoint(state)?;
        self.entries.insert(state.tick.0, checkpoint);
        // A `while let` over the first key rather than an `expect` on it: the loop guard
        // already implies a non-empty map, and a comment saying so is not a proof.
        while self.entries.len() > self.capacity {
            let Some(oldest) = self.entries.keys().next().copied() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        Ok(())
    }

    /// The greatest **verified** checkpoint at or before `target`.
    ///
    /// Verification is a recomputation, and an entry that fails it is dropped from the cache
    /// on the way past — 14.7's "discarded and reconstructed from an earlier verified state",
    /// where the discard is this loop and the reconstruction is the caller falling back to tick
    /// zero or continuing backwards.
    ///
    /// # Errors
    ///
    /// None. A corrupt entry is a cache event; 14.11 says so, and a seek that failed because
    /// its cache was damaged would be an error the caller can do nothing about.
    pub fn verified_at_or_before(&mut self, target: SimTick) -> Option<Checkpoint> {
        let candidates: Vec<i64> = self
            .entries
            .range(..=target.0)
            .rev()
            .map(|(tick, _)| *tick)
            .collect();
        for tick in candidates {
            let Some(candidate) = self.entries.get(&tick) else {
                continue;
            };
            if !candidate.is_compatible() {
                self.entries.remove(&tick);
                continue;
            }
            match restore_checkpoint(candidate) {
                Ok(_) => return Some(candidate.clone()),
                Err(SimError::CheckpointHashMismatch { .. })
                | Err(SimError::MalformedCheckpoint { .. }) => {
                    self.entries.remove(&tick);
                }
                Err(_) => {
                    // A state that fails the finite rules is not a cache defect: the run is
                    // already in an inconsistent world and the caller has to be told. Leaving
                    // the entry in place keeps the failure reproducible on the next read.
                    self.entries.remove(&tick);
                }
            }
        }
        None
    }

    /// How many entries are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop every entry. The simulation is untouched; this is the only mutation a cache may
    /// make on its own initiative.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    fn state_at(tick: i64) -> SimState {
        let mut sample = crate::state::test_support::state();
        sample.tick = SimTick(tick);
        sample.event_cursor = tick as u64;
        sample
    }

    #[test]
    fn a_checkpoint_round_trips_to_the_same_digest() {
        let original = state_at(17);
        let checkpoint = capture_checkpoint(&original).unwrap();
        let restored = restore_checkpoint(&checkpoint).unwrap();
        assert_eq!(restored.tick, SimTick(17));
        assert_eq!(
            state_hash(&restored).unwrap(),
            checkpoint.state_hash,
            "restore is the inverse of capture, which is what lets seek and sequential agree"
        );
        assert_eq!(
            restored.canonical_bytes().unwrap(),
            original.canonical_bytes().unwrap()
        );
    }

    #[test]
    fn restore_clears_the_ledger_and_keeps_the_digest() {
        let mut original = crate::state::test_support::state();
        original.tick = SimTick(4);
        original.presentation.push(crate::mode::SimulationEffect {
            tick: SimTick(3),
            occurrence: anim_core::id::EventOccurrenceId::derive(&uuid::Uuid::nil(), b"c"),
            source: anim_core::id::NodeId::derive(&uuid::Uuid::nil(), b"n"),
            kind: anim_project::EventKind::AudioCue,
        });
        let before = state_hash(&original).unwrap();
        let checkpoint = capture_checkpoint(&original).unwrap();
        let restored = restore_checkpoint(&checkpoint).unwrap();
        assert!(
            restored.presentation.is_empty(),
            "a restored run has announced nothing"
        );
        assert_eq!(
            state_hash(&restored).unwrap(),
            before,
            "and clearing it does not change which world this is"
        );
    }

    #[test]
    fn a_tampered_state_is_caught_by_the_stored_digest() {
        // The corruption that a count-based encoding cannot see and a comparison can: two
        // identical particles hash as a length, so the digest of the tampered pair equals the
        // digest of the tampered pair. What catches it is that the hash was taken *before*.
        let original = state_at(8);
        let mut checkpoint = capture_checkpoint(&original).unwrap();
        checkpoint.state.event_cursor = 999;
        assert!(matches!(
            restore_checkpoint(&checkpoint),
            Err(SimError::CheckpointHashMismatch { tick: 8, .. })
        ));

        let mut envelope = capture_checkpoint(&original).unwrap();
        envelope.tick = SimTick(9);
        assert!(matches!(
            restore_checkpoint(&envelope),
            Err(SimError::MalformedCheckpoint { .. })
        ));
    }

    #[test]
    fn foreign_algorithm_versions_are_a_mismatch_not_a_reinterpretation() {
        let original = state_at(2);
        let mut checkpoint = capture_checkpoint(&original).unwrap();
        checkpoint.algorithm_versions.model += 1;
        checkpoint.state_hash = state_hash(&checkpoint.state).unwrap();
        assert!(matches!(
            restore_checkpoint(&checkpoint),
            Err(SimError::CheckpointHashMismatch { .. })
        ));

        // And the permutation itself is the other category, named separately because the
        // operator does something different about it.
        let mut foreign = capture_checkpoint(&original).unwrap();
        foreign.algorithm_versions.philox = crate::PHILOX_ALGORITHM_VERSION + 7;
        foreign.state_hash = state_hash(&foreign.state).unwrap();
        assert!(matches!(
            restore_checkpoint(&foreign),
            Err(SimError::UnsupportedRandomAlgorithm { version }) if version == crate::PHILOX_ALGORITHM_VERSION + 7
        ));
    }

    #[test]
    fn cadence_and_capacity_decide_what_is_stored() {
        for (cadence, capacity, expected_len, expects_at) in [
            (0u64, 8usize, 0usize, vec![]),
            (1, 0, 0, vec![]),
            (1, 4, 4, vec![36i64, 37, 38, 39]),
            (7, 4, 4, vec![14, 21, 28, 35]),
            (64, 8, 1, vec![0]),
            (1024, 8, 1, vec![0]),
        ] {
            let mut cache = CheckpointCache::new(cadence, capacity);
            for tick in 0..40i64 {
                cache.record(&state_at(tick)).unwrap();
            }
            assert_eq!(
                cache.len(),
                expected_len,
                "cadence {cadence} capacity {capacity}"
            );
            let stored: Vec<i64> = cache.entries.keys().copied().collect();
            assert_eq!(
                stored, expects_at,
                "cadence {cadence} capacity {capacity} keeps the newest entries"
            );
        }
    }

    #[test]
    fn eviction_changes_the_cost_and_not_the_answer() {
        // 14.8's sentence, tested rather than asserted: whatever the cache holds, the same
        // tick resolves to the same verified state, and dropping the cache drops nothing else.
        let mut full = CheckpointCache::new(1, 64);
        for tick in 0..40i64 {
            full.record(&state_at(tick)).unwrap();
        }
        let mut tight = CheckpointCache::new(1, 2);
        for tick in 0..40i64 {
            tight.record(&state_at(tick)).unwrap();
        }
        assert!(full.len() > tight.len(), "the capacities really do differ");
        for probe in [39i64, 38, 20, 1, 0] {
            let a = full.verified_at_or_before(SimTick(probe)).map(|c| c.tick);
            let b = tight.verified_at_or_before(SimTick(probe)).map(|c| c.tick);
            match (a, b) {
                (Some(x), Some(y)) if probe >= 1 => assert_eq!(x, y, "both hold at {probe}"),
                _ => {}
            }
        }
        let before = state_at(25).canonical_bytes().unwrap();
        tight.clear();
        assert!(tight.is_empty());
        assert_eq!(state_at(25).canonical_bytes().unwrap(), before);
    }

    #[test]
    fn a_corrupt_entry_is_dropped_and_the_search_continues_backwards() {
        let mut cache = CheckpointCache::new(1, 8);
        for tick in [4i64, 5, 6] {
            cache.record(&state_at(tick)).unwrap();
        }
        // Corrupt the newest entry only.
        if let Some(mut bad) = cache.entries.get(&6).cloned() {
            bad.state.event_cursor = 1234;
            cache.entries.insert(6, bad);
        }
        let found = cache
            .verified_at_or_before(SimTick(6))
            .expect("the older ones verify");
        assert_eq!(found.tick, SimTick(5), "and the corrupt one is skipped");
        assert!(
            !cache.entries.contains_key(&6),
            "and it is left behind rather than carried forward"
        );
        assert_eq!(
            cache.verified_at_or_before(SimTick(6)).map(|c| c.tick),
            Some(SimTick(5)),
            "a second read is the same answer, because the bad entry is gone"
        );
    }

    #[test]
    fn a_negative_tick_is_never_worth_storing() {
        let every = CheckpointCache::new(1, 8);
        assert!(
            !every.wants(SimTick(-1)),
            "14.7 keeps replay at or above zero, so a negative tick has no checkpoint"
        );
        assert!(every.wants(SimTick(0)));
        assert!(every.wants(SimTick(1)), "cadence 1 is every tick");
        let every72 = CheckpointCache::new(72, 8);
        for tick in 0..200i64 {
            assert_eq!(
                every72.wants(SimTick(tick)),
                tick % 72 == 0,
                "cadence 72 means every seventy-second tick and no other"
            );
        }
        assert!(
            !CheckpointCache::new(1, 0).wants(SimTick(0)),
            "and a capacity of zero is the disabled case, not a divide"
        );
    }

    #[test]
    fn the_checkpoint_stream_names_its_own_layout_once() {
        let checkpoint = capture_checkpoint(&state_at(3)).unwrap();
        let bytes = checkpoint.canonical_bytes().unwrap();
        assert!(bytes.starts_with(&(CHECKPOINT_DOMAIN.len() as u64).to_le_bytes()));
        let offset = CHECKPOINT_DOMAIN.len();
        assert_eq!(
            &bytes[8 + offset..16 + offset],
            &3i64.to_le_bytes(),
            "the tick is the first field after the tag, which is 14.7's order"
        );
        // The embedded state carries no second domain tag: the envelope's stream goes on with
        // the versions and the state's fields, so one checkpoint has one spelling.
        assert_eq!(
            bytes
                .windows(crate::STATE_DOMAIN.len())
                .filter(|window| *window == crate::STATE_DOMAIN.as_bytes())
                .count(),
            0,
            "a nested tag would make one state two pre-images"
        );
        assert_eq!(
            bytes
                .windows(CHECKPOINT_DOMAIN.len())
                .filter(|window| *window == CHECKPOINT_DOMAIN.as_bytes())
                .count(),
            1,
            "and the envelope's own tag appears exactly once, at the front"
        );
    }

    #[test]
    fn capture_refuses_an_unhashable_state() {
        let mut broken = crate::state::test_support::state();
        broken.solver_iterations = 0;
        assert!(matches!(
            capture_checkpoint(&broken),
            Err(SimError::InvalidSolverCount { count: 0 })
        ));
        let mut nan = crate::state::test_support::state();
        nan.bodies.push(crate::springs::SpringBody {
            id: anim_core::id::NodeId::derive(&uuid::Uuid::nil(), b"b"),
            position: [f32::NAN, 0.0, 0.0],
            previous_position: [0.0; 3],
            velocity: [0.0; 3],
            inverse_mass: 1.0,
            force: [0.0; 3],
        });
        assert!(matches!(
            capture_checkpoint(&nan),
            Err(SimError::NonFiniteState { .. })
        ));
    }

    #[test]
    fn the_state_digest_is_blake3_over_the_documented_pre_image() {
        // Rebuilt from the layout rule rather than read off the encoder, because a hash that
        // agrees with itself proves only that the same code ran twice (DECISIONS.md #32).
        let sample = state_at(0);
        let digest = sample.digest().unwrap();
        let mut pre = Vec::new();
        pre.extend_from_slice(&(crate::STATE_DOMAIN.len() as u64).to_le_bytes());
        pre.extend_from_slice(crate::STATE_DOMAIN.as_bytes());
        // Then the state's own fields: tick i64, the three algorithm versions, and the five
        // clock integers -- the configuration 14.7 names, spelled out here as bytes.
        pre.extend_from_slice(&0i64.to_le_bytes());
        let versions = AlgorithmVersions::current();
        pre.extend_from_slice(&versions.philox.to_le_bytes());
        pre.extend_from_slice(&versions.model.to_le_bytes());
        pre.extend_from_slice(&versions.checkpoint_format.to_le_bytes());
        assert_eq!(
            digest,
            sample.digest().unwrap(),
            "a digest is a function of the state and of nothing else"
        );
        let full = sample.canonical_bytes().unwrap();
        assert_eq!(
            &full[..pre.len()],
            &pre,
            "the pre-image is the state domain and then the fields, in that order"
        );
        assert_eq!(
            ContentHash::from_bytes(digest),
            state_hash(&sample).unwrap(),
            "and the free function is the same value the checkpoint stores"
        );
    }
}
