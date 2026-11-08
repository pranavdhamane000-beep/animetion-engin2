//! Opaque/alpha-test forward MRT pass (spec 15.3).
//!
//! 15.6: "Opaque and alpha-tested materials write HDR, depth, normals and velocity." That
//! is three colour attachments plus a depth attachment, and it is the only pass that writes
//! the auxiliary targets — the pass log records that, and
//! `the_command_log_records_15_6_and_15_7_s_order` in `tests/golden.rs` asserts it against what
//! actually ran rather than against a permission lookup.
//!
//! The pass always runs, even with no items, because it is what clears the four targets.
//! Skipping it on an empty frame would leave the previous frame's normals and velocity in
//! place, and a post effect reading them would see motion that no longer exists.

use wgpu::{BindGroup, BindGroupLayout, CommandEncoder, Device, RenderPipeline, TextureFormat};

use super::{color_target, depth_state, pipeline_layout, shader_module, vertex_state, BoundItem};
use crate::graph::{PassKind, PassRecord};
use crate::targets::TargetViews;
use crate::Result;

const SHADER: &str = include_str!("../shaders/scene.wgsl");

/// The forward MRT pipeline.
pub struct OpaquePass {
    pipeline: RenderPipeline,
}

impl OpaquePass {
    /// Build the pipeline. The three colour formats are 15.6's `scene_hdr`, `normal` and
    /// `velocity` rows, all `Rgba16Float`, in attachment order.
    pub fn new(
        device: &Device,
        frame_layout: &BindGroupLayout,
        item_layout: &BindGroupLayout,
    ) -> Result<Self> {
        let module = shader_module(device, "scene.wgsl", SHADER)?;
        let layout = pipeline_layout(device, "opaque_layout", &[frame_layout, item_layout]);
        let targets = [
            Some(color_target(TextureFormat::Rgba16Float, None)),
            Some(color_target(TextureFormat::Rgba16Float, None)),
            Some(color_target(TextureFormat::Rgba16Float, None)),
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("anim_render/opaque"),
            layout: Some(&layout),
            vertex: vertex_state(&module, "vs_main"),
            primitive: wgpu::PrimitiveState::default(),
            // Opaque writes depth: this is the pass that establishes it for the two
            // blended passes to test against.
            depth_stencil: Some(depth_state(true)),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: None,
        });
        Ok(Self { pipeline })
    }

    /// Clear all four targets and draw every opaque item.
    pub fn record(
        &self,
        encoder: &mut CommandEncoder,
        frame: &BindGroup,
        items: &[BoundItem],
        views: &TargetViews,
    ) -> Result<PassRecord> {
        let color = |view| {
            Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("anim_render/opaque"),
            color_attachments: &[
                color(&views.scene_hdr),
                color(&views.normal),
                color(&views.velocity),
            ],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &views.depth,
                depth_ops: Some(wgpu::Operations {
                    // 15.6: "clear 1.0; compare Less".
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, frame, &[]);
        for item in items {
            pass.set_bind_group(1, &item.bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        Ok(PassRecord {
            kind: PassKind::Opaque,
            reads: vec!["depth"],
            writes: vec!["scene_hdr", "normal", "velocity", "depth"],
            draw_count: items.len() as u32,
            enabled: true,
        })
    }
}
