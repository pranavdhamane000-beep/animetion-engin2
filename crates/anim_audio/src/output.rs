//! The cpal output stream and the preallocated lock-free output ring (spec 17.3, 17.5,
//! 17.6, 17.8, 17.11).
//!
//! [`PreviewOutput`] opens one device stream and owns one feeder thread. The callback is
//! the only real-time code: it pops pre-mixed `i16` frames from a `ringbuf` SPSC ring,
//! emits silence on underrun, increments the device-sample counter and reports underrun
//! epochs and device errors as atomic increments — it never allocates, blocks on disk/GPU
//! work, enters the clock seqlock or takes a project lock (17.6). The feeder thread mixes
//! [`crate::preview::mix_preview_block`] blocks from preloaded canonical PCM ahead of the
//! callback and promotes the callback's reports into clock generations (DECISIONS.md #48
//! ruling 3, #49).
//!
//! Ring invalidation is a handshake: on a generation change the callback clears the ring
//! and **acknowledges** the generation; the feeder pushes only for an acknowledged
//! generation. Without the ack, a clear could wipe data the feeder had already mixed for
//! the new position. Silence between the ack and the feeder's first push is startup
//! latency, not an underrun — the `pushed_generation` gate keeps it from being reported as
//! one. One residual race is accepted and bounded: a block whose generation check passed
//! microseconds before a transport bump can land in the ring just after the clear, giving
//! at most one block (~21 ms) of old-position audio; the generation bump still reaches
//! video immediately, which is what 17.6 requires.
//!
//! The stream requests canonical 48 kHz stereo `i16`. WASAPI output streams carry
//! `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, so the audio engine accepts the canonical format
//! regardless of the device's mix format; a device with no 48 kHz stereo output
//! configuration is a typed refusal, never a silent resample (spec 9.11's spirit, 17.11).

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::plan::{MixPlan, PcmAssetSource};
use crate::preview::{mix_preview_block, ClockState, PreloadedSources, PreviewClock};
use crate::time::SampleOffset;
use crate::{AudioError, PreviewClockSnapshot, EXPORT_CHANNELS, EXPORT_SAMPLE_RATE};

/// The preallocated ring depth in sample frames (~171 ms at 48 kHz). DECISIONS.md #48
/// ruling 3: deep enough that an editor stall does not starve the device.
pub const PREVIEW_RING_FRAMES: usize = 8_192;

/// The feeder's mix block in sample frames — the offline default chunk size.
pub const PREVIEW_BLOCK_FRAMES: usize = 1_024;

/// The feeder's idle wait. Bounded per iteration; `clippy.toml` anticipates preview sleep
/// loops. The loop exits on the shutdown flag, so drop joins within one wait.
const FEEDER_IDLE: Duration = Duration::from_millis(1);

/// A live preview output: one device stream, one ring, one feeder thread, one clock.
///
/// Drop signals the feeder, joins it, and then drops the stream (cpal stops and joins its
/// callback thread), so callback data after destruction is ignored by ownership (17.7)
/// and the stream and ring are gone on shutdown or device replacement (17.8).
pub struct PreviewOutput {
    clock: PreviewClock,
    state: Arc<ClockState>,
    #[allow(
        dead_code,
        reason = "held for ownership: dropping the stream stops and joins the cpal callback thread (17.7/17.8)"
    )]
    stream: cpal::Stream,
    feeder: Option<JoinHandle<()>>,
    device_name: String,
    device_id: String,
}

impl PreviewOutput {
    /// Open preview on a caller-chosen device (DECISIONS.md #48 ruling 1; the editor owns
    /// selection, 17.10). The plan's sources are preloaded and validated before the device
    /// is touched, so a missing source fails before any stream exists (17.7).
    ///
    /// The transport starts **paused** at timeline sample 0; call
    /// [`PreviewOutput::set_playing`] to start. The cpal stream itself runs from open (its
    /// callback emits silence while the transport is paused), which keeps the device-sample
    /// counter honest from the first moment.
    ///
    /// # Errors
    ///
    /// Whatever the [`PcmAssetSource`] reports for a missing/changed/unsupported source;
    /// [`AudioError::DeviceLoss`] if the device cannot be identified;
    /// [`AudioError::OutputStreamFailure`] if the device offers no canonical 48 kHz stereo
    /// `i16` output configuration, or the stream cannot be built, started or fed.
    pub fn open(
        device: &cpal::Device,
        plan: MixPlan,
        sources: &dyn PcmAssetSource,
    ) -> Result<Self, AudioError> {
        let preloaded = Arc::new(PreloadedSources::load(&plan, sources)?);
        let plan = Arc::new(plan);

        let description = device.description().map_err(|e| AudioError::DeviceLoss {
            detail: format!("device description unavailable: {e}"),
        })?;
        let device_name = description.name().to_owned();
        let device_id = device
            .id()
            .map_err(|e| AudioError::DeviceLoss {
                detail: format!("device id unavailable: {e}"),
            })?
            .to_string();

        let config = canonical_output_config(device).ok_or_else(|| {
            AudioError::OutputStreamFailure {
                detail: format!(
                    "device {device_name} offers no {EXPORT_SAMPLE_RATE} Hz stereo I16 output configuration"
                ),
            }
        })?;

        let ring = HeapRb::<i16>::new(PREVIEW_RING_FRAMES * EXPORT_CHANNELS as usize);
        let (prod, cons) = ring.split();

        let state = Arc::new(ClockState::new());
        let stream = build_stream(device, &state, cons, &device_name, config)?;

        let feeder_state = Arc::clone(&state);
        let feeder_plan = Arc::clone(&plan);
        let feeder_sources = Arc::clone(&preloaded);
        let feeder = std::thread::Builder::new()
            .name("anim-audio-preview-feeder".to_owned())
            .spawn(move || feeder_loop(feeder_state, feeder_plan, feeder_sources, prod))
            .map_err(|e| AudioError::OutputStreamFailure {
                detail: format!("spawning the preview feeder failed: {e}"),
            })?;

        if let Err(e) = stream.play() {
            state.signal_shutdown();
            let _ = feeder.join();
            return Err(AudioError::OutputStreamFailure {
                detail: format!("starting the output stream on {device_name} failed: {e}"),
            });
        }

        Ok(Self {
            clock: PreviewClock::new(Arc::clone(&state)),
            state,
            stream,
            feeder: Some(feeder),
            device_name,
            device_id,
        })
    }

    /// Open preview on the host's default output device — the convenience over
    /// [`PreviewOutput::open`] (DECISIONS.md #48 ruling 1).
    ///
    /// # Errors
    ///
    /// [`AudioError::DeviceLoss`] when no default output device exists, plus everything
    /// [`PreviewOutput::open`] reports.
    pub fn open_default(plan: MixPlan, sources: &dyn PcmAssetSource) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AudioError::DeviceLoss {
                detail: "no default output device available".to_owned(),
            })?;
        Self::open(&device, plan, sources)
    }

    /// An immutable, consistent reading of the preview clock (spec 17.9).
    #[must_use]
    pub fn preview_clock_snapshot(&self) -> PreviewClockSnapshot {
        self.clock.preview_clock_snapshot()
    }

    /// Replace the anchor and bump the generation (spec 17.9). Editing stays available
    /// even after a device error (17.11).
    ///
    /// # Errors
    ///
    /// [`AudioError::Overflow`] when the position exceeds the anchor's `i64` width.
    pub fn seek(&mut self, position: SampleOffset) -> Result<(), AudioError> {
        self.clock.seek(position)
    }

    /// Start or freeze the transport (spec 17.9); both are generation events.
    ///
    /// # Errors
    ///
    /// [`AudioError::DeviceLoss`] when resuming after a device error — reopen instead.
    pub fn set_playing(&mut self, playing: bool) -> Result<(), AudioError> {
        self.clock.set_playing(playing)
    }

    /// Set the scrub rate (DECISIONS.md #48 ruling 2): audio is silent away from 1.0.
    ///
    /// # Errors
    ///
    /// [`AudioError::InvalidRate`] for a non-finite or non-positive rate.
    pub fn set_rate(&mut self, rate: f64) -> Result<(), AudioError> {
        self.clock.set_rate(rate)
    }

    /// The opened device's human-readable name.
    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// The opened device's stable cpal identity (`host:id`).
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The device error that stopped audio, if one occurred (spec 17.11). The snapshot
    /// also shows it: `playing == false` with `discontinuity == true`. Recovery is an
    /// explicit reopen on a working device.
    #[must_use]
    pub fn device_error(&self) -> Option<AudioError> {
        if self.state.error_epochs() == 0 {
            return None;
        }
        Some(AudioError::DeviceLoss {
            detail: kind_from_code(self.state.error_kind()).to_string(),
        })
    }
}

impl Drop for PreviewOutput {
    fn drop(&mut self) {
        self.state.signal_shutdown();
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.join();
        }
        // The stream field drops next: cpal stops and joins its callback thread, so no
        // callback runs against a destroyed output (17.7's ownership clause).
    }
}

/// Find a supported output configuration carrying canonical 48 kHz stereo `i16`.
fn canonical_output_config(device: &cpal::Device) -> Option<cpal::StreamConfig> {
    let configs = device.supported_output_configs().ok()?;
    let rate = EXPORT_SAMPLE_RATE;
    configs
        .filter(|c| {
            c.channels() == EXPORT_CHANNELS
                && c.min_sample_rate() <= rate
                && c.max_sample_rate() >= rate
                && c.sample_format() == cpal::SampleFormat::I16
        })
        .map(|_| cpal::StreamConfig {
            channels: EXPORT_CHANNELS,
            sample_rate: rate,
            buffer_size: cpal::BufferSize::Default,
        })
        .next()
}

fn build_stream(
    device: &cpal::Device,
    state: &Arc<ClockState>,
    cons: HeapCons<i16>,
    device_name: &str,
    config: cpal::StreamConfig,
) -> Result<cpal::Stream, AudioError> {
    let data_state = Arc::clone(state);
    let mut cons = cons;
    let mut cb = CallbackClock {
        generation: 0,
        underrun_episode: false,
    };
    let data_callback = move |out: &mut [i16], _info: &cpal::OutputCallbackInfo| {
        fill_output(out, &data_state, &mut cons, &mut cb);
    };
    let err_state = Arc::clone(state);
    let error_callback = move |err: cpal::Error| {
        // Atomic report only; the feeder promotes it to a stopped, bumped clock (17.11).
        err_state.note_device_error(kind_code(err.kind()));
    };
    device
        .build_output_stream::<i16, _, _>(config, data_callback, error_callback, None)
        .map_err(|e| AudioError::OutputStreamFailure {
            detail: format!("building the output stream on {device_name} failed: {e}"),
        })
}

/// The callback's local, single-threaded state. Lives inside the data closure.
struct CallbackClock {
    generation: u64,
    underrun_episode: bool,
}

/// The real-time callback (spec 17.6): pop pre-mixed frames, silence on underrun, count
/// device samples. Allocation-free, lock-free, bounded; the only writes are atomic
/// counter increments.
fn fill_output(
    out: &mut [i16],
    state: &ClockState,
    cons: &mut HeapCons<i16>,
    cb: &mut CallbackClock,
) {
    let generation = state.generation();
    if generation != cb.generation {
        // A generation change invalidates everything mixed for the old position. Clearing
        // here (consumer side) is the only place stale audio can be removed lock-free.
        // The ack tells the feeder this generation's data can no longer be wiped.
        cons.clear();
        cb.generation = generation;
        cb.underrun_episode = false;
        state.acknowledge(generation);
    }

    // Read the transport fields, then re-check the generation once: if a transport event
    // landed mid-read, treat this buffer as a transition and emit silence rather than act
    // on a torn playing/rate pair. Bounded — no retry loop in the callback.
    let playing = state.playing();
    let rate = state.rate();
    let stable = state.generation() == generation;

    if playing && rate == 1.0 && stable {
        let got = cons.pop_slice(out);
        if got < out.len() {
            out[got..].fill(0);
            // Silence before the feeder's first push for this generation is startup
            // latency; only a ring that ran dry after having data is an underrun.
            if state.pushed_generation() == generation && !cb.underrun_episode {
                cb.underrun_episode = true;
                state.note_underrun();
            }
        } else {
            cb.underrun_episode = false;
        }
    } else {
        // Paused or silent scrub: the device consumes silence and the counter advances,
        // but the ring is left untouched (the next generation clears it if stale).
        out.fill(0);
        cb.underrun_episode = false;
    }
    state.add_device_samples((out.len() / EXPORT_CHANNELS as usize) as u64);
}

/// The feeder thread: mix ahead of the callback, promote discontinuity reports.
#[allow(
    clippy::disallowed_methods,
    reason = "bounded 1 ms idle waits in the preview feeder; clippy.toml permits preview sleep loops"
)]
fn feeder_loop(
    state: Arc<ClockState>,
    plan: Arc<MixPlan>,
    sources: Arc<PreloadedSources>,
    mut prod: HeapProd<i16>,
) {
    let mut block = vec![0i16; PREVIEW_BLOCK_FRAMES * EXPORT_CHANNELS as usize];
    let mut last_underrun_epochs = 0u64;
    let mut last_error_epochs = 0u64;

    while !state.shutdown() {
        let error_epochs = state.error_epochs();
        if error_epochs != last_error_epochs {
            last_error_epochs = error_epochs;
            state.promote_device_error();
        }
        let underrun_epochs = state.underrun_epochs();
        if underrun_epochs != last_underrun_epochs {
            last_underrun_epochs = underrun_epochs;
            let snap = state.snapshot();
            if snap.playing && snap.rate == 1.0 {
                state.promote_underrun();
            }
        }

        let snap = state.snapshot();
        if !snap.playing || snap.rate != 1.0 {
            std::thread::sleep(FEEDER_IDLE);
            continue;
        }
        // Wait for the callback to clear-and-ack this generation before pushing, so the
        // clear cannot wipe fresh data (the handshake documented in the module header).
        if state.ack_generation() != snap.generation {
            std::thread::sleep(FEEDER_IDLE);
            continue;
        }
        if prod.vacant_len() < block.len() {
            std::thread::sleep(FEEDER_IDLE);
            continue;
        }

        // The next ring sample is consumed at `position + occupied`: the derived position
        // advances by exactly what the ring drains, so this sum is invariant between the
        // snapshot and the push (at rate 1.0). An overflowed/negative position is past
        // every clip — silence, never a wrapped index.
        let position = snap
            .timeline_sample()
            .ok()
            .and_then(|p| u64::try_from(p).ok())
            .unwrap_or(u64::MAX);
        let mix_start =
            position.saturating_add((prod.occupied_len() / EXPORT_CHANNELS as usize) as u64);
        if mix_preview_block(&plan, &sources, mix_start, &mut block).is_err() {
            // Preloaded sources are immutable and validated at open; a failure here is
            // unreachable. Stop audio rather than emit wrong samples (17.11).
            state.promote_device_error();
            break;
        }
        if state.generation() == snap.generation {
            prod.push_slice(&block);
            state.mark_pushed(snap.generation);
        }
        // No idle wait after a push: keep filling until the ring is full.
    }
}

/// Stable codes for the cpal error kinds the preview error channel carries. The mapping
/// lives here (not in `preview.rs`) so the transport module stays cpal-free.
fn kind_code(kind: cpal::ErrorKind) -> u8 {
    match kind {
        cpal::ErrorKind::DeviceBusy => 1,
        cpal::ErrorKind::DeviceChanged => 2,
        cpal::ErrorKind::DeviceNotAvailable => 3,
        cpal::ErrorKind::HostUnavailable => 4,
        cpal::ErrorKind::InvalidInput => 5,
        cpal::ErrorKind::PermissionDenied => 6,
        cpal::ErrorKind::RealtimeDenied => 7,
        cpal::ErrorKind::ResourceExhausted => 8,
        cpal::ErrorKind::StreamInvalidated => 9,
        cpal::ErrorKind::UnsupportedConfig => 10,
        cpal::ErrorKind::UnsupportedOperation => 11,
        cpal::ErrorKind::Xrun => 12,
        cpal::ErrorKind::BackendError => 13,
        cpal::ErrorKind::Other => 14,
        _ => 0,
    }
}

fn kind_from_code(code: u8) -> cpal::ErrorKind {
    match code {
        1 => cpal::ErrorKind::DeviceBusy,
        2 => cpal::ErrorKind::DeviceChanged,
        3 => cpal::ErrorKind::DeviceNotAvailable,
        4 => cpal::ErrorKind::HostUnavailable,
        5 => cpal::ErrorKind::InvalidInput,
        6 => cpal::ErrorKind::PermissionDenied,
        7 => cpal::ErrorKind::RealtimeDenied,
        8 => cpal::ErrorKind::ResourceExhausted,
        9 => cpal::ErrorKind::StreamInvalidated,
        10 => cpal::ErrorKind::UnsupportedConfig,
        11 => cpal::ErrorKind::UnsupportedOperation,
        12 => cpal::ErrorKind::Xrun,
        13 => cpal::ErrorKind::BackendError,
        _ => cpal::ErrorKind::Other,
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

    fn ring() -> (HeapProd<i16>, HeapCons<i16>) {
        HeapRb::<i16>::new(64).split()
    }

    #[test]
    fn the_callback_counts_silence_while_paused_and_preserves_the_ring() {
        let state = ClockState::new();
        let (mut prod, mut cons) = ring();
        prod.push_slice(&[1, 2, 3, 4]);
        let mut cb = CallbackClock {
            generation: 0,
            underrun_episode: false,
        };
        let mut out = [9i16; 4];
        fill_output(&mut out, &state, &mut cons, &mut cb);
        assert_eq!(out, [0; 4]);
        assert_eq!(cons.occupied_len(), 4);
        assert_eq!(state.snapshot().device_samples, 2);
        assert_eq!(state.underrun_epochs(), 0);
    }

    #[test]
    fn a_generation_change_clears_stale_ring_audio() {
        let shared = Arc::new(ClockState::new());
        let (mut prod, mut cons) = ring();
        prod.push_slice(&[7, 7, 7, 7]);
        let mut clock = PreviewClock::new(Arc::clone(&shared));
        clock.set_playing(true).unwrap();
        let mut cb = CallbackClock {
            generation: 0,
            underrun_episode: false,
        };
        let mut out = [0i16; 4];
        fill_output(&mut out, &shared, &mut cons, &mut cb);
        // Generation moved 0 -> 1: the stale [7,7,7,7] was cleared, silence emitted.
        assert_eq!(out, [0; 4]);
        assert_eq!(cons.occupied_len(), 0);
        assert_eq!(shared.snapshot().device_samples, 2);
    }

    #[test]
    fn underrun_reports_one_epoch_per_episode_and_only_after_data() {
        let shared = Arc::new(ClockState::new());
        let mut clock = PreviewClock::new(Arc::clone(&shared));
        clock.set_playing(true).unwrap();
        let (mut prod, mut cons) = ring();
        let mut cb = CallbackClock {
            generation: 1,
            underrun_episode: false,
        };
        let mut out = [0i16; 4];
        // No push yet: silence is startup latency, not an underrun.
        fill_output(&mut out, &shared, &mut cons, &mut cb);
        assert_eq!(shared.underrun_epochs(), 0);
        // Data arrives, then runs dry: one epoch for the episode.
        prod.push_slice(&[1, 2]);
        shared.mark_pushed(1);
        fill_output(&mut out, &shared, &mut cons, &mut cb);
        assert_eq!(out[..2], [1, 2]);
        assert_eq!(out[2..], [0, 0]);
        assert_eq!(shared.underrun_epochs(), 1);
        fill_output(&mut out, &shared, &mut cons, &mut cb);
        assert_eq!(shared.underrun_epochs(), 1);
        // A full buffer ends the episode; the next dry one is a new epoch.
        prod.push_slice(&[3, 4, 5, 6]);
        fill_output(&mut out, &shared, &mut cons, &mut cb);
        assert_eq!(shared.underrun_epochs(), 1);
        fill_output(&mut out, &shared, &mut cons, &mut cb);
        assert_eq!(shared.underrun_epochs(), 2);
    }
}
