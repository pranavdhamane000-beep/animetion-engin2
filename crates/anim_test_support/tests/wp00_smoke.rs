//! Non-GPU workspace smoke tests (spec 9.3, file `tests/wp00_smoke.rs`).
//!
//! Path deviation, recorded in DECISIONS.md: the root `tests/` directory is not
//! compiled by cargo in a virtual workspace, and that was verified on this
//! workspace rather than assumed (`cargo test --test <name>` reports "no test
//! target"). A smoke test that never runs is worse than none, because it reads as
//! coverage. The file keeps its specified name under the crate that owns it.
//!
//! These tests are the part of G0 that can run without a display, a GPU, or a
//! spawned tool: manifest integrity, configuration shape, report determinism, and
//! the audio fixtures the media probe consumed.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test file: a failed expect/unwrap aborts the test with the panic message, which is the desired behaviour and cannot be expressed by returning Result from #[test]"
)]

use std::path::{Path, PathBuf};

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
use anim_test_support::probe::{ProbeError, ProbeReport, REPORT_SCHEMA_VERSION};
use anim_test_support::probe_tools;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("crate sits two levels below the workspace root")
}

/// The pinned executables are still the pinned executables.
///
/// This is the highest-value non-GPU test in the workspace: if a binary is
/// replaced, upgraded, or corrupted, every media and export result produced after
/// that point is describing a different tool. It hashes rather than spawns, so it
/// costs two reads and no process.
#[test]
fn vendored_executables_match_the_pinned_digests() {
    let root = workspace_root();
    let manifest = root.join("tools/ffmpeg/ffmpeg-manifest.json");
    assert!(manifest.exists(), "missing {}", manifest.display());

    for role in ["ffmpeg", "ffprobe"] {
        let entry = probe_tools::manifest_entry(&manifest, role)
            .unwrap_or_else(|e| panic!("{role} is not declared in the manifest: {e}"));
        let exe = root.join(&entry.relative_path);
        assert!(
            exe.exists(),
            "{role} declares {} but the file is absent; the binaries are an \
             operator-supplied input and are git-ignored by design",
            entry.relative_path
        );
        let actual = probe_tools::sha256_hex(&exe)
            .unwrap_or_else(|e| panic!("{role}: could not hash {}: {e}", exe.display()));
        assert_eq!(
            actual,
            entry.sha256.to_ascii_lowercase(),
            "{role} digest drifted from the pin in {}",
            manifest.display()
        );
        if let Some(size) = entry.size_bytes {
            assert_eq!(
                std::fs::metadata(&exe).expect("stat").len(),
                size,
                "{role} size drifted from the manifest"
            );
        }
    }
}

/// A digest mismatch is detected, not merely recorded.
///
/// Without this, the test above could pass while the comparison itself was inert
/// (a typo that made both sides of an `assert_eq!` the same expression, for
/// instance). The guard has to be shown to reject something.
#[test]
fn digest_guard_rejects_a_mismatched_file() {
    let root = workspace_root();
    let manifest = root.join("tools/ffmpeg/ffmpeg-manifest.json");
    let ffprobe = root.join("tools/ffmpeg/ffprobe.exe");
    if !ffprobe.exists() {
        return; // Nothing to verify against when the operator has not staged them.
    }
    let err = probe_tools::verify_executable(&ffprobe, &manifest, "ffmpeg")
        .expect_err("the ffprobe binary must not satisfy the ffmpeg role");
    assert!(
        matches!(err, ProbeError::ToolDigestMismatch(_)),
        "expected a digest mismatch, got {err:?}"
    );
}

/// The config file's tool paths are repo-relative and resolve.
///
/// Spec 5.1 requires an explicit configured path; a configuration that points at
/// nothing is the same class of failure as no configuration at all.
#[test]
fn configured_tool_paths_resolve_under_the_repository() {
    let root = workspace_root();
    let config = std::fs::read_to_string(root.join("config/default.toml"))
        .expect("config/default.toml must exist");
    let mut section = String::new();
    let mut found = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        if section != "tooling.ffmpeg" || line.starts_with('#') {
            continue;
        }
        if let Some((key, raw)) = line.split_once('=') {
            let value = raw.trim().trim_matches('"');
            if matches!(key.trim(), "executable" | "probe_executable") {
                found.push((key.trim().to_owned(), value.to_owned()));
            }
        }
    }
    assert_eq!(
        found.len(),
        2,
        "expected both tool path keys, got {found:?}"
    );
    for (key, value) in &found {
        assert!(
            !Path::new(value).is_absolute(),
            "{key} = {value} is absolute; the configuration must be repo-relative so \
             the workspace moves between machines"
        );
        let resolved = probe_tools::resolve_from_workspace(Path::new(value));
        assert!(
            resolved.starts_with(&root),
            "{key} resolves outside the workspace: {}",
            resolved.display()
        );
    }
    // The prohibition is stated in the config, not only in prose.
    assert!(
        config.contains("shell_launch = false"),
        "config/default.toml must state that no shell is used to launch tools (spec 9.4)"
    );
}

/// Same findings, different insertion order, same bytes.
///
/// G0 evidence is compared across adapter, driver and compiler changes (spec 9.12).
/// If the report's own bytes were order-dependent, that comparison would be
/// measuring map iteration instead of the platform.
#[test]
fn probe_reports_serialize_deterministically() {
    let context = Diagnostic::error(
        DiagnosticCode::new("gpu.readback_mismatch").expect("well-formed code"),
        "first divergence at element 3",
    )
    .with_context("adapter", "rtx3050a")
    .with_context("driver", "617.14");
    let reordered = Diagnostic::error(
        DiagnosticCode::new("gpu.readback_mismatch").expect("well-formed code"),
        "first divergence at element 3",
    )
    .with_context("driver", "617.14")
    .with_context("adapter", "rtx3050a");

    let a = ProbeReport::new("wp00_gpu", vec![context]);
    let b = ProbeReport::new("wp00_gpu", vec![reordered]);
    assert_eq!(
        a.to_json().expect("serializable"),
        b.to_json().expect("serializable"),
        "report bytes must not depend on context insertion order"
    );
    assert_eq!(a.schema_version, REPORT_SCHEMA_VERSION);
    assert!(
        !a.success,
        "a report carrying an error cannot claim success"
    );
}

/// `success` is derived, so it cannot be talked into agreeing with a failure.
#[test]
fn a_report_cannot_claim_success_while_holding_an_error() {
    let info = Diagnostic::info(
        DiagnosticCode::new("gpu.rgba16f_attachment").expect("code"),
        "available",
    );
    assert!(ProbeReport::new("wp00_gpu", vec![info.clone()]).success);
    assert!(ProbeReport::new("wp00_gpu", vec![]).success);
    let err = Diagnostic::error(
        DiagnosticCode::new("gpu.device_loss").expect("code"),
        "lost",
    );
    assert!(!ProbeReport::new("wp00_gpu", vec![info, err]).success);
}

/// The failure taxonomy is namespaced and total.
///
/// Spec 9.7 requires distinct failure kinds; a code that collides or lacks a
/// namespace makes two different findings look the same in a report.
#[test]
fn every_probe_failure_kind_has_a_unique_namespaced_code() {
    let variants = probe_error_catalog();
    let mut seen = std::collections::BTreeMap::new();
    for e in &variants {
        let code = e.diagnostic_code();
        let text = code.as_str().to_owned();
        assert!(
            text.contains('.') && !text.starts_with('.') && !text.ends_with('.'),
            "diagnostic code {text} is not namespaced as <area>.<condition>"
        );
        if let Some(previous) = seen.insert(text.clone(), format!("{e}")) {
            panic!("codes collide: {text} came from both {previous} and {e}");
        }
    }
    assert!(
        variants.len() >= 20,
        "the catalog covers {n} failure kinds; WP00's probes require more than that",
        n = variants.len()
    );
}

/// The audio fixtures are what the media probe believed them to be.
///
/// Decoded here without spawning FFprobe, so a fixture that was truncated in
/// transit is caught by a test rather than by a probe reading its own output back.
#[test]
fn audio_fixtures_decode_to_the_declared_frame_count() {
    let root = workspace_root();
    let dir = root.join("tests/fixtures/media");
    for (file, extension) in [
        ("tone_44100_stereo_1s.wav", "wav"),
        ("tone_44100_stereo_1s.flac", "flac"),
        ("tone_44100_stereo_1s.oga", "oga"),
    ] {
        let path = dir.join(file);
        if !path.exists() {
            panic!(
                "missing fixture {}; probes do not generate their own inputs",
                path.display()
            );
        }
        let frames = count_decoded_frames(&path, extension)
            .unwrap_or_else(|e| panic!("{file}: decode failed: {e}"));
        assert_eq!(
            frames, 44_100,
            "{file} should hold exactly one second at 44 100 Hz"
        );
    }
}

/// Decode one file to planar f32 and return its frame count.
fn count_decoded_frames(path: &Path, extension: &str) -> Result<u64, ProbeError> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = std::fs::File::open(path).map_err(|e| ProbeError::io("open fixture", path, &e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| ProbeError::Config(format!("{}: probe failed: {e}", path.display())))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or(ProbeError::MissingStream)?;
    let params = track
        .codec_params
        .as_ref()
        .ok_or(ProbeError::MissingStream)?
        .audio()
        .ok_or(ProbeError::MissingStream)?
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| ProbeError::CapabilityAbsent(format!("{}: {e}", params.codec)))?;

    let track_id = track.id;
    let mut frames = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => return Err(ProbeError::Config(format!("packet read: {e}"))),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buffer = decoder
            .decode(&packet)
            .map_err(|e| ProbeError::Config(format!("decode: {e}")))?;
        // Fresh destination per packet, then appended here: the library's planar copy
        // resizes what it is handed, so a shared accumulator would be overwritten and
        // the count would come out short while still looking like a success.
        let mut chunk: Vec<Vec<f32>> = Vec::new();
        buffer.copy_to_vecs_planar(&mut chunk);
        frames += chunk.first().map(|p| p.len() as u64).unwrap_or(0);
    }
    Ok(frames)
}

/// One instance of every `ProbeError` variant. Kept here as well as in the crate's
/// unit tests so the catalog is visible to whoever adds a variant: the integration
/// test fails if a new failure kind arrives without a code.
fn probe_error_catalog() -> Vec<ProbeError> {
    vec![
        ProbeError::NoAdapter,
        ProbeError::AdapterNotFound(3, 2),
        ProbeError::UnsupportedCompute,
        ProbeError::ShaderCompilation("unexpected token".into()),
        ProbeError::DeviceLoss("VK_ERROR_DEVICE_LOST".into()),
        ProbeError::MappingFailure("already mapped".into()),
        ProbeError::ReadbackMismatch("element 3".into()),
        ProbeError::FeatureUnavailable("MAPPABLE_PRIMARY_BUFFERS".into()),
        ProbeError::IdentityUnavailable("deviceLUIDValid false".into()),
        ProbeError::LuidMismatch,
        ProbeError::VendorDeviceMismatch(0x10DE, 0x28A3, 0x8086, 0xA7AB),
        ProbeError::ToolNotConfigured("tools/ffmpeg/ffmpeg.exe".into()),
        ProbeError::ToolDigestMismatch("digest".into()),
        ProbeError::ToolNonzeroExit(1, "Invalid data".into()),
        ProbeError::ToolTimeout("ffprobe".into()),
        ProbeError::ToolSpawn("ffprobe".into(), std::io::Error::other("nope")),
        ProbeError::MalformedJson("expected value".into()),
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
        ProbeError::Config("bad ordinal".into()),
        ProbeError::ReportSerialization("value".into()),
        ProbeError::Leak,
    ]
}

/// The severity model has a maximum, and `success` follows it rather than a count.
#[test]
fn warnings_alone_do_not_fail_a_probe() {
    let warn = Diagnostic::warning(
        DiagnosticCode::new("adapter.budget_unavailable").expect("code"),
        "no telemetry",
    );
    let report = ProbeReport::new("wp00_adapter", vec![warn]);
    assert!(
        report.success,
        "an unsupported optional capability is recorded, not fatal (spec 9.11)"
    );
    assert_eq!(
        Diagnostic::max_severity(&report.diagnostics),
        Some(DiagnosticSeverity::Warning)
    );
}
