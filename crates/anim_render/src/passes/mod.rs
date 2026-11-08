//! The five passes of spec 15.3, and the helpers they share.
//!
//! Every pass returns a [`crate::graph::PassRecord`] rather than `()`, because 15.12's
//! "command-log pass-order verification" needs an artifact that says what ran, what it read
//! and what it wrote. A pass that returned nothing would make the log something the graph
//! asserts rather than something the passes report.

pub mod opaque;
pub mod ordered_2d;
pub mod output;
pub mod post;
pub mod transparent;

use wgpu::{BindGroup, BindGroupLayout, Device, PipelineLayout, ShaderModule};

use crate::{RenderError, Result};

/// One item ready to draw: its bind group, built by `graph.rs` from the registry.
pub struct BoundItem {
    /// The item's uniform, texture and sampler at group 1.
    pub bind_group: BindGroup,
    /// Whether this item needs the additive pipeline. A blend state is fixed at pipeline
    /// creation, so 15.7's two blend descriptors are two pipelines rather than a branch.
    pub additive: bool,
}

/// Parse WGSL with naga before handing it to wgpu, so a shader defect stays a typed error.
///
/// `create_shader_module` routes a compile failure through the device's error scope and, in
/// a build with no validation layers installed (BUILD_STATE risk 4), that path is not a
/// reliable place to learn which line was wrong. Parsing first gives the span.
pub fn shader_module(device: &Device, name: &str, source: &str) -> Result<ShaderModule> {
    if let Err(error) = wgpu::naga::front::wgsl::parse_str(source) {
        return Err(RenderError::ShaderCompilation {
            shader: name.to_owned(),
            detail: error.emit_to_string(source),
        });
    }
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(name),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(source)),
    }))
}

/// A pipeline layout over the given bind group layouts, in group order.
///
/// `bind_group_layouts` is a slice of `Option<&BindGroupLayout>` in wgpu 30, and
/// `immediate_size` replaced `push_constant_ranges`. Both are written explicitly here so
/// the next wgpu bump fails at this one function rather than at five call sites.
pub fn pipeline_layout(
    device: &Device,
    label: &str,
    layouts: &[&BindGroupLayout],
) -> PipelineLayout {
    let owned: Vec<Option<&BindGroupLayout>> = layouts.iter().map(|layout| Some(*layout)).collect();
    device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &owned,
        immediate_size: 0,
    })
}

/// The depth state 15.6 specifies: `Depth32Float`, clear 1.0, compare `Less`.
///
/// `write` is the whole difference between the forward pass and the two blended passes:
/// 15.6 says transparent items "depth-test without depth writes", so the same comparison
/// runs with the write disabled.
#[must_use]
pub fn depth_state(write: bool) -> wgpu::DepthStencilState {
    wgpu::DepthStencilState {
        format: wgpu::TextureFormat::Depth32Float,
        depth_write_enabled: Some(write),
        depth_compare: Some(wgpu::CompareFunction::Less),
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState::default(),
    }
}

/// The vertex state every pass here uses: no vertex buffer, position generated from
/// `vertex_index`.
#[must_use]
pub fn vertex_state<'a>(module: &'a ShaderModule, entry: &'a str) -> wgpu::VertexState<'a> {
    wgpu::VertexState {
        module,
        entry_point: Some(entry),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        buffers: &[],
    }
}

/// A colour target with an optional blend.
#[must_use]
pub fn color_target(
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
) -> wgpu::ColorTargetState {
    wgpu::ColorTargetState {
        format,
        blend,
        write_mask: wgpu::ColorWrites::ALL,
    }
}
