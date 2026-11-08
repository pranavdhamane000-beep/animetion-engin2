//! Chunk orchestration and deterministic offline rendering (spec 17.3, 17.5).
//!
//! [`render_offline`] mixes the plan's whole sample range in chunks of `spec.chunk_samples`
//! and concatenates the results. Chunk-size invariance is structural, not measured luck:
//! [`crate::mix::mix_range`] computes every output frame independently from the plan and the
//! canonical sources, with no cross-chunk state, and the resampler's streaming state lives
//! per asset in [`crate::resample`] — never per mix chunk (17.7). So chunk 1, chunk 1,024 and
//! one whole-range chunk produce identical bytes, which 17.12 property-tests.

use crate::mix::mix_range;
use crate::plan::{MixPlan, OfflineAudioSpec, PcmAssetSource};
use crate::time::{SampleOffset, SampleRange};
use crate::{AudioError, EXPORT_CHANNELS};

/// Render the plan's entire output range to interleaved stereo `i16` PCM (spec 17.5).
///
/// The returned vector is `plan.range().len * 2` samples long. The chunk size only
/// partitions the work; it never changes a sample's value.
///
/// # Errors
///
/// [`AudioError::InvalidSpec`] for a zero chunk size; [`AudioError::Overflow`] when the total
/// sample count exceeds the address width; and whatever [`mix_range`] reports for a source
/// or buffer fault.
pub fn render_offline(
    plan: &MixPlan,
    assets: &dyn PcmAssetSource,
    spec: &OfflineAudioSpec,
) -> Result<Vec<i16>, AudioError> {
    if spec.chunk_samples == 0 {
        return Err(AudioError::InvalidSpec {
            detail: "chunk_samples must be nonzero".to_owned(),
        });
    }
    let range = plan.range();
    let total = range
        .len
        .checked_mul(u64::from(EXPORT_CHANNELS))
        .ok_or_else(|| AudioError::Overflow {
            operation: "offline render length",
            value: format!("{} frames * {EXPORT_CHANNELS} channels", range.len),
        })?;
    let total = usize::try_from(total).map_err(|_| AudioError::Overflow {
        operation: "offline render length",
        value: format!("{} frames * {EXPORT_CHANNELS} channels", range.len),
    })?;
    let mut out = vec![0i16; total];

    let chunk = u64::from(spec.chunk_samples);
    let mut offset = 0u64;
    while offset < range.len {
        let len = chunk.min(range.len - offset);
        // range.start + offset < range.end <= u64::MAX, so the chunk start cannot overflow.
        let chunk_range = SampleRange::new(SampleOffset::new(range.start.0 + offset), len);
        let start = offset as usize * EXPORT_CHANNELS as usize;
        let end = start + len as usize * EXPORT_CHANNELS as usize;
        mix_range(plan, assets, chunk_range, &mut out[start..end])?;
        offset += len;
    }
    Ok(out)
}
