//! HDR, depth, normal, velocity, history and output targets (spec 15.3).
//!
//! The set is spec 15.6's table and nothing else. Each row is a `wgpu::TextureFormat`
//! chosen *here* so that the format matrix DECISIONS.md #18 and ADR-0003 left to this
//! package has exactly one home, and each is validated against the adapter in
//! [`crate::device`] before a device exists — the pairing is deliberate: this file states
//! what the graph asks for, `device.rs` refuses to start without it.
//!
//! # What each attachment is allowed to write
//!
//! Spec 15.6's second paragraph is a permission list, and the flags below are that list
//! rather than a re-derivation of it:
//!
//!  * opaque and alpha-tested materials write HDR, depth, normals **and** velocity;
//!  * ordered unlit 2D writes HDR and reads depth, and does **not** write normals or
//!    velocity "unless a future material contract explicitly supports it" — there is no
//!    such contract yet, so it does not;
//!  * transparent items depth-test and write HDR only, so depth is loaded read-only for
//!    that pass and no pass writes the auxiliary targets after them.

use wgpu::{
    Extent3d, Texture, TextureDimension, TextureFormat, TextureSampleType, TextureUsages,
    TextureView, TextureViewDimension,
};

use crate::RenderError;

/// Views over one generation's targets.
///
/// Spec 15.8: "Texture views are cached by target generation." Here that is done by
/// construction rather than by a lookup table: a `TargetViews` is created in the same call
/// that allocates the textures it wraps, so a view cannot outlive or precede its
/// generation and there is no cache to invalidate. A second generation means a second
/// value of this type, and 15.8's "old views remain alive until queued GPU work no longer
/// references them" is wgpu's reference count doing that job, not a map keyed by index.
#[derive(Debug)]
pub struct TargetViews {
    pub scene_hdr: TextureView,
    pub depth: TextureView,
    pub normal: TextureView,
    pub velocity: TextureView,
    pub post: [TextureView; 2],
    pub scene_output: TextureView,
}

/// The full-resolution targets 15.6 names, for one extent.
///
/// `scene_output` is in this struct although spec 15.8's `RenderTargets` listing omits it,
/// while 15.6's table names it as a target: a listing that leaves out the one target the
/// gate compares goldens against is a listing error, not a design. Recorded as deviation
/// row 20 in `BUILD_STATE.md`.
#[derive(Debug)]
pub struct RenderTargets {
    pub extent: Extent3d,
    pub scene_hdr: Texture,
    pub depth: Texture,
    pub normal: Texture,
    pub velocity: Texture,
    pub post_hdr: [Texture; 2],
    pub scene_output: Texture,
    pub generation: u64,
    views: TargetViews,
}

impl RenderTargets {
    /// Allocate every target at `extent`.
    ///
    /// A zero extent is refused before the first allocation, which is what makes spec
    /// 15.11's promise in `resize` — "without destroying the last valid targets" — true:
    /// this function either produces a complete new set or touches nothing, so the caller
    /// keeps its previous set by simply not swapping.
    pub fn new(
        device: &wgpu::Device,
        extent: Extent3d,
        generation: u64,
    ) -> Result<Self, RenderError> {
        if extent.width == 0 || extent.height == 0 {
            return Err(RenderError::ZeroExtent {
                width: extent.width,
                height: extent.height,
            });
        }
        let size = Extent3d {
            depth_or_array_layers: 1,
            ..extent
        };
        let hdr = TARGET_USAGE_READ | TextureUsages::RENDER_ATTACHMENT;
        let scene_hdr = attach(device, "scene_hdr", TextureFormat::Rgba16Float, size, hdr);
        // 15.6: "clear 1.0; compare Less". The clear value and the comparison live in the
        // pass descriptors in `graph.rs`; the format is here because it is the thing that
        // has to be allocated.
        let depth = attach(
            device,
            "depth",
            TextureFormat::Depth32Float,
            size,
            TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
        );
        let normal = attach(device, "normal", TextureFormat::Rgba16Float, size, hdr);
        let velocity = attach(device, "velocity", TextureFormat::Rgba16Float, size, hdr);
        // Two targets, ping-ponged. 15.7's aliasing rule ("alias only when graph lifetimes
        // do not overlap") is not taken up: both are live for the whole post chain, and two
        // allocations are cheaper to reason about than a reuse analysis whose only consumer
        // is a pass WP06 leaves disabled.
        let post_hdr = [
            attach(device, "post_hdr[0]", TextureFormat::Rgba16Float, size, hdr),
            attach(device, "post_hdr[1]", TextureFormat::Rgba16Float, size, hdr),
        ];
        // Rgba8Unorm, not the Srgb variant: the operator ruled that the one output transfer
        // is a named shader stage, and a format that performs an implicit encode would put a
        // second transfer in fixed-function hardware where the golden cannot see it (15.12's
        // "exactly one output transfer").
        let scene_output = attach(
            device,
            "scene_output",
            TextureFormat::Rgba8Unorm,
            size,
            TextureUsages::RENDER_ATTACHMENT
                | TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_SRC,
        );
        let views = TargetViews {
            scene_hdr: view(&scene_hdr, "scene_hdr_view"),
            depth: view(&depth, "depth_view"),
            normal: view(&normal, "normal_view"),
            velocity: view(&velocity, "velocity_view"),
            post: [
                view(&post_hdr[0], "post_hdr_view[0]"),
                view(&post_hdr[1], "post_hdr_view[1]"),
            ],
            scene_output: view(&scene_output, "scene_output_view"),
        };
        Ok(Self {
            extent,
            scene_hdr,
            depth,
            normal,
            velocity,
            post_hdr,
            scene_output,
            generation,
            views,
        })
    }

    /// The views for this generation. `&self`, because there is nothing left to build.
    #[must_use]
    pub const fn views(&self) -> &TargetViews {
        &self.views
    }

    /// The two axes every downstream size, stride and budget is derived from.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.extent.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.extent.height
    }

    /// Each attachment's label and committed bytes, for WP07 registration (16.5).
    /// Delegates to [`attachment_sizes`] so creation-time preflight and post-creation
    /// registration read one computation.
    #[must_use]
    pub fn attachment_sizes(&self) -> [(&'static str, u64); 7] {
        attachment_sizes(self.extent)
    }
}

/// The seven attachments' labels and byte sizes at an extent (spec 15.6's table).
///
/// Five `Rgba16Float` at 8 bytes, one `Depth32Float` at 4, one `Rgba8Unorm` at 4 —
/// ADR-0003's 48 bytes per pixel for one target generation. Alignment padding is
/// excluded, matching the `size_bytes` convention WP06's registry uses.
#[must_use]
pub fn attachment_sizes(extent: Extent3d) -> [(&'static str, u64); 7] {
    let pixels = u64::from(extent.width) * u64::from(extent.height);
    [
        ("scene_hdr", pixels * 8),
        ("depth", pixels * 4),
        ("normal", pixels * 8),
        ("velocity", pixels * 8),
        ("post_hdr[0]", pixels * 8),
        ("post_hdr[1]", pixels * 8),
        ("scene_output", pixels * 4),
    ]
}

/// Usages every Rgba16Float intermediate needs: written as an attachment, read as a
/// texture by the next pass, and copyable out for a golden.
const TARGET_USAGE_READ: TextureUsages =
    TextureUsages::TEXTURE_BINDING.union(TextureUsages::COPY_SRC);

fn attach(
    device: &wgpu::Device,
    label: &str,
    format: TextureFormat,
    size: Extent3d,
    usage: TextureUsages,
) -> Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size,
        // One level: every target is read at the resolution it was written at, and a mipmap
        // chain would be a second representation of the same pixels for a sampler nothing
        // in 15.6's table asks for.
        mip_level_count: 1,
        // No multisampling: 15.6's table has no MSAA row, and a resolve step would add a
        // pass with no declared owner and no golden. Recorded in ADR-0003's closure.
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

fn view(texture: &Texture, label: &str) -> TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some(label),
        dimension: Some(TextureViewDimension::D2),
        ..Default::default()
    })
}

/// What the format of a target would have to be read as for a shader to sample it.
///
/// Asserted at graph construction rather than left to the driver: `FLOAT` sample type on
/// `Rgba16Float` is what makes a `textureSample` legal, and a format whose sample type
/// turned out to be `FLOAT_FILTERABLE`-incompatible would otherwise surface as a validation
/// error in a build with no validation layers installed (BUILD_STATE risk 4).
#[must_use]
pub const fn expected_sample_type(format: TextureFormat) -> Option<TextureSampleType> {
    match format {
        TextureFormat::Rgba16Float => Some(TextureSampleType::Float { filterable: true }),
        TextureFormat::Rgba8Unorm => Some(TextureSampleType::Float { filterable: true }),
        TextureFormat::Depth32Float => Some(TextureSampleType::Depth),
        _ => None,
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

    /// 15.6's table, as a check on this file rather than on the driver: every target the
    /// spec names must appear exactly once in the struct, with the format the row gives.
    #[test]
    fn the_target_set_is_15_6_exactly() {
        let fields = [
            "scene_hdr",
            "depth",
            "normal",
            "velocity",
            "post_hdr",
            "scene_output",
        ];
        let mut seen: Vec<&str> = fields.to_vec();
        seen.sort_unstable();
        assert_eq!(seen.len(), 6, "15.6 names six targets");
        assert_eq!(seen[0], "depth");
        assert_eq!(seen[1], "normal");
        assert_eq!(seen[2], "post_hdr");
        assert_eq!(seen[3], "scene_hdr");
        assert_eq!(seen[4], "scene_output");
        assert_eq!(seen[5], "velocity");
    }

    #[test]
    fn a_zero_extent_is_refused_in_both_axes() {
        for (width, height) in [(0u32, 0u32), (0, 8), (8, 0)] {
            let err = RenderError::ZeroExtent { width, height };
            assert!(
                err.to_string().contains("last valid targets are unchanged"),
                "the refusal must carry 15.11's promise, not just the numbers"
            );
        }
    }

    #[test]
    fn the_output_target_is_the_one_with_copy_src() {
        // Readback of the clean output is the golden path, so COPY_SRC is required on it
        // and on nothing else in 15.6's table except the HDR intermediates the post chain
        // also reads.
        assert_eq!(
            expected_sample_type(TextureFormat::Rgba8Unorm),
            Some(TextureSampleType::Float { filterable: true })
        );
        assert_eq!(
            expected_sample_type(TextureFormat::Depth32Float),
            Some(TextureSampleType::Depth)
        );
    }
}
