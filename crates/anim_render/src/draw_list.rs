//! Draw extraction, validation and total ordering (spec 15.3).
//!
//! # The order is the product
//!
//! Spec 15.7 gives the comparator exactly: `(layer_order, descending quantized_view_depth,
//! material_pass, entity_id, draw_index)`, stable and total, with "hash-map and GPU timing
//! order prohibited". [`total_cmp`] is that tuple and nothing else. It is applied to the
//! whole extracted list rather than only to the transparent subset, because a total order
//! over everything costs one sort and makes the opaque pass's submission order reproducible
//! too — which matters for a command log even where the depth test makes it invisible.
//!
//! **View depth is distance along the camera's forward axis, larger meaning farther.** The
//! authored convention in `camera.rs` is +z toward the viewer, so view depth is the negated
//! view-space z. That sign is what makes 15.7's *descending* sort back-to-front: the largest
//! distance is drawn first. Reading "depth" as the authored z instead would sort
//! front-to-back, which blends to a different image and is finite and plausible at every
//! step — the defect this comment exists to prevent.
//!
//! # Where the claims come from
//!
//! DECISIONS.md #41 ruling 1: [`extract_frame`] takes a fifth parameter, `assets:
//! &AssetManifest`, because the colour and alpha a `DrawItem` needs live on
//! `AssetManifestEntry.image` and neither a `SceneSnapshot` nor a `SimulationFrame` carries
//! them. The lookup rules are that entry's table, and each has a producing test below.
//!
//! # What WP06 can author, and what it cannot
//!
//! `ImageAlpha` has three variants and `AlphaMode` has five. Extraction maps `Opaque` to
//! `AlphaMode::Opaque` and `Straight` to `AlphaMode::Straight`; `AlphaTest`, `Premultiplied`
//! and `AdditivePremultiplied` need a cutoff or a premultiplication intent that no authored
//! field carries yet. They are reachable — [`crate::alpha::blend_descriptor`] is total over
//! all five and `RenderFrame`'s fields are public, so the goldens exercise them — but they
//! become *authorable* with WP12's material contract. [`crate::alpha::mode_from_claims`] is
//! the one place that extension happens.

use std::collections::BTreeMap;

use anim_assets::{AssetKind, AssetManifest, ImageAlpha, ImageColorSpace};
use anim_core::id::{AssetId, NodeId, Revision};
use anim_scene::SceneSnapshot;
use anim_sim::particles::BodyTransform;
use anim_sim::{Particle, SimTick, SimulationFrame};
use glam::{Mat4, Vec3};

use crate::alpha::{self, AlphaMode};
use crate::camera::{
    node_matrix, CameraFrame, CameraMatrices, HistoryState, Invalidation, DEPTH_BUCKETS_PER_PIXEL,
};
use crate::graph::PassKind;
use crate::{RenderError, RenderQuality, Result};

/// The sort's last-resort identity, and the key previous transforms are stored under.
pub type DrawKey = (NodeId, u32);

/// Spec 15.8's `DrawItem`, plus three fields the spec's own algorithms need.
///
/// Additions, each recorded rather than smuggled:
///  * `previous_valid` — DECISIONS.md #40 §7. 15.7's per-item "history is invalid" condition
///    is invisible to a shader that cannot know whether the node existed last submission.
///  * `asset` — the texture to bind. 15.8's list has no asset field, but the sort key is
///    `(entity, draw_index)` and neither names an image, so without it the renderer cannot
///    find the pixels for the item it just sorted.
///  * `opacity` — `EvaluatedNode::opacity` (13.8) has to reach the blend. The parked draft
///    read a field `opacity_override` that was never added; this is the field it meant.
#[derive(Clone, Debug, PartialEq)]
pub struct DrawItem {
    /// The owning node, or the derived identity of a particle.
    pub entity: NodeId,
    /// Unique within `entity`; 0 for a node's single quad.
    pub draw_index: u32,
    /// Project layer order — the node's position in WP04's canonical visit order.
    pub layer_order: i32,
    /// Material grouping key. With no material type yet this is the alpha mode's stable id.
    pub material_pass: u16,
    /// Distance along the camera's forward axis, larger meaning farther.
    pub view_depth: f32,
    /// This frame's world transform.
    pub current_transform: Mat4,
    /// The previous submission's transform for the same key.
    pub previous_transform: Mat4,
    /// Blend and discard behaviour.
    pub alpha_mode: AlphaMode,
    /// Whether this item's history is usable (15.7, #40 §7).
    pub previous_valid: bool,
    /// The image to sample.
    pub asset: AssetId,
    /// Authored opacity, folded into the item uniform's tint alpha.
    pub opacity: f32,
    /// Which of 15.6's three content classes draws this item. Decided at extraction from
    /// the alpha mode and the authored depth, so the graph groups rather than re-derives.
    pub pass: PassKind,
}

impl DrawItem {
    /// The key the sort breaks ties on and the previous-transform map is keyed by.
    #[must_use]
    pub const fn key(&self) -> DrawKey {
        (self.entity, self.draw_index)
    }

    /// 15.7's quantization: signed `i32` in documented camera units.
    ///
    /// The unit is [`DEPTH_BUCKETS_PER_PIXEL`], declared in `camera.rs` so the constant and
    /// the projection that produces the depth are documented in one place.
    #[must_use]
    pub fn quantized_view_depth(&self) -> i32 {
        (self.view_depth * DEPTH_BUCKETS_PER_PIXEL) as i32
    }
}

/// The total comparator of spec 15.7.
///
/// A free function over two items rather than an `Ord` impl on `DrawItem`: `DrawItem` holds
/// `f32` and `Mat4`, so a derived `Ord` would not exist and a hand-written one would invite
/// `partial_cmp` on the raw float. Every float that reaches this comparator has already been
/// quantized to `i32` or validated finite, so the comparison is over integers and ids only.
#[must_use]
pub fn total_cmp(a: &DrawItem, b: &DrawItem) -> std::cmp::Ordering {
    a.layer_order
        .cmp(&b.layer_order)
        // Descending: larger distance first, which is back-to-front. See the module comment.
        .then_with(|| b.quantized_view_depth().cmp(&a.quantized_view_depth()))
        .then_with(|| a.material_pass.cmp(&b.material_pass))
        .then_with(|| a.entity.cmp(&b.entity))
        .then_with(|| a.draw_index.cmp(&b.draw_index))
}

/// What the previous submission left behind, as 15.9's `extract_frame` receives it.
///
/// 15.9 names this type and 15.8 gives it no fields. The two here are exactly what 15.7's
/// velocity and history rules read: the [`HistoryState`] whose `detect` decides validity,
/// and the per-key transforms whose absence means "this item did not exist last submission".
#[derive(Clone, Debug, Default)]
pub struct PreviousFrameState {
    /// The history the last successful submission committed.
    pub history: HistoryState,
    /// Previous world transforms, keyed by `(entity, draw_index)`.
    pub transforms: BTreeMap<DrawKey, Mat4>,
}

impl PreviousFrameState {
    /// An empty state: no previous submission, so history starts invalid.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember a submitted frame's transforms.
    ///
    /// Called **only after a successful submission**, from `graph.rs`, which is what makes
    /// 15.7's "previous transforms update only after successful submission" structural. A
    /// frame that failed to submit leaves this map describing the last thing the GPU
    /// actually completed.
    pub fn record(&mut self, frame: &RenderFrame) {
        self.transforms.clear();
        for item in &frame.items {
            self.transforms.insert(item.key(), item.current_transform);
        }
    }

    /// The previous transform for a key, if that item was drawn last submission.
    #[must_use]
    pub fn transform(&self, key: DrawKey) -> Option<Mat4> {
        self.transforms.get(&key).copied()
    }

    /// The previous camera, reconstructed for [`CameraFrame::matrices`].
    ///
    /// `None` when history is invalid, which makes `matrices` set previous equal to current
    /// and the camera's own velocity zero — the correct reading of an unusable history.
    ///
    /// 15.8's `HistoryState` stores `previous_view_proj`, the *product*, not the view and
    /// projection factors, so `view` is not reconstructible. It is set to identity here and
    /// never read: `matrices` uses only `view_proj` and `cut_serial`. The projection is
    /// carried because `HistoryState` records it for the projection-change test.
    #[must_use]
    pub fn history_camera_frame(&self) -> Option<CameraFrame> {
        if !self.history.valid {
            return None;
        }
        Some(CameraFrame {
            view: Mat4::IDENTITY,
            projection: self.history.previous_projection.unwrap_or(Mat4::IDENTITY),
            view_proj: self.history.previous_view_proj,
            cut_serial: self.history.cut_serial,
        })
    }
}

/// Spec 15.8's `RenderFrame`, verbatim.
#[derive(Clone, Debug)]
pub struct RenderFrame {
    /// The document revision the snapshot was evaluated at.
    pub revision: Revision,
    /// The simulation tick this frame presents.
    pub tick: SimTick,
    /// Every drawable, in the total order of [`total_cmp`].
    pub items: Vec<DrawItem>,
    /// Current and previous view-projection.
    pub camera: CameraMatrices,
    /// Whether a camera cut fell in this frame (15.7's history condition).
    pub cut: bool,
    /// Whether this frame's temporal history is usable, from 15.7's full condition list.
    ///
    /// Carried in the frame rather than read back from `PreviousFrameState` at draw time:
    /// the stored verdict describes the *previous* frame, so reading it here would zero the
    /// velocity of the frame right after the first submission — the one frame that first
    /// has a contiguous history to use.
    pub history_valid: bool,
    /// The resolved profile this frame was extracted at.
    pub quality: RenderQuality,
}

/// Extract a frame from immutable scene and simulation data (spec 15.9, widened by #41).
///
/// The fifth parameter is the manifest the colour and alpha claims live on. Nothing here
/// mutates: `SceneSnapshot` and `SimulationFrame` are immutable, the simulation is a separate
/// extraction (WP05), and 14.6's ban on rendering mutating simulation state is satisfied by
/// construction rather than by a discipline note.
///
/// # Errors
///
/// Every refusal 15.11 lists for extraction — non-finite transforms, non-finite view depths,
/// duplicate sort identities, invalid alpha cutoffs — plus the four manifest outcomes of
/// DECISIONS.md #41's table.
pub fn extract_frame(
    scene: &SceneSnapshot,
    sim: &SimulationFrame,
    previous: &PreviousFrameState,
    quality: RenderQuality,
    assets: &AssetManifest,
) -> Result<RenderFrame> {
    let camera = CameraFrame::from_snapshot(scene, quality.extent)?;
    let paired = camera.matrices(previous.history_camera_frame());

    let conditions = previous.history.detect(
        &camera,
        quality.extent,
        quality.target_generation,
        sim.tick,
        scene.revision,
    );
    let history_valid = !conditions.iter().any(|c| c.invalidates());
    let cut = conditions.contains(&Invalidation::CameraCut);

    // Bodies are looked up by node once, not per particle: `SimulationFrame.body_transforms`
    // is in `SimState::bodies` order, which is not the snapshot's node order.
    let bodies: BTreeMap<NodeId, BodyTransform> = sim
        .body_transforms
        .iter()
        .map(|body| (body.node, *body))
        .collect();

    let mut items = Vec::new();
    let mut layer_order = 0i32;
    let mut node_assets: BTreeMap<NodeId, AssetId> = BTreeMap::new();

    for node in scene.nodes.iter() {
        let order = layer_order;
        layer_order = layer_order.saturating_add(1);
        let Some(asset) = node.asset else {
            // A group node: no drawable content, but its transform is already composed into
            // its children by WP04, so skipping it loses nothing.
            continue;
        };
        node_assets.insert(node.id, asset);
        if !node.visible {
            continue;
        }
        let mode = claims_mode(assets, node.id, asset)?;
        let mut current = node_matrix(&node.world_transform, node.z);
        let mut previous_transform = previous.transform((node.id, 0)).unwrap_or(current);
        let mut item_valid = history_valid;
        if let Some(body) = bodies.get(&node.id) {
            // WP05's spring bodies do not overwrite authored transforms, so WP06 composes:
            // the solver owns where the body is, the authored matrix owns its rotation and
            // scale. Replacing the whole matrix would discard an authored rotation.
            current = with_translation(current, body.position);
            previous_transform = with_translation(current, body.previous_position);
            // A body that did not exist last submission has no previous position to trust.
            item_valid = history_valid && previous.transform((node.id, 0)).is_some();
        }
        if previous.transform((node.id, 0)).is_none() {
            // Topology discontinuity: the item is new, so its previous transform is a
            // fiction and its velocity must read zero rather than a jump from the origin.
            item_valid = false;
            previous_transform = current;
        }
        items.push(make_item(
            node.id,
            0,
            order,
            asset,
            mode,
            node.opacity,
            current,
            previous_transform,
            item_valid,
            &camera,
            node.z,
        )?);
    }

    for particle in &sim.particles {
        if particle.is_expired() {
            continue;
        }
        // A particle draws its emitter's image. An emitter with no image has no content to
        // draw, and inventing a white quad would be the silent substitute 15.11 forbids.
        let Some(asset) = node_assets.get(&particle.emitter).copied() else {
            continue;
        };
        let mode = claims_mode(assets, particle.emitter, asset)?;
        let entity = particle_node(particle);
        let current = Mat4::from_translation(Vec3::from_array(particle.position))
            * Mat4::from_scale(Vec3::splat(particle.size.max(0.0)));
        let key = (entity, 0);
        let existed = previous.transform(key).is_some();
        // 15.7: "a particle is newly spawned" sets validity to zero. `age_ticks == 0` is
        // WP05's own statement of newborn, so the two crates agree without a second rule.
        let newborn = particle.age_ticks == 0;
        let item_valid = history_valid && existed && !newborn;
        let previous_transform = if item_valid {
            previous.transform(key).unwrap_or(current)
        } else {
            current
        };
        let order = scene
            .nodes
            .iter()
            .position(|node| node.id == particle.emitter)
            .map_or(0, |index| index as i32);
        items.push(make_item(
            entity,
            0,
            order,
            asset,
            mode,
            1.0,
            current,
            previous_transform,
            item_valid,
            &camera,
            f64::from(particle.position[2]),
        )?);
    }

    validate_unique(&items)?;
    items.sort_by(total_cmp);
    Ok(RenderFrame {
        revision: scene.revision,
        tick: sim.tick,
        items,
        camera: paired,
        cut,
        history_valid,
        quality,
    })
}

/// The stable node identity of a particle.
///
/// A particle has no authored `NodeId`: `anim_sim::Particle` carries a stable `u128` derived
/// from `(emitter, generation, spawn_ordinal)`. 15.8 requires `DrawItem::entity` to be a
/// `NodeId`, and the key has to be stable *across* frames or the previous-transform lookup
/// misses and every particle reports invalid history forever. Deriving a v5 UUID from the
/// emitter's own identity and those 16 stable bytes is WP01's `derive(namespace, name)`
/// pattern with no new entropy (spec 6.3), and it makes a collision between two emitters'
/// particles impossible rather than merely unlikely.
#[must_use]
pub fn particle_node(particle: &Particle) -> NodeId {
    NodeId::derive(&particle.emitter.uuid(), &particle.id.to_le_bytes())
}

/// Replace a matrix's translation column, keeping its rotation and scale.
fn with_translation(matrix: Mat4, position: [f32; 3]) -> Mat4 {
    let mut out = matrix;
    out.w_axis = Vec3::from_array(position).extend(matrix.w_axis.w);
    out
}

/// Build one item, validating what 15.7 says extraction validates.
#[allow(
    clippy::too_many_arguments,
    reason = "the fields are 15.8's DrawItem plus the camera the depth is measured against; a builder would hide which of them is validated here"
)]
fn make_item(
    entity: NodeId,
    draw_index: u32,
    layer_order: i32,
    asset: AssetId,
    mode: AlphaMode,
    opacity: f64,
    current: Mat4,
    previous_transform: Mat4,
    previous_valid: bool,
    camera: &CameraFrame,
    authored_z: f64,
) -> Result<DrawItem> {
    let mode = mode.validate(entity)?;
    if !current.is_finite() {
        return Err(RenderError::NonFiniteTransform {
            entity,
            draw_index,
            detail: "the current world transform is not finite".to_owned(),
        });
    }
    if !previous_transform.is_finite() {
        return Err(RenderError::NonFiniteTransform {
            entity,
            draw_index,
            detail: "the previous world transform is not finite".to_owned(),
        });
    }
    if !opacity.is_finite() {
        return Err(RenderError::NonFiniteTransform {
            entity,
            draw_index,
            detail: format!("opacity {opacity} is not finite"),
        });
    }
    let position = current.transform_point3(Vec3::ZERO);
    let view_position = camera.view.transform_point3(position);
    // Negated: the authored convention is +z toward the viewer, and view depth is distance
    // along the forward axis so that 15.7's descending sort is back-to-front.
    let view_depth = -view_position.z;
    if !view_depth.is_finite() {
        return Err(RenderError::NonFiniteViewDepth { entity, draw_index });
    }
    Ok(DrawItem {
        entity,
        draw_index,
        layer_order,
        material_pass: alpha::mode_id(mode),
        view_depth,
        current_transform: current,
        previous_transform,
        alpha_mode: mode,
        previous_valid,
        asset,
        opacity: opacity as f32,
        pass: alpha::classify(mode, authored_z),
    })
}

/// The manifest lookup of DECISIONS.md #41 ruling 1, as one function with four refusals.
fn claims_mode(assets: &AssetManifest, entity: NodeId, asset: AssetId) -> Result<AlphaMode> {
    let entry = assets
        .get(&asset)
        .ok_or_else(|| RenderError::MissingAsset {
            entity,
            asset: asset.to_canonical_string(),
        })?;
    if entry.kind != AssetKind::Image {
        return Err(RenderError::NotAnImageAsset {
            entity,
            asset: asset.to_canonical_string(),
            kind: entry.kind.as_str().to_owned(),
        });
    }
    let claims = entry.image.ok_or_else(|| RenderError::NotAnImageAsset {
        entity,
        asset: asset.to_canonical_string(),
        kind: entry.kind.as_str().to_owned(),
    })?;
    if claims.color_space == ImageColorSpace::Unclassified
        || claims.alpha == ImageAlpha::Unclassified
    {
        return Err(RenderError::UnclassifiedAssetMetadata {
            asset: asset.to_canonical_string(),
        });
    }
    Ok(alpha::mode_from_claims(claims))
}

/// 15.7: "Duplicate keys fail extraction."
fn validate_unique(items: &[DrawItem]) -> Result<()> {
    let mut seen: BTreeMap<DrawKey, ()> = BTreeMap::new();
    for item in items {
        if seen.insert(item.key(), ()).is_some() {
            return Err(RenderError::DuplicateDrawKey {
                entity: item.entity,
                draw_index: item.draw_index,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a fixture that cannot be built makes the scenario unreadable, which is a panic rather than a test outcome"
    )]

    use super::*;
    use anim_assets::{AssetManifestEntry, ContentHash, ImageAssetClaims};
    use anim_core::id::SceneId;
    use anim_core::rational::Rational;
    use anim_core::time::FramePos;
    use anim_scene::{
        AudioClip, AudioCue, EvaluatedCamera, EvaluatedNode, SceneEvent, SimulationInput,
    };
    use std::sync::Arc;
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn node_id(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    fn asset_id(label: &str) -> AssetId {
        AssetId::derive(&namespace(), label.as_bytes())
    }

    fn quality() -> RenderQuality {
        RenderQuality {
            extent: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            target_generation: 1,
            final_alpha: crate::FinalAlpha::Premultiplied,
        }
    }

    /// A manifest entry with classified claims, built through the validating constructor.
    fn entry(asset: AssetId, alpha: ImageAlpha, space: ImageColorSpace) -> AssetManifestEntry {
        AssetManifestEntry::new(
            asset,
            ContentHash::from_bytes([7u8; 32]),
            16,
            AssetKind::Image,
            1,
            [],
            "fixture",
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

    fn snapshot(nodes: Vec<EvaluatedNode>) -> SceneSnapshot {
        SceneSnapshot {
            scene_id: SceneId::derive(&namespace(), b"scene"),
            revision: Revision(1),
            position: FramePos::at_frame(0),
            time: Rational::from_i128(0),
            nodes: Arc::from(nodes.into_boxed_slice()),
            cameras: Arc::from(Vec::<EvaluatedCamera>::new().into_boxed_slice()),
            audio_cues: Arc::from(Vec::<AudioCue>::new().into_boxed_slice()),
            simulation_inputs: Arc::from(Vec::<SimulationInput>::new().into_boxed_slice()),
            events: Arc::from(Vec::<SceneEvent>::new().into_boxed_slice()),
            audio_clips: Arc::from(Vec::<AudioClip>::new().into_boxed_slice()),
        }
    }

    fn node(label: &str, asset: Option<AssetId>, z: f64) -> EvaluatedNode {
        EvaluatedNode {
            id: node_id(label),
            parent: None,
            world_transform: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            z,
            opacity: 1.0,
            visible: true,
            asset,
        }
    }

    fn empty_sim() -> SimulationFrame {
        SimulationFrame {
            tick: SimTick(1),
            particles: Vec::new(),
            body_transforms: Vec::new(),
            effects: Vec::new(),
        }
    }

    /// The comparator is 15.7's tuple, in 15.7's order, with the depth sign that makes
    /// descending mean back-to-front. Each key is tested by holding the others equal.
    #[test]
    fn the_comparator_is_15_7_s_tuple_in_15_7_s_order() {
        let base = DrawItem {
            entity: node_id("a"),
            draw_index: 0,
            layer_order: 0,
            material_pass: 0,
            view_depth: 0.0,
            current_transform: Mat4::IDENTITY,
            previous_transform: Mat4::IDENTITY,
            alpha_mode: AlphaMode::Opaque,
            previous_valid: true,
            asset: asset_id("tex"),
            opacity: 1.0,
            pass: PassKind::Opaque,
        };
        // layer_order dominates everything after it.
        let later_layer = DrawItem {
            layer_order: 1,
            view_depth: 1000.0,
            entity: node_id("zzz"),
            ..base.clone()
        };
        assert_eq!(total_cmp(&base, &later_layer), std::cmp::Ordering::Less);
        // Within a layer, larger view depth (farther) sorts first.
        let farther = DrawItem {
            view_depth: 10.0,
            ..base.clone()
        };
        assert_eq!(
            total_cmp(&farther, &base),
            std::cmp::Ordering::Less,
            "descending depth means the farther item is drawn first"
        );
        // Equal depth falls through to material_pass.
        let later_pass = DrawItem {
            material_pass: 1,
            ..base.clone()
        };
        assert_eq!(total_cmp(&base, &later_pass), std::cmp::Ordering::Less);
        // Then entity id. NodeId's Ord is over the 16 UUID bytes (WP01), not over the label
        // the UUID was derived from, so the expected direction comes from NodeId::cmp —
        // which is the rule 15.7 names, not a guess about which label sorts first.
        let later_entity = DrawItem {
            entity: node_id("b"),
            ..base.clone()
        };
        assert_eq!(
            total_cmp(&base, &later_entity),
            base.entity.cmp(&later_entity.entity)
        );
        // Then draw_index, which is what makes the order total.
        let later_index = DrawItem {
            draw_index: 1,
            ..base.clone()
        };
        assert_eq!(total_cmp(&base, &later_index), std::cmp::Ordering::Less);
        assert_eq!(total_cmp(&base, &base), std::cmp::Ordering::Equal);
    }

    /// Two items whose depths differ by less than a bucket tie, and the tie is then broken
    /// by entity id rather than by insertion order. This is 15.12's "equal-depth
    /// transparent ties" case at the comparator level.
    #[test]
    fn an_equal_depth_tie_is_broken_by_identity_not_by_insertion() {
        let mut a = DrawItem {
            entity: node_id("a"),
            draw_index: 0,
            layer_order: 0,
            material_pass: 0,
            view_depth: 1.0,
            current_transform: Mat4::IDENTITY,
            previous_transform: Mat4::IDENTITY,
            alpha_mode: AlphaMode::Straight,
            previous_valid: true,
            asset: asset_id("tex"),
            opacity: 1.0,
            pass: PassKind::Transparent,
        };
        let mut b = DrawItem {
            entity: node_id("b"),
            // A sub-bucket difference: 1/512 of a pixel at 256 buckets per pixel.
            view_depth: 1.0 + 1.0 / 512.0,
            ..a.clone()
        };
        assert_eq!(a.quantized_view_depth(), b.quantized_view_depth());
        let forward = [a.clone(), b.clone()];
        let mut reversed = [b.clone(), a.clone()];
        let mut sorted_forward = forward.to_vec();
        sorted_forward.sort_by(total_cmp);
        reversed.sort_by(total_cmp);
        assert_eq!(
            sorted_forward.iter().map(DrawItem::key).collect::<Vec<_>>(),
            reversed.iter().map(DrawItem::key).collect::<Vec<_>>(),
            "a tie must not depend on the order extraction happened to produce"
        );
        a.view_depth = 2.0;
        b.view_depth = 2.0;
        assert_eq!(total_cmp(&a, &b), a.entity.cmp(&b.entity));
    }

    #[test]
    fn a_duplicate_key_fails_extraction_rather_than_sorting_arbitrarily() {
        let item = DrawItem {
            entity: node_id("a"),
            draw_index: 0,
            layer_order: 0,
            material_pass: 0,
            view_depth: 0.0,
            current_transform: Mat4::IDENTITY,
            previous_transform: Mat4::IDENTITY,
            alpha_mode: AlphaMode::Opaque,
            previous_valid: true,
            asset: asset_id("tex"),
            opacity: 1.0,
            pass: PassKind::Opaque,
        };
        let err = validate_unique(&[item.clone(), item]).unwrap_err();
        assert!(matches!(err, RenderError::DuplicateDrawKey { .. }));
        assert!(validate_unique(&[]).is_ok());
    }

    /// #41's lookup table, one test per row that refuses.
    #[test]
    fn the_manifest_lookup_refuses_each_way_an_asset_can_be_undrawable() {
        let asset = asset_id("tex");
        let entity = node_id("a");
        let empty = manifest(vec![]);
        assert!(matches!(
            claims_mode(&empty, entity, asset).unwrap_err(),
            RenderError::MissingAsset { .. }
        ));

        let mut audio = entry(asset, ImageAlpha::Straight, ImageColorSpace::Srgb);
        audio.kind = AssetKind::Audio;
        audio.image = None;
        let not_image = manifest(vec![audio]);
        assert!(matches!(
            claims_mode(&not_image, entity, asset).unwrap_err(),
            RenderError::NotAnImageAsset { .. }
        ));

        for claims in [
            ImageAssetClaims {
                color_space: ImageColorSpace::Unclassified,
                alpha: ImageAlpha::Straight,
            },
            ImageAssetClaims {
                color_space: ImageColorSpace::Srgb,
                alpha: ImageAlpha::Unclassified,
            },
        ] {
            let record = AssetManifestEntry::new(
                asset,
                ContentHash::from_bytes([7u8; 32]),
                16,
                AssetKind::Image,
                1,
                [],
                "fixture",
                Some(claims),
            )
            .unwrap();
            let unclassified = manifest(vec![record]);
            assert!(matches!(
                claims_mode(&unclassified, entity, asset).unwrap_err(),
                RenderError::UnclassifiedAssetMetadata { .. }
            ));
        }

        let good = manifest(vec![entry(
            asset,
            ImageAlpha::Straight,
            ImageColorSpace::Srgb,
        )]);
        assert_eq!(
            claims_mode(&good, entity, asset).unwrap(),
            AlphaMode::Straight
        );
        let opaque = manifest(vec![entry(
            asset,
            ImageAlpha::Opaque,
            ImageColorSpace::Linear,
        )]);
        assert_eq!(
            claims_mode(&opaque, entity, asset).unwrap(),
            AlphaMode::Opaque
        );
    }

    #[test]
    fn a_group_node_and_an_invisible_node_produce_no_draw() {
        let asset = asset_id("tex");
        let assets = manifest(vec![entry(
            asset,
            ImageAlpha::Straight,
            ImageColorSpace::Srgb,
        )]);
        let mut hidden = node("hidden", Some(asset), 0.0);
        hidden.visible = false;
        let scene = snapshot(vec![
            node("group", None, 0.0),
            hidden,
            node("drawn", Some(asset), 0.0),
        ]);
        let frame = extract_frame(
            &scene,
            &empty_sim(),
            &PreviousFrameState::new(),
            quality(),
            &assets,
        )
        .unwrap();
        assert_eq!(frame.items.len(), 1);
        assert_eq!(frame.items[0].entity, node_id("drawn"));
        // The group node still consumed a layer slot, so project layer order is the visit
        // order and not a count of drawables.
        assert_eq!(frame.items[0].layer_order, 2);
    }

    #[test]
    fn a_nonfinite_transform_fails_extraction() {
        let asset = asset_id("tex");
        let assets = manifest(vec![entry(
            asset,
            ImageAlpha::Opaque,
            ImageColorSpace::Srgb,
        )]);
        let mut bad = node("bad", Some(asset), 0.0);
        bad.world_transform[0][0] = f64::NAN;
        let scene = snapshot(vec![bad]);
        let err = extract_frame(
            &scene,
            &empty_sim(),
            &PreviousFrameState::new(),
            quality(),
            &assets,
        )
        .unwrap_err();
        assert!(matches!(err, RenderError::NonFiniteTransform { .. }));
    }

    #[test]
    fn a_first_frame_has_no_valid_history_and_a_new_item_never_trusts_a_previous_transform() {
        let asset = asset_id("tex");
        let assets = manifest(vec![entry(
            asset,
            ImageAlpha::Straight,
            ImageColorSpace::Srgb,
        )]);
        let scene = snapshot(vec![node("a", Some(asset), 0.0)]);
        let frame = extract_frame(
            &scene,
            &empty_sim(),
            &PreviousFrameState::new(),
            quality(),
            &assets,
        )
        .unwrap();
        assert!(!frame.items[0].previous_valid);
        assert_eq!(
            frame.items[0].previous_transform, frame.items[0].current_transform,
            "a new item's previous transform is its current one, so its velocity is zero"
        );
        assert!(!frame.cut);
    }

    /// A spring body composes with the authored transform rather than replacing it: the
    /// solver owns the position, the author owns the rotation and scale.
    #[test]
    fn a_body_position_replaces_translation_and_keeps_the_authored_scale() {
        let asset = asset_id("tex");
        let assets = manifest(vec![entry(
            asset,
            ImageAlpha::Opaque,
            ImageColorSpace::Srgb,
        )]);
        let mut authored = node("a", Some(asset), 3.0);
        authored.world_transform = [[2.0, 0.0, 5.0], [0.0, 4.0, 9.0], [0.0, 0.0, 1.0]];
        let scene = snapshot(vec![authored]);
        let mut sim = empty_sim();
        sim.body_transforms.push(BodyTransform {
            node: node_id("a"),
            position: [10.0, 20.0, 30.0],
            previous_position: [1.0, 2.0, 3.0],
        });
        let mut previous = PreviousFrameState::new();
        previous.history.valid = true;
        previous.history.previous_tick = Some(SimTick(0));
        previous.history.target_generation = 1;
        previous.history.cut_serial = 0;
        previous.history.previous_revision = Some(Revision(1));
        previous.history.previous_extent = Some(quality().extent);
        previous.history.previous_projection = Some(crate::camera::projection(quality().extent));
        previous
            .transforms
            .insert((node_id("a"), 0), Mat4::IDENTITY);
        let frame = extract_frame(&scene, &sim, &previous, quality(), &assets).unwrap();
        let item = &frame.items[0];
        let translation = item.current_transform.w_axis.truncate();
        assert_eq!(translation.to_array(), [10.0, 20.0, 30.0]);
        // The authored scale survives: a unit vector is still stretched by 2 and 4.
        let scaled = item
            .current_transform
            .transform_vector3(Vec3::new(1.0, 1.0, 0.0));
        assert_eq!(scaled.to_array(), [2.0, 4.0, 0.0]);
    }

    #[test]
    fn a_particle_identity_is_stable_and_distinct_per_generation() {
        let mut particle = Particle {
            id: 0,
            emitter: node_id("emitter"),
            generation: 0,
            spawn_ordinal: 0,
            position: [0.0; 3],
            previous_position: [0.0; 3],
            velocity: [0.0; 3],
            age_ticks: 1,
            lifetime_ticks: 64,
            size: 1.0,
            rotation: 0.0,
        };
        particle.id = anim_sim::particles::particle_id(&particle.emitter, 0, 0);
        let first = particle_node(&particle);
        assert_eq!(
            first,
            particle_node(&particle),
            "the same particle is the same node"
        );
        particle.id = anim_sim::particles::particle_id(&particle.emitter, 1, 0);
        assert_ne!(
            first,
            particle_node(&particle),
            "a new generation is a new identity, so its history starts invalid"
        );
    }

    #[test]
    fn recording_a_frame_is_what_makes_the_next_extraction_have_history() {
        let asset = asset_id("tex");
        let assets = manifest(vec![entry(
            asset,
            ImageAlpha::Straight,
            ImageColorSpace::Srgb,
        )]);
        let scene = snapshot(vec![node("a", Some(asset), 0.0)]);
        let mut previous = PreviousFrameState::new();
        let first = extract_frame(&scene, &empty_sim(), &previous, quality(), &assets).unwrap();
        assert!(!first.items[0].previous_valid);
        previous.record(&first);
        assert_eq!(
            previous.transform((node_id("a"), 0)),
            Some(first.items[0].current_transform)
        );
        assert_eq!(previous.transforms.len(), 1);
    }
}
