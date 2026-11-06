# ADR-0008 — Philox key/counter layout and algorithm versions

Status: **accepted.** Written as WP05's deliverable on 2026-11-06. One clause of the key
layout turned out to contradict the published primitive it names, and Decision 3 below is the
resolution; it is a project decision, not a spec correction, and it is the reason this ADR
exists rather than a comment in `random.rs`.

Date: 2026-11-06
Work package: WP05 (spec register: "ADR-0008 · Philox key/counter layout and algorithm
versions · WP05")
Evidence: `crates/anim_sim/src/random.rs`, `crates/anim_sim/src/lib.rs` (`StreamKey`,
`PHILOX_ALGORITHM_VERSION`, `SIMULATION_MODEL_VERSION`, `CHECKPOINT_FORMAT_VERSION`,
`RandomError`), `crates/anim_sim/src/state.rs` (`AlgorithmVersions`, `SimState::encode`),
`crates/anim_sim/src/checkpoint.rs` (`restore_checkpoint`),
`crates/anim_sim/tests/fixtures/philox4x32_10_test_vectors.json`,
`crates/anim_sim/tests/fixtures/PHILOX_VECTORS_NOTE.md`,
`crates/anim_sim/tests/determinism.rs`
(`the_permutation_reproduces_every_published_known_answer`,
`the_stream_key_derivation_is_blake3_over_the_image_14_7_describes`,
`one_streams_version_move_does_not_touch_another`).
Spec 14.7, 14.8, 14.9, 14.11, 14.12. Continues ADR-0004 (canonical pre-images and domain
tags) and ADR-0005 Decision 4 (one version number per thing that changes).

## Problem

Spec 14.7 defines the whole random path in one paragraph:

> **Philox random access.** Input: `StreamKey`, absolute tick, semantic `draw_index`, and
> lane `0..3`. Derive a 128-bit key from BLAKE3 over domain `"anim_engine/philox/v1"`,
> project seed, system ID, emitter/entity ID, stream purpose and stream version. Split into
> two little-endian u64 Philox keys. Counter words are `(tick_low, tick_high, draw_index, 0)`.
> Execute ten Philox4x32 rounds and select the lane. Convert to f32 using the upper 24 bits
> divided by 2^24, yielding [0,1). Draw indices come from fixed semantic channel tables and
> deterministic spawn/particle ordinals, never a "next random" cursor. Lane above three,
> negative tick or overflow returns an error.

Five questions are open in it, and every one lands in a saved digest:

1. **Which Philox.** "Ten Philox4x32 rounds" names Philox4x32-10, a specific published
   permutation with specific multipliers, specific subkey bumps and a specific round
   function. None of those four appear in the spec. Implemented from a recollection, a wrong
   constant is self-consistent forever: every internal test passes and the numbers are not
   Philox's.
2. **How wide the key is.** 14.7 derives 128 bits and splits them into two `u64`.
   Philox4x32's key is **64 bits**: Random123 documents it as half as wide as the counter,
   `philox4x32_key_t` is a two-word `u32` array, and the round consumes exactly those two
   words. So a literal reading feeds the primitive half of the derived key and discards the
   other half silently.
3. **What the counter's fourth word is for.** 14.7 fixes it at zero. Philox4x32 has 128
   counter bits and this engine addresses 96 of them. Nothing in the spec says why that is
   acceptable.
4. **Which version numbers exist, and where each is checked.** 14.8's `Checkpoint` carries
   `algorithm_versions: AlgorithmVersions`, a type the spec names and never gives fields.
   14.8's `StreamKey` carries `stream_version: u32`. 14.11 demands an error for an
   "unsupported random algorithm". Three different version-ish nouns and one sentence about
   what each is for.
5. **What "overflow returns an error" refers to**, given a signed `i64` tick split into two
   `u32` words always fits.

## Decisions

1. **`philox4x32_10` is Random123's Philox4x32 with ten rounds, and it is certified by the
   published known-answer vectors rather than by this repository.**
   `crates/anim_sim/tests/fixtures/philox4x32_10_test_vectors.json` vendors 55 rows from D. E.
   Shaw Research's own `tests/kat_vectors` and `tests/old_kat_vectors`, pinned to commit
   `9545ff64…`, with the SHA-256 of each fetched file recorded in the fixture and each row
   quoting its verbatim source line. Multipliers `0xD2511F53` / `0xCD9E8D57`, subkey bumps
   `0x9E3779B9` / `0xBB67AE85`, round function `out = [hi1 ⊕ c1 ⊕ k0, lo1, hi0 ⊕ c3 ⊕ k1, lo0]`,
   wrapping 32-bit subkey increment per round, ten rounds.
   The discrimination measured against those vectors is worth keeping in the record, because
   it shows what the fixture does *not* forgive: seven rounds → 0 of 55 reproduced; eleven
   rounds → 0; outputs reversed → 0; multipliers swapped → 0; bumps swapped → 0; bump before
   every round → 0; counter reversed → 2; key words swapped → 2.
   **Rejected:** implementing from memory and testing the layout against itself. An oracle
   that calls the code under test proves only self-consistency, which is the rule
   DECISIONS.md #32 records and which found four defects in WP03 and five in WP02.

2. **14.9's `key: [u64; 2]` container is kept exactly, and only its low 32 bits per word are
   subkeys, because that is the primitive.** `tests::the_unused_high_halves_of_the_key_are_
   inert_by_construction` asserts the deadness rather than leaving it implied — the high
   halves cannot influence an output, since a wrapping 32-bit add carries upward only.

3. **The 128-bit derived key is folded into the 64 key bits Philox4x32 accepts: subkey 0 is
   derived word 0 XOR word 1, subkey 1 is word 2 XOR word 3.**
   **Rejected:** the literal split, in which subkey 0 is word 0 and subkey 1 is word 2, which
   is what 14.7's sentence reads as. Under that reading, 64 of the 128 derived bits are
   unreachable, and two `StreamKey`s differing only in a dead half draw **identical**
   sequences. The purpose of putting the project seed, the system, the entity, the purpose and
   the version into a hash is that all five should matter; a layout that silently makes half
   of the hash inert defeats the sentence it implements, and it does so in the quiet direction
   — the streams look independent in every local test.
   The fold costs entropy, and the ceiling is the primitive's: 64 key bits is 64 key bits, so
   the choice is between spending all 128 derived bits on those 64 or discarding half of them.
   **Consequence:** the vectors cannot detect a defect that lives only in the dead high halves,
   because every published key has zeros there. That limitation is stated in the fixture's own
   `notes` field and in `PHILOX_VECTORS_NOTE.md` rather than being argued away. If the engine
   ever genuinely wants a 128-bit-key Philox4x32, that is a different, unpublished primitive
   and no published vectors exist for it — a spec change, not a decision in this package.

4. **Counter words are `(tick_low, tick_high, draw_index, 0)` with the fourth word a literal
   zero, and the choice is written down rather than left to be discovered.** 96 of the 128
   counter bits are addressed. That is sound because `draw_index` is a semantic channel
   ordinal and not a counter that has to grow, and it is stated because a caller that wanted
   to use word 4 would otherwise be reading a value nothing derives.
   `tick_low` is filled first — the little-endian split — and
   `SimTick::counter_words` refuses a negative tick instead of casting one. A silently
   wrapped negative would be a valid-looking address only this build's wrapping rule
   reproduces, which is the loud-over-quiet argument DECISIONS.md #18 makes about adapter
   identity applied to a counter word.

5. **Three version numbers, one per thing whose reading can change without any data
   changing.** `AlgorithmVersions { philox, model, checkpoint_format }` is the type 14.8 names
   without fields:
   - `PHILOX_ALGORITHM_VERSION = 1` — the permutation, the key fold of Decision 3, and the
     counter layout of Decision 4. Bumping it says every random value this project ever
     produced came from a different function.
   - `SIMULATION_MODEL_VERSION = 1` — the stage order in `step_once`, the spawn arithmetic,
     the force functions and the constants in `particles.rs` and `springs.rs`. One number
     rather than one per stage, because the stages are not independently substitutable:
     moving a lifetime update after removal, or changing gravity, changes what the other
     stages see.
   - `CHECKPOINT_FORMAT_VERSION = 1` — the byte layout that wraps a state.
   All three are encoded in every state digest, which is the WP04 precedent
   (`EASING_ALGORITHM_VERSION` in snapshots, deliberately not in documents): a field addition
   is a data event and belongs in `PROJECT_SCHEMA_VERSION`; a changed **reading** is invisible
   in the data and must be visible in the digest. ADR-0005 Decision 4's separation of the
   document's, the manifest's and the envelope's version numbers extends to these three
   rather than merging into them.

6. **`StreamKey.stream_version` is a per-stream address term and never an algorithm gate.**
   `random_u32` does not compare it to `PHILOX_ALGORITHM_VERSION`. The first implementation
   did, and `one_streams_version_move_does_not_touch_another` failed it — not because
   unrelated streams moved, but because the bumped stream became **unusable**:
   `UnsupportedAlgorithm` on every draw. 14.12's "changing one stream version must leave
   unrelated stream outputs unchanged" therefore needs the version to be drawable at any
   value; a gate makes the criterion untestable by construction.
   `STREAM_VERSION = 1` is the value this crate's own streams start at, and it is deliberately
   not defined as `PHILOX_ALGORITHM_VERSION`.

7. **14.11's "unsupported random algorithm" refusal is raised at the checkpoint boundary,
   where a version arrives from data.** `restore_checkpoint` compares
   `algorithm_versions.philox` against this build's and returns
   `SimError::UnsupportedRandomAlgorithm` if they differ — kept distinct from
   `CheckpointHashMismatch` because the remedies differ: a mismatch means *this entry* is
   damaged and one more entry is tried, a foreign Philox means the *build* no longer speaks
   the algorithm the entry was written in, and the answer is to reconstruct from tick zero.
   `RandomError::UnsupportedAlgorithm` was **removed** rather than left in place, because
   nothing in a `StreamKey` identifies an algorithm and an unreachable variant reads, in a
   later audit, like covered error handling — the failure mode `BUILD_STATE.md` risk 19 names.

8. **`random_f32` is the upper 24 bits of the selected lane's word divided by 2^24**, as 14.7
   states, taken as an integer shift-and-mask over an exact power of two. The interval is
   `[0,1)` with 1.0 unreachable, so a consumer may scale without clamping.

9. **`f32` is the working type, and the sim core uses `+ - * /` and `sqrt` only.** 14.8's
   particle and body fields are `f32`, so the state is `f32` and step durations convert to it.
   No trigonometry appears anywhere in WP05: `BUILD_STATE.md` risk 20 records that
   `f64::sin_cos` is a `libm` call whose bits depend on a library the spec does not pin,
   whereas IEEE-754 requires correctly-rounded binary32 `sqrt`. A particle's rotation is
   stored as an angle and never turned into a matrix in this crate.

## Consequences

1. Two processes on this build agree bit-for-bit, and now two **configurations** of the
   cache do too — the whole point of 14.12 — because a draw is a function of an address and
   not of a position in a sequence.
2. Every random value this project has ever produced is only reproducible with these vectors
   and this fold. Changing the fold is a `PHILOX_ALGORITHM_VERSION` bump and invalidates every
   checkpoint, which is the intended cost.
3. A 128-bit-key Philox4x32 would have no published vectors. If a later package wants one, it
   is raising a spec gap, not editing a constant here.
4. The `stream_version` rule means a consumer may bump its own channel without a global
   change; that is what WP14's `SetVfxIntensityCurve`-era parameter revisions will rely on,
   and it is why the field is per-`StreamKey` rather than a crate constant.
5. Removing `RandomError::UnsupportedAlgorithm` shrank the public enum. Nothing outside
   `anim_sim` referenced it in this build, so it is not a compatibility problem — and if it
   had been, the answer would still have been to delete an unreachable variant rather than
   keep a fake call site.
6. `SimError::UnsupportedRandomAlgorithm` now has two producers — a foreign `philox` version
   and, by chaining, nothing else — and `RandomError`'s three remaining variants each have a
   test that reaches them.
7. The subkey fold means a `StreamKey` differing in one byte always changes both subkeys with
   overwhelming probability, but the guarantee actually relied on is the 64-bit collision
   bound of the two subkeys plus the 96-bit counter address, not the 128-bit digest width.

## Schema and version impact

| Number | Before | After | Why |
| --- | --- | --- | --- |
| `PHILOX_ALGORITHM_VERSION` | (none) | 1 | new: the permutation, the fold and the counter layout |
| `SIMULATION_MODEL_VERSION` | (none) | 1 | new: the stepping model, one number for one interdependent set |
| `CHECKPOINT_FORMAT_VERSION` | (none) | 1 | new: the checkpoint byte layout |
| `StreamKey` | (none) | five fields per 14.8 | new type; `stream_version` is per-stream and not an algorithm gate |
| philox domain | (none) | `anim_engine/philox/v1` | 14.7 names the string literally; new entry in ADR-0004 consequence 5's registry |
| particle-id domain | (none) | `anim_engine/particle_id/v1` | new, same registry |
| spring-id domain | (none) | `anim_engine/spring_id/v1` | new, same registry |
| state domain | (none) | `anim_engine/sim_state/v1` | new, same registry |
| checkpoint domain | (none) | `anim_engine/checkpoint/v1` | new, same registry |
| `RandomError` | four variants | three | `UnsupportedAlgorithm` removed as unreachable; Decision 7 |
| `PROJECT_SCHEMA_VERSION` | 2 | 3 | **ADR-0009** decides this, not this ADR |

No `StreamKey` and no algorithm version is persisted in a project file. Every one of them is
derived at draw time from data that **is** persisted — the seed and the tick rate from
`ProjectDocument.simulation` (ADR-0009) — which is what makes a saved project's random streams
reproducible without storing any random value at all.
