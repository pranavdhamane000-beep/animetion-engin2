//! `SimTick`, fixed rate and exact frame/tick conversion (spec 14.3, 14.6, 14.7).
//!
//! The clock is where 14.1's "results must not depend on rendering rate" is either won or
//! lost, so it holds no mutable time at all. Every value it produces is a pure function of
//! the tick asked about, the frame rate and `ticks_per_frame`, computed in
//! [`anim_core::rational::Rational`] and never accumulated: a tick reached by stepping a
//! thousand times and a tick reached by converting its frame directly name the same instant
//! because neither path carries a running total.
//!
//! Two decisions in this file are load-bearing beyond it.
//!
//! **The step duration is computed once, in [`SimulationClock::new`], and stored.** 14.6's
//! sentence — "The step duration is computed once from exact configuration as `1/(fps*N)`
//! and converted to the declared floating-point working type; mutable seconds are never
//! accumulated" — is the only place in the spec that says where a float may enter the
//! simulation, and recomputing it per tick would still be deterministic. Storing it is what
//! makes the *single* conversion structural rather than a habit: there is one value,
//! produced at one time, and an integrator that wants `dt` cannot accidentally produce a
//! slightly different one.
//!
//! **Conversion reuses [`anim_core::time::tick_duration`] and
//! [`anim_core::time::checked_frame_to_ticks`] rather than restating them.** WP01 wrote and
//! property-tested both, and `BUILD_STATE.md` says using them is the requirement, not
//! rewriting them. That is not conservatism: `tick_duration`'s own comment records that
//! dropping the rate's denominator "turns `1001/48000` into `1/48000`, which is a 1001×
//! error in every step size", which was a defect WP01's test suite found in this
//! repository's actual code.

use anim_core::error::CoreError;
use anim_core::rational::Rational;
use anim_core::time::{checked_frame_to_ticks, tick_duration, FramePos, FrameRate};

use crate::{simulation_rate, RandomError, Result, SimError};

/// One position on the simulation timeline (spec 14.8).
///
/// Signed, per 14.7: "SimTick is signed for timeline arithmetic, but stateful replay
/// supports ticks at or above zero." The sign is what lets a negative timeline position
/// convert at all — WP01 established that floor rather than truncation governs negative
/// time, and a tick type that could only be non-negative would have forced the clock to
/// disagree with `FramePos` about where frame `-1` begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SimTick(pub i64);

impl SimTick {
    /// The initialized state before any simulation step (14.6: "`SimTick(0)` is the
    /// initialized state").
    pub const ZERO: Self = Self(0);

    /// Advance by a checked number of ticks.
    ///
    /// # Errors
    ///
    /// [`SimError::TickOverflow`] at the `i64` bound. Never wraps (spec 10.11).
    pub fn checked_add_ticks(self, delta: i64) -> Result<Self> {
        self.0
            .checked_add(delta)
            .map(Self)
            .ok_or_else(|| SimError::TickOverflow {
                operation: "tick advance",
                value: format!("{} + {delta}", self.0),
            })
    }

    /// The number of ticks from `self` to `other`, checked, negative if `other` is earlier.
    pub fn checked_ticks_to(self, other: Self) -> Result<i64> {
        other
            .0
            .checked_sub(self.0)
            .ok_or_else(|| SimError::TickOverflow {
                operation: "tick distance",
                value: format!("{} - {}", other.0, self.0),
            })
    }

    /// Whether this tick lies before the zero a replay may begin from (14.7).
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// The tick as the two 32-bit Philox counter words, little-endian.
    ///
    /// The low word first, and the sign checked rather than cast: 14.7's counter is
    /// `(tick_low, tick_high, draw_index, 0)`, so a negative tick silently cast to `u64`
    /// would be a *valid-looking* address that only this build's wrapping rule reproduces —
    /// the loud-failure-over-silent-difference class DECISIONS.md #18 argues the same way
    /// about adapter identity.
    pub const fn counter_words(self) -> std::result::Result<(u32, u32), RandomError> {
        if self.0 < 0 {
            return Err(RandomError::NegativeTick { tick: self.0 });
        }
        let unsigned = self.0 as u64;
        Ok((unsigned as u32, (unsigned >> 32) as u32))
    }
}

impl std::fmt::Display for SimTick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tick {}", self.0)
    }
}

/// The fixed conversion between the timeline and the simulation timeline (spec 14.8).
///
/// The three public fields are 14.8's, and `rate` is derived rather than authored: 14.2
/// says the simulation rate *equals* `frame_rate * ticks_per_frame`, so a clock that could
/// hold an unrelated `rate` would be two sources of truth for one number and
/// `scheduled_tick = ceil(t * simulation_rate)` would depend on which one a caller read.
/// The private field is the one step duration 14.6 asks for.
#[derive(Clone, Copy, Debug)]
pub struct SimulationClock {
    /// The timeline's rate, exact and rational (spec 10.6).
    pub frame_rate: FrameRate,
    /// Ticks simulated per frame. Nonzero, checked in [`SimulationClock::new`] (14.2).
    pub ticks_per_frame: u32,
    /// `frame_rate * ticks_per_frame`, checked (14.2).
    pub rate: FrameRate,
    /// `1/(fps*N)` in the working type, computed once in `new` (14.6).
    step_seconds: f64,
}

impl SimulationClock {
    /// Build the clock for one project's rate and one `ticks_per_frame`.
    ///
    /// # Errors
    ///
    /// [`SimError::InvalidRate`] for a zero tick count or a product that does not fit, and
    /// the [`SimError::Core`] of any exact-arithmetic failure behind either.
    pub fn new(frame_rate: FrameRate, ticks_per_frame: u32) -> Result<Self> {
        let rate = simulation_rate(frame_rate, ticks_per_frame)?;
        let exact = tick_duration(frame_rate, ticks_per_frame)?;
        Ok(Self {
            frame_rate,
            ticks_per_frame,
            rate,
            step_seconds: exact.to_f64(),
        })
    }

    /// The step duration as the exact rational it was derived from.
    ///
    /// Asked of [`anim_core::time::tick_duration`] again rather than stored a second time,
    /// so the workspace has one formula for it.
    pub fn exact_step_duration(&self) -> Result<Rational> {
        tick_duration(self.frame_rate, self.ticks_per_frame).map_err(Into::into)
    }

    /// The step duration in the working type: the one conversion 14.6 permits.
    #[must_use]
    pub const fn step_seconds(&self) -> f64 {
        self.step_seconds
    }

    /// The first tick of whole frame `f`, which 14.6 says is `f * N`.
    ///
    /// # Errors
    ///
    /// [`SimError::TickOverflow`] when `f * N` does not fit an `i64`, and
    /// [`SimError::InvalidRate`] if this clock somehow carries a zero tick count — which
    /// [`SimulationClock::new`] refuses, so the second is reachable only through a clock
    /// built by field assignment, and is reported as the rate problem it is rather than as
    /// an arithmetic one.
    pub fn tick_of_frame(&self, frame: i64) -> Result<SimTick> {
        checked_frame_to_ticks(frame, self.ticks_per_frame)
            .map(SimTick)
            .map_err(|e| match e {
                CoreError::NonPositiveRate { .. } => SimError::InvalidRate {
                    which: "ticks_per_frame",
                    detail: "is zero, so no frame maps to a tick".to_string(),
                },
                other => SimError::TickOverflow {
                    operation: "frame to tick",
                    value: format!("{frame}: {other}"),
                },
            })
    }

    /// The tick an event at this timeline position schedules to.
    ///
    /// 14.7: "For event time `t`, compute `scheduled_tick = ceil(t * simulation_rate)`.
    /// Events at an exact tick boundary schedule to that tick." Ceiling is the whole
    /// difference between a spawn that runs before its authored instant and one that runs
    /// at or after it, which is what lets [`crate::events`] prove its own age input
    /// non-negative instead of trusting a caller.
    pub fn tick_of_position(&self, position: FramePos) -> Result<SimTick> {
        let t = self.frame_rate.time_of(position)?;
        let scaled = t.checked_mul(self.rate.as_rational()?)?;
        let ticks = scaled.ceil_to_i64().map_err(|_| SimError::TickOverflow {
            operation: "event time to tick",
            value: format!("{t} / {}", self.rate_text()),
        })?;
        Ok(SimTick(ticks))
    }

    /// The exact time of a tick.
    ///
    /// Composed from the frame and the remainder within it rather than by dividing the tick
    /// by the rate, so the value comes out of `FrameRate::time_of` — the one exact
    /// conversion WP01 property-tested — instead of beside it. This is the quantity 14.7's
    /// `age = time_of_tick(k) - t` subtracts from.
    pub fn time_of_tick(&self, tick: SimTick) -> Result<Rational> {
        let position = self.position_of_tick(tick)?;
        Ok(self.frame_rate.time_of(position)?)
    }

    /// The whole frame that contains a tick, floor-divided (14.6: "whole frame `f` begins at
    /// tick `f*N`").
    #[must_use]
    pub const fn frame_of_tick(&self, tick: SimTick) -> i64 {
        tick.0.div_euclid(self.ticks_per_frame as i64)
    }

    /// The timeline position of a tick: its frame plus the exact fraction of the frame it
    /// sits at.
    ///
    /// `div_euclid`/`rem_euclid` are what make this correct below zero. A tick at `-1` with
    /// four ticks per frame is three quarters of the way through frame `-1`, not a quarter
    /// of the way into frame `0` approached from below — the same reason `FramePos` keeps
    /// its subframe non-negative (DECISIONS.md #22).
    pub fn position_of_tick(&self, tick: SimTick) -> Result<FramePos> {
        let divisor = i64::from(self.ticks_per_frame);
        let frame = tick.0.div_euclid(divisor);
        let remainder = tick.0.rem_euclid(divisor);
        let subframe = Rational::new(i128::from(remainder), i128::from(divisor)).map_err(|_| {
            SimError::TickOverflow {
                operation: "tick subframe",
                value: format!("{tick}"),
            }
        })?;
        Ok(FramePos::new(frame, subframe)?)
    }

    /// `num/den` as text, for the error reports that need to name a rate without `FrameRate`
    /// growing a `Display` impl in someone else's crate.
    #[must_use]
    fn rate_text(&self) -> String {
        format!("{}/{}", self.rate.num, self.rate.den)
    }
}

impl PartialEq for SimulationClock {
    /// Over the three fields 14.8 declares.
    ///
    /// `step_seconds` is left out because it is a pure function of the other three, and
    /// including a float in an equality the exact rationals already decide would let a
    /// future change to the conversion rule make two identical clocks unequal.
    fn eq(&self, other: &Self) -> bool {
        self.frame_rate == other.frame_rate
            && self.ticks_per_frame == other.ticks_per_frame
            && self.rate == other.rate
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

    fn ntsc() -> FrameRate {
        FrameRate::new(30000, 1001).unwrap()
    }

    #[test]
    fn a_zero_tick_rate_is_refused_rather_than_multiplied_into_nothing() {
        for error in [
            SimulationClock::new(FrameRate::new(24, 1).unwrap(), 0).unwrap_err(),
            SimulationClock::new(ntsc(), 0).unwrap_err(),
        ] {
            assert!(
                matches!(error, SimError::InvalidRate { which, .. } if which == "ticks_per_frame"),
                "14.2's nonzero requirement is checked at the clock too, not only in the \
                 settings type that already refuses it: {error}"
            );
        }
    }

    #[test]
    fn the_step_duration_is_the_exact_rational_it_was_derived_as() {
        // The oracle is the formula written out as BigInt arithmetic in this test, not a
        // call to `tick_duration` or `exact_step_duration`. That distinction is the test:
        // WP01 found a 1001x error in this very quantity by hand-computing the expectation,
        // and an oracle that asks the code what it did reports a wrong number as correct
        // (DECISIONS.md #32).
        //
        // fps = 30000/1001, N = 4.  dt = 1 / ((30000/1001) * 4) = 1001 / 120000.
        let clock = SimulationClock::new(ntsc(), 4).unwrap();
        assert_eq!(
            clock.exact_step_duration().unwrap(),
            Rational::new(1001, 120000).unwrap(),
            "the rate's denominator belongs in the numerator of the step"
        );
        // 24 fps at one tick per frame is the case where a dropped denominator is
        // invisible, which is why the NTSC case above is the one that carries the proof.
        let plain = SimulationClock::new(FrameRate::new(24, 1).unwrap(), 1).unwrap();
        assert_eq!(
            plain.exact_step_duration().unwrap(),
            Rational::new(1, 24).unwrap()
        );
        // And N = 4 at 24 fps divides the frame exactly, so dt is 1/96 rather than a
        // repeating binary fraction of anything.
        let quad = SimulationClock::new(FrameRate::new(24, 1).unwrap(), 4).unwrap();
        assert_eq!(
            quad.exact_step_duration().unwrap(),
            Rational::new(1, 96).unwrap()
        );

        // The stored working value is that rational converted once, and once only.
        assert_eq!(
            clock.step_seconds(),
            Rational::new(1001, 120000).unwrap().to_f64()
        );
    }

    #[test]
    fn whole_frame_boundaries_land_on_multiples_of_n() {
        let clock = SimulationClock::new(ntsc(), 4).unwrap();
        for frame in [0i64, 1, 7, 240, -1, -12] {
            assert_eq!(
                clock.tick_of_frame(frame).unwrap().0,
                frame * 4,
                "14.6: whole frame f begins at tick f*N"
            );
            assert_eq!(
                clock.frame_of_tick(SimTick(frame * 4)),
                frame,
                "and the inverse holds at a boundary"
            );
        }
    }

    #[test]
    fn the_rate_is_the_product_and_not_a_second_authorable_number() {
        let clock = SimulationClock::new(ntsc(), 4).unwrap();
        assert_eq!(clock.rate, FrameRate::new(120000, 1001).unwrap());
        let expected = clock
            .frame_rate
            .as_rational()
            .unwrap()
            .checked_mul(Rational::from_i128(4))
            .unwrap();
        assert_eq!(
            clock.rate.as_rational().unwrap(),
            expected,
            "14.2 states the rate as an equality, so it is not free to disagree with the \
             clock's own two inputs"
        );
    }

    #[test]
    fn a_product_that_does_not_fit_is_refused_rather_than_wrapped() {
        let error = SimulationClock::new(FrameRate::new(4_000_000_000, 1).unwrap(), 4).unwrap_err();
        assert!(
            matches!(error, SimError::InvalidRate { which, .. } if which == "simulation rate"),
            "{error}"
        );
    }

    #[test]
    fn an_event_schedules_to_the_tick_at_or_after_its_exact_time() {
        let clock = SimulationClock::new(FrameRate::new(24, 1).unwrap(), 4).unwrap();
        // An exact boundary schedules to that boundary — 14.7's second sentence, and the
        // reason the rule is ceiling rather than round or truncate.
        assert_eq!(
            clock.tick_of_position(FramePos::at_frame(3)).unwrap(),
            SimTick(12)
        );
        // One tick's worth of subframe past the boundary is no longer on it, so it moves up.
        let just_after = FramePos::new(3, Rational::new(1, 96).unwrap()).unwrap();
        assert_eq!(clock.tick_of_position(just_after).unwrap(), SimTick(13));
        // Halfway through frame 3 is 3.5 frames, and four ticks per frame puts it at tick
        // 14 exactly -- on the boundary, so ceiling keeps it there.
        let middle = FramePos::new(3, Rational::new(1, 2).unwrap()).unwrap();
        assert_eq!(clock.tick_of_position(middle).unwrap(), SimTick(14));
        // And a subframe position that is not a tick multiple moves up, never down: an
        // event whose spawn ran before its authored instant would be history that never
        // happened.
        let offset = FramePos::new(0, Rational::new(1, 7).unwrap()).unwrap();
        assert!(clock.tick_of_position(offset).unwrap() > SimTick(0));
    }

    #[test]
    fn tick_and_position_are_inverses_across_a_negative_boundary() {
        let clock = SimulationClock::new(FrameRate::new(24, 1).unwrap(), 4).unwrap();
        for tick in [-9i64, -8, -5, -4, -1, 0, 1, 6, 100] {
            let position = clock.position_of_tick(SimTick(tick)).unwrap();
            assert_eq!(
                clock.tick_of_position(position).unwrap(),
                SimTick(tick),
                "tick {tick} must round-trip through its own position"
            );
        }
        // The subframe stays non-negative below zero, which is #22's rule reaching the
        // clock: tick -1 of four is three quarters through frame -1, not frame 0.
        let position = clock.position_of_tick(SimTick(-1)).unwrap();
        assert_eq!(position.frame, -1);
        assert_eq!(position.subframe(), Rational::new(3, 4).unwrap());
    }

    #[test]
    fn the_time_of_a_tick_is_the_time_of_its_position() {
        let clock = SimulationClock::new(ntsc(), 4).unwrap();
        for tick in [0i64, 1, 5, 84, 1000] {
            let position = clock.position_of_tick(SimTick(tick)).unwrap();
            assert_eq!(
                clock.time_of_tick(SimTick(tick)).unwrap(),
                clock.frame_rate.time_of(position).unwrap(),
                "the spawn-age arithmetic subtracts this value from an event time, so both \
                 spellings have to be one number"
            );
        }
        // Tick 1 at NTSC/4 is dt itself, arriving from the other direction.
        assert_eq!(
            clock.time_of_tick(SimTick(1)).unwrap(),
            Rational::new(1001, 120000).unwrap()
        );
        // Tick 0 is the origin exactly, which is what makes `SimTick(0)` the initialized
        // state rather than a step that has already happened.
        assert_eq!(
            clock.time_of_tick(SimTick(0)).unwrap(),
            Rational::from_i128(0)
        );
    }

    #[test]
    fn counter_words_split_little_endian_and_refuse_a_negative_tick() {
        assert_eq!(SimTick(1).counter_words().unwrap(), (1, 0));
        assert_eq!(
            SimTick(i64::from(u32::MAX) + 1).counter_words().unwrap(),
            (0, 1),
            "the low word fills first, which is what makes the split a little-endian one"
        );
        assert_eq!(
            SimTick(i64::from(u32::MAX)).counter_words().unwrap(),
            (u32::MAX, 0)
        );
        assert!(matches!(
            SimTick(-1).counter_words(),
            Err(RandomError::NegativeTick { tick: -1 })
        ));
    }

    #[test]
    fn tick_arithmetic_reports_instead_of_wrapping() {
        assert!(matches!(
            SimTick(i64::MAX).checked_add_ticks(1),
            Err(SimError::TickOverflow { .. })
        ));
        assert!(matches!(
            SimTick(i64::MIN).checked_ticks_to(SimTick(1)),
            Err(SimError::TickOverflow { .. })
        ));
        assert_eq!(SimTick(5).checked_ticks_to(SimTick(9)).unwrap(), 4);
        assert_eq!(SimTick(9).checked_ticks_to(SimTick(5)).unwrap(), -4);
        assert!(SimTick(-1).is_negative());
        assert!(!SimTick(0).is_negative());
    }
}
