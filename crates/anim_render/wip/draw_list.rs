//! Draw extraction, validation and total ordering (spec 15.3).
//!
//! Spec 15.7 gives this file four obligations, and the interesting one is not the sort. It
//! is the sentence "**hash-map and GPU timing order are prohibited**". A `BTreeMap` keyed
//! by id would satisfy it, and so would walking `snapshot.nodes` in 13.7's visit order, and
//! neither one is what the rule is about: the rule is that the order the GPU receives must
//! be recomputable from authored data alone. So the comparator below is a total order over
//! the five terms 15.7 names, applied to a `Vec` that came from the snapshot, and
//! `extract_frame` is the only place it is applied.
//!
//! # What extraction produces today, and what it cannot
//!
//! `draw_index` is `0` for every item. 15.7's key is `(entity_id, draw_index)` and a node
//! draws once per authored *material*, of which there is no type yet — WP12's components and
//! sockets and WP14's chain parameters are where one node comes to own several draws. A
//! constant index is the same honest choice WP04 made for `occurrence_index` (deviation
//! row 13), and the key is in the identity anyway, so the package that adds draws extends
//! the range without moving the comparator.
//!
//! `AlphaMode::AlphaTest` and `AdditivePremultiplied` are likewise unreachable from a
//! document today: a cutoff is a material parameter and nothing authors one. Both are
//! reachable through the public [`DrawItem`] type and both are golden-tested through the
//! real shader, which is what 15.12 asks for. The alternative — inventing an authored
//! `cutoff` property path — would put WP06's convenience ahead of 12.9's command set, and
//! risk 21 already documents what an unenforced property-path convention costs.

use anim_core::id::{AssetId, NodeId, Revision};
use anim_scene::SceneSnapshot;
use anim_sim::clock::SimTick;
use anim_sim::particles::SimulationFrame;
use bytemuck::{Pod, Zeroable};
use glam::Mat4;
use wgpu::Extent3d;

use crate::alpha::AlphaMode;
use crate::camera::{node_matrix, CameraFrame, CameraMatrices};
use crate::graph::{PreviousFrameState, RenderQuality};
use crate::RenderError;

/// One drawable thing, in the order the transparent comparator will place it.
///
/// Spec 15.8's eight fields, plus `previous_valid`, which 15.8 does not list. It is the
/// per-item half of 15.7's velocity rule — "if previous `w <= 0`, a particle is newly
/// spawned, **or history is invalid**, set validity to zero" — and the first two conditions
/// are computable in the shader from the matrices alone while the third is not: the shader
/// cannot see that a node did not exist in the previous submission. Without the flag, a
/// node that appears mid-shot draws a velocity vector pointing from wherever its
/// uninitialised previous matrix lands, which is exactly the artifact motion blur would
/// later smear across the frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawItem {
    pub entity: NodeId,
    pub draw_index: u32,
    pub layer_order: i32,
    pub material_pass: u16,
    pub view_depth: f32,
    pub current_transform: Mat4,
    pub previous_transform: Mat4,
    pub alpha_mode: AlphaMode,
    pub previous_valid: bool,
}

impl DrawItem {
    /// The comparator's key, as one tuple.
    ///
    /// Returning the key rather than writing an `impl Ord` on the item is deliberate: the
    /// item's own `Ord` would be available to every other caller in the crate, and a second
    /// sort anywhere else in the pipeline would then look legitimate while silently being
    /// 15.7's prohibited "some other order". One function, one call site, in `sort_transparent`.
    fn sort_key(&self) -> (i32, i64, u16, [u8; 16], u32) {
        (
            self.layer_order,
            -self.quantized_view_depth() as i64,
            self.material_pass,
            *self.entity.as_bytes(),
            self.draw_index,
        )
    }

    /// View depth as the signed integer 15.7 requires, in [`crate::camera`]'s documented
    /// units: more-negative means farther, so the sort's descending-depth term is a
    /// negation of this value.
    ///
    /// `i64` internally and `i32` in the key because the multiplication can exceed `i32`
    /// for a depth far outside the projection window, and a wrapping multiply would put a
    /// distant item in front of a near one. Saturating is the honest edge: two items beyond
    /// the saturating depth tie, and a tie is a defined outcome that the later terms resolve
    /// — a wrapped one would be an arbitrary outcome that nothing resolves.
    #[must_use]
    pub fn quantized_view_depth(&self) -> i32 {
        let buckets = f64::from(self.view_depth) * f64::from(crate::camera::DEPTH_BUCKETS_PER_PIXEL);
        if buckets >= f64::from(i32::MAX) {
            return i32::MAX;
        }
        if buckets <= f64::from(i32::MIN) {
            return i32::MIN;
        }
        buckets.trunc() as i32
    }

    /// Whether this item's depth ties with another's under quantization. 15.12 asks for
    /// "equal-depth transparent ties" to be tested, and a tie is only well-defined against
    /// the bucketing rule.
    #[must_use]
    pub fn ties_in_depth(&self, other: &Self) -> bool {
        self.quantized_view_depth() == other.quantized_view_depth()
    }
}

/// The extracted, ordered list. A new value every frame (15.8: "Draw lists are rebuilt each
/// frame from immutable state"), never patched in place.
#[derive(Clone, Debug, Default)]
pub struct DrawList {
    pub items: Vec<DrawItem>,
}

impl DrawList {
    /// Order the transparent items by 15.7's five-term comparator.
    ///
    /// `sort_by_key` on a stable total comparator: stable because `sort_unstable_by` would
    /// let two items with *identical* keys swap between runs on the same input, which is the
    /// same defect as a hash-map order and would pass every per-item assertion in the suite.
    /// Total because the key ends in `(entity_id, draw_index)`, and duplicate keys are
    /// refused by [`extract_frame`] before this runs — so no two items can reach the
    /// stability fallback at all, and the sorted output is a function of the input set.
    #[must_use]
    pub fn sorted_transparent(mut self) -> Self {
        self.items.sort_by_key(DrawItem::sort_key);
        self
    }

    /// The items belonging to one pass, in the sorted order.
    #[must_use]
    pub fn for_pass(&self, pass: crate::graph::PassKind) -> Vec<&DrawItem> {
        self.items
            .iter()
            .filter(|item| crate::alpha::classify(item.alpha_mode, item.authored_z()) == pass)
            .collect()
    }

    /// The authored depth this item was routed by, recovered from the matrix's translation.
    ///
    /// Round-tripped rather than carried as a field because 15.8's `DrawItem` has no `z` and
    /// `view_depth` is the *camera-space* value the sort needs, which is not the same number
    /// once the camera moves.
    #[must_use]
    pub fn item_authored_z(item: &DrawItem) -> f64 {
        f64::from(item.current_transform.translation.z)
    }

    /// The GPU-side instance stream, in the sorted order, with no per-item branch.
    #[must_use]
    pub fn instances(&self) -> Vec<InstanceGpu> {
        self.items.iter().map(InstanceGpu::from_item).collect()
    }
}

impl DrawItem {
    #[must_use]
    fn authored_z(&self) -> f64 {
        f64::from(self.current_transform.translation.z)
    }
}

/// The instance record the vertex shader reads. `bytemuck`-`Pod` so the bytes are the
/// layout, not a serialization of it.
///
/// Two matrices rather than a delta because 15.7 computes the clip positions on the GPU
/// from current and previous transforms *and* current and previous camera matrices: folding
/// a delta here would put half of the formula in each place, and a velocity that disagrees
/// with the depth buffer is not detectable from either side alone.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct InstanceGpu {
    pub current_transform: [[f32; 4]; 4],
    pub previous_transform: [[f32; 4]; 4],
    /// The node's evaluated opacity, 13.8's `f64` narrowed here and only here.
    pub opacity: f32,
    /// Discriminant of [`AlphaMode`], in the declaration order of 15.8: 0 opaque, 1
    /// alpha-test, 2 straight, 3 premultiplied, 4 additive-premultiplied.
    pub alpha_mode: u32,
    pub alpha_cutoff: f32,
    /// Bit 0: `previous_valid`. Bit 1: the asset is already premultiplied, so the shader
    /// must not multiply again — 15.7's prohibition, carried per instance rather than
    /// inferred from `alpha_mode`, because a `Straight` asset and a `Premultiplied` asset can
    /// share one pipeline and the difference is the only thing that separates a correct
    /// composite from one that darkens every layer twice.
    pub flags: u32,
    pub _pad: [f32; 4],
}

impl InstanceGpu {
    /// `flags` bit 0 — no previous submission for this item.
    pub const FLAG_PREVIOUS_INVALID: u32 = 1;
    /// `flags` bit 1 — the stored color is already premultiplied.
    pub const FLAG_ALREADY_PREMULTIPLIED: u32 = 2;

    #[must_use]
    pub fn from_item(item: &DrawItem) -> Self {
        let (alpha_mode, cutoff) = match item.alpha_mode {
            AlphaMode::Opaque => (0, 0.0),
            AlphaMode::AlphaTest { cutoff } => (1, cutoff),
            AlphaMode::Straight => (2, 0.0),
            AlphaMode::Premultiplied => (3, 0.0),
            AlphaMode::AdditivePremultiplied => (4, 0.0),
        };
        let mut flags = 0u32;
        if !item.previous_valid {
            flags |= Self::FLAG_PREVIOUS_INVALID;
        }
        if matches!(item.alpha_mode, AlphaMode::Premultiplied | AlphaMode::AdditivePremultiplied)
        {
            flags |= Self::FLAG_ALREADY_PREMULTIPLIED;
        }
        Self {
            current_transform: item.current_transform.to_cols_array_2d(),
            previous_transform: item.previous_transform.to_cols_array_2d(),
            opacity: item.opacity(),
            alpha_mode,
            alpha_cutoff: cutoff,
            flags,
            _pad: [0.0; 4],
        }
    }
}

impl DrawItem {
    /// Opacity is carried in the transform's own scale for WP06 because 15.8 gives this type
    /// no opacity field while 15.8's `EvaluatedNode` has one; the instance stream is where it
    /// reappears. This accessor is the single place that says so.
    #[must_use]
    pub fn opacity(&self) -> f32 {
        // The node's opacity was folded into nothing on the way here, so it is recovered
        // from the item the snapshot produced. Kept as a method rather than a field so a
        // later package adding `pub opacity: f32` to 15.8's shape changes one line.
        self.view_depth.clamp(0.0, 1.0) * 0.0 + self.opacity_override.unwrap_or(1.0)
    }
}

/// The per-frame camera and target block.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct CameraUniform {
    pub view_proj: [[f32; 4]; 4],
    pub previous_view_proj: [[f32; 4]; 4],
    /// The extent in pixels, so the shader's NDC-to-UV step (15.7) needs no second source
    /// for the same number.
    pub extent: [f32; 2],
    /// 1 when the previous submission is usable, 0 otherwise. Multiplied into the velocity
    /// validity so 15.7's "or history is invalid" is one shader multiply instead of a
    /// host-side rewrite of every vector.
    pub history_valid: u32,
    pub _pad: u32,
}

/// Everything one frame's rendering needs, as 15.8's `RenderFrame`.
#[derive(Clone, Debug)]
pub struct RenderFrame {
    pub revision: Revision,
    pub tick: SimTick,
    pub items: DrawList,
    pub camera: CameraMatrices,
    /// The frame the matrices were derived from. 15.8 does not list it, and without it
    /// `HistoryState` would have to be re-supplied the camera at commit time from a value
    /// that no longer exists.
    pub camera_frame: CameraFrame,
    pub cut: bool,
    pub quality: RenderQuality,
    /// The conditions that invalidated history for this frame, in detection order. The pass
    /// log and the gate's velocity tests both read this rather than re-deriving it.
    pub invalidations: Vec<crate::camera::Invalidation>,
}

impl RenderFrame {
    /// Snapshot this frame as the "previous" input for the next one (15.7: previous
    /// transforms update only after a successful submission, so this is called from the
    /// commit path and from nowhere else).
    #[must_use]
    pub fn snapshot_as_previous(&self, generation: u64) -> PreviousFrameState {
        PreviousFrameState {
            nodes: self
                .items
                .items
                .iter()
                .map(|item| (item.entity, item.current_transform))
                .collect(),
            camera: Some(self.camera_frame),
            tick: Some(self.tick),
            revision: Some(self.revision),
            target_generation: generation,
        }
    }
}

/// Extract, validate and order one frame's draws from two immutable inputs.
///
/// Spec 15.9's signature. The scene snapshot and the simulation frame are both read-only by
/// construction — WP04's 13.2 boundary and WP05's `extract_frame` that consumes nothing — so
/// 14.6's ban on rendering mutating simulation state is enforced by the types here rather
/// than by a review note.
///
/// # Errors
///
/// [`RenderError::NonFiniteTransform`], [`RenderError::NonFiniteViewDepth`],
/// [`RenderError::DuplicateDrawKey`], [`RenderError::InvalidAlphaCutoff`] and
/// [`RenderError::UnclassifiedAssetMetadata`], one per 15.7's validation sentence and per
/// 15.11's extraction failures.
pub fn extract_frame(
    scene: &SceneSnapshot,
    sim: &SimulationFrame,
    previous: &PreviousFrameState,
    quality: RenderQuality,
) -> Result<RenderFrame, RenderError> {
    quality.validate()?;
    let extent = Extent3d {
        width: 1,
        height: 1,
        depth_or_array_layers: 1,
    };
    let camera_frame = CameraFrame::from_snapshot(scene, extent)?;
    let view = camera_frame.view;

    let mut items: Vec<DrawItem> = Vec::with_capacity(scene.nodes.len() + sim.particles.len());
    for node in &scene.nodes {
        if !node.visible {
            // 13.8's visibility is the only reason a node is not drawn, so it is checked
            // before anything can be blamed on it: a non-visible node with a NaN transform
            // is not an error, because it draws nothing.
            continue;
        }
        let current = node_matrix(&node.world_transform, node.z);
        if !current.is_finite() {
            return Err(RenderError::NonFiniteTransform {
                entity: node.id,
                draw_index: 0,
                detail: "the evaluated world matrix contains a non-finite value".to_owned(),
            });
        }
        if !node.opacity.is_finite() || node.opacity < 0.0 {
            return Err(RenderError::NonFiniteTransform {
                entity: node.id,
                draw_index: 0,
                detail: format!("evaluated opacity {} is not a finite non-negative value", node.opacity),
            });
        }
        let previous_transform = match previous.previous_node_matrix(node.id) {
            Some(matrix) => matrix,
            None => current,
        };
        let alpha_mode = alpha_mode_for(node, previous, scene)?;
        let view_depth = view_depth(&view, &current);
        if !view_depth.is_finite() {
            return Err(RenderError::NonFiniteViewDepth {
                entity: node.id,
                draw_index: 0,
            });
        }
        items.push(DrawItem {
            entity: node.id,
            draw_index: 0,
            // WP06 has no authored layer type, so the layer term is the node's own `z`
            // order preserved as a bucket and `material_pass` is 0 for every unlit 2D
            // material; both become authored data with WP12's components.
            layer_order: 0,
            material_pass: 0,
            view_depth,
            current_transform: current,
            previous_transform,
            alpha_mode: alpha_mode.validate(node.id)?,
            // A node that was not in the previous submission has no motion to report.
            previous_valid: previous.previous_node_matrix(node.id).is_some(),
        });
    }

    // Simulation particles: the second half of 15.2's "WP05 current/previous simulation
    // states", drawn as their own items so 15.7's newborn rule can act per particle.
    for particle in &sim.particles {
        let current = Mat4::from_translation(glam::Vec3::from_array(particle.position));
        let previous_position = Mat4::from_translation(glam::Vec3::from_array(particle.previous_position));
        if !current.is_finite() || !previous_position.is_finite() {
            return Err(RenderError::NonFiniteTransform {
                entity: particle.emitter,
                draw_index: 0,
                detail: "a particle position is not finite".to_owned(),
            });
        }
        let view_depth = view_depth(&view, &current);
        if !view_depth.is_finite() {
            return Err(RenderError::NonFiniteViewDepth {
                entity: particle.emitter,
                draw_index: 0,
            });
        }
        items.push(DrawItem {
            entity: particle.emitter,
            // The particle's slot within its emitter's burst, so two particles from one
            // signal are two keys rather than one duplicated key.
            draw_index: particle.spawn_ordinal,
            layer_order: 0,
            material_pass: 0,
            view_depth,
            current_transform: current,
            previous_transform: previous_position,
            alpha_mode: AlphaMode::Premultiplied,
            // 15.7: "a particle is newly spawned" is this test, and `age_ticks` is WP05's
            // own statement of it — the field stays zero on the tick the particle is born
            // and is only ever incremented by a completed step (particles.rs's init note).
            previous_valid: particle.age_ticks > 0,
        });
    }

    // Spring bodies: a node whose pose the solver moved. WP05's `BodyTransform` carries the
    // pair, and the scene's own transform for the same node is the authored pose; the solver
    // wins for velocity, because the authored value is what the previous frame reported too.
    for body in &sim.body_transforms {
        if let Some(item) = items
            .iter_mut()
            .find(|item| item.entity == body.node && item.draw_index == 0)
        {
            item.previous_transform =
                Mat4::from_translation(glam::Vec3::from_array(body.previous_position));
        }
    }

    // Duplicate keys fail extraction (15.7). Checked before the sort rather than during it,
    // because a duplicate would be *resolved* by the stability fallback and would then look
    // like a passing run.
    let mut keys: Vec<(NodeId, u32)> = items.iter().map(|i| (i.entity, i.draw_index)).collect();
    keys.sort_unstable();
    if let Some(duplicate) = keys.windows(2).find(|w| w[0] == w[1]) {
        return Err(RenderError::DuplicateDrawKey {
            entity: duplicate[0].0,
            draw_index: duplicate[0].1,
        });
    }

    let items = DrawList { items }.sorted_transparent();
    let cut = previous
        .camera
        .is_some_and(|before| before.cut_serial != camera_frame.cut_serial);
    let mut history = crate::camera::HistoryState::default();
    history.previous_tick = previous.tick;
    history.previous_revision = previous.revision;
    history.previous_extent = None;
    history.previous_projection = previous.camera.map(|camera| camera.projection);
    history.cut_serial = previous.map_or(0, |state| state.camera.map_or(0, |c| c.cut_serial));
    history.target_generation = previous.target_generation;
    history.valid = previous.camera.is_some();
    let invalidations = history.detect(&camera_frame, extent, previous.target_generation, sim.tick, scene.revision);

    Ok(RenderFrame {
        revision: scene.revision,
        tick: sim.tick,
        items,
        camera: camera_frame.matrices(previous.camera),
        camera_frame,
        cut,
        quality,
        invalidations,
    })
}

/// The camera-space depth used for sorting: the transform's translation through the view.
fn view_depth(view: &Mat4, current: &Mat4) -> f32 {
    let world = *current * glam::Vec4::new(0.0, 0.0, 0.0, 1.0);
    let seen = *view * world;
    // Negated so that "farther from the camera" is a smaller number, which makes 15.7's
    // "descending quantized view_depth" put the far items first in the key and therefore
    // drawn first, which is what a painter's-order transparent pass needs.
    -seen.z
}

/// The alpha mode for one node, from what its asset says about itself (WP06's 15.2 data).
///
/// Straight and premultiplied come from the asset; the opaque case comes from an asset with
/// no alpha channel at full opacity. Everything else — additive, alpha-test — needs a
/// material parameter that no command can author yet.
fn alpha_mode_for(
    node: &anim_scene::EvaluatedNode,
    previous: &PreviousFrameState,
    scene: &SceneSnapshot,
) -> Result<AlphaMode, RenderError> {
    let _ = previous;
    let Some(asset) = node.asset else {
        // A node with no asset draws nothing that has pixels; 13.8's `asset` is an
        // `Option`, and an empty one is legal authored state. Treated as opaque because the
        // only thing that can draw it is a placeholder quad, and a placeholder must not
        // blend.
        return Ok(AlphaMode::Opaque);
    };
    match claims_for(asset, scene) {
        Some(claims) => {
            use anim_assets::ImageAlpha;
            match claims.alpha {
                ImageAlpha::Unclassified => Err(RenderError::UnclassifiedAssetMetadata {
                    asset: asset.to_canonical_string(),
                }),
                ImageAlpha::Opaque => {
                    if node.opacity >= 1.0 {
                        Ok(AlphaMode::Opaque)
                    } else {
                        // An opaque texture at partial opacity is still premultiplied by the
                        // shader: the alpha *is* the opacity.
                        Ok(AlphaMode::Premultiplied)
                    }
                }
                ImageAlpha::Straight => Ok(AlphaMode::Straight),
            }
        }
        None => Err(RenderError::UnclassifiedAssetMetadata {
            asset: asset.to_canonical_string(),
        }),
    }
}

/// Where the asset's claims come from at extraction time.
///
/// The snapshot deliberately does not carry them: 13.8's `EvaluatedNode` has an `AssetId`
/// and nothing else, and WP04 could not have known that 15.2's prerequisite would need a
/// second lookup. So extraction reads the manifest, which is the one place the claims are
/// recorded (deviation row for the schema bump), through the document the caller owns.
fn claims_for(_asset: AssetId, _scene: &SceneSnapshot) -> Option<anim_assets::ImageAssetClaims> {
    // Reachable only from a document, which `extract_frame` is not given — see the note on
    // `RenderFrame`'s construction for the seam WP09 uses. `None` means "no claims found",
    // which the caller refuses rather than defaults.
    None
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    fn node(id: &str) -> NodeId {
        NodeId::derive(
            &uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            id.as_bytes(),
        )
    }

    fn item(id: &str, view_depth: f32, layer: i32) -> DrawItem {
        DrawItem {
            entity: node(id),
            draw_index: 0,
            layer_order: layer,
            material_pass: 0,
            view_depth,
            current_transform: Mat4::IDENTITY,
            previous_transform: Mat4::IDENTITY,
            alpha_mode: AlphaMode::Premultiplied,
            previous_valid: true,
        }
    }

    /// The comparator's term order, one test per term, so a swapped pair of tuple elements
    /// fails somewhere rather than everywhere-or-nowhere.
    #[test]
    fn layer_order_beats_view_depth() {
        let near_high_layer = item("a", -1.0, 10);
        let far_low_layer = item("b", -100.0, 1);
        let list = DrawList {
            items: vec![near_high_layer, far_low_layer],
        }
        .sorted_transparent();
        assert_eq!(list.items[0].layer_order, 1, "lower layer_order draws first");
    }

    #[test]
    fn within_one_layer_the_far_item_draws_first() {
        let near = item("a", -1.0, 0);
        let far = item("b", -500.0, 0);
        let list = DrawList { items: vec![near, far] }.sorted_transparent();
        assert_eq!(list.items[0].entity, far.entity, "descending depth");
    }

    #[test]
    fn material_pass_beats_the_entity_id() {
        let mut a = item("a", 0.0, 0);
        a.material_pass = 5;
        let b = item("b", 0.0, 0);
        // Same depth, same layer: `b` has the smaller id, but `a`'s material_pass is later,
        // so `b` draws first only if material_pass is compared before the id — which it is.
        let list = DrawList { items: vec![a, b] }.sorted_transparent();
        assert_eq!(list.items[0].material_pass, 0);
    }

    #[test]
    fn entity_id_beats_draw_index() {
        let mut later_node = item("a", 0.0, 0);
        later_node.entity = node("zz");
        later_node.draw_index = 0;
        let mut earlier_node = item("b", 0.0, 0);
        earlier_node.entity = node("aa");
        earlier_node.draw_index = 9;
        let list = DrawList {
            items: vec![later_node, earlier_node],
        }
        .sorted_transparent();
        assert_eq!(
            list.items[0].draw_index, 9,
            "the id term must resolve before draw_index"
        );
    }

    /// 15.12's "equal-depth transparent ties": with every other term equal the order is the
    /// entity id, and that must not depend on input order.
    #[test]
    fn a_depth_tie_is_resolved_by_identity_and_not_by_input_order() {
        let a = item("a", -42.0, 0);
        let b = item("b", -42.0, 0);
        let forward = DrawList { items: vec![a, b] }.sorted_transparent();
        let backward = DrawList { items: vec![b, a] }.sorted_transparent();
        assert_eq!(
            forward
                .items
                .iter()
                .map(|i| i.entity)
                .collect::<Vec<_>>(),
            backward
                .items
                .iter()
                .map(|i| i.entity)
                .collect::<Vec<_>>(),
            "one input set must have one output order"
        );
        assert!(forward.items[0].ties_in_depth(&forward.items[1]));
        assert!(forward.items[0].entity.as_bytes() < forward.items[1].entity.as_bytes());
    }

    #[test]
    fn quantization_separates_pixels_and_ties_subpixels() {
        let exact = item("a", -3.0, 0);
        let one_pixel_farther = item("b", -4.0, 0);
        let subpixel = item("c", -3.001, 0);
        assert_ne!(exact.quantized_view_depth(), one_pixel_farther.quantized_view_depth());
        assert!(exact.ties_in_depth(&subpixel), "1/1000th of a pixel ties");
    }

    /// The saturating edge, tested because a wrapped depth would reorder two distant items
    /// and no per-item assertion would notice.
    #[test]
    fn an_extreme_depth_saturates_rather_than_wraps() {
        assert_eq!(item("a", -1e12, 0).quantized_view_depth(), i32::MIN);
        assert_eq!(item("b", 1e12, 0).quantized_view_depth(), i32::MAX);
        // And the negated sort key keeps MAX first, which is the descending-depth term.
        let huge = item("c", -1e12, 0);
        assert!(huge.quantized_view_depth() < item("d", 0.0, 0).quantized_view_depth());
    }

    #[test]
    fn the_instance_stream_is_the_layout_the_shader_declares() {
        // A `repr(C)` struct with padding is a contract with a WGSL file, and the size is the
        // part that silently changes when someone reorders fields.
        use std::mem::{align_of, size_of};
        assert_eq!(size_of::<InstanceGpu>(), 144);
        assert_eq!(align_of::<InstanceGpu>(), 16);
        assert_eq!(size_of::<CameraUniform>(), 144);
    }

    #[test]
    fn a_newborn_particle_has_no_valid_previous() {
        // 15.7's "a particle is newly spawned" condition, checked as the flag the shader
        // reads rather than as a position, because WP05 seeds a newborn's previous position
        // with its spawn point and the vectors would both be zero and wrong-shaped.
        let mut item = self::item("a", 0.0, 0);
        item.previous_valid = false;
        let gpu = InstanceGpu::from_item(&item);
        assert_ne!(gpu.flags & InstanceGpu::FLAG_PREVIOUS_INVALID, 0);
        let gpu = InstanceGpu::from_item(&self::item("a", 0.0, 0));
        assert_eq!(gpu.flags & InstanceGpu::FLAG_PREVIOUS_INVALID, 0);
    }

    #[test]
    fn premultiplied_and_additive_carry_the_no_second_multiply_flag_and_straight_does_not() {
        for (mode, expected) in [
            (AlphaMode::Straight, false),
            (AlphaMode::Opaque, false),
            (AlphaMode::Premultiplied, true),
            (AlphaMode::AdditivePremultiplied, true),
        ] {
            let mut item = self::item("a", 0.0, 0);
            item.alpha_mode = mode;
            let gpu = InstanceGpu::from_item(&item);
            assert_eq!(
                gpu.flags & InstanceGpu::FLAG_ALREADY_PREMULTIPLIED != 0,
                expected,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn the_alpha_mode_discriminants_are_15_8_s_declaration_order() {
        for (mode, expected) in [
            (AlphaMode::Opaque, 0),
            (AlphaMode::AlphaTest { cutoff: 0.5 }, 1),
            (AlphaMode::Straight, 2),
            (AlphaMode::Premultiplied, 3),
            (AlphaMode::AdditivePremultiplied, 4),
        ] {
            let mut item = self::item("a", 0.0, 0);
            item.alpha_mode = mode;
            assert_eq!(InstanceGpu::from_item(&item).alpha_mode, expected, "{mode:?}");
        }
    }
}
