//! Normalized rationals with checked arithmetic (spec 10.6, 10.7, 10.9).
//!
//! One time model serves serialization, keyframes, events, ranges, audio samples
//! and simulation scheduling, so this type is the only place a fraction is
//! allowed to be reduced. The invariants are total, not conventions:
//!
//! * the denominator is strictly positive;
//! * numerator and denominator are coprime;
//! * zero is exactly `0/1`;
//! * arithmetic runs in arbitrary precision and is converted back to the
//!   persisted `i128` width only after reduction, failing rather than wrapping.
//!
//! The last point is why [`Rational::num`] and [`Rational::den`] are private
//! while [`crate::time::FramePos`]'s fields are not: spec 10.8 lists both shapes,
//! and only this one has an invariant a caller could break by assignment.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{Signed, Zero};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::CoreError;

/// An exact fraction held in the persisted width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rational {
    num: i128,
    den: i128,
}

impl Rational {
    /// Build a normalized rational. A zero denominator is rejected rather than
    /// rendered as an infinite or zero value.
    pub fn new(num: i128, den: i128) -> Result<Self, CoreError> {
        if den.is_zero() {
            return Err(CoreError::ZeroDenominator {
                numerator: num.to_string(),
            });
        }
        Self::from_bigint_parts(&BigInt::from(num), &BigInt::from(den))
    }

    /// An integer as a rational. Cannot fail: the denominator is one.
    #[must_use]
    pub const fn from_i128(value: i128) -> Self {
        Self { num: value, den: 1 }
    }

    /// The single construction path every other operation funnels through:
    /// normalize the sign, reduce by the gcd, then check the reduced pair fits
    /// the persisted width. Reduction happens *before* the width check, so
    /// `2^127 / 2^127` is `1/1` rather than an overflow.
    ///
    /// Public because WP04's snapshot hashing and WP05's tick conversion build
    /// rationals straight out of `BigInt` products and must not re-implement this.
    pub fn from_bigint_parts(num: &BigInt, den: &BigInt) -> Result<Self, CoreError> {
        if den.is_zero() {
            return Err(CoreError::ZeroDenominator {
                numerator: num.to_string(),
            });
        }
        let (n, d) = if den.is_negative() {
            (-num.clone(), -den.clone())
        } else {
            (num.clone(), den.clone())
        };
        if n.is_zero() {
            return Ok(Self { num: 0, den: 1 });
        }
        let g = n.gcd(&d);
        let reduced_num = &n / &g;
        let reduced_den = &d / &g;
        // The texts are taken before the values move into the width check, so the
        // error can still name what was rejected.
        let num_text = reduced_num.to_string();
        let den_text = reduced_den.to_string();
        let overflow = |value: String| CoreError::ArithmeticOverflow {
            operation: "rational reduce to persisted width",
            value,
        };
        let num =
            i128::try_from(reduced_num).map_err(|_| overflow(format!("{num_text}/{den_text}")))?;
        let den =
            i128::try_from(reduced_den).map_err(|_| overflow(format!("{num_text}/{den_text}")))?;
        debug_assert!(den > 0, "the sign rule must leave the denominator positive");
        debug_assert!(
            num == 0 || BigInt::from(num).gcd(&BigInt::from(den)) == BigInt::from(1),
            "the pair must be coprime after reduction"
        );
        Ok(Self { num, den })
    }

    /// The numerator, always carrying the sign.
    #[must_use]
    pub const fn num(&self) -> i128 {
        self.num
    }

    /// The denominator, always strictly positive.
    #[must_use]
    pub const fn den(&self) -> i128 {
        self.den
    }

    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.num == 0
    }

    /// True when the value is an exact integer, which is when a frame boundary
    /// falls precisely on a sample or tick boundary.
    #[must_use]
    pub const fn is_integer(&self) -> bool {
        self.den == 1
    }

    pub fn checked_add(self, rhs: Self) -> Result<Self, CoreError> {
        let n = BigInt::from(self.num) * BigInt::from(rhs.den)
            + BigInt::from(rhs.num) * BigInt::from(self.den);
        let d = BigInt::from(self.den) * BigInt::from(rhs.den);
        Self::finish(n, d, "rational add")
    }

    pub fn checked_sub(self, rhs: Self) -> Result<Self, CoreError> {
        self.checked_add(rhs.checked_neg()?)
    }

    pub fn checked_mul(self, rhs: Self) -> Result<Self, CoreError> {
        let n = BigInt::from(self.num) * BigInt::from(rhs.num);
        let d = BigInt::from(self.den) * BigInt::from(rhs.den);
        Self::finish(n, d, "rational multiply")
    }

    /// Dividing by a zero rational would put zero in the denominator, so it
    /// reports [`CoreError::ZeroDenominator`] with the divisor as the offending
    /// value rather than inventing a seventh variant for the same failure.
    pub fn checked_div(self, rhs: Self) -> Result<Self, CoreError> {
        if rhs.is_zero() {
            return Err(CoreError::ZeroDenominator {
                numerator: rhs.to_string(),
            });
        }
        let n = BigInt::from(self.num) * BigInt::from(rhs.den);
        let d = BigInt::from(self.den) * BigInt::from(rhs.num);
        Self::finish(n, d, "rational divide")
    }

    pub fn checked_neg(self) -> Result<Self, CoreError> {
        Self::from_bigint_parts(&-BigInt::from(self.num), &BigInt::from(self.den))
    }

    fn finish(num: BigInt, den: BigInt, operation: &'static str) -> Result<Self, CoreError> {
        Self::from_bigint_parts(&num, &den).map_err(|e| match e {
            // The width failure names the operation that produced it; a caller
            // reading `17014118386048386516920845/3` alone cannot tell whether it
            // came from an add, a multiply or a divide.
            CoreError::ArithmeticOverflow { value, .. } => {
                CoreError::ArithmeticOverflow { operation, value }
            }
            other => other,
        })
    }

    /// Mathematical floor, not truncation. `-1/2` floors to `-1`, which is what
    /// makes a negative exact boundary select the mathematically preceding frame
    /// (spec 10.7) instead of collapsing onto zero.
    pub fn floor_to_i64(self) -> Result<i64, CoreError> {
        let whole = self.num.div_euclid(self.den);
        i64::try_from(whole).map_err(|_| CoreError::ArithmeticOverflow {
            operation: "rational floor to i64",
            value: self.to_string(),
        })
    }

    /// Mathematical ceiling. `ceil(x) == floor(x)` exactly when `x` is an integer,
    /// which is the case that decides whether a covering sample range gains one
    /// endpoint (spec 10.7).
    pub fn ceil_to_i64(self) -> Result<i64, CoreError> {
        let whole = self.num.div_euclid(self.den);
        let remainder = self.num.rem_euclid(self.den);
        let ceil = if remainder.is_zero() {
            whole
        } else {
            whole.checked_add(1).ok_or(CoreError::ArithmeticOverflow {
                operation: "rational ceil to i64",
                value: self.to_string(),
            })?
        };
        i64::try_from(ceil).map_err(|_| CoreError::ArithmeticOverflow {
            operation: "rational ceil to i64",
            value: self.to_string(),
        })
    }

    /// Parse the canonical `"num/den"` form. Strict: the input must already be
    /// normalized, so `"10/20"`, `"5"`, `"+1/2"` and `"05/1"` are all rejected.
    ///
    /// The strictness is not pedantry. A parser that reduced on the way in would
    /// accept two different byte streams for one value, and a saved document
    /// would stop matching the hash of its own bytes.
    pub fn parse_canonical(raw: &str) -> Result<Self, CoreError> {
        let malformed = || CoreError::MalformedCanonical {
            raw: raw.to_owned(),
        };
        let Some((num_text, den_text)) = raw.split_once('/') else {
            return Err(malformed());
        };
        let num = i128::from_str(num_text).map_err(|_| malformed())?;
        let den = i128::from_str(den_text).map_err(|_| malformed())?;
        let parsed = Self::new(num, den).map_err(|e| match e {
            CoreError::ZeroDenominator { .. } => e,
            CoreError::ArithmeticOverflow { .. } => e,
            _ => malformed(),
        })?;
        if parsed.to_string() != raw {
            return Err(malformed());
        }
        Ok(parsed)
    }
}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Rational {
    /// Cross-multiplied in arbitrary precision. Both denominators are positive by
    /// the type invariant, so the comparison never flips sign; `i128` products
    /// would overflow long before a 30000/1001 timeline gets deep.
    fn cmp(&self, other: &Self) -> Ordering {
        let lhs = BigInt::from(self.num) * BigInt::from(other.den);
        let rhs = BigInt::from(other.num) * BigInt::from(self.den);
        lhs.cmp(&rhs)
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

impl FromStr for Rational {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_canonical(s)
    }
}

impl Serialize for Rational {
    /// The canonical wire form is the single string `"num/den"` (spec 10.5), with
    /// both parts always present. Recorded in ADR-0004, together with why this is
    /// not an object of two integers.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rational {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse_canonical(&raw).map_err(serde::de::Error::custom)
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

    #[test]
    fn invariants_hold_for_sign_and_reduction() {
        assert_eq!(Rational::new(10, 20).unwrap().to_string(), "1/2");
        assert_eq!(Rational::new(-10, 20).unwrap().to_string(), "-1/2");
        assert_eq!(Rational::new(10, -20).unwrap().to_string(), "-1/2");
        assert_eq!(Rational::new(0, -5).unwrap().to_string(), "0/1");
        assert_eq!(Rational::new(0, 5).unwrap().to_string(), "0/1");
        assert!(Rational::new(1, 0).is_err());
    }

    #[test]
    fn floor_and_ceil_are_mathematical_not_truncating() {
        assert_eq!(Rational::new(-1, 2).unwrap().floor_to_i64().unwrap(), -1);
        assert_eq!(Rational::new(-1, 2).unwrap().ceil_to_i64().unwrap(), 0);
        assert_eq!(Rational::new(-4, 2).unwrap().floor_to_i64().unwrap(), -2);
        assert_eq!(Rational::new(-4, 2).unwrap().ceil_to_i64().unwrap(), -2);
        assert_eq!(Rational::new(7, 2).unwrap().floor_to_i64().unwrap(), 3);
        assert_eq!(Rational::new(7, 2).unwrap().ceil_to_i64().unwrap(), 4);
    }

    #[test]
    fn ordering_matches_exact_rational_comparison() {
        // 1/3 vs 10000/30001: the decimal intuition says "equal", the cross
        // product says otherwise, and this type must say otherwise.
        let a = Rational::new(1, 3).unwrap();
        let b = Rational::new(10_000, 30_001).unwrap();
        assert!(a > b);
        assert_eq!(
            Rational::new(24_000, 1001).unwrap(),
            Rational::new(48_000, 2002).unwrap()
        );
    }

    #[test]
    fn arithmetic_reduces_before_checking_width() {
        let big = Rational::from_i128(i128::MAX);
        let half = Rational::new(1, 2).unwrap();
        // i128::MAX * 2 would overflow the numerator, but this multiplies by 1/2.
        assert_eq!(big.checked_mul(half).unwrap().den(), 2);

        // MAX/2 + MAX/2 produces intermediates far outside i128 and still lands
        // exactly on i128::MAX, because reduction happens before the width check.
        let step = Rational::new(i128::MAX, 2).unwrap();
        let sum = step.checked_add(step).unwrap();
        assert_eq!(sum, big);
        assert_eq!(sum.checked_div(step).unwrap(), Rational::from_i128(2));
    }

    #[test]
    fn width_overflow_is_reported_not_wrapped() {
        let huge = Rational::from_bigint_parts(
            &(BigInt::from(i128::MAX) * BigInt::from(i128::MAX)),
            &BigInt::from(1),
        );
        assert!(matches!(huge, Err(CoreError::ArithmeticOverflow { .. })));
        let near = Rational::from_i128(i128::MAX);
        assert!(matches!(
            near.checked_add(near),
            Err(CoreError::ArithmeticOverflow {
                operation: "rational add",
                ..
            })
        ));
        assert!(matches!(
            Rational::from_i128(i128::MIN).checked_neg(),
            Err(CoreError::ArithmeticOverflow { .. })
        ));
        assert!(matches!(
            Rational::from_i128(i64::MAX as i128 + 1).floor_to_i64(),
            Err(CoreError::ArithmeticOverflow { .. })
        ));
        assert!(matches!(
            Rational::from_i128(i64::MAX as i128 + 1).ceil_to_i64(),
            Err(CoreError::ArithmeticOverflow { .. })
        ));
        // A value whose floor fits and whose ceiling is one above it: (2^64-1)/2 is
        // i64::MAX + 1/2, so floor_to_i64 is i64::MAX and ceil_to_i64 has nowhere
        // to go. The two must not be conflated by a truncating cast.
        let edge = Rational::new(i64::MAX as i128 * 2 + 1, 2).unwrap();
        assert_eq!(edge.floor_to_i64().unwrap(), i64::MAX);
        assert!(matches!(
            edge.ceil_to_i64(),
            Err(CoreError::ArithmeticOverflow { .. })
        ));
        assert_eq!(
            Rational::new(i64::MAX as i128, 1)
                .unwrap()
                .ceil_to_i64()
                .unwrap(),
            i64::MAX
        );
    }

    #[test]
    fn canonical_text_round_trips_and_rejects_non_canonical_forms() {
        for raw in ["0/1", "-1/2", "24000/1001", "-24000/1001", "1/1000000"] {
            let parsed = Rational::parse_canonical(raw).unwrap();
            assert_eq!(parsed.to_string(), raw);
        }
        for raw in [
            "10/20", "5", "+1/2", "05/1", "1/2/3", "", "1/", "/2", "1/0", "1/-2",
        ] {
            assert!(
                Rational::parse_canonical(raw).is_err(),
                "must reject non-canonical input: {raw:?}"
            );
        }
        // The zero-denominator case keeps its own error rather than collapsing
        // into a generic parse failure, because the caller's bug differs.
        assert!(matches!(
            Rational::parse_canonical("1/0"),
            Err(CoreError::ZeroDenominator { .. })
        ));
    }

    #[test]
    fn json_is_a_single_string() {
        let r = Rational::new(-24_000, 1001).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, "\"-24000/1001\"");
        assert_eq!(serde_json::from_str::<Rational>(&json).unwrap(), r);
        assert!(serde_json::from_str::<Rational>("\"-48000/2002\"").is_err());
        assert!(serde_json::from_str::<Rational>("1.5").is_err());
    }

    #[test]
    fn division_by_zero_names_the_divisor() {
        let e = Rational::from_i128(1)
            .checked_div(Rational::from_i128(0))
            .unwrap_err();
        assert!(matches!(e, CoreError::ZeroDenominator { .. }));
        assert!(e.to_string().contains('0'), "{e}");
    }
}
