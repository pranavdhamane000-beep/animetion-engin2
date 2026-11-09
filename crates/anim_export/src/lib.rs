//! `anim_export` — resolved export jobs, FFmpeg, readback coordination, verification and
//! publication (spec 18).
//!
//! The crate turns a saved project plus a fixed [`settings::ExportSettings`] into one
//! deterministic file on disk through the pipeline of 18.5: validate the settings against the
//! project and codec matrix, verify every referenced asset, prepare the simulation
//! deterministically, render the offline WAV and check its sample count, then for each frame
//! evaluate → render fixed-quality → read back → strip row padding → write packed RGBA to a
//! verified FFmpeg process; close and reap the encoder, mux without re-encoding, decode and
//! verify counts and metadata, and atomically publish a same-volume temporary to the final
//! destination. Every failure path (18.11) leaves any previous destination untouched, never
//! retries at lower quality, and never substitutes an unverified codec or a system FFmpeg
//! (spec 9.11).
//!
//! The public surface 18.9 defines is `validate`, `execute`, `verify_output`, `publish` and
//! `run`. This crate currently lands the deterministic half of that surface — [`validate`],
//! [`verify_output`], [`publish`], the codec matrix, the profile table, the job manifest, the
//! FFmpeg tool-integrity + argument contract and the typed error taxonomy — all of it verified
//! without a GPU or a spawned process. `execute` and `run` drive the readback/pipeline loop and
//! are wired in `pipeline.rs`/`cli` when the GPU runtime integration lands (see the WP09 handoff
//! in `BUILD_STATE.md`); nothing references them yet. WP10's editor will call the same job API in
//! an isolated context — nothing here reads viewport, selection or playback state (18.6).
//!
//! ## The two hashes, kept straight
//!
//! Blake3 ([`anim_assets::ContentHash`]) is the internal content hash — it is what
//! [`ExportReport`] carries and what the environment/determinism split is drawn around.
//! SHA-256 is used *only* to verify the external FFmpeg executable against
//! `tools/ffmpeg/ffmpeg-manifest.json` before spawn (spec 5.1, ADR-0014); the two never
//! substitute for one another and the reason is recorded in ADR-0001/ADR-0014.

pub mod ffmpeg;
pub mod manifest;
pub mod progress;
pub mod publish;
pub mod settings;
pub mod verify;

use std::path::PathBuf;

use anim_assets::ContentHash;

pub use manifest::JobManifest;
pub use progress::{CancellationToken, NullProgress, Progress, ProgressSink, Stage};
pub use publish::publish;
pub use settings::{
    validate, AudioMode, CodecId, ExportAlphaMode, ExportSettings, QualityProfile, ResolvedExport,
};
pub use verify::{verify_output, ExpectedAudio, ExpectedCounts};

/// The machine-checkable reasons `validate` rejects a settings/project combination. Each is a
/// pre-render failure, which is the point: 18.12's "H.264 alpha and odd dimensions fail before
/// rendering" and 18.11's "existing destination without overwrite fails before rendering" are
/// observable only because the reason is named *and* produced before the pipeline starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationCheck {
    /// `ExportSettings.schema_version` is not the version this build resolves.
    SchemaVersion,
    /// The codec row is in the 18.6 matrix but its FFmpeg capability is not certified here
    /// (WP09 MP4-only ruling): MOV and PNG-sequence land on this.
    CapabilityNotCertified,
    /// Alpha requested on a row that carries none (MP4/H.264).
    AlphaNotSupported,
    /// Audio mode requested that the row's audio column does not allow.
    AudioNotSupported,
    /// A zero width or height.
    ZeroDimension,
    /// An extent past `MAX_DIMENSION`/`MAX_AREA`.
    ExcessiveDimensions,
    /// Odd width or height on a row requiring even (H.264 `yuv420p`).
    OddDimension,
    /// Packed frame length would overflow `usize`.
    StrideOverflow,
    /// Export fps differs from the project frame rate (one timeline, one rate — WP05).
    FrameRateMismatch,
    /// An empty frame range — zero frames is not a legal export.
    EmptyRange,
    /// Destination already exists and `overwrite` is false (checked before rendering).
    DestinationExists,
}

/// Every typed failure the export path can produce (spec 18.11). The categories are 18.11's
/// stable list — validation, asset, simulation, GPU, memory, encoder, mux, verification, I/O,
/// cancellation and publication — plus the tool-integrity and process-lifecycle errors the FFmpeg
/// boundary needs. 18.11's guarantees ("errors never trigger a lower-quality retry or an
/// unverified codec fallback", "verification or publication failure leaves any previous
/// destination untouched") are the reason these are separate variants rather than one opaque
/// error string: the caller can tell a *prevention* (this output must not publish) from a
/// *retry candidate*, and none of them is retried internally.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("export validation failed ({check:?}): {detail}")]
    Validation {
        check: ValidationCheck,
        detail: String,
    },

    #[error(transparent)]
    Project(#[from] anim_project::ProjectError),

    #[error(transparent)]
    Core(#[from] anim_core::error::CoreError),

    #[error(transparent)]
    Asset(#[from] anim_assets::AssetError),

    #[error(transparent)]
    Scene(#[from] anim_scene::SceneError),

    #[error(transparent)]
    Simulation(#[from] anim_sim::SimError),

    #[error(transparent)]
    Gpu(#[from] anim_render::RenderError),

    #[error(transparent)]
    Audio(#[from] anim_audio::AudioError),

    /// A budget/refusal from the WP07 memory manager reached during readback or upload.
    #[error("GPU memory budget refused {purpose} ({requested} bytes): {detail}")]
    Memory {
        purpose: &'static str,
        requested: u64,
        detail: String,
    },

    /// The FFmpeg executable failed its SHA-256 manifest check, or the wrong binary was
    /// presented under a role (spec 9.11 — this is terminal, there is no PATH fallback).
    #[error("FFmpeg tool integrity check failed for {role}: {detail}")]
    ToolIntegrity { role: String, detail: String },

    /// A required codec/pixel-format/container capability was absent from the probed build.
    #[error("required FFmpeg capability unavailable: {capability}")]
    CapabilityMissing { capability: String },

    /// The raw-video encode stage: a broken pipe or non-zero exit carries FFmpeg's status and
    /// the bounded stderr tail (18.11 "Broken pipes include FFmpeg status and bounded
    /// diagnostics").
    #[error("encoder failure at {stage} (ffmpeg exit {status:?}): {detail}\n--- ffmpeg stderr tail ---\n{stderr_tail}")]
    Encoder {
        stage: &'static str,
        detail: String,
        status: Option<i32>,
        stderr_tail: String,
    },

    /// The mux stage (map streams, copy video, encode audio) — same diagnostic shape.
    #[error(
        "mux failure (ffmpeg exit {status:?}): {detail}\n--- ffmpeg stderr tail ---\n{stderr_tail}"
    )]
    Mux {
        detail: String,
        status: Option<i32>,
        stderr_tail: String,
    },

    /// An FFmpeg process exceeded its deadline; it was terminated and reaped (18.11).
    #[error("FFmpeg {role} timed out and was terminated and reaped")]
    Timeout { role: &'static str },

    /// Verification of the produced file: a count, dimension, frame rate, codec id, pixel
    /// format, stream order or audio-priming window that did not match the resolved expectation.
    #[error("output verification failed ({check}): expected {expected}, found {found}")]
    Verification {
        check: &'static str,
        expected: String,
        found: String,
    },

    /// Readback returned fewer bytes than one packed frame — 18.7's "short readback … fails
    /// before publication".
    #[error("short readback for frame {frame_index}: expected {expected} bytes, mapped {got}")]
    ShortReadback {
        frame_index: i64,
        expected: usize,
        got: usize,
    },

    /// A produced container the verifier could decode but that has trailing bytes or a shape FFmpeg
    /// should not have emitted.
    #[error("malformed output at {path}: {detail}")]
    MalformedOutput { path: String, detail: String },

    /// The destination volume was out of space while staging or publishing (18.11: disk-full
    /// prevents publication).
    #[error("no space left on device writing {path}")]
    DiskFull { path: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The job's `CancellationToken` was tripped; 18.10's shutdown ran (children reaped,
    /// buffers unmapped, temporaries removed unless diagnostics were preserved).
    #[error("export cancelled at {stage}")]
    Cancellation { stage: &'static str },

    /// The atomic publication step failed (rename/replacement durable-write problem).
    #[error(transparent)]
    Publication(#[from] anim_core::io::atomic_replace::ReplacementError),
}

impl ExportError {
    /// A named validation failure with context (18.11's validation category).
    #[must_use]
    pub fn validation(check: ValidationCheck, detail: impl Into<String>) -> Self {
        Self::Validation {
            check,
            detail: detail.into(),
        }
    }

    /// A cancellation failure at a stage.
    #[must_use]
    pub fn cancellation(stage: &'static str) -> Self {
        Self::Cancellation { stage }
    }

    /// Map an [`std::io::Error`] at a write site, distinguishing disk-full (which must prevent
    /// publication) from every other I/O fault (18.11).
    #[must_use]
    pub fn from_write_io(error: std::io::Error, path: &std::path::Path) -> Self {
        // `StorageFull` is the stable kind for ENOSPC/EDQUOT on the platforms that map it; on
        // Windows the raw code is checked too so a full volume is never mislabelled as generic.
        let disk_full = error.kind() == std::io::ErrorKind::StorageFull
            || raw_os_is_disk_full(error.raw_os_error());
        if disk_full {
            Self::DiskFull {
                path: path.display().to_string(),
            }
        } else {
            Self::Io(error)
        }
    }
}

/// Windows `GetLastError` codes for a full volume: `ERROR_DISK_FULL` (112) and the quota
/// variants. `ErrorKind::StorageFull` does not always surface on Windows for these, so the raw
/// code is checked alongside it (the disk-full-prevents-publication rule in 18.11 must not
/// depend on the std mapping).
fn raw_os_is_disk_full(code: Option<i32>) -> bool {
    const ERROR_DISK_FULL: i32 = 112;
    const ERROR_HANDLE_DISK_FULL: i32 = 39;
    matches!(code, Some(ERROR_DISK_FULL) | Some(ERROR_HANDLE_DISK_FULL))
}

/// The export report (spec 18.8's `ExportReport`). The three deterministic hashes
/// (`project_hash`, `asset_manifest_hash`, `settings_hash`) plus `output_hash` are the identity
/// of the run; `ffmpeg_version` is environment metadata carried *separately* (18.8: "Reports
/// contain environment metadata separately from deterministic settings hashes"), so re-reading
/// a report never needs the FFmpeg binary that made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportReport {
    pub video_frames: u64,
    pub audio_samples: u64,
    pub project_hash: ContentHash,
    pub asset_manifest_hash: ContentHash,
    pub settings_hash: ContentHash,
    pub output_hash: ContentHash,
    pub ffmpeg_version: String,
}

/// An immutable export job (18.9's `execute(job: ExportJob, …)`): the resolved settings, the
/// document revision and manifest it binds to, and the destination. Constructed by [`run`] or by
/// WP10's editor from a validated `ResolvedExport` plus a loaded project. The heavy owned state
/// (document, store, tool handles) is attached in the `job` module once those types land.
#[derive(Clone, Debug)]
pub struct ExportJob {
    pub resolved: ResolvedExport,
    pub destination: PathBuf,
    pub overwrite: bool,
}

// The pipeline entry points (`execute`, `run`) and the FFmpeg/readback/verify/publish/manifest
// modules are added next; they depend on `ffmpeg.rs`'s tool handles and `readback.rs`'s ring.

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only"
    )]

    use super::*;

    #[test]
    fn validation_error_names_its_check() {
        let error = ExportError::validation(ValidationCheck::OddDimension, "320×241");
        assert!(matches!(
            error,
            ExportError::Validation {
                check: ValidationCheck::OddDimension,
                ..
            }
        ));
        assert!(error.to_string().contains("OddDimension"));
    }

    #[test]
    fn a_disk_full_windows_code_is_not_a_generic_io_error() {
        // ERROR_DISK_FULL is 112; the helper must classify the raw code even where the std kind
        // does not, so the disk-full-prevents-publication rule holds on this platform.
        assert!(raw_os_is_disk_full(Some(112)));
        assert!(raw_os_is_disk_full(Some(39)));
        assert!(!raw_os_is_disk_full(Some(2))); // ERROR_FILE_NOT_FOUND is not disk-full
        assert!(!raw_os_is_disk_full(None));
    }
}
