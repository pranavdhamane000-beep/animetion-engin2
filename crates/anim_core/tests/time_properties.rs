//! Property and boundary verification for canonical time and stable identity
//! (spec 10.3, 10.12). This file is the certification for gate G1.
//!
//! Two rules govern how these tests are written.
//!
//! **References are computed independently.** Every expected value comes from
//! hand-rolled `BigInt` arithmetic in this file — its own gcd, its own floor, its
//! own truncation correction — using only comparisons, so that the oracle does not
//! lean on the same traits the implementation does. A test whose expected value is
//! produced by the code it tests proves only that the code is self-consistent,
//! which is exactly how a subtle determinism bug survives to WP11.
//!
//! **Rejection is part of the specification.** Spec 10.12 requires that all overflow
//! and invalid-rate tests fail explicitly, so the error cases are asserted as
//! carefully as the valid ones — including the property that an error is only
//! correct when the true value genuinely does not fit. A spurious rejection is as
//! much a defect as a spurious acceptance.
//!
//! Everything here uses the public API only. That is itself a check: if a private
//! invariant leaked, this file would have to reach around it to compile.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "G1's whole method is comparing against independently computed references, so an expected-to-hold conversion unwraps here rather than converting into proptest's boxed error type; the workspace denial still governs every production crate"
)]

use std::cmp::Ordering;
use std::collections::BTreeMap;

use anim_core::diagnostic::DiagnosticSeverity;
use anim_core::error::CoreError;
use anim_core::hash::CanonicalBytes;
use anim_core::id::{
    AssetId, CommandId, EventDefinitionId, NodeId, ProjectId, Revision, SceneId, TrackId,
};
use anim_core::rational::Rational;
use anim_core::time::{
    checked_frame_to_ticks, tick_duration, FramePos, FrameRange, FrameRate, SampleInterval,
};

use num_bigint::BigInt;
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Independent references
// ---------------------------------------------------------------------------

fn zero() -> BigInt {
    BigInt::from(0)
}

fn one() -> BigInt {
    BigInt::from(1)
}

fn is_zero(value: &BigInt) -> bool {
    *value == zero()
}

fn is_negative(value: &BigInt) -> bool {
    *value < zero()
}

fn magnitude(value: &BigInt) -> BigInt {
    if is_negative(value) {
        -value.clone()
    } else {
        value.clone()
    }
}

/// Euclid's algorithm, written here so `anim_core`'s gcd is not also the oracle.
fn ref_gcd(mut a: BigInt, mut b: BigInt) -> BigInt {
    while !is_zero(&b) {
        let t = a % &b;
        a = b;
        b = t;
    }
    magnitude(&a)
}

/// Floor division for a positive divisor, by correcting `BigInt`'s truncating `/`.
fn ref_floor(n: &BigInt, positive_den: &BigInt) -> BigInt {
    assert!(
        *positive_den > zero(),
        "references assume a positive divisor"
    );
    let q = n / positive_den;
    if is_negative(n) && !is_zero(&(n % positive_den)) {
        q - one()
    } else {
        q
    }
}

/// The normalized form of `num/den`: coprime pair, positive denominator, `0/1`.
fn ref_normalized(num: &BigInt, den: &BigInt) -> (BigInt, BigInt) {
    assert!(!is_zero(den), "zero denominators are tested separately");
    let (mut n, mut d) = if is_negative(den) {
        (-num.clone(), -den.clone())
    } else {
        (num.clone(), den.clone())
    };
    if is_zero(&n) {
        return (zero(), one());
    }
    let g = ref_gcd(magnitude(&n), magnitude(&d));
    n = &n / &g;
    d = &d / &g;
    (n, d)
}

fn bi(value: i128) -> BigInt {
    BigInt::from(value)
}

fn fits_i128(value: &BigInt) -> bool {
    *value >= bi(i128::MIN) && *value <= bi(i128::MAX)
}

fn fits_i64(value: &BigInt) -> bool {
    *value >= bi(i64::MIN as i128) && *value <= bi(i64::MAX as i128)
}

/// Every call site asserts the width fits first, so a panic here means the test's
/// own precondition broke rather than that a value was rejected.
fn to_i128(value: &BigInt) -> i128 {
    i128::try_from(value.clone()).unwrap()
}

fn as_reference(r: &Rational) -> (BigInt, BigInt) {
    (bi(r.num()), bi(r.den()))
}

/// Equality decided by cross-multiplication, not by the crate's `Ord`.
fn same_value(a: &Rational, b: &Rational) -> bool {
    bi(a.num()) * bi(b.den()) == bi(b.num()) * bi(a.den())
}

fn compare_reference(a: &Rational, b: &Rational) -> Ordering {
    (bi(a.num()) * bi(b.den())).cmp(&(bi(b.num()) * bi(a.den())))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

impl Op {
    fn apply(self, a: Rational, b: Rational) -> Result<Rational, CoreError> {
        match self {
            Op::Add => a.checked_add(b),
            Op::Sub => a.checked_sub(b),
            Op::Mul => a.checked_mul(b),
            Op::Div => a.checked_div(b),
        }
    }

    /// `a op b` reduced, in arbitrary precision. `None` only for division by zero,
    /// which has no value at all rather than an unrepresentable one.
    fn reference(self, a: &Rational, b: &Rational) -> Option<(BigInt, BigInt)> {
        let (an, ad) = as_reference(a);
        let (bn, bd) = as_reference(b);
        let (n, d) = match self {
            Op::Add => (an * bd.clone() + bn * ad.clone(), ad * bd),
            Op::Sub => (an * bd.clone() - bn * ad.clone(), ad * bd),
            Op::Mul => (an * bn, ad * bd),
            Op::Div => {
                if is_zero(&bn) {
                    return None;
                }
                (an * bd, ad * bn)
            }
        };
        Some(ref_normalized(&n, &d))
    }
}

/// Spec 10.7's whole-frame boundary: `floor(f * sr * fps_den / fps_num)`.
fn ref_sample_boundary(frame: i64, sample_rate: u32, num: u32, den: u32) -> BigInt {
    let n = bi(i128::from(frame)) * bi(i128::from(sample_rate)) * bi(i128::from(den));
    ref_floor(&n, &bi(i128::from(num)))
}

// ---------------------------------------------------------------------------
// Strategies and fixtures
// ---------------------------------------------------------------------------

fn rational_strategy() -> BoxedStrategy<Rational> {
    (any::<i64>(), 1i64..100_000)
        .prop_map(|(n, d)| Rational::new(i128::from(n), i128::from(d)).unwrap())
        .boxed()
}

/// Integer and NTSC rates alike, which spec 10.12 names together.
fn rates() -> Vec<FrameRate> {
    [
        (24u32, 1u32),
        (25, 1),
        (30, 1),
        (48, 1),
        (60, 1),
        (24_000, 1001),
        (30_000, 1001),
        (48_000, 1001),
        (60_000, 1001),
    ]
    .into_iter()
    .map(|(n, d)| FrameRate::new(n, d).unwrap())
    .collect()
}

fn rate_strategy() -> BoxedStrategy<FrameRate> {
    prop::sample::select(rates()).boxed()
}

fn sample_rate_strategy() -> BoxedStrategy<u32> {
    prop::sample::select(vec![
        8_000u32, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000,
    ])
    .boxed()
}

const NTSC_24: (u32, u32) = (24_000, 1001);
const NTSC_30: (u32, u32) = (30_000, 1001);

/// The namespace every fixture derives from, so identities are reproducible.
fn namespace() -> Uuid {
    Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
}

/// A stand-in for the document types WP03 will write, holding one of everything
/// whose persistence WP01 decides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Persisted {
    project: ProjectId,
    scenes: Vec<SceneId>,
    nodes: Vec<NodeId>,
    tracks: Vec<TrackId>,
    assets: Vec<AssetId>,
    commands: Vec<CommandId>,
    events: Vec<EventDefinitionId>,
    keys: BTreeMap<i64, Rational>,
    positions: BTreeMap<i64, FramePos>,
    rate: FrameRate,
    revision: Revision,
}

impl Persisted {
    fn sample() -> Self {
        let nodes: Vec<NodeId> = (0..8)
            .map(|i| NodeId::derive(&namespace(), format!("node-{i}").as_bytes()))
            .collect();
        let mut keys = BTreeMap::new();
        keys.insert(3, Rational::new(1001, 24_000).unwrap());
        keys.insert(-2, Rational::new(-1, 3).unwrap());
        keys.insert(0, Rational::from_i128(0));
        let mut positions = BTreeMap::new();
        positions.insert(1001, FramePos::at_frame(1001));
        positions.insert(
            -1,
            FramePos::new(-1, Rational::new(999, 1000).unwrap()).unwrap(),
        );
        Self {
            project: ProjectId::derive(&namespace(), b"project"),
            scenes: (0..2)
                .map(|i| SceneId::derive(&namespace(), format!("scene-{i}").as_bytes()))
                .collect(),
            nodes,
            tracks: vec![TrackId::derive(&namespace(), b"track")],
            assets: vec![AssetId::derive(&namespace(), b"asset")],
            commands: vec![CommandId::derive(&namespace(), b"command")],
            events: vec![EventDefinitionId::derive(&namespace(), b"event")],
            keys,
            positions,
            rate: FrameRate::new(24_000, 1001).unwrap(),
            revision: Revision(7),
        }
    }

    /// The canonical byte encoding of the same state, built through `hash.rs`.
    fn encode(&self) -> CanonicalBytes {
        let mut cp = CanonicalBytes::with_domain("anim_engine/document/v1").unwrap();
        cp.u64(self.revision.0);
        cp.u32(self.rate.num);
        cp.u32(self.rate.den);
        let ids: Vec<Uuid> = std::iter::once(self.project.uuid())
            .chain(self.scenes.iter().map(|i| i.uuid()))
            .chain(self.nodes.iter().map(|i| i.uuid()))
            .chain(self.tracks.iter().map(|i| i.uuid()))
            .chain(self.assets.iter().map(|i| i.uuid()))
            .chain(self.commands.iter().map(|i| i.uuid()))
            .chain(self.events.iter().map(|i| i.uuid()))
            .collect();
        for id in &ids {
            cp.uuid(id);
        }
        for (key, value) in &self.keys {
            cp.i64(*key);
            cp.rational(value);
        }
        for (key, position) in &self.positions {
            cp.i64(*key);
            cp.i64(position.frame);
            cp.rational(&position.subframe());
        }
        cp
    }
}

// ---------------------------------------------------------------------------
// Rational: normalization, ordering, arithmetic
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    /// Spec 10.6: positive denominator, reduced by the gcd, zero is `0/1`. An error
    /// is acceptable only when the reduced value truly does not fit.
    #[test]
    fn normalization_is_unique_and_never_wraps(num in any::<i128>(), den in any::<i128>()) {
        let den = if den == 0 { 1 } else { den };
        let (n, d) = ref_normalized(&bi(num), &bi(den));
        let fits = fits_i128(&n) && fits_i128(&d);
        match Rational::new(num, den) {
            Ok(r) => {
                prop_assert!(fits, "a value that does not fit was accepted: {}", r);
                prop_assert_eq!(r.num(), to_i128(&n));
                prop_assert_eq!(r.den(), to_i128(&d));
                prop_assert!(r.den() > 0, "denominator must stay positive: {}", r);
                if r.num() == 0 {
                    prop_assert_eq!(r.den(), 1, "zero must be exactly 0/1");
                } else {
                    let g = ref_gcd(magnitude(&bi(r.num())), bi(r.den()));
                    prop_assert_eq!(g, one(), "{} is not coprime", r);
                }
                prop_assert_eq!(r.to_string(), format!("{n}/{d}"));
            }
            Err(e) => {
                let overflow = matches!(e, CoreError::ArithmeticOverflow { .. });
                prop_assert!(overflow, "wrong error for {}/{}: {}", num, den, e);
                prop_assert!(!fits, "rejected a value that fits: {}/{} -> {}/{}", num, den, n, d);
            }
        }
    }

    /// `Ok` must equal the reference; `Err` must be justified by width or by a zero
    /// divisor, never by a bug.
    #[test]
    fn arithmetic_matches_the_reference(
        a in rational_strategy(),
        b in rational_strategy(),
        op in prop::sample::select(vec![Op::Add, Op::Sub, Op::Mul, Op::Div]),
    ) {
        let reference = op.reference(&a, &b);
        match (op.apply(a, b), reference) {
            (Ok(got), Some((n, d))) => {
                let fits = fits_i128(&n) && fits_i128(&d);
                prop_assert!(fits, "reference does not fit but no error was raised");
                let expected = Rational::new(to_i128(&n), to_i128(&d)).unwrap();
                prop_assert_eq!(got, expected);
                prop_assert!(same_value(&got, &expected));
            }
            (Ok(got), None) => {
                let never = false;
                prop_assert!(never, "a zero divisor returned {}", got);
            }
            (Err(e), Some((n, d))) => {
                let fits = fits_i128(&n) && fits_i128(&d);
                prop_assert!(!fits, "spurious rejection of {} {:?} {} = {}/{}", a, op, b, n, d);
                let overflow = matches!(e, CoreError::ArithmeticOverflow { .. });
                prop_assert!(overflow, "{}", e);
            }
            (Err(e), None) => {
                let zero_div = matches!(e, CoreError::ZeroDenominator { .. });
                prop_assert!(zero_div, "division by zero reported {}: {}", e, e);
            }
        }
    }

    #[test]
    fn ordering_agrees_with_cross_multiplication(a in rational_strategy(), b in rational_strategy()) {
        prop_assert_eq!(a.cmp(&b), compare_reference(&a, &b));
        prop_assert_eq!(a == b, same_value(&a, &b));
        // Hash must agree with Eq, or an ordered container keyed on a rational
        // silently loses entries.
        if a == b {
            let mut x = CanonicalBytes::new();
            let mut y = CanonicalBytes::new();
            x.rational(&a);
            y.rational(&b);
            prop_assert_eq!(x.as_slice(), y.as_slice());
        }
    }

    #[test]
    fn floor_and_ceil_follow_the_mathematical_definitions(r in rational_strategy()) {
        let (n, d) = as_reference(&r);
        let floor = ref_floor(&n, &d);
        let exact = is_zero(&(&n % &d));
        let ceil = if exact { floor.clone() } else { floor.clone() + one() };
        let got_floor = r.floor_to_i64();
        let got_ceil = r.ceil_to_i64();
        match (&got_floor, &got_ceil) {
            (Ok(f), Ok(c)) => {
                prop_assert_eq!(*f, i64::try_from(floor.clone()).unwrap());
                prop_assert_eq!(*c, i64::try_from(ceil.clone()).unwrap());
                prop_assert!(f <= c);
                prop_assert!(*c - *f <= 1, "ceil and floor differ by more than one");
                if f == c {
                    prop_assert!(r.is_integer(), "{} is exact but floor == ceil", r);
                }
            }
            _ => {
                if got_floor.is_err() {
                    prop_assert!(!fits_i64(&floor), "spurious floor rejection: {}", r);
                    let bad = !matches!(got_floor, Err(CoreError::ArithmeticOverflow { .. }));
                    prop_assert!(!bad, "floor rejection used the wrong error kind");
                }
                if got_ceil.is_err() {
                    prop_assert!(!fits_i64(&ceil), "spurious ceil rejection: {}", r);
                    let bad = !matches!(got_ceil, Err(CoreError::ArithmeticOverflow { .. }));
                    prop_assert!(!bad, "ceil rejection used the wrong error kind");
                }
            }
        }
    }

    /// Canonical text is the persisted form, so it must round-trip exactly and refuse
    /// anything merely equivalent.
    #[test]
    fn canonical_text_round_trips(r in rational_strategy()) {
        let text = r.to_string();
        let parsed = Rational::parse_canonical(&text).unwrap();
        prop_assert_eq!(parsed, r);
        prop_assert_eq!(parsed.to_string(), text.clone());
        let doubled = format!("{}/{}", r.num() * 2, r.den() * 2);
        let refused = Rational::parse_canonical(&doubled).is_err();
        prop_assert!(refused, "accepted the non-canonical spelling {}", doubled);
    }
}

// ---------------------------------------------------------------------------
// Frame positions, rates, and the two conversions
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// `frame_containing` must be the inverse of `time_of`. This is the property the
    /// private normalized subframe exists to guarantee: one instant, one name. A
    /// second spelling would split `(SceneId, Revision, FramePos)` cache keys and
    /// reorder keyframes by representation rather than by time.
    #[test]
    fn frame_containing_inverts_time_of(
        rate in rate_strategy(),
        frame in -5_000_000i64..5_000_000i64,
        sub_num in -100_000i64..100_000i64,
        sub_den in 1i64..10_000,
    ) {
        let sub = Rational::new(i128::from(sub_num), i128::from(sub_den)).unwrap();
        let position = FramePos::normalized(frame, sub).unwrap();
        prop_assert!(position.subframe() >= Rational::from_i128(0));
        prop_assert!(position.subframe() < Rational::from_i128(1));

        let time = rate.time_of(position).unwrap();
        let pos_num = bi(i128::from(position.frame)) * bi(position.subframe().den())
            + bi(position.subframe().num());
        let num = pos_num * bi(i128::from(rate.den));
        let den = bi(position.subframe().den()) * bi(i128::from(rate.num));
        let (rn, rd) = ref_normalized(&num, &den);
        let expected = Rational::new(to_i128(&rn), to_i128(&rd)).unwrap();
        prop_assert!(same_value(&time, &expected));

        let back = rate.frame_containing(time).unwrap();
        prop_assert_eq!(back, position, "one instant got two positions");
        prop_assert_eq!(rate.time_of(back).unwrap(), time);
    }

    #[test]
    fn position_order_matches_time_order(
        rate in rate_strategy(),
        a_frame in -1_000_000i64..1_000_000i64,
        b_frame in -1_000_000i64..1_000_000i64,
        a_sub in 0i64..10_000,
        b_sub in 0i64..10_000,
        den in 1i64..10_000,
    ) {
        // normalized, not new: a_sub can exceed den, so the generated subframe is
        // legitimately out of range and must carry rather than be rejected.
        let a = FramePos::normalized(a_frame, Rational::new(i128::from(a_sub), i128::from(den)).unwrap()).unwrap();
        let b = FramePos::normalized(b_frame, Rational::new(i128::from(b_sub), i128::from(den)).unwrap()).unwrap();
        let ta = rate.time_of(a).unwrap();
        let tb = rate.time_of(b).unwrap();
        prop_assert_eq!(a.cmp(&b), ta.cmp(&tb), "FramePos order must be time order");
    }

    /// Spec 10.7's formula asserted boundary by boundary, plus the partition
    /// property: adjacent frames share an endpoint and no sample.
    #[test]
    fn sample_boundaries_and_partitioning(
        rate in rate_strategy(),
        frame in -200_000i64..200_000i64,
        sample_rate in sample_rate_strategy(),
    ) {
        let interval = rate.frame_samples(frame, sample_rate).unwrap();
        prop_assert_eq!(interval.sample_rate, sample_rate);
        prop_assert!(interval.start < interval.end, "a frame must own a sample: {:?}", interval);
        prop_assert_eq!(
            interval.start,
            i64::try_from(ref_sample_boundary(frame, sample_rate, rate.num, rate.den))
                .unwrap()
        );
        prop_assert_eq!(
            interval.end,
            i64::try_from(ref_sample_boundary(frame + 1, sample_rate, rate.num, rate.den))
                .unwrap()
        );

        // Deliberately no assertion that consecutive frames own equal sample counts:
        // at 24000/1001 and 48 kHz a frame owns 1600 or 1602 samples alternately, and
        // requiring equality would fail on correct arithmetic.
        let next = rate.frame_samples(frame + 1, sample_rate).unwrap();
        prop_assert!(interval.touches(next), "{:?} then {:?}", interval, next);

        prop_assert_eq!(
            rate.sample_at(FramePos::at_frame(frame), sample_rate).unwrap(),
            interval.start
        );
    }

    #[test]
    fn frame_range_span_equals_its_two_boundaries(
        rate in rate_strategy(),
        start in -100_000i64..100_000i64,
        count in 0i64..3_000,
        sample_rate in sample_rate_strategy(),
    ) {
        let end = start + count;
        let range = FrameRange::new(start, end).unwrap();
        let span = rate.frame_range_samples(range, sample_rate).unwrap();
        prop_assert_eq!(range.len().unwrap(), count as u64);
        if count == 0 {
            prop_assert!(span.is_empty());
        } else {
            let first = rate.frame_samples(start, sample_rate).unwrap();
            let last = rate.frame_samples(end - 1, sample_rate).unwrap();
            prop_assert_eq!(span.start, first.start);
            prop_assert_eq!(span.end, last.end);
            let mut sum = 0u64;
            for f in start..end {
                let iv = rate.frame_samples(f, sample_rate).unwrap();
                sum += iv.len().unwrap();
            }
            prop_assert_eq!(sum, span.len().unwrap(), "per-frame counts must tile the span");
        }
    }

    #[test]
    fn tick_conversion_matches_the_reference(
        rate in rate_strategy(),
        frame in -1_000_000i64..1_000_000i64,
        ticks_per_frame in 1u32..1000,
    ) {
        let ticks = checked_frame_to_ticks(frame, ticks_per_frame).unwrap();
        prop_assert_eq!(
            ticks,
            i64::try_from(bi(i128::from(frame)) * bi(i128::from(ticks_per_frame)))
                .unwrap()
        );
        // The step duration times the steps per second is exactly one second.
        let step = tick_duration(rate, ticks_per_frame).unwrap();
        let per_second = Rational::new(
            i128::from(rate.num) * i128::from(ticks_per_frame),
            i128::from(rate.den),
        )
        .unwrap();
        let product = step.checked_mul(per_second).unwrap();
        prop_assert!(same_value(&product, &Rational::from_i128(1)), "1/(fps*N) is not the reciprocal");
    }
}

// ---------------------------------------------------------------------------
// G1's named requirement: 10,000 adjacent frames, both NTSC rates
// ---------------------------------------------------------------------------

#[test]
fn ten_thousand_adjacent_frames_partition_the_sample_axis() {
    for (num, den) in [NTSC_24, NTSC_30] {
        let rate = FrameRate::new(num, den).unwrap();
        let frames = 10_000i64;
        let sample_rate = 48_000;
        let mut previous: Option<SampleInterval> = None;
        let mut counted = 0u64;
        for frame in 0..frames {
            let interval = rate.frame_samples(frame, sample_rate).unwrap();
            // Each boundary recomputed from spec 10.7's formula, never carried from
            // the previous frame. Accumulating a step is what drift would look like,
            // and asserting against a running total could not detect it.
            assert_eq!(
                interval.start,
                i64::try_from(ref_sample_boundary(frame, sample_rate, num, den)).unwrap(),
                "frame {frame} at {num}/{den}"
            );
            if let Some(previous) = previous {
                assert_eq!(
                    previous.end, interval.start,
                    "gap or overlap at frame {frame}"
                );
            } else {
                assert_eq!(interval.start, 0, "frame 0 must start at sample 0");
            }
            counted += interval.len().unwrap();
            previous = Some(interval);
        }
        let span = rate
            .frame_range_samples(FrameRange::new(0, frames).unwrap(), sample_rate)
            .unwrap();
        assert_eq!(
            counted,
            span.len().unwrap(),
            "the 10,000 intervals must tile the span"
        );
        assert_eq!(previous.unwrap().end, span.end);
    }
}

#[test]
fn each_sample_is_claimed_by_exactly_one_frame() {
    // The adjacency result checked by enumeration over a window: 2,000 frames is
    // ~96,000 samples, small enough to count every one. Adjacency plus endpoints
    // implies this, and this is the test that catches an off-by-one which happened
    // to still tile.
    for (num, den) in [NTSC_24, NTSC_30, (30u32, 1u32), (24u32, 1u32)] {
        let rate = FrameRate::new(num, den).unwrap();
        let frames = 2_000i64;
        let first = rate.frame_samples(0, 48_000).unwrap();
        let last = rate.frame_samples(frames - 1, 48_000).unwrap();
        let base = first.start;
        let width = usize::try_from(last.end - base).unwrap();
        let mut claims = vec![0u32; width];
        for frame in 0..frames {
            let interval = rate.frame_samples(frame, 48_000).unwrap();
            let from = usize::try_from(interval.start - base).unwrap();
            let to = usize::try_from(interval.end - base).unwrap();
            for claim in &mut claims[from..to] {
                *claim += 1;
            }
        }
        let max = *claims.iter().max().unwrap();
        assert!(
            claims.iter().all(|count| *count == 1),
            "{num}/{den}: a sample was claimed {max} times"
        );
    }
}

#[test]
fn long_timelines_do_not_accumulate_error() {
    let rate = FrameRate::new(30_000, 1001).unwrap();
    for frame in [
        1_000i64,
        100_000,
        1_000_000,
        7_776_000,
        100_000_000,
        1_000_000_000,
        8_000_000_000_000,
    ] {
        let interval = rate.frame_samples(frame, 48_000).unwrap();
        assert_eq!(
            interval.start,
            i64::try_from(ref_sample_boundary(frame, 48_000, 30_000, 1001)).unwrap(),
            "frame {frame}"
        );
        let round = rate
            .frame_containing(rate.time_of(FramePos::at_frame(frame)).unwrap())
            .unwrap();
        assert_eq!(
            round,
            FramePos::at_frame(frame),
            "frame {frame} lost its identity"
        );
    }
    // The sample axis, not the timeline, is what runs out of width first: a boundary
    // is `f * 48000 * 1001 / 30000` = f × 1601.6, so any frame past ~5.75e15 has no
    // representable sample index even though the frame number itself is fine. This
    // must be an overflow, never a wrapped value that looks plausible.
    for frame in [5_776_000_000_000_000_000i64, i64::MAX / 2, i64::MAX] {
        assert!(
            matches!(
                rate.frame_samples(frame, 48_000),
                Err(CoreError::ArithmeticOverflow { .. })
            ),
            "frame {frame} wrapped instead of failing"
        );
        // Yet the same frame is still a legal position on the canonical timeline,
        // because `Rational` is wider than the sample index. That asymmetry is
        // spec 10.6's "the canonical timeline supports negative positions" and 18.5's
        // "filesystem indices reject negative timeline samples" being decided at
        // different boundaries.
        let time = rate.time_of(FramePos::at_frame(frame)).unwrap();
        assert_eq!(
            rate.frame_containing(time).unwrap(),
            FramePos::at_frame(frame),
            "frame {frame} lost its identity"
        );
    }
}

#[test]
fn negative_time_never_truncates_toward_zero() {
    for (num, den) in [NTSC_24, NTSC_30, (24u32, 1u32)] {
        let rate = FrameRate::new(num, den).unwrap();
        for frame in [-1i64, -2, -24, -1001, -100_000] {
            let boundary = rate.time_of(FramePos::at_frame(frame)).unwrap();
            let exact = rate.frame_containing(boundary).unwrap();
            assert_eq!(
                exact,
                FramePos::at_frame(frame),
                "exact boundary at frame {frame}"
            );
            // A hair below the boundary belongs to the preceding frame.
            let step = Rational::new(1, i128::from(num) * 1000).unwrap();
            let below = boundary.checked_sub(step).unwrap();
            let pos = rate.frame_containing(below).unwrap();
            assert_eq!(pos.frame, frame - 1, "below the boundary at frame {frame}");
            assert!(pos.subframe() < Rational::from_i128(1));
            assert!(
                pos.subframe() >= Rational::from_i128(0),
                "subframe went negative"
            );
            assert!(pos.subframe() > Rational::from_i128(0), "the step vanished");
        }
    }
}

#[test]
fn subframe_carry_is_exact_in_both_directions() {
    for (num, den) in [NTSC_24, NTSC_30] {
        let rate = FrameRate::new(num, den).unwrap();
        for (frame, over) in [
            (0i64, Rational::new(3, 2).unwrap()),
            (5, Rational::new(-1, 3).unwrap()),
            (-4, Rational::new(7, 4).unwrap()),
            (-3, Rational::new(-7, 4).unwrap()),
            (1001, Rational::new(1, 1).unwrap()),
        ] {
            let carried = FramePos::normalized(frame, over).unwrap();
            assert!(carried.subframe() >= Rational::from_i128(0));
            assert!(carried.subframe() < Rational::from_i128(1));
            // The carry moved the frame but preserved the instant: the position is
            // the same time as the un-normalized pair it came from.
            let instant = rate.time_of(carried).unwrap();
            let direct =
                Rational::new(i128::from(frame) * over.den() + over.num(), over.den()).unwrap();
            let expected = direct.checked_div(rate.as_rational().unwrap()).unwrap();
            assert!(
                same_value(&instant, &expected),
                "{frame} + {over} moved in time"
            );
            assert_eq!(
                FramePos::normalized(carried.frame, carried.subframe()).unwrap(),
                carried,
                "normalization is not idempotent"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn uuid_text_round_trips_and_order_is_byte_order(
        a_bytes in prop::array::uniform16(any::<u8>()),
        b_bytes in prop::array::uniform16(any::<u8>()),
    ) {
        let a = NodeId::from_uuid(Uuid::from_bytes(a_bytes));
        let b = NodeId::from_uuid(Uuid::from_bytes(b_bytes));
        let text = a.to_canonical_string();
        prop_assert_eq!(text.len(), 36);
        prop_assert_eq!(text.to_lowercase(), text.clone(), "ids must serialize lowercase");
        prop_assert_eq!(NodeId::parse(&text).unwrap(), a);
        prop_assert_eq!(
            a.cmp(&b),
            a_bytes.as_slice().cmp(b_bytes.as_slice()),
            "spec 13.7: comparison uses the 16 bytes"
        );
        let json = serde_json::to_string(&b).unwrap();
        let parsed: NodeId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, b);
        prop_assert_eq!(json, format!("\"{}\"", b.to_canonical_string()));
    }
}

#[test]
fn loose_uuid_spellings_are_refused() {
    let id = AssetId::derive(&namespace(), b"cel-01");
    let text = id.to_canonical_string();
    for bad in [
        text.to_uppercase(),
        format!("{{{text}}}"),
        format!("urn:uuid:{text}"),
        text.replace('-', ""),
        String::new(),
        "00000000-0000-0000-0000-00000000000x".to_owned(),
    ] {
        assert!(
            AssetId::parse(&bad).is_err(),
            "accepted a non-canonical id: {bad}"
        );
    }
}

#[test]
fn derivation_is_reproducible_and_input_sensitive() {
    // The v5-only pin means every id in a saved document is reproducible from what
    // the document already knows. Asserted twice from scratch, as a fresh process
    // would see it.
    let first = CommandId::derive(&namespace(), b"command-1");
    assert_eq!(first, CommandId::derive(&namespace(), b"command-1"));
    assert_ne!(first, CommandId::derive(&namespace(), b"command-2"));
    assert_ne!(
        first,
        CommandId::derive(&Uuid::from_bytes([7u8; 16]), b"command-1")
    );
    assert_eq!(
        EventDefinitionId::derive(&namespace(), b"event-1").kind(),
        "event_definition"
    );
}

#[test]
fn a_saved_document_loads_back_with_the_same_ids_and_hashes() {
    let original = Persisted::sample();
    let digest_before = original.encode().digest();

    let json = serde_json::to_vec(&original).unwrap();
    let loaded: Persisted = serde_json::from_slice(&json).unwrap();

    assert_eq!(
        loaded, original,
        "identity or time changed across the round trip"
    );
    assert_eq!(
        loaded.encode().digest(),
        digest_before,
        "hash is not stable after load"
    );
    // Byte-for-byte identical on re-serialization, or a saved file stops matching
    // the hash recorded for it.
    assert_eq!(serde_json::to_vec(&loaded).unwrap(), json);

    assert_eq!(
        serde_json::from_str::<Revision>("0").unwrap(),
        Revision::ZERO
    );
    assert_eq!(
        Revision(u64::MAX - 1).checked_next().unwrap(),
        Revision(u64::MAX)
    );
    assert!(matches!(
        Revision(u64::MAX).checked_next(),
        Err(CoreError::ArithmeticOverflow { .. })
    ));
}

#[test]
fn canonical_order_does_not_depend_on_insertion_order() {
    // Spec 7.1: persisted collections are ordered by their stable keys. The value is
    // a function of the key here, so insertion order can only affect the bytes if
    // the ordering is wrong.
    let ids: Vec<NodeId> = (0..12)
        .map(|i| NodeId::derive(&namespace(), format!("node-{i}").as_bytes()))
        .collect();
    let keyed = |id: &NodeId| Rational::from_i128(i128::from(id.as_bytes()[0]) * 100 + 7);

    let build = |ordered: Vec<NodeId>| -> BTreeMap<NodeId, Rational> {
        ordered.into_iter().map(|id| (id, keyed(&id))).collect()
    };
    let mut descending = ids.clone();
    descending.reverse();
    let mut rotated: Vec<NodeId> = ids[5..].to_vec();
    rotated.extend_from_slice(&ids[..5]);

    let a = build(ids.clone());
    let b = build(descending);
    let c = build(rotated);
    let text_a = serde_json::to_string(&a).unwrap();
    assert_eq!(
        text_a,
        serde_json::to_string(&b).unwrap(),
        "key order leaked into the bytes"
    );
    assert_eq!(
        text_a,
        serde_json::to_string(&c).unwrap(),
        "key order leaked into the bytes"
    );
    assert_eq!(a, b);
    assert_eq!(a, c);

    let keys: Vec<NodeId> = a.keys().copied().collect();
    let mut by_bytes = keys.clone();
    by_bytes.sort_by_key(|id| *id.as_bytes());
    assert_eq!(keys, by_bytes, "the map's key order is not byte order");
}

// ---------------------------------------------------------------------------
// Canonical encoding
// ---------------------------------------------------------------------------

#[test]
fn canonical_encoding_matches_a_hand_written_byte_stream() {
    // The layout contract, written out by hand from the rules in `hash.rs`. If this
    // passes, a digest is a function of documented bytes and nothing else.
    let mut cp = CanonicalBytes::with_domain("anim_engine/fixture/v1").unwrap();
    cp.u32(1);
    cp.i64(-2);
    cp.text("ab");
    cp.rational(&Rational::new(-1, 3).unwrap());
    cp.bool(false);
    cp.f64(0.5).unwrap();
    cp.uuid(&Uuid::nil());

    let mut expected: Vec<u8> = Vec::new();
    // "anim_engine" 11 + "/" 1 + "fixture" 7 + "/" 1 + "v1" 2 = 22 bytes of domain.
    expected.extend_from_slice(&22u64.to_le_bytes());
    expected.extend_from_slice(b"anim_engine/fixture/v1");
    expected.extend_from_slice(&1u32.to_le_bytes());
    expected.extend_from_slice(&(-2i64).to_le_bytes());
    expected.extend_from_slice(&2u64.to_le_bytes());
    expected.extend_from_slice(b"ab");
    expected.extend_from_slice(&4u64.to_le_bytes()); // "-1/3"
    expected.extend_from_slice(b"-1/3");
    expected.push(0x00);
    expected.extend_from_slice(&0.5f64.to_bits().to_le_bytes());
    expected.extend_from_slice(&[0u8; 16]);

    assert_eq!(cp.as_slice(), expected.as_slice());
    assert_eq!(
        cp.hex_digest(),
        blake3::hash(&expected).to_hex().to_string()
    );
    assert_eq!(cp.digest(), *blake3::hash(&expected).as_bytes());
}

#[test]
fn encoding_is_injective_enough_to_tell_structures_apart() {
    let mut one_field = CanonicalBytes::with_domain("anim_engine/x/v1").unwrap();
    one_field.text("ab");
    let mut two_fields = CanonicalBytes::with_domain("anim_engine/x/v1").unwrap();
    two_fields.text("a");
    two_fields.text("b");
    assert_ne!(
        one_field.digest(),
        two_fields.digest(),
        "length prefixes must make the stream prefix-free"
    );

    let mut empty = CanonicalBytes::with_domain("anim_engine/x/v1").unwrap();
    empty.text("");
    assert_ne!(empty.digest(), one_field.digest());

    let mut other_domain = CanonicalBytes::with_domain("anim_engine/y/v1").unwrap();
    other_domain.text("ab");
    assert_ne!(
        one_field.digest(),
        other_domain.digest(),
        "domains must separate identical payloads"
    );

    // Width is part of the identity, so a reordered field cannot collide.
    let mut narrow = CanonicalBytes::new();
    narrow.u32(1);
    let mut wide = CanonicalBytes::new();
    wide.i64(1);
    assert_ne!(narrow.digest(), wide.digest());
}

proptest! {
    #[test]
    fn encoding_is_a_pure_function_of_the_values(
        frame in any::<i64>(),
        sub in rational_strategy(),
        rate in rate_strategy(),
        label in "[a-z]{0,20}",
    ) {
        let mut a = CanonicalBytes::with_domain("anim_engine/snapshot/v1").unwrap();
        let mut b = CanonicalBytes::with_domain("anim_engine/snapshot/v1").unwrap();
        a.i64(frame);
        a.rational(&sub);
        a.u32(rate.num);
        a.u32(rate.den);
        a.text(&label);
        b.i64(frame);
        b.rational(&sub);
        b.u32(rate.num);
        b.u32(rate.den);
        b.text(&label);
        prop_assert_eq!(a.digest(), b.digest());
        prop_assert_eq!(a.as_slice(), b.as_slice());

        let mut extended = a.clone();
        extended.u8(1);
        prop_assert_ne!(a.digest(), extended.digest());
    }
}

// ---------------------------------------------------------------------------
// Explicit failure, which spec 10.12 makes part of the gate
// ---------------------------------------------------------------------------

#[test]
fn every_invalid_rate_denominator_and_division_fails() {
    for (num, den) in [(0u32, 1u32), (0, 0), (24, 0), (0, 1001), (1, 0)] {
        assert!(
            matches!(
                FrameRate::new(num, den),
                Err(CoreError::NonPositiveRate { .. })
            ),
            "FrameRate({num}/{den}) was accepted"
        );
    }
    let rate = FrameRate::new(24, 1).unwrap();
    assert!(matches!(
        rate.frame_samples(0, 0),
        Err(CoreError::NonPositiveRate { .. })
    ));
    assert!(matches!(
        rate.sample_at(FramePos::at_frame(0), 0),
        Err(CoreError::NonPositiveRate { .. })
    ));
    assert!(matches!(
        rate.frame_range_samples(FrameRange::new(0, 10).unwrap(), 0),
        Err(CoreError::NonPositiveRate { .. })
    ));
    assert!(matches!(
        checked_frame_to_ticks(1, 0),
        Err(CoreError::NonPositiveRate { .. })
    ));
    assert!(matches!(
        tick_duration(rate, 0),
        Err(CoreError::NonPositiveRate { .. })
    ));
    assert!(matches!(
        Rational::new(1, 0),
        Err(CoreError::ZeroDenominator { .. })
    ));
    assert!(matches!(
        Rational::from_i128(1).checked_div(Rational::from_i128(0)),
        Err(CoreError::ZeroDenominator { .. })
    ));
}

#[test]
fn invalid_ranges_and_subframes_are_refused_not_clamped() {
    assert!(matches!(
        FrameRange::new(10, 9),
        Err(CoreError::InvalidRange { .. })
    ));
    assert!(matches!(
        SampleInterval::new(10, 9, 48_000),
        Err(CoreError::InvalidRange { .. })
    ));
    for bad in [(1, 1), (2, 1), (-1, 3), (i128::MAX, 1)] {
        let sub = Rational::new(bad.0, bad.1).unwrap();
        assert!(
            matches!(
                FramePos::new(0, sub),
                Err(CoreError::SubframeOutOfRange { .. })
            ),
            "subframe {sub} was accepted"
        );
    }
    // A negative sample index stays legal here; spec 10.11 puts the clipping at the
    // filesystem and export boundaries, not in this type.
    let rate = FrameRate::new(24, 1).unwrap();
    assert!(rate.frame_samples(-1, 48_000).unwrap().start < 0);
}

#[test]
fn decimal_ntsc_is_never_silently_accepted() {
    // Spec 10.11: never silently round a decimal NTSC rate. "23.976" has no rational
    // parse, and a float is not canonical time at all.
    assert!(Rational::parse_canonical("23.976").is_err());
    assert!(serde_json::from_str::<Rational>("23.976").is_err());
    assert!(serde_json::from_str::<Rational>("\"23.976\"").is_err());
    assert!(serde_json::from_str::<Rational>("null").is_err());
    assert!(serde_json::from_str::<Rational>("1e10").is_err());
    // The nearest plausible decimal spelling is not the NTSC rate.
    let ntsc = FrameRate::new(24_000, 1001).unwrap();
    let decimal_ish = FrameRate::new(23_976, 1000).unwrap();
    assert_ne!(
        ntsc.as_rational().unwrap(),
        decimal_ish.as_rational().unwrap()
    );
}

#[test]
fn non_finite_and_malformed_canonical_input_is_refused() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut cp = CanonicalBytes::with_domain("anim_engine/test/v1").unwrap();
        assert!(matches!(cp.f64(bad), Err(CoreError::NonFiniteValue { .. })));
    }
    for bad in [
        "",
        " leading",
        "trailing ",
        "has whitespace",
        "anim_engine/\u{1f600}",
    ] {
        assert!(
            matches!(
                CanonicalBytes::with_domain(bad),
                Err(CoreError::MalformedCanonical { .. })
            ),
            "accepted domain {bad:?}"
        );
    }
    assert!(CanonicalBytes::with_domain("anim_engine/ok/v1").is_ok());
    assert!(matches!(
        NodeId::parse("not-an-id"),
        Err(CoreError::IdentifierParse { .. })
    ));
    assert!(serde_json::from_str::<FramePos>(r#"{"frame":5,"subframe":"3/2"}"#).is_err());
    assert!(serde_json::from_str::<FramePos>(r#"{"frame":5,"subframe":"10/20"}"#).is_err());
}

#[test]
fn an_error_names_the_value_that_was_rejected() {
    // Spec 23: reject with the offending value in hand, so a report says what the
    // document held rather than only that something was wrong.
    let e = Rational::new(12_345, 0).unwrap_err();
    assert!(matches!(e, CoreError::ZeroDenominator { .. }), "{e}");
    assert!(
        e.to_string().contains("12345"),
        "must name the numerator: {e}"
    );
    assert!(e.to_string().contains("zero"), "must name the cause: {e}");
    assert!(FrameRate::new(0, 1001)
        .unwrap_err()
        .to_string()
        .contains("0/1001"));
    assert!(FramePos::new(4, Rational::new(5, 2).unwrap())
        .unwrap_err()
        .to_string()
        .contains("5/2"));
    let e = checked_frame_to_ticks(i64::MAX, 4).unwrap_err();
    let product = (i64::MAX as i128) * 4;
    assert!(
        e.to_string().contains(&product.to_string()),
        "overflow must carry the product: {e}"
    );
    assert_eq!(e.code().as_str(), "core.arithmetic_overflow");
    assert_eq!(e.to_diagnostic().severity, DiagnosticSeverity::Error);
    assert!(DiagnosticSeverity::Error >= DiagnosticSeverity::Warning);
}
