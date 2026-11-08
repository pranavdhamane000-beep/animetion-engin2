//! Linear HDR post chain and tone mapping (spec 15.3).
//!
//! WP06 implements exactly one stage of the chain: the tone map. DECISIONS.md #40 ruling 4
//! declares DOF, motion blur and bloom and leaves them disabled, and `PostChainConfig::validate`
//! refuses to enable them, so this module builds one pipeline rather than four against
//! kernels that do not exist. The disabled stages are still *recorded* by `graph.rs`, with
//! empty reads and writes — that is the contract #40 requires to be tested.
//!
//! The ping-pong is graph.rs's: this pass is handed a source bind group and a destination
//! view and does not know which of the two `post_hdr` targets either is. That keeps the
//! aliasing rule (15.7's "alias only when graph lifetimes do not overlap") in one place.

use wgpu::{
    BindGroup, BindGroupLayout, CommandEncoder, Device, RenderPipeline, TextureFormat, TextureView,
};

use super::{color_target, pipeline_layout, shader_module, vertex_state};
use crate::graph::{PassKind, PassRecord};
use crate::Result;

const SHADER: &str = include_str!("../shaders/post.wgsl");

/// The tone-map stage.
pub struct PostPass {
    tone_map: RenderPipeline,
}

impl PostPass {
    /// Build the tone-map pipeline against the `Rgba16Float` post targets.
    pub fn new(device: &Device, fullscreen_layout: &BindGroupLayout) -> Result<Self> {
        let module = shader_module(device, "post.wgsl", SHADER)?;
        let layout = pipeline_layout(device, "post_layout", &[fullscreen_layout]);
        let targets = [Some(color_target(TextureFormat::Rgba16Float, None))];
        let tone_map = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("anim_render/tone_map"),
            layout: Some(&layout),
            vertex: vertex_state(&module, "vs_fullscreen"),
            primitive: wgpu::PrimitiveState::default(),
            // No depth: the post chain operates on the resolved scene, not on geometry.
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_tone_map"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: None,
        });
        Ok(Self { tone_map })
    }

    /// Run the one tone map, reading `source` and writing `destination`.
    ///
    /// 15.6: "HDR post effects operate before tone mapping. Tone mapping runs once." The
    /// graph calls this exactly once per frame because `PostChainConfig` has one `tone_map`
    /// bool and `validate` refuses a chain without it.
    pub fn record_tone_map(
        &self,
        encoder: &mut CommandEncoder,
        source: &BindGroup,
        destination: &TextureView,
        source_name: &'static str,
        destination_name: &'static str,
    ) -> Result<PassRecord> {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("anim_render/tone_map"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: destination,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Every pixel is covered by the full-screen triangle, so a load would
                    // read a target nobody has written yet on the first frame.
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.tone_map);
        pass.set_bind_group(0, source, &[]);
        pass.draw(0..3, 0..1);
        Ok(PassRecord {
            kind: PassKind::ToneMap,
            reads: vec![source_name],
            writes: vec![destination_name],
            draw_count: 1,
            enabled: true,
        })
    }
}
