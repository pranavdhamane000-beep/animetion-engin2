//! Typed errors for arithmetic, ranges, identifiers and canonical encoding
//! (spec 10.11).
//!
//! Spec 7.4 requires typed errors with stable categories, and spec 23's overflow
//! row requires the *offending value* to travel with the error rather than a bare
//! code: "Reject operation with offending value context → No canonical state
//! mutation." Every variant here therefore carries the operands or bounds that
//! were rejected, rendered in canonical form. Nothing carries a file's contents.
//!
//! The six variants spec 10.11 names are all present. `MalformedCanonical` and
//! `NonFiniteValue` are additions, because 10.11's list covers arithmetic but the
//! canonical string parser and the float encoder in `hash.rs` still need to
//! refuse input rather than accept it silently.

use crate::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};

/// Every rejection produced by `anim_core`'s time, identity and encoding types.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    /// A denominator of zero was offered. Distinct from a non-positive rate
    /// because the caller is a rational constructor, not a frame rate.
    #[error("rational denominator is zero (numerator {numerator})")]
    ZeroDenominator { numerator: String },

    /// A frame rate or sample rate that is zero or would be non-positive.
    #[error("rate must be strictly positive, got {numerator}/{denominator}")]
    NonPositiveRate {
        numerator: String,
        denominator: String,
    },

    /// A result that does not fit the persisted width. Never wraps, never clamps.
    #[error("arithmetic overflow in {operation}: {value}")]
    ArithmeticOverflow {
        operation: &'static str,
        value: String,
    },

    /// A range whose bounds contradict each other or fall outside an allowed set.
    #[error("invalid {kind} range [{start}, {end})")]
    InvalidRange {
        kind: &'static str,
        start: i64,
        end: i64,
    },

    /// A subframe outside `[0, 1)` reached a constructor that does not carry.
    #[error("subframe {value} is outside [0, 1)")]
    SubframeOutOfRange { value: String },

    /// An identifier string that is not a lowercase hyphenated UUID.
    #[error("identifier could not be parsed: {raw}")]
    IdentifierParse { raw: String },

    /// Canonical text or a domain separator that is not in canonical form.
    /// Accepted-and-normalized would let two byte streams describe one value,
    /// which is what makes a saved document stop matching its own hash.
    #[error("value is not in canonical form: {raw}")]
    MalformedCanonical { raw: String },

    /// A non-finite float reached canonical encoding. Spec 7.1 permits floats in
    /// canonical hashing only as explicitly encoded finite bit patterns.
    #[error("non-finite floating-point value cannot be encoded canonically (bits {bits:#018x})")]
    NonFiniteValue { bits: u64 },
}

impl CoreError {
    /// The stable category for this error (spec 7.4).
    ///
    /// Categories are append-only and dot-separated, matching
    /// [`DiagnosticCode`], so a report written now stays readable by a tool built
    /// after the next package.
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        let raw = match self {
            Self::ZeroDenominator { .. } => "core.zero_denominator",
            Self::NonPositiveRate { .. } => "core.non_positive_rate",
            Self::ArithmeticOverflow { .. } => "core.arithmetic_overflow",
            Self::InvalidRange { .. } => "core.invalid_range",
            Self::SubframeOutOfRange { .. } => "core.subframe_out_of_range",
            Self::IdentifierParse { .. } => "core.identifier_parse",
            Self::MalformedCanonical { .. } => "core.malformed_canonical",
            Self::NonFiniteValue { .. } => "core.non_finite_value",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    /// This error's severity. Every variant here rejects an operation, so all of
    /// them are errors rather than warnings; the mapping exists so a caller
    /// converting to [`Diagnostic`] does not pick a severity ad hoc.
    #[must_use]
    pub fn severity(&self) -> DiagnosticSeverity {
        DiagnosticSeverity::Error
    }

    /// Render this error as a structured diagnostic carrying its own message.
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        Diagnostic::new(self.code(), self.severity(), self.to_string())
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
    fn every_variant_has_a_distinct_stable_code() {
        let errors = [
            CoreError::ZeroDenominator {
                numerator: "1".into(),
            },
            CoreError::NonPositiveRate {
                numerator: "0".into(),
                denominator: "1".into(),
            },
            CoreError::ArithmeticOverflow {
                operation: "test",
                value: "x".into(),
            },
            CoreError::InvalidRange {
                kind: "test",
                start: 1,
                end: 0,
            },
            CoreError::SubframeOutOfRange {
                value: "3/2".into(),
            },
            CoreError::IdentifierParse { raw: "nope".into() },
            CoreError::MalformedCanonical {
                raw: "10/20".into(),
            },
            CoreError::NonFiniteValue { bits: u64::MAX },
        ];
        let mut codes: Vec<String> = errors
            .iter()
            .map(|e| e.code().as_str().to_owned())
            .collect();
        codes.sort();
        let before = codes.len();
        codes.dedup();
        assert_eq!(before, codes.len(), "codes must distinguish variants");
        for code in &codes {
            assert!(
                code.starts_with("core."),
                "unexpected code namespace: {code}"
            );
        }
    }

    #[test]
    fn message_names_the_offending_value() {
        // Spec 23 requires the rejected value to be recoverable from the error
        // text, so a caller can report what it was holding without re-deriving it.
        let e = CoreError::SubframeOutOfRange {
            value: "3/2".into(),
        };
        assert!(e.to_string().contains("3/2"), "{e}");
        let e = CoreError::InvalidRange {
            kind: "frame",
            start: 5,
            end: 2,
        };
        assert!(e.to_string().contains("[5, 2)"), "{e}");
    }

    #[test]
    fn diagnostic_carries_code_and_error_severity() {
        let d = CoreError::MalformedCanonical { raw: "01/2".into() }.to_diagnostic();
        assert_eq!(d.code.as_str(), "core.malformed_canonical");
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        assert!(Diagnostic::any_error(std::slice::from_ref(&d)));
    }
}
