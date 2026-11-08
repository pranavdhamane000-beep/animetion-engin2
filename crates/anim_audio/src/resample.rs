//! Locked rubato configuration and canonical 48 kHz conversion (spec 17.3, 17.7).
//!
//! DECISIONS.md #46 ruling 4 forces the **scalar** sinc interpolator. rubato 5.x otherwise
//! dispatches at runtime across AVX+FMA -> SSE -> scalar (`asynchro_sinc::make_interpolator`),
//! and FMA's fused rounding differs from separate mul+add, so a different CPU could produce
//! different PCM bits. `Async::new_with_sinc_interpolator` with a hand-built
//! `ScalarInterpolator` (both public only under the `bench_asyncro` feature) removes the
//! dispatch entirely: the whole decode -> resample -> mix path is then bit-exact on every
//! CPU, exceeding the spec's pinned-stack minimum deliberately. Offline export is not
//! real-time, so the scalar speed cost is irrelevant.
//!
//! Every coefficient-setting below is a locked, versioned constant. A change to any of them
//! is a [`RESAMPLER_ALGORITHM_VERSION`] bump, not a quiet edit, and the settings are folded
//! into [`resampler_settings_hash`] so a derived cache keyed on `(content_hash,
//! decoder_version, resampler_version, settings_hash)` (17.7) cannot serve yesterday's
//! coefficients. The automatic cutoff is resolved exactly as rubato's own `new_sinc` does
//! (`calculate_cutoff` against the rounded sinc length, scaled by the ratio when
//! downsampling) — that helper is `pub(crate)`, so the two-line resolution is reproduced
//! here against the public `calculate_cutoff` rather than guessed at 0.95.
//!
//! A source already at 48 kHz is passed through with **no** resampler: a ratio-1.0 sinc is
//! not a bit-exact identity (it filters), so running one would change bytes that need no
//! conversion.

use anim_assets::ContentHash;
use anim_core::hash::CanonicalBytes;
use rubato::audioadapter_buffers::owned::InterleavedOwned;
use rubato::sinc_interpolator::{AnyInterpolator, ScalarInterpolator};
use rubato::{
    calculate_cutoff, Async, FixedAsync, Resampler, SincInterpolationType, WindowFunction,
};

use crate::{AudioError, CanonicalPcm, NativePcm, EXPORT_SAMPLE_RATE, RESAMPLER_ALGORITHM_VERSION};

/// Locked sinc filter length in taps. A multiple of 8 (the scalar interpolator asserts it).
pub const RESAMPLER_SINC_LEN: usize = 128;
/// Locked oversampling factor: the number of intermediate sincs between taps.
pub const RESAMPLER_OVERSAMPLING_FACTOR: usize = 128;
/// Locked window function shaping the sinc.
pub const RESAMPLER_WINDOW: WindowFunction = WindowFunction::BlackmanHarris2;
/// Locked interpolation between oversampled sincs.
pub const RESAMPLER_INTERPOLATION: SincInterpolationType = SincInterpolationType::Cubic;
/// Locked input chunk size in frames. The streaming state is private to one asset's
/// resample session and independent of the output mix chunk size (17.7).
pub const RESAMPLER_CHUNK_SIZE: usize = 1024;

/// The domain separator for the resampler settings digest.
const RESAMPLER_SETTINGS_DOMAIN: &str = "anim_engine/audio_resampler/v1";

/// The canonical digest of the locked resampler settings (spec 17.7's `settings_hash`).
///
/// Infallible: the domain is a validated compile-time constant, applied as the first
/// length-prefixed text field exactly as [`CanonicalBytes::with_domain`] would (reproduced
/// with [`CanonicalBytes::text`] because `with_domain` returns a `Result` for a condition a
/// constant cannot meet, and production forbids `unwrap`/`expect`). The pre-image is the
/// domain, the algorithm version, then each locked setting in declaration order, so a
/// changed coefficient moves the digest and a stale cache entry is refused.
#[must_use]
pub fn resampler_settings_hash() -> ContentHash {
    let mut cp = CanonicalBytes::new();
    cp.text(RESAMPLER_SETTINGS_DOMAIN);
    cp.u32(RESAMPLER_ALGORITHM_VERSION);
    cp.u64(RESAMPLER_SINC_LEN as u64);
    cp.u64(RESAMPLER_OVERSAMPLING_FACTOR as u64);
    cp.text(window_name(RESAMPLER_WINDOW));
    cp.text(interpolation_name(RESAMPLER_INTERPOLATION));
    cp.u64(RESAMPLER_CHUNK_SIZE as u64);
    // The cutoff is automatic (derived from sinc_len + window + ratio), so the *rule* is
    // what is locked, not a value that depends on the source rate.
    cp.text("auto-cutoff");
    ContentHash::from_bytes(cp.digest())
}

/// The canonical spelling of a window function, for the settings digest.
const fn window_name(window: WindowFunction) -> &'static str {
    match window {
        WindowFunction::Blackman => "blackman",
        WindowFunction::Blackman2 => "blackman2",
        WindowFunction::BlackmanHarris => "blackman_harris",
        WindowFunction::BlackmanHarris2 => "blackman_harris2",
        WindowFunction::Hann => "hann",
        WindowFunction::Hann2 => "hann2",
    }
}

/// The canonical spelling of an interpolation type, for the settings digest.
const fn interpolation_name(interpolation: SincInterpolationType) -> &'static str {
    match interpolation {
        SincInterpolationType::Cubic => "cubic",
        SincInterpolationType::Quadratic => "quadratic",
        SincInterpolationType::Linear => "linear",
        SincInterpolationType::Nearest => "nearest",
    }
}

/// Resample one decoded source to canonical 48 kHz interleaved `f32` (spec 17.7).
///
/// `expected_hash` is the content revision the resolver verified; it becomes the canonical
/// PCM's revision metadata and names the asset in any error (the resolver enriches it to the
/// logical id). The output frame count is the **exact integer** `ceil(frames * 48000 / rate)`.
///
/// # Errors
///
/// [`AudioError::UnsupportedLayout`] for a channel count other than 1 or 2;
/// [`AudioError::ResampleFailure`] for a zero rate, a buffer whose length disagrees with its
/// frame count, a rubato construction/processing error, or an output count that disagrees
/// with the exact expectation; [`AudioError::Overflow`] when a count exceeds its width.
pub fn resample_to_48k(
    native: NativePcm,
    expected_hash: ContentHash,
) -> Result<CanonicalPcm, AudioError> {
    let fail = |detail: String| AudioError::ResampleFailure {
        asset_id: expected_hash.to_hex(),
        detail,
    };

    if native.rate == 0 {
        return Err(fail("zero native sample rate".to_owned()));
    }
    if native.channels != 1 && native.channels != 2 {
        return Err(AudioError::UnsupportedLayout {
            channels: native.channels,
        });
    }
    let channels = usize::from(native.channels);
    let expected_len = (native.frames as usize)
        .checked_mul(channels)
        .ok_or_else(|| AudioError::Overflow {
            operation: "native sample count",
            value: format!("{} frames * {channels} channels", native.frames),
        })?;
    if native.samples.len() != expected_len {
        return Err(fail(format!(
            "native buffer holds {} samples, expected {expected_len}",
            native.samples.len()
        )));
    }

    // An empty source has nothing to resample; return it canonical so a looping clip over
    // it is refused by the planner (InvalidLoop) rather than dividing by zero here.
    if native.frames == 0 {
        return CanonicalPcm::new(expected_hash, native.channels, 0, Vec::new());
    }
    // Already canonical: pass through untouched. A ratio-1.0 sinc filters rather than
    // identities, so running one would change bytes that need no conversion.
    if native.rate == EXPORT_SAMPLE_RATE {
        return CanonicalPcm::new(
            expected_hash,
            native.channels,
            native.frames,
            native.samples,
        );
    }

    let ratio = f64::from(EXPORT_SAMPLE_RATE) / f64::from(native.rate);
    // Reproduce rubato's pub(crate) `resolve_cutoff`: the automatic cutoff is computed
    // against the rounded sinc length (128 is already a multiple of 8), then scaled by the
    // ratio when downsampling so it stays below the output Nyquist frequency.
    let base_cutoff: f32 = calculate_cutoff(RESAMPLER_SINC_LEN, RESAMPLER_WINDOW);
    let cutoff = if ratio >= 1.0 {
        base_cutoff
    } else {
        base_cutoff * ratio as f32
    };
    let interpolator = ScalarInterpolator::<f32>::new(
        RESAMPLER_SINC_LEN,
        RESAMPLER_OVERSAMPLING_FACTOR,
        cutoff,
        RESAMPLER_WINDOW,
    );
    let mut resampler = Async::<f32>::new_with_sinc_interpolator(
        ratio,
        1.0,
        RESAMPLER_INTERPOLATION,
        AnyInterpolator::Scalar(interpolator),
        cutoff,
        RESAMPLER_CHUNK_SIZE,
        channels,
        FixedAsync::Input,
    )
    .map_err(|e| fail(format!("construct resampler: {e}")))?;

    let input = InterleavedOwned::new_from(native.samples, channels, native.frames as usize)
        .map_err(|e| fail(format!("input buffer: {e}")))?;
    let output = resampler
        .process_all(&input, native.frames as usize, None)
        .map_err(|e| fail(format!("process: {e}")))?;
    let mut samples = output.take_data();

    if samples.len() % channels != 0 {
        return Err(fail(format!(
            "output {} samples is not a whole number of {channels}-channel frames",
            samples.len()
        )));
    }
    let produced = (samples.len() / channels) as u64;

    // The exact integer expectation, computed without floating point: ceil(n*48000/rate)
    // as floor plus a remainder term, so neither the product nor the ceiling can overflow.
    let product = native
        .frames
        .checked_mul(u64::from(EXPORT_SAMPLE_RATE))
        .ok_or_else(|| AudioError::Overflow {
            operation: "resample output count",
            value: format!("{} * {EXPORT_SAMPLE_RATE}", native.frames),
        })?;
    let rate = u64::from(native.rate);
    let exact = product / rate + u64::from(product % rate != 0);

    if produced == exact {
        // The common case: rubato's count agrees with the exact integer ceiling.
    } else if produced.saturating_sub(exact) == 1 && product % rate == 0 {
        // At an exact rational boundary (n*48000 divisible by rate) rubato's own
        // `ceil(ratio_f64 * n)` can round one frame above the exact ceiling, because the
        // f64 ratio is a rounded approximation of 48000/rate. The extra frame is the
        // interpolation evaluated strictly past the input's temporal extent — a boundary
        // artifact, not signal. Trim to the exact count so the canonical length is the
        // mathematically exact one and stays portable (the decision is integer arithmetic,
        // not a float comparison). This reconciliation is part of RESAMPLER_ALGORITHM_VERSION.
        samples.truncate(exact as usize * channels);
    } else {
        return Err(fail(format!(
            "resampler produced {produced} frames, exact expectation {exact}"
        )));
    }

    CanonicalPcm::new(expected_hash, native.channels, exact, samples)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    fn hash(tag: u8) -> ContentHash {
        ContentHash::from_bytes([tag; 32])
    }

    #[test]
    fn settings_hash_is_stable_and_matches_the_hand_built_pre_image() {
        let h = resampler_settings_hash();
        assert_eq!(h, resampler_settings_hash());
        // Rebuilt from the layout rule, not read off the encoder (risk 2: an oracle that
        // calls the code under test proves only self-consistency).
        let mut cp = CanonicalBytes::new();
        cp.text(RESAMPLER_SETTINGS_DOMAIN);
        cp.u32(RESAMPLER_ALGORITHM_VERSION);
        cp.u64(128);
        cp.u64(128);
        cp.text("blackman_harris2");
        cp.text("cubic");
        cp.u64(1024);
        cp.text("auto-cutoff");
        assert_eq!(h, ContentHash::from_bytes(cp.digest()));
    }

    #[test]
    fn a_48k_source_passes_through_unchanged() {
        let samples = vec![0.25, -0.25, 0.5, -0.5];
        let native = NativePcm {
            rate: 48_000,
            channels: 2,
            frames: 2,
            samples: samples.clone(),
        };
        let out = resample_to_48k(native, hash(1)).unwrap();
        assert_eq!(out.frames(), 2);
        assert_eq!(out.channels(), 2);
        assert_eq!(out.samples(), samples.as_slice());
        assert_eq!(out.content_hash(), hash(1));
    }

    #[test]
    fn an_empty_source_stays_empty() {
        let native = NativePcm {
            rate: 44_100,
            channels: 1,
            frames: 0,
            samples: Vec::new(),
        };
        let out = resample_to_48k(native, hash(2)).unwrap();
        assert_eq!(out.frames(), 0);
        assert!(out.samples().is_empty());
    }

    #[test]
    fn upsample_count_is_the_exact_integer_ceiling() {
        // 441 frames at 44100 Hz is exactly 480 frames at 48 kHz (441*48000/44100 = 480).
        // This is an exact rational boundary where rubato's f64 ceil can round up; the
        // reconciliation must still report 480.
        let frames = 441u64;
        let native = NativePcm {
            rate: 44_100,
            channels: 1,
            frames,
            samples: vec![0.0; frames as usize],
        };
        let out = resample_to_48k(native, hash(3)).unwrap();
        assert_eq!(out.frames(), 480);
        assert_eq!(out.samples().len(), 480);
    }

    #[test]
    fn a_non_boundary_upsample_count_is_ceiled() {
        // 100 frames at 44100 -> 100*48000/44100 = 108.84... -> ceil 109.
        let native = NativePcm {
            rate: 44_100,
            channels: 2,
            frames: 100,
            samples: vec![0.1; 200],
        };
        let out = resample_to_48k(native, hash(4)).unwrap();
        assert_eq!(out.frames(), 109);
        assert_eq!(out.samples().len(), 218);
    }

    #[test]
    fn downsample_count_is_exact() {
        // 960 frames at 96000 -> exactly 480 at 48 kHz.
        let native = NativePcm {
            rate: 96_000,
            channels: 1,
            frames: 960,
            samples: vec![0.0; 960],
        };
        let out = resample_to_48k(native, hash(5)).unwrap();
        assert_eq!(out.frames(), 480);
    }

    #[test]
    fn resampling_a_known_ramp_is_deterministic() {
        let native = NativePcm {
            rate: 44_100,
            channels: 1,
            frames: 512,
            samples: (0..512).map(|i| (i as f32) / 512.0).collect(),
        };
        let a = resample_to_48k(native.clone(), hash(6)).unwrap();
        let b = resample_to_48k(native, hash(6)).unwrap();
        assert_eq!(a.samples(), b.samples());
        assert_eq!(a.frames(), b.frames());
    }

    #[test]
    fn unsupported_layout_and_bad_length_are_refused() {
        let native = NativePcm {
            rate: 44_100,
            channels: 3,
            frames: 1,
            samples: vec![0.0; 3],
        };
        assert!(matches!(
            resample_to_48k(native, hash(7)),
            Err(AudioError::UnsupportedLayout { channels: 3 })
        ));
        let short = NativePcm {
            rate: 44_100,
            channels: 2,
            frames: 4,
            samples: vec![0.0; 3],
        };
        assert!(matches!(
            resample_to_48k(short, hash(8)),
            Err(AudioError::ResampleFailure { .. })
        ));
        let zero_rate = NativePcm {
            rate: 0,
            channels: 1,
            frames: 0,
            samples: Vec::new(),
        };
        assert!(matches!(
            resample_to_48k(zero_rate, hash(9)),
            Err(AudioError::ResampleFailure { .. })
        ));
    }
}
