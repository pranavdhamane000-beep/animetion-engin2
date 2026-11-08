//! Output transfer, alpha conversion and final target (spec 15.3).
//!
//! This is the one place the linear scene becomes display-referred. DECISIONS.md #40 ruling
//! 3 chose `Rgba8Unorm` over the `Srgb` variant precisely so the encode is a named stage
//! here rather than fixed-function state the golden cannot attribute; 15.12's "exactly one
//! output transfer" is then a claim about this pass, and the pass log carries it.
//!
//! The final alpha mode is two entry points rather than a uniform branch, so which one ran
//! is visible in the pipeline and therefore in the log. 15.7: premultiplied output converts
//! to straight "only when the selected codec requires it".

use wgpu::{
    BindGroup, BindGroupLayout, CommandEncoder, Device, RenderPipeline, TextureFormat, TextureView,
};

use super::{color_target, pipeline_layout, shader_module, vertex_state};
use crate::graph::{PassKind, PassRecord};
use crate::{FinalAlpha, Result};

const SHADER: &str = include_str!("../shaders/output.wgsl");

/// The editor UI shader, exposed so `graph::create_ui_pipeline` can build against it.
///
/// The same file as the output transfer, but a different entry point: `ui_main` performs no
/// encode, because the UI texture is already display-referred. Encoding it again would be
/// the second transfer 15.12 forbids.
pub const UI_SHADER: &str = SHADER;

/// The output pass, one pipeline per final alpha convention.
pub struct OutputPass {
    premultiplied: RenderPipeline,
    straight: RenderPipeline,
}

impl OutputPass {
    /// Build both pipelines against the `Rgba8Unorm` clean output target.
    pub fn new(device: &Device, fullscreen_layout: &BindGroupLayout) -> Result<Self> {
        let module = shader_module(device, "output.wgsl", SHADER)?;
        let layout = pipeline_layout(device, "output_layout", &[fullscreen_layout]);
        let targets = [Some(color_target(TextureFormat::Rgba8Unorm, None))];
        let premultiplied = build(device, &module, &layout, &targets, "fs_premultiplied");
        let straight = build(device, &module, &layout, &targets, "fs_straight");
        Ok(Self {
            premultiplied,
            straight,
        })
    }

    /// Encode and write the clean output.
    pub fn record(
        &self,
        encoder: &mut CommandEncoder,
        source: &BindGroup,
        destination: &TextureView,
        source_name: &'static str,
        final_alpha: FinalAlpha,
    ) -> Result<PassRecord> {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("anim_render/output"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: destination,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(match final_alpha {
            FinalAlpha::Premultiplied => &self.premultiplied,
            FinalAlpha::Straight => &self.straight,
        });
        pass.set_bind_group(0, source, &[]);
        pass.draw(0..3, 0..1);
        Ok(PassRecord {
            kind: PassKind::Output,
            reads: vec![source_name],
            writes: vec!["scene_output"],
            draw_count: 1,
            enabled: true,
        })
    }
}

fn build(
    device: &Device,
    module: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    targets: &[Option<wgpu::ColorTargetState>],
    entry: &str,
) -> RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("anim_render/output"),
        layout: Some(layout),
        vertex: vertex_state(module, "vs_fullscreen"),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets,
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// A full-screen pipeline over an arbitrary source shader, for the editor UI composite.
///
/// Exposed because 15.9's `composite_editor_ui` receives no `Device` and so cannot create a
/// pipeline itself; `anim_editor` builds one through this at surface configuration time and
/// carries it in `EditorUiFrame`. The blend is the caller's, because the UI's alpha
/// convention is the editor's contract rather than the renderer's.
pub fn fullscreen_pipeline(
    device: &Device,
    layout: &BindGroupLayout,
    format: TextureFormat,
    blend: Option<wgpu::BlendState>,
    source: &str,
    entry: &str,
) -> RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("anim_render/ui"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(source)),
    });
    let pipeline_layout = pipeline_layout(device, "ui_layout", &[layout]);
    let targets = [Some(color_target(format, blend))];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("anim_render/ui"),
        layout: Some(&pipeline_layout),
        vertex: vertex_state(&module, "vs_fullscreen"),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &targets,
        }),
        multiview_mask: None,
        cache: None,
    })
}
