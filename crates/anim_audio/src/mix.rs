//! Ordered mixing, gain, fades, looping and PCM conversion (spec 17.3, 17.7).
//!
//! This is the determinism heart of the offline path. For every output sample it processes
//! the intersecting planned clips in **canonical order** (the plan's `(start, clip_id)`
//! sort), accumulates in `f64`, clamps to `[-1, 1]` and rounds **once** to signed 16-bit
//! with a symmetric rule. There is no cross-chunk state, no dither and no adaptive
//! normalization, which is what makes [`crate::offline::render_offline`] chunk-size
//! invariant and a repeated run byte-identical (17.12).
//!
//! The rounding rule is versioned under [`crate::MIX_ALGORITHM_VERSION`]:
//! `(acc.clamp(-1, 1) * 32767.0).round() as i16` — round half away from zero, symmetric
//! `±32767` (never the asymmetric `−32768`), one rounding at the very end. `f64` appears
//! here and only here: authored gains arrive already converted once by the planner, source
//! samples are `f32` widened to `f64`, and every placement decision upstream was an integer.

use crate::plan::{MixPlan, PcmAssetSource, PlannedClip};
use crate::time::SampleRange;
use crate::{AudioError, CanonicalPcm, EXPORT_CHANNELS};

/// The symmetric full-scale magnitude: round to `±32767`, not `−32768`, so positive and
/// negative full scale are mirror images (spec 17.7's "specified symmetric rule").
const FULL_SCALE: f64 = 32_767.0;

/// Mix one sample range into an interleaved stereo `i16` buffer (spec 17.9).
///
/// `out` must hold exactly `range.len * 2` samples. Each output frame is computed
/// independently from the plan and the canonical sources, so splitting the render into
/// chunks and concatenating gives the same bytes as mixing the whole range at once.
///
/// # Errors
///
/// [`AudioError::InvalidSpec`] when `out` is not `range.len * 2` long; [`AudioError::Overflow`]
/// when that product exceeds the width; [`AudioError::UnsupportedLayout`] when a source's
/// canonical channel count is not 1 or 2; [`AudioError::ChangedAsset`] when a source's
/// canonical length disagrees with the length the plan was compiled against (which would
/// otherwise index out of bounds); and whatever the [`PcmAssetSource`] reports for a source
/// it cannot supply.
pub fn mix_range(
    plan: &MixPlan,
    assets: &dyn PcmAssetSource,
    range: SampleRange,
    out: &mut [i16],
) -> Result<(), AudioError> {
    let expected_len = range
        .len
        .checked_mul(u64::from(EXPORT_CHANNELS))
        .ok_or_else(|| AudioError::Overflow {
            operation: "mix output length",
            value: format!("{} frames * {EXPORT_CHANNELS} channels", range.len),
        })?;
    if out.len() as u64 != expected_len {
        return Err(AudioError::InvalidSpec {
            detail: format!(
                "mix output buffer holds {} samples, expected {expected_len} ({} frames * {EXPORT_CHANNELS})",
                out.len(),
                range.len
            ),
        });
    }

    let range_start = range.start.0;
    let range_end = range.end()?;

    // Load each intersecting clip's canonical PCM once, outside the per-sample loop, in the
    // plan's canonical order. The accumulator iterates this vector, so the f64 addition
    // order is the plan order on every run — f64 addition is not associative, so this order
    // is load-bearing for bit-exactness, not merely tidy.
    let mut active: Vec<(&PlannedClip, CanonicalPcm, u64)> = Vec::new();
    for clip in plan.clips() {
        let clip_start = clip.timeline.start.0;
        let clip_end =
            clip_start
                .checked_add(clip.timeline.len)
                .ok_or_else(|| AudioError::Overflow {
                    operation: "clip timeline end",
                    value: format!("{clip_start} + {}", clip.timeline.len),
                })?;
        if clip_end <= range_start || clip_start >= range_end {
            continue;
        }
        let pcm = assets.canonical_pcm(clip.asset_id)?;
        let channels = pcm.channels();
        if channels != 1 && channels != 2 {
            return Err(AudioError::UnsupportedLayout { channels });
        }
        // The plan's source_len is the length it was compiled against; a source that now
        // decodes to a different length would index out of bounds below. Refuse it as a
        // changed asset rather than panicking or silently substituting silence (17.11).
        if pcm.frames() != clip.source_len {
            return Err(AudioError::ChangedAsset {
                asset_id: clip.asset_id.to_canonical_string(),
                detail: format!(
                    "canonical length {} at mix time, plan compiled against {}",
                    pcm.frames(),
                    clip.source_len
                ),
            });
        }
        active.push((clip, pcm, clip_end));
    }

    for i in 0..range.len {
        // absolute < range_end <= u64::MAX because i < range.len and range_end is the
        // checked start + len, so this addition cannot overflow.
        let absolute = range_start + i;
        let mut acc = [0.0f64; 2];

        for (clip, pcm, clip_end) in &active {
            let clip_start = clip.timeline.start.0;
            let clip_len = clip.timeline.len;
            if absolute < clip_start || absolute >= *clip_end {
                continue;
            }
            let pos = absolute - clip_start;

            // Source mapping: source_start + (output - timeline_start), checked modulo for
            // a loop, silence for an exhausted nonlooping source (17.7). The addition is
            // checked so a hand-built plan with an extreme offset reports overflow rather
            // than wrapping.
            let mut source_pos =
                clip.source_start
                    .0
                    .checked_add(pos)
                    .ok_or_else(|| AudioError::Overflow {
                        operation: "mix source position",
                        value: format!("{} + {pos}", clip.source_start.0),
                    })?;
            if clip.looped {
                // compile_plan validates a nonzero loop modulus (InvalidLoop); guard it here
                // too so a hand-built plan can never divide by zero.
                if clip.source_len == 0 {
                    return Err(AudioError::InvalidLoop {
                        clip_id: clip.clip_id.to_canonical_string(),
                        source_len: 0,
                    });
                }
                source_pos %= clip.source_len;
            } else if source_pos >= clip.source_len {
                continue;
            }

            // Linear, sample-indexed fades. The fade-out threshold uses saturating_sub so a
            // fade longer than the clip does not underflow; the factor is clamped to [0, 1].
            let mut fade = 1.0f64;
            if clip.fade_in_samples > 0 && pos < clip.fade_in_samples {
                fade *= pos as f64 / clip.fade_in_samples as f64;
            }
            if clip.fade_out_samples > 0 && pos >= clip_len.saturating_sub(clip.fade_out_samples) {
                fade *= (clip_len - pos) as f64 / clip.fade_out_samples as f64;
            }
            let total_gain = clip.gain * fade.clamp(0.0, 1.0);

            // Channel conversion driven by the decoded count: mono up-mixed to both channels
            // with the declared gain, stereo preserved (17.7). source_pos < source_len ==
            // pcm.frames(), so these indices are in bounds by the checked invariant above.
            let samples = pcm.samples();
            let idx = source_pos as usize;
            if pcm.channels() == 1 {
                let s = f64::from(samples[idx]);
                acc[0] += s * total_gain;
                acc[1] += s * total_gain;
            } else {
                let left = f64::from(samples[idx * 2]);
                let right = f64::from(samples[idx * 2 + 1]);
                acc[0] += left * total_gain;
                acc[1] += right * total_gain;
            }
        }

        // Clamp then round once, symmetric ±32767, no dither, no normalization.
        let out_idx = i as usize * 2;
        out[out_idx] = (acc[0].clamp(-1.0, 1.0) * FULL_SCALE).round() as i16;
        out[out_idx + 1] = (acc[1].clamp(-1.0, 1.0) * FULL_SCALE).round() as i16;
    }
    Ok(())
}
