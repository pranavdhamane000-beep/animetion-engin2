//! Checked nonnegative sample offsets, ranges and frame-boundary conversion (spec 17.5,
//! 17.7).
//!
//! WP01's [`anim_core::time`] computes in signed `i64` because the timeline supports
//! negative positions. Export does not: 17.5 asks for "checked nonnegative export
//! SampleOffset and SampleRange wrappers around WP01 signed calculations". This module is
//! that wrapper. It never re-derives the boundary arithmetic — it calls
//! [`FrameRate::frame_range_samples`] at the canonical [`EXPORT_SAMPLE_RATE`] and refuses
//! anything that lands below zero, empty, reversed or beyond the width, so the count
//! `N = end_sample - first_sample` is the exact pre-mux PCM expectation 17.7 names and
//! WP09 compares against.

use anim_core::time::{FrameRange, FrameRate};

use crate::{from_core, AudioError, EXPORT_SAMPLE_RATE};

/// A nonnegative sample index on the export timeline (spec 17.8's `SampleOffset(pub u64)`).
///
/// The inner value is public exactly as 17.8 spells it: a `u64` has one spelling and needs
/// no normalization, so there is no invariant a constructor would protect that the type
/// does not already state. The checked conversions below are where a signed WP01 result
/// becomes an export offset, and they refuse rather than wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SampleOffset(pub u64);

impl SampleOffset {
    /// Wrap an already-nonnegative offset.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Checked conversion from WP01's signed sample calculation. A negative value is
    /// refused (spec 17.7's "negative export samples"), never cast into a huge `u64`.
    ///
    /// # Errors
    ///
    /// [`AudioError::NegativeRange`] when `value < 0`.
    pub fn from_i64(value: i64) -> Result<Self, AudioError> {
        if value < 0 {
            return Err(AudioError::NegativeRange { sample: value });
        }
        Ok(Self(value as u64))
    }

    /// The offset as a `u64`.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A half-open run of export samples `[start, start + len)` (spec 17.8).
///
/// `len` is a count of sample *frames* at [`EXPORT_SAMPLE_RATE`], not interleaved scalars;
/// the stereo channel count is applied by the mixer and the WAV writer, never folded in
/// here, so `len` stays the quantity WP09's pre-mux comparison names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleRange {
    pub start: SampleOffset,
    pub len: u64,
}

impl SampleRange {
    /// A range from its start offset and frame count.
    #[must_use]
    pub const fn new(start: SampleOffset, len: u64) -> Self {
        Self { start, len }
    }

    /// The exclusive end, checked so a full-width range reports overflow instead of
    /// wrapping (the same rule [`FrameRange::len`] follows in WP01).
    ///
    /// # Errors
    ///
    /// [`AudioError::Overflow`] when `start + len` exceeds `u64`.
    pub fn end(&self) -> Result<u64, AudioError> {
        self.start
            .0
            .checked_add(self.len)
            .ok_or_else(|| AudioError::Overflow {
                operation: "sample range end",
                value: format!("{} + {}", self.start.0, self.len),
            })
    }

    /// True when the range selects no samples.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Convert a whole-frame export range to the exact 48 kHz sample range (spec 17.7).
///
/// Uses WP01's [`FrameRate::frame_range_samples`] at both boundaries — the same function
/// `frame_samples` uses, so adjacent frames share an endpoint but never a sample — then
/// checked-converts the signed interval to nonnegative export offsets. `N = len` is the
/// expected pre-mux PCM sample count.
///
/// # Errors
///
/// - [`AudioError::EmptyRange`] for an empty frame range (export refuses zero frames).
/// - [`AudioError::Core`] (`InvalidRange`) for a reversed range.
/// - [`AudioError::InvalidRate`] for a zero frame-rate numerator or denominator.
/// - [`AudioError::NegativeRange`] when the first sample lands below zero.
/// - [`AudioError::Overflow`] when the boundary or the count exceeds its width.
pub fn sample_range(frames: FrameRange, fps: FrameRate) -> Result<SampleRange, AudioError> {
    if frames.is_empty() {
        return Err(AudioError::EmptyRange {
            start: frames.start,
            end: frames.end,
        });
    }
    if frames.start > frames.end {
        return Err(AudioError::Core(
            anim_core::error::CoreError::InvalidRange {
                kind: "frame",
                start: frames.start,
                end: frames.end,
            },
        ));
    }
    // A zero rate would divide by zero inside WP01's BigInt boundary; refuse it here so
    // the fault is a typed audio error rather than a panic. `FrameRate::new` rejects these,
    // but the fields are public, so a hand-built rate can still carry one.
    if fps.num == 0 || fps.den == 0 {
        return Err(AudioError::InvalidRate {
            rate: format!("{}/{}", fps.num, fps.den),
        });
    }
    let interval = fps
        .frame_range_samples(frames, EXPORT_SAMPLE_RATE)
        .map_err(from_core)?;
    let start = SampleOffset::from_i64(interval.start)?;
    let len = interval.len().map_err(from_core)?;
    Ok(SampleRange::new(start, len))
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
    fn sample_offset_refuses_negative_and_keeps_nonnegative() {
        assert_eq!(SampleOffset::from_i64(0).unwrap().get(), 0);
        assert_eq!(SampleOffset::from_i64(48_000).unwrap().get(), 48_000);
        assert!(matches!(
            SampleOffset::from_i64(-1),
            Err(AudioError::NegativeRange { sample: -1 })
        ));
    }

    #[test]
    fn sample_range_end_is_checked() {
        let r = SampleRange::new(SampleOffset::new(u64::MAX - 1), 2);
        assert!(matches!(r.end(), Err(AudioError::Overflow { .. })));
        assert_eq!(
            SampleRange::new(SampleOffset::new(10), 5).end().unwrap(),
            15
        );
        assert!(SampleRange::new(SampleOffset::new(10), 0).is_empty());
    }

    #[test]
    fn integer_rate_converts_exactly() {
        // 24 fps: frame f starts at f/24 s, so sample floor(f*48000/24) = f*2000 exactly.
        let fps = FrameRate::new(24, 1).unwrap();
        let r = sample_range(FrameRange::new(0, 10).unwrap(), fps).unwrap();
        assert_eq!(r.start.get(), 0);
        assert_eq!(r.len, 20_000);
        let r = sample_range(FrameRange::new(3, 7).unwrap(), fps).unwrap();
        assert_eq!(r.start.get(), 6_000);
        assert_eq!(r.len, 8_000);
    }

    #[test]
    fn ntsc_rate_matches_the_known_wp01_value() {
        // 100 frames at 30000/1001 is 100*1001/30000 s = 160160 samples at 48 kHz, the
        // exact value anim_core's own test pins. A decimal 29.97 would say 160006.
        let fps = FrameRate::new(30_000, 1001).unwrap();
        let r = sample_range(FrameRange::new(100, 200).unwrap(), fps).unwrap();
        assert_eq!(r.len, 160_160);
        // start = floor(100*48000*1001/30000) = floor(160160) = 160160.
        assert_eq!(r.start.get(), 160_160);
    }

    #[test]
    fn empty_negative_reversed_and_zero_rate_are_refused() {
        let fps = FrameRate::new(24, 1).unwrap();
        assert!(matches!(
            sample_range(FrameRange::new(5, 5).unwrap(), fps),
            Err(AudioError::EmptyRange { .. })
        ));
        assert!(matches!(
            sample_range(FrameRange::new(-4, 2).unwrap(), fps),
            Err(AudioError::NegativeRange { .. })
        ));
        // A reversed range cannot come from FrameRange::new, so build it by hand.
        let reversed = FrameRange { start: 9, end: 2 };
        assert!(matches!(
            sample_range(reversed, fps),
            Err(AudioError::Core(
                anim_core::error::CoreError::InvalidRange { .. }
            ))
        ));
        assert!(matches!(
            sample_range(FrameRange::new(0, 4).unwrap(), FrameRate { num: 0, den: 1 }),
            Err(AudioError::InvalidRate { .. })
        ));
        assert!(matches!(
            sample_range(
                FrameRange::new(0, 4).unwrap(),
                FrameRate { num: 24, den: 0 }
            ),
            Err(AudioError::InvalidRate { .. })
        ));
    }

    #[test]
    fn a_full_width_range_reports_overflow_not_wrap() {
        // frame i64::MAX at 24 fps would need ~1.8e20 samples, beyond i64; WP01's boundary
        // conversion overflows and the audio wrapper surfaces it as Overflow.
        let fps = FrameRate::new(24, 1).unwrap();
        let r = sample_range(FrameRange::new(0, i64::MAX).unwrap(), fps);
        assert!(matches!(r, Err(AudioError::Overflow { .. })), "{r:?}");
    }
}
