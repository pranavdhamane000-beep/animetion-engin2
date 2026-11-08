//! Sorted transparent forward pass (spec 15.3).
//!
//! 15.6: "Transparent items depth-test without depth writes. Initial transparent objects do
//! not write depth, normals or velocity, and post effects document the resulting
//! limitation." The limitation is real and recorded in ADR-0011: motion blur and DOF read
//! the velocity and depth targets, and transparent content is in neither, so a transparent
//! object gets no motion blur. That is a consequence of the spec's own permission list, not
//! an oversight this pass could fix by writing auxiliaries it is told not to write.
//!
//! The sort already happened, in `draw_list::total_cmp`. This pass draws in the order it is
//! given and does not sort again.

use wgpu::{BindGroup, BindGroupLayout, CommandEncoder, Device, RenderPipeline, TextureFormat};

use super::{color_target, depth_state, pipeline_layout, shader_module, vertex_state, BoundItem};
use crate::alpha::{blend_descriptor, AlphaMode};
use crate::graph::{PassKind, PassRecord};
use crate::targets::TargetViews;
use crate::Result;

const SHADER: &str = include_str!("../shaders/transparent.wgsl");

/// The transparent pass, with one pipeline per 15.7 blend descriptor.
pub struct TransparentPass {
    standard: RenderPipeline,
    additive: RenderPipeline,
}

impl TransparentPass {
    /// Build both pipelines.
    pub fn new(
        device: &Device,
        frame_layout: &BindGroupLayout,
        item_layout: &BindGroupLayout,
    ) -> Result<Self> {
        let module = shader_module(device, "transparent.wgsl", SHADER)?;
        let layout = pipeline_layout(device, "transparent_layout", &[frame_layout, item_layout]);
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

    /// Draw back-to-front in the order the total comparator produced.
    pub fn record(
        &self,
        encoder: &mut CommandEncoder,
        frame: &BindGroup,
        items: &[BoundItem],
        views: &TargetViews,
    ) -> Result<PassRecord> {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("anim_render/transparent"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &views.scene_hdr,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
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
            kind: PassKind::Transparent,
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
        label: Some("anim_render/transparent"),
        layout: Some(layout),
        vertex: vertex_state(module, "vs_main"),
        primitive: wgpu::PrimitiveState::default(),
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
