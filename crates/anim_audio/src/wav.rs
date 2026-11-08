//! Canonical little-endian PCM WAV writing and counting (spec 17.3, 17.9).
//!
//! [`write_wav`] mixes the requested range chunk by chunk through [`crate::mix::mix_range`]
//! and writes signed 16-bit little-endian stereo at 48 kHz with `hound`, the crate spec 5.1
//! pins for canonical WAV I/O. It returns the number of sample **frames**, not interleaved
//! scalar values (17.9) — the count WP09's export verification compares against WP01's
//! frame-range expectation. The writer is finalized so the RIFF sizes are correct; a failure
//! aborts before publication rather than leaving a partial WAV presented as complete (17.11).

use std::io::{Seek, Write};

use hound::{SampleFormat, WavSpec, WavWriter};

use crate::mix::mix_range;
use crate::plan::{MixPlan, PcmAssetSource};
use crate::time::{SampleOffset, SampleRange};
use crate::{AudioError, EXPORT_CHANNELS, EXPORT_SAMPLE_RATE};

/// Write one sample range as a canonical 48 kHz stereo 16-bit WAV (spec 17.9).
///
/// Returns `range.len`, the number of sample frames written. The chunk size comes from the
/// plan's validated spec and, by [`crate::offline`]'s invariance, does not affect the bytes.
///
/// # Errors
///
/// [`AudioError::OutputFailure`] when the WAV header, a sample write or the finalization
/// fails (including an underlying writer IO error); whatever [`mix_range`] reports for a
/// source or buffer fault.
pub fn write_wav<W: Write + Seek>(
    plan: &MixPlan,
    assets: &dyn PcmAssetSource,
    range: SampleRange,
    writer: W,
) -> Result<u64, AudioError> {
    // A public `MixPlan::new` can carry an unvalidated spec, and a zero chunk would make the
    // loop below advance by zero forever. `compile_plan` validates this for the offline path;
    // guard it here too so a hand-built plan reports rather than hangs (the same rule
    // `render_offline` applies to its spec).
    if plan.spec().chunk_samples == 0 {
        return Err(AudioError::InvalidSpec {
            detail: "chunk_samples must be nonzero".to_owned(),
        });
    }
    let spec = WavSpec {
        channels: EXPORT_CHANNELS,
        sample_rate: EXPORT_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut wav = WavWriter::new(writer, spec).map_err(|e| AudioError::OutputFailure {
        detail: format!("write WAV header: {e}"),
    })?;

    // The plan's chunk size is validated nonzero at compile time. Reuse one buffer across
    // chunks; mixing is per-sample independent, so the partition is invisible in the bytes.
    let chunk = u64::from(plan.spec().chunk_samples);
    let mut buf = vec![0i16; chunk as usize * EXPORT_CHANNELS as usize];
    let mut offset = 0u64;
    while offset < range.len {
        let len = chunk.min(range.len - offset);
        let chunk_range = SampleRange::new(SampleOffset::new(range.start.0 + offset), len);
        let needed = len as usize * EXPORT_CHANNELS as usize;
        mix_range(plan, assets, chunk_range, &mut buf[..needed])?;
        for &sample in &buf[..needed] {
            wav.write_sample(sample)
                .map_err(|e| AudioError::OutputFailure {
                    detail: format!("write WAV sample: {e}"),
                })?;
        }
        offset += len;
    }

    wav.finalize().map_err(|e| AudioError::OutputFailure {
        detail: format!("finalize WAV: {e}"),
    })?;
    Ok(range.len)
}
