//! Track and layer blending, and basic pose and cel selection (spec 13.3, 13.5 steps 3
//! and 5).
//!
//! Two halves of 13.5 live here, and they are in opposite situations, which is why the
//! module comment says so before the code does.
//!
//! **Blending is deferred, by decision and not by omission.** 13.5 step 3 asks for "blend
//! animation layers in authored priority order", and there is no authored animation-layer
//! type anywhere in the spec. WP15's `MotionLayer` (spec 30.8) is camera motion — its
//! `MotionKind` is `Pan`, `Tilt`, `Truck`, `Dolly`, `Rail`, `WhipPan` — and WP12's pose
//! layers (spec 27.5, "blend pose layers using per-bone masks") have no data structure at
//! all. A scene-animation-layer type invented here would be replaced by whichever of those
//! packages authors a real one, and the replacement would move `PROJECT_SCHEMA_VERSION` a
//! second time and change the shape of a package whose gate has already been certified. So
//! the seam is [`blend_layers`] below: named, documented, called by the evaluator, and
//! carrying the one layer that exists today. 13.12's G3 criteria list no layer-blending
//! clause, which is what makes deferring it a recorded gap rather than a missed one.
//!
//! **Property paths are authored here because nothing else names them.** 13.8's
//! `EvaluatedNode` has `z`, `opacity`, `visible` and `asset` fields and a `world_transform`
//! matrix, and the only place those can come from is the document, whose properties are
//! [`anim_project::PropertyPath`] values — dotted strings, open by design,
//! for the reason `property.rs` gives: the property set is not closed at this stage, and
//! WP12's bone controls would otherwise have to edit that crate to add a variant. No
//! section of the spec lists which paths mean which node field. [`PATH_TRANSFORM_X`] and
//! its siblings are that list. The names are not invented from nothing: `transform.x` and
//! `opacity` are already the paths WP03's own schema fixtures author, so this convention
//! continues the spelling the repository already uses rather than introducing one.

use std::collections::BTreeMap;

use anim_core::id::NodeId;
use anim_core::time::FramePos;
use anim_project::{NodeData, PropertyPath, PropertyValue, SceneData};

use crate::hierarchy::multiply;
use crate::properties::{sample_property, value_as_f64, with_subject};
use crate::snapshot::EvaluatedNode;
use crate::{Result, SceneError};

/// The transform's horizontal offset, in world units.
pub const PATH_TRANSFORM_X: &str = "transform.x";
/// The transform's vertical offset, in world units.
pub const PATH_TRANSFORM_Y: &str = "transform.y";
/// The transform's rotation, in **degrees**.
///
/// Degrees rather than radians or turns because it is the unit an animator authors in and
/// the unit every timing sheet in WP21's X-sheet displays. The conversion to radians
/// happens once, in [`local_transform`], and is a float operation like the trigonometry
/// that follows it — which is the same "last, not in the middle" rule 13.6 states for
/// interpolation.
pub const PATH_TRANSFORM_ROTATION: &str = "transform.rotation";
/// The transform's uniform scale.
pub const PATH_TRANSFORM_SCALE: &str = "transform.scale";
/// The depth axis, which 13.8 keeps outside the 3x3 matrix rather than folding it in.
pub const PATH_Z: &str = "z";
/// A rational opacity. Read as the engine sees it, not through a display gamma — colour
/// management is spec 15's and belongs to the renderer that composites it.
pub const PATH_OPACITY: &str = "opacity";
/// The visibility switch. The one field whose authored absence is not an error: a node
/// with no `visible` property anywhere is visible, because invisibility is a thing an
/// operator does and an imported sprite that vanished on load would be the surprising half.
pub const PATH_VISIBLE: &str = "visible";
/// The cel or sprite this node shows.
///
/// 13.8 types it `Option<AssetId>` rather than required, which is what makes a node with
/// no art a legal node. 13.11's "missing assets remain typed resolution failures" is
/// `anim_assets`' resolution of the id, not this crate's: an asset property is checked
/// here for holding an id, never for existing on disk. That split is why
/// [`crate::snapshot::AudioCue`] can carry an unresolved asset without this package
/// having to open a store it is not allowed to depend on.
pub const PATH_ASSET: &str = "asset";

/// What an opacity of "nothing authored" means: opaque. A default of `0` would render an
/// untouched scene invisible, and every golden frame from WP06 onward would be black.
const DEFAULT_OPACITY: f64 = 1.0;
/// What a depth of "nothing authored" means: the origin of the depth axis, which is where
/// an imported sprite sits when nobody has put it anywhere.
const DEFAULT_Z: f64 = 0.0;
/// What a scale of "nothing authored" means: the sprite at its own size. `0` would
/// collapse the matrix, and `1` is the only value that makes "unauthored" mean "unchanged".
const DEFAULT_SCALE: f64 = 1.0;

/// Where one property value for one node comes from, in the order 13.6 sets.
///
/// A view over a scene rather than a method on one, for the reason
/// [`crate::hierarchy::NodeView`] is a view: the resolution rule is testable in isolation,
/// and a test that has to build a whole validated document to ask what a missing property
/// evaluates to is testing the document builder instead of the rule.
#[derive(Clone, Copy, Debug)]
pub struct PropertySources<'a> {
    scene: &'a SceneData,
}

impl<'a> PropertySources<'a> {
    /// Bind to one scene.
    #[must_use]
    pub const fn of(scene: &'a SceneData) -> Self {
        Self { scene }
    }

    /// The value of one `(node, path)` pair at one exact position, or `None` when the
    /// property has no keys *and* no authored default at any level.
    ///
    /// 13.6's order, per property: a track's key list first, because authored animation
    /// outranks a default; then the node's own `defaults` entry, which is what "authored
    /// default" denotes in a raw scene; then nothing. The track is found through
    /// `SceneData::track_for`, whose one-track-per-property rule is 13.2's, so there is no
    /// ambiguity to resolve and no order between two tracks to get wrong.
    ///
    /// # Errors
    ///
    /// Whatever [`sample_property`] raises for the track's key list, and
    /// [`SceneError::InvalidPropertyKeys`] if `path` is not a canonical property path —
    /// which is this package's bug and not a document's, and is reported instead of being
    /// read as an absent property, because a typo in one of the constants above would
    /// otherwise make a field silently default.
    pub fn value(
        &self,
        node: &NodeData,
        path: &str,
        at: &FramePos,
    ) -> Result<Option<PropertyValue>> {
        let parsed = PropertyPath::new(path).map_err(|e| SceneError::InvalidPropertyKeys {
            node: node.node_id.to_string(),
            property: path.to_string(),
            detail: format!("the evaluator asked for a path that is not canonical: {e}"),
        })?;
        if let Some(track) = self.scene.track_for(&node.node_id, &parsed) {
            // The key list cannot say which property it belongs to; this can, and 13.11's
            // refusals are worth nothing in a report that names neither.
            let sampled = sample_property(&track.curve, at)
                .map_err(|e| with_subject(e, &node.node_id, path))?;
            if sampled.is_some() {
                return Ok(sampled);
            }
        }
        Ok(node.defaults.get(&parsed).cloned())
    }
}

/// The four authored numbers that make one node's local matrix.
///
/// A type rather than a tuple because 13.7's `world = parent_world * local` needs all four
/// together, and a caller that assembled them in the wrong order — scale into translation,
/// say — would produce a finite, plausible, wrong matrix.
///
/// It is also the half of node evaluation that must happen *before* the hierarchy pass,
/// which is why it is its own function rather than a line inside [`evaluate_node`]:
/// [`crate::hierarchy::visit`] consumes local matrices to build world ones, so a single
/// pass that produced only world matrices could not hand anything to it.
///
/// # Errors
///
/// As [`evaluate_node`], for the transform paths.
pub fn local_transform(
    node: &NodeData,
    sources: &PropertySources<'_>,
    at: &FramePos,
) -> Result<LocalTransform> {
    Ok(LocalTransform {
        x: number(sources, node, PATH_TRANSFORM_X, at, 0.0, "transform.x")?,
        y: number(sources, node, PATH_TRANSFORM_Y, at, 0.0, "transform.y")?,
        rotation: number(
            sources,
            node,
            PATH_TRANSFORM_ROTATION,
            at,
            0.0,
            "transform.rotation",
        )? * std::f64::consts::PI
            / 180.0,
        scale: number(
            sources,
            node,
            PATH_TRANSFORM_SCALE,
            at,
            DEFAULT_SCALE,
            "transform.scale",
        )?,
    })
}

/// Every node's local matrix, keyed by identity, which is what
/// [`crate::hierarchy::visit`] takes.
///
/// # Errors
///
/// As [`local_transform`], for whichever node fails first.
pub fn local_matrices(
    nodes: &[NodeData],
    sources: &PropertySources<'_>,
    at: &FramePos,
) -> Result<BTreeMap<NodeId, [[f64; 3]; 3]>> {
    nodes
        .iter()
        .map(|node| Ok((node.node_id, local_transform(node, sources, at)?.matrix())))
        .collect()
}

/// The four authored numbers, before any parent is involved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalTransform {
    pub x: f64,
    pub y: f64,
    /// Radians; the authored value was degrees.
    pub rotation: f64,
    pub scale: f64,
}

impl LocalTransform {
    /// `translate * rotate * scale`, row-major.
    ///
    /// That order, and not scale-then-translate, is what makes `transform.x` mean a
    /// position in the parent's units rather than a position multiplied by this node's own
    /// size — the difference `a_child_is_not_scaled_by_its_own_translation` pins down,
    /// because the two orders agree whenever the scale is 1 and a test written at scale 1
    /// would prove nothing.
    #[must_use]
    pub fn matrix(&self) -> [[f64; 3]; 3] {
        // `f64::sin_cos` returns its pair in `(sin, cos)` order. Destructuring it the way
        // the matrix reads — `(cos, sin)` — swaps the two, and the failure is not a wrong
        // sign but a quarter turn on every node that authored no rotation at all: the
        // identity becomes `[[0, -1], [1, 0]]`, finite, plausible and wrong, which is what
        // `an_untouched_node_is_visible_opaque_and_at_the_origin` caught.
        let (sin, cos) = self.rotation.sin_cos();
        let s = self.scale;
        [
            [cos * s, -sin * s, self.x],
            [sin * s, cos * s, self.y],
            [0.0, 0.0, 1.0],
        ]
    }
}

/// Evaluate one node's authored state into 13.8's `EvaluatedNode`.
///
/// `world` is the matrix [`crate::hierarchy::visit`] composed for this node, and this
/// function composes the node's own local matrix onto it. It does not look at the parent
/// chain: that is the separation 13.7 draws between the hierarchy pass and the property
/// pass, and it is what lets [`crate::cache::SnapshotCache`] key on a position without
/// two evaluation paths disagreeing about which pass owns which number.
///
/// # Errors
///
/// - [`SceneError::InvalidNumericValue`] when a field that must be a number holds a bool,
///   a label or an asset, or when an evaluated value is not finite.
/// - [`SceneError::InvalidEasing`] when a non-`Step` easing is declared on a value with no
///   interval arithmetic — the reachability argument in [`crate::SceneError`]'s comment.
/// - [`SceneError::TimeOverflow`] and [`SceneError::Core`] from the rational arithmetic.
pub fn evaluate_node(
    node: &NodeData,
    world: [[f64; 3]; 3],
    sources: &PropertySources<'_>,
    at: &FramePos,
) -> Result<EvaluatedNode> {
    let subject = node.node_id.to_string();
    let local = local_transform(node, sources, at)?;
    let world_transform = multiply(&world, &local.matrix());

    let z = match sources.value(node, PATH_Z, at)? {
        Some(value) => value_as_f64(&value, "z", &subject)?,
        None => DEFAULT_Z,
    };
    let opacity = match sources.value(node, PATH_OPACITY, at)? {
        Some(value) => value_as_f64(&value, "opacity", &subject)?,
        None => DEFAULT_OPACITY,
    };
    // 13.7's nonfinite rule, applied to the two fields it states for matrices and which
    // `CanonicalBytes::f64` would otherwise refuse at hash time — a field that fails here
    // names the node, and a field that fails in the digest names nothing.
    for (field, value) in [("z", z), ("opacity", opacity)] {
        if !value.is_finite() {
            return Err(SceneError::InvalidNumericValue {
                field,
                subject: subject.clone(),
                detail: format!("the evaluated value is {value}, which is not finite"),
            });
        }
    }
    let visible = match sources.value(node, PATH_VISIBLE, at)? {
        Some(PropertyValue::Bool(flag)) => flag,
        Some(other) => {
            return Err(SceneError::InvalidNumericValue {
                field: "visible",
                subject,
                detail: format!(
                    "a visibility property holds a {} and not a switch",
                    other.kind_name()
                ),
            });
        }
        None => true,
    };
    let asset = match sources.value(node, PATH_ASSET, at)? {
        Some(PropertyValue::Asset(id)) => Some(id),
        Some(other) => {
            return Err(SceneError::InvalidNumericValue {
                field: "asset",
                subject,
                detail: format!(
                    "an asset property holds a {} and not an asset reference",
                    other.kind_name()
                ),
            });
        }
        None => None,
    };

    Ok(EvaluatedNode {
        id: node.node_id,
        parent: node.parent,
        world_transform,
        z,
        opacity,
        visible,
        asset,
    })
}

/// The blend 13.5 step 3 names, over the layers this build has.
///
/// One layer: the base. The function exists so the deferral is a named call site in the
/// evaluator rather than an absent feature, and so the package that authors real layers —
/// WP12 for per-bone pose masks, WP15 for camera motion, whichever lands first — replaces
/// this body instead of discovering where it should have gone. The rule it will implement
/// is 13.5's "authored priority order", which spec 30.5 states as `(priority,
/// MotionLayerId)` for WP15's own layers and which will need the layer type that
/// `BUILD_STATE.md` records as deferred.
#[must_use]
pub fn blend_layers(base: EvaluatedNode) -> EvaluatedNode {
    base
}

fn number(
    sources: &PropertySources<'_>,
    node: &NodeData,
    path: &str,
    at: &FramePos,
    engine_default: f64,
    field: &'static str,
) -> Result<f64> {
    let subject = node.node_id.to_string();
    match sources.value(node, path, at)? {
        Some(value) => value_as_f64(&value, field, &subject),
        None => Ok(engine_default),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the workspace denial governs production code"
    )]

    use super::*;
    use crate::hierarchy::IDENTITY;
    use anim_core::id::{AssetId, TrackId};
    use anim_core::rational::Rational;
    use anim_project::{Easing, EasingCurve, PropertyCurve, PropertyKey, TrackData};
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn node_id(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    fn track_id(label: &str) -> TrackId {
        TrackId::derive(&namespace(), label.as_bytes())
    }

    fn r(num: i128, den: i128) -> Rational {
        Rational::new(num, den).unwrap()
    }

    fn node(label: &str) -> NodeData {
        NodeData::new(node_id(label), None, 0, label)
    }

    fn track(
        node_label: &str,
        path: &str,
        keys: impl IntoIterator<Item = PropertyKey>,
    ) -> TrackData {
        TrackData::new(
            track_id(path),
            node_id(node_label),
            PropertyPath::new(path).unwrap(),
            PropertyCurve::new(keys, None).unwrap(),
        )
    }

    fn key(frame: i64, num: i128, den: i128, easing: Easing) -> PropertyKey {
        PropertyKey::new(
            FramePos::at_frame(frame),
            PropertyValue::Rational(r(num, den)),
            easing,
        )
    }

    fn evaluated(scene: &SceneData, label: &str, at: i64) -> EvaluatedNode {
        let node = scene.node(&node_id(label)).unwrap().clone();
        evaluate_node(
            &node,
            IDENTITY,
            &PropertySources::of(scene),
            &FramePos::at_frame(at),
        )
        .unwrap()
    }

    fn with_default(mut node: NodeData, path: &str, value: PropertyValue) -> NodeData {
        node.defaults
            .insert(PropertyPath::new(path).unwrap(), value);
        node
    }

    #[test]
    fn an_untouched_node_is_visible_opaque_and_at_the_origin() {
        // The three engine defaults, each asserted at the value that makes "authored
        // nothing" mean "draw as imported". A `0` for opacity or a `false` for visibility
        // would pass every other test in this crate and produce a black screen in WP06.
        let scene = SceneData::from_parts("shot01", [node("plain")], [], [], []).unwrap();
        let out = evaluated(&scene, "plain", 5);
        assert!(out.visible, "invisibility is authored, never defaulted");
        assert_eq!(out.opacity, 1.0);
        assert_eq!(out.z, 0.0);
        assert_eq!(out.asset, None);
        assert_eq!(out.world_transform, IDENTITY);
    }

    #[test]
    fn a_node_default_is_used_when_no_track_covers_the_property() {
        // 13.6's "Empty properties use authored defaults", through the node's `defaults`
        // map, which is where an authored default actually lives in a raw scene.
        let plain = with_default(
            node("plain"),
            PATH_OPACITY,
            PropertyValue::Rational(r(1, 4)),
        );
        let scene = SceneData::from_parts("shot01", [plain], [], [], []).unwrap();
        assert_eq!(evaluated(&scene, "plain", 1).opacity, 0.25);
    }

    #[test]
    fn an_animated_track_outranks_the_default_it_overwrites() {
        let hero = with_default(node("hero"), PATH_OPACITY, PropertyValue::Rational(r(1, 1)));
        let anim = track(
            "hero",
            PATH_OPACITY,
            [key(0, 0, 1, Easing::Linear), key(10, 1, 1, Easing::Linear)],
        );
        let scene = SceneData::from_parts("shot01", [hero], [anim], [], []).unwrap();
        assert_eq!(
            evaluated(&scene, "hero", 5).opacity,
            0.5,
            "halfway up the track, and not the authored default"
        );
    }

    #[test]
    fn an_empty_track_falls_through_to_the_node_default() {
        // The order is per property and not per object: a track that exists and holds
        // nothing is 13.6's "empty property", and the node's default still answers.
        let hero = with_default(node("hero"), PATH_OPACITY, PropertyValue::Rational(r(1, 2)));
        let empty = TrackData::new(
            track_id("opacity"),
            node_id("hero"),
            PropertyPath::new(PATH_OPACITY).unwrap(),
            PropertyCurve::with_default(None),
        );
        let scene = SceneData::from_parts("shot01", [hero], [empty], [], []).unwrap();
        assert_eq!(evaluated(&scene, "hero", 3).opacity, 0.5);
    }

    #[test]
    fn a_child_is_not_scaled_by_its_own_translation() {
        // `translate * rotate * scale` against `scale * translate`: at scale 2 and x 1 the
        // two orders differ, and the difference is the authored meaning of `transform.x`.
        let local = LocalTransform {
            x: 1.0,
            y: 0.0,
            rotation: 0.0,
            scale: 2.0,
        };
        assert_eq!(local.matrix()[0][2], 1.0, "the offset is not doubled");
        assert_eq!(local.matrix()[0][0], 2.0, "the axis is");
        let scaled_first = multiply(
            &[[2.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 1.0]],
            &[[1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        );
        assert_eq!(
            scaled_first[0][2], 2.0,
            "the wrong order, for comparison, and it is visibly different"
        );
    }

    #[test]
    fn a_quarter_turn_is_axis_aligned_because_the_unit_is_degrees() {
        let hero = with_default(
            node("hero"),
            PATH_TRANSFORM_ROTATION,
            PropertyValue::Rational(r(90, 1)),
        );
        let scene = SceneData::from_parts("shot01", [hero], [], [], []).unwrap();
        let out = evaluated(&scene, "hero", 0);
        assert!(
            out.world_transform[0][0].abs() < 1e-12,
            "cos 90 is zero, got {}",
            out.world_transform[0][0]
        );
        assert!((out.world_transform[1][0] - 1.0).abs() < 1e-12);
        assert!(out.world_transform[0][1] < 0.0, "and CCW-positive");
    }

    #[test]
    fn a_parent_offset_reaches_a_child_through_the_two_passes() {
        // The evaluator's ordering, tested here rather than in `evaluator.rs`: locals are
        // composed before worlds exist, so a child's local must be independent of its
        // parent's authored numbers.
        let mut parent = node("parent");
        let parent_id = parent.node_id;
        let mut child = NodeData::new(node_id("child"), Some(parent_id), 0, "child");
        parent.defaults.insert(
            PropertyPath::new(PATH_TRANSFORM_X).unwrap(),
            PropertyValue::Rational(r(10, 1)),
        );
        child.defaults.insert(
            PropertyPath::new(PATH_TRANSFORM_X).unwrap(),
            PropertyValue::Rational(r(1, 1)),
        );
        let scene =
            SceneData::from_parts("shot01", [parent.clone(), child.clone()], [], [], []).unwrap();
        let sources = PropertySources::of(&scene);
        let at = FramePos::at_frame(0);
        let locals = local_matrices(&[parent.clone(), child.clone()], &sources, &at).unwrap();
        assert_eq!(locals[&parent_id][0][2], 10.0, "local, not world");
        assert_eq!(locals[&node_id("child")][0][2], 1.0);
        let entries = crate::hierarchy::visit(
            &[
                crate::hierarchy::NodeView {
                    id: parent_id,
                    parent: None,
                    sibling_order: 0,
                },
                crate::hierarchy::NodeView {
                    id: node_id("child"),
                    parent: Some(parent_id),
                    sibling_order: 0,
                },
            ],
            &locals,
        )
        .unwrap();
        // `evaluate_node` takes the *parent's* world and composes the node's own local
        // matrix onto it, which is the same product `visit` already formed. The two passes
        // agreeing is the invariant worth asserting here — if they disagreed, the cached
        // snapshot would carry a matrix that the hierarchy had not produced.
        let parent_world = entries.iter().find(|e| e.id == parent_id).unwrap().world;
        let child_world = entries
            .iter()
            .find(|e| e.id == node_id("child"))
            .unwrap()
            .world;
        let child_evaluated = evaluate_node(&child, parent_world, &sources, &at).unwrap();
        assert!(
            (child_evaluated.world_transform[0][2] - 11.0).abs() < 1e-12,
            "10 then 1, in the parent's units, got {}",
            child_evaluated.world_transform[0][2]
        );
        assert_eq!(
            child_evaluated.world_transform, child_world,
            "and the property pass composes what the hierarchy pass already did"
        );
    }

    #[test]
    fn a_cel_is_selected_and_held_and_never_interpolated() {
        // 13.5 step 5's "basic pose/cel selection", through the `asset` path, and 13.6's
        // step rule: the authored cel in force is the last one at or before the position.
        let mut scene = SceneData::new("shot01").unwrap();
        scene.insert_node(node("hero")).unwrap();
        let first = AssetId::derive(&namespace(), b"cel-a");
        let second = AssetId::derive(&namespace(), b"cel-b");
        scene
            .insert_track(track(
                "hero",
                PATH_ASSET,
                [
                    PropertyKey::new(
                        FramePos::at_frame(0),
                        PropertyValue::Asset(first),
                        Easing::Step,
                    ),
                    PropertyKey::new(
                        FramePos::at_frame(4),
                        PropertyValue::Asset(second),
                        Easing::Step,
                    ),
                ],
            ))
            .unwrap();
        assert_eq!(evaluated(&scene, "hero", 0).asset, Some(first));
        assert_eq!(evaluated(&scene, "hero", 3).asset, Some(first), "held");
        assert_eq!(evaluated(&scene, "hero", 4).asset, Some(second));
        assert_eq!(evaluated(&scene, "hero", 9).asset, Some(second));
    }

    #[test]
    fn trying_to_interpolate_two_cels_is_an_easing_failure_at_the_second_key() {
        let mut scene = SceneData::new("shot01").unwrap();
        scene.insert_node(node("hero")).unwrap();
        scene
            .insert_track(track(
                "hero",
                PATH_ASSET,
                [
                    PropertyKey::new(
                        FramePos::at_frame(0),
                        PropertyValue::Asset(AssetId::derive(&namespace(), b"a")),
                        Easing::Linear,
                    ),
                    PropertyKey::new(
                        FramePos::at_frame(4),
                        PropertyValue::Asset(AssetId::derive(&namespace(), b"b")),
                        Easing::Linear,
                    ),
                ],
            ))
            .unwrap();
        let node_data = scene.node(&node_id("hero")).unwrap().clone();
        let Err(SceneError::InvalidEasing {
            position, detail, ..
        }) = evaluate_node(
            &node_data,
            IDENTITY,
            &PropertySources::of(&scene),
            &FramePos::at_frame(2),
        )
        else {
            panic!("there is no halfway between two cels");
        };
        assert_eq!(
            position, "4",
            "the key whose easing cannot be applied is named"
        );
        assert!(detail.contains("switches at a key"), "{detail}");
    }

    #[test]
    fn a_label_where_a_number_belongs_names_the_field_rather_than_the_easing() {
        let mut scene = SceneData::new("shot01").unwrap();
        scene.insert_node(node("hero")).unwrap();
        scene
            .insert_track(track(
                "hero",
                PATH_OPACITY,
                [PropertyKey::new(
                    FramePos::at_frame(0),
                    PropertyValue::Text("solid".into()),
                    Easing::Step,
                )],
            ))
            .unwrap();
        let node_data = scene.node(&node_id("hero")).unwrap().clone();
        let Err(SceneError::InvalidNumericValue { field, detail, .. }) = evaluate_node(
            &node_data,
            IDENTITY,
            &PropertySources::of(&scene),
            &FramePos::at_frame(0),
        ) else {
            panic!("a string is not an opacity");
        };
        assert_eq!(field, "opacity");
        assert!(detail.contains("text"), "{detail}");
    }

    #[test]
    fn a_curve_on_a_transform_is_eased_before_it_becomes_a_matrix() {
        // 13.6's rule in the node path: the fraction is rational, the table is applied to
        // it, and only then does a float appear. Halfway along a rise from 0 to 8 eased by
        // a table whose midpoint is 1/4 is 2 — not 4, which is what an implementation that
        // eased in floats after the matrix would have produced.
        let mut scene = SceneData::new("shot01").unwrap();
        scene.insert_node(node("hero")).unwrap();
        scene
            .insert_track(track(
                "hero",
                PATH_TRANSFORM_X,
                [
                    key(0, 0, 1, Easing::Linear),
                    PropertyKey::new(
                        FramePos::at_frame(4),
                        PropertyValue::Rational(r(8, 1)),
                        Easing::Curve(EasingCurve::new([r(0, 1), r(1, 4), r(1, 1)]).unwrap()),
                    ),
                ],
            ))
            .unwrap();
        assert_eq!(evaluated(&scene, "hero", 2).world_transform[0][2], 2.0);
        assert_eq!(
            evaluated(&scene, "hero", 4).world_transform[0][2],
            8.0,
            "and it still arrives at the authored value"
        );
    }

    #[test]
    fn a_switch_authored_off_is_off_for_the_whole_interval() {
        let mut scene = SceneData::new("shot01").unwrap();
        scene.insert_node(node("hero")).unwrap();
        scene
            .insert_track(track(
                "hero",
                PATH_VISIBLE,
                [
                    PropertyKey::new(
                        FramePos::at_frame(0),
                        PropertyValue::Bool(true),
                        Easing::Step,
                    ),
                    PropertyKey::new(
                        FramePos::at_frame(6),
                        PropertyValue::Bool(false),
                        Easing::Step,
                    ),
                ],
            ))
            .unwrap();
        assert!(evaluated(&scene, "hero", 5).visible);
        assert!(!evaluated(&scene, "hero", 6).visible);
        assert!(!evaluated(&scene, "hero", 40).visible);
    }

    #[test]
    fn a_key_list_refusal_names_the_node_and_the_property_it_broke() {
        // `sample_property` is about values and cannot know whose they are, so the error it
        // raises arrives with blank fields and `value` fills them on the way out. Asserted
        // here because the blank version would still be a correct diagnosis of the wrong
        // thing: 13.11's categories are only useful if a report says which property to fix.
        let mut scene = SceneData::new("shot01").unwrap();
        scene.insert_node(node("hero")).unwrap();
        scene
            .insert_track(track(
                "hero",
                PATH_OPACITY,
                [
                    PropertyKey::new(
                        FramePos::at_frame(0),
                        PropertyValue::Rational(Rational::from_i128(0)),
                        Easing::Linear,
                    ),
                    PropertyKey::new(
                        FramePos::at_frame(4),
                        PropertyValue::Text("opaque".into()),
                        Easing::Linear,
                    ),
                ],
            ))
            .unwrap();
        let node_data = scene.node(&node_id("hero")).unwrap().clone();
        let Err(SceneError::InvalidPropertyKeys {
            node,
            property,
            detail,
        }) = evaluate_node(
            &node_data,
            IDENTITY,
            &PropertySources::of(&scene),
            &FramePos::at_frame(2),
        )
        else {
            panic!("a rational does not become a label halfway along");
        };
        assert_eq!(
            node,
            node_id("hero").to_string(),
            "the report names the node"
        );
        assert_eq!(property, PATH_OPACITY, "and the property");
        assert!(
            detail.contains("rational") && detail.contains("text"),
            "{detail}"
        );
    }

    #[test]
    fn blending_one_layer_is_that_layer() {
        // The deferral, asserted rather than described. A later package that adds blending
        // fails this only by changing what it means, which is the point of a named seam.
        let scene = SceneData::from_parts("shot01", [node("plain")], [], [], []).unwrap();
        let base = evaluated(&scene, "plain", 3);
        assert_eq!(blend_layers(base.clone()), base);
    }

    #[test]
    fn a_noncanonical_path_asked_for_by_name_is_refused_rather_than_read_as_absent() {
        // The evaluator's own bug class, made unreachable-by-accident: a typo in one of the
        // constants above would otherwise default a field silently.
        let scene = SceneData::from_parts("shot01", [node("plain")], [], [], []).unwrap();
        let node_data = scene.node(&node_id("plain")).unwrap().clone();
        assert!(matches!(
            PropertySources::of(&scene).value(
                &node_data,
                "transform bad path",
                &FramePos::at_frame(0)
            ),
            Err(SceneError::InvalidPropertyKeys { .. })
        ));
    }
}
