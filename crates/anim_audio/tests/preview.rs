//! Preview validation (spec 17.12, preview half; DECISIONS.md #48 ruling 4).
//!
//! The device matrix runs on deterministic fake devices injected into the real
//! `cpal::Device` enum through cpal's `custom` host feature: the test thread drives every
//! callback, so pause, seek, underrun, device restart and teardown are forced exactly
//! rather than hoped for. The final test opens this machine's default WASAPI device as a
//! live smoke. The "preview disabled" leg of 17.12's isolation clause is the default
//! (feature-off) gate run, which compiles no cpal at all; the in-process legs below cover
//! enabled, paused, scrubbing, underrun, device loss and two different devices.
//!
//! Expected audio is always `mix_range` over the same plan and range — the offline mixer
//! is the oracle, so a preview that drifts from export bytes fails here.

#![cfg(feature = "preview-audio")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    reason = "test assertions and bounded waits for the feeder/device; clippy.toml permits preview sleep loops"
)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use anim_assets::ContentHash;
use anim_audio::cpal::traits::{DeviceTrait, StreamTrait};
use anim_audio::cpal::{
    self, Data, DeviceDescription, DeviceDescriptionBuilder, DeviceDirection, DeviceId,
    Error as CpalError, ErrorKind, FrameCount, HostId, InputCallbackInfo, OutputCallbackInfo,
    OutputStreamTimestamp, SampleFormat, StreamConfig, StreamInstant, SupportedBufferSize,
    SupportedStreamConfig, SupportedStreamConfigRange,
};
use anim_audio::{
    mix_range, render_offline, AudioError, CanonicalPcm, MixPlan, OfflineAudioSpec, PcmAssetSource,
    PlannedClip, PreviewOutput, SampleOffset, SampleRange, PCM_DOMAIN,
};
use anim_core::hash::CanonicalBytes;
use anim_core::id::{AssetId, TrackId};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fake cpal host: the test thread is the device clock
// ---------------------------------------------------------------------------

type DataCallback = Box<dyn FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static>;
type ErrorCallback = Box<dyn FnMut(CpalError) + Send + 'static>;

struct BuiltStream {
    data: DataCallback,
    error: ErrorCallback,
    state: Arc<FakeStreamState>,
}

struct FakeStreamState {
    playing: AtomicBool,
    dropped: AtomicBool,
}

struct FakeControl {
    tx: Sender<BuiltStream>,
    fail_builds: AtomicBool,
    rate: u32,
    channels: u16,
    buffer_min: u32,
    buffer_max: u32,
}

#[derive(Clone)]
struct FakeDevice {
    id: String,
    name: String,
    control: Arc<FakeControl>,
}

impl PartialEq for FakeDevice {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for FakeDevice {}

impl std::hash::Hash for FakeDevice {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl std::fmt::Display for FakeDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl std::fmt::Debug for FakeDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeDevice").field("id", &self.id).finish()
    }
}

impl DeviceTrait for FakeDevice {
    type SupportedInputConfigs = std::vec::IntoIter<SupportedStreamConfigRange>;
    type SupportedOutputConfigs = std::vec::IntoIter<SupportedStreamConfigRange>;
    type Stream = FakeStream;

    fn description(&self) -> Result<DeviceDescription, CpalError> {
        Ok(DeviceDescriptionBuilder::new(&self.name)
            .direction(DeviceDirection::Output)
            .build())
    }

    fn id(&self) -> Result<DeviceId, CpalError> {
        Ok(DeviceId::new(HostId::Custom, &self.id))
    }

    fn supported_input_configs(&self) -> Result<Self::SupportedInputConfigs, CpalError> {
        Ok(Vec::new().into_iter())
    }

    fn supported_output_configs(&self) -> Result<Self::SupportedOutputConfigs, CpalError> {
        Ok(vec![SupportedStreamConfigRange::new(
            self.control.channels,
            self.control.rate,
            self.control.rate,
            SupportedBufferSize::Range {
                min: self.control.buffer_min,
                max: self.control.buffer_max,
            },
            SampleFormat::I16,
        )]
        .into_iter())
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, CpalError> {
        Err(CpalError::new(ErrorKind::UnsupportedOperation))
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, CpalError> {
        Ok(SupportedStreamConfig::new(
            self.control.channels,
            self.control.rate,
            SupportedBufferSize::Range {
                min: self.control.buffer_min,
                max: self.control.buffer_max,
            },
            SampleFormat::I16,
        ))
    }

    fn build_input_stream_raw<D, E>(
        &self,
        _config: StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, CpalError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(CpalError) + Send + 'static,
    {
        Err(CpalError::new(ErrorKind::UnsupportedOperation))
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, CpalError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(CpalError) + Send + 'static,
    {
        if self.control.fail_builds.load(Ordering::Acquire) {
            return Err(CpalError::new(ErrorKind::DeviceNotAvailable));
        }
        if config.sample_rate != self.control.rate
            || config.channels != self.control.channels
            || sample_format != SampleFormat::I16
        {
            return Err(CpalError::new(ErrorKind::UnsupportedConfig));
        }
        let state = Arc::new(FakeStreamState {
            playing: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
        });
        let built = BuiltStream {
            data: Box::new(data_callback),
            error: Box::new(error_callback),
            state: Arc::clone(&state),
        };
        self.control.tx.send(built).map_err(|_| {
            CpalError::with_message(ErrorKind::BackendError, "test harness dropped")
        })?;
        Ok(FakeStream { state })
    }
}

struct FakeStream {
    state: Arc<FakeStreamState>,
}

impl StreamTrait for FakeStream {
    fn play(&self) -> Result<(), CpalError> {
        if self.state.dropped.load(Ordering::Acquire) {
            return Err(CpalError::new(ErrorKind::DeviceNotAvailable));
        }
        self.state.playing.store(true, Ordering::Release);
        Ok(())
    }

    fn pause(&self) -> Result<(), CpalError> {
        self.state.playing.store(false, Ordering::Release);
        Ok(())
    }

    fn buffer_size(&self) -> Result<FrameCount, CpalError> {
        Ok(256)
    }

    fn now(&self) -> StreamInstant {
        StreamInstant::ZERO
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.state.dropped.store(true, Ordering::Release);
    }
}

struct FakeHarness {
    rx: Receiver<BuiltStream>,
    built: Option<BuiltStream>,
}

impl FakeHarness {
    fn accept_stream(&mut self) {
        self.built = Some(self.rx.try_recv().expect("open built a stream"));
    }

    /// One device callback of exactly `frames` frames. Returns the samples the callback
    /// wrote, or an empty vec when the stream is paused/destroyed (cpal fires no callback
    /// then — the ownership clause of 17.7).
    fn tick(&mut self, frames: usize) -> Vec<i16> {
        let built = self.built.as_mut().expect("call accept_stream first");
        if !built.state.playing.load(Ordering::Acquire)
            || built.state.dropped.load(Ordering::Acquire)
        {
            return Vec::new();
        }
        let mut samples = vec![0i16; frames * 2];
        // SAFETY: the pointer is `samples`' own buffer, alive and exclusively borrowed for
        // the callback call, and the I16 format matches the buffer's element type.
        let mut data = unsafe {
            Data::from_parts(
                samples.as_mut_ptr().cast(),
                samples.len(),
                SampleFormat::I16,
            )
        };
        let info = OutputCallbackInfo::new(OutputStreamTimestamp {
            callback: StreamInstant::ZERO,
            playback: StreamInstant::ZERO,
        });
        (built.data)(&mut data, &info);
        samples
    }

    fn fire_error(&mut self, kind: ErrorKind) {
        let built = self.built.as_mut().expect("call accept_stream first");
        (built.error)(CpalError::new(kind));
    }

    fn stream_dropped(&self) -> bool {
        self.built
            .as_ref()
            .expect("call accept_stream first")
            .state
            .dropped
            .load(Ordering::Acquire)
    }
}

fn fake_device(
    id: &str,
    rate: u32,
    channels: u16,
) -> (cpal::Device, FakeHarness, Arc<FakeControl>) {
    fake_device_with_buffers(id, rate, channels, 64, 4096)
}

fn fake_device_with_buffers(
    id: &str,
    rate: u32,
    channels: u16,
    buffer_min: u32,
    buffer_max: u32,
) -> (cpal::Device, FakeHarness, Arc<FakeControl>) {
    let (tx, rx) = channel();
    let control = Arc::new(FakeControl {
        tx,
        fail_builds: AtomicBool::new(false),
        rate,
        channels,
        buffer_min,
        buffer_max,
    });
    let device = FakeDevice {
        id: id.to_owned(),
        name: format!("Fake {id}"),
        control: Arc::clone(&control),
    };
    let custom = cpal::platform::CustomDevice::from_device(device);
    (
        cpal::Device::from(custom),
        FakeHarness { rx, built: None },
        control,
    )
}

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

fn namespace() -> Uuid {
    Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
}

fn asset_id(label: &str) -> AssetId {
    AssetId::derive(&namespace(), label.as_bytes())
}

fn track_id(label: &str) -> TrackId {
    TrackId::derive(&namespace(), label.as_bytes())
}

struct TestPcmSource {
    pcms: BTreeMap<AssetId, CanonicalPcm>,
}

impl TestPcmSource {
    fn new() -> Self {
        Self {
            pcms: BTreeMap::new(),
        }
    }

    fn with(mut self, id: AssetId, pcm: CanonicalPcm) -> Self {
        self.pcms.insert(id, pcm);
        self
    }
}

impl PcmAssetSource for TestPcmSource {
    fn canonical_pcm(&self, asset_id: AssetId) -> Result<CanonicalPcm, AudioError> {
        self.pcms
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

/// A position-dependent mono ramp so captured audio identifies its timeline position.
fn ramp_plan(frames: u64) -> (MixPlan, TestPcmSource) {
    let id = asset_id("ramp");
    let samples: Vec<f32> = (0..frames)
        .map(|i| ((i % 1000) as f32 / 1000.0) - 0.5)
        .collect();
    let src = TestPcmSource::new().with(id, pcm(1, samples));
    let clip = PlannedClip {
        clip_id: track_id("ramp-clip"),
        asset_id: id,
        timeline: SampleRange::new(SampleOffset::new(0), frames),
        source_start: SampleOffset::new(0),
        source_len: frames,
        looped: false,
        gain: 1.0,
        fade_in_samples: 0,
        fade_out_samples: 0,
    };
    let plan = MixPlan::new(
        vec![clip],
        SampleRange::new(SampleOffset::new(0), frames),
        OfflineAudioSpec::default(),
    );
    (plan, src)
}

fn offline_block(plan: &MixPlan, src: &TestPcmSource, start: u64, frames: u64) -> Vec<i16> {
    let mut out = vec![0i16; (frames * 2) as usize];
    mix_range(
        plan,
        src,
        SampleRange::new(SampleOffset::new(start), frames),
        &mut out,
    )
    .unwrap();
    out
}

fn pcm_hash(samples: &[i16]) -> ContentHash {
    let mut raw = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        raw.extend_from_slice(&s.to_le_bytes());
    }
    let mut cp = CanonicalBytes::with_domain(PCM_DOMAIN).unwrap();
    cp.u64(samples.len() as u64);
    cp.bytes(&raw);
    ContentHash::from_bytes(cp.digest())
}

/// Bounded wait for the feeder thread or a live device (up to ~5 s).
fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..5_000 {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    cond()
}

/// A tick paced well below real-time drain so the feeder keeps the ring full; underruns
/// in this suite are forced explicitly, never by test-thread speed.
fn paced_tick(harness: &mut FakeHarness, frames: usize) -> Vec<i16> {
    let cap = harness.tick(frames);
    std::thread::sleep(Duration::from_millis(2));
    cap
}

fn is_silence(samples: &[i16]) -> bool {
    samples.iter().all(|&s| s == 0)
}

/// Tick until audio flows, counting every tick (including the startup-silence ones: the
/// first callback after a generation change clears-and-acks the ring, so it is silent by
/// protocol). The count keeps position assertions exact.
fn tick_until_audio(harness: &mut FakeHarness, ticks: &mut u64) {
    for _ in 0..500 {
        let cap = paced_tick(harness, 256);
        *ticks += 1;
        if !is_silence(&cap) {
            return;
        }
    }
    panic!("the feeder never produced audio");
}

// ---------------------------------------------------------------------------
// Transport generations (spec 17.6/17.7/17.9)
// ---------------------------------------------------------------------------

#[test]
fn transport_events_publish_generations_and_idempotent_calls_do_not() {
    let (plan, src) = ramp_plan(4_096);
    let (device, mut harness, _control) = fake_device("gen", 48_000, 2);
    let mut output = PreviewOutput::open(&device, plan, &src).unwrap();
    harness.accept_stream();

    assert_eq!(output.preview_clock_snapshot().generation, 0);
    output.set_playing(true).unwrap();
    assert_eq!(output.preview_clock_snapshot().generation, 1);
    output.set_playing(true).unwrap();
    assert_eq!(output.preview_clock_snapshot().generation, 1);
    output.set_playing(false).unwrap();
    assert_eq!(output.preview_clock_snapshot().generation, 2);
    output.seek(SampleOffset::new(500)).unwrap();
    let snap = output.preview_clock_snapshot();
    assert_eq!(snap.generation, 3);
    assert_eq!(snap.anchor_timeline_sample, 500);
    output.set_rate(2.0).unwrap();
    assert_eq!(output.preview_clock_snapshot().generation, 4);
    output.set_rate(2.0).unwrap();
    assert_eq!(output.preview_clock_snapshot().generation, 4);
    output.set_rate(1.0).unwrap();
    assert_eq!(output.preview_clock_snapshot().generation, 5);
    assert!(!output.preview_clock_snapshot().discontinuity);
}

#[test]
fn pause_freezes_the_position_while_the_device_keeps_counting() {
    let (plan, src) = ramp_plan(200_000);
    let (device, mut harness, _control) = fake_device("pause", 48_000, 2);
    let mut output = PreviewOutput::open(&device, plan, &src).unwrap();
    harness.accept_stream();
    output.set_playing(true).unwrap();
    let mut ticks = 0u64;
    tick_until_audio(&mut harness, &mut ticks);

    let frozen = output.preview_clock_snapshot().timeline_sample().unwrap();
    assert_eq!(frozen, (ticks * 256) as i64);
    output.set_playing(false).unwrap();
    assert!(is_silence(&harness.tick(2_000)));
    let snap = output.preview_clock_snapshot();
    assert!(!snap.playing);
    assert_eq!(snap.timeline_sample().unwrap(), frozen);
    assert!(
        snap.device_samples > 0,
        "the device consumed the paused silence"
    );

    output.set_playing(true).unwrap();
    tick_until_audio(&mut harness, &mut ticks);
    assert!(output.preview_clock_snapshot().timeline_sample().unwrap() >= frozen);
}

#[test]
fn seek_replaces_the_anchor_and_preview_plays_from_the_new_position() {
    let (plan, src) = ramp_plan(200_000);
    let (device, mut harness, _control) = fake_device("seek", 48_000, 2);
    let mut output = PreviewOutput::open(&device, plan.clone(), &src).unwrap();
    harness.accept_stream();
    let seek_to = 12_345u64;
    output.seek(SampleOffset::new(seek_to)).unwrap();
    output.set_playing(true).unwrap();

    let mut consumed = 0u64;
    let mut proven = false;
    for _ in 0..500 {
        let cap = paced_tick(&mut harness, 256);
        let expected = offline_block(&plan, &src, seek_to + consumed, 256);
        consumed += 256;
        if cap == expected {
            proven = true;
            break;
        }
        assert!(
            is_silence(&cap),
            "preview played audio from the wrong position after a seek"
        );
    }
    assert!(proven, "the feeder never refilled after the seek");
    let snap = output.preview_clock_snapshot();
    assert_eq!(snap.timeline_sample().unwrap(), (seek_to + consumed) as i64);
}

#[test]
fn rate_changes_silence_the_scrub_and_scale_the_clock() {
    let (plan, src) = ramp_plan(200_000);
    let (device, mut harness, _control) = fake_device("rate", 48_000, 2);
    let mut output = PreviewOutput::open(&device, plan.clone(), &src).unwrap();
    harness.accept_stream();
    output.set_playing(true).unwrap();
    let mut ticks = 0u64;
    tick_until_audio(&mut harness, &mut ticks);
    let played = ticks * 256;
    assert_eq!(
        output.preview_clock_snapshot().timeline_sample().unwrap(),
        played as i64
    );

    output.set_rate(2.0).unwrap();
    assert!(
        is_silence(&harness.tick(1_000)),
        "scrub audio must be silent"
    );
    let snap = output.preview_clock_snapshot();
    assert_eq!(snap.rate, 2.0);
    assert_eq!(snap.anchor_timeline_sample, played as i64);
    assert_eq!(snap.device_samples, 1_000);
    assert_eq!(snap.timeline_sample().unwrap(), (played + 2_000) as i64);

    output.set_rate(1.0).unwrap();
    let resume_at = output.preview_clock_snapshot().anchor_timeline_sample;
    assert_eq!(resume_at, (played + 2_000) as i64);
    let mut consumed = 0u64;
    let mut proven = false;
    for _ in 0..500 {
        let cap = paced_tick(&mut harness, 256);
        let expected = offline_block(&plan, &src, (resume_at as u64) + consumed, 256);
        consumed += 256;
        if cap == expected {
            proven = true;
            break;
        }
        assert!(
            is_silence(&cap),
            "audio after the scrub resumed from the wrong position"
        );
    }
    assert!(
        proven,
        "the feeder never refilled after the rate returned to 1.0"
    );
}

// ---------------------------------------------------------------------------
// Underrun, device loss, restart, teardown (spec 17.6/17.11/17.12)
// ---------------------------------------------------------------------------

#[test]
fn underrun_marks_a_discontinuity_and_the_timeline_keeps_advancing() {
    let (plan, src) = ramp_plan(500_000);
    let (device, mut harness, _control) = fake_device("underrun", 48_000, 2);
    let mut output = PreviewOutput::open(&device, plan, &src).unwrap();
    harness.accept_stream();
    output.set_playing(true).unwrap();
    let mut ticks = 0u64;
    tick_until_audio(&mut harness, &mut ticks);
    let mut total = ticks * 256;
    let gen_before = output.preview_clock_snapshot().generation;

    // One callback larger than the whole ring: the remainder is consumed silence, which
    // is an underrun no matter what the feeder does concurrently.
    let starved = anim_audio::output::PREVIEW_RING_FRAMES + 4_096;
    let cap = harness.tick(starved);
    total += starved as u64;
    assert!(
        is_silence(&cap[cap.len() - 8_192..]),
        "the tail of the starved callback must be silence"
    );

    assert!(
        wait_for(|| {
            let snap = output.preview_clock_snapshot();
            snap.discontinuity && snap.generation > gen_before
        }),
        "the feeder never promoted the underrun to a generation"
    );
    let snap = output.preview_clock_snapshot();
    assert_eq!(
        snap.timeline_sample().unwrap(),
        total as i64,
        "consumed silence still advances the timeline"
    );
    assert!(output.device_error().is_none());

    let mut recovered = false;
    for _ in 0..200 {
        if !is_silence(&paced_tick(&mut harness, 256)) {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "audio never recovered after the underrun");
}

#[test]
fn device_error_stops_audio_bumps_generation_and_leaves_editing_available() {
    let (plan, src) = ramp_plan(4_096);
    let (device, mut harness, _control) = fake_device("loss", 48_000, 2);
    let mut output = PreviewOutput::open(&device, plan, &src).unwrap();
    harness.accept_stream();
    output.set_playing(true).unwrap();
    let gen_before = output.preview_clock_snapshot().generation;

    harness.fire_error(ErrorKind::DeviceNotAvailable);
    assert!(
        wait_for(|| {
            let snap = output.preview_clock_snapshot();
            !snap.playing && snap.discontinuity && snap.generation > gen_before
        }),
        "the feeder never stopped audio after the device error"
    );
    assert!(matches!(
        output.device_error(),
        Some(AudioError::DeviceLoss { .. })
    ));
    assert!(is_silence(&harness.tick(512)));
    // Editing stays available (17.11); resuming on the dead device is refused.
    output.seek(SampleOffset::new(777)).unwrap();
    assert_eq!(output.preview_clock_snapshot().anchor_timeline_sample, 777);
    assert!(matches!(
        output.set_playing(true),
        Err(AudioError::DeviceLoss { .. })
    ));
}

#[test]
fn device_restart_replaces_the_stream_and_ring() {
    let (plan, src) = ramp_plan(200_000);
    let (device, mut harness, _control) = fake_device("restart", 48_000, 2);
    let mut output = PreviewOutput::open(&device, plan.clone(), &src).unwrap();
    harness.accept_stream();
    output.set_playing(true).unwrap();
    let mut ticks = 0u64;
    tick_until_audio(&mut harness, &mut ticks);
    harness.fire_error(ErrorKind::DeviceNotAvailable);
    assert!(wait_for(|| !output.preview_clock_snapshot().playing));
    drop(output);
    assert!(
        harness.stream_dropped(),
        "the old stream must be dropped on replacement"
    );

    let mut output = PreviewOutput::open(&device, plan, &src).unwrap();
    harness.accept_stream();
    assert_eq!(
        output.preview_clock_snapshot().generation,
        0,
        "a reopen is a fresh clock"
    );
    assert!(output.device_error().is_none());
    output.set_playing(true).unwrap();
    tick_until_audio(&mut harness, &mut ticks);
}

#[test]
fn callback_teardown_is_ignored_by_ownership() {
    let (plan, src) = ramp_plan(4_096);
    let (device, mut harness, _control) = fake_device("teardown", 48_000, 2);
    let mut output = PreviewOutput::open(&device, plan, &src).unwrap();
    harness.accept_stream();
    output.set_playing(true).unwrap();
    let mut ticks = 0u64;
    tick_until_audio(&mut harness, &mut ticks);
    drop(output);
    assert!(harness.stream_dropped());
    assert!(
        harness.tick(256).is_empty(),
        "no callback may run against a destroyed output"
    );
}

// ---------------------------------------------------------------------------
// Open refusals (spec 17.7/17.11)
// ---------------------------------------------------------------------------

#[test]
fn open_refuses_devices_without_a_canonical_output_configuration() {
    let (plan, src) = ramp_plan(1_024);
    let (device, _h, _c) = fake_device("cd-rate", 44_100, 2);
    assert!(matches!(
        PreviewOutput::open(&device, plan.clone(), &src),
        Err(AudioError::OutputStreamFailure { .. })
    ));
    let (device, _h, _c) = fake_device("mono", 48_000, 1);
    assert!(matches!(
        PreviewOutput::open(&device, plan.clone(), &src),
        Err(AudioError::OutputStreamFailure { .. })
    ));
    let (device, _h, control) = fake_device("busy", 48_000, 2);
    control.fail_builds.store(true, Ordering::Release);
    assert!(matches!(
        PreviewOutput::open(&device, plan, &src),
        Err(AudioError::OutputStreamFailure { .. })
    ));
}

#[test]
fn open_refuses_missing_and_changed_sources_before_any_stream_exists() {
    let (plan, _src) = ramp_plan(1_024);
    let (device, harness, _c) = fake_device("sources", 48_000, 2);
    let empty = TestPcmSource::new();
    assert!(matches!(
        PreviewOutput::open(&device, plan.clone(), &empty),
        Err(AudioError::MissingAsset { .. })
    ));
    let wrong = TestPcmSource::new().with(asset_id("ramp"), pcm(1, vec![0.0; 8]));
    assert!(matches!(
        PreviewOutput::open(&device, plan, &wrong),
        Err(AudioError::ChangedAsset { .. })
    ));
    assert!(
        harness.rx.try_recv().is_err(),
        "no stream may be built for a refused plan"
    );
}

// ---------------------------------------------------------------------------
// The isolation clause (spec 17.12)
// ---------------------------------------------------------------------------

#[test]
fn offline_output_is_identical_across_preview_states_and_devices() {
    let (plan, src) = ramp_plan(48_000);
    let baseline = render_offline(&plan, &src, plan.spec()).unwrap();
    let baseline_hash = pcm_hash(&baseline);
    assert_eq!(baseline.len(), 48_000 * 2);

    let render = || pcm_hash(&render_offline(&plan, &src, plan.spec()).unwrap());

    // Enabled and playing on device A.
    let (device_a, mut harness, _c) = fake_device("iso-a", 48_000, 2);
    let mut output = PreviewOutput::open(&device_a, plan.clone(), &src).unwrap();
    harness.accept_stream();
    output.set_playing(true).unwrap();
    let mut ticks = 0u64;
    tick_until_audio(&mut harness, &mut ticks);
    assert_eq!(
        render(),
        baseline_hash,
        "preview playing changed offline output"
    );

    // Paused.
    output.set_playing(false).unwrap();
    harness.tick(512);
    assert_eq!(
        render(),
        baseline_hash,
        "preview paused changed offline output"
    );

    // Seeked and scrubbing.
    output.seek(SampleOffset::new(9_000)).unwrap();
    output.set_rate(2.0).unwrap();
    output.set_playing(true).unwrap();
    harness.tick(512);
    assert_eq!(
        render(),
        baseline_hash,
        "preview scrubbing changed offline output"
    );
    output.set_rate(1.0).unwrap();
    tick_until_audio(&mut harness, &mut ticks);

    // Underrun.
    harness.tick(anim_audio::output::PREVIEW_RING_FRAMES + 4_096);
    assert!(wait_for(|| output.preview_clock_snapshot().discontinuity));
    assert_eq!(
        render(),
        baseline_hash,
        "a preview underrun changed offline output"
    );

    // Device loss.
    harness.fire_error(ErrorKind::DeviceNotAvailable);
    assert!(wait_for(|| !output.preview_clock_snapshot().playing));
    assert_eq!(
        render(),
        baseline_hash,
        "preview device loss changed offline output"
    );
    drop(output);
    assert_eq!(
        render(),
        baseline_hash,
        "dropping preview changed offline output"
    );

    // A different device: distinct identity and buffer capabilities.
    let (device_b, mut harness_b, _c) = fake_device_with_buffers("iso-b", 48_000, 2, 128, 2048);
    let mut output = PreviewOutput::open(&device_b, plan.clone(), &src).unwrap();
    harness_b.accept_stream();
    assert_ne!(output.device_id(), "custom:iso-a");
    output.set_playing(true).unwrap();
    tick_until_audio(&mut harness_b, &mut ticks);
    assert_eq!(
        render(),
        baseline_hash,
        "a different preview device changed offline output"
    );
    drop(output);

    // Disabled (no preview at all). The feature-off gate run is the compile-time leg.
    assert_eq!(render(), baseline_hash);
}

// ---------------------------------------------------------------------------
// Live smoke (DECISIONS.md #48 ruling 4): this machine's default WASAPI device
// ---------------------------------------------------------------------------

#[test]
fn live_default_device_plays_and_counts() {
    let (plan, src) = ramp_plan(96_000);
    let mut output = PreviewOutput::open_default(plan, &src)
        .expect("the reference machine has a default output device (WP00 media probe)");
    assert!(!output.device_name().is_empty());
    assert!(!output.device_id().is_empty());
    output.set_playing(true).unwrap();
    assert!(
        wait_for(|| output.preview_clock_snapshot().device_samples > 0),
        "the live device never consumed a sample"
    );
    assert!(
        output.device_error().is_none(),
        "{:?}",
        output.device_error()
    );
    // Drop joins the feeder and stops the stream.
}
