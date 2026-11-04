//! Structured diagnostics with stable categories (spec 7.4).
//!
//! The spec declares `Vec<Diagnostic>` as a field of `ProbeReport` (9.8),
//! `AcceptanceReport.warnings` (46) and `Level3Report.limits` (49), but never
//! defines the type. This is the definition those three sites share. Its shape is
//! taken from `MechanicDiagnostic` (spec 47), the only diagnostic-shaped struct the
//! document does define: a stable code, a severity, an optional structured
//! location, and a human-readable message.
//!
//! Spec 7.4 forbids logging secrets or arbitrary user file contents, so context
//! values are constructed at the call site from identifiers the engine already
//! owns rather than from raw input.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Severity of a diagnostic. Ordered from least to most consequential.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// Informational. Never fails a gate on its own.
    Info,
    /// A capability or expectation that was not met but is explicitly permitted.
    Warning,
    /// A requirement that failed. Presence of an error makes a report unsuccessful.
    Error,
}

/// A namespaced, stable diagnostic category.
///
/// The dot-separated string is the wire form; producers construct it through
/// [`DiagnosticCode::new`] so the namespace cannot be empty and the code cannot
/// contain whitespace. Codes are append-only: a code that a report has ever
/// emitted stays meaningful, because gate evidence outlives the build that
/// produced it (spec 8.2).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DiagnosticCode(String);

impl DiagnosticCode {
    /// Build a code from a dotted namespace. Panics are avoided by returning
    /// `None` for an empty or whitespace-bearing value rather than accepting one.
    pub fn new(code: &str) -> Option<Self> {
        let trimmed = code.trim();
        if trimmed.is_empty() || trimmed.len() != code.len() || code.contains(char::is_whitespace) {
            return None;
        }
        Some(Self(trimmed.to_owned()))
    }

    /// Fall back code for a path that a producer forgot to label. Using it is a
    /// defect in the producer, never in the caller, so it is always a `Warning`.
    #[must_use]
    pub fn unspecified() -> Self {
        Self("core.unspecified".to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Safe, identifying context for one diagnostic.
///
/// Keys are engine-owned identifiers (`asset`, `command`, `expected_revision`,
/// `frame`, `tick`, `resource`, `export_stage`, `process_exit`) rather than free
/// text, so a report can be diffed across runs. A `BTreeMap` is used deliberately:
/// serialized order must not depend on insertion order (spec 7.1).
pub type DiagnosticContext = BTreeMap<String, String>;

/// One structured finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub context: DiagnosticContext,
}

impl Diagnostic {
    pub fn new(
        code: DiagnosticCode,
        severity: DiagnosticSeverity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            severity,
            message: message.into(),
            context: DiagnosticContext::new(),
        }
    }

    /// Attach one identifier-valued piece of context. A repeated key replaces the
    /// earlier value, which is what a retry path needs: the report describes the
    /// state that decided the outcome, not every state it passed through.
    #[must_use]
    pub fn with_context(mut self, key: &str, value: impl Into<String>) -> Self {
        self.context.insert(key.to_owned(), value.into());
        self
    }

    pub fn info(code: DiagnosticCode, message: impl Into<String>) -> Self {
        Self::new(code, DiagnosticSeverity::Info, message)
    }

    pub fn warning(code: DiagnosticCode, message: impl Into<String>) -> Self {
        Self::new(code, DiagnosticSeverity::Warning, message)
    }

    pub fn error(code: DiagnosticCode, message: impl Into<String>) -> Self {
        Self::new(code, DiagnosticSeverity::Error, message)
    }

    /// True when any error-severity diagnostic is present. This is the rule a
    /// machine-readable report uses to decide success, so that "success" is a
    /// function of the findings rather than of an exit code a probe chose freely.
    #[must_use]
    pub fn any_error(diagnostics: &[Self]) -> bool {
        diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error)
    }

    #[must_use]
    pub fn max_severity(diagnostics: &[Self]) -> Option<DiagnosticSeverity> {
        diagnostics.iter().map(|d| d.severity).max()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assertion here should abort the test; the production rule these would violate is enforced by the deny at the workspace level"
    )]

    use super::*;

    #[test]
    fn code_rejects_blank_and_whitespace_forms() {
        assert!(DiagnosticCode::new("").is_none());
        assert!(DiagnosticCode::new("   ").is_none());
        assert!(DiagnosticCode::new(" leading").is_none());
        assert!(DiagnosticCode::new("gpu.readback mismatch").is_none());
        assert!(DiagnosticCode::new("gpu.readback_mismatch").is_some());
    }

    #[test]
    fn context_order_survives_insertion_order() {
        // Two diagnostics carrying the same context keys in opposite insertion
        // orders must serialize identically, or report hashing is unstable.
        let a = Diagnostic::error(DiagnosticCode::new("probe.a").unwrap(), "m")
            .with_context("frame", "1")
            .with_context("asset", "ab");
        let b = Diagnostic::error(DiagnosticCode::new("probe.a").unwrap(), "m")
            .with_context("asset", "ab")
            .with_context("frame", "1");
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn any_error_tracks_presence_not_order() {
        let warn = Diagnostic::warning(DiagnosticCode::new("probe.w").unwrap(), "w");
        let err = Diagnostic::error(DiagnosticCode::new("probe.e").unwrap(), "e");
        assert!(!Diagnostic::any_error(std::slice::from_ref(&warn)));
        assert!(Diagnostic::any_error(&[err.clone(), warn.clone()]));
        assert!(Diagnostic::any_error(&[warn, err]));
        assert!(!Diagnostic::any_error(&[]));
    }
}
