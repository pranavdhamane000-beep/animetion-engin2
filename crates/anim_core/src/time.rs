//! Frame rates, positions, half-open ranges and sample intervals (spec 10.6-10.9).
//!
//! Frame `n` starts at exact time `n/fps`; an arbitrary position is
//! `(frame + subframe)/fps`. Every conversion in this file is a floor or an exact
//! rational multiply — never a multiply-by-reciprocal, because `1/(30000/1001)`
//! is not representable in decimal and accumulating that error across 10,000
//! frames is the drift spec 10.12 requires the tests to disprove.

use std::cmp::Ordering;

use num_bigint::BigInt;
use num_traits::Euclid;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::CoreError;
use crate::rational::Rational;

/// A positive frame rate held as an exact ratio, so NTSC is `24000/1001` and not
/// `23.976` (spec 10.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32,
}

impl FrameRate {
    /// Build a rate from a numerator and denominator. Either being zero is
    /// rejected: a zero numerator is a rate that never advances, and a zero
    /// denominator is a division that cannot be evaluated.
    pub fn new(num: u32, den: u32) -> Result<Self, CoreError> {
        if num == 0 || den == 0 {
            return Err(CoreError::NonPositiveRate {
                numerator: num.to_string(),
                denominator: den.to_string(),
            });
        }
        Ok(Self { num, den })
    }

    /// The rate as a rational, reduced. Reducing here rather than storing the
    /// reduced pair keeps [`FrameRate`]'s public fields honest: `24/1` and a
    /// hypothetical `48/2` produce the same [`Rational`].
    pub fn as_rational(&self) -> Result<Rational, CoreError> {
        Rational::new(i128::from(self.num), i128::from(self.den))
    }

    /// Exact time of a position: `(frame + subframe) / fps`.
    pub fn time_of(self, position: FramePos) -> Result<Rational, CoreError> {
        // position as one fraction: (frame * sub.den + sub.num) / sub.den
        let pos_num = BigInt::from(position.frame) * BigInt::from(position.subframe.den())
            + BigInt::from(position.subframe.num());
        let pos_den = BigInt::from(position.subframe.den());
        // divide by (num/den) => multiply by (den/num)
        let num = pos_num * BigInt::from(self.den);
        let den = pos_den * BigInt::from(self.num);
        Rational::from_bigint_parts(&num, &den)
    }

    /// The frame that contains a time (spec 10.7). `whole = floor(t * fps)`, so a
    /// time exactly on a negative boundary lands on the mathematically preceding
    /// frame: at 24000/1001, `t = -1/fps` is frame `-1` with zero subframe, not
    /// frame `0` approached from below.
    pub fn frame_containing(self, time: Rational) -> Result<FramePos, CoreError> {
        let scaled_num = BigInt::from(time.num()) * BigInt::from(self.num);
        let scaled_den = BigInt::from(time.den()) * BigInt::from(self.den);
        let whole = scaled_num.div_euclid(&scaled_den);
        let remainder = scaled_num.rem_euclid(&scaled_den);
        let frame = i64::try_from(&whole).map_err(|_| CoreError::ArithmeticOverflow {
            operation: "frame index from time",
            value: whole.to_string(),
        })?;
        let subframe = Rational::from_bigint_parts(&remainder, &scaled_den)?;
        FramePos::new(frame, subframe)
    }

    /// The half-open sample interval occupied by whole frame `f` (spec 10.7):
    ///
    /// ```text
    /// start = floor(f     * sr * fps_den / fps_num)
    /// end   = floor((f+1) * sr * fps_den / fps_num)
    /// ```
    ///
    /// Both ends are floors of the same monotone function, which is what makes
    /// adjacent frames share an endpoint but never a sample.
    pub fn frame_samples(self, frame: i64, sample_rate: u32) -> Result<SampleInterval, CoreError> {
        if sample_rate == 0 {
            return Err(CoreError::NonPositiveRate {
                numerator: "0".to_owned(),
                denominator: "1".to_owned(),
            });
        }
        let start = self.sample_boundary(frame, sample_rate)?;
        let next = BigInt::from(frame) + 1;
        let end = self.sample_boundary_bigint(&next, sample_rate)?;
        SampleInterval::new(start, end, sample_rate)
    }

    /// The sample containing a position: `floor(time * sr)` (spec 10.7). Use
    /// [`Rational::ceil_to_i64`] only where a decoder range must cover every
    /// source sample an interpolation touches; that is the caller's decision, not
    /// a variant of this function.
    pub fn sample_at(self, position: FramePos, sample_rate: u32) -> Result<i64, CoreError> {
        if sample_rate == 0 {
            return Err(CoreError::NonPositiveRate {
                numerator: "0".to_owned(),
                denominator: "1".to_owned(),
            });
        }
        let time = self.time_of(position)?;
        let num = BigInt::from(time.num()) * BigInt::from(sample_rate);
        let den = BigInt::from(time.den());
        to_i64(num.div_euclid(&den), "sample position from time")
    }

    /// Samples spanned by `[start, end)`: the floor at each boundary. This is the
    /// conversion WP08's frame-range PCM expectation and WP09's pre-mux sample
    /// count both ask for, and it uses the same boundary function as
    /// [`FrameRate::frame_samples`] so the two cannot disagree.
    pub fn frame_range_samples(
        self,
        range: FrameRange,
        sample_rate: u32,
    ) -> Result<SampleInterval, CoreError> {
        range.validate()?;
        let start = self.sample_boundary(range.start, sample_rate)?;
        let end = self.sample_boundary(range.end, sample_rate)?;
        SampleInterval::new(start, end, sample_rate)
    }

    fn sample_boundary(self, frame: i64, sample_rate: u32) -> Result<i64, CoreError> {
        self.sample_boundary_bigint(&BigInt::from(frame), sample_rate)
    }

    fn sample_boundary_bigint(self, frame: &BigInt, sample_rate: u32) -> Result<i64, CoreError> {
        if sample_rate == 0 {
            return Err(CoreError::NonPositiveRate {
                numerator: "0".to_owned(),
                denominator: "1".to_owned(),
            });
        }
        let num = frame * BigInt::from(sample_rate) * BigInt::from(self.den);
        let den = BigInt::from(self.num);
        to_i64(num.div_euclid(&den), "sample boundary from frame")
    }
}

/// A position on the timeline: a whole frame plus a subframe in `[0, 1)`.
///
/// `frame` is public and `subframe` is not. That is how both halves of spec 10.8
/// stay true at once: the listing writes `pub subframe: Rational`, and the closing
/// rule says "Fields whose invariants require **normalization** are private" —
/// which is the same word 10.6 uses for this exact field ("a subframe normalized
/// to [0,1)"). Where a listing and a rule name the same thing, the rule wins.
/// `frame` genuinely needs no normalization, because 10.6 supports negative
/// positions outright, so reading or writing it cannot produce an illegal value.
///
/// The `[0, 1)` range is not tidiness. `(frame, subframe)` must be a **unique**
/// name for one instant, because `FramePos` is a sort key wherever the spec caches
/// or orders: `(scene_id, revision, FramePos)` for snapshot caches (spec 7.2,
/// 13.8), "Sort keys by FramePos" for track sampling (spec 30). Since
/// `clippy.toml` rules out `HashMap`, those are ordered maps keyed on the type's
/// `Ord`. A public subframe would let a caller hold `frame: 5, subframe: 3/2`,
/// which is the same instant as `frame: 6, subframe: 1/2`: `time_of` returns an
/// equal rational for both, while `Eq` says they differ and `Ord` ranks them by
/// representation instead of by time. One timeline position, two cache keys, and
/// a keyframe order that depends on how a number happened to be written.
///
/// Downstream crates cannot restore that uniqueness by normalizing at the point of
/// use, because a private field makes an out-of-range `FramePos` unconstructable
/// outside this crate in the first place. Spec 10.12 names normalization as an
/// operation to be property-tested rather than a rendering convention, which is
/// further evidence the invariant belongs to the type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FramePos {
    pub frame: i64,
    subframe: Rational,
}

impl FramePos {
    /// A position whose subframe is already in range. This rejects rather than
    /// carries, because it is the constructor deserialization uses: silently
    /// rewriting a persisted position would make a loaded document disagree with
    /// the bytes it was loaded from. Use [`FramePos::normalized`] when the caller
    /// holds a value that legitimately over- or under-shoots.
    pub fn new(frame: i64, subframe: Rational) -> Result<Self, CoreError> {
        if subframe < Rational::from_i128(0) || subframe >= Rational::from_i128(1) {
            return Err(CoreError::SubframeOutOfRange {
                value: subframe.to_string(),
            });
        }
        Ok(Self { frame, subframe })
    }

    /// A frame with no subframe.
    #[must_use]
    pub const fn at_frame(frame: i64) -> Self {
        Self {
            frame,
            subframe: Rational::from_i128(0),
        }
    }

    /// Carry a subframe outside `[0, 1)` into the frame (spec 10.6). `3/2` at
    /// frame `n` is frame `n+1` at half; `-1/3` at frame `n` is frame `n-1` at
    /// `2/3`. The carry is exact in both directions because the fraction is
    /// already normalized, and the result always satisfies the constructor's
    /// range, so `normalized` is idempotent.
    pub fn normalized(frame: i64, subframe: Rational) -> Result<Self, CoreError> {
        let carry = subframe.num().div_euclid(subframe.den());
        let remainder = subframe.num().rem_euclid(subframe.den());
        let carry = i64::try_from(carry).map_err(|_| CoreError::ArithmeticOverflow {
            operation: "subframe carry",
            value: subframe.to_string(),
        })?;
        let frame = frame
            .checked_add(carry)
            .ok_or(CoreError::ArithmeticOverflow {
                operation: "subframe carry into frame",
                value: format!("{frame} + {subframe}"),
            })?;
        let subframe = Rational::new(remainder, subframe.den())?;
        Ok(Self { frame, subframe })
    }

    /// The fractional part of the position, always in `[0, 1)`.
    #[must_use]
    pub const fn subframe(&self) -> Rational {
        self.subframe
    }

    /// True when the position sits exactly on a frame boundary, which is the case
    /// that decides ownership: "An event exactly at the upper boundary belongs to
    /// the next frame" (spec 13.6).
    #[must_use]
    pub const fn is_whole_frame(&self) -> bool {
        self.subframe.is_zero()
    }

    /// The frame a position at or after this one would enter: this frame when the
    /// subframe is zero, the next otherwise. Event ownership at `[time_of(n),
    /// time_of(n+1))` needs the boundary form, so the distinction is exposed here
    /// rather than re-derived by every caller that splits a range in two.
    pub fn next_whole_frame(self) -> Result<i64, CoreError> {
        if self.is_whole_frame() {
            Ok(self.frame)
        } else {
            self.frame
                .checked_add(1)
                .ok_or(CoreError::ArithmeticOverflow {
                    operation: "advance to next whole frame",
                    value: self.frame.to_string(),
                })
        }
    }
}

impl<'de> Deserialize<'de> for FramePos {
    /// Read back through the validating constructor, never around it.
    ///
    /// A derived implementation could reach `FramePos`'s private field, which is
    /// exactly the problem: a project file containing `{"frame":5,
    /// "subframe":"3/2"}` would produce a position that breaks the uniqueness the
    /// snapshot caches depend on, and the violation would arrive from disk rather
    /// than from code. Persisted data is untrusted input, so this is the one place
    /// the private field needs a gate. Spec 10.11's "never clamp invalid user
    /// data" makes the gate reject, not carry.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            frame: i64,
            subframe: Rational,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.frame, wire.subframe).map_err(serde::de::Error::custom)
    }
}

impl PartialOrd for FramePos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FramePos {
    /// Lexicographic on `(frame, subframe)`. This equals time order at any
    /// positive rate **only because** the subframe is normalized, which is the
    /// reason the field is private: an unnormalized pair would sort by how it was
    /// written rather than by when it happens, and every ordered map keyed on
    /// `FramePos` would inherit that order.
    fn cmp(&self, other: &Self) -> Ordering {
        self.frame
            .cmp(&other.frame)
            .then_with(|| self.subframe.cmp(&other.subframe))
    }
}

/// A half-open frame range `[start, end)` (spec 10.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRange {
    pub start: i64,
    pub end: i64,
}

impl FrameRange {
    /// Reversed ranges are rejected; empty ones are allowed, because "zero frames
    /// selected" is a real state a caller must be able to name. Packages that
    /// must not accept empty ranges — export's validated nonnegative range, spec
    /// 18.4 — check [`FrameRange::is_empty`] at their own boundary.
    pub fn new(start: i64, end: i64) -> Result<Self, CoreError> {
        let range = Self { start, end };
        range.validate()?;
        Ok(range)
    }

    fn validate(self) -> Result<(), CoreError> {
        if self.start > self.end {
            return Err(CoreError::InvalidRange {
                kind: "frame",
                start: self.start,
                end: self.end,
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    #[must_use]
    pub const fn contains(self, frame: i64) -> bool {
        frame >= self.start && frame < self.end
    }

    /// Frame count, computed so that a full-width range reports overflow instead
    /// of wrapping: `i64::MAX - i64::MIN` does not fit an `i64`.
    pub fn len(self) -> Result<u64, CoreError> {
        self.validate()?;
        let span = i128::from(self.end) - i128::from(self.start);
        u64::try_from(span).map_err(|_| CoreError::ArithmeticOverflow {
            operation: "frame range length",
            value: span.to_string(),
        })
    }
}

/// A half-open sample interval at a declared sample rate (spec 10.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleInterval {
    pub start: i64,
    pub end: i64,
    pub sample_rate: u32,
}

impl SampleInterval {
    pub fn new(start: i64, end: i64, sample_rate: u32) -> Result<Self, CoreError> {
        if sample_rate == 0 {
            return Err(CoreError::NonPositiveRate {
                numerator: "0".to_owned(),
                denominator: "1".to_owned(),
            });
        }
        if start > end {
            return Err(CoreError::InvalidRange {
                kind: "sample",
                start,
                end,
            });
        }
        Ok(Self {
            start,
            end,
            sample_rate,
        })
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Sample count. `end - start` cannot be negative once the interval is
    /// constructed, but it can exceed `i64`, so the width check stays.
    pub fn len(self) -> Result<u64, CoreError> {
        let span = i128::from(self.end) - i128::from(self.start);
        u64::try_from(span).map_err(|_| CoreError::ArithmeticOverflow {
            operation: "sample interval length",
            value: span.to_string(),
        })
    }

    /// True when this interval's end is the next interval's start, which is the
    /// partition property spec 10.7 states and 10.12 tests.
    #[must_use]
    pub const fn touches(self, next: Self) -> bool {
        self.end == next.start && self.sample_rate == next.sample_rate
    }
}

/// Narrow a `BigInt` to the persisted frame width, keeping the rejected value in
/// the error. `TryFromBigIntError`'s own message says only "number too large to fit
/// in target type", which is useless in a report about a specific frame; the text is
/// taken before the value moves into the conversion.
fn to_i64(value: BigInt, operation: &'static str) -> Result<i64, CoreError> {
    let text = value.to_string();
    i64::try_from(value).map_err(|_| CoreError::ArithmeticOverflow {
        operation,
        value: text,
    })
}

/// A frame index times a ticks-per-frame count, checked (spec 14.7 requires
/// `f * N` to be checked rather than trusted). Lives here because it is the same
/// overflow question as every other frame multiplication in this package, and
/// WP05 must not define a second one.
pub fn checked_frame_to_ticks(frame: i64, ticks_per_frame: u32) -> Result<i64, CoreError> {
    if ticks_per_frame == 0 {
        return Err(CoreError::NonPositiveRate {
            numerator: "0".to_owned(),
            denominator: "ticks_per_frame".to_owned(),
        });
    }
    let product = BigInt::from(frame) * BigInt::from(ticks_per_frame);
    to_i64(product, "frame to tick")
}

/// The step duration of a simulation clock as an exact rational: `1/(fps*N)`
/// (spec 14.6 computes it once from configuration). With `fps = num/den` that is
/// `den/(num*N)` — the rate's own denominator belongs in the numerator, and
/// dropping it turns `1001/48000` into `1/48000`, which is a 1001× error in every
/// step size WP05 takes. Returned as a rational so the caller converts to floating
/// point at its final numeric operation rather than accumulating seconds.
pub fn tick_duration(rate: FrameRate, ticks_per_frame: u32) -> Result<Rational, CoreError> {
    if ticks_per_frame == 0 {
        return Err(CoreError::NonPositiveRate {
            numerator: "0".to_owned(),
            denominator: "ticks_per_frame".to_owned(),
        });
    }
    let num = BigInt::from(rate.den);
    let den = BigInt::from(rate.num) * BigInt::from(ticks_per_frame);
    Rational::from_bigint_parts(&num, &den)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    fn ntsc24() -> FrameRate {
        FrameRate::new(24_000, 1001).unwrap()
    }

    #[test]
    fn zero_rates_are_rejected_everywhere() {
        assert!(matches!(
            FrameRate::new(0, 1),
            Err(CoreError::NonPositiveRate { .. })
        ));
        assert!(matches!(
            FrameRate::new(24, 0),
            Err(CoreError::NonPositiveRate { .. })
        ));
        assert!(matches!(
            ntsc24().frame_samples(0, 0),
            Err(CoreError::NonPositiveRate { .. })
        ));
        assert!(matches!(
            SampleInterval::new(0, 10, 0),
            Err(CoreError::NonPositiveRate { .. })
        ));
    }

    #[test]
    fn frame_start_times_are_exact() {
        let rate = ntsc24();
        let t1 = rate.time_of(FramePos::at_frame(1)).unwrap();
        assert_eq!(t1, Rational::new(1001, 24_000).unwrap());
        // The NTSC fact worth pinning: frame 24 is at 1.001 s, not 1 s, and no
        // whole frame lands on exactly one second. 24000 frames is 1001 seconds.
        assert_eq!(
            rate.time_of(FramePos::at_frame(24)).unwrap(),
            Rational::new(1001, 1000).unwrap()
        );
        assert_eq!(
            rate.time_of(FramePos::at_frame(24_000)).unwrap(),
            Rational::from_i128(1001)
        );
        assert_ne!(
            rate.time_of(FramePos::at_frame(24)).unwrap(),
            Rational::from_i128(1)
        );
    }

    #[test]
    fn negative_boundary_selects_the_preceding_frame() {
        let rate = ntsc24();
        let boundary = rate.time_of(FramePos::at_frame(-1)).unwrap();
        let pos = rate.frame_containing(boundary).unwrap();
        assert_eq!(pos, FramePos::at_frame(-1));
        // One tick below the boundary is frame -2, not frame -1 truncated.
        let below = boundary
            .checked_sub(Rational::new(1, 24_000).unwrap())
            .unwrap();
        let pos = rate.frame_containing(below).unwrap();
        assert_eq!(pos.frame, -2);
        assert!(pos.subframe() > Rational::from_i128(0));
    }

    #[test]
    fn subframe_carries_in_both_directions() {
        let three_halves = Rational::new(3, 2).unwrap();
        assert_eq!(
            FramePos::normalized(5, three_halves).unwrap(),
            FramePos::new(6, Rational::new(1, 2).unwrap()).unwrap()
        );
        let minus_third = Rational::new(-1, 3).unwrap();
        assert_eq!(
            FramePos::normalized(5, minus_third).unwrap(),
            FramePos::new(4, Rational::new(2, 3).unwrap()).unwrap()
        );
        // And the strict constructor refuses what the carrying one fixes.
        assert!(matches!(
            FramePos::new(5, three_halves),
            Err(CoreError::SubframeOutOfRange { .. })
        ));
        assert!(matches!(
            FramePos::new(5, Rational::from_i128(1)),
            Err(CoreError::SubframeOutOfRange { .. })
        ));
        assert!(matches!(
            FramePos::new(5, minus_third),
            Err(CoreError::SubframeOutOfRange { .. })
        ));
    }

    #[test]
    fn a_time_has_exactly_one_frame_position() {
        // This is what the private subframe buys. Several out-of-range spellings
        // denote one instant, so they must collapse to one value, one ordering and
        // one time — otherwise (SceneId, Revision, FramePos) cache keys and
        // "sort keys by FramePos" would group by representation instead of by when.
        let rate = ntsc24();
        let half = Rational::new(1, 2).unwrap();
        let canonical = FramePos::new(6, half).unwrap();
        let same_time = rate.time_of(canonical).unwrap();
        for (frame, subframe) in [
            (5, Rational::new(3, 2).unwrap()),
            (7, Rational::new(-1, 2).unwrap()),
            (4, Rational::new(5, 2).unwrap()),
            (8, Rational::new(-3, 2).unwrap()),
            (6, Rational::new(13, 26).unwrap()),
            (6, Rational::new(1, 2).unwrap()),
        ] {
            let got = FramePos::normalized(frame, subframe).unwrap();
            assert_eq!(got, canonical, "({frame}, {subframe}) is not one key");
            assert_eq!(got.cmp(&canonical), Ordering::Equal);
            assert_eq!(rate.time_of(got).unwrap(), same_time);
            // Idempotent, so normalization never oscillates between two forms.
            assert_eq!(
                FramePos::normalized(got.frame, got.subframe()).unwrap(),
                got
            );
        }
    }

    #[test]
    fn deserialization_gates_the_private_field() {
        // Derived Deserialize could reach the private field, which is how a project
        // file would be able to violate the invariant from disk.
        let json = r#"{"frame":6,"subframe":"1/2"}"#;
        let pos: FramePos = serde_json::from_str(json).unwrap();
        assert_eq!(pos, FramePos::new(6, Rational::new(1, 2).unwrap()).unwrap());
        assert_eq!(serde_json::to_string(&pos).unwrap(), json);

        for bad in [
            r#"{"frame":5,"subframe":"3/2"}"#,
            r#"{"frame":5,"subframe":"-1/3"}"#,
            r#"{"frame":5,"subframe":"1/1"}"#,
            r#"{"frame":5,"subframe":"10/20"}"#,
            r#"{"frame":5,"subframe":"1/0"}"#,
            r#"{"frame":5,"subframe":"1/2","extra":0}"#,
            r#"{"frame":"5","subframe":"1/2"}"#,
            r#"{"frame":5}"#,
        ] {
            assert!(
                serde_json::from_str::<FramePos>(bad).is_err(),
                "must reject: {bad}"
            );
        }

        // serde's derived form also accepts a sequence, so `[6,"1/2"]` parses. That
        // is not a hole in the gate — it runs through the same constructor, and the
        // invalid sequence form is refused too. Pinning a project document to the
        // object representation is WP03's schema validation, not this type's job.
        let from_seq: FramePos = serde_json::from_str(r#"[6,"1/2"]"#).unwrap();
        assert_eq!(from_seq, pos);
        assert!(serde_json::from_str::<FramePos>(r#"[5,"3/2"]"#).is_err());
    }

    #[test]
    fn adjacent_frame_intervals_share_endpoints_only() {
        let rate = ntsc24();
        let mut previous: Option<SampleInterval> = None;
        for frame in -50..50i64 {
            let interval = rate.frame_samples(frame, 48_000).unwrap();
            if let Some(prev) = previous {
                assert!(prev.touches(interval), "{prev:?} then {interval:?}");
            }
            previous = Some(interval);
        }
    }

    #[test]
    fn range_samples_equal_the_two_boundaries() {
        let rate = FrameRate::new(30_000, 1001).unwrap();
        let range = FrameRange::new(100, 200).unwrap();
        let span = rate.frame_range_samples(range, 48_000).unwrap();
        let first = rate.frame_samples(100, 48_000).unwrap();
        let last = rate.frame_samples(199, 48_000).unwrap();
        assert_eq!(span.start, first.start);
        assert_eq!(span.end, last.end);
        // The span is in samples, not frames: 100 frames at 30000/1001 is
        // 100*1001/30000 s, which at 48 kHz is exactly 160,160 samples. A decimal
        // 29.97 would have said 160,006.
        assert_eq!(span.len().unwrap(), 160_160);
        assert_eq!(span.len().unwrap(), (last.end - first.start) as u64);
        // And the empty range selects no samples at all.
        let empty = rate
            .frame_range_samples(FrameRange::new(7, 7).unwrap(), 48_000)
            .unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn reversed_ranges_are_invalid() {
        assert!(matches!(
            FrameRange::new(5, 2),
            Err(CoreError::InvalidRange { .. })
        ));
        assert!(FrameRange::new(3, 3).unwrap().is_empty());
        assert_eq!(FrameRange::new(3, 3).unwrap().len().unwrap(), 0);
        assert!(!FrameRange::new(3, 3).unwrap().contains(3));
    }

    #[test]
    fn sample_at_agrees_with_the_frame_boundary() {
        let rate = ntsc24();
        for frame in [0i64, 1, 7, 100, 1001, -3] {
            let interval = rate.frame_samples(frame, 48_000).unwrap();
            assert_eq!(
                rate.sample_at(FramePos::at_frame(frame), 48_000).unwrap(),
                interval.start
            );
        }
    }

    #[test]
    fn tick_conversion_is_checked() {
        assert_eq!(checked_frame_to_ticks(10, 120).unwrap(), 1200);
        assert!(matches!(checked_frame_to_ticks(-1, 120), Ok(-120)));
        assert!(matches!(
            checked_frame_to_ticks(1, 0),
            Err(CoreError::NonPositiveRate { .. })
        ));
        assert!(matches!(
            checked_frame_to_ticks(i64::MAX, 4),
            Err(CoreError::ArithmeticOverflow { .. })
        ));
        let rate = ntsc24();
        assert_eq!(
            tick_duration(rate, 2).unwrap(),
            Rational::new(1001, 48_000).unwrap()
        );
        // The real property: the step duration times the step count per second is
        // one second. `fps * N` for 24000/1001 at two ticks is 48000/1001, and the
        // first draft of this function put the rate's denominator in the wrong
        // half of the fraction, which this catches and a bare 1/48000 assertion
        // would have waved through.
        let steps_per_second = Rational::new(48_000, 1001).unwrap();
        assert_eq!(
            tick_duration(rate, 2)
                .unwrap()
                .checked_mul(steps_per_second)
                .unwrap(),
            Rational::from_i128(1)
        );
        assert_eq!(
            tick_duration(FrameRate::new(24, 1).unwrap(), 120).unwrap(),
            Rational::new(1, 2880).unwrap()
        );
    }

    #[test]
    fn frame_position_order_is_rate_independent() {
        let a = FramePos::new(1, Rational::new(1, 3).unwrap()).unwrap();
        let b = FramePos::new(2, Rational::from_i128(0)).unwrap();
        assert!(a < b);
        assert!(!a.is_whole_frame());
        assert!(b.is_whole_frame());
        assert_eq!(a.next_whole_frame().unwrap(), 2);
        assert_eq!(b.next_whole_frame().unwrap(), 2);
        assert_eq!(b.subframe(), Rational::from_i128(0));
    }
}
