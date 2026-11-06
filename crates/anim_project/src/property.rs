//! Rational-time property keys (spec 12.3).
//!
//! A property is one named value on one node that changes over time. The time is a
//! [`FramePos`] and never a float, because spec 10.10 says `anim_project` persists
//! rational keys and spec 7.1 forbids wall-clock or rounded values in deterministic
//! state; the value is one of four closed types for the same reason — a `f64` in a
//! persisted property would make `serde_json`'s shortest-round bit pattern an
//! authority the engine never agreed to.
//!
//! What this file owns is the *uniqueness* rule of spec 13.2: "Property keys have
//! unique canonical times within each property." That is a normalization (sorted, and
//! two keys at one time collapse to one), so per DECISIONS.md #22 the field holding it
//! is private, its `Deserialize` is hand-written, and a `project.anim.json` cannot
//! smuggle an unsorted or duplicated key list in from disk.

use anim_core::hash::CanonicalBytes;
use anim_core::rational::Rational;
use anim_core::time::FramePos;
use serde::{Deserialize, Serialize};

use crate::{ProjectError, Result};

/// A named property on a node, as one dotted path.
///
/// A newtype over a validated string rather than an enum of known properties, because
/// the property set is not closed at this stage: spec 13.6 evaluates "a property",
/// WP12 adds bone controls, WP14 adds VFX intensity curves, and each of them would
/// otherwise have to edit this crate to add a variant. The dotted shape is what makes
/// the key stable in a `BTreeMap` and in a canonical stream.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PropertyPath(String);

impl PropertyPath {
    /// Parse and validate a path.
    ///
    /// The rules each answer an ordering or encoding requirement rather than taste:
    /// non-empty; segments of `[A-Za-z0-9_-]+` separated by `.`; no leading, trailing
    /// or repeated separator. A path that could be spelled two ways would hash two
    /// ways, which is the ADR-0004 failure mode this engine refuses everywhere else.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if `raw` is not a canonical path.
    pub fn new(raw: &str) -> Result<Self> {
        if raw.is_empty() {
            return Err(invalid_path(raw, "it is empty"));
        }
        for segment in raw.split('.') {
            if segment.is_empty() {
                return Err(invalid_path(raw, "an empty segment"));
            }
            if !segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(invalid_path(raw, "a character outside [A-Za-z0-9_-]"));
            }
        }
        Ok(Self(raw.to_string()))
    }

    /// The path as written, which is canonical by construction.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The dot-separated segments, outermost first.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.text(&self.0);
    }
}

impl std::fmt::Display for PropertyPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// Hand-written in both directions: `new` is the only way to hold a path, so
// `Serialize` can only ever be writing a valid one, and `Deserialize` must re-pass the
// same gate or a hand-edited file gets a second spelling of one property.
impl Serialize for PropertyPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PropertyPath {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

fn invalid_path(raw: &str, why: &str) -> ProjectError {
    ProjectError::Validation {
        location: format!("property path {raw:?}"),
        detail: format!("must be dot-separated segments of [A-Za-z0-9_-]+; rejected because {why}"),
    }
}

/// The version of the numeric algorithm that turns an [`EasingCurve`] table plus one
/// exact interval fraction into one eased fraction.
///
/// Spec 13.7 requires "a versioned numeric algorithm" and WP15's own track sampler
/// repeats the phrase (spec 30.7), so the version has to be a number a snapshot can
/// carry rather than a fact about whichever build produced it. It moves when the
/// *arithmetic* moves, not when a field is added — the same split ADR-0005 Decision 4
/// keeps between an encoding version and a data version.
///
/// It is recorded here and hashed into a snapshot's pre-image by
/// `anim_scene::snapshot::snapshot_hash`, and deliberately not into the document's:
/// authored data does not change when the interpretation of it changes, but two
/// snapshots that differ must not share a hash. WP20 states the same rule for a
/// summation technique (spec 41.7's "changing summation technique is an algorithm
/// version change"), which is the precedent this constant follows.
pub const EASING_ALGORITHM_VERSION: u32 = 1;

/// The largest table one [`EasingCurve`] may hold.
///
/// A bound rather than an open `Vec` for one arithmetic reason: the lookup multiplies a
/// fraction whose denominator is already `points.len() - 1` into an interval fraction
/// whose denominator comes from two rational frame positions, and spec 10.7's exact
/// arithmetic has to stay inside `i128`. Sixty-five samples is the ceiling that keeps
/// that product bounded while still being finer than any easing a person authors by
/// hand; WP15's rail tables are a separate 256-sample mechanism with its own bound
/// (spec 30.7) and do not read this one.
pub const EASING_CURVE_MAX_POINTS: usize = 65;

/// How a value is reached from the preceding key.
///
/// `Linear` and `Step` are the two modes spec 13.6 states a rule for. `Curve` is the
/// parameterized easing spec 13.7 needs in order to say anything at all: 13.11 lists
/// `InvalidEasing` and 13.7 makes "invalid easing parameters" a validation error, and
/// neither is a reachable condition while the type has no parameters. The survey that
/// preceded this found no section anywhere enumerating easing variants — WP15 names
/// `Curve<Rational>` (30.8), WP17 names `SpeedRampCurve` (32.8) and "a cubic or
/// table-based ramp" (32.7), and WP19 names `EaseKind` (41.8), all without defining
/// any of them — so this is the first easing model in the project and those packages
/// extend it rather than replace it. What they extend it *with* is what is absent from
/// the spec: no package asks for per-key Bézier handles, so a table of normalized
/// outputs stays the shape that satisfies 32.7's "cubic or table-based" alternative
/// without inventing a handle-authoring model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Easing {
    /// Interpolate linearly to the next key.
    Linear,
    /// Hold this key's value until the next key. Spec 13.6: "Step properties select the
    /// preceding key."
    Step,
    /// Reshape the interval fraction through an authored table.
    Curve(EasingCurve),
}

impl Easing {
    /// The canonical tag: the lowercase name JSON already writes for the unit variants,
    /// so that no table sits between the two (ADR-0005 Decision 5's rule). `Curve`
    /// carries its parameters after this tag, exactly as `PropertyValue` does.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Step => "step",
            Self::Curve(_) => "curve",
        }
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.text(self.as_str());
        if let Self::Curve(curve) = self {
            curve.encode(cp);
        }
    }
}

impl std::fmt::Display for Easing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Curve(curve) => write!(f, "curve({})", curve.points.len()),
            other => f.write_str(other.as_str()),
        }
    }
}

/// An authored easing table: normalized output values at equally spaced input positions.
///
/// `points[i]` is the eased fraction at input `i / (len - 1)`, so the samples are
/// exactly spaced and the lookup is exact rational arithmetic. Spec 13.7 asks for "a
/// versioned numeric algorithm" rather than for one particular curve family, and a
/// table is the one form the spec sanctions twice — 32.7's "a cubic or table-based
/// ramp" and 32.5's "fixed easing tables" — while needing no transcendental step whose
/// reproducibility would rest on whichever libm the toolchain happens to call.
///
/// The first and last values are pinned to `0/1` and `1/1`. That is what makes an
/// authored curve safe rather than merely expressible: it guarantees the eased fraction
/// is `u = 0` at the preceding key and `u = 1` at the following one, so a curve can
/// reshape the motion *between* two keys without ever displacing either of them. Without
/// that rule the spec's own interpolation text — "At exact key time return that key" —
/// would be contradicted by authored data, and one property would have two values at one
/// instant depending on how the lookup rounds.
///
/// `points` is private and its `Deserialize` is hand-written for the ordinary reason
/// (DECISIONS.md #22, applied to spec 10.8's normalization rule): the invariant needs
/// validation, and a derived impl would fill the field without asking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EasingCurve {
    points: Vec<Rational>,
}

impl EasingCurve {
    /// Build a curve from its sample values, validating them.
    ///
    /// The rules are the ones a curve has to satisfy to be a function of the interval at
    /// all, and each refuses a distinct authored mistake:
    ///
    /// - at least two points, because one value is a constant, not a mapping;
    /// - at most [`EASING_CURVE_MAX_POINTS`], for the `i128` bound stated there;
    /// - the first sample exactly `0/1` and the last exactly `1/1`, so the curve cannot
    ///   move an authored key value.
    ///
    /// Interior samples are *not* clamped to `[0, 1]`. Overshoot is the point of several
    /// of the easings later packages ask for — WP17's "dolly zoom" and WP20's
    /// anticipation/recovery motion both need a fraction that leaves the range and comes
    /// back — and a rule against it would forbid authored behaviour the spec wants.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] naming which rule broke. This is the condition spec
    /// 13.11 calls `InvalidEasing` and spec 13.7 calls "invalid easing parameters"; at
    /// this layer it arrives as a project validation error, because validation errors
    /// belong to the type that owns the field (spec 12.11), and `anim_scene` re-raises
    /// it as its own typed variant for the context-dependent case that the type cannot
    /// see. See [`crate::scene`]`'s` module comment.
    pub fn new(points: impl IntoIterator<Item = Rational>) -> Result<Self> {
        let values: Vec<Rational> = points.into_iter().collect();
        if values.len() < 2 {
            return Err(Self::rejected(
                &values,
                "an easing table needs at least two samples; one value maps nothing",
            ));
        }
        if values.len() > EASING_CURVE_MAX_POINTS {
            return Err(Self::rejected(
                &values,
                "an easing table holds at most EASING_CURVE_MAX_POINTS samples, so that the \
                 exact fraction arithmetic that reads it cannot overflow its width",
            ));
        }
        if values[0] != Rational::from_i128(0) {
            return Err(Self::rejected(
                &values,
                "the first sample must be 0/1, or the curve would move the value of the key \
                 it eases away from",
            ));
        }
        if values[values.len() - 1] != Rational::from_i128(1) {
            return Err(Self::rejected(
                &values,
                "the last sample must be 1/1, or the curve would not arrive at the value of \
                 the key it eases toward",
            ));
        }
        Ok(Self { points: values })
    }

    /// The samples, in input order.
    #[must_use]
    pub fn points(&self) -> &[Rational] {
        &self.points
    }

    /// The samples as a fixed spacing: sample `i` sits at input `i / (len - 1)`.
    ///
    /// Exposed because the spacing rule is what makes the table interpretable, and a
    /// caller that had to re-derive `len - 1` would be re-deriving a validation rule
    /// rather than reading data.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if `index` is not a sample of this curve, and
    /// [`ProjectError::Core`] if the ratio cannot be represented — neither is reachable
    /// for a curve built through [`EasingCurve::new`], whose length bound is what makes
    /// the arithmetic safe.
    pub fn position_of(&self, index: usize) -> Result<Rational> {
        if index >= self.points.len() {
            return Err(ProjectError::Validation {
                location: format!("easing sample {index}"),
                detail: format!(
                    "is outside this curve, which has {} samples numbered from 0",
                    self.points.len()
                ),
            });
        }
        let last = i128::try_from(self.points.len() - 1).map_err(|e| ProjectError::Validation {
            location: "easing curve length".to_string(),
            detail: format!("does not fit the width its arithmetic uses: {e}"),
        })?;
        let i = i128::try_from(index).map_err(|e| ProjectError::Validation {
            location: format!("easing sample {index}"),
            detail: format!("does not fit the width its arithmetic uses: {e}"),
        })?;
        Ok(Rational::new(i, last)?)
    }

    fn rejected(values: &[Rational], why: &str) -> ProjectError {
        ProjectError::Validation {
            location: format!(
                "easing curve with {} samples [{}]",
                values.len(),
                values
                    .iter()
                    .map(Rational::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            detail: format!("invalid easing parameters: {why}"),
        }
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.u64(self.points.len() as u64);
        for point in &self.points {
            cp.rational(point);
        }
    }
}

impl Serialize for EasingCurve {
    /// Derived-in-shape but hand-written, so the member name is spelled once and
    /// [`EasingCurve`]’s JSON is the same object whether it arrives inside an `Easing`
    /// or on its own.
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("EasingCurve", 1)?;
        state.serialize_field("points", &self.points)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for EasingCurve {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawCurve {
            points: Vec<Rational>,
        }
        let raw = RawCurve::deserialize(deserializer)?;
        Self::new(raw.points).map_err(serde::de::Error::custom)
    }
}

/// A property value, one of four closed types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyValue {
    /// An exact rational — a scalar, a normalized coordinate, an angle.
    Rational(Rational),
    /// A switch: `visible`, `locked`, a cut flag.
    Bool(bool),
    /// Authored text: a pose name, a label.
    Text(String),
    /// An asset reference, which spec 11.8 keeps as an id and never as a path.
    Asset(anim_core::id::AssetId),
}

impl PropertyValue {
    /// The type name, used in diagnostics and as the canonical tag.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Rational(_) => "rational",
            Self::Bool(_) => "bool",
            Self::Text(_) => "text",
            Self::Asset(_) => "asset",
        }
    }

    /// This value as a rational, when it is one.
    #[must_use]
    pub const fn as_rational(&self) -> Option<Rational> {
        match self {
            Self::Rational(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        // The tag is text rather than a discriminant for ADR-0004's reason: reordering
        // these variants would redefine every persisted value without a byte of this
        // layout looking wrong in a diff.
        cp.text(self.kind_name());
        match self {
            Self::Rational(value) => cp.rational(value),
            Self::Bool(value) => cp.bool(*value),
            Self::Text(value) => cp.text(value),
            Self::Asset(value) => cp.fixed_bytes(value.as_bytes()),
        }
    }
}

/// One authored value at one exact time.
///
/// Derives both directions: it holds no invariant of its own. `FramePos`'s private
/// subframe is what enforces the range rule, and it already has a hand-written
/// `Deserialize` that re-passes that gate (DECISIONS.md #22), so reading a key from
/// JSON cannot produce a position `new` would have rejected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyKey {
    /// When.
    pub position: FramePos,
    /// What.
    pub value: PropertyValue,
    /// How it is reached from the preceding key.
    pub easing: Easing,
}

impl PropertyKey {
    /// A new key.
    #[must_use]
    pub fn new(position: FramePos, value: PropertyValue, easing: Easing) -> Self {
        Self {
            position,
            value,
            easing,
        }
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.i64(self.position.frame);
        cp.rational(&self.position.subframe());
        self.value.encode(cp);
        self.easing.encode(cp);
    }

    /// The key's canonical field stream, with no domain tag, so an envelope that carries
    /// a key hashes these bytes rather than a second rendering of them.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp);
        cp.into_vec()
    }
}

/// The keys of one property, plus the value used when there are none.
///
/// Invariant, from spec 13.2 and 13.6: keys are sorted by canonical time and no two
/// share one. Spec 13.11 makes duplicate times a validation *error* rather than
/// something evaluation resolves, which is why `keys` is private and the only ways to
/// change one are [`PropertyCurve::new`], [`PropertyCurve::set`] and
/// [`PropertyCurve::remove`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyCurve {
    keys: Vec<PropertyKey>,
    /// Spec 13.6: "Empty properties use authored defaults." No normalization applies,
    /// so this is a plain field.
    pub authored_default: Option<PropertyValue>,
}

impl PropertyCurve {
    /// Build a curve: sort by canonical time, then refuse a repeated time.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if two keys land on one canonical time.
    pub fn new(
        keys: impl IntoIterator<Item = PropertyKey>,
        authored_default: Option<PropertyValue>,
    ) -> Result<Self> {
        let mut sorted: Vec<PropertyKey> = keys.into_iter().collect();
        sorted.sort_by_key(|key| key.position);
        if let Some(pair) = sorted.windows(2).find(|w| w[0].position == w[1].position) {
            return Err(ProjectError::Validation {
                location: format!("property key at frame {}", pair[0].position.frame),
                detail: format!(
                    "two keys land on the same canonical time (frame {}, subframe {}), which spec 13.2 forbids",
                    pair[0].position.frame, pair[0].position.subframe()
                ),
            });
        }
        Ok(Self {
            keys: sorted,
            authored_default,
        })
    }

    /// An empty curve with one authored default.
    #[must_use]
    pub fn with_default(authored_default: Option<PropertyValue>) -> Self {
        Self {
            keys: Vec::new(),
            authored_default,
        }
    }

    /// The keys, in canonical time order.
    #[must_use]
    pub fn keys(&self) -> &[PropertyKey] {
        &self.keys
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// The key at `position`, if any.
    #[must_use]
    pub fn get(&self, position: FramePos) -> Option<&PropertyKey> {
        self.index_of(position).map(|i| &self.keys[i])
    }

    /// Insert or replace one key, returning what it displaced.
    ///
    /// The returned value is what makes an exact inverse possible: spec 12.6 wants a
    /// command to "return an inverse", and the inverse of a replaced key has to carry
    /// the key that was there. Same shape as `AssetManifest::insert`, which spec 11.6's
    /// reimport depends on for the same reason. Infallible, because inserting into a
    /// sorted position cannot create a duplicate.
    #[must_use = "the displaced key is what a command's inverse carries"]
    pub fn set(&mut self, key: PropertyKey) -> Option<PropertyKey> {
        match self.index_of(key.position) {
            Some(index) => Some(std::mem::replace(&mut self.keys[index], key)),
            None => {
                let insert_at = self
                    .keys
                    .partition_point(|existing| existing.position < key.position);
                self.keys.insert(insert_at, key);
                None
            }
        }
    }

    /// Remove the key at `position`, returning it.
    #[must_use = "the removed key is what a command's inverse carries"]
    pub fn remove(&mut self, position: FramePos) -> Option<PropertyKey> {
        let index = self.index_of(position)?;
        Some(self.keys.remove(index))
    }

    fn index_of(&self, position: FramePos) -> Option<usize> {
        self.keys
            .iter()
            .position(|existing| existing.position == position)
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.u64(self.keys.len() as u64);
        for key in &self.keys {
            key.encode(cp);
        }
        // An absent default is a distinguishable state rather than a zero: writing
        // `false` for "no default" would collide with a default of `false`.
        cp.bool(self.authored_default.is_some());
        if let Some(default) = &self.authored_default {
            default.encode(cp);
        }
    }
}

impl Serialize for PropertyCurve {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("PropertyCurve", 2)?;
        state.serialize_field("keys", &self.keys)?;
        state.serialize_field("authored_default", &self.authored_default)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for PropertyCurve {
    /// Hand-written so that the sorted-and-unique gate is the gate a saved file passes
    /// too, for the reason DECISIONS.md #22 gives and spec 12.12 tests: a derived impl
    /// would fill the private `keys` directly, and a hand-edited `project.anim.json`
    /// could then hold two keys at one time and be loaded as one curve.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawCurve {
            keys: Vec<PropertyKey>,
            authored_default: Option<PropertyValue>,
        }
        let raw = RawCurve::deserialize(deserializer)?;
        Self::new(raw.keys, raw.authored_default).map_err(serde::de::Error::custom)
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

    fn frame(n: i64) -> FramePos {
        FramePos::at_frame(n)
    }

    fn key(n: i64, value: i128) -> PropertyKey {
        PropertyKey::new(
            frame(n),
            PropertyValue::Rational(Rational::from_i128(value)),
            Easing::Linear,
        )
    }

    /// A rational, spelled `num/den`, for the curve tests.
    fn r(num: i128, den: i128) -> Rational {
        Rational::new(num, den).unwrap()
    }

    #[test]
    fn path_rules_accept_canonical_names_and_reject_two_spellings_of_one_key() {
        assert_eq!(
            PropertyPath::new("transform.x").unwrap().as_str(),
            "transform.x"
        );
        for rejected in ["", "transform.", ".x", "a..b", "transform x", "a.b(c)"] {
            assert!(
                PropertyPath::new(rejected).is_err(),
                "{rejected:?} must not be a property path"
            );
        }
    }

    #[test]
    fn path_survives_json_only_through_the_validating_constructor() {
        let text = serde_json::to_string(&PropertyPath::new("pose.name").unwrap()).unwrap();
        assert_eq!(text, "\"pose.name\"");
        assert!(serde_json::from_str::<PropertyPath>("\"bad path\"").is_err());
    }

    #[test]
    fn keys_sort_and_a_repeated_time_is_refused_not_collapsed() {
        let curve = PropertyCurve::new([key(9, 1), key(2, 0), key(5, 3)], None).unwrap();
        assert_eq!(
            curve
                .keys()
                .iter()
                .map(|k| k.position.frame)
                .collect::<Vec<_>>(),
            vec![2, 5, 9]
        );
        assert!(matches!(
            PropertyCurve::new([key(4, 0), key(4, 7)], None),
            Err(ProjectError::Validation { .. })
        ));
    }

    #[test]
    fn a_repeated_time_written_by_hand_is_refused_on_load() {
        // The same rule as above, reached through the file path rather than the
        // constructor, which is the half DECISIONS.md #22 says a derived impl would
        // leave unguarded.
        let json = r#"{"keys":[
            {"position":{"frame":4,"subframe":"0/1"},"value":{"rational":"1/1"},"easing":"linear"},
            {"position":{"frame":4,"subframe":"0/1"},"value":{"rational":"2/1"},"easing":"linear"}
        ],"authored_default":null}"#;
        assert!(serde_json::from_str::<PropertyCurve>(json).is_err());
    }

    #[test]
    fn set_keeps_order_and_returns_what_it_displaced() {
        let mut curve = PropertyCurve::with_default(None);
        assert_eq!(curve.set(key(5, 0)), None, "a new key displaces nothing");
        assert_eq!(curve.set(key(2, 0)), None);
        assert_eq!(curve.set(key(9, 0)), None);
        assert_eq!(
            curve
                .keys()
                .iter()
                .map(|k| k.position.frame)
                .collect::<Vec<_>>(),
            vec![2, 5, 9],
            "insertion at any position keeps the canonical order"
        );
        assert_eq!(curve.set(key(5, 7)), Some(key(5, 0)));
        assert_eq!(curve.len(), 3);
        assert_eq!(curve.get(frame(5)), Some(&key(5, 7)));
        assert_eq!(curve.remove(frame(5)), Some(key(5, 7)));
        assert_eq!(curve.remove(frame(5)), None);
    }

    #[test]
    fn easing_and_value_spellings_agree_between_json_and_bytes() {
        assert_eq!(Easing::Step.as_str(), "step");
        assert_eq!(serde_json::to_string(&Easing::Step).unwrap(), "\"step\"");
        assert_eq!(
            PropertyValue::Rational(Rational::new(1001, 24).unwrap()).kind_name(),
            "rational"
        );
        let mut cp = CanonicalBytes::new();
        PropertyValue::Rational(Rational::new(1001, 24).unwrap()).encode(&mut cp);
        let bytes = cp.into_vec();
        assert_eq!(
            &bytes[..8],
            &[8, 0, 0, 0, 0, 0, 0, 0],
            "the tag is a little-endian u64 length prefix, per ADR-0004's layout"
        );
        assert_eq!(
            &bytes[8..16],
            b"rational",
            "and the tag text is the same lowercase spelling JSON writes, with no table \
             between the two"
        );
    }

    #[test]
    fn an_absent_default_is_not_the_same_bytes_as_a_false_default() {
        let mut cp_none = CanonicalBytes::new();
        PropertyCurve::with_default(None).encode(&mut cp_none);
        let mut cp_false = CanonicalBytes::new();
        PropertyCurve::with_default(Some(PropertyValue::Bool(false))).encode(&mut cp_false);
        assert_ne!(cp_none.as_slice(), cp_false.as_slice());
    }

    #[test]
    fn an_easing_curve_that_does_not_start_at_zero_or_end_at_one_is_refused() {
        // These two rules are what keep a curve from displacing an authored key value,
        // so they are what spec 13.7's "invalid easing parameters" and spec 13.11's
        // `InvalidEasing` are about. Each row breaks exactly one rule.
        for bad in [
            vec![],
            vec![r(0, 1)],
            vec![r(0, 1), r(0, 1)],
            vec![r(1, 1), r(1, 1)],
            vec![r(0, 1), r(2, 1)],
            vec![r(0, 1), r(5, 4), r(3, 2)],
        ] {
            let Err(ProjectError::Validation { detail, .. }) = EasingCurve::new(bad.clone()) else {
                panic!("{bad:?} must be refused as invalid easing parameters, not accepted");
            };
            assert!(detail.contains("invalid easing parameters"), "{detail}");
        }
        // And the legal shapes are accepted, including an overshoot, which is what
        // WP17's dolly zoom and WP20's anticipation need.
        assert!(EasingCurve::new([r(0, 1), r(1, 1)]).is_ok());
        assert!(EasingCurve::new([r(0, 1), r(5, 4), r(1, 1)]).is_ok());
    }

    #[test]
    fn an_easing_curve_over_the_sample_ceiling_is_refused_by_the_bound_not_by_luck() {
        let mut too_many: Vec<Rational> = (0..EASING_CURVE_MAX_POINTS)
            .map(|i| Rational::new(i128::try_from(i).unwrap(), 100).unwrap())
            .collect();
        too_many.push(Rational::from_i128(1));
        assert_eq!(too_many.len(), EASING_CURVE_MAX_POINTS + 1);
        assert!(matches!(
            EasingCurve::new(too_many),
            Err(ProjectError::Validation { .. })
        ));
    }

    #[test]
    fn curve_samples_are_equally_spaced_over_the_unit_interval() {
        // The lookup in `anim_scene::properties` is only exact because of this rule, and
        // the rule lives with the data rather than with the reader.
        let curve = EasingCurve::new([r(0, 1), r(1, 2), r(1, 1)]).unwrap();
        assert_eq!(curve.position_of(0).unwrap(), Rational::from_i128(0));
        assert_eq!(curve.position_of(1).unwrap(), Rational::new(1, 2).unwrap());
        assert_eq!(curve.position_of(2).unwrap(), Rational::from_i128(1));
        assert!(
            curve.position_of(3).is_err(),
            "one past the table is not a sample"
        );
    }

    #[test]
    fn a_curve_easing_is_one_object_in_json_and_carries_its_table_in_bytes() {
        let easing = Easing::Curve(EasingCurve::new([r(0, 1), r(1, 2), r(1, 1)]).unwrap());
        let text = serde_json::to_string(&easing).unwrap();
        assert_eq!(
            text, r#"{"curve":{"points":["0/1","1/2","1/1"]}}"#,
            "the struct variant spells itself as tag plus payload, the same shape \
             PropertyValue uses"
        );
        let loaded: Easing = serde_json::from_str(&text).unwrap();
        assert_eq!(loaded, easing);
        assert!(
            serde_json::from_str::<Easing>(r#"{"curve":{"points":["0/1","0/1"]}}"#).is_err(),
            "a table that does not arrive at 1/1 must not load from a file either"
        );

        let mut with_table = CanonicalBytes::new();
        easing.encode(&mut with_table);
        let mut linear = CanonicalBytes::new();
        Easing::Linear.encode(&mut linear);
        assert_eq!(
            &with_table.as_slice()[..13],
            &[5, 0, 0, 0, 0, 0, 0, 0, b'c', b'u', b'r', b'v', b'e'],
            "the tag is the lowercase name JSON writes, length-prefixed little-endian, \
             and the table follows it"
        );
        assert_ne!(
            with_table.as_slice(),
            linear.as_slice(),
            "a curve is not the same bytes as its tag alone"
        );
    }

    #[test]
    fn curve_bytes_are_independent_of_the_order_keys_arrived_in() {
        let a = PropertyCurve::new([key(1, 0), key(2, 0), key(3, 0)], None).unwrap();
        let b = PropertyCurve::new([key(3, 0), key(1, 0), key(2, 0)], None).unwrap();
        let mut ca = CanonicalBytes::new();
        let mut cb = CanonicalBytes::new();
        a.encode(&mut ca);
        b.encode(&mut cb);
        assert_eq!(ca.as_slice(), cb.as_slice());
    }

    #[test]
    fn a_curve_round_trips_through_json_unchanged() {
        let curve = PropertyCurve::new(
            [
                key(1, 3),
                PropertyKey::new(frame(2), PropertyValue::Text("hold".into()), Easing::Step),
            ],
            Some(PropertyValue::Bool(true)),
        )
        .unwrap();
        let bytes = serde_json::to_vec(&curve).unwrap();
        let loaded: PropertyCurve = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded, curve);
        assert_eq!(
            serde_json::to_vec(&loaded).unwrap(),
            bytes,
            "re-serializing a loaded curve reproduces the same bytes"
        );
    }
}
