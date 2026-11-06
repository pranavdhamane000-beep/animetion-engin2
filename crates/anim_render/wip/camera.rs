//! Current/previous view-projection and cut state (spec 15.3).
//!
//! This file owns the renderer's coordinate convention, which spec 15 never states and
//! which everything downstream depends on: the velocity calculation (15.7), the depth
//! quantization that makes the transparent sort reproducible (15.7's "signed i32 in
//! documented camera units"), and the resize that invalidates history (15.7). Deciding it
//! here is the honest place; the alternative is a WGSL file deciding it, which is where a
//! convention becomes invisible.
//!
//! # The convention
//!
//!  * **Authored units are canvas pixels.** `transform.x`/`transform.y` (WP04's property
//!    paths, risk 21) are pixels from the viewport centre, **+x right, +y down** — the
//!    screen-space reading a 2D animation author expects, and the one that puts a sprite
//!    authored at `y = 0` on the top edge.
//!  * **Authored `z` is depth in the same pixels**, larger toward the viewer, and the
//!    projection maps `[-Z_NEAR, Z_FAR]` onto Vulkan's `[0, 1]` window.
//!  * **The projection is orthographic** over the extent, so a resize changes what is
//!    visible and never rescales geometry. There is no perspective divide anywhere in this
//!    package, which is what makes 15.7's `previous w <= 0` test mean exactly one thing.
//!  * **NDC to UV is `uv = ndc * vec2(0.5, -0.5) + 0.5`** — 15.7's own formula, and the
//!    reason the projection negates `y`: NDC's +y is the texture's top row, so a world
//!    point below the centre must land on a higher row.
//!  * **A camera has no authored depth.** 13.8's `EvaluatedCamera` carries a node, its
//!    world matrix and whether it is on; there is no z, no field of view and no near plane
//!    anywhere in WP04's shape. The camera's z is therefore 0 and the window above is an
//!    engine constant, not a rig. WP15's camera rig (30.8) is where these become data, and
//!    when it does, `projection` stops being a function of extent alone.
//!
//! # What floats may appear here
//!
//! `glam` is built with `scalar-math` in this workspace, and every operation below is
//! `+ - * /` — matrix composition, the orthographic scale, the cofactor inverse. No
//! trigonometry reaches a render-side matrix, which is what BUILD_STATE risk 28 requires:
//! WP04's one `libm` dependency is in the *evaluator*, and the renderer must not add a
//! second one whose bits a different C runtime could change.

use anim_core::id::Revision;
use anim_scene::SceneSnapshot;
use anim_sim::clock::SimTick;
use glam::{Mat4, Vec4};
use wgpu::Extent3d;

use crate::RenderError;

/// The authored depth window mapped onto Vulkan's `[0, 1]`. See the module comment's
/// fourth bullet for why this is a constant and not authored data.
pub const Z_NEAR: f32 = -32_000.0;

/// The far plane, in the same units. See [`Z_NEAR`].
pub const Z_FAR: f32 = 32_000.0;

/// Quantization scale for view depth in the transparent sort: 256 buckets per pixel.
///
/// Spec 15.7 requires the sort key to be "signed i32 in documented camera units", and this
/// is that document. Two authored depths a whole pixel apart cannot land in the same
/// bucket, so a tie inside one bucket is a *depth tie* — the thing 15.12 asks to be tested
/// — rather than an accident of the scale factor.
pub const DEPTH_BUCKETS_PER_PIXEL: f32 = 256.0;

/// One frame's camera, as the renderer needs it to draw *and* to explain the drawing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraFrame {
    pub view: Mat4,
    pub projection: Mat4,
    pub view_proj: Mat4,
    /// Monotonic count of camera cuts at or before this snapshot's position. A change in
    /// this value is what "a cut happened" means operationally; it is derived from the
    /// snapshot's own event list rather than from a caller's counter, because 13.7 puts the
    /// events in the snapshot in canonical order and a second source would be free to
    /// disagree after a seek.
    pub cut_serial: u64,
}

impl CameraFrame {
    /// Evaluate the on-camera for this snapshot against this extent.
    ///
    /// # Errors
    ///
    /// [`RenderError::NonFiniteTransform`] when the on-camera's world matrix is not finite
    /// or is singular. With no camera authored there is nothing to be wrong with, and the
    /// identity is used — see [`identity_view`].
    #[must_use]
    pub fn from_snapshot(snapshot: &SceneSnapshot, extent: Extent3d) -> Result<Self, RenderError> {
        let projection = projection(extent);
        let view = match snapshot.cameras.iter().find(|camera| camera.is_on) {
            None => identity_view(),
            Some(camera) => {
                let world = Mat4::from_cols_array_2d(&row_major_affine(&camera.world_transform, 0.0));
                view_from_world(&world).map_err(|detail| RenderError::NonFiniteTransform {
                    entity: camera.node,
                    draw_index: 0,
                    detail,
                })?
            }
        };
        Ok(Self {
            view,
            projection,
            view_proj: projection * view,
            cut_serial: cut_serial_at(snapshot),
        })
    }

    /// Pair this frame with the one before it, the way 15.7's velocity stage needs.
    ///
    /// Across a cut the previous matrix *becomes* the current one — 30's epoch rule, which
    /// zeroes the camera's own motion vector while leaving every object vector valid. That
    /// is not an approximation: a cut has no camera motion to represent, and carrying the
    /// old camera's matrix forward instead draws a full-frame smear toward a camera the
    /// viewer never saw.
    #[must_use]
    pub fn matrices(self, previous: Option<Self>) -> CameraMatrices {
        let Some(previous) = previous else {
            return CameraMatrices {
                current_view_proj: self.view_proj,
                previous_view_proj: self.view_proj,
            };
        };
        CameraMatrices {
            current_view_proj: self.view_proj,
            previous_view_proj: if self.cut_serial == previous.cut_serial {
                previous.view_proj
            } else {
                self.view_proj
            },
        }
    }
}

/// The two view-projection matrices the velocity pass reads (15.8's pair, in one type).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraMatrices {
    pub current_view_proj: Mat4,
    pub previous_view_proj: Mat4,
}

/// The orthographic projection for an extent, in the documented units.
#[must_use]
pub fn projection(extent: Extent3d) -> Mat4 {
    // Half-extents in f64 then rounded once: the same value a golden's expectation is
    // computed from, so a `f32` multiply of a `u32` width cannot disagree with the test.
    let half_width = f64::from(extent.width) / 2.0;
    let half_height = f64::from(extent.height) / 2.0;
    let sx = (1.0 / half_width) as f32;
    let sy = (-1.0 / half_height) as f32;
    // `z' = (z - Z_NEAR) / (Z_FAR - Z_NEAR)`, which maps the near plane to 0 and the far
    // plane to 1 — Vulkan's window, since this workspace builds Vulkan only (spec 9.11) and
    // a `[-1, 1]` projection would put every authored depth outside the depth test.
    let span = f64::from(Z_FAR) - f64::from(Z_NEAR);
    let sz = (1.0 / span) as f32;
    let bz = (-f64::from(Z_NEAR) / span) as f32;
    Mat4::from_cols(
        Vec4::new(sx, 0.0, 0.0, 0.0),
        Vec4::new(0.0, sy, 0.0, 0.0),
        Vec4::new(0.0, 0.0, sz, 0.0),
        Vec4::new(0.0, 0.0, bz, 1.0),
    )
}

/// 13.7's row-major 3×3 world matrix, widened to the 4×4 the GPU reads.
///
/// The layout is `[[a, b, tx], [c, d, ty], [0, 0, 1]]` — the one
/// `anim_scene::animation::LocalTransform::matrix` composes, with the translation in the
/// third element of each of the first two rows. `z` comes from 13.8's separate depth field,
/// not from the matrix, because 2D authoring keeps depth out of the 2D affine.
#[must_use]
pub fn node_matrix(world: &[[f64; 3]; 3], z: f64) -> Mat4 {
    Mat4::from_cols_array_2d(&row_major_affine(world, z))
}

/// The column-major 4×4 behind [`node_matrix`], spelled so the transpose is visible.
#[must_use]
pub fn row_major_affine(world: &[[f64; 3]; 3], z: f64) -> [[f32; 4]; 4] {
    let [row0, row1, _] = *world;
    [
        [row0[0] as f32, row1[0] as f32, 0.0, 0.0],
        [row0[1] as f32, row1[1] as f32, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [row0[2] as f32, row1[2] as f32, z as f32, 1.0],
    ]
}

fn identity_view() -> Mat4 {
    Mat4::IDENTITY
}

fn view_from_world(world: &Mat4) -> Result<Mat4, String> {
    if !world.is_finite() {
        return Err("the active camera's world matrix is not finite".to_owned());
    }
    let view = world.inverse();
    if !view.is_finite() {
        // A singular camera matrix — a zero scale authored on the camera node — inverts to
        // infinities. `glam` returns them rather than signalling, so the check is here.
        return Err(
            "the active camera's world matrix is singular; a camera cannot have zero scale"
                .to_owned(),
        );
    }
    Ok(view)
}

fn cut_serial_at(snapshot: &SceneSnapshot) -> u64 {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == anim_project::EventKind::CameraCut)
        .count() as u64
}

/// Every condition 15.7 (plus 7.2, 15.11, 30 and 32) says invalidates temporal history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalidation {
    /// Nothing disagrees with this frame; the history is usable.
    None,
    /// First frame this process has rendered, so there is no previous submission to
    /// compare with. 15.7's list calls this "scene load".
    SceneLoad,
    /// A camera cut fell in this frame's interval.
    CameraCut,
    /// The projection matrix changed shape: an extent change here, a lens change once
    /// WP15 authors one. 15.7 lists it separately from a cut.
    ProjectionChange,
    /// The targets were reallocated, so the previous vectors name rows that no longer exist
    /// at that address. 15.8's `target_generation` is what makes it detectable.
    Resize,
    /// The device was lost and recovered: 15.11's own sentence, and 21.5's "preview resumes
    /// through an explicit discontinuity" is the same event from the caller's side.
    DeviceLoss,
    /// The tick jumped, so the previous submission was not the immediately preceding step —
    /// a seek. A contiguous run cannot produce this, which is what makes it the condition
    /// that proves the others are real.
    DiscontinuousSeek,
    /// A node appeared, disappeared, or moved with no step in between. 15.7's "teleport"
    /// and "topology discontinuity" are one detection seen from two directions, so they are
    /// one condition with two names rather than two rules that can disagree about which
    /// fired.
    TopologyDiscontinuity,
    /// The document revision changed, which 7.2 lists alongside cut state and target
    /// generation. A revision change at the same tick is an edit, and an edit moves authored
    /// values the previous submission never saw.
    RevisionChange,
}

impl Invalidation {
    /// Whether this condition forces `HistoryState::valid` to false.
    ///
    /// A function rather than a `match` at each call site so that a new variant added later
    /// has to be answered here or the build stops at this line.
    #[must_use]
    pub const fn invalidates(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// 15.8's `HistoryState`, with the rule that decides what `valid` means.
#[derive(Clone, Debug)]
pub struct HistoryState {
    pub valid: bool,
    pub target_generation: u64,
    pub previous_view_proj: Mat4,
    pub previous_tick: Option<SimTick>,
    pub cut_serial: u64,
    /// The projection, extent and revision the fields above were computed for. None of the
    /// three is in 15.8's listing, and without them "projection change" and "revision
    /// change" are undetectable from the five fields the spec gives this type.
    pub previous_projection: Option<Mat4>,
    pub previous_extent: Option<Extent3d>,
    pub previous_revision: Option<Revision>,
    /// The condition that decided the last verdict, kept so a report can say *why* history
    /// restarted rather than only that it did.
    pub last_invalidation: Invalidation,
}

impl Default for HistoryState {
    fn default() -> Self {
        Self {
            // `valid: false` and `SceneLoad` are the same statement from two sides, and the
            // default has to be the one that is true before any frame exists.
            valid: false,
            target_generation: 0,
            previous_view_proj: Mat4::IDENTITY,
            previous_tick: None,
            cut_serial: 0,
            previous_projection: None,
            previous_extent: None,
            previous_revision: None,
            last_invalidation: Invalidation::SceneLoad,
        }
    }
}

impl HistoryState {
    /// Test every condition against this frame.
    #[must_use]
    pub fn detect(
        &self,
        camera: &CameraFrame,
        extent: Extent3d,
        generation: u64,
        tick: SimTick,
        revision: Revision,
    ) -> Vec<Invalidation> {
        let mut out = Vec::new();
        if !self.valid {
            out.push(Invalidation::SceneLoad);
        }
        if camera.cut_serial != self.cut_serial {
            out.push(Invalidation::CameraCut);
        }
        if self.previous_projection != Some(camera.projection) {
            out.push(Invalidation::ProjectionChange);
        }
        if self.previous_extent != Some(extent) {
            out.push(Invalidation::Resize);
        }
        if generation != self.target_generation {
            out.push(Invalidation::Resize);
        }
        if self.previous_revision != Some(revision) {
            out.push(Invalidation::RevisionChange);
        }
        if let Some(previous) = self.previous_tick {
            // A first tick has nothing to be contiguous with, and the absence of a previous
            // submission is already reported as `SceneLoad` above.
            if tick.0.saturating_sub(previous.0) != 1 {
                out.push(Invalidation::DiscontinuousSeek);
            }
        }
        out
    }

    /// The one place history is updated, and the reason 15.7's "previous transforms update
    /// only after successful submission" is structural rather than remembered: `graph.rs`
    /// calls this *after* `submit`, so a frame that failed to submit cannot leave a
    /// previous state the next frame will believe.
    ///
    /// The first condition that invalidates decides the verdict; the rest are still
    /// returned, because a stale `previous_tick` must not hide a resize behind it.
    pub fn commit_submission(
        &mut self,
        camera: &CameraFrame,
        extent: Extent3d,
        generation: u64,
        tick: SimTick,
        revision: Revision,
        submitted: bool,
    ) -> Vec<Invalidation> {
        if !submitted {
            // Nothing committed, nothing invalidated: the previous state still describes the
            // last thing the GPU completed, which is exactly what history is for.
            return vec![self.last_invalidation];
        }
        let conditions = self.detect(camera, extent, generation, tick, revision);
        let blocking: Vec<Invalidation> = conditions
            .iter()
            .copied()
            .filter(Invalidation::invalidates)
            .collect();
        self.valid = blocking.is_empty();
        self.last_invalidation = blocking.first().copied().unwrap_or(Invalidation::None);

        self.previous_view_proj = camera.view_proj;
        self.previous_projection = Some(camera.projection);
        self.previous_extent = Some(extent);
        self.previous_tick = Some(tick);
        self.cut_serial = camera.cut_serial;
        self.previous_revision = Some(revision);
        self.target_generation = generation;
        conditions
    }

    /// Mark history unusable and forget the tick, the device-loss path: the *contents* of
    /// the previous submission are gone, the authored state is not.
    pub fn invalidate_for_device_loss(&mut self) {
        self.valid = false;
        self.last_invalidation = Invalidation::DeviceLoss;
        self.previous_tick = None;
    }

    /// Whether the history is usable for a velocity pass, as the shader's validity input.
    #[must_use]
    pub const fn velocity_history_valid(&self) -> bool {
        self.valid
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

    const EXTENT: Extent3d = Extent3d {
        width: 1920,
        height: 1200,
        depth_or_array_layers: 1,
    };

    #[test]
    fn a_centred_point_projects_to_the_ndc_origin() {
        let p = projection(EXTENT);
        let clip = p * Vec4::new(0.0, 0.0, 0.0, 1.0);
        assert_eq!([clip.x, clip.y], [0.0, 0.0]);
        // `w` is 1 because there is no perspective divide anywhere in this package, which
        // is what makes 15.7's `previous w <= 0` test mean "behind the volume" and nothing
        // else.
        assert_eq!(clip.w, 1.0);
        // Authored z = 0 is the middle of the window, so it maps to half the Vulkan range.
        assert!(
            (clip.z - 0.5).abs() < 1e-6,
            "the depth window mapped to {} instead of 0.5",
            clip.z
        );
    }

    #[test]
    fn the_depth_window_maps_exactly_onto_vulkans_zero_to_one() {
        let p = projection(EXTENT);
        let near = p * Vec4::new(0.0, 0.0, Z_NEAR, 1.0);
        let far = p * Vec4::new(0.0, 0.0, Z_FAR, 1.0);
        assert_eq!(near.z, 0.0, "the near plane must land on the depth clear value");
        assert_eq!(far.z, 1.0);
        // 15.6 clears depth to 1.0 and compares Less, so a point *beyond* the far plane is
        // not merely equal to the clear value: it is refused by the same test that refuses
        // nothing at all. Reported rather than clamped, because a clamp would silently pull
        // an out-of-range node onto the far plane.
        let beyond = p * Vec4::new(0.0, 0.0, Z_FAR * 2.0, 1.0);
        assert!(beyond.z > 1.0, "out-of-range depth must stay visible as out-of-range");
    }

    /// The y-down half of the convention. This test is the difference between a scene
    /// rendered upside down and one rendered correctly, and both are finite and plausible.
    #[test]
    fn authored_downward_motion_lands_on_a_higher_row() {
        let p = projection(EXTENT);
        let above = p * Vec4::new(0.0, -25.0, 0.0, 1.0);
        let below = p * Vec4::new(0.0, 25.0, 0.0, 1.0);
        assert!(above.y > 0.0, "an authored negative y must reach NDC +y");
        assert!(below.y < 0.0);
        // And 15.7's formula turns NDC +y into the top rows.
        let uv_above = above.y * -0.5 + 0.5;
        let uv_below = below.y * -0.5 + 0.5;
        assert!(uv_above < uv_below, "NDC +y is the lower UV row, so +y authored is down");
    }

    #[test]
    fn the_node_matrix_puts_translation_in_the_last_column() {
        // Reading tx/ty as `world[2][0]`/`world[2][1]` instead would put the translation in
        // the depth row, move every sprite to z, and still produce finite numbers.
        let m = node_matrix(&[[2.0, 0.0, 7.0], [0.0, 3.0, 11.0], [0.0, 0.0, 1.0]], 5.0);
        let t = m.translation;
        assert_eq!([t.x, t.y, t.z], [7.0, 11.0, 5.0]);
        let scaled = m * Vec4::new(1.0, 1.0, 0.0, 1.0);
        assert_eq!([scaled.x, scaled.y], [9.0, 14.0]);
    }

    /// The identity case is the one WP04 can author today: no cut has fired, so no camera
    /// is on, and the canvas must still be framed rather than empty.
    #[test]
    fn an_unframed_scene_uses_the_identity_camera_not_a_guess() {
        let view = identity_view();
        assert_eq!(view * Vec4::new(10.0, 20.0, 0.0, 1.0), Vec4::new(10.0, 20.0, 0.0, 1.0));
    }

    #[test]
    fn a_singular_camera_is_refused_rather_than_inverted_to_infinities() {
        let zero_scale = Mat4::from_scale(glam::Vec3::ZERO, 1.0);
        assert!(
            view_from_world(&zero_scale).is_err(),
            "a camera with zero scale must not produce a view matrix"
        );
    }

    #[test]
    fn a_cut_sets_the_previous_camera_matrix_equal_to_the_current() {
        let current = CameraFrame {
            view: Mat4::IDENTITY,
            projection: Mat4::IDENTITY,
            view_proj: Mat4::from_translation(glam::Vec3::new(3.0, 0.0, 0.0)),
            cut_serial: 2,
        };
        let previous = CameraFrame {
            view_proj: Mat4::from_translation(glam::Vec3::new(-40.0, 0.0, 0.0)),
            ..current
        };
        let paired = current.matrices(Some(previous));
        assert_eq!(
            paired.current_view_proj, paired.previous_view_proj,
            "30's epoch rule: a cut produces a zero camera vector, not a smear"
        );
        // And without a cut, the previous matrix is carried — the case that makes an
        // object's velocity non-zero when only the camera moved.
        let carried = current.matrices(Some(CameraFrame {
            cut_serial: 2,
            ..previous
        }));
        assert_eq!(carried.previous_view_proj, previous.view_proj);
    }

    #[test]
    fn only_a_one_tick_step_is_contiguous() {
        let mut state = HistoryState::default();
        let camera = CameraFrame {
            view: Mat4::IDENTITY,
            projection: projection(EXTENT),
            view_proj: projection(EXTENT),
            cut_serial: 0,
        };
        let revision = Revision(1);
        // First commit: the detection runs against the default, so it reports the load.
        let first = state.commit_submission(&camera, EXTENT, 1, SimTick(10), revision, true);
        assert!(
            first.contains(&Invalidation::SceneLoad),
            "the first frame has no previous submission: {first:?}"
        );
        // Second, contiguous, unchanged in every other axis: valid.
        let second = state.commit_submission(&camera, EXTENT, 1, SimTick(11), revision, true);
        assert!(
            !second.iter().any(Invalidation::invalidates),
            "a contiguous identical step must not invalidate: {second:?}"
        );
        assert!(state.velocity_history_valid());
        // A jump of two ticks is a seek.
        let jumped = state.commit_submission(&camera, EXTENT, 1, SimTick(30), revision, true);
        assert!(
            jumped.contains(&Invalidation::DiscontinuousSeek),
            "a two-tick step is a discontinuous seek: {jumped:?}"
        );
        assert!(!state.velocity_history_valid());
    }

    #[test]
    fn a_failed_submission_commits_nothing() {
        let mut state = HistoryState::default();
        let camera = CameraFrame {
            view: Mat4::IDENTITY,
            projection: projection(EXTENT),
            view_proj: projection(EXTENT),
            cut_serial: 0,
        };
        let _ = state.commit_submission(&camera, EXTENT, 1, SimTick(10), Revision(1), true);
        let before = (state.previous_tick, state.cut_serial, state.target_generation);
        let after_failure =
            state.commit_submission(&camera, EXTENT, 9, SimTick(77), Revision(5), false);
        assert_eq!(
            (state.previous_tick, state.cut_serial, state.target_generation),
            before,
            "15.7: previous transforms update only after successful submission"
        );
        assert_eq!(after_failure, vec![state.last_invalidation]);
    }

    #[test]
    fn each_history_axis_invalidates_on_its_own() {
        let camera = CameraFrame {
            view: Mat4::IDENTITY,
            projection: projection(EXTENT),
            view_proj: projection(EXTENT),
            cut_serial: 0,
        };
        let mut state = HistoryState::default();
        // Seed a valid history.
        state.commit_submission(&camera, EXTENT, 1, SimTick(10), Revision(1), true);
        state.valid = true;
        state.commit_submission(&camera, EXTENT, 1, SimTick(11), Revision(1), true);
        state.valid = true;

        let cut = CameraFrame {
            cut_serial: 7,
            ..camera
        };
        assert!(state
            .detect(&cut, EXTENT, 1, SimTick(12), Revision(1))
            .contains(&Invalidation::CameraCut));
        assert!(state
            .detect(
                &camera,
                Extent3d {
                    width: 640,
                    height: 480,
                    depth_or_array_layers: 1,
                },
                1,
                SimTick(12),
                Revision(1)
            )
            .iter()
            .any(|c| *c == Invalidation::Resize));
        assert!(state
            .detect(&camera, EXTENT, 42, SimTick(12), Revision(1))
            .contains(&Invalidation::Resize));
        assert!(state
            .detect(&camera, EXTENT, 1, SimTick(12), Revision(2))
            .contains(&Invalidation::RevisionChange));
        assert!(state
            .detect(
                &CameraFrame {
                    projection: Mat4::IDENTITY,
                    view_proj: Mat4::IDENTITY,
                    ..camera
                },
                EXTENT,
                1,
                SimTick(12),
                Revision(1)
            )
            .contains(&Invalidation::ProjectionChange));
    }

    #[test]
    fn device_loss_forgets_the_tick_and_keeps_the_authored_state() {
        let mut state = HistoryState::default();
        state.previous_tick = Some(SimTick(9));
        state.previous_extent = Some(EXTENT);
        state.invalidate_for_device_loss();
        assert!(!state.valid);
        assert_eq!(state.previous_tick, None, "15.11: loss invalidates history");
        assert_eq!(
            state.previous_extent,
            Some(EXTENT),
            "21.5: the extent survives; only the GPU state is gone"
        );
    }

    /// 15.7 requires "signed i32 in documented camera units". The constant is the
    /// document, and this is the check that it is fine enough to make a tie meaningful.
    #[test]
    fn the_depth_quantum_separates_pixels_and_bundles_subpixels() {
        let bucket = |z: f32| (z * DEPTH_BUCKETS_PER_PIXEL) as i32;
        assert_ne!(bucket(0.0), bucket(1.0), "a pixel apart must not tie");
        assert_eq!(bucket(0.0), bucket(1.0 / 512.0), "a sub-pixel difference ties");
        assert_eq!(bucket(-3.5), -896);
    }

    #[test]
    fn every_condition_in_15_7_s_list_invalidates() {
        // The enum is 15.7's checklist plus 7.2's revision and 15.11's device loss. This
        // test is the guard that no variant was added as decoration.
        let named = [
            Invalidation::SceneLoad,
            Invalidation::CameraCut,
            Invalidation::ProjectionChange,
            Invalidation::Resize,
            Invalidation::DeviceLoss,
            Invalidation::DiscontinuousSeek,
            Invalidation::TopologyDiscontinuity,
            Invalidation::RevisionChange,
        ];
        for condition in named {
            assert!(
                condition.invalidates(),
                "{condition:?} is in 15.7's list but does not invalidate"
            );
        }
        assert!(!Invalidation::None.invalidates());
    }
}
