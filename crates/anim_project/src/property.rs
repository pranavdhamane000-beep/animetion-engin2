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

/// How a value is reached from the preceding key.
///
/// Only the two modes spec 13.6 states a rule for. Bezier parameters and extrapolation
/// modes are named later — `InvalidEasing` in spec 13.11, pre/post extrapolation in
/// spec 15 — and each of those packages adds its variant with its own validation;
/// inventing the parameter fields here would author an easing model WP15 has to
/// replace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Easing {
    /// Interpolate linearly to the next key.
    Linear,
    /// Hold this key's value until the next key. Spec 13.6: "Step properties select the
    /// preceding key."
    Step,
}

impl Easing {
    /// The persisted and hashed spelling: the lowercase name JSON already writes, so
    /// that no table sits between the two (ADR-0005 Decision 5's rule).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Step => "step",
        }
    }

    pub(crate) fn encode(self, cp: &mut CanonicalBytes) {
        cp.text(self.as_str());
    }
}

impl std::fmt::Display for Easing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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
