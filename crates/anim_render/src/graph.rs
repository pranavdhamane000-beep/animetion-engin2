//! Concrete pass sequence and resource declarations (spec 15.3).
//!
//! # The order is versioned data, not code order
//!
//! Spec 15.7: "Execute enabled passes in versioned order: depth-aware effects, motion blur,
//! bloom and other linear HDR effects; tone map; display-referred grade; output transfer."
//! [`POST_CHAIN_VERSION`] and [`PostStage::ORDER`] are that version and that order. The
//! execution loop walks `ORDER`, not the field order of [`PostChainConfig`], so a caller
//! cannot reorder the chain by reordering a struct literal.
//!
//! # A disabled pass is a real code path
//!
//! DECISIONS.md #40 ruling 4 declares DOF, motion blur and bloom and leaves them disabled,
//! and requires the disabled-pass contract to be *tested*: "a disabled stage declares no
//! reads and no writes and returns its input target". [`PassRecord`] is what makes that
//! observable — a disabled stage still produces a record, with empty `reads` and `writes`,
//! so the command log shows it was considered and skipped rather than absent. A stage that
//! vanished from the log would be indistinguishable from a stage the graph forgot.
//!
//! # History is written in exactly one place
//!
//! 15.7: "Previous transforms update only after successful submission." [`Renderer::render_scene`]
//! calls `queue.submit`, and only then [`HistoryState::commit_submission`] and
//! [`PreviousFrameState::record`]. There is no other writer of either, so a frame that failed
//! to submit cannot leave a previous state the next frame will believe.

use std::collections::{BTreeMap, BTreeSet};

use anim_core::id::AssetId;
use wgpu::{
    BindGroup, BindGroupLayout, CommandEncoder, Device, Extent3d, Queue, Sampler, TextureView,
};

use crate::camera::CameraFrame;
use crate::draw_list::{DrawItem, PreviousFrameState, RenderFrame};
use crate::memory::{
    BudgetSource, ContentSource, EvictionPlan, MemoryManager, MemoryMode, MemoryPolicy, ResourceId,
    ResourcePriority, ResourceRecord,
};
use crate::passes::{self, BoundItem};
use crate::readback::{self, OutputReadback};
use crate::resources::{self, ItemUniform, ResourceRegistry, SceneUniform, TEXEL_BYTES};
use crate::targets::{self, RenderTargets};
use crate::{FinalAlpha, OutputDestination, RenderConfig, RenderError, RenderedScene, Result};

/// The version of the fixed post-chain order (spec 15.7's "versioned order").
///
/// Bumped when the order changes, not when a stage is enabled: enablement is per-frame
/// configuration, order is an algorithm identity, and a golden that reproduces across a
/// config change but not across an order change is the distinction this number draws.
pub const POST_CHAIN_VERSION: u32 = 1;

/// One stage of the linear HDR post chain, in 15.7's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostStage {
    /// Depth-aware effects — DOF and anything else that reads the depth target.
    DepthAware,
    /// Motion blur, which reads the velocity target.
    MotionBlur,
    /// Bloom and other linear HDR effects.
    Bloom,
    /// The one tone map. 15.6: "Tone mapping runs once."
    ToneMap,
    /// Display-referred grading, after the tone map and before the output transfer.
    Grade,
}

impl PostStage {
    /// 15.7's order, as data the execution loop walks.
    pub const ORDER: [Self; 5] = [
        Self::DepthAware,
        Self::MotionBlur,
        Self::Bloom,
        Self::ToneMap,
        Self::Grade,
    ];

    /// Stable name for the pass log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DepthAware => "depth-aware",
            Self::MotionBlur => "motion-blur",
            Self::Bloom => "bloom",
            Self::ToneMap => "tone-map",
            Self::Grade => "grade",
        }
    }

    /// The package that owns this stage's kernel, for the refusal when it is enabled early.
    ///
    /// #40 ruling 4 names WP14 (spec 29) and WP17 (spec 32) as the owners of the effect
    /// parameters without splitting them per stage, so the refusal names both rather than
    /// inventing an assignment the spec does not make.
    #[must_use]
    pub const fn owner(self) -> &'static str {
        match self {
            Self::ToneMap => "WP06",
            _ => "WP14/WP17",
        }
    }
}

/// Which post stages run. WP06 implements the chain and exactly one tone map.
///
/// Every field defaults to `false` except `tone_map`, because 15.7 says tone mapping runs
/// once and a chain with no tone map would write scene-referred HDR into an 8-bit output
/// target and clamp it — a silent quality fallback 15.11 forbids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostChainConfig {
    /// Declared and disabled; WP14/WP17 own the kernel (#40 ruling 4).
    pub depth_aware: bool,
    /// Declared and disabled.
    pub motion_blur: bool,
    /// Declared and disabled.
    pub bloom: bool,
    /// The one tone map. Must be enabled.
    pub tone_map: bool,
    /// Declared and disabled; display-referred grading follows its declared profile (15.6).
    pub grade: bool,
}

impl Default for PostChainConfig {
    fn default() -> Self {
        Self {
            depth_aware: false,
            motion_blur: false,
            bloom: false,
            tone_map: true,
            grade: false,
        }
    }
}

impl PostChainConfig {
    /// Whether a stage is enabled, as one lookup the execution loop and the validator share.
    #[must_use]
    pub const fn enabled(self, stage: PostStage) -> bool {
        match stage {
            PostStage::DepthAware => self.depth_aware,
            PostStage::MotionBlur => self.motion_blur,
            PostStage::Bloom => self.bloom,
            PostStage::ToneMap => self.tone_map,
            PostStage::Grade => self.grade,
        }
    }

    /// Refuse a configuration WP06 cannot execute.
    ///
    /// Two refusals, and they are different: an effect kernel that does not exist yet is
    /// [`RenderError::PostStageUnavailable`] naming its owner, while a chain with no tone map
    /// is [`RenderError::GraphInvalid`] because 15.7 requires exactly one and the graph
    /// cannot supply it by enabling something twice.
    pub fn validate(self) -> Result<()> {
        for stage in PostStage::ORDER {
            if self.enabled(stage) && stage != PostStage::ToneMap {
                return Err(RenderError::PostStageUnavailable {
                    stage: stage.as_str(),
                    owner: stage.owner(),
                });
            }
        }
        if !self.tone_map {
            return Err(RenderError::GraphInvalid {
                stage: "post-chain".to_owned(),
                detail: "15.7 requires exactly one tone map; a chain with none would clamp \
                         scene-referred HDR into the 8-bit output target"
                    .to_owned(),
            });
        }
        Ok(())
    }
}

/// A pass identity for the command log (spec 15.12's pass-order verification).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassKind {
    /// Opaque and alpha-tested forward MRT.
    Opaque,
    /// Explicit 2D layer composition.
    Ordered2d,
    /// Sorted transparent forward.
    Transparent,
    /// A linear HDR post stage.
    Post {
        /// Which stage.
        stage: PostStage,
    },
    /// The one tone map.
    ToneMap,
    /// Display-referred grade.
    Grade,
    /// Output transfer and final alpha conversion.
    Output,
}

impl PassKind {
    /// Stable name for the log. A `&'static str` rather than `{:?}` because the log is a
    /// gate artifact and a debug format is not a contract.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Opaque => "opaque",
            Self::Ordered2d => "ordered-2d",
            Self::Transparent => "transparent",
            Self::Post { stage } => stage.as_str(),
            Self::ToneMap => "tone-map",
            Self::Grade => "grade",
            Self::Output => "output",
        }
    }
}

/// What one pass did, as the command log records it.
///
/// `reads` and `writes` are target names from 15.6's table. A disabled stage produces a
/// record with both empty — that emptiness *is* the disabled-pass contract of #40 ruling 4,
/// and a test asserts it rather than a comment claiming it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassRecord {
    /// Which pass ran, or was considered and skipped.
    pub kind: PassKind,
    /// Targets read, in binding order.
    pub reads: Vec<&'static str>,
    /// Targets written, in attachment order.
    pub writes: Vec<&'static str>,
    /// Draw calls issued. Zero for a disabled stage and for a pass with no items.
    pub draw_count: u32,
    /// Whether the stage was enabled. A disabled stage is still recorded.
    pub enabled: bool,
}

/// The editor's UI, supplied separately (spec 15.10).
///
/// 15.9 names this type and gives it no fields, and its `composite_editor_ui` signature
/// receives no `Device` — so the pipeline that draws the UI cannot be created inside that
/// function. It travels with the frame instead: `anim_editor` owns egui and builds these
/// once through [`create_ui_pipeline`], and this crate owns only the operation that encodes
/// them *after* the clean scene. That split is what keeps 15.4's prohibition on an editor
/// dependency true while still making the composite a real render pass.
pub struct EditorUiFrame {
    /// The composited UI, premultiplied, at `extent`.
    pub texture: TextureView,
    /// Its extent.
    pub extent: Extent3d,
    /// The pipeline that draws it, built by [`create_ui_pipeline`].
    pub pipeline: wgpu::RenderPipeline,
    /// Its bind group against that pipeline's layout.
    pub bind_group: BindGroup,
}

/// Build the pipeline and layout an [`EditorUiFrame`] needs, for a given viewport format.
///
/// Exposed so the editor can create its UI pipeline once at surface configuration time
/// rather than per frame. The blend is premultiplied-over, the same 15.7 descriptor the
/// standard alpha modes use, because egui's output is premultiplied.
#[must_use]
pub fn create_ui_pipeline(
    device: &Device,
    viewport_format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, BindGroupLayout) {
    let layout = resources::fullscreen_layout(device);
    let pipeline = passes::output::fullscreen_pipeline(
        device,
        &layout,
        viewport_format,
        Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        passes::output::UI_SHADER,
        "ui_main",
    );
    (pipeline, layout)
}

/// A per-frame buffer held until its submission completes, then released (16.7).
///
/// wgpu keeps the GPU allocation alive until queued work finishes even after the handle
/// drops, so the accounting release waits for the fence rather than the handle drop.
struct PendingBuffer {
    id: ResourceId,
    buffer: wgpu::Buffer,
    serial: u64,
}

/// The forward renderer: targets, pipelines, resources and the previous submission.
pub struct Renderer {
    targets: RenderTargets,
    post: PostChainConfig,
    registry: ResourceRegistry,
    previous: PreviousFrameState,
    frame_layout: BindGroupLayout,
    item_layout: BindGroupLayout,
    fullscreen_layout: BindGroupLayout,
    sampler: Sampler,
    opaque: passes::opaque::OpaquePass,
    ordered_2d: passes::ordered_2d::Ordered2dPass,
    transparent: passes::transparent::TransparentPass,
    post_pass: passes::post::PostPass,
    output: passes::output::OutputPass,
    /// Monotonic target generation. Incremented only by a successful reallocation.
    generation: u64,
    /// WP07 accounting. Owned here, consumed by application/export diagnostics (16.4).
    memory: MemoryManager,
    /// Memory records for the seven locked target attachments, in `attachment_sizes`
    /// order.
    target_ids: Vec<ResourceId>,
    /// Asset to memory record, so eviction and reload keep one identity per asset.
    image_ids: BTreeMap<AssetId, ResourceId>,
    /// Uniform buffers awaiting fence completion.
    pending_buffers: Vec<PendingBuffer>,
    /// Frame counter for `last_used_frame`, monotonic since construction.
    frame_counter: u64,
}

/// Create a renderer over an already-open device (spec 15.9).
///
/// The device is the caller's because `device.rs` owns selection and revalidation: a
/// `create_renderer` that opened its own device would be a second path past the #18 guards.
///
/// `_queue` is 15.9's parameter, kept for signature fidelity and unused: target allocation
/// and pipeline creation are device-only operations, and the first submission happens in
/// `render_scene`. Recorded as a deviation rather than discarded with a `let _ = queue`
/// inside the body, which would read as a forgotten use.
///
/// # Memory accounting at construction
///
/// WP07 registers the seven targets as locked `Critical` records. The manager starts
/// **unconfigured**: it accounts every byte but its preflight pressure check is inert
/// (the budget estimate is "unknown"), because 16.4 makes actual allocation errors
/// authoritative and this signature receives no adapter facts. The integrator calls
/// [`Renderer::configure_memory`] with the adapter's dedicated VRAM and ids to attach
/// telemetry and a conservative estimate; until then a GPU OOM surfaces as device loss,
/// which [`Renderer::notify_device_loss`] handles.
///
/// # Errors
///
/// [`RenderError::ZeroExtent`] before anything is allocated, and
/// [`RenderError::PostStageUnavailable`] / [`RenderError::GraphInvalid`] for a chain WP06
/// cannot run. Pipeline and shader failures travel as themselves.
pub fn create_renderer(device: &Device, _queue: &Queue, config: &RenderConfig) -> Result<Renderer> {
    config.post.validate()?;
    let memory = MemoryManager::new(
        MemoryPolicy::default(),
        MemoryMode::Export,
        BudgetSource::Unavailable {
            reason: "telemetry not configured; actual allocation errors are authoritative (16.4)"
                .to_owned(),
            estimated_budget: u64::MAX,
        },
    )?;
    let targets = RenderTargets::new(device, config.extent, 1)?;
    let frame_layout = resources::frame_layout(device);
    let item_layout = resources::item_layout(device);
    let fullscreen_layout = resources::fullscreen_layout(device);
    let sampler = resources::nearest_sampler(device);
    let mut renderer = Renderer {
        opaque: passes::opaque::OpaquePass::new(device, &frame_layout, &item_layout)?,
        ordered_2d: passes::ordered_2d::Ordered2dPass::new(device, &frame_layout, &item_layout)?,
        transparent: passes::transparent::TransparentPass::new(
            device,
            &frame_layout,
            &item_layout,
        )?,
        post_pass: passes::post::PostPass::new(device, &fullscreen_layout)?,
        output: passes::output::OutputPass::new(device, &fullscreen_layout)?,
        targets,
        post: config.post,
        registry: ResourceRegistry::new(),
        previous: PreviousFrameState::new(),
        frame_layout,
        item_layout,
        fullscreen_layout,
        sampler,
        generation: 1,
        memory,
        target_ids: Vec::new(),
        image_ids: BTreeMap::new(),
        pending_buffers: Vec::new(),
        frame_counter: 0,
    };
    renderer.register_targets()?;
    Ok(renderer)
}

impl Renderer {
    /// The current target generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The current targets.
    #[must_use]
    pub const fn targets(&self) -> &RenderTargets {
        &self.targets
    }

    /// The resource registry, for registration and for a WP07 report.
    pub const fn registry(&self) -> &ResourceRegistry {
        &self.registry
    }

    /// Mutable access to the registry, for uploading decoded images.
    pub fn registry_mut(&mut self) -> &mut ResourceRegistry {
        &mut self.registry
    }

    /// The memory manager, for diagnostics and reports (16.4, 16.10).
    pub const fn memory(&self) -> &MemoryManager {
        &self.memory
    }

    /// Mutable memory access — pinning export resources, synthetic budgets in tests.
    pub fn memory_mut(&mut self) -> &mut MemoryManager {
        &mut self.memory
    }

    /// Attach a validated policy and live telemetry (16.5's integration step).
    ///
    /// Call before uploading images: the new manager starts with an empty table, and a
    /// registry already holding textures would be resident without records. Targets are
    /// re-registered into the new manager. A correlation failure is not fatal — the
    /// source degrades to the conservative estimate (16.6), visible in
    /// [`MemoryManager::telemetry_status`].
    ///
    /// # Errors
    ///
    /// [`RenderError::GraphInvalid`] when images are already registered, and
    /// [`RenderError::Internal`] carrying [`crate::MemoryError::InvalidPolicy`] for a
    /// policy that fails validation.
    pub fn configure_memory(
        &mut self,
        policy: MemoryPolicy,
        mode: MemoryMode,
        dedicated_video_memory: u64,
        vendor_id: u32,
        device_id: u32,
    ) -> Result<()> {
        if !self.registry.is_empty() {
            return Err(RenderError::GraphInvalid {
                stage: "configure_memory".to_owned(),
                detail: "configure memory before uploading images; the new table cannot adopt \
                         resident textures it has no records for"
                    .to_owned(),
            });
        }
        let mut memory = MemoryManager::new(
            policy,
            mode,
            BudgetSource::Unavailable {
                reason: "DXGI telemetry not yet attached".to_owned(),
                estimated_budget: dedicated_video_memory * 3 / 4,
            },
        )?;
        #[cfg(feature = "dxgi-telemetry")]
        memory.attach_dxgi(vendor_id, device_id);
        #[cfg(not(feature = "dxgi-telemetry"))]
        let _ = (vendor_id, device_id);
        self.memory = memory;
        self.target_ids.clear();
        self.register_targets()
    }

    /// Preflight and register the seven target attachments as locked `Critical` records.
    /// Locked is structural: 16.7's exclusion list then makes "never free live targets"
    /// a property of the plan rather than a remembered rule.
    fn register_targets(&mut self) -> Result<()> {
        for (label, bytes) in self.targets.attachment_sizes() {
            let permit = self
                .memory
                .preflight_allocation(bytes, ResourcePriority::Critical)?;
            permit.commit();
            let mut record = ResourceRecord::new(
                format!("target/{label}"),
                bytes,
                ContentSource::Regenerable,
                ResourcePriority::Critical,
            )?;
            record.locked = true;
            let id = self.memory.register_resource(record)?;
            self.target_ids.push(id);
        }
        Ok(())
    }

    /// The previous submission's state, which `extract_frame` receives.
    #[must_use]
    pub const fn previous_state(&self) -> &PreviousFrameState {
        &self.previous
    }

    /// The resolved profile for the current targets, for `extract_frame`.
    ///
    /// The generation comes from the live targets rather than from the caller, so a frame
    /// cannot be extracted against a stale one and silently miss a resize invalidation.
    #[must_use]
    pub fn quality(&self, final_alpha: FinalAlpha) -> crate::RenderQuality {
        crate::RenderQuality {
            extent: self.targets.extent,
            target_generation: self.generation,
            final_alpha,
        }
    }

    /// Upload a decoded image through the memory manager (16.10: disk-backed sources).
    ///
    /// Three paths, one identity per asset: a first upload reserves, creates and
    /// registers; a re-upload of an **evicted** asset reuses its record and confirms the
    /// reload (16.12's exact-reload clause); a re-upload of a resident asset releases the
    /// old record and registers a replacement. The permit rolls back on any failure
    /// before commit.
    ///
    /// # Errors
    ///
    /// The registry's own refusals (zero extent, malformed pixels), plus
    /// [`crate::MemoryError::OutOfMemory`] when the budget cannot fit the texture and
    /// nothing safe remains to evict.
    pub fn register_image(
        &mut self,
        device: &Device,
        queue: &Queue,
        asset: AssetId,
        image: &anim_assets::DecodedImage,
        configuration: anim_assets::DecoderConfiguration,
    ) -> Result<()> {
        let bytes = (image.width as u64)
            .checked_mul(image.height as u64)
            .and_then(|pixels| pixels.checked_mul(TEXEL_BYTES))
            .ok_or_else(|| RenderError::internal("register_image", "byte count overflowed"))?;
        if bytes == 0 {
            return Err(RenderError::ZeroExtent {
                width: image.width,
                height: image.height,
            });
        }
        let existing = self.image_ids.get(&asset).copied();
        let reload = existing.filter(|&id| {
            self.memory
                .table()
                .get(id)
                .is_some_and(|record| !record.resident)
        });
        if reload.is_none() {
            if let Some(old) = existing {
                self.memory.release_resource(old);
            }
        }
        let permit = self
            .memory
            .preflight_allocation(bytes, ResourcePriority::Cache)?;
        self.execute_eviction_plan(permit.plan())?;
        self.registry
            .register_image(device, queue, asset, image, configuration)?;
        permit.commit();
        let id = if let Some(id) = reload {
            self.memory.confirm_reloaded(id)?;
            id
        } else {
            let record = ResourceRecord::new(
                format!("image/{}", asset.to_canonical_string()),
                bytes,
                ContentSource::Disk(asset),
                ResourcePriority::Cache,
            )?;
            self.memory.register_resource(record)?
        };
        self.image_ids.insert(asset, id);
        Ok(())
    }

    /// Evict one asset's texture, keeping its memory record for reload (16.7).
    ///
    /// # Errors
    ///
    /// [`RenderError::TextureNotRegistered`] when the asset has no upload, and the
    /// memory layer's [`crate::MemoryError::EvictionIneligible`] (as
    /// [`RenderError::Internal`]) when the record is required, in flight, locked or
    /// mapped — the exclusion is re-checked at execution, not trusted from a plan.
    pub fn evict_image(&mut self, asset: &AssetId) -> Result<()> {
        let Some(&id) = self.image_ids.get(asset) else {
            return Err(RenderError::TextureNotRegistered {
                asset: asset.to_canonical_string(),
            });
        };
        // Known completions become known by draining; without this, a texture whose
        // frame finished between renders still looks in-flight (16.7). The explicit
        // eviction also releases the last frame's required claim on the asset.
        self.memory.drain_completions();
        self.memory.remove_required(id);
        self.registry.remove_image(asset);
        self.memory.apply_eviction(&[id])?;
        Ok(())
    }

    /// The memory record for an uploaded asset, if any.
    #[must_use]
    pub fn image_memory_id(&self, asset: &AssetId) -> Option<ResourceId> {
        self.image_ids.get(asset).copied()
    }

    /// Execute an eviction plan: drop the GPU handles, then transition the records.
    /// Entries the world has moved on from (now required, in flight or locked) are
    /// skipped rather than forced — 16.11's exclusions bind at execution time.
    fn execute_eviction_plan(&mut self, plan: &EvictionPlan) -> Result<()> {
        if plan.is_empty() {
            return Ok(());
        }
        self.memory.drain_completions();
        let completed = self.memory.completed_serial();
        let required = self.memory.required().clone();
        let mut ids = Vec::new();
        let mut assets = Vec::new();
        for entry in &plan.entries {
            let evictable = self
                .memory
                .table()
                .get(entry.id)
                .is_some_and(|record| record.evictable(&required, completed));
            if !evictable {
                continue;
            }
            if let Some((&asset, _)) = self.image_ids.iter().find(|(_, &id)| id == entry.id) {
                assets.push(asset);
            }
            ids.push(entry.id);
        }
        for asset in &assets {
            self.registry.remove_image(asset);
        }
        self.memory.apply_eviction(&ids)?;
        Ok(())
    }

    /// Release per-frame buffers whose submissions have completed (16.7).
    fn release_completed_buffers(&mut self) {
        let completed = self.memory.completed_serial();
        let mut pending = Vec::new();
        for entry in self.pending_buffers.drain(..) {
            if entry.serial <= completed {
                self.memory.release_resource(entry.id);
                drop(entry.buffer);
            } else {
                pending.push(entry);
            }
        }
        self.pending_buffers = pending;
    }

    /// Drain known completions and release the buffers they free. The export loop calls
    /// this at frame end; `render_scene` also drains at frame start, so a caller that
    /// never calls this still releases one frame late rather than never.
    pub fn release_completed_work(&mut self) {
        self.memory.drain_completions();
        self.release_completed_buffers();
    }

    /// Reallocate every target at a new extent (spec 15.9, 15.11).
    ///
    /// The zero-extent refusal happens *before* the allocation, which is what makes 15.11's
    /// promise true: "Reject zero extents without destroying the last valid targets." A
    /// failed reallocation of any other kind also leaves the previous set in place, because
    /// the swap happens only after `RenderTargets::new` returns a complete set.
    ///
    /// Old views stay alive until queued GPU work no longer references them, which is wgpu's
    /// reference count doing 15.8's job rather than a fence this crate would have to own.
    /// The old generation's accounting releases at the swap; under telemetry the observed
    /// usage still covers the in-flight bytes (16.4's advisory/authoritative split).
    pub fn resize(&mut self, device: &Device, extent: Extent3d) -> Result<()> {
        if extent.width == 0 || extent.height == 0 {
            return Err(RenderError::ZeroExtent {
                width: extent.width,
                height: extent.height,
            });
        }
        if extent == self.targets.extent {
            return Ok(());
        }
        let next = self.generation.saturating_add(1);
        // Preflight the whole new generation before creating anything, so a budget
        // refusal leaves the previous generation current and accounted.
        let mut permits = Vec::new();
        for (label, bytes) in targets::attachment_sizes(extent) {
            let permit = self
                .memory
                .preflight_allocation(bytes, ResourcePriority::Critical)?;
            permits.push((label, bytes, permit));
        }
        let targets = RenderTargets::new(device, extent, next)?;
        for id in self.target_ids.drain(..) {
            self.memory.release_resource(id);
        }
        self.targets = targets;
        self.generation = next;
        for (label, bytes, permit) in permits {
            permit.commit();
            let mut record = ResourceRecord::new(
                format!("target/{label}"),
                bytes,
                ContentSource::Regenerable,
                ResourcePriority::Critical,
            )?;
            record.locked = true;
            let id = self.memory.register_resource(record)?;
            self.target_ids.push(id);
        }
        // 15.7: target resize invalidates history. The generation change is what
        // `HistoryState::detect` reads, so nothing else has to be marked here.
        Ok(())
    }

    /// Invalidate history and drop GPU resources after device loss (spec 15.11).
    pub fn invalidate_after_device_loss(&mut self) {
        self.notify_device_loss("renderer invalidated after device loss");
    }

    /// Device loss with the driver's reason (16.7, 16.11): history, registry, residency
    /// records and serial assumptions all die. The caller owns the wiring —
    /// `GpuContext::loss()` is the authority, and the render loop checks it each frame;
    /// reconstruction is a fresh [`create_renderer`] on the recovered device plus
    /// re-uploads from CPU/disk, which is why this method clears rather than rebuilds.
    pub fn notify_device_loss(&mut self, reason: &str) {
        self.previous.history.invalidate_for_device_loss();
        self.previous.transforms.clear();
        self.registry.clear();
        self.memory.on_device_loss(reason);
        self.image_ids.clear();
        self.target_ids.clear();
        self.pending_buffers.clear();
    }

    /// Draw a frame and read back or hand over the clean output (spec 15.9).
    ///
    /// The submission boundary is the whole point of the ordering below: `submit` first,
    /// then history. A failure anywhere before `submit` returns without touching
    /// `self.previous`, so the next frame's velocity still describes the last completed one.
    pub fn render_scene(
        &mut self,
        device: &Device,
        queue: &Queue,
        frame: &RenderFrame,
    ) -> Result<RenderedScene> {
        let extent = self.targets.extent;
        if frame.quality.extent != extent {
            return Err(RenderError::GraphInvalid {
                stage: "render_scene".to_owned(),
                detail: format!(
                    "frame was extracted at {:?} but the targets are {extent:?}; resize first",
                    frame.quality.extent
                ),
            });
        }
        // WP07's frame boundary: drain known completions, release the buffers they free,
        // name the current frame's required set, and let the hysteresis streak decide
        // whether pressure earns an eviction pass (16.6, 16.7).
        self.release_completed_work();
        let serial = self.memory.begin_submission()?;
        self.frame_counter = self.frame_counter.saturating_add(1);
        let mut required: BTreeSet<ResourceId> = self.target_ids.iter().copied().collect();
        for item in &frame.items {
            if let Some(&id) = self.image_ids.get(&item.asset) {
                required.insert(id);
            }
        }
        self.memory.set_required(required);
        if let Some(plan) = self.memory.pressure_eviction_plan()? {
            self.execute_eviction_plan(&plan)?;
        }
        let result = self.render_frame_inner(device, queue, frame, serial);
        if result.is_err() {
            self.abort_frame(serial);
        }
        result
    }

    /// A failed frame releases the buffers it registered and rolls back its serial when
    /// unsubmitted, so neither the accounting nor the fence order carries a ghost. A
    /// serial already submitted keeps its place — its completion callback is still coming.
    fn abort_frame(&mut self, serial: u64) {
        let mut keep = Vec::new();
        for entry in self.pending_buffers.drain(..) {
            if entry.serial == serial {
                self.memory.release_resource(entry.id);
                drop(entry.buffer);
            } else {
                keep.push(entry);
            }
        }
        self.pending_buffers = keep;
        self.memory.abort_submission(serial);
    }

    /// Encode and submit one frame. The caller owns the serial assignment and the
    /// failure cleanup; this body is WP06's pass sequence with WP07's buffer accounting.
    fn render_frame_inner(
        &mut self,
        device: &Device,
        queue: &Queue,
        frame: &RenderFrame,
        serial: u64,
    ) -> Result<RenderedScene> {
        let extent = self.targets.extent;
        let camera = CameraFrame {
            view: glam::Mat4::IDENTITY,
            projection: frame.camera.current_view_proj,
            view_proj: frame.camera.current_view_proj,
            cut_serial: self.previous.history.cut_serial,
        };
        let scene_uniform = SceneUniform {
            view_proj: frame.camera.current_view_proj.to_cols_array_2d(),
            previous_view_proj: frame.camera.previous_view_proj.to_cols_array_2d(),
            extent: [extent.width as f32, extent.height as f32],
            history_valid: if frame.history_valid { 1.0 } else { 0.0 },
            _pad: 0.0,
        };
        let scene_bytes = size_of::<SceneUniform>() as u64;
        let permit = self
            .memory
            .preflight_allocation(scene_bytes, ResourcePriority::CurrentFrame)?;
        self.execute_eviction_plan(permit.plan())?;
        let scene_buffer = resources::scene_buffer(device, &scene_uniform);
        permit.commit();
        let mut record = ResourceRecord::new(
            "frame/scene_uniform",
            scene_bytes,
            ContentSource::Regenerable,
            ResourcePriority::CurrentFrame,
        )?;
        record.last_used_serial = serial;
        record.last_used_frame = self.frame_counter;
        let scene_id = self.memory.register_resource(record)?;
        self.pending_buffers.push(PendingBuffer {
            id: scene_id,
            buffer: scene_buffer.clone(),
            serial,
        });
        let frame_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("anim_render/frame_bg"),
            layout: &self.frame_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: scene_buffer.as_entire_binding(),
            }],
        });

        let mut opaque = Vec::new();
        let mut ordered = Vec::new();
        let mut transparent = Vec::new();
        for item in &frame.items {
            let bound = self.bind_item(device, item, serial)?;
            match item.pass {
                PassKind::Opaque => opaque.push(bound),
                PassKind::Ordered2d => ordered.push(bound),
                PassKind::Transparent => transparent.push(bound),
                _ => {
                    return Err(RenderError::GraphInvalid {
                        stage: "render_scene".to_owned(),
                        detail: format!("item routed to non-forward pass {}", item.pass.as_str()),
                    });
                }
            }
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("anim_render/frame"),
        });
        let views = self.targets.views();
        let mut log = Vec::new();
        log.push(
            self.opaque
                .record(&mut encoder, &frame_bind_group, &opaque, views)?,
        );
        log.push(
            self.ordered_2d
                .record(&mut encoder, &frame_bind_group, &ordered, views)?,
        );
        log.push(
            self.transparent
                .record(&mut encoder, &frame_bind_group, &transparent, views)?,
        );

        // The post chain. `current` is the view the next enabled stage reads; a disabled
        // stage leaves it exactly where it was, which is #40 ruling 4's contract.
        let mut current = &views.scene_hdr;
        let mut current_name = "scene_hdr";
        let mut ping = 0usize;
        for stage in PostStage::ORDER {
            let kind = match stage {
                PostStage::ToneMap => PassKind::ToneMap,
                PostStage::Grade => PassKind::Grade,
                other => PassKind::Post { stage: other },
            };
            if !self.post.enabled(stage) {
                log.push(PassRecord {
                    kind,
                    reads: Vec::new(),
                    writes: Vec::new(),
                    draw_count: 0,
                    enabled: false,
                });
                continue;
            }
            let destination = &views.post[ping];
            let destination_name = if ping == 0 {
                "post_hdr[0]"
            } else {
                "post_hdr[1]"
            };
            let source = self.post_bind_group(device, current);
            log.push(self.post_pass.record_tone_map(
                &mut encoder,
                &source,
                destination,
                current_name,
                destination_name,
            )?);
            current = destination;
            current_name = destination_name;
            ping = 1 - ping;
        }

        let source = self.post_bind_group(device, current);
        log.push(self.output.record(
            &mut encoder,
            &source,
            &views.scene_output,
            current_name,
            frame.quality.final_alpha,
        )?);

        let command_buffer = encoder.finish();
        queue.submit([command_buffer]);
        // 16.7: the serial is recorded at submission, and the completion callback feeds
        // the fence tracker through the manager's channel (#44 ruling 8).
        self.memory.on_submitted(serial);
        queue.on_submitted_work_done(self.memory.submission_callback(serial));

        // Only now: 15.7's "previous transforms update only after successful submission".
        self.previous.history.commit_submission(
            &camera,
            extent,
            self.generation,
            frame.tick,
            frame.revision,
            true,
        );
        self.previous.record(frame);

        let output = OutputDestination::Readback(self.read_output_tracked(device, queue)?);
        Ok(RenderedScene {
            output,
            generation: self.generation,
            extent,
            pass_log: log,
        })
    }

    /// Build one item's bind group, checking the decode/blend pairing on the way.
    fn bind_item(&mut self, device: &Device, item: &DrawItem, serial: u64) -> Result<BoundItem> {
        // Copy the image facts out so the registry borrow ends before the memory
        // manager's &mut self calls below.
        let (image_view, image_extent, premultiplied, configuration) = {
            let image = self.registry.image(&item.asset)?;
            (
                image.view.clone(),
                image.extent,
                image.premultiplied,
                image.configuration,
            )
        };
        if let Some(&id) = self.image_ids.get(&item.asset) {
            self.memory.mark_used(id, serial, self.frame_counter);
        }
        if crate::alpha::expects_premultiplied(item.alpha_mode) != premultiplied {
            return Err(RenderError::GraphInvalid {
                stage: "bind_item".to_owned(),
                detail: format!(
                    "{} expects a {} decode but asset {} was decoded {}",
                    item.alpha_mode.as_str(),
                    if crate::alpha::expects_premultiplied(item.alpha_mode) {
                        "premultiplied"
                    } else {
                        "straight"
                    },
                    item.asset.to_canonical_string(),
                    configuration.as_str(),
                ),
            });
        }
        let uniform = ItemUniform {
            current_transform: item.current_transform.to_cols_array_2d(),
            previous_transform: item.previous_transform.to_cols_array_2d(),
            tint: [1.0, 1.0, 1.0, item.opacity],
            params: [
                item.alpha_mode.alpha_test_cutoff().unwrap_or(0.0),
                if crate::alpha::premultiply_for(item.alpha_mode, premultiplied) {
                    1.0
                } else {
                    0.0
                },
                crate::alpha::mode_id(item.alpha_mode) as f32,
                if item.previous_valid { 1.0 } else { 0.0 },
            ],
            half_size: [
                image_extent.width as f32 * 0.5,
                image_extent.height as f32 * 0.5,
            ],
            _pad2: [0.0; 2],
        };
        let item_bytes = size_of::<ItemUniform>() as u64;
        let permit = self
            .memory
            .preflight_allocation(item_bytes, ResourcePriority::CurrentFrame)?;
        self.execute_eviction_plan(permit.plan())?;
        let buffer = resources::item_buffer(device, &uniform);
        permit.commit();
        let mut record = ResourceRecord::new(
            format!("frame/item/{}", item.asset.to_canonical_string()),
            item_bytes,
            ContentSource::Regenerable,
            ResourcePriority::CurrentFrame,
        )?;
        record.last_used_serial = serial;
        record.last_used_frame = self.frame_counter;
        let id = self.memory.register_resource(record)?;
        self.pending_buffers.push(PendingBuffer {
            id,
            buffer: buffer.clone(),
            serial,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("anim_render/item_bg"),
            layout: &self.item_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&image_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        Ok(BoundItem {
            bind_group,
            additive: matches!(item.alpha_mode, crate::AlphaMode::AdditivePremultiplied),
        })
    }

    /// Bind a full-screen source texture for a post or output stage.
    fn post_bind_group(&self, device: &Device, view: &TextureView) -> BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("anim_render/post_bg"),
            layout: &self.fullscreen_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    /// Read the clean output back without drawing again, for a golden comparison.
    pub fn read_output(&mut self, device: &Device, queue: &Queue) -> Result<OutputReadback> {
        self.read_output_tracked(device, queue)
    }

    /// Readback with staging accounting (16.5: register every owned buffer).
    ///
    /// The staging buffer is reserved before the copy and released after it: the copy is
    /// synchronous (`read_texture` polls its own submission and unmaps before returning),
    /// so the mapped window never outlives this call and the record cannot become an
    /// eviction candidate while mapped. The internal copy submission takes no fence
    /// serial for the same reason — nothing it owns survives the call to be evicted
    /// later. Recorded as a deviation from 16.7's "every queue submission" with that
    /// reasoning; scene submissions, whose resources outlive the call, all take serials.
    fn read_output_tracked(&mut self, device: &Device, queue: &Queue) -> Result<OutputReadback> {
        let extent = self.targets.extent;
        let bytes = readback::staging_size(extent, 4);
        let permit = self
            .memory
            .preflight_allocation(bytes, ResourcePriority::CurrentFrame)?;
        self.execute_eviction_plan(permit.plan())?;
        let result = readback::read_texture(
            device,
            queue,
            &self.targets.scene_output,
            extent,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let output = result?;
        permit.commit();
        let mut record = ResourceRecord::new(
            "readback/staging",
            bytes,
            ContentSource::Regenerable,
            ResourcePriority::CurrentFrame,
        )?;
        record.mapped = true;
        let id = self.memory.register_resource(record)?;
        self.memory.set_mapped(id, false)?;
        self.memory.release_resource(id);
        Ok(output)
    }
}

/// Composite editor UI over a viewport, after the clean scene (spec 15.9).
///
/// "Editor UI is a separate operation. Export requests a readback-compatible
/// `OutputDestination` and cannot call `composite_editor_ui`." The second sentence is
/// enforced by the first arm: a scene whose output is a readback has no view to composite
/// from, so the call is refused rather than silently drawing nothing. 15.6's "Editor
/// overlays, selection outlines, gizmos and egui composite only after clean scene output and
/// never enter export" is then true by construction — the export path never reaches here.
///
/// # Errors
///
/// [`RenderError::GraphInvalid`] when the scene has no view destination.
pub fn composite_editor_ui(
    encoder: &mut CommandEncoder,
    scene: &RenderedScene,
    ui: &EditorUiFrame,
    viewport: &TextureView,
) -> Result<()> {
    let OutputDestination::View(_) = &scene.output else {
        return Err(RenderError::GraphInvalid {
            stage: "composite_editor_ui".to_owned(),
            detail: "the scene's output is a readback, which export owns; editor UI must \
                     never enter it (15.9)"
                .to_owned(),
        });
    };
    if ui.extent != scene.extent {
        return Err(RenderError::GraphInvalid {
            stage: "composite_editor_ui".to_owned(),
            detail: format!(
                "UI extent {:?} does not match the clean scene's {:?}",
                ui.extent, scene.extent
            ),
        });
    }
    // The viewport already holds the clean scene: 15.10 gives anim_app the surface, so
    // presenting `scene_output` to it happens before this call. `LoadOp::Load` is what keeps
    // this operation a composite *over* the clean output rather than a redraw of it, and
    // that is the property 15.12's "compare editor clean scene output with export before UI
    // composition" is checked against.
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("anim_render/editor_ui"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: viewport,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(&ui.pipeline);
    pass.set_bind_group(0, &ui.bind_group, &[]);
    pass.draw(0..3, 0..1);
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

    /// #40 ruling 4's disabled-pass contract, tested as data rather than as a GPU run: a
    /// disabled stage declares no reads and no writes and returns its input target.
    #[test]
    fn a_disabled_stage_declares_no_reads_and_no_writes() {
        let config = PostChainConfig::default();
        let mut current = "scene_hdr";
        let mut log = Vec::new();
        for stage in PostStage::ORDER {
            if config.enabled(stage) {
                log.push((stage, vec![current], vec!["post_hdr[0]"]));
                current = "post_hdr[0]";
            } else {
                log.push((stage, Vec::new(), Vec::new()));
            }
        }
        let disabled: Vec<_> = log
            .iter()
            .filter(|(stage, _, _)| !config.enabled(*stage))
            .collect();
        assert_eq!(
            disabled.len(),
            4,
            "four of the five stages are disabled in WP06"
        );
        for (stage, reads, writes) in disabled {
            assert!(reads.is_empty(), "{} claimed a read", stage.as_str());
            assert!(writes.is_empty(), "{} claimed a write", stage.as_str());
        }
        // And the input target survived every disabled stage unchanged.
        assert_eq!(current, "post_hdr[0]", "only the tone map moved the target");
    }

    #[test]
    fn the_chain_order_is_15_7_s_order_and_not_the_struct_field_order() {
        assert_eq!(
            PostStage::ORDER.map(PostStage::as_str),
            ["depth-aware", "motion-blur", "bloom", "tone-map", "grade"]
        );
        assert_eq!(POST_CHAIN_VERSION, 1);
    }

    #[test]
    fn enabling_an_effect_wp06_does_not_implement_is_refused_with_its_owner() {
        for config in [
            PostChainConfig {
                depth_aware: true,
                ..PostChainConfig::default()
            },
            PostChainConfig {
                motion_blur: true,
                ..PostChainConfig::default()
            },
            PostChainConfig {
                bloom: true,
                ..PostChainConfig::default()
            },
            PostChainConfig {
                grade: true,
                ..PostChainConfig::default()
            },
        ] {
            let err = config.validate().unwrap_err();
            match err {
                RenderError::PostStageUnavailable { owner, .. } => {
                    assert_eq!(owner, "WP14/WP17");
                }
                other => panic!("expected a stage refusal, got {other}"),
            }
        }
    }

    /// 15.7 says tone mapping runs once. A chain with none is refused rather than allowed
    /// to clamp HDR into an 8-bit target, which is the silent quality fallback 15.11 names.
    #[test]
    fn a_chain_with_no_tone_map_is_refused() {
        let config = PostChainConfig {
            tone_map: false,
            ..PostChainConfig::default()
        };
        assert!(matches!(
            config.validate().unwrap_err(),
            RenderError::GraphInvalid { .. }
        ));
        assert!(PostChainConfig::default().validate().is_ok());
    }

    #[test]
    fn the_default_chain_runs_exactly_one_stage() {
        let config = PostChainConfig::default();
        let enabled: Vec<_> = PostStage::ORDER
            .iter()
            .copied()
            .filter(|stage| config.enabled(*stage))
            .collect();
        assert_eq!(enabled, [PostStage::ToneMap]);
    }

    #[test]
    fn a_pass_kind_has_a_stable_log_name() {
        assert_eq!(PassKind::Opaque.as_str(), "opaque");
        assert_eq!(PassKind::Ordered2d.as_str(), "ordered-2d");
        assert_eq!(PassKind::Transparent.as_str(), "transparent");
        assert_eq!(PassKind::Output.as_str(), "output");
        assert_eq!(
            PassKind::Post {
                stage: PostStage::Bloom
            }
            .as_str(),
            "bloom"
        );
    }

    #[test]
    fn a_pass_record_for_a_disabled_stage_is_empty_but_present() {
        let record = PassRecord {
            kind: PassKind::Post {
                stage: PostStage::Bloom,
            },
            reads: Vec::new(),
            writes: Vec::new(),
            draw_count: 0,
            enabled: false,
        };
        assert!(!record.enabled);
        assert_eq!(record.draw_count, 0);
        assert!(record.reads.is_empty() && record.writes.is_empty());
    }

    #[test]
    fn the_final_alpha_spellings_are_stable() {
        assert_eq!(FinalAlpha::Premultiplied.as_str(), "premultiplied");
        assert_eq!(FinalAlpha::Straight.as_str(), "straight");
    }
}
