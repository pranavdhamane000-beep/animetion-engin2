# ADR-0004 — Rational serialization and arithmetic limits

Status: accepted
Date: 2026-11-05
Work package: WP01 (spec register: "ADR-0004 · Rational serialization and arithmetic limits · WP01")
Evidence: `crates/anim_core/tests/time_properties.rs` (28 tests), `crates/anim_core/src/{rational,time,id,hash}.rs`
unit tests (48 tests), gate G1. Spec 10.6, 10.7, 10.8, 10.11, 10.12.

## Problem

Every time-bearing value in the engine — keyframe positions, event times, frame
ranges, audio sample boundaries, simulation ticks, persisted document keys — has to
be one number system, or the pipeline disagrees with itself at NTSC rates. Spec 10.2
makes the requirement absolute: NTSC is a rational, never a decimal approximation.
That leaves two decisions the spec does not make for us, and both are load-bearing
for every package after this one:

1. **What a rational looks like on disk.** Spec 10.5 says "canonical `num/den`
   serialization" without fixing whether that is one string, an object of two
   integers, or two adjacent array elements.
2. **How wide the arithmetic is, and where it stops.** Spec 10.8 fixes the persisted
   fields at `i128`, and 10.7 requires arbitrary-precision intermediates "followed by
   checked conversion", but does not say whether the width check happens before or
   after reduction — which is the difference between `MAX/2 + MAX/2` working and
   failing.

Both choices are visible in saved files forever, so spec 1.2's rule ("semantics,
ordering, units and failure behaviour MUST NOT change without an ADR") applies here
before anywhere else in the build.

## Alternatives considered

**Serialization.**

- **`{"num": -24000, "den": 1001}`** — an object of two integers. Rejected: it lets a
  non-canonical pair exist on disk, because the reducer becomes a loader behaviour
  rather than a type invariant, and JSON gives no way to tell "as stored" from "as
  meant". It also costs more bytes than the string in the place they matter most: a
  project file with tens of thousands of keyframe keys.
- **`[-24000, 1001]`** — a two-element array. Rejected: field identity is positional,
  so a schema edit that reorders two numbers silently redefines every rational in
  every existing file.
- **A single decimal string, `"-23.976"`** — rejected outright. It is the thing spec
  10.2 forbids, and 23.976 is not 24000/1001: at 48 kHz the difference is roughly
  0.4 samples per frame, which is audio drift with a smile on its face.
- **`"-24000/1001"`** — chosen. One field, one value, self-delimiting, and readable
  in a log without a schema.

**Parsing strictness.**

- **Accept any equivalent spelling** (`"10/20"`, `"+1/2"`, `"05/1"`, `"5"`) and
  normalize on read. Rejected: two byte streams would describe one value, so a saved
  document would stop matching the hash of its own bytes, and WP04's "identical
  canonical snapshots from two fresh processes" (G3) would depend on which process
  happened to write first.
- **Round-trip check** — parse, normalize, re-render, and require the text to be
  unchanged. Chosen: one line, and it makes the canonical form the only accepted
  form rather than a convention some loader remembers to apply.

**Width checking.**

- **Check the intermediate** (`num + num` before reducing). Rejected: it rejects
  valid arithmetic — `i128::MAX/2 + i128::MAX/2` produces `i128::MAX`, which fits
  perfectly well and is exactly what a long timeline's frame sum looks like.
- **Reduce, then check.** Chosen. `Rational::from_bigint_parts` is the single funnel:
  normalize sign, divide by the `BigInt` gcd, then `i128::try_from` both halves.
- **Widen the persisted fields.** Rejected without measuring: 10.8 specifies `i128`,
  and the first failure of a wider type would be every schema written after WP02.

**Domain separation for hashes.**

- **`blake3::derive_key` + keyed hash.** Rejected: the derived key is not part of the
  hashed bytes, so a report showing a digest cannot be checked by hand without
  reproducing the KDF. Auditability won.
- **Length-prefixed domain tag as the first field.** Chosen. Length prefixing makes
  it non-confusable with a payload and makes no domain a prefix of another, and the
  whole input stays visible in the hashed byte stream.

## Decision

1. **Wire form.** `Rational` serializes as one string, always both halves, always
   reduced, always with a positive denominator, and zero exactly `"0/1"`. Parsing is
   strict by round-trip; a non-canonical spelling is a `MalformedCanonical` error.
2. **FrameRate keeps its two integers.** `num`/`den` are `u32` fields persisted as an
   object, because spec 10.8 lists them as public and FFmpeg receives them as a pair
   (spec 18.7). Reduction happens at use, in `as_rational`, not at construction — so
   `24/1` and a hypothetical `48/2` cannot disagree.
3. **Arithmetic.** Intermediates are `BigInt`; reduction precedes the width check;
   failure is `ArithmeticOverflow` carrying the rejected value. Nothing wraps and
   nothing clamps (spec 10.11).
4. **Canonical bytes.** Little-endian fixed widths for integers; the bit pattern of a
   finite `f64`, non-finite refused; `u64` length prefixes on all variable-length
   items; `Rational` as its canonical text; UUIDs as 16 raw bytes; the domain tag
   first and length-prefixed. Callers sort; the encoder never does.
5. **IDs.** Canonical text is 36-character lowercase hyphenated, and parsing refuses
   every other spelling `uuid` would accept. Order is over the 16 bytes, spelled out
   in `anim_core` rather than inherited from `Uuid`'s derived `Ord`. Creation is
   `derive(namespace, name)` over v5 — no entropy source, per DECISIONS.md #23.
6. **Revision** is a checked `u64` counter: zero is a valid new document and
   increment cannot wrap.

## Evidence

- `normalization_is_unique_and_never_wraps`, `arithmetic_matches_the_reference`,
  `floor_and_ceil_follow_the_mathematical_definitions`, `ordering_agrees_with_cross_multiplication`,
  `canonical_text_round_trips` — 4096 generated cases each, every expected value
  computed by this file's own `BigInt` gcd, floor and truncation correction, using
  comparisons only so the oracle does not import the traits under test.
- `ten_thousand_adjacent_frames_partition_the_sample_axis` and
  `each_sample_is_claimed_by_exactly_one_frame` — spec 10.12's named requirement, at
  24000/1001 and 30000/1001, plus the same check by enumeration over ~96,000 samples.
- `long_timelines_do_not_accumulate_error` — frame numbers to 8e12 exact against the
  reference, and the sample axis failing at frame ~5.78e18 while the timeline still
  round-trips the same frame. That asymmetry is the reason 18.5's rejection of
  negative and out-of-range samples is a boundary decision, not a type decision.
- `a_saved_document_loads_back_with_the_same_ids_and_hashes`,
  `canonical_order_does_not_depend_on_insertion_order` — the persistence half:
  byte-identical re-serialization and identical digests after a load.
- Two defects were found by these tests while this ADR was being written, which is
  the evidence that the method works: `tick_duration` computed `1/(num*N)` instead of
  `den/(num*N)`, a 1001× error in every simulation step size WP05 would take, and
  `to_i64` reported `TryFromBigIntError`'s text instead of the rejected value, which
  would have made an overflow diagnostic useless.

## Consequences

1. **A saved file is its own proof.** Because only the canonical spelling parses, a
   document that hashes equal to a recorded digest really is that document. WP03's
   recovery scan and WP11's benchmark identity can rely on this instead of
   re-normalizing first.
2. **Reduction is a type invariant, not a loader behaviour.** No package after WP01
   has to decide whether to reduce; if it compiled, it is reduced. This is also why
   `Rational`'s fields are private and `FramePos::subframe` is too (DECISIONS.md #22).
3. **`i128` is the ceiling, and it is now a documented one.** Callers must handle
   `ArithmeticOverflow` at their boundaries; WP08's non-negative `SampleOffset`
   wrappers (spec 17.5) wrap these calculations rather than repeat them.
4. **A `Rational` field in any later schema is one string.** WP02's manifest, WP03's
   document schemas and WP11's golden comparisons should expect a string and refuse
   an object; a schema that accepts both would reintroduce the two-spellings problem
   one layer up.
5. **Domain strings are convention, not registry.** The shape is
   `anim_engine/<area>/v<n>`; a later package adding a domain cannot register it
   anywhere, so the ADR register is the list. If a hash is ever published without its
   domain, this is unresolvable — so every digest reported by a probe must name its
   domain in the same record.
6. **`derive(Ord)` on `Uuid` is not trusted.** `uuid` is caret-pinned, so an
   in-range minor bump can change its internal representation; ordering is
   implemented locally, and `uuid_text_round_trips_and_order_is_byte_order` is the
   test that would notice if the two ever diverged.
7. **The 10.8 listing and 10.8's closing rule disagreed about `pub subframe`.** The
   rule won. Spec 20's traceability matrix should record the field as private if the
   document is ever regenerated (DECISIONS.md #22).

## Schema/version impact

New persisted types, no existing schema changes: this is the first package that
writes anything. `Rational` is `"num/den"`, `FramePos` is `{"frame": i64, "subframe":
"num/den"}`, `FrameRate` is `{"num": u32, "den": u32}`, typed IDs are 36-character
lowercase strings, `Revision` is a JSON integer. All are versionless value types, so
**any change to these spellings is a schema-version event**, and canonical byte
encodings that embed them inherit that requirement.

ADR-0005 ("Canonical project and manifest encoding", register: WP01–WP03) is **opened
but not closed by this package**. The value-encoding rules in Decision 4 are its
foundation; the document-level questions — how an object's fields are ordered, how the
schema version is carried, what the manifest's record framing is — belong to WP02 and
WP03, where the records exist. Recording them here would be deciding WP03's schema
from WP01's file listing.
