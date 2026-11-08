//! Explicit 2D layer composition (spec 15.3).
//!
//! 15.6: "Ordered unlit 2D content follows project layer order; blended 2D items do not
//! write auxiliary normals/velocity unless a future material contract explicitly supports
//! it." No such contract exists, so this pass has one colour attachment and reads depth
//! without writing it.
//!
//! The order is the draw list's, which sorts on `layer_order` first — the node's position in
//! WP04's canonical visit order. This pass does not re-sort: a second sort here would be a
//! second source of the ordering truth, and 15.7 prohibits exactly that.

use wgpu::{BindGroup, BindGroupLayout, CommandEncoder, Device, RenderPipeline, TextureFormat};

use super::{color_target, depth_state, pipeline_layout, shader_module, vertex_state, BoundItem};
use crate::alpha::{blend_descriptor, AlphaMode};
use crate::graph::{PassKind, PassRecord};
use crate::targets::TargetViews;
use crate::Result;

const SHADER: &str = include_str!("../shaders/ordered_2d.wgsl");

/// The ordered 2D pass, with one pipeline per 15.7 blend descriptor.
pub struct Ordered2dPass {
    standard: RenderPipeline,
    additive: RenderPipeline,
}

impl Ordered2dPass {
    /// Build both pipelines. They differ only in blend state, which is fixed at creation.
    pub fn new(
        device: &Device,
        frame_layout: &BindGroupLayout,
        item_layout: &BindGroupLayout,
    ) -> Result<Self> {
        let module = shader_module(device, "ordered_2d.wgsl", SHADER)?;
        let layout = pipeline_layout(device, "ordered_2d_layout", &[frame_layout, item_layout]);
        let standard = build(
            device,
            &module,
            &layout,
            blend_descriptor(AlphaMode::Straight),
        );
        let additive = build(
            device,
            &module,
            &layout,
            blend_descriptor(AlphaMode::AdditivePremultiplied),
        );
        Ok(Self { standard, additive })
    }

    /// Composite in the order the draw list already established.
    pub fn record(
        &self,
        encoder: &mut CommandEncoder,
        frame: &BindGroup,
        items: &[BoundItem],
        views: &TargetViews,
    ) -> Result<PassRecord> {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("anim_render/ordered_2d"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &views.scene_hdr,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Load, not Clear: the opaque pass already wrote the scene.
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &views.depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, frame, &[]);
        for item in items {
            pass.set_pipeline(if item.additive {
                &self.additive
            } else {
                &self.standard
            });
            pass.set_bind_group(1, &item.bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        Ok(PassRecord {
            kind: PassKind::Ordered2d,
            reads: vec!["scene_hdr", "depth"],
            writes: vec!["scene_hdr"],
            draw_count: items.len() as u32,
            enabled: true,
        })
    }
}

fn build(
    device: &Device,
    module: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    blend: Option<wgpu::BlendState>,
) -> RenderPipeline {
    let targets = [Some(color_target(TextureFormat::Rgba16Float, blend))];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("anim_render/ordered_2d"),
        layout: Some(layout),
        vertex: vertex_state(module, "vs_main"),
        primitive: wgpu::PrimitiveState::default(),
        // Depth test, no depth write: 15.6's rule for blended content.
        depth_stencil: Some(depth_state(false)),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &targets,
        }),
        multiview_mask: None,
        cache: None,
    })
}
