//! Canonical byte encoding with domain separation (spec 10.3, 10.5, 10.9).
//!
//! One encoder, so that "the hash of this state" means the same thing in WP04's
//! snapshots, WP11's benchmark identity and WP09's export verification. The rules
//! that make that true:
//!
//! * **Little-endian, fixed width** for every integer, and for the bit pattern of a
//!   finite `f64` (spec 14.7 prescribes little-endian bit patterns after finite
//!   validation; the width is this package's choice, recorded in ADR-0004).
//! * **Length-prefixed** `u64` for every variable-length item: bytes, text, and the
//!   domain tag itself.
//! * **Sorted before written.** The encoder does not order anything. Spec 7.1 makes
//!   the caller sort "by their specified stable keys", which for IDs is the byte
//!   order published in [`crate::id`].
//! * **A domain tag is always present and always first**, so a snapshot hash and a
//!   manifest hash over identical field layouts cannot collide.
//!
//! Those rules make the encoding prefix-free, which is what lets two different
//! structures be told apart by their bytes rather than by luck: `bytes(b"ab")` is
//! not the same stream as `bytes(b"a")` followed by `bytes(b"b")`.

use blake3::Hasher;
use uuid::Uuid;

use crate::error::CoreError;
use crate::rational::Rational;

/// A byte stream built under the rules above.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CanonicalBytes(Vec<u8>);

impl CanonicalBytes {
    /// An empty stream with no domain. Use [`CanonicalBytes::with_domain`] for
    /// anything that will be hashed; this exists for callers that need the raw
    /// layout rules without a tag.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Start a stream tagged with `domain`, which must be non-empty ASCII and is
    /// conventional rather than registered: existing precedents are
    /// `anim_engine/philox/v1` (spec 14.7) and the `anim_engine/<area>/v1` shape
    /// ADR-0004 asks later packages to follow.
    ///
    /// The tag is length-prefixed, so no domain is a prefix of another and a domain
    /// cannot be confused with a payload field.
    pub fn with_domain(domain: &str) -> Result<Self, CoreError> {
        // The same shape `DiagnosticCode` requires: non-empty, no whitespace, and
        // ASCII, so a domain is stable across locales and greppable in a report.
        if domain.is_empty()
            || !domain.is_ascii()
            || domain != domain.trim()
            || domain.contains(char::is_whitespace)
        {
            return Err(CoreError::MalformedCanonical {
                raw: domain.to_owned(),
            });
        }
        let mut out = Self::new();
        out.text(domain);
        Ok(out)
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// The encoded stream, moved out. Callers that publish bytes rather than a
    /// digest — WP02's length-prefixed manifest records — use this.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    /// Number of encoded bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    pub fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn i32(&mut self, value: i32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn i64(&mut self, value: i64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn i128(&mut self, value: i128) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    /// One byte, never a `bool`-sized surprise: `false` is `0x00` and `true` is
    /// `0x01`, so a stream cannot be padded into a different meaning.
    pub fn bool(&mut self, value: bool) {
        self.0.push(u8::from(value));
    }

    /// A finite float as its raw IEEE-754 bits, little-endian.
    ///
    /// NaN is refused rather than encoded. Two NaNs with the same value can carry
    /// different payloads, so a hashed NaN is not reproducible, and spec 7.1
    /// requires canonical-state floats to be finite. The error carries the bit
    /// pattern, because "which NaN" is the question a caller then has.
    pub fn f64(&mut self, value: f64) -> Result<(), CoreError> {
        if !value.is_finite() {
            return Err(CoreError::NonFiniteValue {
                bits: value.to_bits(),
            });
        }
        self.0.extend_from_slice(&value.to_bits().to_le_bytes());
        Ok(())
    }

    /// Length-prefixed bytes.
    pub fn bytes(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.0.extend_from_slice(value);
    }

    /// Length-prefixed UTF-8. Text is not distinguished from bytes on the wire, so
    /// a caller that hashes both must not let one field be either.
    pub fn text(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    /// The canonical `"num/den"` spelling, length-prefixed.
    ///
    /// Rationals go through their text form rather than as two `i128`s because the
    /// type's whole guarantee is one unique normal form, and `Display` is the
    /// published spelling of that form. Two encodings of one value — a wire text
    /// and a hashing layout — would be two chances to disagree.
    pub fn rational(&mut self, value: &Rational) {
        self.text(&value.to_string());
    }

    /// Sixteen raw bytes, not length-prefixed: the width is fixed by the type, so a
    /// prefix would only let a malformed length disagree with it.
    pub fn uuid(&mut self, value: &Uuid) {
        self.0.extend_from_slice(value.as_bytes());
    }

    /// BLAKE3 over the stream.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = Hasher::new();
        hasher.update(&self.0);
        *hasher.finalize().as_bytes()
    }

    /// The digest as lowercase hex, which is the form a report prints and the form
    /// an asset object path is derived from (spec 11.7).
    #[must_use]
    pub fn hex_digest(&self) -> String {
        hex::encode(self.digest())
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

    fn rate(num: i128, den: i128) -> Rational {
        Rational::new(num, den).unwrap()
    }

    #[test]
    fn layout_is_exactly_what_the_rules_say() {
        // Built by hand from the documented rules, not from the implementation, so
        // this catches an encoder that drifted away from its own contract.
        let mut cp = CanonicalBytes::with_domain("anim_engine/test/v1").unwrap();
        cp.u32(7);
        cp.text("ab");
        cp.rational(&rate(1, 2));
        cp.uuid(&Uuid::nil());

        let mut expected: Vec<u8> = Vec::new();
        // "anim_engine" 11 + "/" 1 + "test" 4 + "/" 1 + "v1" 2 = 19 bytes of domain.
        expected.extend_from_slice(&19u64.to_le_bytes());
        expected.extend_from_slice(b"anim_engine/test/v1");
        expected.extend_from_slice(&7u32.to_le_bytes());
        expected.extend_from_slice(&2u64.to_le_bytes()); // text length
        expected.extend_from_slice(b"ab");
        expected.extend_from_slice(&3u64.to_le_bytes()); // "1/2" length
        expected.extend_from_slice(b"1/2");
        expected.extend_from_slice(&[0u8; 16]); // uuid, unprefixed
        assert_eq!(cp.as_slice(), expected.as_slice());
        assert_eq!(cp.len(), expected.len());
        // And the digest is a function of those documented bytes only.
        assert_eq!(
            cp.hex_digest(),
            hex::encode(blake3::hash(&expected).as_bytes())
        );
    }

    #[test]
    fn domains_separate_streams_that_otherwise_agree() {
        let mut a = CanonicalBytes::with_domain("anim_engine/snapshot/v1").unwrap();
        let mut b = CanonicalBytes::with_domain("anim_engine/manifest/v1").unwrap();
        for cp in [&mut a, &mut b] {
            cp.u64(1);
            cp.text("same");
        }
        assert_eq!(
            a.as_slice()[32..],
            b.as_slice()[32..],
            "payloads must be identical"
        );
        assert_ne!(a.digest(), b.digest(), "domains must separate them");
    }

    #[test]
    fn length_prefixes_make_the_encoding_prefix_free() {
        let mut one = CanonicalBytes::new();
        one.bytes(b"ab");
        let mut two = CanonicalBytes::new();
        two.bytes(b"a");
        two.bytes(b"b");
        assert_ne!(one.digest(), two.digest());

        // Same for an empty field versus no field at all.
        let mut empty = CanonicalBytes::new();
        empty.text("");
        let none = CanonicalBytes::new();
        assert_ne!(empty.digest(), none.digest());
    }

    #[test]
    fn widths_are_part_of_the_identity() {
        let mut a = CanonicalBytes::new();
        a.u8(1);
        let mut b = CanonicalBytes::new();
        b.u32(1);
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn non_finite_floats_are_refused_and_signed_zero_is_not() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut cp = CanonicalBytes::new();
            let e = cp.f64(bad).unwrap_err();
            assert!(matches!(e, CoreError::NonFiniteValue { .. }), "{e}");
            assert_eq!(e.to_diagnostic().code.as_str(), "core.non_finite_value");
        }
        // Negative zero is finite, so it encodes — and it must encode *differently*
        // from zero, because a bit-pattern rule that lost the sign would make two
        // states hash alike.
        let mut neg = CanonicalBytes::new();
        neg.f64(-0.0).unwrap();
        let mut pos = CanonicalBytes::new();
        pos.f64(0.0).unwrap();
        assert_ne!(neg.digest(), pos.digest());
        assert_ne!(neg.as_slice(), pos.as_slice());
    }

    #[test]
    fn equivalent_values_encode_alike_and_distinct_values_do_not() {
        // Rational cannot hold 2/4, so an encoder fed "equal but differently
        // written" cannot exist; this asserts the encoder agrees with the type.
        let mut a = CanonicalBytes::new();
        a.rational(&rate(24_000, 1001));
        let mut b = CanonicalBytes::new();
        b.rational(&Rational::new(48_000, 2002).unwrap());
        assert_eq!(a.as_slice(), b.as_slice());

        let mut c = CanonicalBytes::new();
        c.rational(&rate(30_000, 1001));
        assert_ne!(a.as_slice(), c.as_slice());
    }

    #[test]
    fn domains_must_be_non_empty_ascii() {
        assert!(CanonicalBytes::with_domain("").is_err());
        assert!(CanonicalBytes::with_domain("anim_engine/\u{3000}v1").is_err());
        assert!(CanonicalBytes::with_domain("anim_engine/payload_hash/v1").is_ok());
    }

    #[test]
    fn hex_is_lowercase_and_full_width() {
        let mut cp = CanonicalBytes::with_domain("anim_engine/test/v1").unwrap();
        cp.u64(0);
        let text = cp.hex_digest();
        assert_eq!(text.len(), 64);
        assert_eq!(text.to_lowercase(), text);
        assert!(text.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
