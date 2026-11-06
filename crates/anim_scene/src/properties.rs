//! Key lookup and interpolation (spec 13.3, 13.6, 13.7).
//!
//! Spec 13.7's interpolation paragraph is four sentences, and this file is those four
//! sentences in order, because each rules out a different way of getting it wrong:
//!
//! 1. "Binary-search the ordered keys." [`PropertyCurve`] already stores its keys sorted
//!    and unique — 13.2's rule, and the reason DECISIONS.md #22 makes that field private —
//!    so the search is a `partition_point` over a slice rather than a sort per query.
//! 2. "At exact key time return that key." Checked on the way in, not as a special case at
//!    the end, so an authored value at a boundary is reproduced rather than re-derived from
//!    whichever side of the interval a rounding chose.
//! 3. "Calculate `u = (t - t0) / (t1 - t0)` exactly." Every step here is rational
//!    arithmetic, including `u` and the easing applied to it. The frame rate never enters:
//!    `u` is a ratio of two exact times, so it has the same value measured in seconds or in
//!    frames, and measuring it in frames keeps floats out of the path entirely. 13.6's
//!    "never accumulates seconds from the previous frame" is the failure this avoids.
//! 4. "Apply the declared easing using a versioned numeric algorithm." [`eased_fraction`]
//!    is that algorithm, and its version is
//!    [`anim_project::EASING_ALGORITHM_VERSION`], which
//!    [`crate::snapshot::snapshot_hash`] places in every snapshot's pre-image so that
//!    changing how a curve is *read* cannot leave a digest untouched.
//!
//! The float appears in [`value_as_f64`], and nowhere else in this file.

use anim_core::error::CoreError;
use anim_core::rational::Rational;
use anim_core::time::FramePos;
use anim_project::{Easing, EasingCurve, PropertyCurve, PropertyKey, PropertyValue};

use crate::{position_label, Result, SceneError};

/// One position as a count of frames: `frame + subframe`, exactly.
///
/// `FramePos` has no arithmetic of its own and should not: `(frame, normalized subframe)`
/// is the canonical form that makes it a unique cache key (DECISIONS.md #22). Interpolation
/// needs the number that pair denotes, and this is the conversion — an addition over
/// `i128`, not a float.
///
/// # Errors
///
/// [`SceneError::TimeOverflow`] if the frame number does not fit the rational width, or if
/// the sum exceeds it.
pub fn frame_count(position: &FramePos) -> Result<Rational> {
    // `i64` to `i128` cannot fail, so the conversion is the infallible one and the only
    // width question left is the addition — which is where a frame number near the edge
    // of `i64` actually reports.
    rational(
        "frame plus subframe",
        &position_label(position),
        Rational::from_i128(i128::from(position.frame)).checked_add(position.subframe()),
    )
}

/// The exact interval fraction `u = (t - t0) / (t1 - t0)`, in frame units.
///
/// A zero-width interval would divide by zero, and it cannot arrive from a validated
/// curve: 13.2 gives each property unique canonical times, which `PropertyCurve::new`
/// enforces and its `Deserialize` re-checks. So `checked_div`'s `ZeroDenominator` is left
/// to surface as [`SceneError::Core`] naming the arithmetic, rather than dressed up as a
/// time that overflowed — a report that blamed time for a division would send a reader to
/// the wrong file.
///
/// # Errors
///
/// [`SceneError::TimeOverflow`] on a width failure, [`SceneError::Core`] on a zero
/// denominator.
pub fn interval_fraction(t: &FramePos, start: &FramePos, end: &FramePos) -> Result<Rational> {
    let at = frame_count(t)?;
    let lower = frame_count(start)?;
    let upper = frame_count(end)?;
    let span = rational(
        "interval span",
        &format!("{}..{}", position_label(start), position_label(end)),
        upper.checked_sub(lower),
    )?;
    let offset = rational(
        "interval offset",
        &format!("{}..{}", position_label(start), position_label(end)),
        at.checked_sub(lower),
    )?;
    rational(
        "interval fraction",
        &format!("{}..{}", position_label(start), position_label(end)),
        offset.checked_div(span),
    )
}

/// The versioned numeric algorithm spec 13.7 names: the eased fraction for `u`.
///
/// - [`Easing::Linear`] is the identity — `u` is already the fraction.
/// - [`Easing::Step`] is not an interpolation. 13.6 states it as a selection rule ("Step
///   properties select the preceding key"), and [`sample_property`] applies it before
///   calling this function, so a `Step` that arrives here means a caller skipped that step
///   and there is no interval to ease.
/// - [`Easing::Curve`] is the table lookup: locate the segment by the table's own spacing
///   rule, take its exact fraction, lerp the two samples. Every one of those is rational,
///   so a table symmetric about its midpoint yields a symmetric result rather than one that
///   depends on how a float rounded.
///
/// # Errors
///
/// [`SceneError::InvalidEasing`] when `easing` is `Step`, which is not an easing of a
/// fraction at all. It is *not* raised for a table with bad endpoints: [`EasingCurve::new`]
/// refuses those and its `Deserialize` runs `new`, so no evaluated curve can hold one — see
/// `ease_through_table`'s comment for why re-checking that here would be an unreachable
/// branch rather than a defence. Width failures arrive as [`SceneError::TimeOverflow`], and
/// a sample index outside the table arrives as `InvalidEasing` naming the index.
pub fn eased_fraction(u: &Rational, easing: &Easing) -> Result<Rational> {
    match easing {
        Easing::Linear => Ok(*u),
        Easing::Step => Err(SceneError::InvalidEasing {
            property: String::new(),
            position: u.to_string(),
            detail: "a Step interval has no fraction to ease; it selects the preceding key, \
                     and spec 13.6 states that as a selection rule rather than an \
                     interpolation"
                .to_string(),
        }),
        Easing::Curve(curve) => ease_through_table(u, curve),
    }
}

fn ease_through_table(u: &Rational, curve: &EasingCurve) -> Result<Rational> {
    let samples = curve.points();
    // No endpoint re-check here, and the reason belongs in the file rather than in a
    // comment somewhere else: `EasingCurve::new` refuses a table that does not begin at
    // `0/1` and end at `1/1`, and its `Deserialize` runs `new`, so no evaluated curve can
    // hold a table the constructor would have rejected. 13.7's "invalid easing parameters
    // are validation errors" is enforced where the parameters arrive, which is
    // `anim_project`, and that is where its test lives too.
    //
    // What stays reachable at this level is the condition the type cannot see, because its
    // two halves are authored in two different objects: a shaping easing declared on a
    // value with no interval arithmetic. `interpolate` raises that, and the test for it is
    // below.
    //
    // The table's own spacing rule supplies the segment bounds, so the search is over the
    // sample positions rather than over an index arithmetic this file would have to
    // re-derive and could get wrong in the last slot.
    let mut segment = samples.len() - 2;
    for index in 1..samples.len() {
        if curve_position(curve, index)? >= *u {
            segment = index - 1;
            break;
        }
    }
    let lower = curve_position(curve, segment)?;
    let upper = curve_position(curve, segment + 1)?;
    let span = rational(
        "easing table span",
        &format!("{lower}..{upper}"),
        upper.checked_sub(lower),
    )?;
    let offset = rational("easing table offset", &u.to_string(), u.checked_sub(lower))?;
    let within = rational(
        "easing table fraction",
        &u.to_string(),
        offset.checked_div(span),
    )?;
    let low = samples[segment];
    let high = samples[segment + 1];
    let rise = rational(
        "easing table rise",
        &format!("{low}..{high}"),
        high.checked_sub(low),
    )?;
    let moved = rational(
        "easing table move",
        &u.to_string(),
        rise.checked_mul(within),
    )?;
    rational(
        "easing table result",
        &u.to_string(),
        low.checked_add(moved),
    )
}

/// A sample's input position, from the table's own spacing rule.
///
/// The `ProjectError` this returns is re-labelled rather than chained: the curve is a
/// field of a document that already validated itself, so the only way to fail here is to
/// address a sample that does not exist, which is an easing problem a report should file
/// under 13.11's easing category with the sample number in it.
fn curve_position(curve: &EasingCurve, index: usize) -> Result<Rational> {
    curve
        .position_of(index)
        .map_err(|e| SceneError::InvalidEasing {
            property: String::new(),
            position: index.to_string(),
            detail: format!("the easing table has no sample {index}: {e}"),
        })
}

/// The value of one property at one exact position (spec 13.6, 13.7).
///
/// `None` means the property has no keys *and* no authored default. That is not an error at
/// this level: 13.6's sentence is about which value a property takes, and whether a given
/// node field is allowed to have none is a question about the field.
/// [`crate::animation`] answers that, and raises
/// [`SceneError::InvalidPropertyKeys`] for the fields that must have a value.
///
/// # Errors
///
/// What [`interval_fraction`] and [`eased_fraction`] raise, plus
/// [`SceneError::InvalidEasing`] when a non-`Step` easing is declared on a value with no
/// interval arithmetic — a bool, a label, an asset reference. That last case is why 13.11's
/// easing failure is an evaluation error and not only a load-time one: the two halves of
/// the condition are authored in two different objects, a key's value type and that key's
/// easing, and neither type can see the other.
pub fn sample_property(curve: &PropertyCurve, at: &FramePos) -> Result<Option<PropertyValue>> {
    let keys = curve.keys();
    if keys.is_empty() {
        return Ok(curve.authored_default.clone());
    }
    // 13.6, twice over in one sentence: "Before the first key, use the first value; after
    // the last, use the last." No extrapolation mode exists yet — WP15's spec 30.7 names
    // pre- and post-extrapolation as its own addition — so holding the end value is the
    // whole behaviour, not a stand-in for one.
    if at <= &keys[0].position {
        return Ok(Some(keys[0].value.clone()));
    }
    let final_index = keys.len() - 1;
    if at >= &keys[final_index].position {
        return Ok(Some(keys[final_index].value.clone()));
    }
    // `at` is strictly between the ends, so the partition point is in `1..final_index`.
    let following = keys.partition_point(|key| &key.position < at);
    let after = &keys[following];
    if &after.position == at {
        return Ok(Some(after.value.clone()));
    }
    let before = &keys[following - 1];
    if matches!(after.easing, Easing::Step) {
        return Ok(Some(before.value.clone()));
    }
    let u = interval_fraction(at, &before.position, &after.position)?;
    let eased = eased_fraction(&u, &after.easing).map_err(|e| with_position(e, after))?;
    interpolate(before, after, &eased).map(Some)
}

/// Put the key's own position into an easing refusal.
///
/// [`eased_fraction`] is useful on its own and therefore cannot know which key it was
/// asked about; this is the one place that does, and 13.11's "the offending object" rule
/// (DECISIONS.md #29) means the position has to be attached before the error leaves here
/// rather than left for a report to reconstruct.
fn with_position(error: SceneError, after: &PropertyKey) -> SceneError {
    match error {
        SceneError::InvalidEasing {
            property, detail, ..
        } => SceneError::InvalidEasing {
            property,
            position: position_label(&after.position),
            detail,
        },
        other => other,
    }
}

fn interpolate(
    before: &PropertyKey,
    after: &PropertyKey,
    eased: &Rational,
) -> Result<PropertyValue> {
    match (&before.value, &after.value) {
        (PropertyValue::Rational(low), PropertyValue::Rational(high)) => {
            // `v0 + (v1 - v0) * eased`, exactly. The result is still a rational, and the
            // caller that needs an `f64` asks for it through `value_as_f64` — which is the
            // only float in the path and is 13.6's last clause satisfied.
            let rise = rational(
                "value rise",
                &format!("{low}..{high}"),
                high.checked_sub(*low),
            )?;
            let moved = rational("value move", &eased.to_string(), rise.checked_mul(*eased))?;
            let blended = rational("value blend", &low.to_string(), low.checked_add(moved))?;
            Ok(PropertyValue::Rational(blended))
        }
        (PropertyValue::Bool(_), PropertyValue::Bool(_))
        | (PropertyValue::Text(_), PropertyValue::Text(_))
        | (PropertyValue::Asset(_), PropertyValue::Asset(_)) => Err(SceneError::InvalidEasing {
            property: String::new(),
            position: position_label(&after.position),
            detail: format!(
                "a {} switches at a key and has no interval to ease; author Step on the \
                 following key so the preceding value is held",
                before.value.kind_name()
            ),
        }),
        // The catch-all rather than a guard on "the kinds differ", which the compiler
        // cannot use to discharge the other nine pairs: any remaining combination is two
        // different value types in one key list, which is a key-list fault and not an
        // easing one.
        (low, high) => Err(SceneError::InvalidPropertyKeys {
            node: String::new(),
            property: String::new(),
            detail: format!(
                "the key at frame {} holds a {} and the one at frame {} holds a {}, and \
                 one property's key list is one value type",
                position_label(&before.position),
                low.kind_name(),
                position_label(&after.position),
                high.kind_name()
            ),
        }),
    }
}

/// The one float conversion in the evaluation path (spec 13.6's last clause).
///
/// # Errors
///
/// [`SceneError::InvalidNumericValue`] when the property read as a number holds a bool, a
/// label or an asset reference. This is 13.11's numeric variant rather than the easing one
/// because the easing may be perfectly well-formed: it is the *value* that cannot be a
/// number, and blaming the curve would send a reader to the wrong field.
pub fn value_as_f64(value: &PropertyValue, field: &'static str, subject: &str) -> Result<f64> {
    match value {
        PropertyValue::Rational(number) => Ok(number.to_f64()),
        other => Err(SceneError::InvalidNumericValue {
            field,
            subject: subject.to_string(),
            detail: format!(
                "the property holds a {} and cannot be read as a number",
                other.kind_name()
            ),
        }),
    }
}

/// Route a rational step: an overflow is 13.11's `TimeOverflow`, and everything else keeps
/// the name `anim_core` gave it.
/// Fill in the object an interpolation refusal could not know.
///
/// [`sample_property`] and [`eased_fraction`] are about values, and a value does not know
/// which node or which property it was read from — that is why the errors they build carry
/// empty `node` and `property` fields. [`crate::animation::PropertySources::value`] does
/// know, so it calls this on the way out, and a report names the property whose keys
/// disagreed rather than a blank.
///
/// Only an empty field is filled. An error that already names something is passed through
/// unchanged, because the caller knows less about it than the raiser did.
#[must_use]
pub fn with_subject(
    error: crate::SceneError,
    node: &anim_core::id::NodeId,
    property: &str,
) -> crate::SceneError {
    use crate::SceneError;
    match error {
        SceneError::InvalidEasing {
            property: blank,
            position,
            detail,
        } => SceneError::InvalidEasing {
            property: if blank.is_empty() {
                property.to_string()
            } else {
                blank
            },
            position,
            detail,
        },
        SceneError::InvalidPropertyKeys {
            node: blank_node,
            property: blank_property,
            detail,
        } => SceneError::InvalidPropertyKeys {
            node: if blank_node.is_empty() {
                node.to_string()
            } else {
                blank_node
            },
            property: if blank_property.is_empty() {
                property.to_string()
            } else {
                blank_property
            },
            detail,
        },
        other => other,
    }
}

fn rational(
    operation: &'static str,
    value: &str,
    result: std::result::Result<Rational, CoreError>,
) -> Result<Rational> {
    result.map_err(|e| match e {
        CoreError::ArithmeticOverflow { .. } => SceneError::TimeOverflow {
            operation,
            value: format!("{value}: {e}"),
        },
        other => SceneError::Core(other),
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the workspace denial governs production code"
    )]

    use super::*;
    use anim_core::id::TrackId;

    /// Every expected value below is spelled as the rational the specification says it is,
    /// written by hand. `Rational::new(5, 2)` is not "the code's answer computed twice":
    /// it is the fraction a person gets with pen and paper, which is the oracle rule
    /// DECISIONS.md #32 records and the one that found WP02's five and WP03's four defects.
    fn r(num: i128, den: i128) -> Rational {
        Rational::new(num, den).unwrap()
    }

    fn frame(n: i64) -> FramePos {
        FramePos::at_frame(n)
    }

    fn at_frame(n: i64, sub: Rational) -> FramePos {
        FramePos::new(n, sub).unwrap()
    }

    fn curve(keys: impl IntoIterator<Item = PropertyKey>) -> PropertyCurve {
        PropertyCurve::new(keys, None).unwrap()
    }

    fn sampled(curve: &PropertyCurve, at: FramePos) -> Rational {
        match sample_property(curve, &at).unwrap().unwrap() {
            PropertyValue::Rational(value) => value,
            other => panic!("expected a rational, got {}", other.kind_name()),
        }
    }

    #[test]
    fn a_linear_interval_is_the_exact_fraction_between_two_keys() {
        let track = curve([
            PropertyKey::new(frame(0), PropertyValue::Rational(r(0, 1)), Easing::Linear),
            PropertyKey::new(frame(4), PropertyValue::Rational(r(10, 1)), Easing::Linear),
        ]);
        assert_eq!(sampled(&track, frame(2)), r(5, 1));
        assert_eq!(sampled(&track, frame(1)), r(5, 2));
        assert_eq!(sampled(&track, frame(3)), r(15, 2));
    }

    #[test]
    fn a_subframe_position_interpolates_without_a_float_entering_the_path() {
        // The case 13.12 names "subframe interpolation": a quarter of the way into the
        // first frame of a four-frame interval is a quarter of the way up the value, and
        // the answer is exact rather than a float that happens to look right.
        let track = curve([
            PropertyKey::new(frame(0), PropertyValue::Rational(r(0, 1)), Easing::Linear),
            PropertyKey::new(frame(4), PropertyValue::Rational(r(8, 1)), Easing::Linear),
        ]);
        assert_eq!(sampled(&track, at_frame(0, r(1, 4))), r(1, 2));
        assert_eq!(
            sampled(&track, at_frame(1, r(1, 2))),
            r(3, 1),
            "one and a half frames of a four-frame interval is three eighths of the rise,              and three eighths of eight is three"
        );
    }

    #[test]
    fn a_negative_frame_boundary_interpolates_the_same_way() {
        // 13.12's "negative frame boundaries" clause, and the reason the arithmetic is
        // floor-based: an interval straddling zero has the same fraction as one that does
        // not.
        let track = curve([
            PropertyKey::new(frame(-4), PropertyValue::Rational(r(0, 1)), Easing::Linear),
            PropertyKey::new(frame(4), PropertyValue::Rational(r(10, 1)), Easing::Linear),
        ]);
        assert_eq!(sampled(&track, frame(-2)), r(5, 2));
        assert_eq!(sampled(&track, frame(0)), r(5, 1));
        assert_eq!(
            sampled(&track, at_frame(-4, r(1, 2))),
            r(5, 8),
            "half a frame of an eight-frame interval is one sixteenth of the rise"
        );
    }

    #[test]
    fn outside_the_key_range_the_end_value_is_held() {
        let track = curve([
            PropertyKey::new(frame(2), PropertyValue::Rational(r(7, 1)), Easing::Linear),
            PropertyKey::new(frame(6), PropertyValue::Rational(r(9, 1)), Easing::Linear),
        ]);
        assert_eq!(
            sampled(&track, frame(-100)),
            r(7, 1),
            "before the first key"
        );
        assert_eq!(sampled(&track, frame(2_000)), r(9, 1), "after the last key");
        assert_eq!(sampled(&track, frame(2)), r(7, 1), "exactly on a key");
    }

    #[test]
    fn a_step_holds_the_preceding_value_until_its_own_key() {
        // 13.6: "Step properties select the preceding key." The easing lives on the key it
        // eases *toward*, which is the spelling property.rs's own doc comment states.
        let track = curve([
            PropertyKey::new(frame(0), PropertyValue::Rational(r(0, 1)), Easing::Linear),
            PropertyKey::new(frame(4), PropertyValue::Rational(r(10, 1)), Easing::Step),
        ]);
        assert_eq!(sampled(&track, frame(1)), r(0, 1));
        assert_eq!(sampled(&track, at_frame(3, r(99, 100))), r(0, 1));
        assert_eq!(
            sampled(&track, frame(4)),
            r(10, 1),
            "and it arrives at the key"
        );
    }

    #[test]
    fn an_empty_property_uses_its_authored_default_and_reports_having_none() {
        // 13.6's "Empty properties use authored defaults", and the second half of the
        // sentence the spec does not write: with no default either, `None` is the answer
        // this function gives and `animation` decides whether that is legal.
        let with_default = PropertyCurve::new([], Some(PropertyValue::Rational(r(3, 2)))).unwrap();
        assert_eq!(
            sample_property(&with_default, &frame(17)).unwrap(),
            Some(PropertyValue::Rational(r(3, 2)))
        );
        let without = PropertyCurve::with_default(None);
        assert_eq!(sample_property(&without, &frame(17)).unwrap(), None);
    }

    #[test]
    fn a_table_eases_the_interval_without_moving_either_endpoint() {
        // The endpoint rule in `EasingCurve::new` is what makes this true, so it is
        // tested where it is used rather than only where it is checked.
        let easing = Easing::Curve(EasingCurve::new([r(0, 1), r(1, 4), r(1, 1)]).unwrap());
        assert_eq!(eased_fraction(&r(0, 1), &easing).unwrap(), r(0, 1));
        assert_eq!(eased_fraction(&r(1, 1), &easing).unwrap(), r(1, 1));
        assert_eq!(
            eased_fraction(&r(1, 2), &easing).unwrap(),
            r(1, 4),
            "the middle sample is reached at the middle of the interval"
        );
        assert_eq!(
            eased_fraction(&r(1, 4), &easing).unwrap(),
            r(1, 8),
            "halfway from 0 to the first sample is half its height"
        );
        assert_eq!(
            eased_fraction(&r(3, 4), &easing).unwrap(),
            r(5, 8),
            "halfway from 1/4 to 1 is 5/8"
        );
    }

    #[test]
    fn an_overshooting_table_is_read_the_way_it_was_authored() {
        // Interior samples are deliberately unclamped (WP17's dolly zoom, WP20's
        // anticipation), so the lookup must not quietly limit them.
        let easing = Easing::Curve(EasingCurve::new([r(0, 1), r(5, 4), r(1, 1)]).unwrap());
        assert_eq!(eased_fraction(&r(1, 2), &easing).unwrap(), r(5, 4));
        assert_eq!(eased_fraction(&r(1, 4), &easing).unwrap(), r(5, 8));
    }

    #[test]
    fn a_switch_value_with_a_shaping_easing_is_an_easing_failure_not_a_number_failure() {
        let track = curve([
            PropertyKey::new(frame(0), PropertyValue::Bool(false), Easing::Linear),
            PropertyKey::new(frame(4), PropertyValue::Bool(true), Easing::Linear),
        ]);
        let Err(SceneError::InvalidEasing {
            position, detail, ..
        }) = sample_property(&track, &frame(2))
        else {
            panic!("there is no fraction of a bool");
        };
        assert_eq!(
            position, "4",
            "the key whose easing cannot be applied is named"
        );
        assert!(detail.contains("switches at a key"), "{detail}");
        // And the same pair authored as Step is legal, which is the advice the error gives.
        let legal = curve([
            PropertyKey::new(frame(0), PropertyValue::Bool(false), Easing::Linear),
            PropertyKey::new(frame(4), PropertyValue::Bool(true), Easing::Step),
        ]);
        assert_eq!(
            sample_property(&legal, &frame(2)).unwrap(),
            Some(PropertyValue::Bool(false))
        );
    }

    #[test]
    fn two_value_types_in_one_key_list_are_refused_naming_both() {
        let track = curve([
            PropertyKey::new(frame(0), PropertyValue::Rational(r(0, 1)), Easing::Linear),
            PropertyKey::new(frame(4), PropertyValue::Text("hold".into()), Easing::Linear),
        ]);
        let Err(SceneError::InvalidPropertyKeys { detail, .. }) =
            sample_property(&track, &frame(2))
        else {
            panic!("a rational does not become a label halfway along");
        };
        assert!(
            detail.contains("rational") && detail.contains("text"),
            "{detail}"
        );
    }

    #[test]
    fn reading_a_label_as_a_number_names_the_field_and_the_subject() {
        let Err(SceneError::InvalidNumericValue {
            field,
            subject,
            detail,
        }) = value_as_f64(
            &PropertyValue::Text("nope".into()),
            "opacity",
            &TrackId::derive(&uuid::Uuid::nil(), b"x").to_string(),
        )
        else {
            panic!("a label is not a number");
        };
        assert_eq!(field, "opacity");
        assert!(!subject.is_empty());
        assert!(detail.contains("text"), "{detail}");
    }

    #[test]
    fn the_final_conversion_is_the_only_rounding_in_a_subframe_ease() {
        // 1/3 of the way along a 3-unit rise is exactly 1, and a float used anywhere
        // before the last operation would have to be re-parsed to produce it. This asserts
        // the rational survives to the end *and* that the float at the end is right.
        let track = curve([
            PropertyKey::new(frame(0), PropertyValue::Rational(r(0, 1)), Easing::Linear),
            PropertyKey::new(
                at_frame(3, r(0, 1)),
                PropertyValue::Rational(r(3, 1)),
                Easing::Linear,
            ),
        ]);
        let at = at_frame(1, r(0, 1));
        let value = sample_property(&track, &at).unwrap().unwrap();
        assert_eq!(value, PropertyValue::Rational(r(1, 1)));
        assert_eq!(
            value_as_f64(&value, "transform.x", "node").unwrap(),
            1.0,
            "three frames of a three-unit interval at 24 fps is one frame, exactly"
        );
    }

    #[test]
    fn a_table_that_skipped_its_constructor_cannot_be_built_at_all() {
        // The negative of a check `ease_through_table` deliberately does not make. An
        // earlier version of this file re-verified the endpoints at evaluation time; this
        // asserts why that was wrong to write, because the only way to hold a curve whose
        // endpoints moved is `EasingCurve::new`, and it refuses. A guard for a state that
        // cannot be built is a branch no test can reach and no reader can justify.
        for bad in [
            vec![r(0, 1), r(1, 2), r(3, 2)],
            vec![r(1, 1), r(1, 2), r(1, 1)],
            vec![r(0, 1)],
        ] {
            assert!(
                EasingCurve::new(bad.clone()).is_err(),
                "{bad:?} must be refused by the constructor, not by the evaluator"
            );
        }
    }

    #[test]
    fn a_step_handed_to_the_easing_function_is_refused_rather_than_eased_to_zero() {
        // 13.6's Step rule is a *selection*, and a caller that reaches the easing function
        // with it has skipped the selection. Returning `u` would interpolate where the
        // spec says to hold; returning `0` would be the right answer for the wrong reason.
        let Err(SceneError::InvalidEasing { detail, .. }) = eased_fraction(&r(1, 2), &Easing::Step)
        else {
            panic!("a Step interval has no fraction to ease");
        };
        assert!(detail.contains("selection rule"), "{detail}");
    }
}
