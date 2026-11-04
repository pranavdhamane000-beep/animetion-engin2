//! Structured probe reports and exit handling (spec 9.3).
//!
//! Every `wp00_*` binary funnels through this module so that "did the platform
//! satisfy the requirement" is expressed the same way five times rather than
//! five different ways. Spec 9.6: each probe is isolated and returns a nonzero
//! status on unmet requirements.
//!
//! Two report shapes, deliberately distinct:
//!   * [`ProbeReport`] is the machine-readable envelope that lands on disk. Its
//!     fields are fixed by spec 9.8.
//!   * [`ProbeError`] is the typed library error (spec 9.4: libraries use typed
//!     errors; only binaries may use `anyhow`). Its variants are the *distinct*
//!     failure kinds that spec 9.7 requires — collapsing two of them into one
//!     would let a probe pass by mislabelling why it failed.

use std::io::Write as _;
use std::path::Path;

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
use serde::{Deserialize, Serialize};

/// Schema version of the on-disk report. Bump when a field's meaning changes, not
/// when a field is added: readers of G0 evidence must be able to tell a
/// reinterpreted field from a new one (spec 8.2).
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// The machine-readable envelope every probe writes (spec 9.8).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeReport {
    pub schema_version: u32,
    pub capability: String,
    pub success: bool,
    pub diagnostics: Vec<Diagnostic>,
}

impl ProbeReport {
    /// Build the envelope from the findings alone. `success` is derived, never
    /// supplied: a probe cannot claim success while carrying an error finding, and
    /// cannot admit an error while claiming success.
    #[must_use]
    pub fn new(capability: &str, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            capability: capability.to_owned(),
            success: !Diagnostic::any_error(&diagnostics),
            diagnostics,
        }
    }

    #[must_use]
    pub fn with_diagnostics(capability: &str, diagnostics: Vec<Diagnostic>) -> Self {
        Self::new(capability, diagnostics)
    }

    /// Serialize canonically. Field order is fixed by the struct declaration and
    /// diagnostic context maps are `BTreeMap`, so two runs that observed the same
    /// thing produce the same bytes.
    ///
    /// # Errors
    ///
    /// `ProbeError::ReportSerialization` when the report cannot be encoded. A report
    /// that will not serialize is a probe failure, because the serialized form is the
    /// G0 deliverable (spec 9.5 step 10).
    pub fn to_json(&self) -> Result<String, ProbeError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| ProbeError::ReportSerialization(e.to_string()))
    }

    /// Write the report into `dir` as `<capability>.json`, creating the directory.
    ///
    /// Reports are evidence, not scratch: a write failure is a probe failure,
    /// because spec 9.5 step 10 makes the machine-readable report a deliverable.
    ///
    /// # Errors
    ///
    /// `ProbeError::Io` for a failure to create the directory, create the file, write
    /// it, or sync it; `ProbeError::ReportSerialization` if the report cannot be
    /// encoded first.
    pub fn write_to_dir(&self, dir: &Path) -> Result<(), ProbeError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| ProbeError::io("create report directory", dir, &e))?;
        let path = dir.join(format!("{}.json", self.capability));
        let bytes = self.to_json()?;
        // Write-then-rename is not needed here: the report is a leaf artifact with
        // no readers that depend on atomicity, and inventing an atomic path for it
        // would imply a durability requirement the spec does not place on reports.
        let mut file = std::fs::File::create(&path)
            .map_err(|e| ProbeError::io("create report file", &path, &e))?;
        file.write_all(bytes.as_bytes())
            .map_err(|e| ProbeError::io("write report file", &path, &e))?;
        file.sync_all()
            .map_err(|e| ProbeError::io("sync report file", &path, &e))?;
        Ok(())
    }

    /// The single exit-status rule shared by all probes (spec 9.6).
    #[must_use]
    pub fn exit_code(&self) -> std::process::ExitCode {
        if self.success {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::FAILURE
        }
    }
}

/// Adapter identity as recorded by the GPU and adapter probes.
///
/// Spec 9.7 compares `deviceLUID` across Vulkan and DXGI and cross-checks
/// vendor/device ids, so all three are carried. `luid` is `None` when the
/// `deviceLUIDValid` flag is clear or when the Vulkan query is unsupported; that is
/// an explicit unavailable state, never a silent zero.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterIdentity {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub luid: Option<String>,
    pub device_pci_bus_id: Option<String>,
    pub driver: Option<String>,
    pub driver_info: Option<String>,
}

/// GPU/readback probe output (spec 9.8).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuProbeReport {
    pub adapter: AdapterIdentity,
    pub backend: String,
    pub driver: Option<String>,
    pub max_buffer_size: u64,
    pub rgba16f_attachment: bool,
    pub compute_readback_verified: bool,
}

/// Typed probe failures. Each variant is one of the distinct outcomes spec 9.7
/// and 9.11 require; the mapping from variant to diagnostic code is fixed so a
/// report's code can be looked up from the spec section that demanded the split.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    // --- GPU / readback (spec 9.7 "unsupported compute, shader compilation,
    //     device loss and mapping failure are distinct failures") ---
    #[error("no Vulkan adapter was enumerated")]
    NoAdapter,
    #[error("requested adapter index {0} is not present ({1} adapters enumerated)")]
    AdapterNotFound(usize, usize),
    #[error("adapter does not support compute")]
    UnsupportedCompute,
    #[error("WGSL failed to compile: {0}")]
    ShaderCompilation(String),
    #[error("device was lost before readback completed: {0}")]
    DeviceLoss(String),
    #[error("output buffer mapping failed: {0}")]
    MappingFailure(String),
    #[error("readback values did not match the known compute inputs: {0}")]
    ReadbackMismatch(String),
    #[error("device feature request was refused: {0}")]
    FeatureUnavailable(String),

    // --- Adapter correlation (spec 9.7 "Output: matched AdapterIdentity or
    //     IdentityUnavailable") ---
    #[error("adapter identity is unavailable: {0}")]
    IdentityUnavailable(String),
    #[error("Vulkan and DXGI report different LUIDs for the rendering adapter")]
    LuidMismatch,
    #[error("LUID matched but vendor/device ids disagree (vulkan {0}/{1}, dxgi {2}/{3})")]
    VendorDeviceMismatch(u32, u32, u32, u32),

    // --- External tools (spec 9.4, 9.11) ---
    #[error("executable path is not configured or does not exist: {0}")]
    ToolNotConfigured(String),
    #[error("executable digest does not match tools/ffmpeg/ffmpeg-manifest.json: {0}")]
    ToolDigestMismatch(String),
    #[error("FFmpeg/FFprobe exited nonzero ({0}) after {1} on stderr")]
    ToolNonzeroExit(i32, String),
    #[error("external process exceeded its timeout and was terminated: {0}")]
    ToolTimeout(String),
    #[error("child process could not be spawned for {0}")]
    ToolSpawn(String, #[source] std::io::Error),
    #[error("tool emitted malformed JSON: {0}")]
    MalformedJson(String),
    #[error("requested stream is missing from the tool output")]
    MissingStream,
    #[error("declared capability is absent from the tool build: {0}")]
    CapabilityAbsent(String),

    // --- Media decode accounting ---
    #[error("decoded sample count {got} is outside the declared range {expected_low}..={expected_high} ({rationale})")]
    SampleCountOutOfRange {
        got: u64,
        expected_low: u64,
        expected_high: u64,
        rationale: String,
    },
    #[error("decoded count mismatch: expected {expected}, got {got}")]
    CountMismatch { expected: u64, got: u64 },

    // --- Atomic replacement (spec 9.7 "a fault before commit must leave old
    //     bytes; a fault after commit may leave new bytes but never mixed bytes") ---
    #[error("replacement fault point {0} left a destination that is neither the old nor the new complete state")]
    MixedBytesAfterFault(String),
    #[error("destination and backup disagree after replacement at fault point {0}")]
    BackupDisagreement(String),
    #[error("temporary file would not be on the same volume as the destination")]
    CrossVolume,

    // --- Environment around the probe itself ---
    #[error("i/o failure during {action} at {path}")]
    Io {
        action: String,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration error: {0}")]
    Config(String),
    #[error("probe report could not be serialized")]
    ReportSerialization(String),
    #[error("a required resource was not released on some return path")]
    Leak,
}

impl ProbeError {
    /// Build an `Io` finding. Public because a `src/bin/*.rs` target links this
    /// crate as an external crate, so `pub(crate)` would not reach it.
    #[must_use]
    pub fn io(action: &str, path: &Path, source: &std::io::Error) -> Self {
        Self::Io {
            action: action.to_owned(),
            path: path.display().to_string(),
            source: std::io::Error::new(source.kind(), source.to_string()),
        }
    }

    /// Stable diagnostic code per failure kind. Keeping the table here means a
    /// probe cannot invent a flattering code for a failure it did not expect.
    pub fn diagnostic_code(&self) -> DiagnosticCode {
        // DiagnosticCode::new returns None only for malformed input, and every
        // literal below is well-formed; fall back to the unspecified code rather
        // than unwrapping, so a future typo degrades to a warning rather than a
        // panic inside the error reporter.
        let raw = match self {
            Self::NoAdapter => "gpu.no_adapter",
            Self::AdapterNotFound(..) => "gpu.adapter_not_found",
            Self::UnsupportedCompute => "gpu.unsupported_compute",
            Self::ShaderCompilation(_) => "gpu.shader_compilation",
            Self::DeviceLoss(_) => "gpu.device_loss",
            Self::MappingFailure(_) => "gpu.mapping_failure",
            Self::ReadbackMismatch(_) => "gpu.readback_mismatch",
            Self::FeatureUnavailable(_) => "gpu.feature_unavailable",
            Self::IdentityUnavailable(_) => "adapter.identity_unavailable",
            Self::LuidMismatch => "adapter.luid_mismatch",
            Self::VendorDeviceMismatch(..) => "adapter.vendor_device_mismatch",
            Self::ToolNotConfigured(_) => "tool.not_configured",
            Self::ToolDigestMismatch(_) => "tool.digest_mismatch",
            Self::ToolNonzeroExit(..) => "tool.nonzero_exit",
            Self::ToolTimeout(_) => "tool.timeout",
            Self::ToolSpawn(..) => "tool.spawn",
            Self::MalformedJson(_) => "tool.malformed_json",
            Self::MissingStream => "media.missing_stream",
            Self::CapabilityAbsent(_) => "tool.capability_absent",
            Self::SampleCountOutOfRange { .. } => "media.sample_count",
            Self::CountMismatch { .. } => "media.count_mismatch",
            Self::MixedBytesAfterFault(_) => "replace.mixed_bytes",
            Self::BackupDisagreement(_) => "replace.backup_disagreement",
            Self::CrossVolume => "replace.cross_volume",
            Self::Io { .. } => "core.io",
            Self::Config(_) => "core.config",
            Self::ReportSerialization(_) => "core.report_serialization",
            Self::Leak => "core.leak",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    /// Every probe failure is an error-severity finding. An "unsupported optional
    /// capability" is recorded as a `Diagnostic::info` by the probe that observed
    /// it, not by this mapping (spec 9.11 requires it recorded explicitly rather
    /// than reported as available).
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        let mut d = Diagnostic::new(
            self.diagnostic_code(),
            DiagnosticSeverity::Error,
            self.to_string_display(),
        );
        if let Self::Io { action, path, .. } = self {
            d = d.with_context("action", action).with_context("path", path);
        }
        if let Self::AdapterNotFound(wanted, count) = self {
            d = d
                .with_context("requested_ordinal", wanted.to_string())
                .with_context("adapter_count", count.to_string());
        }
        if let Self::DeviceLoss(reason) = self {
            d = d.with_context("loss_reason", reason.clone());
        }
        if let Self::ToolNonzeroExit(code, tail) = self {
            d = d
                .with_context("process_exit", code.to_string())
                .with_context("stderr_tail", tail.clone());
        }
        if let Self::CountMismatch { expected, got } = self {
            d = d
                .with_context("expected", expected.to_string())
                .with_context("got", got.to_string());
        }
        if let Self::SampleCountOutOfRange {
            got,
            expected_low,
            expected_high,
            rationale,
        } = self
        {
            d = d
                .with_context("got", got.to_string())
                .with_context("expected_low", expected_low.to_string())
                .with_context("expected_high", expected_high.to_string())
                .with_context("rationale", rationale.clone());
        }
        d
    }

    /// `thiserror` owns `Display`; this is the same text without the source chain,
    /// kept separate so a report never embeds a filesystem path twice by accident.
    fn to_string_display(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        // Writing to a String cannot fail; the ignore is structural, not a guess.
        let _ = write!(s, "{self}");
        s
    }
}

/// Turn one probe's outcome into the report and the process exit status, and print
/// a one-line human summary so a terminal run is readable without opening the JSON.
#[must_use]
pub fn finish(
    capability: &str,
    diagnostics: Vec<Diagnostic>,
    report_dir: &Path,
) -> std::process::ExitCode {
    let mut report = ProbeReport::new(capability, diagnostics);

    if let Err(e) = report.write_to_dir(report_dir) {
        // A report that cannot be written invalidates the probe: spec 9.5 step 10
        // makes the machine-readable report a deliverable, not a convenience.
        report.diagnostics.push(e.to_diagnostic());
        report.success = false;
    }

    println!(
        "{}: {} ({} diagnostic{}), schema v{}",
        report.capability,
        if report.success { "PASS" } else { "FAIL" },
        report.diagnostics.len(),
        if report.diagnostics.len() == 1 {
            ""
        } else {
            "s"
        },
        report.schema_version
    );
    for d in &report.diagnostics {
        println!("  [{:?}] {}: {}", d.severity, d.code.as_str(), d.message);
    }
    report.exit_code()
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
    fn success_is_derived_from_findings_not_claimed() {
        let err = Diagnostic::error(DiagnosticCode::new("gpu.x").unwrap(), "boom");
        let ok = Diagnostic::info(DiagnosticCode::new("gpu.y").unwrap(), "fine");
        assert!(!ProbeReport::new("c", vec![err]).success);
        assert!(ProbeReport::new("c", vec![ok.clone()]).success);
        assert!(ProbeReport::new("c", vec![]).success);
        // A probe cannot report success by passing an empty list while holding an
        // error somewhere else: the constructor has no other input.
        assert_eq!(ProbeReport::new("c", vec![ok]).diagnostics.len(), 1);
    }

    #[test]
    fn every_error_variant_has_a_distinct_code() {
        let errors = all_error_variants();
        let mut seen = std::collections::BTreeSet::new();
        for e in &errors {
            let code = e.diagnostic_code();
            let dupped = !seen.insert(code.as_str().to_owned());
            assert!(
                !dupped,
                "two failure kinds share diagnostic code {}; spec 9.7 requires they stay distinct",
                code.as_str()
            );
        }
        assert_eq!(
            seen.len(),
            errors.len(),
            "variant list and code table are out of step"
        );
    }

    #[test]
    fn error_variants_render_and_carry_context() {
        for e in all_error_variants() {
            let d = e.to_diagnostic();
            assert_eq!(d.severity, DiagnosticSeverity::Error);
            assert_ne!(d.message, "");
        }
        let d = ProbeError::Io {
            action: "write report file".into(),
            path: "probe-out/x.json".into(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        }
        .to_diagnostic();
        assert_eq!(
            d.context.get("path").map(String::as_str),
            Some("probe-out/x.json")
        );
    }

    #[test]
    fn report_json_round_trips_and_is_field_stable() {
        let r = ProbeReport::new(
            "wp00_gpu",
            vec![Diagnostic::info(
                DiagnosticCode::new("gpu.rgba16f").unwrap(),
                "available",
            )],
        );
        let json = r.to_json().unwrap();
        let back: ProbeReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.capability, r.capability);
        assert_eq!(back.schema_version, REPORT_SCHEMA_VERSION);
        assert!(json.contains("\"capability\": \"wp00_gpu\""));
    }

    #[test]
    fn exit_code_is_failure_unless_clean() {
        assert_eq!(
            ProbeReport::new("c", vec![]).exit_code(),
            std::process::ExitCode::SUCCESS
        );
        let e = ProbeError::DeviceLoss("destroyed".into()).to_diagnostic();
        assert_ne!(
            ProbeReport::new("c", vec![e]).exit_code(),
            std::process::ExitCode::SUCCESS
        );
    }

    #[test]
    fn report_write_lands_under_the_named_capability() {
        let dir = tempfile::tempdir().unwrap();
        let r = ProbeReport::new(
            "wp00_replacement",
            vec![Diagnostic::info(
                DiagnosticCode::new("replace.old").unwrap(),
                "kept",
            )],
        );
        r.write_to_dir(dir.path()).unwrap();
        let written = dir.path().join("wp00_replacement.json");
        assert!(
            written.exists(),
            "report must be findable by capability name"
        );
        let text = std::fs::read_to_string(&written).unwrap();
        assert!(text.contains("wp00_replacement"));
    }

    /// One instance of every `ProbeError` variant, so the tests above cover the
    /// whole taxonomy and a variant added without a code fails loudly.
    fn all_error_variants() -> Vec<ProbeError> {
        vec![
            ProbeError::NoAdapter,
            ProbeError::AdapterNotFound(3, 2),
            ProbeError::UnsupportedCompute,
            ProbeError::ShaderCompilation("unexpected token".into()),
            ProbeError::DeviceLoss("VK_ERROR_DEVICE_LOST".into()),
            ProbeError::MappingFailure("buffer already mapped".into()),
            ProbeError::ReadbackMismatch("first divergence at element 3".into()),
            ProbeError::FeatureUnavailable("features(VERTEX_WRITABLE_STORAGE)".into()),
            ProbeError::IdentityUnavailable("deviceLUIDValid false".into()),
            ProbeError::LuidMismatch,
            ProbeError::VendorDeviceMismatch(0x10de, 0x25e9, 0x8086, 0xa7ab),
            ProbeError::ToolNotConfigured("tools/ffmpeg/ffmpeg.exe".into()),
            ProbeError::ToolDigestMismatch("sha mismatch".into()),
            ProbeError::ToolNonzeroExit(1, "Invalid data found".into()),
            ProbeError::ToolTimeout("ffprobe".into()),
            ProbeError::ToolSpawn(
                "ffprobe".into(),
                std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
            ),
            ProbeError::MalformedJson("expected value at 1".into()),
            ProbeError::MissingStream,
            ProbeError::CapabilityAbsent("libx264".into()),
            ProbeError::SampleCountOutOfRange {
                got: 500,
                expected_low: 480,
                expected_high: 481,
                rationale: "decoder delay".into(),
            },
            ProbeError::CountMismatch {
                expected: 10,
                got: 9,
            },
            ProbeError::MixedBytesAfterFault("before_commit".into()),
            ProbeError::BackupDisagreement("before_commit".into()),
            ProbeError::CrossVolume,
            ProbeError::Io {
                action: "open".into(),
                path: "x".into(),
                source: std::io::Error::other("x"),
            },
            ProbeError::Config("adapter_ordinal out of range".into()),
            ProbeError::ReportSerialization("value".into()),
            ProbeError::Leak,
        ]
    }
}
