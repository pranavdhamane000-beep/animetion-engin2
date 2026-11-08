//! Texture/buffer handles and resource-registry registration (spec 15.3).
//!
//! # This file is where the CPU and the WGSL agree
//!
//! [`SceneUniform`] and [`ItemUniform`] are the two layouts every shader in
//! `src/shaders/` reads. They are declared here, once, as `#[repr(C)]` `bytemuck::Pod`
//! structs, and the matching WGSL `struct` is written beside each one in a comment that
//! names the file that must agree. Inventing them twice — once in Rust and once in a
//! `.wgsl` — is how a stride and a binding disagree and the golden comes out a solid
//! colour, so the offsets are asserted by tests at the bottom of this file rather than
//! left to a reader with a calculator.
//!
//! Both are uniform buffers, not storage buffers, and the reason is a ruling rather than a
//! preference: DECISIONS.md #40 §6 keeps `required_features` at `Features::empty()`, and a
//! binding array of per-item textures needs `BINDING_ARRAY` on most adapters. Without it
//! every item already needs its own bind group to name its own texture, so a single
//! instance array would add a stride to agree on and a dynamic offset to bind, and buy
//! nothing. One [`ItemUniform`] buffer per item is the instance-transform buffer 15.5 step
//! 2 asks for, at the granularity the feature set actually allows.
//!
//! # What is registered, and what is not
//!
//! Spec 15.5 step 11 says "Register every resource with WP07 when memory management is
//! integrated". WP07 does not exist, so [`ResourceRegistry`] keeps the records WP07 will
//! consume — size, usage, ownership and the submission boundary 16.2 asks for — and
//! registers nothing with a module that is not there. The records are the seam; inventing a
//! budget allocator now would be WP07's design decision made early.

use std::collections::BTreeMap;

use anim_assets::{DecodedImage, DecoderConfiguration};
use anim_core::id::AssetId;
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt as _;
use wgpu::{
    BindGroupLayout, BindGroupLayoutEntry, BindingType, Buffer, BufferBindingType, BufferUsages,
    Device, Extent3d, ShaderStages, Texture, TextureDimension, TextureFormat, TextureSampleType,
    TextureUsages, TextureView, TextureViewDimension,
};

use crate::{RenderError, Result};

/// Bytes per pixel of the GPU form of a decoded image. `Rgba16Float` is four `f16`.
pub const TEXEL_BYTES: u64 = 8;

/// The frame-level uniform every pass reads.
///
/// WGSL counterpart — `shaders/scene.wgsl`, and included by the other four:
///
/// ```text
/// struct SceneUniform {
///   view_proj: mat4x4<f32>,          // offset   0, size 64
///   previous_view_proj: mat4x4<f32>, // offset  64, size 64
///   extent: vec2<f32>,               // offset 128, size  8
///   history_valid: f32,              // offset 136, size  4
///   _pad: f32,                       // offset 140, size  4
/// }                                  // total 144, align 16
/// ```
///
/// `history_valid` is an `f32` rather than a `bool` because WGSL's uniform layout has no
/// 1-byte type: a `bool` there would be 4 bytes with the next member's alignment decided by
/// the driver's reading of the spec rather than by this struct.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct SceneUniform {
    /// Current unjittered view-projection (15.7's velocity input).
    pub view_proj: [[f32; 4]; 4],
    /// Previous submission's view-projection. Equal to `view_proj` across a cut, which is
    /// what zeroes the camera's own motion vector without touching object vectors.
    pub previous_view_proj: [[f32; 4]; 4],
    /// Width and height in pixels, as `f32` so the shader's UV arithmetic has one spelling.
    pub extent: [f32; 2],
    /// 1.0 when temporal history is usable, 0.0 when any 15.7 condition invalidated it.
    pub history_valid: f32,
    /// Explicit tail padding. Present so the struct's size is a multiple of 16 without
    /// relying on the compiler's tail-padding rule, which `bytemuck` does not guarantee.
    pub _pad: f32,
}

/// The per-item uniform: 15.5 step 2's instance-transform buffer, one per draw.
///
/// WGSL counterpart — `shaders/scene.wgsl`:
///
/// ```text
/// struct ItemUniform {
///   current_transform: mat4x4<f32>,  // offset   0, size 64
///   previous_transform: mat4x4<f32>, // offset  64, size 64
///   tint: vec4<f32>,                 // offset 128, size 16  (rgb tint, a = opacity)
///   params: vec4<f32>,               // offset 144, size 16
///   half_size: vec2<f32>,            // offset 160, size  8  (canvas pixels)
///   _pad2: vec2<f32>,                // offset 168, size  8
/// }                                  // total 176, align 16
/// ```
///
/// `params` is `(alpha_cutoff, premultiply, alpha_mode_id, item_history_valid)`. The
/// premultiply flag is computed on the CPU by [`crate::alpha::premultiply_for`] rather than
/// branched on in the shader, so 15.7's "not multiplied again" rule has one producer and a
/// unit test instead of five shader copies.
///
/// `half_size` is the decoded image's half extent in canvas pixels. It is here and not in
/// the vertex buffer because the quad is generated from `vertex_index` in every shader:
/// WP04's `transform.scale` is a multiplier on authored content, and without the asset's
/// own pixel size a scale of 1 would draw a one-pixel sprite.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct ItemUniform {
    /// Current world transform, widened from 13.7's row-major 3x3 by `camera::node_matrix`.
    pub current_transform: [[f32; 4]; 4],
    /// Previous submission's transform for the same draw key.
    pub previous_transform: [[f32; 4]; 4],
    /// RGB tint and authored opacity from `EvaluatedNode::opacity`.
    pub tint: [f32; 4],
    /// `(alpha_cutoff, premultiply, alpha_mode_id, item_history_valid)`.
    pub params: [f32; 4],
    /// Half the decoded image's width and height, in canvas pixels.
    pub half_size: [f32; 2],
    /// Explicit tail padding to a 16-byte multiple.
    pub _pad2: [f32; 2],
}

/// A registered GPU image and the facts WP07 will need about it.
#[derive(Debug)]
pub struct ImageResource {
    /// The texture itself, `Rgba16Float` carrying **linear** pixels.
    pub texture: Texture,
    /// Its default view, cached with the resource rather than rebuilt per frame (15.8).
    pub view: TextureView,
    /// Extent the texture was allocated at.
    pub extent: Extent3d,
    /// Allocated bytes, excluding alignment padding. 16.2 asks for explicit sizes.
    pub size_bytes: u64,
    /// Whether the decode premultiplied, which decides the shader's multiply.
    pub premultiplied: bool,
    /// The configuration the decode ran under — the cache-key axis of spec 11.10.
    pub configuration: DecoderConfiguration,
}

/// Every GPU resource the renderer owns, in `AssetId` order.
///
/// A `BTreeMap` and not a `HashMap`: `clippy.toml` bans the latter because its iteration
/// order is not canonical, and 15.7 prohibits hash-map order in anything that reaches a
/// frame. The order here is not the draw order — that is the draw list's total comparator —
/// but a registry that iterates differently across runs makes a resource report
/// irreproducible, which is the same defect one report later.
#[derive(Debug, Default)]
pub struct ResourceRegistry {
    images: BTreeMap<AssetId, ImageResource>,
    /// Bytes registered, so a report does not have to walk the map to state a total.
    total_bytes: u64,
}

impl ResourceRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Upload a decoded image as a linear `Rgba16Float` texture.
    ///
    /// The transfer and the premultiply already happened in `anim_assets` against the
    /// manifest's claim (DECISIONS.md #41 ruling 2), so this function does neither: a
    /// renderer that decided the transfer itself would be free to change its mind on a
    /// driver move, which is exactly what #40 ruling 2 moved the decision into hashed data
    /// to prevent. `f32` to `f16` is the only conversion here, and it is a width reduction
    /// with no colour meaning.
    ///
    /// # Errors
    ///
    /// [`RenderError::ZeroExtent`] for a degenerate image, and
    /// [`RenderError::OutOfMemory`] when the allocation is refused.
    pub fn register_image(
        &mut self,
        device: &Device,
        queue: &wgpu::Queue,
        asset: AssetId,
        image: &DecodedImage,
        configuration: DecoderConfiguration,
    ) -> Result<()> {
        if image.width == 0 || image.height == 0 {
            return Err(RenderError::ZeroExtent {
                width: image.width,
                height: image.height,
            });
        }
        let expected = (image.width as usize)
            .checked_mul(image.height as usize)
            .ok_or_else(|| RenderError::internal("register_image", "pixel count overflowed"))?;
        if image.pixels.len() != expected {
            return Err(RenderError::internal(
                "register_image",
                format!(
                    "decoded image declares {}x{} = {expected} pixels but carries {}",
                    image.width,
                    image.height,
                    image.pixels.len()
                ),
            ));
        }
        let mut texels = Vec::with_capacity(expected * 4);
        for pixel in &image.pixels {
            for component in pixel {
                texels.push(half::f16::from_f32(*component));
            }
        }
        let extent = Extent3d {
            width: image.width,
            height: image.height,
            depth_or_array_layers: 1,
        };
        let size_bytes = (expected as u64) * TEXEL_BYTES;
        let texture = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("anim_render/image"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::Rgba16Float,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            },
            // One level, tightly packed: the source is a decoded image with no mip chain,
            // and `create_texture_with_data` computes the row pitch from the extent.
            wgpu::util::TextureDataOrder::LayerMajor,
            bytemuck::cast_slice(&texels),
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("anim_render/image_view"),
            dimension: Some(TextureViewDimension::D2),
            ..Default::default()
        });
        self.total_bytes += size_bytes;
        self.images.insert(
            asset,
            ImageResource {
                texture,
                view,
                extent,
                size_bytes,
                premultiplied: matches!(configuration, DecoderConfiguration::Rgba8Premultiplied),
                configuration,
            },
        );
        Ok(())
    }

    /// The registered image for an asset, or a refusal naming it.
    ///
    /// A missing texture is an error rather than a white placeholder: 15.11 forbids a
    /// silent substitute, and spec 11.11 allows a diagnostic placeholder only in preview
    /// and only "where safe", which a frame that is about to be compared against a golden
    /// is not.
    pub fn image(&self, asset: &AssetId) -> Result<&ImageResource> {
        self.images
            .get(asset)
            .ok_or_else(|| RenderError::TextureNotRegistered {
                asset: asset.to_canonical_string(),
            })
    }

    /// Drop one asset's GPU handle, returning it so the caller can hold it until queued
    /// work completes. WP07's eviction path uses this; the memory record survives in the
    /// manager's table so the reload can restore the same identity.
    pub fn remove_image(&mut self, asset: &AssetId) -> Option<ImageResource> {
        let image = self.images.remove(asset)?;
        self.total_bytes = self.total_bytes.saturating_sub(image.size_bytes);
        Some(image)
    }

    /// How many images are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Total allocated image bytes. The figure WP07's preflight will reserve against.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Every record in `AssetId` order, for a resource report.
    pub fn iter(&self) -> impl Iterator<Item = (&AssetId, &ImageResource)> {
        self.images.iter()
    }

    /// Drop every resource. Called on device loss, where 15.11 invalidates GPU targets.
    pub fn clear(&mut self) {
        self.images.clear();
        self.total_bytes = 0;
    }
}

/// Bind group 0: the frame uniform. Shared by all five passes.
#[must_use]
pub fn frame_layout(device: &Device) -> BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("anim_render/frame_layout"),
        entries: &[BindGroupLayoutEntry {
            binding: 0,
            visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
            ty: BindingType::Buffer {
                ty: BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(size_of::<SceneUniform>() as u64),
            },
            count: None,
        }],
    })
}

/// Bind group 1: one item's transform uniform, texture and sampler.
///
/// The sampler is `nearest` and clamped. A 2D animation engine composites authored pixels,
/// and a filtering sampler would invent values between them that no golden could
/// attribute; bilinear filtering is a material contract WP12 has not written.
#[must_use]
pub fn item_layout(device: &Device) -> BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("anim_render/item_layout"),
        entries: &[
            BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(size_of::<ItemUniform>() as u64),
                },
                count: None,
            },
            BindGroupLayoutEntry {
                binding: 1,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Texture {
                    sample_type: TextureSampleType::Float { filterable: false },
                    view_dimension: TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            BindGroupLayoutEntry {
                binding: 2,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
        ],
    })
}

/// The full-screen triangle layout the post and output passes read.
///
/// Binding 0 is the source texture, binding 1 a non-filtering sampler. Post reads one
/// `Rgba16Float` target and writes the other; output reads the last post target and writes
/// `scene_output`. Neither needs the frame uniform, which is why they have their own layout
/// rather than sharing [`frame_layout`].
#[must_use]
pub fn fullscreen_layout(device: &Device) -> BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("anim_render/fullscreen_layout"),
        entries: &[
            BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Texture {
                    sample_type: TextureSampleType::Float { filterable: false },
                    view_dimension: TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            BindGroupLayoutEntry {
                binding: 1,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
        ],
    })
}

/// Upload a [`SceneUniform`] as a uniform buffer.
#[must_use]
pub fn scene_buffer(device: &Device, uniform: &SceneUniform) -> Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("anim_render/scene_uniform"),
        contents: bytemuck::bytes_of(uniform),
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
    })
}

/// Upload an [`ItemUniform`] as a uniform buffer.
#[must_use]
pub fn item_buffer(device: &Device, uniform: &ItemUniform) -> Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("anim_render/item_uniform"),
        contents: bytemuck::bytes_of(uniform),
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
    })
}

/// The shared non-filtering, clamped sampler.
#[must_use]
pub fn nearest_sampler(device: &Device) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("anim_render/nearest"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    /// The layout test that makes the WGSL agreement checkable without a GPU.
    ///
    /// WGSL's uniform layout rules are: `mat4x4<f32>` align 16 size 64, `vec4<f32>` align 16
    /// size 16, `vec2<f32>` align 8 size 8, `f32` align 4 size 4, and a struct's size is
    /// rounded up to its alignment. These are the offsets written in the doc comments above,
    /// computed here from the Rust side so a field inserted in the middle fails this test
    /// rather than silently shifting every shader binding after it.
    #[test]
    fn the_scene_uniform_offsets_are_the_ones_the_wgsl_declares() {
        assert_eq!(size_of::<SceneUniform>(), 144);
        assert_eq!(align_of::<SceneUniform>(), 16);
        let base = SceneUniform::zeroed();
        let ptr = std::ptr::from_ref(&base).cast::<u8>();
        // SAFETY: every field is within the struct's own allocation, and the struct is
        // `Pod` so there is no padding byte being read as a value.
        unsafe {
            assert_eq!(
                std::ptr::from_ref(&base.view_proj)
                    .cast::<u8>()
                    .offset_from(ptr),
                0
            );
            assert_eq!(
                std::ptr::from_ref(&base.previous_view_proj)
                    .cast::<u8>()
                    .offset_from(ptr),
                64
            );
            assert_eq!(
                std::ptr::from_ref(&base.extent)
                    .cast::<u8>()
                    .offset_from(ptr),
                128
            );
            assert_eq!(
                std::ptr::from_ref(&base.history_valid)
                    .cast::<u8>()
                    .offset_from(ptr),
                136
            );
            assert_eq!(
                std::ptr::from_ref(&base._pad).cast::<u8>().offset_from(ptr),
                140
            );
        }
        // A uniform buffer binding must be a multiple of 16, which is the constraint that
        // makes the explicit tail padding load-bearing rather than decorative.
        assert_eq!(size_of::<SceneUniform>() % 16, 0);
    }

    #[test]
    fn the_item_uniform_offsets_are_the_ones_the_wgsl_declares() {
        assert_eq!(size_of::<ItemUniform>(), 176);
        assert_eq!(align_of::<ItemUniform>(), 16);
        let base = ItemUniform::zeroed();
        let ptr = std::ptr::from_ref(&base).cast::<u8>();
        // SAFETY: as above — every address is inside the struct's own allocation.
        unsafe {
            assert_eq!(
                std::ptr::from_ref(&base.current_transform)
                    .cast::<u8>()
                    .offset_from(ptr),
                0
            );
            assert_eq!(
                std::ptr::from_ref(&base.previous_transform)
                    .cast::<u8>()
                    .offset_from(ptr),
                64
            );
            assert_eq!(
                std::ptr::from_ref(&base.tint).cast::<u8>().offset_from(ptr),
                128
            );
            assert_eq!(
                std::ptr::from_ref(&base.params)
                    .cast::<u8>()
                    .offset_from(ptr),
                144
            );
            assert_eq!(
                std::ptr::from_ref(&base.half_size)
                    .cast::<u8>()
                    .offset_from(ptr),
                160
            );
            assert_eq!(
                std::ptr::from_ref(&base._pad2)
                    .cast::<u8>()
                    .offset_from(ptr),
                168
            );
        }
        assert_eq!(size_of::<ItemUniform>() % 16, 0);
    }

    /// `bytemuck::bytes_of` is what actually reaches the GPU, so the byte image is the
    /// contract, not just the offsets. A `[[f32; 4]; 4]` must be column-major contiguous,
    /// because that is what `mat4x4<f32>` reads.
    #[test]
    fn a_matrix_reaches_the_gpu_column_major_and_contiguous() {
        let mut uniform = ItemUniform::zeroed();
        uniform.current_transform = [
            [1.0, 2.0, 3.0, 4.0],
            [5.0, 6.0, 7.0, 8.0],
            [9.0, 10.0, 11.0, 12.0],
            [13.0, 14.0, 15.0, 16.0],
        ];
        let bytes = bytemuck::bytes_of(&uniform);
        let first: [f32; 4] = bytemuck::try_pod_read_unaligned(&bytes[0..16]).unwrap();
        assert_eq!(
            first,
            [1.0, 2.0, 3.0, 4.0],
            "the first column is the first 16 bytes"
        );
        let last: [f32; 4] = bytemuck::try_pod_read_unaligned(&bytes[48..64]).unwrap();
        assert_eq!(last, [13.0, 14.0, 15.0, 16.0]);
    }

    #[test]
    fn a_registry_reports_bytes_and_order_without_a_gpu() {
        let registry = ResourceRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.total_bytes(), 0);
        let ids: Vec<AssetId> = registry.iter().map(|(id, _)| *id).collect();
        assert!(ids.is_empty());
    }

    #[test]
    fn the_texel_width_matches_the_format_the_registry_uploads() {
        // Rgba16Float is four 16-bit components. A mismatch here would make every
        // `size_bytes` figure WP07 reserves against wrong by a factor of two.
        assert_eq!(TEXEL_BYTES, 8);
        assert_eq!(size_of::<half::f16>(), 2);
        assert_eq!(TEXEL_BYTES, 4 * size_of::<half::f16>() as u64);
    }
}
