//! Philox4x32-10, stream keys and address-based draws (spec 14.3, 14.7, 14.9).
//!
//! 14.1's fourth independence — "results must not depend on … random-call history" — is
//! bought here, and it is the reason this file has no `Rng` object. A stream draw is an
//! **address function**: `StreamKey` plus absolute tick plus semantic draw index plus lane
//! go in, one `u32` comes out, and nothing is read from or advanced in state. 14.7 says so
//! twice, once constructively ("Input: StreamKey, absolute tick, semantic draw_index, and
//! lane") and once as a prohibition ("never a 'next random' cursor"). A cursor cannot
//! survive the seek test in 14.12, because seeking to tick 400 without having stepped 399
//! times would land on a different number than a sequential run does; a pure function of
//! the address cannot.
//!
//! ## The permutation is published, and certified against the published answers
//!
//! [`philox4x32_10`] is Salmon, Moraes, Dror and Shaw's Philox4x32 with exactly ten rounds,
//! from D. E. Shaw Research's own Random123 source. `tests/fixtures/
//! philox4x32_10_test_vectors.json` carries 55 known-answer vectors fetched from that
//! repository at a pinned commit, with the SHA-256 of each fetched file recorded in the
//! fixture, and [`tests`] certifies all of them. The round function, the multipliers and
//! the subkey bump are not reconstructed from memory anywhere in this crate; if they were,
//! a wrong constant would be a wrong-but-self-consistent generator that every internal test
//! here would pass.
//!
//! ## The key is narrower than the derivation, and that is a spec gap this file decides
//!
//! 14.7 says to derive a **128-bit** key and "split into two little-endian u64 Philox
//! keys", and 14.9 types the parameter `[u64; 2]`. Philox4x32 does not have a 128-bit key:
//! Random123 documents the key as half as wide as the counter, `philox4x32_key_t` is a
//! **two-word `u32`** array, and the round consumes those two words. A literal
//! implementation of 14.7 therefore feeds the primitive 64 of its 128 derived bits and
//! ignores the other 64 silently — two `StreamKey`s differing only in a dead half would
//! draw identical numbers, which defeats 14.7's whole purpose of including the seed and the
//! entity in the key at all.
//!
//! So the split is kept exactly as 14.9 types it, and the **derivation** folds: the 128-bit
//! BLAKE3 output's first four bytes XOR its second four into subkey 0, and its third four
//! XOR its fourth into subkey 1. Every derived bit now influences every draw, the
//! permutation is untouched and still vector-certified, and the entropy ceiling is the
//! primitive's own 64 key bits rather than a discard we could have avoided. The fold is a
//! `StreamKey`-layer decision, recorded with its rejected alternative in ADR-0008, which the
//! spec's own ADR register assigns to exactly this question.

use crate::{RandomError, StreamKey};

/// Philox4x32 multiplier for counter word 0 (Random123 `PHILOX_A0_32`).
const PHILOX_MULTIPLIER_0: u32 = 0xD251_1F53;

/// Philox4x32 multiplier for counter word 2 (Random123 `PHILOX_A1_32`).
const PHILOX_MULTIPLIER_1: u32 = 0xCD9E_8D57;

/// The subkey-0 bump, applied once per round (Random123 `PHILOX_W32_0`).
const PHILOX_SUBKEY_BUMP_0: u32 = 0x9E37_79B9;

/// The subkey-1 bump, applied once per round (Random123 `PHILOX_W32_1`).
const PHILOX_SUBKEY_BUMP_1: u32 = 0xBB67_AE85;

/// The round count this primitive is named for, spelled once (spec 14.5 step 2).
const PHILOX_ROUNDS: usize = 10;

/// The number of 32-bit words one Philox4x32 round produces, which is why a lane is `0..=3`.
const PHILOX_WORDS: usize = 4;

/// Ten rounds of Philox4x32 (spec 14.9's `philox4x32_10`).
///
/// `key` is 14.9's `[u64; 2]`. Only the low 32 bits of each word are subkeys, because that
/// is the primitive: see the module comment for why the derivation makes that narrowing
/// harmless rather than lossy. The high halves cannot influence any output — the subkey
/// bump is wrapping 32-bit addition, and addition carries upward only — so they are
/// deliberately zeroed here rather than mixed in, and
/// [`tests::the_unused_high_halves_of_the_key_are_inert_by_construction`] asserts that
/// rather than leaving it as a claim.
#[must_use]
pub fn philox4x32_10(counter: [u32; 4], key: [u64; 2]) -> [u32; 4] {
    let mut state = counter;
    let mut subkey_0 = key[0] as u32;
    let mut subkey_1 = key[1] as u32;
    for _ in 0..PHILOX_ROUNDS {
        let (high_0, low_0) = mulhilo32(PHILOX_MULTIPLIER_0, state[0]);
        let (high_1, low_1) = mulhilo32(PHILOX_MULTIPLIER_1, state[2]);
        state = [
            high_1 ^ state[1] ^ subkey_0,
            low_1,
            high_0 ^ state[3] ^ subkey_1,
            low_0,
        ];
        subkey_0 = subkey_0.wrapping_add(PHILOX_SUBKEY_BUMP_0);
        subkey_1 = subkey_1.wrapping_add(PHILOX_SUBKEY_BUMP_1);
    }
    state
}

/// The 32-bit multiply whose full 64-bit product is split, as Random123's `mulhilo32`.
///
/// Widening to `u64` before multiplying is the whole of it; writing it as a named function
/// is so the two uses in the round read as one operation rather than as two half-expressions.
#[must_use]
const fn mulhilo32(a: u32, b: u32) -> (u32, u32) {
    let product = a as u64 * b as u64;
    ((product >> 32) as u32, product as u32)
}

/// The Philox subkeys this stream resolves to: 14.7's 128-bit derived key, folded to the
/// two 32-bit subkeys the primitive accepts.
///
/// The pre-image is 14.7's list in 14.7's order under the domain tag 14.7 names literally,
/// built by [`StreamKey::canonical_bytes`]. The fold is described in the module comment.
#[must_use]
pub fn philox_key(stream: &StreamKey) -> [u64; 2] {
    let digest = stream.pre_image().digest();
    let word = |index: usize| {
        u32::from_le_bytes([
            digest[index * 4],
            digest[index * 4 + 1],
            digest[index * 4 + 2],
            digest[index * 4 + 3],
        ])
    };
    // Little-endian words 0..4 of the 128-bit key, which is 14.7's "split into two
    // little-endian u64" read as the split it describes; the XOR pairs the halves of each
    // u64 together so the discarded-by-the-primitive halves still count.
    let folded_0 = word(0) ^ word(1);
    let folded_1 = word(2) ^ word(3);
    [u64::from(folded_0), u64::from(folded_1)]
}

/// The derived subkeys of a stream key, spelled out so the test below compares the
/// derivation rather than restating it. Test-only: the production path is [`philox_key`].
#[cfg(test)]
#[must_use]
fn anim_sim_key_version(stream: &StreamKey) -> [u32; 2] {
    let key = philox_key(stream);
    [key[0] as u32, key[1] as u32]
}

/// The four counter words 14.7 specifies: `(tick_low, tick_high, draw_index, 0)`.
///
/// The fourth word is a literal zero, which is worth saying out loud: Philox4x32 has 128
/// counter bits and this engine addresses 96 of them, so two draws that agree on tick and
/// draw index cannot be separated by anything else. That is acceptable precisely because
/// `draw_index` is a semantic channel ordinal rather than a counter that needs room to
/// grow, and ADR-0008 is where that choice is defended.
///
/// # Errors
///
/// [`RandomError::NegativeTick`] for a tick below zero.
fn counter_words(tick: crate::SimTick, draw: u32) -> std::result::Result<[u32; 4], RandomError> {
    let (low, high) = tick.counter_words()?;
    Ok([low, high, draw, 0])
}

/// One 32-bit draw from one stream address (spec 14.9).
///
/// # Errors
///
/// [`RandomError::LaneOutOfRange`] above lane 3, [`RandomError::NegativeTick`] below tick
/// zero, and [`RandomError::UnsupportedAlgorithm`] if [`PHILOX_ALGORITHM_VERSION`] ever
/// names a permutation this function does not implement.
pub fn random_u32(
    key: &StreamKey,
    tick: crate::SimTick,
    draw: u32,
    lane: u8,
) -> std::result::Result<u32, RandomError> {
    if lane as usize >= PHILOX_WORDS {
        return Err(RandomError::LaneOutOfRange { lane });
    }
    let counter = counter_words(tick, draw)?;
    // No version gate here, deliberately. `stream_version` is one of the five terms the key is
    // derived from -- that is what makes a bumped version change this stream's numbers -- and
    // 14.12's "changing one stream version must leave unrelated stream outputs unchanged"
    // needs it to be free to differ. The value that says *which permutation* is in force is
    // `AlgorithmVersions::philox`, and it is read off persisted data at
    // `crate::checkpoint::restore_checkpoint`.
    Ok(philox4x32_10(counter, philox_key(key))[lane as usize])
}

/// One draw converted to `[0,1)` in `f32` (spec 14.7, 14.9).
///
/// 14.7 states the conversion exactly — "using the upper 24 bits divided by `2^24`, yielding
/// `[0,1)`" — and it is taken as a bit operation: the word shifts right by eight and the
/// low eight bits are masked off, then the integer is divided by the exact power of two.
/// Doing it in integer arithmetic rather than by multiplying by a reciprocal keeps the
/// rounding question out of the one place the spec pins the bits down. The result never
/// reaches 1.0, so a caller may treat the interval as half-open without a clamp.
///
/// # Errors
///
/// As [`random_u32`].
pub fn random_f32(
    key: &StreamKey,
    tick: crate::SimTick,
    draw: u32,
    lane: u8,
) -> std::result::Result<f32, RandomError> {
    let word = random_u32(key, tick, draw, lane)?;
    Ok(((word >> 8) & 0x00ff_ffff) as f32 / (1u32 << 24) as f32)
}

/// The subsystem drawing, 14.7's "system ID".
///
/// Two values, because WP05 has two subsystems that draw. A later package adds its own
/// number here rather than reusing one, and a reuse would merge two systems' streams into
/// identical sequences while both looked independent in every local test.
pub mod system {
    /// [`crate::particles`].
    pub const PARTICLES: u16 = 1;
    /// [`crate::springs`].
    pub const SPRINGS: u16 = 2;
}

/// The semantic channel a draw belongs to, 14.7's "stream purpose".
///
/// A fixed table of numbers, not a per-call string, because 14.7's "fixed semantic channel
/// tables" is what makes a draw reproducible from the address alone. The names follow the
/// attribute list spec 29 gives for particle streams (`position, direction, life, size,
/// color, subframe`) so that WP14 extends this table instead of renaming it; the values are
/// append-only, and a purpose that has been written into a saved project's history never
/// changes number.
pub mod purpose {
    /// The three direction components, drawn over lanes 0..2 of one address.
    pub const PARTICLE_DIRECTION: u16 = 1;
    /// Lifetime, in ticks, scaled by [`crate::particles::lifetime_range`].
    pub const PARTICLE_LIFETIME: u16 = 2;
    /// Speed along the drawn direction.
    pub const PARTICLE_SPEED: u16 = 3;
    /// Size, in the scene's length unit.
    pub const PARTICLE_SIZE: u16 = 4;
    /// Rotation, in turns.
    pub const PARTICLE_ROTATION: u16 = 5;
    /// The subframe phase of a repeated emission.
    pub const PARTICLE_SUBFRAME: u16 = 6;
    /// The initial offset of a spring body from its authored anchor.
    pub const SPRING_INITIAL_OFFSET: u16 = 7;
}

/// The stream version every stream this crate creates starts at.
///
/// 14.12's last clause — "changing one stream version must leave unrelated stream outputs
/// unchanged" — is a statement about *per-stream* versions, so the field exists on
/// [`StreamKey`] and is not folded into [`PHILOX_ALGORITHM_VERSION`]. The two are
/// deliberately not equal-by-definition either: this one is a channel's own revision and a
/// consumer bumps it alone, and the algorithm version is global and moves every stream at
/// once. A test that conflated them would have caught the gate this package first wrote and
/// then removed, and `one_streams_version_move_does_not_touch_another` is where it stays
/// removed.
pub const STREAM_VERSION: u32 = 1;

/// A stream key for one subsystem, entity, channel and version (spec 14.7, 14.8).
#[must_use]
pub fn stream_key(
    project_seed: [u8; anim_project::PROJECT_SEED_BYTES],
    system_id: u16,
    entity_id: anim_core::id::NodeId,
    purpose: u16,
    stream_version: u32,
) -> StreamKey {
    StreamKey {
        project_seed,
        system_id,
        entity_id,
        purpose,
        stream_version,
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
    use crate::SimTick;
    use anim_core::id::NodeId;

    /// The published known-answer vectors, read from the repository.
    ///
    /// Read from a file rather than transcribed into this test because the file carries its
    /// own provenance — the Random123 commit and the SHA-256 of each fetched vector file —
    /// and 14.5 step 2 asks for *published* vectors. A transcription would be a place for a
    /// digit to change quietly.
    fn official_vectors() -> serde_json::Value {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir)
            .join("tests")
            .join("fixtures")
            .join("philox4x32_10_test_vectors.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is a versioned input: {e}", path.display()));
        serde_json::from_str(&text).expect("the fixture is JSON")
    }

    fn word(value: &serde_json::Value) -> u32 {
        value.as_u64().expect("words are unsigned integers") as u32
    }

    #[test]
    fn every_published_vector_matches_word_for_word() {
        let fixture = official_vectors();
        assert_eq!(
            fixture["algorithm"].as_str(),
            Some("Philox4x32-10"),
            "the fixture must be the ten-round variant; the same source files also carry \
             philox4x32-7 rows, and a seven-round implementation would pass no row here"
        );
        let vectors = fixture["vectors"].as_array().unwrap();
        assert!(
            vectors.len() >= 50,
            "the fetched Random123 KAT files publish 55 ten-round 4x32 rows; a truncated \
             fixture would weaken this to a spot check ({}",
            vectors.len()
        );
        for vector in vectors {
            let counter = vector["counter"].as_array().unwrap();
            let key = vector["key"].as_array().unwrap();
            let expected = vector["expected"].as_array().unwrap();
            let input = [
                word(&counter[0]),
                word(&counter[1]),
                word(&counter[2]),
                word(&counter[3]),
            ];
            // Positional zero-extension, which is the derivation the fixture's own notes
            // record for spec 14.9's [u64; 2] container.
            let input_key = [u64::from(word(&key[0])), u64::from(word(&key[1]))];
            let got = philox4x32_10(input, input_key);
            let want = [
                word(&expected[0]),
                word(&expected[1]),
                word(&expected[2]),
                word(&expected[3]),
            ];
            assert_eq!(
                got,
                want,
                "line {:?} => got {got:?}, want {want:?}",
                vector["source_line"].as_str().unwrap_or("<none>")
            );
        }
    }

    #[test]
    fn the_unused_high_halves_of_the_key_are_inert_by_construction() {
        // Asserting the limitation rather than hiding it. Philox4x32 accepts two 32-bit
        // subkeys, so these bits cannot matter -- and `philox_key` is written so that no
        // derived bit lands only there. If this test ever starts failing, the round
        // function has begun reading a wider key than the published vectors describe.
        let counter = [7u32, 0, 11, 0];
        let narrow = philox4x32_10(counter, [0x1234_5678, 0x9abc_def0]);
        let padded = philox4x32_10(counter, [0xdead_beef_1234_5678, 0xcafe_f00d_9abc_def0]);
        assert_eq!(narrow, padded);
    }

    #[test]
    fn a_stream_key_derivation_covers_all_128_derived_bits() {
        // The fold's whole purpose, so it gets its own proof: two StreamKeys that differ
        // only where the primitive would not look must still be different addresses.
        let node = NodeId::derive(
            &uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            b"emitter",
        );
        let base = stream_key(
            [0u8; anim_project::PROJECT_SEED_BYTES],
            1,
            node,
            1,
            STREAM_VERSION,
        );
        // Every byte position of the 128-bit derived key has to be able to change a draw.
        // The fold pairs bytes 0..4 with 4..8 and 8..12 with 12..16, so the pairs are
        // probed together and the rest of the seed is left at a fixed nonzero value so the
        // digest input actually differs.
        for pair in [0usize, 8] {
            let mut seed = [0u8; anim_project::PROJECT_SEED_BYTES];
            seed[pair] = 1;
            let changed = stream_key(seed, 1, node, 1, STREAM_VERSION);
            assert_ne!(
                philox_key(&base),
                philox_key(&changed),
                "byte {pair} of the seed cannot be seen by the stream: the fold has \
                 collapsed two of the four derived words onto the same subkey"
            );
        }
        // And the dead-by-construction halves: flipping a byte the primitive never reads
        // still changes the folded subkey, which is the property the fold exists to buy.
        let mut seed = [0u8; anim_project::PROJECT_SEED_BYTES];
        seed[4] = 0xff;
        let flipped = stream_key(seed, 1, node, 1, STREAM_VERSION);
        assert_ne!(
            random_u32(&base, SimTick(0), 0, 0).unwrap(),
            random_u32(&flipped, SimTick(0), 0, 0).unwrap(),
            "seed byte 4 sits in the high half of derived word 1, which Philox4x32 would \
             discard unread without the fold"
        );
    }

    #[test]
    fn a_draw_is_a_pure_function_of_its_address() {
        let node = NodeId::derive(
            &uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            b"emitter",
        );
        let key = stream_key(
            [3u8; anim_project::PROJECT_SEED_BYTES],
            1,
            node,
            1,
            STREAM_VERSION,
        );
        let first = random_u32(&key, SimTick(97), 4, 2).unwrap();
        for _ in 0..500 {
            // Draws anywhere else, in any order, then the same address again.
            let _ = random_u32(&key, SimTick(1000), 17, 3).unwrap();
            let _ = random_f32(&key, SimTick(3), 0, 0).unwrap();
            assert_eq!(
                random_u32(&key, SimTick(97), 4, 2).unwrap(),
                first,
                "there is no cursor to advance: an address asked twice is one number"
            );
        }
    }

    #[test]
    fn every_lane_of_one_address_is_a_different_number() {
        let node = NodeId::derive(
            &uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            b"lanes",
        );
        let key = stream_key(
            [9u8; anim_project::PROJECT_SEED_BYTES],
            1,
            node,
            3,
            STREAM_VERSION,
        );
        let lanes: Vec<u32> = (0..4)
            .map(|lane| random_u32(&key, SimTick(5), 1, lane).unwrap())
            .collect();
        for pair in lanes.windows(2) {
            assert_ne!(
                pair[0], pair[1],
                "one Philox round yields four words: {lanes:?}"
            );
        }
        for lane in [4u8, 5, 255] {
            assert!(
                matches!(
                    random_u32(&key, SimTick(5), 1, lane),
                    Err(RandomError::LaneOutOfRange { .. })
                ),
                "14.7 says a lane above three is an error, not a wrap"
            );
        }
    }

    #[test]
    fn the_f32_conversion_is_the_upper_24_bits_over_2_pow_24() {
        let node = NodeId::derive(
            &uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            b"fraction",
        );
        let key = stream_key(
            [1u8; anim_project::PROJECT_SEED_BYTES],
            1,
            node,
            2,
            STREAM_VERSION,
        );
        for tick in 0..2000i64 {
            for lane in 0..4u8 {
                let word = random_u32(&key, SimTick(tick), 0, lane).unwrap();
                let value = random_f32(&key, SimTick(tick), 0, lane).unwrap();
                assert_eq!(
                    value,
                    ((word >> 8) & 0x00ff_ffff) as f32 / (1u32 << 24) as f32,
                    "the spec's sentence, spelled once"
                );
                assert!(
                    (0.0..1.0).contains(&value),
                    "[0,1) with the upper bound unreachable: {value}"
                );
            }
        }
    }

    #[test]
    fn a_negative_tick_is_an_error_and_a_foreign_stream_version_is_an_address() {
        let node = NodeId::derive(
            &uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            b"strict",
        );
        let key = stream_key(
            [0u8; anim_project::PROJECT_SEED_BYTES],
            1,
            node,
            1,
            STREAM_VERSION,
        );
        assert!(matches!(
            random_u32(&key, SimTick(-1), 0, 0),
            Err(RandomError::NegativeTick { tick: -1 })
        ));
        // A stream version this build never creates is drawn from, not refused: it is one of
        // the five terms the key is derived from, so it changes the numbers rather than
        // invalidating the call. Refusing it would make 14.12's per-stream-version criterion
        // untestable -- the bumped stream would simply be unusable.
        let bumped = StreamKey {
            stream_version: STREAM_VERSION + 1,
            ..key
        };
        let before = random_u32(&key, SimTick(0), 0, 0).unwrap();
        let after = random_u32(&bumped, SimTick(0), 0, 0).unwrap();
        assert_ne!(before, after, "the version is in the address");
        assert_ne!(
            anim_sim_key_version(&key),
            anim_sim_key_version(&bumped),
            "and it reaches the derived subkeys, which is where an address term has to land"
        );
    }

    #[test]
    fn the_counter_is_the_address_and_the_fourth_word_stays_zero() {
        let node = NodeId::derive(
            &uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            b"counter",
        );
        let key = stream_key(
            [5u8; anim_project::PROJECT_SEED_BYTES],
            1,
            node,
            1,
            STREAM_VERSION,
        );
        let philox_key_words = philox_key(&key);
        // Directly against the permutation, so an address rule is tested rather than a
        // wrapper agreeing with itself.
        assert_eq!(
            random_u32(&key, SimTick(1), 7, 0).unwrap(),
            philox4x32_10([1, 0, 7, 0], philox_key_words)[0],
            "counter words are (tick_low, tick_high, draw_index, 0) exactly as 14.7 states"
        );
        assert_eq!(
            random_u32(&key, SimTick(i64::from(u32::MAX) + 1), 2, 3).unwrap(),
            philox4x32_10([0, 1, 2, 0], philox_key_words)[3],
            "and a tick that exceeds 32 bits continues into the second word, with the draw \
             index in the third and nothing in the fourth"
        );
    }
}
