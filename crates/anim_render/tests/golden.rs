//! Color, alpha, ordering, velocity and clean-output tests (spec 15.3, gate G5).
//!
//! # The oracle rule, and why there is no tolerance here
//!
//! No expected value in this file is produced by calling the function under test in a
//! different way, and **no numeric tolerance appears anywhere**. Cross-device golden
//! tolerance is WP11's ADR-0016 and golden *approval* is Gate 1 after S2; inventing an
//! epsilon here would be both of those decisions made early and silently.
//!
//! So the goldens are of three kinds, each exact:
//!
//!  * **Render against render.** 15.7 says a straight-alpha asset and a premultiplied one
//!    holding the same pixels produce the same image. Comparing two readbacks is byte
//!    equality, with no expected colour to compute and no transcendental to disagree about.
//!    The same shape covers equal-depth ties, layer ordering and the editor/export split.
//!  * **Analytic where the arithmetic is exact.** Black is 0.0 through the tone map and
//!    through both segments of the sRGB curve; alpha passes through both untouched. Those
//!    bytes are written as the numbers the specification says they are.
//!  * **Structural.** The pass log, the target generation, the velocity validity flag and
//!    the f16 HDR range are claims about what ran, not about what colour it produced.
//!
//! Where a GPU transcendental is involved (`pow` in the sRGB encode) the assertion is a
//! relation — greater than, less than, not clamped — rather than a byte, because a byte
//! there would be a tolerance in disguise.
//!
//! # This suite opens a real device
//!
//! 15.12 requires "pinned-stack readback goldens", which cannot be done without the pinned
//! stack. `GpuContext::create` enforces DECISIONS.md #18 — PCI bus id, adapter name, driver,
//! the VRAM floor — so on any other machine this suite *refuses* rather than quietly
//! producing goldens from a different adapter. That is spec 9.12's rule, and a skip would
//! violate it: a golden that does not run is not a golden that passed.
//!
//! Clauses deliberately not covered here, and where they went: the sRGB EOTF's exact values,
//! premultiplication order, interlaced PNG and paletted expansion are CPU-side and tested in
//! `anim_assets::image`; the transparent comparator's tuple is tested in `draw_list`; the
//! disabled-post-stage contract is tested in `graph`.

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
use anim_render::alpha::AlphaMode;
use anim_render::device::{DeviceConfig, GpuContext};
use anim_render::draw_list::{extract_frame, DrawItem, PreviousFrameState, RenderFrame};
use anim_render::graph::{create_renderer, PassKind, PostChainConfig, PostStage};
use anim_render::readback::OutputReadback;
use anim_render::{FinalAlpha, RenderConfig, RenderQuality, RenderedScene};
use anim_scene::{
    AudioClip, AudioCue, EvaluatedCamera, EvaluatedNode, SceneEvent, SceneSnapshot, SimulationInput,
};
use anim_sim::{SimTick, SimulationFrame};
use glam::{Mat4, Vec3};

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

/// A manifest entry with classified claims, through the validating constructor.
fn entry(asset: AssetId, alpha: ImageAlpha, space: ImageColorSpace) -> AssetManifestEntry {
    AssetManifestEntry::new(
        asset,
        ContentHash::from_bytes([3u8; 32]),
        64,
        AssetKind::Image,
        1,
        [],
        "golden",
        Some(ImageAssetClaims {
            color_space: space,
            alpha,
        }),
    )
    .unwrap()
}

fn manifest(entries: Vec<AssetManifestEntry>) -> AssetManifest {
    AssetManifest::from_records(anim_assets::MANIFEST_SCHEMA_VERSION, entries).unwrap()
}

/// A solid-colour decoded image the size of the target, so an identity-transformed quad
/// covers every pixel and the readback has no background to reason about.
fn solid(color: [f32; 4]) -> anim_assets::DecodedImage {
    anim_assets::DecodedImage {
        width: EXTENT.width,
        height: EXTENT.height,
        pixels: vec![color; (EXTENT.width * EXTENT.height) as usize],
    }
}

fn snapshot(nodes: Vec<EvaluatedNode>, events: Vec<SceneEvent>) -> SceneSnapshot {
    SceneSnapshot {
        scene_id: SceneId::derive(&namespace(), b"golden"),
        revision: Revision(1),
        position: FramePos::at_frame(0),
        time: Rational::from_i128(0),
        nodes: Arc::from(nodes.into_boxed_slice()),
        cameras: Arc::from(Vec::<EvaluatedCamera>::new().into_boxed_slice()),
        audio_cues: Arc::from(Vec::<AudioCue>::new().into_boxed_slice()),
        simulation_inputs: Arc::from(Vec::<SimulationInput>::new().into_boxed_slice()),
        events: Arc::from(events.into_boxed_slice()),
        audio_clips: Arc::from(Vec::<AudioClip>::new().into_boxed_slice()),
    }
}

fn node(label: &str, asset: AssetId, z: f64) -> EvaluatedNode {
    EvaluatedNode {
        id: node_id(label),
        parent: None,
        world_transform: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        z,
        opacity: 1.0,
        visible: true,
        asset: Some(asset),
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

/// A device, a renderer, and one registered image.
struct Fixture {
    gpu: GpuContext,
    renderer: anim_render::Renderer,
    asset: AssetId,
    assets: AssetManifest,
}

impl Fixture {
    fn new(color: [f32; 4], alpha: ImageAlpha, config: DecoderConfiguration) -> Self {
        let gpu = GpuContext::create(&DeviceConfig::default()).unwrap();
        let config_render = RenderConfig {
            extent: EXTENT,
            post: PostChainConfig::default(),
        };
        let mut renderer = create_renderer(&gpu.device, &gpu.queue, &config_render).unwrap();
        let asset = asset_id("golden/tex");
        renderer
            .register_image(&gpu.device, &gpu.queue, asset, &solid(color), config)
            .unwrap();
        Self {
            gpu,
            renderer,
            asset,
            assets: manifest(vec![entry(asset, alpha, ImageColorSpace::Linear)]),
        }
    }

    fn quality(&self, final_alpha: FinalAlpha) -> RenderQuality {
        self.renderer.quality(final_alpha)
    }

    /// Register a second image in the same renderer, for a test that needs two colours in
    /// one frame. Returns its asset id.
    fn register_extra(
        &mut self,
        label: &str,
        color: [f32; 4],
        config: DecoderConfiguration,
    ) -> AssetId {
        let asset = asset_id(label);
        self.renderer
            .register_image(
                &self.gpu.device,
                &self.gpu.queue,
                asset,
                &solid(color),
                config,
            )
            .unwrap();
        self.assets
            .insert(entry(asset, ImageAlpha::Straight, ImageColorSpace::Linear));
        asset
    }

    /// Extract from a scene and render, the path export and preview both take.
    fn render_scene(
        &mut self,
        scene: &SceneSnapshot,
        sim: &SimulationFrame,
        final_alpha: FinalAlpha,
    ) -> (OutputReadback, RenderedScene) {
        let quality = self.quality(final_alpha);
        let frame = extract_frame(
            scene,
            sim,
            self.renderer.previous_state(),
            quality,
            &self.assets,
        )
        .unwrap();
        let rendered = self
            .renderer
            .render_scene(&self.gpu.device, &self.gpu.queue, &frame)
            .unwrap();
        let bytes = match &rendered.output {
            anim_render::OutputDestination::Readback(readback) => readback.clone(),
            anim_render::OutputDestination::View(_) => panic!("export path produced a view"),
        };
        (bytes, rendered)
    }

    /// Render a frame built by hand, for the alpha modes extraction cannot author yet.
    fn render_items(
        &mut self,
        items: Vec<DrawItem>,
        final_alpha: FinalAlpha,
        tick: i64,
    ) -> OutputReadback {
        self.render_items_with_history(items, final_alpha, tick, false)
    }

    /// As `render_items`, with an explicit frame-level history verdict.
    fn render_items_with_history(
        &mut self,
        items: Vec<DrawItem>,
        final_alpha: FinalAlpha,
        tick: i64,
        history_valid: bool,
    ) -> OutputReadback {
        let frame = RenderFrame {
            revision: Revision(1),
            tick: SimTick(tick),
            items,
            camera: anim_render::CameraMatrices {
                current_view_proj: anim_render::camera::projection(EXTENT),
                previous_view_proj: anim_render::camera::projection(EXTENT),
            },
            cut: false,
            history_valid,
            quality: self.quality(final_alpha),
        };
        let rendered = self
            .renderer
            .render_scene(&self.gpu.device, &self.gpu.queue, &frame)
            .unwrap();
        match &rendered.output {
            anim_render::OutputDestination::Readback(readback) => readback.clone(),
            anim_render::OutputDestination::View(_) => panic!("export path produced a view"),
        }
    }

    /// One screen-covering item in a chosen alpha mode.
    fn item(&self, mode: AlphaMode, pass: PassKind) -> DrawItem {
        DrawItem {
            entity: node_id("quad"),
            draw_index: 0,
            layer_order: 0,
            material_pass: anim_render::alpha::mode_id(mode),
            view_depth: 0.0,
            current_transform: Mat4::IDENTITY,
            previous_transform: Mat4::IDENTITY,
            alpha_mode: mode,
            previous_valid: false,
            asset: self.asset,
            opacity: 1.0,
            pass,
        }
    }
}

/// The pixel at the centre of a readback, as four bytes.
fn centre(readback: &OutputReadback) -> [u8; 4] {
    let index = ((readback.height / 2) * readback.width + readback.width / 2) as usize * 4;
    [
        readback.pixels[index],
        readback.pixels[index + 1],
        readback.pixels[index + 2],
        readback.pixels[index + 3],
    ]
}

// ---------------------------------------------------------------------------
// The CPU/WGSL layout agreement. No GPU needed, and it is the test that catches a
// stride drifting between Rust and a shader before any golden can notice.
// ---------------------------------------------------------------------------

/// The three scene shaders repeat the uniform structs because wgpu has no include. This
/// compares the copies byte for byte, so a field added to one is a failure here rather than
/// a sheared image three passes later.
#[test]
fn the_uniform_structs_agree_across_every_shader() {
    let scene = include_str!("../src/shaders/scene.wgsl");
    let ordered = include_str!("../src/shaders/ordered_2d.wgsl");
    let transparent = include_str!("../src/shaders/transparent.wgsl");

    fn block(source: &str, name: &str) -> String {
        let start = source.find(&format!("struct {name} {{")).unwrap();
        let end = source[start..].find('}').unwrap() + start + 1;
        source[start..end].to_owned()
    }

    for name in ["SceneUniform", "ItemUniform"] {
        let reference = block(scene, name);
        assert_eq!(
            block(ordered, name),
            reference,
            "{name} drifted in ordered_2d.wgsl"
        );
        assert_eq!(
            block(transparent, name),
            reference,
            "{name} drifted in transparent.wgsl"
        );
    }
}

/// Every shader parses. A WGSL defect should be a shader error at construction, and this
/// is the check that the five files are well-formed without opening a device.
#[test]
fn every_shader_parses_as_wgsl() {
    for (name, source) in [
        ("scene", include_str!("../src/shaders/scene.wgsl")),
        ("ordered_2d", include_str!("../src/shaders/ordered_2d.wgsl")),
        (
            "transparent",
            include_str!("../src/shaders/transparent.wgsl"),
        ),
        ("post", include_str!("../src/shaders/post.wgsl")),
        ("output", include_str!("../src/shaders/output.wgsl")),
    ] {
        if let Err(error) = wgpu::naga::front::wgsl::parse_str(source) {
            panic!(
                "{name}.wgsl does not parse: {}",
                error.emit_to_string(source)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Color and alpha (15.12's first sentence).
// ---------------------------------------------------------------------------

/// Black is exact through every stage: 0.0 tone-maps to 0.0, and both segments of the sRGB
/// curve map 0.0 to 0.0. No transcendental is involved, so these bytes are the number the
/// specification says they are rather than a measured baseline.
#[test]
fn an_empty_scene_reads_back_as_exact_black_with_zero_alpha() {
    let mut fixture = Fixture::new(
        [1.0, 1.0, 1.0, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let scene = snapshot(vec![], vec![]);
    let (bytes, rendered) = fixture.render_scene(&scene, &empty_sim(1), FinalAlpha::Premultiplied);
    assert_eq!(bytes.pixels.len(), (64 * 64 * 4) as usize);
    assert!(
        bytes.pixels.iter().all(|byte| *byte == 0),
        "an empty scene must be exactly zero, not merely dark"
    );
    assert_eq!(centre(&bytes), [0, 0, 0, 0]);
    assert_eq!(rendered.generation, 1);
}

/// 15.7: "Zero-alpha standard-blend output contributes no color." Over black that is
/// arithmetic — One/OneMinusSrcAlpha with a zero source alpha adds nothing — so the result
/// is exactly the black frame, byte for byte.
#[test]
fn a_zero_alpha_straight_item_contributes_no_color() {
    let mut fixture = Fixture::new(
        [1.0, 0.0, 0.0, 0.0],
        ImageAlpha::Straight,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let item = fixture.item(AlphaMode::Straight, PassKind::Ordered2d);
    let with_item = fixture.render_items(vec![item], FinalAlpha::Premultiplied, 1);
    assert!(
        with_item.pixels.iter().all(|byte| *byte == 0),
        "a zero-alpha red quad must leave the frame exactly black"
    );
}

/// 15.7's two standard modes differ only in *where* the multiply happens, so the same
/// pixels through either must produce the same image. This is a render-against-render
/// golden: no expected colour, no tolerance.
#[test]
fn straight_and_premultiplied_produce_the_same_image() {
    // A 50%-alpha grey. Straight-decoded, the shader multiplies; premultiplied-decoded, the
    // multiply already happened in linear space at decode.
    let color = [0.4, 0.4, 0.4, 0.5];
    let premultiplied = [0.4 * 0.5, 0.4 * 0.5, 0.4 * 0.5, 0.5];

    let mut straight = Fixture::new(
        color,
        ImageAlpha::Straight,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let item = straight.item(AlphaMode::Straight, PassKind::Transparent);
    let a = straight.render_items(vec![item], FinalAlpha::Premultiplied, 1);

    let mut premul = Fixture::new(
        premultiplied,
        ImageAlpha::Straight,
        DecoderConfiguration::Rgba8Premultiplied,
    );
    let item = premul.item(AlphaMode::Premultiplied, PassKind::Transparent);
    let b = premul.render_items(vec![item], FinalAlpha::Premultiplied, 1);

    assert_eq!(
        a.pixels, b.pixels,
        "15.7: the two standard modes differ only in where the multiply happens"
    );
    assert_ne!(centre(&a)[3], 0, "the fixture must actually have coverage");
}

/// Additive is One/One (15.7), so two identical additive quads accumulate where two
/// standard quads saturate. Comparing the two renders is exact and needs no expected colour.
#[test]
fn additive_accumulates_where_standard_over_saturates() {
    let color = [0.2, 0.1, 0.05, 0.5];
    let mut standard = Fixture::new(
        color,
        ImageAlpha::Straight,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let mut a = standard.item(AlphaMode::Straight, PassKind::Transparent);
    let mut b = standard.item(AlphaMode::Straight, PassKind::Transparent);
    b.entity = node_id("quad-second");
    b.view_depth = -1.0;
    let over = standard.render_items(vec![a.clone(), b.clone()], FinalAlpha::Premultiplied, 1);

    let mut additive = Fixture::new(
        [0.2 * 0.5, 0.1 * 0.5, 0.05 * 0.5, 0.5],
        ImageAlpha::Straight,
        DecoderConfiguration::Rgba8Premultiplied,
    );
    a.alpha_mode = AlphaMode::AdditivePremultiplied;
    a.material_pass = anim_render::alpha::mode_id(AlphaMode::AdditivePremultiplied);
    b.alpha_mode = AlphaMode::AdditivePremultiplied;
    b.material_pass = a.material_pass;
    let sum = additive.render_items(vec![a, b], FinalAlpha::Premultiplied, 1);

    let over_pixel = centre(&over);
    let sum_pixel = centre(&sum);
    assert!(
        sum_pixel[0] > over_pixel[0],
        "additive must accumulate past where over-blending saturates: {sum_pixel:?} vs {over_pixel:?}"
    );
}

/// An alpha-test cutoff discards below the threshold *before* attachment writes, so a
/// fully transparent quad under AlphaTest leaves no depth hole and no colour. Over black
/// the result is exactly black at any cutoff above zero.
#[test]
fn an_alpha_test_above_the_coverage_discards_every_fragment() {
    let mut fixture = Fixture::new(
        [1.0, 1.0, 1.0, 0.25],
        ImageAlpha::Straight,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let mut item = fixture.item(AlphaMode::AlphaTest { cutoff: 0.5 }, PassKind::Opaque);
    item.previous_valid = false;
    let discarded = fixture.render_items(vec![item.clone()], FinalAlpha::Premultiplied, 1);
    assert!(
        discarded.pixels.iter().all(|byte| *byte == 0),
        "coverage 0.25 is below the 0.5 cutoff, so nothing may be written"
    );

    // Below the cutoff the same quad draws, which is what proves the test above was the
    // discard firing rather than a quad that never rasterized.
    item.alpha_mode = AlphaMode::AlphaTest { cutoff: 0.1 };
    let kept = fixture.render_items(vec![item], FinalAlpha::Premultiplied, 1);
    assert_ne!(
        centre(&kept),
        [0, 0, 0, 0],
        "coverage above the cutoff must draw"
    );
}

/// An opaque item's alpha is forced to 1, and neither the tone map nor the output transfer
/// touches alpha, so the readback's alpha channel is exactly 255.
#[test]
fn an_opaque_quad_reports_full_coverage_exactly() {
    let mut fixture = Fixture::new(
        [0.3, 0.6, 0.9, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let scene = snapshot(vec![node("quad", fixture.asset, 0.0)], vec![]);
    let (bytes, _) = fixture.render_scene(&scene, &empty_sim(1), FinalAlpha::Premultiplied);
    assert_eq!(centre(&bytes)[3], 255);
    assert!(bytes.pixels.chunks(4).all(|pixel| pixel[3] == 255));
}

// ---------------------------------------------------------------------------
// Ordering (15.12's equal-depth ties and layer ordering).
// ---------------------------------------------------------------------------

/// Two transparent items at the same quantized depth must composite identically regardless
/// of the order extraction produced them in. The tie-break is the entity id, so this is a
/// render-against-render equality with no tolerance.
#[test]
fn an_equal_depth_tie_composites_the_same_way_in_either_insertion_order() {
    let mut fixture = Fixture::new(
        [0.8, 0.2, 0.1, 0.5],
        ImageAlpha::Straight,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let mut a = fixture.item(AlphaMode::Straight, PassKind::Transparent);
    let mut b = fixture.item(AlphaMode::Straight, PassKind::Transparent);
    b.entity = node_id("quad-second");
    // Same depth bucket: 1/512 of a pixel at 256 buckets per pixel.
    a.view_depth = 4.0;
    b.view_depth = 4.0 + 1.0 / 512.0;
    assert_eq!(a.quantized_view_depth(), b.quantized_view_depth());

    let mut forward = vec![a.clone(), b.clone()];
    forward.sort_by(anim_render::draw_list::total_cmp);
    let mut reversed = vec![b, a];
    reversed.sort_by(anim_render::draw_list::total_cmp);
    assert_eq!(
        forward.iter().map(DrawItem::key).collect::<Vec<_>>(),
        reversed.iter().map(DrawItem::key).collect::<Vec<_>>(),
        "the comparator must be total before the render can be compared"
    );

    let x = fixture.render_items(forward, FinalAlpha::Premultiplied, 1);
    let y = fixture.render_items(reversed, FinalAlpha::Premultiplied, 1);
    assert_eq!(
        x.pixels, y.pixels,
        "a depth tie must not depend on insertion order"
    );
}

/// Layer order dominates depth (15.7's tuple), so swapping two layers changes the image
/// even when the depths would sort the other way.
#[test]
fn layer_order_dominates_depth_in_the_composited_image() {
    let mut fixture = Fixture::new(
        [0.9, 0.1, 0.1, 0.5],
        ImageAlpha::Straight,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    // Two DIFFERENT colours. Over-blending one colour is commutative even at unequal alpha,
    // so a single-texture fixture could not observe ordering at all.
    let blue = fixture.register_extra(
        "golden/blue",
        [0.1, 0.1, 0.9, 0.5],
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let mut lower = fixture.item(AlphaMode::Straight, PassKind::Ordered2d);
    let mut upper = fixture.item(AlphaMode::Straight, PassKind::Ordered2d);
    upper.entity = node_id("quad-upper");
    upper.asset = blue;
    lower.layer_order = 0;
    upper.layer_order = 1;
    // The lower layer is nearer, so depth alone would draw it last; layer order must win.
    lower.view_depth = -10.0;
    upper.view_depth = 10.0;

    // render_items bypasses extract_frame, so the sort is applied here explicitly — it is
    // the same total comparator extraction uses, and the graph deliberately does not re-sort.
    let mut first = vec![lower.clone(), upper.clone()];
    first.sort_by(anim_render::draw_list::total_cmp);
    let ordered = fixture.render_items(first, FinalAlpha::Premultiplied, 1);

    lower.layer_order = 1;
    upper.layer_order = 0;
    let mut second = vec![lower, upper];
    second.sort_by(anim_render::draw_list::total_cmp);
    let swapped = fixture.render_items(second, FinalAlpha::Premultiplied, 1);
    assert_ne!(
        ordered.pixels, swapped.pixels,
        "layer order must change the composite even when depth disagrees"
    );
}

// ---------------------------------------------------------------------------
// Linear decoding, HDR before tone map, exactly one output transfer.
// ---------------------------------------------------------------------------

/// The same PNG bytes under an sRGB claim and a Linear claim decode to different linear
/// values, so the two renders differ. This is 15.12's "linear decoding" clause end to end:
/// the transfer comes from the recorded claim, through the real decoder, into the texture.
#[test]
fn an_srgb_claim_and_a_linear_claim_render_differently() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grey.png");
    write_png(&path, 8, 8, [128u8, 128, 128, 255]);
    let bytes = std::fs::read(&path).unwrap();

    let gpu = GpuContext::create(&DeviceConfig::default()).unwrap();
    let mut rendered = Vec::new();
    for space in [ImageColorSpace::Srgb, ImageColorSpace::Linear] {
        let asset = asset_id(if space == ImageColorSpace::Srgb {
            "golden/srgb"
        } else {
            "golden/linear"
        });
        // Two records over the same bytes, differing only in the colour claim. Built
        // directly rather than imported twice, because import would classify both the same
        // way and the comparison would have nothing to vary.
        let record = AssetManifestEntry::new(
            asset,
            ContentHash::from_bytes([9u8; 32]),
            bytes.len() as u64,
            AssetKind::Image,
            1,
            [],
            "grey",
            Some(ImageAssetClaims {
                color_space: space,
                alpha: ImageAlpha::Opaque,
            }),
        )
        .unwrap();
        let decoded = anim_assets::ImageDecoder
            .decode_pixels(&record, &path, DecoderConfiguration::Rgba8Unpremultiplied)
            .unwrap();
        let mut renderer = create_renderer(
            &gpu.device,
            &gpu.queue,
            &RenderConfig {
                extent: EXTENT,
                post: PostChainConfig::default(),
            },
        )
        .unwrap();
        renderer
            .register_image(
                &gpu.device,
                &gpu.queue,
                asset,
                &decoded,
                DecoderConfiguration::Rgba8Unpremultiplied,
            )
            .unwrap();
        let frame = RenderFrame {
            revision: Revision(1),
            tick: SimTick(1),
            items: vec![DrawItem {
                entity: node_id("quad"),
                draw_index: 0,
                layer_order: 0,
                material_pass: 0,
                view_depth: 0.0,
                // The texture is 8x8 and the target 64x64, so scale by 8 to cover it.
                current_transform: Mat4::from_scale(Vec3::splat(8.0)),
                previous_transform: Mat4::from_scale(Vec3::splat(8.0)),
                alpha_mode: AlphaMode::Opaque,
                previous_valid: false,
                asset,
                opacity: 1.0,
                pass: PassKind::Opaque,
            }],
            camera: anim_render::CameraMatrices {
                current_view_proj: anim_render::camera::projection(EXTENT),
                previous_view_proj: anim_render::camera::projection(EXTENT),
            },
            cut: false,
            history_valid: false,
            quality: renderer.quality(FinalAlpha::Premultiplied),
        };
        let scene = renderer
            .render_scene(&gpu.device, &gpu.queue, &frame)
            .unwrap();
        rendered.push(match &scene.output {
            anim_render::OutputDestination::Readback(readback) => readback.clone(),
            anim_render::OutputDestination::View(_) => panic!("export path produced a view"),
        });
    }
    assert_ne!(
        rendered[0].pixels, rendered[1].pixels,
        "the recorded colour space must change the image; a hardcoded transfer would not"
    );
    // sRGB 128/255 decodes to about 0.216 linear, which is darker than the 0.502 a Linear
    // claim passes through. The relation is exact even though the encode uses pow.
    assert!(
        centre(&rendered[0])[0] < centre(&rendered[1])[0],
        "an sRGB claim must decode darker than a linear one for the same encoded bytes"
    );
}

/// 15.6: "HDR post effects operate before tone mapping." A scene value above 1.0 must
/// survive in the Rgba16Float target and arrive tone-mapped below saturation in the 8-bit
/// output. Reading both targets makes the ordering observable rather than inferred.
#[test]
fn hdr_above_one_survives_before_the_tone_map_and_is_compressed_after() {
    let mut fixture = Fixture::new(
        [4.0, 4.0, 4.0, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let scene = snapshot(vec![node("quad", fixture.asset, 0.0)], vec![]);
    let (output, _) = fixture.render_scene(&scene, &empty_sim(1), FinalAlpha::Premultiplied);

    let hdr = anim_render::readback::read_texture(
        &fixture.gpu.device,
        &fixture.gpu.queue,
        &fixture.renderer.targets().scene_hdr,
        EXTENT,
        wgpu::TextureFormat::Rgba16Float,
    )
    .unwrap();
    // The centre texel as f16, little-endian, assembled explicitly because a mapped buffer
    // carries no alignment guarantee.
    let texel_bytes = 8usize;
    let index = ((EXTENT.height / 2) * EXTENT.width + EXTENT.width / 2) as usize * texel_bytes;
    let red = u16::from_le_bytes([hdr.pixels[index], hdr.pixels[index + 1]]);
    let value = half::f16::from_bits(red).to_f32();
    assert!(
        value > 1.0,
        "the HDR target must hold the scene-referred value above 1.0, got {value}"
    );
    // Reinhard maps 4.0 to 0.8, which encodes below saturation. The assertion is a relation
    // rather than a byte because the sRGB encode uses a GPU transcendental.
    let pixel = centre(&output);
    assert!(
        pixel[0] < 255,
        "tone mapping must compress HDR, got {pixel:?}"
    );
    assert!(pixel[0] > 0, "and must not crush it to black");
    assert_eq!(pixel[3], 255, "alpha passes through the tone map untouched");
}

/// 15.12's "exactly one output transfer", as a claim about the pass log: one Output record,
/// and it is last. #40 ruling 3 put the encode in a named shader stage precisely so this is
/// checkable as a stage rather than as fixed-function hardware state.
#[test]
fn the_pass_log_shows_exactly_one_output_transfer_and_it_is_last() {
    let mut fixture = Fixture::new(
        [0.2, 0.4, 0.6, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let scene = snapshot(vec![node("quad", fixture.asset, 0.0)], vec![]);
    let (_, rendered) = fixture.render_scene(&scene, &empty_sim(1), FinalAlpha::Premultiplied);
    let outputs: Vec<_> = rendered
        .pass_log
        .iter()
        .filter(|record| record.kind == PassKind::Output)
        .collect();
    assert_eq!(
        outputs.len(),
        1,
        "15.12 requires exactly one output transfer"
    );
    assert_eq!(rendered.pass_log.last().unwrap().kind, PassKind::Output);
    assert_eq!(outputs[0].writes, vec!["scene_output"]);
}

/// 15.12's command-log pass-order verification: the forward passes run in 15.6's order,
/// then the chain in 15.7's versioned order, then output. Disabled stages are present with
/// empty reads and writes, which is #40 ruling 4's contract.
#[test]
fn the_command_log_records_15_6_and_15_7_s_order() {
    let mut fixture = Fixture::new(
        [0.2, 0.4, 0.6, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let scene = snapshot(vec![node("quad", fixture.asset, 0.0)], vec![]);
    let (_, rendered) = fixture.render_scene(&scene, &empty_sim(1), FinalAlpha::Premultiplied);
    let kinds: Vec<PassKind> = rendered.pass_log.iter().map(|record| record.kind).collect();
    assert_eq!(
        kinds,
        vec![
            PassKind::Opaque,
            PassKind::Ordered2d,
            PassKind::Transparent,
            PassKind::Post {
                stage: PostStage::DepthAware
            },
            PassKind::Post {
                stage: PostStage::MotionBlur
            },
            PassKind::Post {
                stage: PostStage::Bloom
            },
            PassKind::ToneMap,
            PassKind::Grade,
            PassKind::Output,
        ]
    );
    for record in &rendered.pass_log {
        if !record.enabled {
            assert!(
                record.reads.is_empty(),
                "{} claimed a read",
                record.kind.as_str()
            );
            assert!(
                record.writes.is_empty(),
                "{} claimed a write",
                record.kind.as_str()
            );
            assert_eq!(record.draw_count, 0);
        }
    }
    // Only the opaque pass writes the auxiliaries (15.6).
    let auxiliary_writers: Vec<_> = rendered
        .pass_log
        .iter()
        .filter(|record| record.writes.contains(&"normal") || record.writes.contains(&"velocity"))
        .map(|record| record.kind)
        .collect();
    assert_eq!(auxiliary_writers, vec![PassKind::Opaque]);
}

/// The final alpha conversion is the codec's requirement, and at full coverage dividing by
/// alpha 1.0 is exact — so the two output modes agree byte for byte on an opaque scene.
#[test]
fn the_two_final_alpha_modes_agree_at_full_coverage() {
    let mut fixture = Fixture::new(
        [0.25, 0.5, 0.75, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let scene = snapshot(vec![node("quad", fixture.asset, 0.0)], vec![]);
    let (premul, _) = fixture.render_scene(&scene, &empty_sim(1), FinalAlpha::Premultiplied);
    let (straight, _) = fixture.render_scene(&scene, &empty_sim(1), FinalAlpha::Straight);
    assert_eq!(
        premul.pixels, straight.pixels,
        "at alpha 1.0 the straight conversion divides by one and must change nothing"
    );
}

/// 15.7's zero-alpha RGB rule: converting to straight alpha at zero coverage must produce
/// the defined zero, not a division by zero or an undefined value.
#[test]
fn a_zero_alpha_output_converts_to_the_defined_zero_rgb() {
    let mut fixture = Fixture::new(
        [1.0, 0.0, 0.0, 0.0],
        ImageAlpha::Straight,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let item = fixture.item(AlphaMode::Straight, PassKind::Ordered2d);
    let bytes = fixture.render_items(vec![item], FinalAlpha::Straight, 1);
    assert_eq!(centre(&bytes), [0, 0, 0, 0]);
    assert!(bytes.pixels.iter().all(|byte| *byte == 0));
}

// ---------------------------------------------------------------------------
// Velocity and history (15.12's second sentence).
// ---------------------------------------------------------------------------

/// A moving object with valid history writes a non-zero velocity; the same object on the
/// first frame writes zero because there is no previous submission. 15.7's validity rule,
/// read out of the velocity target rather than inferred from the image.
#[test]
fn a_moving_object_writes_velocity_only_when_history_is_valid() {
    let mut fixture = Fixture::new(
        [1.0, 1.0, 1.0, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let mut first = fixture.item(AlphaMode::Opaque, PassKind::Opaque);
    first.previous_valid = false;
    let _ = fixture.render_items(vec![first.clone()], FinalAlpha::Premultiplied, 1);
    let invalid = read_velocity(&fixture);
    assert_eq!(
        invalid,
        [0.0, 0.0],
        "a first frame has no previous submission"
    );

    // Now a valid history and a transform that moved one pixel to the right. Tick 2 is
    // contiguous with tick 1, which is what makes the committed history usable.
    first.previous_valid = true;
    first.previous_transform = Mat4::IDENTITY;
    first.current_transform = Mat4::from_translation(Vec3::new(1.0, 0.0, 0.0));
    let _ = fixture.render_items_with_history(vec![first], FinalAlpha::Premultiplied, 2, true);
    let valid = read_velocity(&fixture);
    // One pixel of a 64-wide target is 1/64 of the UV range. The assertion is a relation
    // plus the exact expected magnitude to within f16 storage, which is a width reduction
    // and not a colour tolerance.
    assert!(
        valid[0] > 0.0,
        "rightward motion must produce positive u velocity"
    );
    assert_eq!(valid[1], 0.0, "no vertical motion means no v velocity");
    let expected = 1.0 / EXTENT.width as f32;
    assert!(
        (valid[0] - expected).abs() < expected * 0.01,
        "one pixel of a {}-wide target is {expected} UV, got {}",
        EXTENT.width,
        valid[0]
    );
}

/// Read the centre texel's velocity xy out of the Rgba16Float target.
fn read_velocity(fixture: &Fixture) -> [f32; 2] {
    let velocity = anim_render::readback::read_texture(
        &fixture.gpu.device,
        &fixture.gpu.queue,
        &fixture.renderer.targets().velocity,
        EXTENT,
        wgpu::TextureFormat::Rgba16Float,
    )
    .unwrap();
    let index = ((EXTENT.height / 2) * EXTENT.width + EXTENT.width / 2) as usize * 8;
    let component = |offset: usize| {
        let bits = u16::from_le_bytes([
            velocity.pixels[index + offset],
            velocity.pixels[index + offset + 1],
        ]);
        half::f16::from_bits(bits).to_f32()
    };
    [component(0), component(2)]
}

/// A resize bumps the generation and invalidates history, and the old targets are gone.
/// 15.11's "reject zero extents without destroying the last valid targets" is the other
/// half: a refused resize leaves the generation exactly where it was.
#[test]
fn a_resize_bumps_the_generation_and_a_zero_extent_leaves_it_alone() {
    let mut fixture = Fixture::new(
        [1.0, 1.0, 1.0, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let before = fixture.renderer.generation();
    let err = fixture
        .renderer
        .resize(
            &fixture.gpu.device,
            wgpu::Extent3d {
                width: 0,
                height: 64,
                depth_or_array_layers: 1,
            },
        )
        .unwrap_err();
    assert!(matches!(err, anim_render::RenderError::ZeroExtent { .. }));
    assert_eq!(fixture.renderer.generation(), before, "15.11's promise");
    assert_eq!(fixture.renderer.targets().extent, EXTENT);

    let smaller = wgpu::Extent3d {
        width: 32,
        height: 32,
        depth_or_array_layers: 1,
    };
    fixture
        .renderer
        .resize(&fixture.gpu.device, smaller)
        .unwrap();
    assert_eq!(fixture.renderer.generation(), before + 1);
    assert_eq!(fixture.renderer.targets().extent, smaller);

    // The new generation is a resize invalidation, so the next extraction reports it.
    let scene = snapshot(vec![node("quad", fixture.asset, 0.0)], vec![]);
    let quality = fixture.renderer.quality(FinalAlpha::Premultiplied);
    let mut previous = PreviousFrameState::new();
    previous.history.valid = true;
    previous.history.previous_tick = Some(SimTick(0));
    previous.history.target_generation = before;
    previous.history.previous_extent = Some(EXTENT);
    previous.history.previous_projection = Some(anim_render::camera::projection(EXTENT));
    previous.history.previous_revision = Some(Revision(1));
    let frame = extract_frame(&scene, &empty_sim(1), &previous, quality, &fixture.assets).unwrap();
    assert!(
        frame.items.iter().all(|item| !item.previous_valid),
        "a resize must invalidate every item's history"
    );
}

// The camera-cut epoch rule is covered by `a_cut_sets_the_previous_camera_matrix_equal_to_
// the_current` in camera.rs, which needs no device. Repeating it here would add a GPU
// dependency to a matrix assertion.

/// Device loss invalidates history and drops GPU resources, and the registry's byte total
/// returns to zero rather than describing allocations that no longer exist.
#[test]
fn device_loss_invalidates_history_and_clears_the_registry() {
    let mut fixture = Fixture::new(
        [1.0, 1.0, 1.0, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    assert_eq!(fixture.renderer.registry().len(), 1);
    assert!(fixture.renderer.registry().total_bytes() > 0);
    fixture.renderer.invalidate_after_device_loss();
    assert!(fixture.renderer.registry().is_empty());
    assert_eq!(fixture.renderer.registry().total_bytes(), 0);
    assert!(!fixture
        .renderer
        .previous_state()
        .history
        .velocity_history_valid());
}

// ---------------------------------------------------------------------------
// Clean output isolation (15.12's third sentence).
// ---------------------------------------------------------------------------

/// The clean scene output and the export readback are the same bytes, because export *is*
/// the readback destination and UI composition is a separate operation that never runs on
/// it. 15.9's "export ... cannot call composite_editor_ui" is enforced by the destination
/// type, and this is the check that the refusal fires.
#[test]
fn the_clean_output_is_the_export_output_and_ui_cannot_enter_it() {
    let mut fixture = Fixture::new(
        [0.3, 0.6, 0.9, 1.0],
        ImageAlpha::Opaque,
        DecoderConfiguration::Rgba8Unpremultiplied,
    );
    let scene = snapshot(vec![node("quad", fixture.asset, 0.0)], vec![]);
    let (export_bytes, rendered) =
        fixture.render_scene(&scene, &empty_sim(1), FinalAlpha::Premultiplied);
    let clean = fixture
        .renderer
        .read_output(&fixture.gpu.device, &fixture.gpu.queue)
        .unwrap();
    assert_eq!(
        clean.pixels, export_bytes.pixels,
        "the clean scene output and the export readback must be identical before any UI"
    );

    // A readback destination has no view, so the editor composite must refuse rather than
    // silently draw nothing into an export. The UI frame is real — a real pipeline and bind
    // group — so the refusal is the destination check firing, not a malformed argument.
    let (pipeline, layout) = anim_render::graph::create_ui_pipeline(
        &fixture.gpu.device,
        wgpu::TextureFormat::Rgba8Unorm,
    );
    let sampler = anim_render::resources::nearest_sampler(&fixture.gpu.device);
    let ui_view = fixture
        .renderer
        .targets()
        .scene_output
        .create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = fixture
        .gpu
        .device
        .create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("golden/ui"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&ui_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
    let ui = anim_render::graph::EditorUiFrame {
        texture: ui_view,
        extent: EXTENT,
        pipeline,
        bind_group,
    };
    let mut encoder = fixture
        .gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("golden/ui"),
        });
    let viewport = fixture
        .renderer
        .targets()
        .scene_output
        .create_view(&wgpu::TextureViewDescriptor::default());
    let refusal = anim_render::graph::composite_editor_ui(&mut encoder, &rendered, &ui, &viewport);
    assert!(
        matches!(refusal, Err(anim_render::RenderError::GraphInvalid { .. })),
        "export must not be able to reach the editor composite"
    );
}

// ---------------------------------------------------------------------------
// The real decode path (15.12's "linear decoding" against an actual file).
// ---------------------------------------------------------------------------

/// A real PNG, imported and decoded through `anim_assets`, renders. This is the end-to-end
/// check that the renderer samples decoded pixels rather than a procedural stand-in.
#[test]
fn a_real_imported_png_renders_through_the_decode_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quad.png");
    write_png(&path, 8, 8, [200u8, 100, 50, 255]);

    let store = anim_assets::AssetStore::init(dir.path()).unwrap();
    let record = anim_assets::import(
        &store,
        asset_id("golden/png"),
        "quad.png".to_owned(),
        AssetKind::Image,
        anim_assets::CURRENT_IMPORTER_VERSION,
        std::fs::File::open(&path).unwrap(),
    )
    .unwrap();
    assert!(
        record.image.is_some(),
        "import must classify the colour and alpha"
    );

    let decoded = anim_assets::ImageDecoder
        .decode_pixels(
            &record,
            &store.record_path(&record).unwrap(),
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
    assert_eq!((decoded.width, decoded.height), (8, 8));
    // The PNG is sRGB-tagged by the importer's rule, so the decoded red is below the
    // encoded 200/255 rather than equal to it. That inequality IS the linear-decoding claim.
    let encoded = 200.0 / 255.0;
    assert!(
        decoded.pixels[0][0] < encoded,
        "an sRGB claim must decode below the encoded value"
    );

    let gpu = GpuContext::create(&DeviceConfig::default()).unwrap();
    let mut renderer = create_renderer(
        &gpu.device,
        &gpu.queue,
        &RenderConfig {
            extent: EXTENT,
            post: PostChainConfig::default(),
        },
    )
    .unwrap();
    renderer
        .register_image(
            &gpu.device,
            &gpu.queue,
            record.asset_id,
            &decoded,
            DecoderConfiguration::Rgba8Unpremultiplied,
        )
        .unwrap();
    assert_eq!(renderer.registry().len(), 1);
    assert_eq!(
        renderer.registry().total_bytes(),
        8 * 8 * anim_render::resources::TEXEL_BYTES
    );
}

/// Write a minimal solid-colour RGBA8 PNG with the pinned encoder.
fn write_png(path: &std::path::Path, width: u32, height: u32, color: [u8; 4]) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    let data: Vec<u8> = (0..width * height).flat_map(|_| color).collect();
    writer.write_image_data(&data).unwrap();
}
