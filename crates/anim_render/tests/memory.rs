//! WP07's GPU validation (spec 16.12): exact reload, resize stress, scrubbing, device
//! loss and reservation rollback on the pinned adapter.
//!
//! The oracle rule is the golden suite's: no tolerance, and no expected value produced
//! by calling the code under test. The reload clause is render-against-render — an
//! evicted texture re-uploaded from the same decoded bytes must produce a byte-identical
//! image — and the accounting clauses are structural: record counts, labels and byte
//! totals the spec's formulas name.
//!
//! Like `golden.rs`, this suite opens a real device through `GpuContext::create`, so
//! #18's guards run on every test and another machine refuses rather than skips (9.12).
//! The file itself is a deviation in the class of rows 10/13/16: 16.3 lists no render
//! test file, and 16.12's GPU clauses need one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a fixture that cannot be built makes the scenario unreadable, which is a panic rather than a test outcome; the workspace denial still governs every production crate"
)]

use std::sync::Arc;

use anim_assets::{
    AssetKind, AssetManifest, AssetManifestEntry, ContentHash, DecoderConfiguration, ImageAlpha,
    ImageAssetClaims, ImageColorSpace,
};
use anim_core::id::{AssetId, NodeId, Revision, SceneId};
use anim_core::rational::Rational;
use anim_core::time::FramePos;
use anim_render::device::{DeviceConfig, GpuContext};
use anim_render::draw_list::extract_frame;
use anim_render::graph::{create_renderer, PostChainConfig};
use anim_render::memory::{MemoryMode, MemoryPolicy, TelemetryStatus};
use anim_render::readback::OutputReadback;
use anim_render::{FinalAlpha, RenderConfig, RenderError, VramFloor};
use anim_scene::{
    AudioClip, AudioCue, EvaluatedCamera, EvaluatedNode, SceneEvent, SceneSnapshot, SimulationInput,
};
use anim_sim::{SimTick, SimulationFrame};

const EXTENT: wgpu::Extent3d = wgpu::Extent3d {
    width: 64,
    height: 64,
    depth_or_array_layers: 1,
};

fn namespace() -> uuid::Uuid {
    uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
}

fn node_id(label: &str) -> NodeId {
    NodeId::derive(&namespace(), label.as_bytes())
}

fn asset_id(label: &str) -> AssetId {
    AssetId::derive(&namespace(), label.as_bytes())
}

fn entry(asset: AssetId) -> AssetManifestEntry {
    AssetManifestEntry::new(
        asset,
        ContentHash::from_bytes([7u8; 32]),
        64,
        AssetKind::Image,
        1,
        [],
        "wp07",
        Some(ImageAssetClaims {
            color_space: ImageColorSpace::Linear,
            alpha: ImageAlpha::Straight,
        }),
    )
    .unwrap()
}

fn solid(color: [f32; 4]) -> anim_assets::DecodedImage {
    anim_assets::DecodedImage {
        width: EXTENT.width,
        height: EXTENT.height,
        pixels: vec![color; (EXTENT.width * EXTENT.height) as usize],
    }
}

fn snapshot(asset: AssetId) -> SceneSnapshot {
    SceneSnapshot {
        scene_id: SceneId::derive(&namespace(), b"wp07"),
        revision: Revision(1),
        position: FramePos::at_frame(0),
        time: Rational::from_i128(0),
        nodes: Arc::from(
            vec![EvaluatedNode {
                id: node_id("quad"),
                parent: None,
                world_transform: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                z: 0.0,
                opacity: 1.0,
                visible: true,
                asset: Some(asset),
            }]
            .into_boxed_slice(),
        ),
        cameras: Arc::from(Vec::<EvaluatedCamera>::new().into_boxed_slice()),
        audio_cues: Arc::from(Vec::<AudioCue>::new().into_boxed_slice()),
        simulation_inputs: Arc::from(Vec::<SimulationInput>::new().into_boxed_slice()),
        events: Arc::from(Vec::<SceneEvent>::new().into_boxed_slice()),
        audio_clips: Arc::from(Vec::<AudioClip>::new().into_boxed_slice()),
    }
}

fn empty_sim(tick: i64) -> SimulationFrame {
    SimulationFrame {
        tick: SimTick(tick),
        particles: Vec::new(),
        body_transforms: Vec::new(),
        effects: Vec::new(),
    }
}

struct Fixture {
    gpu: GpuContext,
    renderer: anim_render::Renderer,
    asset: AssetId,
    assets: AssetManifest,
    image: anim_assets::DecodedImage,
}

impl Fixture {
    fn new() -> Self {
        let gpu = GpuContext::create(&DeviceConfig::default()).unwrap();
        let config = RenderConfig {
            extent: EXTENT,
            post: PostChainConfig::default(),
        };
        let mut renderer = create_renderer(&gpu.device, &gpu.queue, &config).unwrap();
        let asset = asset_id("wp07/tex");
        let image = solid([0.25, 0.5, 0.75, 1.0]);
        renderer
            .register_image(
                &gpu.device,
                &gpu.queue,
                asset,
                &image,
                DecoderConfiguration::Rgba8Unpremultiplied,
            )
            .unwrap();
        Self {
            gpu,
            renderer,
            asset,
            assets: AssetManifest::from_records(
                anim_assets::MANIFEST_SCHEMA_VERSION,
                vec![entry(asset)],
            )
            .unwrap(),
            image,
        }
    }

    fn render(&mut self, tick: i64) -> OutputReadback {
        let scene = snapshot(self.asset);
        let sim = empty_sim(tick);
        let quality = self.renderer.quality(FinalAlpha::Premultiplied);
        let frame = extract_frame(
            &scene,
            &sim,
            self.renderer.previous_state(),
            quality,
            &self.assets,
        )
        .unwrap();
        let rendered = self
            .renderer
            .render_scene(&self.gpu.device, &self.gpu.queue, &frame)
            .unwrap();
        match &rendered.output {
            anim_render::OutputDestination::Readback(readback) => readback.clone(),
            anim_render::OutputDestination::View(_) => panic!("export path produced a view"),
        }
    }
}

/// 16.12: "Evicted resources must reload exactly and match a residency-stable render
/// reference." The reference is the same renderer with the texture never evicted; the
/// reload re-uploads the same decoded bytes through the same record identity.
#[test]
fn an_evicted_texture_reloads_to_a_byte_identical_render() {
    let mut fixture = Fixture::new();
    let reference = fixture.render(0);
    let id_before = fixture.renderer.image_memory_id(&fixture.asset).unwrap();

    fixture.renderer.evict_image(&fixture.asset).unwrap();
    assert!(!fixture.renderer.registry().image(&fixture.asset).is_ok());
    assert!(
        !fixture
            .renderer
            .memory()
            .table()
            .get(id_before)
            .unwrap()
            .resident
    );

    // Rendering without the texture is a refusal, not a placeholder (15.11).
    let scene = snapshot(fixture.asset);
    let sim = empty_sim(1);
    let quality = fixture.renderer.quality(FinalAlpha::Premultiplied);
    let frame = extract_frame(
        &scene,
        &sim,
        fixture.renderer.previous_state(),
        quality,
        &fixture.assets,
    )
    .unwrap();
    let missing = fixture
        .renderer
        .render_scene(&fixture.gpu.device, &fixture.gpu.queue, &frame);
    assert!(matches!(
        missing,
        Err(RenderError::TextureNotRegistered { .. })
    ));

    // Reload through the batch path, then re-upload the same bytes.
    let batch = fixture
        .renderer
        .memory_mut()
        .reload_required(&[id_before], 1024 * 1024)
        .unwrap();
    assert_eq!(batch.entries, vec![id_before]);
    fixture
        .renderer
        .register_image(
            &fixture.gpu.device,
            &fixture.gpu.queue,
            fixture.asset,
            &fixture.image,
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
    let id_after = fixture.renderer.image_memory_id(&fixture.asset).unwrap();
    assert_eq!(id_before, id_after, "reload keeps the identity");
    assert!(
        fixture
            .renderer
            .memory()
            .table()
            .get(id_after)
            .unwrap()
            .resident
    );

    let reloaded = fixture.render(2);
    assert_eq!(reference.pixels, reloaded.pixels);
    assert_eq!(fixture.renderer.memory().metrics().reloads, 1);
}

/// 16.12's repeated-resize stress: every generation keeps exactly seven locked target
/// records, the accounting matches ADR-0003's 48 bytes per pixel plus the image, and a
/// zero extent changes nothing.
#[test]
fn repeated_resize_keeps_the_target_accounting_exact() {
    let mut fixture = Fixture::new();
    let image_bytes = 64 * 64 * 8;
    // Every entry differs from the one before it, including across the round boundary
    // (the last is not the first), so each resize is a real new generation.
    let extents = [(96_u32, 80_u32), (128, 128), (32, 48), (64, 64), (100, 60)];
    let mut generation = fixture.renderer.generation();
    for _round in 0..5 {
        for (width, height) in extents {
            let extent = wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            };
            fixture
                .renderer
                .resize(&fixture.gpu.device, extent)
                .unwrap();
            generation = generation.saturating_add(1);
            assert_eq!(fixture.renderer.generation(), generation);
            let table = fixture.renderer.memory().table();
            let targets: Vec<_> = table
                .iter()
                .filter(|r| r.label.starts_with("target/"))
                .collect();
            assert_eq!(
                targets.len(),
                7,
                "labels: {:?}",
                table.iter().map(|r| &r.label).collect::<Vec<_>>()
            );
            assert!(targets.iter().all(|r| r.resident && r.locked));
            let expected = u64::from(width) * u64::from(height) * 48 + image_bytes;
            assert_eq!(table.accounted_bytes(), expected, "{width}x{height}");
        }
    }
    // A resize to the current extent is a no-op, not a generation.
    let current = fixture.renderer.targets().extent;
    fixture
        .renderer
        .resize(&fixture.gpu.device, current)
        .unwrap();
    assert_eq!(fixture.renderer.generation(), generation);
    let before = fixture.renderer.memory().table().accounted_bytes();
    let zero = fixture.renderer.resize(
        &fixture.gpu.device,
        wgpu::Extent3d {
            width: 0,
            height: 8,
            depth_or_array_layers: 1,
        },
    );
    assert!(matches!(zero, Err(RenderError::ZeroExtent { .. })));
    assert_eq!(fixture.renderer.memory().table().accounted_bytes(), before);
}

/// 16.12's scrubbing stress: frames rendered out of tick order release their uniform
/// buffers once the fences complete, leaving only targets and the image accounted.
#[test]
fn scrubbing_releases_completed_frame_buffers() {
    let mut fixture = Fixture::new();
    for tick in [0_i64, 5, 2, 9, 1, 7, 3] {
        fixture.render(tick);
    }
    fixture.renderer.release_completed_work();
    let table = fixture.renderer.memory().table();
    let frame_records: Vec<_> = table
        .iter()
        .filter(|r| r.label.starts_with("frame/"))
        .collect();
    assert!(
        frame_records.is_empty(),
        "completed frames must leave no records: {:?}",
        frame_records.iter().map(|r| &r.label).collect::<Vec<_>>()
    );
    assert_eq!(table.len(), 8, "seven targets and one image");
    assert!(fixture.renderer.memory().metrics().completion_drains >= 7);
    assert_eq!(
        fixture.renderer.memory().metrics().fence_order_violations,
        0
    );
}

/// 16.7/16.11: device loss invalidates residency and serials; reconstruction from the
/// CPU source renders identically. The device here is not truly lost — the test drives
/// the invalidation path and proves the rebuild is exact.
#[test]
fn device_loss_invalidates_and_reconstruction_renders_identically() {
    let mut fixture = Fixture::new();
    let reference = fixture.render(0);
    fixture.renderer.notify_device_loss("wp07 test loss");
    assert!(fixture.renderer.registry().is_empty());
    assert!(fixture.renderer.memory_mut().query_local_memory().is_err());
    assert!(fixture.renderer.memory().table().is_empty());
    assert_eq!(fixture.renderer.memory().completed_serial(), 0);

    // Reconstruction: a fresh renderer on the recovered device, re-upload from CPU.
    let config = RenderConfig {
        extent: EXTENT,
        post: PostChainConfig::default(),
    };
    let mut rebuilt = create_renderer(&fixture.gpu.device, &fixture.gpu.queue, &config).unwrap();
    rebuilt
        .register_image(
            &fixture.gpu.device,
            &fixture.gpu.queue,
            fixture.asset,
            &fixture.image,
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
    let scene = snapshot(fixture.asset);
    let sim = empty_sim(0);
    let quality = rebuilt.quality(FinalAlpha::Premultiplied);
    let frame = extract_frame(
        &scene,
        &sim,
        rebuilt.previous_state(),
        quality,
        &fixture.assets,
    )
    .unwrap();
    let rendered = rebuilt
        .render_scene(&fixture.gpu.device, &fixture.gpu.queue, &frame)
        .unwrap();
    let pixels = match &rendered.output {
        anim_render::OutputDestination::Readback(readback) => readback.pixels.clone(),
        anim_render::OutputDestination::View(_) => panic!("export path produced a view"),
    };
    assert_eq!(reference.pixels, pixels);
}

/// 16.7's rollback: a creation that fails after a successful preflight releases the
/// reservation and registers nothing.
#[test]
fn a_malformed_upload_after_preflight_leaves_no_reservation() {
    let mut fixture = Fixture::new();
    let records_before = fixture.renderer.memory().table().len();
    let malformed = anim_assets::DecodedImage {
        width: 8,
        height: 8,
        pixels: vec![[1.0, 0.0, 0.0, 1.0]; 3], // claims 64 pixels, carries 3
    };
    let result = fixture.renderer.register_image(
        &fixture.gpu.device,
        &fixture.gpu.queue,
        asset_id("wp07/bad"),
        &malformed,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    assert!(result.is_err());
    assert_eq!(fixture.renderer.memory().reserved_bytes(), 0);
    assert_eq!(fixture.renderer.memory().table().len(), records_before);
    assert!(fixture
        .renderer
        .image_memory_id(&asset_id("wp07/bad"))
        .is_none());
}

/// 16.10's diagnostics: a configured renderer reports live telemetry and accounts the
/// full resident set through a real render.
#[cfg(feature = "dxgi-telemetry")]
#[test]
fn configured_telemetry_is_live_during_a_render_run() {
    let gpu = GpuContext::create(&DeviceConfig::default()).unwrap();
    let config = RenderConfig {
        extent: EXTENT,
        post: PostChainConfig::default(),
    };
    let mut renderer = create_renderer(&gpu.device, &gpu.queue, &config).unwrap();
    let dedicated = match &gpu.vram {
        VramFloor::Verified { bytes } => *bytes,
        other => panic!("the certified adapter must verify its VRAM floor, got {other:?}"),
    };
    renderer
        .configure_memory(
            MemoryPolicy::default(),
            MemoryMode::Export,
            dedicated,
            gpu.record.vendor_id,
            gpu.record.device_id,
        )
        .unwrap();
    let asset = asset_id("wp07/live");
    let assets =
        AssetManifest::from_records(anim_assets::MANIFEST_SCHEMA_VERSION, vec![entry(asset)])
            .unwrap();
    renderer
        .register_image(
            &gpu.device,
            &gpu.queue,
            asset,
            &solid([1.0, 1.0, 1.0, 1.0]),
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
    let scene = snapshot(asset);
    let sim = empty_sim(0);
    let quality = renderer.quality(FinalAlpha::Premultiplied);
    let frame = extract_frame(&scene, &sim, renderer.previous_state(), quality, &assets).unwrap();
    renderer
        .render_scene(&gpu.device, &gpu.queue, &frame)
        .unwrap();
    renderer.release_completed_work();

    assert!(matches!(
        renderer.memory().telemetry_status(),
        TelemetryStatus::Live(_)
    ));
    let snapshot = renderer.memory().last_snapshot().unwrap();
    assert!(snapshot.budget > 0);
    assert!(snapshot.effective_usage >= snapshot.accounted_usage);
    let expected = 64 * 64 * 48 + 64 * 64 * 8;
    assert_eq!(renderer.memory().table().accounted_bytes(), expected);
}
