//! Preview transport, clock generations and discontinuities (spec 17.3, 17.6, 17.7, 17.8).
//!
//! The preview clock is driven by the audio device, never by a wall clock: the callback
//! increments a device-sample counter and every transport event publishes a new generation
//! so video never infers continuity across a discontinuity (17.6). The snapshot fields are
//! 17.8's verbatim list; `device_samples` is the counter *within the current generation*
//! (the global counter minus the generation's base), so a reader can derive position from
//! the snapshot alone: `anchor + device_samples * rate` while playing, `anchor` while
//! paused (17.7).
//!
//! Three shape decisions are recorded rather than silent (DECISIONS.md #48/#49):
//!
//! - **atomics, not locks.** `clippy.toml` denies `Mutex`/`RwLock`. Transport fields live
//!   behind a seqlock built from atomics: writers enter with a CAS (so the transport owner
//!   and the feeder thread can both publish), readers retry until the sequence is stable.
//!   The callback itself only ever `fetch_add`s counters — it never enters the seqlock, so
//!   it cannot spin behind a writer.
//! - **the feeder promotes discontinuities.** The callback reports an underrun epoch and
//!   device errors as counter increments; the feeder thread turns them into generation
//!   bumps. That keeps the callback allocation-free, lock-free and bounded (17.6).
//! - **a borrowing preview mixer.** [`mix_range`] clones each active asset's whole PCM per
//!   call, which a real-time feeder cannot afford per block. [`mix_preview_block`] borrows
//!   preloaded canonical PCM and otherwise mirrors `mix.rs` sample-for-sample; a proptest
//!   below proves the two are byte-identical, so 17.5's "preview-only mixer" is a borrow
//!   optimization, not a second algorithm.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use anim_core::id::AssetId;

use crate::plan::{MixPlan, PcmAssetSource, PlannedClip};
use crate::time::SampleOffset;
use crate::{AudioError, CanonicalPcm, EXPORT_CHANNELS};

/// The symmetric full-scale magnitude, identical to `mix.rs`'s private constant: round to
/// `±32767`, never `−32768`. The mixer-identity proptest below guards the pairing.
const FULL_SCALE: f64 = 32_767.0;

/// An immutable reading of the preview clock (spec 17.8's field list, verbatim).
///
/// `device_samples` counts device sample frames consumed *within this generation*; the
/// anchor is the timeline sample that corresponded to the generation's first device
/// sample. Readers derive position with [`PreviewClockSnapshot::timeline_sample`] and
/// treat a changed `generation` as a discontinuity boundary (17.6/17.7).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreviewClockSnapshot {
    pub generation: u64,
    pub device_samples: u64,
    pub anchor_timeline_sample: i64,
    pub rate: f64,
    pub playing: bool,
    pub discontinuity: bool,
}

impl PreviewClockSnapshot {
    /// The derived timeline position in 48 kHz samples (spec 17.7).
    ///
    /// Paused: the anchor, frozen. Playing at rate 1.0: exact integer addition. Playing at
    /// another rate: `anchor + floor(device_samples * rate)` — the single float derivation
    /// in the preview path, legal because preview position never reaches exported bytes
    /// (17.6 forbids float *clip placement*, which stays integer in the plan).
    ///
    /// # Errors
    ///
    /// [`AudioError::Overflow`] when the derived position leaves `i64` — unreachable for
    /// any real session (i64::MAX samples is ~4.9 million years at 48 kHz), refused rather
    /// than wrapped for a hand-built snapshot.
    pub fn timeline_sample(&self) -> Result<i64, AudioError> {
        derive_position(
            self.anchor_timeline_sample,
            self.device_samples,
            self.rate,
            self.playing,
        )
    }
}

fn derive_position(anchor: i64, elapsed: u64, rate: f64, playing: bool) -> Result<i64, AudioError> {
    if !playing {
        return Ok(anchor);
    }
    if rate == 1.0 {
        let elapsed = i64::try_from(elapsed).map_err(|_| AudioError::Overflow {
            operation: "preview clock elapsed samples",
            value: elapsed.to_string(),
        })?;
        return anchor
            .checked_add(elapsed)
            .ok_or_else(|| AudioError::Overflow {
                operation: "preview clock position",
                value: format!("{anchor} + {elapsed}"),
            });
    }
    let scaled = elapsed as f64 * rate;
    // i64::MAX as f64 rounds up to 2^63, so `>=` is the correct exclusive bound; the floor
    // of anything strictly below it fits i64. A non-finite product (hand-built snapshot
    // with an absurd rate) is refused, never cast.
    if !scaled.is_finite() || scaled >= i64::MAX as f64 {
        return Err(AudioError::Overflow {
            operation: "preview clock scaled position",
            value: format!("{elapsed} * {rate}"),
        });
    }
    let whole = scaled.floor() as i64;
    anchor
        .checked_add(whole)
        .ok_or_else(|| AudioError::Overflow {
            operation: "preview clock position",
            value: format!("{anchor} + {whole}"),
        })
}

/// The shared atomic clock state. One logical clock, two writer roles (the transport owner
/// applying editor commands, and the feeder promoting underrun/device-error events), one
/// counter writer (the callback). The seqlock makes every snapshot a consistent reading of
/// the transport fields; the callback's counters are monotonic and read outside it.
pub(crate) struct ClockState {
    /// Seqlock sequence: odd while a writer is inside. CAS entry serializes writers.
    seq: AtomicU64,
    generation: AtomicU64,
    anchor: AtomicI64,
    base_device_samples: AtomicU64,
    rate_bits: AtomicU64,
    playing: AtomicBool,
    discontinuity: AtomicBool,
    /// Global device-sample counter; only the callback adds to it.
    device_samples: AtomicU64,
    /// The generation the feeder last pushed ring data for. Silence before the feeder's
    /// first push of a generation is startup latency, not an underrun (17.6).
    pushed_generation: AtomicU64,
    /// The generation the callback has acknowledged (cleared the ring for). The feeder
    /// pushes only for an acknowledged generation, so a clear can never wipe fresh data.
    ack_generation: AtomicU64,
    /// Callback reports one epoch per underrun episode; the feeder promotes each to a
    /// generation bump.
    underrun_epochs: AtomicU64,
    /// cpal error callback reports kind + epoch; the feeder stops audio and bumps.
    error_epochs: AtomicU64,
    error_kind: AtomicU8,
    /// Set inside the write window by `promote_device_error`; resuming a failed output is
    /// refused until an explicit reopen. Kept out of the snapshot: it is a refusal gate,
    /// not clock state the video side reads.
    failed: AtomicBool,
    shutdown: AtomicBool,
}

impl ClockState {
    pub(crate) fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            anchor: AtomicI64::new(0),
            base_device_samples: AtomicU64::new(0),
            rate_bits: AtomicU64::new(1.0f64.to_bits()),
            playing: AtomicBool::new(false),
            discontinuity: AtomicBool::new(false),
            device_samples: AtomicU64::new(0),
            pushed_generation: AtomicU64::new(u64::MAX),
            ack_generation: AtomicU64::new(0),
            underrun_epochs: AtomicU64::new(0),
            error_epochs: AtomicU64::new(0),
            error_kind: AtomicU8::new(0),
            failed: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        }
    }

    /// A consistent snapshot (17.8: readers receive atomic snapshots). Retries while a
    /// writer is inside or the sequence moved; the callback never calls this.
    pub(crate) fn snapshot(&self) -> PreviewClockSnapshot {
        loop {
            let seq = self.seq.load(Ordering::Acquire);
            if seq & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let generation = self.generation.load(Ordering::Relaxed);
            let anchor = self.anchor.load(Ordering::Relaxed);
            let base = self.base_device_samples.load(Ordering::Relaxed);
            let rate = f64::from_bits(self.rate_bits.load(Ordering::Relaxed));
            let playing = self.playing.load(Ordering::Relaxed);
            let discontinuity = self.discontinuity.load(Ordering::Relaxed);
            let device = self.device_samples.load(Ordering::Acquire);
            if self.seq.load(Ordering::Acquire) == seq {
                return PreviewClockSnapshot {
                    generation,
                    device_samples: device.saturating_sub(base),
                    anchor_timeline_sample: anchor,
                    rate,
                    playing,
                    discontinuity,
                };
            }
        }
    }

    /// Enter the write window. CAS from even to odd so two writers (transport owner and
    /// feeder) serialize without a lock. Called off the callback thread only.
    fn begin_write(&self) -> u64 {
        loop {
            let seq = self.seq.load(Ordering::Acquire);
            if seq & 1 == 0
                && self
                    .seq
                    .compare_exchange_weak(seq, seq + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                return seq;
            }
            std::hint::spin_loop();
        }
    }

    fn end_write(&self, started: u64) {
        self.seq.store(started + 2, Ordering::Release);
    }

    /// The current position computed inside a write window, where the transport fields
    /// are stable. An overflow (unreachable in any real session) freezes at the anchor
    /// rather than advancing into a wrap.
    fn position_locked(&self) -> i64 {
        let anchor = self.anchor.load(Ordering::Relaxed);
        let base = self.base_device_samples.load(Ordering::Relaxed);
        let rate = f64::from_bits(self.rate_bits.load(Ordering::Relaxed));
        let playing = self.playing.load(Ordering::Relaxed);
        let device = self.device_samples.load(Ordering::Relaxed);
        derive_position(anchor, device.saturating_sub(base), rate, playing).unwrap_or(anchor)
    }

    fn rate_locked(&self) -> f64 {
        f64::from_bits(self.rate_bits.load(Ordering::Relaxed))
    }

    fn playing_locked(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Publish one transport event inside a single write window: the closure reads the
    /// stable fields and returns the new (anchor, rate, playing, discontinuity); the
    /// generation rebases on the current device counter and bumps. An `Err` from the
    /// closure aborts the event with no field changed.
    fn publish<F>(&self, f: F) -> Result<(), AudioError>
    where
        F: FnOnce(&ClockState) -> Result<(i64, f64, bool, bool), AudioError>,
    {
        let seq = self.begin_write();
        let event = f(self);
        let (anchor, rate, playing, discontinuity) = match event {
            Ok(event) => event,
            Err(e) => {
                self.end_write(seq);
                return Err(e);
            }
        };
        let device = self.device_samples.load(Ordering::Relaxed);
        let generation = self.generation.load(Ordering::Relaxed);
        self.anchor.store(anchor, Ordering::Relaxed);
        self.base_device_samples.store(device, Ordering::Relaxed);
        self.rate_bits.store(rate.to_bits(), Ordering::Relaxed);
        self.playing.store(playing, Ordering::Relaxed);
        self.discontinuity.store(discontinuity, Ordering::Relaxed);
        // u64::MAX generations is unreachable; saturate at the width rather than wrap.
        self.generation
            .store(generation.saturating_add(1), Ordering::Relaxed);
        self.end_write(seq);
        Ok(())
    }

    // -- callback-side counters (fetch_add/store only, no seqlock) -------------------

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    pub(crate) fn rate(&self) -> f64 {
        f64::from_bits(self.rate_bits.load(Ordering::Acquire))
    }

    pub(crate) fn pushed_generation(&self) -> u64 {
        self.pushed_generation.load(Ordering::Acquire)
    }

    pub(crate) fn ack_generation(&self) -> u64 {
        self.ack_generation.load(Ordering::Acquire)
    }

    /// Callback-only writer: the ring has been cleared for this generation, so the feeder
    /// may push data for it without the clear wiping fresh samples.
    pub(crate) fn acknowledge(&self, generation: u64) {
        self.ack_generation.store(generation, Ordering::Release);
    }

    pub(crate) fn add_device_samples(&self, frames: u64) {
        self.device_samples.fetch_add(frames, Ordering::Release);
    }

    pub(crate) fn note_underrun(&self) {
        self.underrun_epochs.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn underrun_epochs(&self) -> u64 {
        self.underrun_epochs.load(Ordering::Acquire)
    }

    pub(crate) fn note_device_error(&self, kind_code: u8) {
        self.error_kind.store(kind_code, Ordering::Relaxed);
        self.error_epochs.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn error_epochs(&self) -> u64 {
        self.error_epochs.load(Ordering::Acquire)
    }

    pub(crate) fn error_kind(&self) -> u8 {
        self.error_kind.load(Ordering::Relaxed)
    }

    /// Feeder-only writer: the generation ring data was last pushed for. Plain store is
    /// safe because the feeder is the only writer and its snapshots are monotonic; a
    /// concurrent transport bump simply leaves this behind until the next push.
    pub(crate) fn mark_pushed(&self, generation: u64) {
        self.pushed_generation.store(generation, Ordering::Release);
    }

    pub(crate) fn shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    pub(crate) fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    // -- feeder-side promotions ------------------------------------------------------

    /// Underrun: the timeline kept advancing through consumed silence, so re-anchor at the
    /// current position (keeping elapsed small) and publish a discontinuity generation.
    pub(crate) fn promote_underrun(&self) {
        let _ = self.publish(|s| {
            Ok((
                s.position_locked(),
                s.rate_locked(),
                s.playing_locked(),
                true,
            ))
        });
    }

    /// Device error: stop audio, mark the discontinuity, leave editing available (17.11).
    pub(crate) fn promote_device_error(&self) {
        let _ = self.publish(|s| {
            s.failed.store(true, Ordering::Relaxed);
            Ok((s.position_locked(), s.rate_locked(), false, true))
        });
    }
}

/// The preview transport (spec 17.9's receiver). The editor owns it; every mutation is a
/// generation event so video re-syncs instead of inferring continuity (17.6).
pub struct PreviewClock {
    state: Arc<ClockState>,
}

impl PreviewClock {
    pub(crate) fn new(state: Arc<ClockState>) -> Self {
        Self { state }
    }

    /// An immutable, consistent reading of the clock (spec 17.9).
    #[must_use]
    pub fn preview_clock_snapshot(&self) -> PreviewClockSnapshot {
        self.state.snapshot()
    }

    /// Replace the anchor and increment the generation (spec 17.7/17.9).
    ///
    /// # Errors
    ///
    /// [`AudioError::Overflow`] when the position exceeds `i64` (the anchor's width).
    pub fn seek(&mut self, position: SampleOffset) -> Result<(), AudioError> {
        let anchor = i64::try_from(position.0).map_err(|_| AudioError::Overflow {
            operation: "preview seek anchor",
            value: position.0.to_string(),
        })?;
        self.state
            .publish(|s| Ok((anchor, s.rate_locked(), s.playing_locked(), false)))
    }

    /// Pause freezes the position; resume continues from it (spec 17.7). Both publish a
    /// generation change (17.6). Calling with the current state is a no-op, not an event.
    ///
    /// # Errors
    ///
    /// [`AudioError::DeviceLoss`] when resuming after a device error — recovery is an
    /// explicit reopen (DECISIONS.md #48 ruling 1).
    pub fn set_playing(&mut self, playing: bool) -> Result<(), AudioError> {
        if self.state.snapshot().playing == playing {
            return Ok(());
        }
        self.state.publish(|s| {
            if playing && s.failed.load(Ordering::Relaxed) {
                return Err(AudioError::DeviceLoss {
                    detail: "output device failed; reopen the preview output".to_owned(),
                });
            }
            let anchor = if playing {
                s.anchor.load(Ordering::Relaxed)
            } else {
                s.position_locked()
            };
            Ok((anchor, s.rate_locked(), playing, false))
        })
    }

    /// Set the playback rate (DECISIONS.md #48 ruling 2: silent scrub). The position
    /// re-anchors at the current instant and the generation bumps; audio is silent while
    /// the rate is not 1.0. Calling with the current rate is a no-op.
    ///
    /// # Errors
    ///
    /// [`AudioError::InvalidRate`] for a non-finite or non-positive rate.
    pub fn set_rate(&mut self, rate: f64) -> Result<(), AudioError> {
        if !rate.is_finite() || rate <= 0.0 {
            return Err(AudioError::InvalidRate {
                rate: format!("{rate}"),
            });
        }
        if self.state.snapshot().rate == rate {
            return Ok(());
        }
        self.state
            .publish(|s| Ok((s.position_locked(), rate, s.playing_locked(), false)))
    }
}

/// The plan's canonical PCM, preloaded once before the stream opens so the feeder never
/// touches the resolver, the disk or an allocation it does not own (17.6/17.10).
///
/// `AssetResolver` is single-threaded (`RefCell` cache), so preview cannot share it; the
/// preloaded map is immutable and `Sync`, and every clip's source invariants are checked
/// here with the same errors `mix_range` would report at mix time.
pub(crate) struct PreloadedSources {
    pcms: BTreeMap<AssetId, CanonicalPcm>,
}

impl PreloadedSources {
    pub(crate) fn load(plan: &MixPlan, sources: &dyn PcmAssetSource) -> Result<Self, AudioError> {
        let mut pcms = BTreeMap::new();
        for clip in plan.clips() {
            if let std::collections::btree_map::Entry::Vacant(entry) = pcms.entry(clip.asset_id) {
                let pcm = sources.canonical_pcm(clip.asset_id)?;
                let channels = pcm.channels();
                if channels != 1 && channels != 2 {
                    return Err(AudioError::UnsupportedLayout { channels });
                }
                entry.insert(pcm);
            }
            let pcm = pcms
                .get(&clip.asset_id)
                .ok_or_else(|| AudioError::MissingAsset {
                    asset_id: clip.asset_id.to_canonical_string(),
                })?;
            if pcm.frames() != clip.source_len {
                return Err(AudioError::ChangedAsset {
                    asset_id: clip.asset_id.to_canonical_string(),
                    detail: format!(
                        "canonical length {} at preload, plan compiled against {}",
                        pcm.frames(),
                        clip.source_len
                    ),
                });
            }
            if clip.looped && pcm.frames() == 0 {
                return Err(AudioError::InvalidLoop {
                    clip_id: clip.clip_id.to_canonical_string(),
                    source_len: 0,
                });
            }
        }
        Ok(Self { pcms })
    }

    fn get(&self, asset_id: AssetId) -> Option<&CanonicalPcm> {
        self.pcms.get(&asset_id)
    }
}

/// Mix one preview block into an interleaved stereo `i16` buffer, borrowing preloaded PCM.
///
/// This is 17.5's "preview-only mixer": the per-sample math, canonical clip order, fades,
/// looping, channel conversion, clamp and symmetric rounding are `mix.rs`'s verbatim, so
/// preview audio and offline audio are the same samples (proven by the identity proptest
/// below). `start` is an absolute timeline sample; positions outside every clip mix silence.
/// The buffer length must be even (stereo frames); the feeder owns a fixed one.
///
/// # Errors
///
/// The same categories [`crate::mix_range`] reports for a source invariant violation.
/// With preloaded, validated sources they are unreachable; they are kept so a hand-built
/// plan fails typed rather than panicking.
pub(crate) fn mix_preview_block(
    plan: &MixPlan,
    sources: &PreloadedSources,
    start: u64,
    out: &mut [i16],
) -> Result<(), AudioError> {
    let frames = out.len() as u64 / u64::from(EXPORT_CHANNELS);
    let end = start.saturating_add(frames);

    let mut active: Vec<(&PlannedClip, &CanonicalPcm, u64)> = Vec::new();
    for clip in plan.clips() {
        let clip_start = clip.timeline.start.0;
        let clip_end =
            clip_start
                .checked_add(clip.timeline.len)
                .ok_or_else(|| AudioError::Overflow {
                    operation: "clip timeline end",
                    value: format!("{clip_start} + {}", clip.timeline.len),
                })?;
        if clip_end <= start || clip_start >= end {
            continue;
        }
        let pcm = sources
            .get(clip.asset_id)
            .ok_or_else(|| AudioError::MissingAsset {
                asset_id: clip.asset_id.to_canonical_string(),
            })?;
        let channels = pcm.channels();
        if channels != 1 && channels != 2 {
            return Err(AudioError::UnsupportedLayout { channels });
        }
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

    for i in 0..frames {
        // Saturating because `end` saturates: a position at the width boundary intersects
        // no clip (every clip_end <= u64::MAX <= absolute), i.e. silence, never a wrap.
        let absolute = start.saturating_add(i);
        let mut acc = [0.0f64; 2];

        for (clip, pcm, clip_end) in &active {
            let clip_start = clip.timeline.start.0;
            let clip_len = clip.timeline.len;
            if absolute < clip_start || absolute >= *clip_end {
                continue;
            }
            let pos = absolute - clip_start;

            let mut source_pos =
                clip.source_start
                    .0
                    .checked_add(pos)
                    .ok_or_else(|| AudioError::Overflow {
                        operation: "mix source position",
                        value: format!("{} + {pos}", clip.source_start.0),
                    })?;
            if clip.looped {
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

            let mut fade = 1.0f64;
            if clip.fade_in_samples > 0 && pos < clip.fade_in_samples {
                fade *= pos as f64 / clip.fade_in_samples as f64;
            }
            if clip.fade_out_samples > 0 && pos >= clip_len.saturating_sub(clip.fade_out_samples) {
                fade *= (clip_len - pos) as f64 / clip.fade_out_samples as f64;
            }
            let total_gain = clip.gain * fade.clamp(0.0, 1.0);

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

        let out_idx = i as usize * 2;
        out[out_idx] = (acc[0].clamp(-1.0, 1.0) * FULL_SCALE).round() as i16;
        out[out_idx + 1] = (acc[1].clamp(-1.0, 1.0) * FULL_SCALE).round() as i16;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use crate::mix::mix_range;
    use crate::time::SampleRange;
    use crate::OfflineAudioSpec;
    use anim_assets::ContentHash;
    use anim_core::id::TrackId;
    use proptest::prelude::*;
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn asset(label: &str) -> AssetId {
        AssetId::derive(&namespace(), label.as_bytes())
    }

    struct TestSrc(BTreeMap<AssetId, CanonicalPcm>);

    impl PcmAssetSource for TestSrc {
        fn canonical_pcm(&self, asset_id: AssetId) -> Result<CanonicalPcm, AudioError> {
            self.0
                .get(&asset_id)
                .cloned()
                .ok_or_else(|| AudioError::MissingAsset {
                    asset_id: asset_id.to_canonical_string(),
                })
        }
    }

    fn pcm(channels: u16, samples: Vec<f32>) -> CanonicalPcm {
        let frames = samples.len() as u64 / u64::from(channels);
        CanonicalPcm::new(
            ContentHash::from_bytes([0x5A; 32]),
            channels,
            frames,
            samples,
        )
        .unwrap()
    }

    #[test]
    fn a_fresh_clock_is_paused_at_zero_and_seeking_replaces_the_anchor() {
        let state = Arc::new(ClockState::new());
        let mut clock = PreviewClock::new(Arc::clone(&state));
        let snap = clock.preview_clock_snapshot();
        assert_eq!(snap.generation, 0);
        assert_eq!(snap.anchor_timeline_sample, 0);
        assert_eq!(snap.device_samples, 0);
        assert_eq!(snap.rate, 1.0);
        assert!(!snap.playing);
        assert!(!snap.discontinuity);
        assert_eq!(snap.timeline_sample().unwrap(), 0);

        clock.seek(SampleOffset::new(48_000)).unwrap();
        let snap = clock.preview_clock_snapshot();
        assert_eq!(snap.generation, 1);
        assert_eq!(snap.anchor_timeline_sample, 48_000);
        assert_eq!(snap.timeline_sample().unwrap(), 48_000);

        clock.set_playing(true).unwrap();
        state.add_device_samples(1_000);
        let snap = clock.preview_clock_snapshot();
        assert_eq!(snap.generation, 2);
        assert_eq!(snap.device_samples, 1_000);
        assert_eq!(snap.timeline_sample().unwrap(), 49_000);

        clock.set_playing(false).unwrap();
        let frozen = clock.preview_clock_snapshot();
        assert_eq!(frozen.generation, 3);
        assert_eq!(frozen.timeline_sample().unwrap(), 49_000);
        state.add_device_samples(5_000);
        let snap = clock.preview_clock_snapshot();
        assert_eq!(snap.timeline_sample().unwrap(), 49_000);
        // Elapsed within the paused generation: 5,000 of the global 6,000 (the pause
        // rebased the counter at 1,000).
        assert_eq!(snap.device_samples, 5_000);

        clock.set_playing(true).unwrap();
        let snap = clock.preview_clock_snapshot();
        assert_eq!(snap.generation, 4);
        assert_eq!(snap.device_samples, 0);
        assert_eq!(snap.timeline_sample().unwrap(), 49_000);
    }

    #[test]
    fn rate_changes_reanchor_and_scale_the_derived_position() {
        let state = Arc::new(ClockState::new());
        let mut clock = PreviewClock::new(Arc::clone(&state));
        clock.seek(SampleOffset::new(10_000)).unwrap();
        clock.set_playing(true).unwrap();
        state.add_device_samples(500);
        clock.set_rate(2.0).unwrap();
        let snap = clock.preview_clock_snapshot();
        assert_eq!(snap.anchor_timeline_sample, 10_500);
        assert_eq!(snap.device_samples, 0);
        assert_eq!(snap.rate, 2.0);
        state.add_device_samples(100);
        assert_eq!(
            clock.preview_clock_snapshot().timeline_sample().unwrap(),
            10_700
        );
        // Idempotent calls are not events.
        let gen = clock.preview_clock_snapshot().generation;
        clock.set_rate(2.0).unwrap();
        clock.set_playing(true).unwrap();
        assert_eq!(clock.preview_clock_snapshot().generation, gen);
    }

    #[test]
    fn invalid_rates_and_overflowing_seeks_are_refused() {
        let state = Arc::new(ClockState::new());
        let mut clock = PreviewClock::new(state);
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                clock.set_rate(rate),
                Err(AudioError::InvalidRate { .. })
            ));
        }
        assert!(matches!(
            clock.seek(SampleOffset::new(u64::MAX)),
            Err(AudioError::Overflow { .. })
        ));
        let snap = PreviewClockSnapshot {
            generation: 0,
            device_samples: u64::MAX,
            anchor_timeline_sample: i64::MAX,
            rate: 1.0,
            playing: true,
            discontinuity: false,
        };
        assert!(matches!(
            snap.timeline_sample(),
            Err(AudioError::Overflow { .. })
        ));
    }

    #[test]
    fn underrun_and_device_error_promotions_bump_and_reanchor() {
        let state = Arc::new(ClockState::new());
        let mut clock = PreviewClock::new(Arc::clone(&state));
        clock.set_playing(true).unwrap();
        state.add_device_samples(2_000);
        state.promote_underrun();
        let snap = clock.preview_clock_snapshot();
        assert!(snap.discontinuity);
        assert!(snap.playing);
        assert_eq!(snap.anchor_timeline_sample, 2_000);
        assert_eq!(snap.device_samples, 0);
        assert_eq!(snap.timeline_sample().unwrap(), 2_000);

        state.add_device_samples(500);
        state.promote_device_error();
        let snap = clock.preview_clock_snapshot();
        assert!(snap.discontinuity);
        assert!(!snap.playing);
        assert_eq!(snap.timeline_sample().unwrap(), 2_500);
        assert!(matches!(
            clock.set_playing(true),
            Err(AudioError::DeviceLoss { .. })
        ));
        // Editing stays available after device loss (17.11).
        clock.seek(SampleOffset::new(7)).unwrap();
        assert_eq!(clock.preview_clock_snapshot().anchor_timeline_sample, 7);
    }

    proptest! {
        /// 17.5's preview-only mixer must be the offline mixer: same plan, same range,
        /// byte-identical samples. The feeder borrows where `mix_range` clones; nothing
        /// else may differ.
        #[test]
        fn the_preview_mixer_is_byte_identical_to_the_offline_mixer(
            seed in 0u64..1000,
            start in 0u64..160,
            len in 1u64..80,
        ) {
            let mut pcms = BTreeMap::new();
            let mut clips = Vec::new();
            let count = 1 + (seed % 3) as usize;
            for c in 0..count {
                let channels = if (seed + c as u64).is_multiple_of(2) { 1u16 } else { 2u16 };
                let frames = 1 + ((seed * 7 + c as u64 * 13) % 47);
                let samples: Vec<f32> = (0..frames * u64::from(channels))
                    .map(|i| (((seed.wrapping_mul(31) + i * 17 + c as u64 * 5) % 2001) as f32 - 1000.0) / 1000.0)
                    .collect();
                let id = asset(&format!("asset-{c}"));
                pcms.insert(id, pcm(channels, samples));
                let clip_start = (seed * 3 + c as u64 * 29) % 120;
                let clip_len = 1 + (seed * 11 + c as u64 * 7) % 90;
                clips.push(PlannedClip {
                    clip_id: TrackId::derive(&namespace(), format!("clip-{c}").as_bytes()),
                    asset_id: id,
                    timeline: SampleRange::new(SampleOffset::new(clip_start), clip_len),
                    source_start: SampleOffset::new((seed + c as u64 * 3) % 40),
                    source_len: frames,
                    looped: (seed + c as u64).is_multiple_of(3),
                    gain: ((seed + c as u64) % 21) as f64 / 10.0,
                    fade_in_samples: (seed + c as u64 * 5) % 20,
                    fade_out_samples: (seed * 2 + c as u64 * 3) % 20,
                });
            }
            let plan = MixPlan::new(
                clips,
                SampleRange::new(SampleOffset::new(0), 240),
                OfflineAudioSpec::default(),
            );
            let src = TestSrc(pcms);
            let preloaded = PreloadedSources::load(&plan, &src).unwrap();
            let range = SampleRange::new(SampleOffset::new(start), len);
            let mut offline = vec![0i16; (len * 2) as usize];
            mix_range(&plan, &src, range, &mut offline).unwrap();
            let mut preview = vec![0i16; (len * 2) as usize];
            mix_preview_block(&plan, &preloaded, start, &mut preview).unwrap();
            prop_assert_eq!(offline, preview);
        }
    }

    #[test]
    fn positions_outside_every_clip_mix_silence() {
        let id = asset("one");
        let mut pcms = BTreeMap::new();
        pcms.insert(id, pcm(1, vec![0.5; 16]));
        let plan = MixPlan::new(
            vec![PlannedClip {
                clip_id: TrackId::derive(&namespace(), b"clip"),
                asset_id: id,
                timeline: SampleRange::new(SampleOffset::new(10), 16),
                source_start: SampleOffset::new(0),
                source_len: 16,
                looped: false,
                gain: 1.0,
                fade_in_samples: 0,
                fade_out_samples: 0,
            }],
            SampleRange::new(SampleOffset::new(0), 64),
            OfflineAudioSpec::default(),
        );
        let src = TestSrc(pcms);
        let preloaded = PreloadedSources::load(&plan, &src).unwrap();
        let mut out = [7i16; 32];
        mix_preview_block(&plan, &preloaded, 1_000, &mut out).unwrap();
        assert_eq!(out, [0i16; 32]);
        mix_preview_block(&plan, &preloaded, u64::MAX - 4, &mut out).unwrap();
        assert_eq!(out, [0i16; 32]);
    }

    /// A writer hammering transport events while a reader takes snapshots must never
    /// expose a torn generation/anchor pair: every snapshot's position is one of the
    /// positions the writer published, and generations never move backwards.
    #[test]
    fn concurrent_writers_and_readers_never_tear_a_snapshot() {
        let state = Arc::new(ClockState::new());
        let mut clock = PreviewClock::new(Arc::clone(&state));
        clock.set_playing(true).unwrap();
        let writer = std::thread::spawn(move || {
            for i in 0..2_000u64 {
                clock.seek(SampleOffset::new(i * 100)).unwrap();
                if i % 7 == 0 {
                    clock.set_rate(1.0).unwrap();
                }
            }
        });
        let reader_state = Arc::clone(&state);
        let reader = std::thread::spawn(move || {
            let mut last_gen = 0;
            for _ in 0..2_000 {
                let snap = reader_state.snapshot();
                assert!(snap.generation >= last_gen);
                last_gen = snap.generation;
                let pos = snap.timeline_sample().unwrap();
                assert!(pos >= 0);
                assert_eq!(
                    pos,
                    snap.anchor_timeline_sample + snap.device_samples as i64
                );
            }
        });
        writer.join().unwrap();
        reader.join().unwrap();
    }
}
