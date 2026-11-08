//! Offline audio validation (spec 17.12, offline half; DECISIONS.md #46 ruling 2).
//!
//! Every expected value here is computed independently of the code under test (risk 2): the
//! fade/loop/offset/rounding expectations are hand-derived integers, the frame-range counts
//! are hand-computed from the exact rate, and the canonical-order test crafts three gains
//! whose f64 sum differs between the canonical order and insertion order. The synthetic
//! [`TestPcmSource`] drives the mix logic without a filesystem; the real decode/resample and
//! compile-plan tests write actual WAVs and resolve them through a real store, so the pinned
//! symphonia/rubato path is exercised end to end. G7's offline half — exact counts and
//! pinned-stack PCM identity — is certified by this suite passing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; the production no-unwrap rule is enforced at the workspace level"
)]

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anim_assets::{
    hash_bytes, AssetKind, AssetManifest, AssetManifestEntry, AssetStore, ContentHash,
    CURRENT_IMPORTER_VERSION,
};
use anim_audio::{
    compile_plan, mix_range, render_offline, resample_to_48k, sample_range, write_wav, AudioClip,
    AudioError, CanonicalPcm, ChannelLayout, MixPlan, OfflineAudioSpec, PcmAssetSource,
    PlannedClip, SampleOffset, SampleRange, PCM_DOMAIN,
};
use anim_core::hash::CanonicalBytes;
use anim_core::id::{AssetId, Revision, SceneId, TrackId};
use anim_core::rational::Rational;
use anim_core::time::{FramePos, FrameRange, FrameRate};
use anim_scene::SceneSnapshot;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use proptest::prelude::*;
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixtures and independent helpers
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

fn r(num: i128, den: i128) -> Rational {
    Rational::new(num, den).unwrap()
}

/// A synthetic canonical-PCM source: hand-built samples, no filesystem. The mix logic is
/// exercised against known values rather than a decode whose own correctness is in question.
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

/// Build canonical PCM from interleaved samples, deriving the frame count.
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

/// A planned clip with every coordinate explicit, for the synthetic mix tests.
#[allow(
    clippy::too_many_arguments,
    reason = "a test fixture names every plan field"
)]
fn planned(
    id: &str,
    asset: AssetId,
    start: u64,
    len: u64,
    source_start: u64,
    source_len: u64,
    looped: bool,
    gain: f64,
    fade_in: u64,
    fade_out: u64,
) -> PlannedClip {
    PlannedClip {
        clip_id: track_id(id),
        asset_id: asset,
        timeline: SampleRange::new(SampleOffset::new(start), len),
        source_start: SampleOffset::new(source_start),
        source_len,
        looped,
        gain,
        fade_in_samples: fade_in,
        fade_out_samples: fade_out,
    }
}

fn spec_with_chunk(chunk: u32) -> OfflineAudioSpec {
    OfflineAudioSpec {
        chunk_samples: chunk,
        ..OfflineAudioSpec::default()
    }
}

fn spec_frames(start: i64, end: i64) -> OfflineAudioSpec {
    OfflineAudioSpec {
        frames: FrameRange::new(start, end).unwrap(),
        ..OfflineAudioSpec::default()
    }
}

fn plan_with(clips: Vec<PlannedClip>, start: u64, len: u64, chunk: u32) -> MixPlan {
    MixPlan::new(
        clips,
        SampleRange::new(SampleOffset::new(start), len),
        spec_with_chunk(chunk),
    )
}

/// The canonical PCM digest: domain tag, count, then the little-endian sample bytes. Built
/// from the published layout, used to compare two runs (determinism) rather than to derive
/// an expected mix value.
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

/// Write a real 16-bit WAV file, returning its path.
fn write_wav_file(dir: &Path, name: &str, channels: u16, rate: u32, samples: &[i16]) -> PathBuf {
    let path = dir.join(name);
    let spec = WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::new(File::create(&path).unwrap(), spec).unwrap();
    for &s in samples {
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();
    path
}

/// Import a WAV file into a real store + manifest as an Audio asset, returning the id.
fn import_wav(
    dir: &TempDir,
    label: &str,
    wav_path: &Path,
    manifest: &mut AssetManifest,
) -> (AssetStore, AssetId) {
    let bytes = std::fs::read(wav_path).unwrap();
    let hash = hash_bytes(&bytes);
    let store = AssetStore::init(dir.path()).unwrap();
    store.create_object_parent(&hash).unwrap();
    std::fs::write(store.object_path(&hash), &bytes).unwrap();
    let id = asset_id(label);
    let entry = AssetManifestEntry::new(
        id,
        hash,
        bytes.len() as u64,
        AssetKind::Audio,
        CURRENT_IMPORTER_VERSION,
        [],
        format!("{label}.wav"),
        None,
    )
    .unwrap();
    manifest.insert(entry);
    (store, id)
}

fn snapshot_with_clips(clips: Vec<AudioClip>) -> SceneSnapshot {
    SceneSnapshot {
        scene_id: SceneId::derive(&namespace(), b"scene"),
        revision: Revision(1),
        position: FramePos::at_frame(0),
        time: Rational::from_i128(0),
        nodes: Arc::new([]),
        cameras: Arc::new([]),
        audio_cues: Arc::new([]),
        simulation_inputs: Arc::new([]),
        events: Arc::new([]),
        audio_clips: Arc::from(clips),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "a test fixture names every authored field"
)]
fn authored(
    id: &str,
    asset: AssetId,
    start: FramePos,
    end: FramePos,
    source_offset: Rational,
    looped: bool,
    gain: Rational,
    fade_in: Rational,
    fade_out: Rational,
    mute: bool,
    channels: ChannelLayout,
) -> AudioClip {
    AudioClip::new(
        track_id(id),
        asset,
        start,
        end,
        source_offset,
        looped,
        gain,
        fade_in,
        fade_out,
        mute,
        channels,
    )
}

/// A 48 kHz mono WAV of `frames` zero samples, long enough for planning tests.
fn planning_asset(
    dir: &TempDir,
    manifest: &mut AssetManifest,
    frames: usize,
) -> (AssetStore, AssetId) {
    let path = write_wav_file(dir.path(), "plan.wav", 1, 48_000, &vec![0i16; frames]);
    import_wav(dir, "plan-asset", &path, manifest)
}

// ---------------------------------------------------------------------------
// 1. Frame-range conversion (integer + NTSC), spec 17.12
// ---------------------------------------------------------------------------

#[test]
fn integer_and_ntsc_frame_ranges_convert_to_exact_sample_counts() {
    // 24 fps: one frame is exactly 2000 samples at 48 kHz.
    let fps24 = FrameRate::new(24, 1).unwrap();
    let r = sample_range(FrameRange::new(0, 100).unwrap(), fps24).unwrap();
    assert_eq!(r.start.get(), 0);
    assert_eq!(r.len, 200_000);

    // 30000/1001: 100 frames is 100*1001/30000 s = 160160 samples exactly. A decimal
    // 29.97 would have said 160006 — the drift WP01's exact arithmetic exists to prevent.
    let ntsc = FrameRate::new(30_000, 1001).unwrap();
    let r = sample_range(FrameRange::new(0, 100).unwrap(), ntsc).unwrap();
    assert_eq!(r.len, 160_160);
    // A nonzero start: floor(100*48000*1001/30000) = 160160.
    let r = sample_range(FrameRange::new(100, 200).unwrap(), ntsc).unwrap();
    assert_eq!(r.start.get(), 160_160);
    assert_eq!(r.len, 160_160);
}

#[test]
fn frame_range_refuses_empty_negative_reversed_zero_rate_and_overflow() {
    let fps = FrameRate::new(24, 1).unwrap();
    assert!(matches!(
        sample_range(FrameRange::new(3, 3).unwrap(), fps),
        Err(AudioError::EmptyRange { .. })
    ));
    assert!(matches!(
        sample_range(FrameRange::new(-1, 5).unwrap(), fps),
        Err(AudioError::NegativeRange { .. })
    ));
    assert!(matches!(
        sample_range(FrameRange { start: 8, end: 2 }, fps),
        Err(AudioError::Core(_))
    ));
    assert!(matches!(
        sample_range(FrameRange::new(0, 5).unwrap(), FrameRate { num: 0, den: 1 }),
        Err(AudioError::InvalidRate { .. })
    ));
    assert!(matches!(
        sample_range(FrameRange::new(0, i64::MAX).unwrap(), fps),
        Err(AudioError::Overflow { .. })
    ));
}

// ---------------------------------------------------------------------------
// 2. Mix: hand-computed fades, loops, offsets, channel conversion, rounding
// ---------------------------------------------------------------------------

/// Mix one clip over its whole timeline and return the interleaved i16 output.
fn mix_one(clip: PlannedClip, source: TestPcmSource, len: u64) -> Vec<i16> {
    let start = clip.timeline.start.0;
    let plan = plan_with(vec![clip], start, len, 1024);
    let mut out = vec![0i16; (len * 2) as usize];
    mix_range(
        &plan,
        &source,
        SampleRange::new(SampleOffset::new(start), len),
        &mut out,
    )
    .unwrap();
    out
}

#[test]
fn linear_fade_in_matches_hand_computed_ramp() {
    // Stereo constant source [1,1], gain 1, fade_in 4 samples, length 8. The fade factor is
    // pos/4 for pos<4 else 1; expected i16 = round(factor*32767), computed by hand:
    //   pos0: 0      -> 0
    //   pos1: 0.25   -> 8191.75  -> 8192
    //   pos2: 0.5    -> 16383.5  -> 16384
    //   pos3: 0.75   -> 24575.25 -> 24575
    //   pos4..7: 1   -> 32767
    let asset = asset_id("fade");
    let source = TestPcmSource::new().with(asset, pcm(2, vec![1.0, 1.0]));
    // Looped over a one-frame constant source so every position reads [1,1]; the fade is
    // what varies, not the source (a nonlooping one-frame source would exhaust after pos 0).
    let clip = planned("c", asset, 0, 8, 0, 1, true, 1.0, 4, 0);
    let out = mix_one(clip, source, 8);
    let expected = [0, 8192, 16384, 24575, 32767, 32767, 32767, 32767];
    for (i, &e) in expected.iter().enumerate() {
        assert_eq!(out[i * 2], e, "left frame {i}");
        assert_eq!(out[i * 2 + 1], e, "right frame {i}");
    }
}

#[test]
fn linear_fade_out_matches_hand_computed_ramp() {
    // fade_out 4, length 8: factor = (8-pos)/4 for pos>=4 else 1.
    //   pos0..4: 1 -> 32767 (pos4 gives (8-4)/4 = 1)
    //   pos5: 0.75 -> 24575 ; pos6: 0.5 -> 16384 ; pos7: 0.25 -> 8192
    let asset = asset_id("fadeout");
    let source = TestPcmSource::new().with(asset, pcm(2, vec![1.0, 1.0]));
    let clip = planned("c", asset, 0, 8, 0, 1, true, 1.0, 0, 4);
    let out = mix_one(clip, source, 8);
    let expected = [32767, 32767, 32767, 32767, 32767, 24575, 16384, 8192];
    for (i, &e) in expected.iter().enumerate() {
        assert_eq!(out[i * 2], e, "frame {i}");
    }
}

#[test]
fn loop_modulo_and_mono_to_stereo_upmix() {
    // Mono source [1.0, 0.5, 0.25] (all f32-exact), looped, length 9. source_pos = pos%3.
    // Hand values: 1.0->32767, 0.5->16383.5->16384, 0.25->8191.75->8192. Mono up-mixed so
    // both channels equal. A non-looping reading would fall silent after frame 3.
    let asset = asset_id("loop");
    let source = TestPcmSource::new().with(asset, pcm(1, vec![1.0, 0.5, 0.25]));
    let clip = planned("c", asset, 0, 9, 0, 3, true, 1.0, 0, 0);
    let out = mix_one(clip, source, 9);
    let expected = [32767, 16384, 8192, 32767, 16384, 8192, 32767, 16384, 8192];
    for (i, &e) in expected.iter().enumerate() {
        assert_eq!(out[i * 2], e, "left frame {i}");
        assert_eq!(out[i * 2 + 1], e, "right frame {i} (mono up-mix)");
    }
}

#[test]
fn source_offset_and_exhausted_nonlooping_silence() {
    // Mono source [0,0.25,0.5,0.75,1.0], source_start 2, length 5. pos maps to 2+pos:
    //   pos0->0.5->16384, pos1->0.75->24575, pos2->1.0->32767, pos3->5 (exhausted)->0,
    //   pos4->6 (exhausted)->0. Exhaustion contributes silence, not a wrap.
    let asset = asset_id("offset");
    let source = TestPcmSource::new().with(asset, pcm(1, vec![0.0, 0.25, 0.5, 0.75, 1.0]));
    let clip = planned("c", asset, 0, 5, 2, 5, false, 1.0, 0, 0);
    let out = mix_one(clip, source, 5);
    let expected = [16384, 24575, 32767, 0, 0];
    for (i, &e) in expected.iter().enumerate() {
        assert_eq!(out[i * 2], e, "frame {i}");
    }
}

#[test]
fn stereo_channels_are_preserved_not_summed() {
    // Distinct L/R proves the mixer preserves stereo rather than folding to mono.
    let asset = asset_id("stereo");
    let source = TestPcmSource::new().with(asset, pcm(2, vec![1.0, 0.0, 0.5, -0.5]));
    let clip = planned("c", asset, 0, 2, 0, 2, false, 1.0, 0, 0);
    let out = mix_one(clip, source, 2);
    assert_eq!(out, vec![32767, 0, 16384, -16384]);
}

#[test]
fn symmetric_rounding_and_full_scale() {
    // Hand values: 0.5*32767 = 16383.5 -> 16384 (half away from zero); -0.5 -> -16384
    // (symmetric, not -16383 truncation); 1.0 -> 32767; -1.0 -> -32767 (symmetric full
    // scale, NOT the asymmetric -32768).
    let asset = asset_id("round");
    let source = TestPcmSource::new().with(asset, pcm(2, vec![0.5, -0.5, 1.0, -1.0, 0.0, 0.0]));
    let clip = planned("c", asset, 0, 3, 0, 3, false, 1.0, 0, 0);
    let out = mix_one(clip, source, 3);
    assert_eq!(out, vec![16384, -16384, 32767, -32767, 0, 0]);
}

#[test]
fn sum_above_full_scale_clamps() {
    // Two clips each [1,1] gain 1 sum to 2.0 -> clamp 1.0 -> 32767; negatives -> -32767.
    let a = asset_id("clampA");
    let b = asset_id("clampB");
    let source = TestPcmSource::new()
        .with(a, pcm(2, vec![1.0, 1.0]))
        .with(b, pcm(2, vec![1.0, 1.0]));
    let clips = vec![
        planned("a", a, 0, 1, 0, 1, false, 1.0, 0, 0),
        planned("b", b, 0, 1, 0, 1, false, 1.0, 0, 0),
    ];
    let plan = plan_with(clips, 0, 1, 1024);
    let mut out = vec![0i16; 2];
    mix_range(
        &plan,
        &source,
        SampleRange::new(SampleOffset::new(0), 1),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, vec![32767, 32767]);

    let neg = TestPcmSource::new()
        .with(a, pcm(2, vec![-1.0, -1.0]))
        .with(b, pcm(2, vec![-1.0, -1.0]));
    let clips = vec![
        planned("a", a, 0, 1, 0, 1, false, 1.0, 0, 0),
        planned("b", b, 0, 1, 0, 1, false, 1.0, 0, 0),
    ];
    let plan = plan_with(clips, 0, 1, 1024);
    mix_range(
        &plan,
        &neg,
        SampleRange::new(SampleOffset::new(0), 1),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, vec![-32767, -32767]);
}

#[test]
fn canonical_accumulation_order_changes_the_f64_sum() {
    // Three terms whose f64 sum is order-sensitive: 1e16 + (-1e16) + 1.0. In canonical order
    // (by timeline start A=0, B=1, C=2) the sum is ((1e16 - 1e16) + 1) = 1 -> 32767. In the
    // insertion order C, B, A it is ((1 - 1e16) + 1e16) = 0 -> 0, because 1 is lost against
    // 1e16's ulp of 2. The clips are inserted C, B, A; MixPlan::new must sort them so the
    // output is 32767. A mixer that iterated insertion order, or a plan that did not sort,
    // would emit 0 and fail this test.
    let a = asset_id("ordA");
    let b = asset_id("ordB");
    let c = asset_id("ordC");
    let source = TestPcmSource::new()
        .with(a, pcm(1, vec![1.0, 1.0, 1.0]))
        .with(b, pcm(1, vec![1.0, 1.0]))
        .with(c, pcm(1, vec![1.0]));
    let clips = vec![
        planned("C", c, 2, 1, 0, 1, false, 1.0, 0, 0),
        planned("B", b, 1, 2, 0, 2, false, -1e16, 0, 0),
        planned("A", a, 0, 3, 0, 3, false, 1e16, 0, 0),
    ];
    let plan = plan_with(clips, 2, 1, 1024);
    // The plan is sorted by start: A(0), B(1), C(2).
    assert_eq!(plan.clips()[0].clip_id, track_id("A"));
    assert_eq!(plan.clips()[1].clip_id, track_id("B"));
    assert_eq!(plan.clips()[2].clip_id, track_id("C"));
    let mut out = vec![0i16; 2];
    mix_range(
        &plan,
        &source,
        SampleRange::new(SampleOffset::new(2), 1),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, vec![32767, 32767]);
}

#[test]
fn plan_sorts_by_start_then_clip_id() {
    let asset = asset_id("sort");
    // Two clips share start 5; the tie-break is clip_id byte order. Insert reversed.
    let id_low = track_id("tie-a");
    let id_high = track_id("tie-b");
    let (first, second) = if id_low < id_high {
        ("tie-a", "tie-b")
    } else {
        ("tie-b", "tie-a")
    };
    let clips = vec![
        planned("later", asset, 9, 1, 0, 1, false, 1.0, 0, 0),
        planned(second, asset, 5, 1, 0, 1, false, 1.0, 0, 0),
        planned(first, asset, 5, 1, 0, 1, false, 1.0, 0, 0),
        planned("earlier", asset, 1, 1, 0, 1, false, 1.0, 0, 0),
    ];
    let plan = plan_with(clips, 0, 10, 1024);
    let starts: Vec<u64> = plan.clips().iter().map(|c| c.timeline.start.0).collect();
    assert_eq!(starts, vec![1, 5, 5, 9]);
    assert_eq!(plan.clips()[1].clip_id, track_id(first));
    assert_eq!(plan.clips()[2].clip_id, track_id(second));
    assert!(plan.clips()[1].clip_id < plan.clips()[2].clip_id);
}

#[test]
fn mix_refuses_wrong_buffer_length_changed_source_and_unsupported_layout() {
    let asset = asset_id("guard");
    let source = TestPcmSource::new().with(asset, pcm(1, vec![0.0; 4]));
    let clip = planned("c", asset, 0, 4, 0, 4, false, 1.0, 0, 0);
    let plan = plan_with(vec![clip], 0, 4, 1024);
    let mut wrong = vec![0i16; 7]; // should be 8
    assert!(matches!(
        mix_range(
            &plan,
            &source,
            SampleRange::new(SampleOffset::new(0), 4),
            &mut wrong
        ),
        Err(AudioError::InvalidSpec { .. })
    ));

    // A source whose canonical length disagrees with the plan's source_len.
    let changed = TestPcmSource::new().with(asset, pcm(1, vec![0.0; 2]));
    let clip = planned("c", asset, 0, 4, 0, 4, false, 1.0, 0, 0);
    let plan = plan_with(vec![clip], 0, 4, 1024);
    let mut out = vec![0i16; 8];
    assert!(matches!(
        mix_range(
            &plan,
            &changed,
            SampleRange::new(SampleOffset::new(0), 4),
            &mut out
        ),
        Err(AudioError::ChangedAsset { .. })
    ));

    // A three-channel canonical source has no conversion rule.
    let three = TestPcmSource::new().with(asset, pcm(3, vec![0.0, 0.0, 0.0]));
    let clip = planned("c", asset, 0, 1, 0, 1, false, 1.0, 0, 0);
    let plan = plan_with(vec![clip], 0, 1, 1024);
    let mut out = vec![0i16; 2];
    assert!(matches!(
        mix_range(
            &plan,
            &three,
            SampleRange::new(SampleOffset::new(0), 1),
            &mut out
        ),
        Err(AudioError::UnsupportedLayout { channels: 3 })
    ));
}

// ---------------------------------------------------------------------------
// 3. Chunk-size invariance and determinism (10,000 frames), spec 17.12
// ---------------------------------------------------------------------------

/// A two-clip plan over 10,000 frames: a mono loop and a stereo loop with fades. Small
/// looped sources keep per-case setup cheap for the property test.
fn invariance_fixture() -> (MixPlan, TestPcmSource) {
    let mono = asset_id("inv-mono");
    let stereo = asset_id("inv-stereo");
    let source = TestPcmSource::new()
        .with(mono, pcm(1, vec![0.25, -0.25, 0.5, -0.5]))
        .with(stereo, pcm(2, vec![0.5, -0.5]));
    let clips = vec![
        planned("mono", mono, 0, 10_000, 0, 4, true, 1.0, 0, 0),
        planned("stereo", stereo, 0, 10_000, 0, 1, true, 1.0, 500, 500),
    ];
    let plan = plan_with(clips, 0, 10_000, 1024);
    (plan, source)
}

#[test]
fn chunk_sizes_1_7_1024_4096_produce_identical_pcm() {
    let (plan, source) = invariance_fixture();
    let reference = render_offline(&plan, &source, &spec_with_chunk(1024)).unwrap();
    assert_eq!(reference.len(), 20_000);
    for chunk in [1u32, 7, 4096] {
        let got = render_offline(&plan, &source, &spec_with_chunk(chunk)).unwrap();
        assert_eq!(got, reference, "chunk {chunk} changed the PCM");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn any_chunk_size_matches_the_reference(chunk in 1u32..4096) {
        let (plan, source) = invariance_fixture();
        let reference = render_offline(&plan, &source, &spec_with_chunk(1024)).unwrap();
        let got = render_offline(&plan, &source, &spec_with_chunk(chunk)).unwrap();
        prop_assert_eq!(got, reference);
    }
}

#[test]
fn mixing_10000_frames_twice_has_identical_hash_and_count() {
    let (plan, source) = invariance_fixture();
    let a = render_offline(&plan, &source, &spec_with_chunk(1024)).unwrap();
    let b = render_offline(&plan, &source, &spec_with_chunk(1024)).unwrap();
    assert_eq!(a.len(), 20_000);
    assert_eq!(a.len(), b.len());
    assert_eq!(pcm_hash(&a), pcm_hash(&b));
    // The hash is over the actual bytes, so it is not a tautology: a one-sample change moves
    // it. Verify the digest reacts to a flipped sample.
    let mut mutated = a.clone();
    mutated[1234] ^= 1;
    assert_ne!(pcm_hash(&a), pcm_hash(&mutated));
}

// ---------------------------------------------------------------------------
// 4. WAV writing returns frames and round-trips, spec 17.9
// ---------------------------------------------------------------------------

#[test]
fn write_wav_returns_frame_count_and_decodes_back_to_the_same_pcm() {
    let dir = TempDir::new().unwrap();
    let (plan, source) = invariance_fixture();
    let range = SampleRange::new(SampleOffset::new(0), 10_000);
    let path = dir.path().join("out.wav");
    let frames = write_wav(&plan, &source, range, File::create(&path).unwrap()).unwrap();
    // Frames, not interleaved scalars: 10000, not 20000.
    assert_eq!(frames, 10_000);

    let mut reader = WavReader::new(File::open(&path).unwrap()).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.sample_rate, 48_000);
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.sample_format, SampleFormat::Int);
    assert_eq!(u64::from(reader.duration()), 10_000);
    let read: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    let expected = render_offline(&plan, &source, &spec_with_chunk(1024)).unwrap();
    assert_eq!(read, expected);
}

#[test]
fn write_wav_refuses_a_zero_chunk_plan_instead_of_hanging() {
    // A public MixPlan::new admits an unvalidated spec; a zero chunk would advance the write
    // loop by zero forever. The guard must report InvalidSpec before any sample is written.
    let dir = TempDir::new().unwrap();
    let (plan, source) = invariance_fixture();
    let zero = MixPlan::new(plan.clips().to_vec(), plan.range(), spec_with_chunk(0));
    let path = dir.path().join("zero.wav");
    assert!(matches!(
        write_wav(&zero, &source, zero.range(), File::create(&path).unwrap()),
        Err(AudioError::InvalidSpec { .. })
    ));
}

// ---------------------------------------------------------------------------
// 5. compile_plan against a real store: selection, sort, offset, mute, loop, missing
// ---------------------------------------------------------------------------

#[test]
fn compile_plan_selects_intersecting_clips_and_clips_the_source_offset() {
    let dir = TempDir::new().unwrap();
    let mut manifest = AssetManifest::new();
    let (store, asset) = planning_asset(&dir, &mut manifest, 48_000);
    let resolver = anim_audio::AssetResolver::new(&store, &manifest);
    let fps = FrameRate::new(24, 1).unwrap();
    let spec = spec_frames(2, 8); // samples [4000, 16000)

    let z = Rational::from_i128(0);
    let one = Rational::from_i128(1);
    let clips = vec![
        // wholly before the range: [0,4000) — excluded (half-open).
        authored(
            "left-out",
            asset,
            FramePos::at_frame(0),
            FramePos::at_frame(2),
            z,
            false,
            one,
            z,
            z,
            false,
            ChannelLayout::Mono,
        ),
        // wholly after: [16000,20000) — excluded.
        authored(
            "right-out",
            asset,
            FramePos::at_frame(8),
            FramePos::at_frame(10),
            z,
            false,
            one,
            z,
            z,
            false,
            ChannelLayout::Mono,
        ),
        // inside: [6000,10000).
        authored(
            "inside",
            asset,
            FramePos::at_frame(3),
            FramePos::at_frame(5),
            z,
            false,
            one,
            z,
            z,
            false,
            ChannelLayout::Mono,
        ),
        // overlaps the left edge: [2000,8000) clipped to [4000,8000), advance 2000.
        authored(
            "overlap-left",
            asset,
            FramePos::at_frame(1),
            FramePos::at_frame(4),
            z,
            false,
            one,
            z,
            z,
            false,
            ChannelLayout::Mono,
        ),
        // overlaps the right edge: [14000,18000) clipped to [14000,16000).
        authored(
            "overlap-right",
            asset,
            FramePos::at_frame(7),
            FramePos::at_frame(9),
            z,
            false,
            one,
            z,
            z,
            false,
            ChannelLayout::Mono,
        ),
    ];
    let scene = snapshot_with_clips(clips);
    let plan = compile_plan(&scene, &resolver, &spec, fps).unwrap();

    let ids: Vec<String> = plan
        .clips()
        .iter()
        .map(|c| c.clip_id.to_canonical_string())
        .collect();
    // All three starts are distinct (4000, 6000, 14000), so the order is purely by start.
    let expected = vec![
        track_id("overlap-left").to_canonical_string(),
        track_id("inside").to_canonical_string(),
        track_id("overlap-right").to_canonical_string(),
    ];
    assert_eq!(ids, expected);
    assert_eq!(plan.clips().len(), 3);

    let left = &plan.clips()[0];
    assert_eq!(left.timeline.start.get(), 4000);
    assert_eq!(left.timeline.len, 4000);
    // source_offset 0 advanced by the 2000 samples clipped off the front.
    assert_eq!(left.source_start.get(), 2000);
    assert_eq!(left.source_len, 48_000);
    assert_eq!(plan.range().start.get(), 4000);
    assert_eq!(plan.range().len, 12_000);
}

#[test]
fn compile_plan_places_subframe_bounds_and_clips_ending_between_frames() {
    let dir = TempDir::new().unwrap();
    let mut manifest = AssetManifest::new();
    let (store, asset) = planning_asset(&dir, &mut manifest, 48_000);
    let resolver = anim_audio::AssetResolver::new(&store, &manifest);
    let fps = FrameRate::new(24, 1).unwrap();
    let spec = spec_frames(0, 10);
    let z = Rational::from_i128(0);
    let one = Rational::from_i128(1);
    // start = (1 + 1/2)/24 s = 1/16 s -> 3000 samples; end = (2 + 1/4)/24 = 3/32 s -> 4500.
    let clip = authored(
        "subframe",
        asset,
        FramePos::new(1, r(1, 2)).unwrap(),
        FramePos::new(2, r(1, 4)).unwrap(),
        z,
        false,
        one,
        z,
        z,
        false,
        ChannelLayout::Mono,
    );
    let scene = snapshot_with_clips(vec![clip]);
    let plan = compile_plan(&scene, &resolver, &spec, fps).unwrap();
    assert_eq!(plan.clips().len(), 1);
    assert_eq!(plan.clips()[0].timeline.start.get(), 3000);
    assert_eq!(plan.clips()[0].timeline.len, 1500);
}

#[test]
fn compile_plan_mutes_to_zero_gain_and_converts_exact_gain() {
    let dir = TempDir::new().unwrap();
    let mut manifest = AssetManifest::new();
    let (store, asset) = planning_asset(&dir, &mut manifest, 48_000);
    let resolver = anim_audio::AssetResolver::new(&store, &manifest);
    let fps = FrameRate::new(24, 1).unwrap();
    let spec = spec_frames(0, 10);
    let z = Rational::from_i128(0);
    let clips = vec![
        authored(
            "muted",
            asset,
            FramePos::at_frame(0),
            FramePos::at_frame(2),
            z,
            false,
            r(3, 1),
            z,
            z,
            true,
            ChannelLayout::Mono,
        ),
        authored(
            "loud",
            asset,
            FramePos::at_frame(3),
            FramePos::at_frame(5),
            z,
            false,
            r(3, 2),
            z,
            z,
            false,
            ChannelLayout::Mono,
        ),
    ];
    let scene = snapshot_with_clips(clips);
    let plan = compile_plan(&scene, &resolver, &spec, fps).unwrap();
    let muted = plan
        .clips()
        .iter()
        .find(|c| c.clip_id == track_id("muted"))
        .unwrap();
    let loud = plan
        .clips()
        .iter()
        .find(|c| c.clip_id == track_id("loud"))
        .unwrap();
    assert_eq!(muted.gain, 0.0);
    assert_eq!(loud.gain, 1.5);
}

#[test]
fn compile_plan_refuses_a_loop_over_an_empty_source() {
    let dir = TempDir::new().unwrap();
    let mut manifest = AssetManifest::new();
    // A zero-frame WAV decodes to an empty canonical source.
    let path = write_wav_file(dir.path(), "empty.wav", 1, 48_000, &[]);
    let (store, asset) = import_wav(&dir, "empty", &path, &mut manifest);
    let resolver = anim_audio::AssetResolver::new(&store, &manifest);
    let fps = FrameRate::new(24, 1).unwrap();
    let spec = spec_frames(0, 10);
    let z = Rational::from_i128(0);
    let one = Rational::from_i128(1);
    let clip = authored(
        "loop",
        asset,
        FramePos::at_frame(0),
        FramePos::at_frame(2),
        z,
        true,
        one,
        z,
        z,
        false,
        ChannelLayout::Mono,
    );
    let scene = snapshot_with_clips(vec![clip]);
    assert!(matches!(
        compile_plan(&scene, &resolver, &spec, fps),
        Err(AudioError::InvalidLoop { .. })
    ));
}

#[test]
fn compile_plan_fails_a_missing_source_before_rendering() {
    let dir = TempDir::new().unwrap();
    let mut manifest = AssetManifest::new();
    let (store, _asset) = planning_asset(&dir, &mut manifest, 48_000);
    let resolver = anim_audio::AssetResolver::new(&store, &manifest);
    let fps = FrameRate::new(24, 1).unwrap();
    let spec = spec_frames(0, 10);
    let z = Rational::from_i128(0);
    let one = Rational::from_i128(1);
    let clip = authored(
        "ghost",
        asset_id("not-in-manifest"),
        FramePos::at_frame(0),
        FramePos::at_frame(2),
        z,
        false,
        one,
        z,
        z,
        false,
        ChannelLayout::Mono,
    );
    let scene = snapshot_with_clips(vec![clip]);
    assert!(matches!(
        compile_plan(&scene, &resolver, &spec, fps),
        Err(AudioError::MissingAsset { .. })
    ));
}

#[test]
fn compile_plan_refuses_a_reversed_clip_and_a_negative_offset() {
    let dir = TempDir::new().unwrap();
    let mut manifest = AssetManifest::new();
    let (store, asset) = planning_asset(&dir, &mut manifest, 48_000);
    let resolver = anim_audio::AssetResolver::new(&store, &manifest);
    let fps = FrameRate::new(24, 1).unwrap();
    let spec = spec_frames(0, 10);
    let z = Rational::from_i128(0);
    let one = Rational::from_i128(1);

    // Reversed timeline (hand-built; the authored constructor does not gate it).
    let reversed = authored(
        "rev",
        asset,
        FramePos::at_frame(5),
        FramePos::at_frame(2),
        z,
        false,
        one,
        z,
        z,
        false,
        ChannelLayout::Mono,
    );
    let scene = snapshot_with_clips(vec![reversed]);
    assert!(matches!(
        compile_plan(&scene, &resolver, &spec, fps),
        Err(AudioError::MalformedClip { .. })
    ));

    // A negative source offset is malformed (the authored validator gates it, but a
    // hand-built snapshot can carry one, and compile_plan must refuse it before rendering).
    let negative = authored(
        "neg",
        asset,
        FramePos::at_frame(0),
        FramePos::at_frame(2),
        r(-1, 2),
        false,
        one,
        z,
        z,
        false,
        ChannelLayout::Mono,
    );
    let scene = snapshot_with_clips(vec![negative]);
    assert!(matches!(
        compile_plan(&scene, &resolver, &spec, fps),
        Err(AudioError::MalformedClip { .. })
    ));
}

// ---------------------------------------------------------------------------
// 6. Real decode + resample determinism and exact count, spec 17.12
// ---------------------------------------------------------------------------

#[test]
fn real_decode_and_resample_are_deterministic_with_exact_count() {
    let dir = TempDir::new().unwrap();
    // A known half-scale ramp at 44100 Hz, 1000 frames.
    let samples: Vec<i16> = (0..1000)
        .map(|i| ((i as f64 / 1000.0) * 16_383.0) as i16)
        .collect();
    let path = write_wav_file(dir.path(), "ramp.wav", 1, 44_100, &samples);
    let hash = hash_bytes(&std::fs::read(&path).unwrap());

    let a = anim_audio::decode_source(&path, hash).unwrap();
    let b = anim_audio::decode_source(&path, hash).unwrap();
    assert_eq!(a.frames, 1000, "decoded frame count is authoritative");
    assert_eq!(a.rate, 44_100);
    assert_eq!(a.channels, 1);
    assert_eq!(a, b);

    let ca = resample_to_48k(a, hash).unwrap();
    let cb = resample_to_48k(b, hash).unwrap();
    // ceil(1000*48000/44100) = ceil(1088.435...) = 1089, by exact integer arithmetic
    // (std div_ceil, independent of the production floor-plus-remainder formula).
    let expected_frames = (1000u64 * 48_000).div_ceil(44_100);
    assert_eq!(expected_frames, 1089);
    assert_eq!(ca.frames(), expected_frames);
    assert_eq!(ca.samples().len(), expected_frames as usize);
    assert_eq!(ca.samples(), cb.samples());
    assert_eq!(ca.content_hash(), hash);
}

#[test]
fn end_to_end_real_asset_plan_render_and_wav_count() {
    let dir = TempDir::new().unwrap();
    let mut manifest = AssetManifest::new();
    // A 44100 Hz stereo source, 4410 frames (0.1 s), a modest ramp.
    let mut samples = Vec::new();
    for i in 0..4410 {
        let v = ((i as f64 / 4410.0) * 8000.0) as i16;
        samples.push(v);
        samples.push(-v);
    }
    let path = write_wav_file(dir.path(), "score.wav", 2, 44_100, &samples);
    let (store, asset) = import_wav(&dir, "score", &path, &mut manifest);
    let resolver = anim_audio::AssetResolver::new(&store, &manifest);
    let fps = FrameRate::new(24, 1).unwrap();
    let spec = spec_frames(0, 12); // 12 frames at 24 fps = 24000 samples
    let z = Rational::from_i128(0);
    let clip = authored(
        "music",
        asset,
        FramePos::at_frame(0),
        FramePos::at_frame(12),
        z,
        false,
        Rational::from_i128(1),
        z,
        z,
        false,
        ChannelLayout::Stereo,
    );
    let scene = snapshot_with_clips(vec![clip]);
    let plan = compile_plan(&scene, &resolver, &spec, fps).unwrap();
    assert_eq!(plan.range().len, 24_000);
    // The canonical source is ceil(4410*48000/44100) = 4800 frames.
    assert_eq!(plan.clips()[0].source_len, 4800);

    let pcm = render_offline(&plan, &resolver, &spec).unwrap();
    assert_eq!(pcm.len(), 48_000);
    // Determinism across a second render through the real resolver.
    let again = render_offline(&plan, &resolver, &spec).unwrap();
    assert_eq!(pcm_hash(&pcm), pcm_hash(&again));

    let out_path = dir.path().join("export.wav");
    let frames = write_wav(
        &plan,
        &resolver,
        plan.range(),
        File::create(&out_path).unwrap(),
    )
    .unwrap();
    assert_eq!(frames, 24_000);
    let mut reader = WavReader::new(File::open(&out_path).unwrap()).unwrap();
    assert_eq!(u64::from(reader.duration()), 24_000);
    let read: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    assert_eq!(read, pcm);
}

#[test]
fn a_changed_asset_is_refused_by_content_verification() {
    let dir = TempDir::new().unwrap();
    let mut manifest = AssetManifest::new();
    let path = write_wav_file(dir.path(), "tone.wav", 1, 48_000, &vec![1000i16; 4800]);
    let (store, asset) = import_wav(&dir, "tone", &path, &mut manifest);
    // Corrupt the stored object without changing its length: only Content verification sees it.
    let entry = manifest.get(&asset).unwrap();
    let obj = store.object_path(&entry.content_hash);
    let mut bytes = std::fs::read(&obj).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&obj, &bytes).unwrap();

    let resolver = anim_audio::AssetResolver::new(&store, &manifest);
    let fps = FrameRate::new(24, 1).unwrap();
    let spec = spec_frames(0, 10);
    let z = Rational::from_i128(0);
    let one = Rational::from_i128(1);
    let clip = authored(
        "tone",
        asset,
        FramePos::at_frame(0),
        FramePos::at_frame(2),
        z,
        false,
        one,
        z,
        z,
        false,
        ChannelLayout::Mono,
    );
    let scene = snapshot_with_clips(vec![clip]);
    assert!(matches!(
        compile_plan(&scene, &resolver, &spec, fps),
        Err(AudioError::ChangedAsset { .. })
    ));
}

// ---------------------------------------------------------------------------
// 7. Spec validation
// ---------------------------------------------------------------------------

#[test]
fn offline_spec_validation_gates_rate_channels_chunk_version_and_range() {
    let base = OfflineAudioSpec {
        frames: FrameRange::new(0, 10).unwrap(),
        ..OfflineAudioSpec::default()
    };
    base.validate().unwrap();

    let mut bad_rate = base;
    bad_rate.sample_rate = 44_100;
    assert!(matches!(
        bad_rate.validate(),
        Err(AudioError::InvalidRate { .. })
    ));

    let mut bad_channels = base;
    bad_channels.channels = 1;
    assert!(matches!(
        bad_channels.validate(),
        Err(AudioError::InvalidSpec { .. })
    ));

    let mut bad_chunk = base;
    bad_chunk.chunk_samples = 0;
    assert!(matches!(
        bad_chunk.validate(),
        Err(AudioError::InvalidSpec { .. })
    ));

    let mut bad_version = base;
    bad_version.algorithm_version = 99;
    assert!(matches!(
        bad_version.validate(),
        Err(AudioError::InvalidSpec { .. })
    ));

    let mut empty = base;
    empty.frames = FrameRange::new(0, 0).unwrap();
    assert!(matches!(
        empty.validate(),
        Err(AudioError::EmptyRange { .. })
    ));

    let mut negative = base;
    negative.frames = FrameRange::new(-2, 5).unwrap();
    assert!(matches!(
        negative.validate(),
        Err(AudioError::NegativeRange { .. })
    ));
}
