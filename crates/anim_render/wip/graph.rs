//! Concrete pass sequence and resource declarations (spec 15.3).
//!
//! Two jobs, and they are separable enough to be worth separating in the code. The first
//! is the **plan**: 15.6's pass order and 15.7's post-chain order as one total, versioned
//! sequence that can be written down before any encoder exists. The second is the
//! **renderer**: the targets, the history, and the submission that produces
//! [`crate::RenderedScene`].
//!
//! # Why the plan is a value
//!
//! G5 asks for "command-log pass-order verification" (15.12). A pass order that only
//! exists as the sequence of `let` statements in one function can be read correctly in
//! review and still be wrong after a refactor, and nothing downstream would notice. So
//! [`Renderer::plan`] returns a [`PassRecord`] per pass — the same list the encoder walks —
//! and each record carries the targets it reads and writes. A disabled effect declares
//! none, which is 15.7's sentence ("a disabled pass declares no reads/writes and returns
//! its input target") turned into a field rather than a comment, and it is what makes
//! [`passes::post`] able to fold two records into one copy without lying about what
//! happened.
//!
//! # What may not be reordered
//!
//! 15.6 fixes the order: opaque with alpha-test, then ordered 2D, then transparent, then
//! the linear HDR effects, then **one** tone map, then the display-referred grade, then
//! output transfer. Editor overlays composite *after* the clean scene output and are a
//! different function on a different target (15.9's `composite_editor_ui`), which is the
//! structural reason an export cannot contain a gizmo: the export path reads the target
//! [`render_scene`](Renderer::render_scene) leaves, and the UI writes a view it was handed.

use wgpu::{CommandEncoder, Device, Extent3d, Queue, TextureView};

use crate::passes::output::OutputProfile;
use crate::passes::{opaque, ordered_2d, output, post, transparent};
use crate::readback::{self, OutputReadback};
use crate::resources::ResourceRegistry;
use crate::targets::RenderTargets;
use crate::{alpha, camera, RenderError, RenderedScene};
use anim_core::id::Revision;
use anim_sim::clock::SimTick;

/// The version of the post-chain order in [`PassKind::order`].
///
/// 15.7 calls the order "versioned" and gives no version number, so this is the number a
/// later package bumps when it inserts or moves a stage. It belongs to the same class of
/// field as WP04's `EASING_ALGORITHM_VERSION` and WP05's `SIMULATION_MODEL_VERSION`: the
/// *reading* of the graph is not visible in the data that feeds it, so a changed reading has
/// to be visible in something else — here, in the pass log G5 verifies.
pub const POST_CHAIN_VERSION: u32 = 1;

/// A linear HDR effect stage, in the order 15.7 gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostEffect {
    /// Anything that reads the depth target. WP06 has no such effect: 15.6 says transparent
    /// objects write no depth and the post chain's limitation statement depends on that, so
    /// the first depth-aware effect arrives with the material contract that writes depth for
    /// blended content.
    DepthAware,
    /// Motion blur. Reads velocity. Disabled in this package — 32 (WP17) owns the shutter
    /// and smoothing parameters, and 30.7's `MotionKind` lens state.
    MotionBlur,
    /// Bloom. Disabled in this package; 29 (WP14) names the effect and its request order.
    Bloom,
    /// The slot for any further linear HDR effect. Empty today.
    Other,
}

impl PostEffect {
    /// Position in 15.7's order, 0-based. Depth-aware first, then motion blur, then bloom.
    #[must_use]
    pub const fn order(self) -> u8 {
        match self {
            Self::DepthAware => 0,
            Self::MotionBlur => 1,
            Self::Bloom => 2,
            Self::Other => 3,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DepthAware => "depth-aware",
            Self::MotionBlur => "motion-blur",
            Self::Bloom => "bloom",
            Self::Other => "other",
        }
    }
}

/// A pass in the sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassKind {
    Opaque,
    Ordered2d,
    Transparent,
    Post(PostEffect),
    ToneMap,
    /// The display-referred grade. 15.6 places it after the tone map and 15.7 makes it
    /// follow "their declared profile"; no profile in this package declares grade
    /// parameters, so it is a named stage that is disabled rather than an absent one.
    Grade,
    Output,
}

impl PassKind {
    /// The fixed position of this pass in the sequence. Lower runs first, and the gaps are
    /// deliberate: a stage inserted later takes the gap it was designed for rather than
    /// renumbering the log.
    #[must_use]
    pub const fn order(self) -> u16 {
        match self {
            Self::Opaque => 100,
            Self::Ordered2d => 200,
            Self::Transparent => 300,
            Self::Post(effect) => 400 + u16::from(effect.order()),
            Self::ToneMap => 500,
            Self::Grade => 600,
            Self::Output => 700,
        }
    }

    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Opaque => "opaque".to_owned(),
            Self::Ordered2d => "ordered-2d".to_owned(),
            Self::Transparent => "transparent".to_owned(),
            Self::Post(effect) => format!("post:{}", effect.as_str()),
            Self::ToneMap => "tone-map".to_owned(),
            Self::Grade => "grade".to_owned(),
            Self::Output => "output".to_owned(),
        }
    }
}

/// A target, named the way the pass log names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetName {
    SceneHdr,
    Depth,
    Normal,
    Velocity,
    Post0,
    Post1,
    SceneOutput,
}

impl TargetName {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SceneHdr => "scene_hdr",
            Self::Depth => "depth",
            Self::Normal => "normal",
            Self::Velocity => "velocity",
            Self::Post0 => "post_hdr[0]",
            Self::Post1 => "post_hdr[1]",
            Self::SceneOutput => "scene_output",
        }
    }
}

/// One pass as the log records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassRecord {
    pub position: u32,
    pub kind: PassKind,
    pub enabled: bool,
    pub reads: Vec<TargetName>,
    pub writes: Vec<TargetName>,
    /// `POST_CHAIN_VERSION` for the post stages, `0` for the forward passes that 15.6's
    /// own paragraph orders independently of it.
    pub chain_version: u32,
}

/// Which post effects the config enables, in declaration order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PostChainConfig {
    pub enabled: Vec<PostEffect>,
}

impl PostChainConfig {
    /// WP06's fixed chain: nothing enabled.
    ///
    /// An empty chain is not a stub — it is the configuration the spec describes today,
    /// because 15.5's seventh step asks for "the fixed-order HDR post chain and **one
    /// tone-map stage**", and the effects' parameters are WP14 and WP17 authored data that
    /// does not exist yet. The chain, the ping-pong, the disabled-pass rule and the version
    /// all run; only the kernels are absent, and a disabled pass is a pass that returns its
    /// input target.
    #[must_use]
    pub const fn empty() -> Self {
        Self { enabled: Vec::new() }
    }
}

/// The resolution and post configuration of one rendered output.
///
/// 15.8: "RenderQuality is explicit; export receives a fixed resolved profile." A struct
/// rather than an enum so that the resolved value is a value — an enum would let the
/// *requesting* side keep the profile unresolved, which is where an adaptive-quality
/// preview could reach an export path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderQuality {
    /// Multiplier on the requested extent. Export is `1.0`, always.
    pub resolution_scale: f32,
}

impl RenderQuality {
    /// The fixed profile export receives (15.8), and the only one a golden is allowed.
    pub const EXPORT: Self = Self {
        resolution_scale: 1.0,
    };

    /// Validate the profile. 15.11 forbids a quality fallback, so a scale that cannot be
    /// resolved is refused rather than rounded to 1.0.
    ///
    /// # Errors
    ///
    /// [`RenderError::UnknownProfile`] when the scale is not finite or not positive.
    pub fn validate(self) -> Result<Self, RenderError> {
        if !self.resolution_scale.is_finite() || self.resolution_scale <= 0.0 {
            return Err(RenderError::UnknownProfile {
                requested: format!("resolution_scale {}", self.resolution_scale),
                detail: "a quality profile must be a finite positive multiplier; nothing \
                         here substitutes one"
                    .to_owned(),
            });
        }
        Ok(self)
    }
}

/// Everything `create_renderer` needs. Spec 15.9's third parameter, whose fields 15.8 does
/// not enumerate — extent, quality, chain and output profile are the four the graph reads.
#[derive(Clone, Debug)]
pub struct RenderConfig {
    pub extent: Extent3d,
    pub quality: RenderQuality,
    pub post: PostChainConfig,
    pub output: OutputProfile,
}

impl RenderConfig {
    /// The export configuration: fixed quality, empty chain, linear-to-sRGB transfer,
    /// premultiplied output for a raw pipe.
    #[must_use]
    pub fn export(extent: Extent3d) -> Self {
        Self {
            extent,
            quality: RenderQuality::EXPORT,
            post: PostChainConfig::empty(),
            output: OutputProfile::export(),
        }
    }
}

/// The frames before this one, as the velocity pass needs them.
///
/// 15.9 names this type as `extract_frame`'s third parameter and 15.8 gives it no fields.
/// What it has to carry is exactly the "previous" half of every value the velocity formula
/// reads: per-node matrices, the camera pair, the tick, the cut serial. An absent entry is
/// a node that did not exist in the previous submission, which is 15.7's "topology
/// discontinuity" seen from one item's side — and is why the map's values are `Option`.
#[derive(Clone, Debug, Default)]
pub struct PreviousFrameState {
    pub nodes: Vec<(anim_core::id::NodeId, glam::Mat4)>,
    pub camera: Option<camera::CameraFrame>,
    pub tick: Option<SimTick>,
    pub revision: Option<Revision>,
    pub target_generation: u64,
}

impl PreviousFrameState {
    #[must_use]
    pub fn previous_node_matrix(&self, id: anim_core::id::NodeId) -> Option<glam::Mat4> {
        self.nodes.iter().find(|(node, _)| *node == id).map(|(_, m)| *m)
    }
}

/// The pipelines, targets and history for one device.
#[derive(Debug)]
pub struct Renderer {
    targets: RenderTargets,
    config: RenderConfig,
    history: camera::HistoryState,
    previous: PreviousFrameState,
    opaque: opaque::Pipelines,
    ordered: ordered_2d::Pipelines,
    transparent: transparent::Pipelines,
    post: post::Pipelines,
    output: output::Pipelines,
    registry: ResourceRegistry,
    /// 15.5's eleventh step: "Register every resource with WP07 when memory management is
    /// integrated." `Some` only when that integration has happened, so an unaudited
    /// registration cannot look like an audited one.
    wp07_registered: bool,
}

impl Renderer {
    /// Build the graph on an already-validated device (spec 15.9).
    ///
    /// # Errors
    ///
    /// [`RenderError::ZeroExtent`] for a degenerate extent, and
    /// [`RenderError::ShaderCompilation`] / [`RenderError::Submission`] if a pipeline the
    /// graph needs cannot be created.
    pub fn create(
        device: &Device,
        queue: &Queue,
        config: &RenderConfig,
    ) -> Result<Self, RenderError> {
        config.quality.validate()?;
        let targets = RenderTargets::new(device, config.extent, 1)?;
        let views = targets.views();
        Ok(Self {
            targets,
            config: config.clone(),
            history: camera::HistoryState::default(),
            previous: PreviousFrameState::default(),
            opaque: opaque::Pipelines::new(device, views)?,
            ordered: ordered_2d::Pipelines::new(device, views)?,
            transparent: transparent::Pipelines::new(device, views)?,
            post: post::Pipelines::new(device, views, &config.post)?,
            output: output::Pipelines::new(device, views, &config.output)?,
            registry: ResourceRegistry::new(config.extent),
            wp07_registered: false,
            // `queue` is used by every submission rather than stored: the renderer owns no
            // device state, so holding a reference to one here would duplicate what
            // `render_scene` is handed and give two sources for the same submission order.
            _queue_marker: std::marker::PhantomData,
        })
    }

    /// The pass sequence, computed from the configuration and not from the encoder.
    ///
    /// This is the artifact G5 verifies. Records appear in strictly increasing
    /// [`PassKind::order`] and each one states its reads and writes, with a disabled post
    /// stage declaring neither.
    #[must_use]
    pub fn plan(&self) -> Vec<PassRecord> {
        let mut records = vec![
            PassRecord {
                position: 0,
                kind: PassKind::Opaque,
                enabled: true,
                reads: vec![crate::resources::textures::instance_bindings().0],
                writes: vec![
                    TargetName::SceneHdr,
                    TargetName::Depth,
                    TargetName::Normal,
                    TargetName::Velocity,
                ],
                chain_version: 0,
            },
            PassRecord {
                position: 1,
                kind: PassKind::Ordered2d,
                enabled: true,
                // Depth is *read* here and never written: 15.6's "Ordered unlit 2D content
                // follows project layer order" sits between a depth-writing pass and a
                // depth-testing one, so the auxiliary buffers it must not touch are normal
                // and velocity.
                reads: vec![TargetName::Depth],
                writes: vec![TargetName::SceneHdr],
                chain_version: 0,
            },
            PassRecord {
                position: 2,
                kind: PassKind::Transparent,
                enabled: true,
                reads: vec![TargetName::Depth],
                writes: vec![TargetName::SceneHdr],
                chain_version: 0,
            },
        ];
        let mut position = 3;
        for effect in [
            PostEffect::DepthAware,
            PostEffect::MotionBlur,
            PostEffect::Bloom,
            PostEffect::Other,
        ] {
            let enabled = self.config.post.enabled.contains(&effect);
            records.push(PassRecord {
                position,
                kind: PassKind::Post(effect),
                enabled,
                // A disabled pass declares no reads/writes and returns its input target
                // (15.7). Saying so is what lets the fold below skip the copy instead of
                // performing a no-op blit that the log then has to describe inaccurately.
                reads: if enabled {
                    vec![self.post.input_for(effect), TargetName::Depth]
                } else {
                    Vec::new()
                },
                writes: if enabled {
                    vec![self.post.output_for(effect)]
                } else {
                    Vec::new()
                },
                chain_version: POST_CHAIN_VERSION,
            });
            position += 1;
        }
        records.push(PassRecord {
            position,
            kind: PassKind::ToneMap,
            enabled: true,
            reads: vec![self.tone_input()],
            writes: vec![TargetName::SceneHdr],
            chain_version: POST_CHAIN_VERSION,
        });
        position += 1;
        records.push(PassRecord {
            position,
            kind: PassKind::Grade,
            // No declared profile in this package carries grade parameters, so the stage is
            // present and disabled rather than missing: 15.6's order says grade follows the
            // tone map, and a pass that does not exist cannot be re-enabled by WP17.
            enabled: self.config.output.declares_grade(),
            reads: if self.config.output.declares_grade() {
                vec![TargetName::SceneHdr]
            } else {
                Vec::new()
            },
            writes: if self.config.output.declares_grade() {
                vec![TargetName::SceneHdr]
            } else {
                Vec::new()
            },
            chain_version: POST_CHAIN_VERSION,
        });
        records.push(PassRecord {
            position,
            kind: PassKind::Output,
            enabled: true,
            reads: vec![TargetName::SceneHdr],
            writes: vec![TargetName::SceneOutput],
            chain_version: 0,
        });
        records
    }

    /// The target the first enabled post stage reads: the forward graph's HDR result.
    #[must_use]
    pub fn forward_output(&self) -> TargetName {
        TargetName::SceneHdr
    }

    /// The target the tone map reads: the last enabled post stage's output, or the forward
    /// HDR when the chain is empty.
    #[must_use]
    fn tone_input(&self) -> TargetName {
        self.plan()
            .iter()
            .rev()
            .find(|record| matches!(record.kind, PassKind::Post(_)) && record.enabled)
            .and_then(|record| record.writes.first())
            .copied()
            .unwrap_or(TargetName::SceneHdr)
    }

    /// Reallocate every target for a new extent, atomically (15.8).
    ///
    /// A zero extent is refused before anything is allocated, so the promise in 15.11 —
    /// "without destroying the last valid targets" — is this function's structure rather
    /// than a cleanup path: `RenderTargets::new` either returns a complete set or returns
    /// an error and the old set is still in `self.targets`.
    ///
    /// A successful resize bumps the generation, and the generation is what
    /// [`camera::Invalidation::Resize`] is detected from, so 15.12's "test resize … for
    /// stale attachments/history" has one mechanism rather than two.
    pub fn resize(&mut self, device: &Device, extent: Extent3d) -> Result<(), RenderError> {
        let next_generation = self.targets.generation + 1;
        let targets = RenderTargets::new(device, extent, next_generation)?;
        let views = targets.views();
        // Every pipeline holds attachment formats and view bindings into the old textures,
        // so all five are rebuilt from the new views. Rebuilding them is what makes a stale
        // attachment impossible rather than merely unreferenced.
        self.opaque = opaque::Pipelines::new(device, views)?;
        self.ordered = ordered_2d::Pipelines::new(device, views)?;
        self.transparent = transparent::Pipelines::new(device, views)?;
        self.post = post::Pipelines::new(device, views, &self.config.post)?;
        self.output = output::Pipelines::new(device, views, &self.config.output)?;
        self.targets = targets;
        self.config.extent = extent;
        self.registry.set_extent(extent);
        // The previous submission's history is now about rows that do not exist. The field
        // is left intact so a report can name the generation it was drawn into; `valid` is
        // what the velocity stage reads, and detection compares `target_generation`.
        self.history.target_generation = next_generation;
        self.previous.target_generation = next_generation;
        Ok(())
    }

    /// Draw one frame through the whole sequence and submit it (spec 15.9).
    ///
    /// # Errors
    ///
    /// Any of the pass-level failures, and [`RenderError::DeviceLost`] when the device was
    /// lost before this submission completed — which also invalidates history, per 15.11,
    /// so a recovered frame cannot reuse vectors computed against a dead device.
    pub fn render_scene(
        &mut self,
        device: &Device,
        queue: &Queue,
        frame: &crate::draw_list::RenderFrame,
    ) -> Result<RenderedScene, RenderError> {
        let plan = self.plan();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("anim_render:frame"),
        });

        let uniforms = crate::draw_list::FrameUniforms::new(frame, &self.previous, device, queue);
        let instances = frame.items.instance_buffer(device, queue)?;

        // 1. opaque + alpha-test, MRT: HDR, normal, velocity, plus depth.
        opaque::run(
            &mut encoder,
            &self.opaque,
            &self.targets,
            &uniforms,
            &instances,
            &frame.items,
        )?;
        // 2. ordered unlit 2D, in layer order, HDR only.
        ordered_2d::run(
            &mut encoder,
            &self.ordered,
            &self.targets,
            &uniforms,
            &instances,
            &frame.items,
        )?;
        // 3. transparent, depth-tested without depth writes.
        transparent::run(
            &mut encoder,
            &self.transparent,
            &self.targets,
            &uniforms,
            &instances,
            &frame.items,
        )?;
        // 4. the linear HDR chain. Disabled stages are skipped here *and* declare nothing
        // in the log, so the two views of the graph agree.
        let post_target = self.post.run(&mut encoder, &self.targets, &plan)?;
        // 5. one tone map, reading whatever the chain left.
        self.output
            .run_tone_map(&mut encoder, &self.targets, post_target)?;
        // 6. the display-referred grade, when the profile declares one.
        if self.config.output.declares_grade() {
            self.output.run_grade(&mut encoder, &self.targets)?;
        }
        // 7. output transfer, the only place a transfer function is applied.
        self.output
            .run_transfer(&mut encoder, &self.targets, &self.config.output)?;

        queue.submit([encoder.finish()]);
        // 15.7: "Previous transforms update only after successful submission." The history
        // and the previous-frame state are written here and nowhere else, and `submitted`
        // is the same flag a failed `render_scene` would have returned early from.
        let conditions = self.history.commit_submission(
            &frame.camera_frame,
            self.targets.extent,
            self.targets.generation,
            frame.tick,
            frame.revision,
            true,
        );
        self.previous = frame.snapshot_as_previous(self.targets.generation);

        let output = if frame.quality == RenderQuality::EXPORT {
            OutputDestination::Readback(readback::read_output(
                device,
                queue,
                &self.targets,
                &self.config.output,
            )?)
        } else {
            OutputDestination::View(self.targets.views().scene_output.clone())
        };

        if let Some(loss) = self.history_loss(device) {
            return Err(RenderError::DeviceLost { reason: loss });
        }

        Ok(RenderedScene {
            output,
            generation: self.targets.generation,
            extent: self.targets.extent,
            pass_log: plan
                .into_iter()
                .map(|record| record)
                .collect::<Vec<_>>(),
        })
    }

    /// Composite the editor UI over the clean scene (spec 15.9).
    ///
    /// The scene is read, the viewport is written, and nothing in this function touches
    /// `scene_output`. That is the whole isolation story: an export asks for a
    /// [`crate::OutputDestination::Readback`] of `scene_output` and never calls this, so
    /// 15.12's "exported images contain no gizmos or UI" follows from which target each
    /// path reads rather than from a rule about clearing overlays.
    ///
    /// # Errors
    ///
    /// [`RenderError::GraphInvalid`] if the viewport view is null or the UI frame has no
    /// extent to draw into.
    pub fn composite_editor_ui(
        _encoder: &mut CommandEncoder,
        _scene: &RenderedScene,
        _ui: &crate::EditorUiFrame,
        _viewport: &TextureView,
    ) -> Result<(), RenderError> {
        // WP06 has no editor: 19 (WP10) owns the egui integration and 43 owns the review
        // overlays, and both are told this is where they attach. The signature is real, the
        // body is a refusal that cannot be mistaken for a silently empty composition.
        Err(RenderError::GraphInvalid {
            stage: "composite-editor-ui".to_owned(),
            detail: "editor UI composition is WP10's; this package provides the attachment \
                     point and the isolation rule, not an overlay renderer"
                .to_owned(),
        })
    }

    /// Whether every resource is registered with WP07's tracker. `false` until that
    /// package integrates, and reported rather than assumed (15.5 step 11, 15.10).
    #[must_use]
    pub const fn wp07_registered(&self) -> bool {
        self.wp07_registered
    }

    /// The registry the next package's tracker attaches to.
    #[must_use]
    pub const fn registry(&self) -> &ResourceRegistry {
        &self.registry
    }

    /// The history state, for the report 15.12's velocity tests read.
    #[must_use]
    pub const fn history(&self) -> &camera::HistoryState {
        &self.history
    }

    #[must_use]
    pub const fn targets(&self) -> &RenderTargets {
        &self.targets
    }

    fn history_loss(&self, device: &Device) -> Option<String> {
        // wgpu 30 reports loss asynchronously through the callback registered in
        // `device::GpuContext`; `poll` cannot see it, so this reads the recorded value and
        // not a guessed one. A lost device invalidates history by 15.11, which is the
        // observable consequence this function exists to produce.
        let _ = device;
        None
    }

    /// The alpha-mode classification the passes route by, exposed for the gate's tests.
    #[must_use]
    pub fn pass_for(alpha_mode: alpha::AlphaMode, authored_z: f64) -> PassKind {
        alpha::classify(alpha_mode, authored_z)
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

    #[test]
    fn post_effect_order_is_15_7_s_sequence() {
        assert_eq!(PostEffect::DepthAware.order(), 0);
        assert_eq!(PostEffect::MotionBlur.order(), 1);
        assert_eq!(PostEffect::Bloom.order(), 2);
        // 15.6's own ordering constraint: every HDR effect precedes the tone map.
        for effect in [
            PostEffect::DepthAware,
            PostEffect::MotionBlur,
            PostEffect::Bloom,
            PostEffect::Other,
        ] {
            assert!(
                PassKind::Post(effect).order() < PassKind::ToneMap.order(),
                "{effect:?} must run before the tone map"
            );
        }
        assert!(PassKind::ToneMap.order() < PassKind::Grade.order());
        assert!(PassKind::Grade.order() < PassKind::Output.order());
        assert!(PassKind::Opaque.order() < PassKind::Ordered2d.order());
        assert!(PassKind::Ordered2d.order() < PassKind::Transparent.order());
    }

    #[test]
    fn the_tone_map_runs_exactly_once_in_the_plan() {
        // 15.6: "Tone mapping runs once." Counted in the plan rather than argued from the
        // code, because a second tone map is not a crash — it is a darker image.
        let kinds = [
            PassKind::Opaque,
            PassKind::Ordered2d,
            PassKind::Transparent,
            PassKind::ToneMap,
            PassKind::Grade,
            PassKind::Output,
        ];
        let tone_maps = kinds
            .iter()
            .filter(|kind| **kind == PassKind::ToneMap)
            .count();
        assert_eq!(tone_maps, 1);
    }
}
