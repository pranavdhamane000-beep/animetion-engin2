//! Low-level padded-row GPU readback (spec 15.3).
//!
//! # Why the padding is not optional
//!
//! `copy_texture_to_buffer` requires `bytes_per_row` to be a multiple of
//! [`wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`] (256). A 1920-wide `Rgba8Unorm` target is 7680
//! bytes per row, which is already aligned; a 640-wide one is 2560, also aligned; a
//! **100-wide** one is 400, which is not. The copy then either fails validation or — in a
//! build with no validation layers installed, which is this machine (BUILD_STATE risk 4) —
//! is accepted and reads back rows at the wrong stride, producing an image that is sheared
//! rather than wrong-sized. Shear is the harder defect to see in a golden, so the row math
//! is a pure function here with its own tests instead of an expression inside the copy.
//!
//! The buffer is `COPY_DST | MAP_READ` and nothing else. Adding `STORAGE` would require
//! `Features::MAPPABLE_PRIMARY_BUFFERS`, which ADR-0003 deliberately does not request: its
//! absence on some adapter should be discovered in a probe, not during a long export. This
//! is the same narrower contract `wp00_gpu` proved readback on.
//!
//! # No `Mutex`
//!
//! `map_async` is callback-based in wgpu 30, so the result has to cross from the callback
//! into this thread. `clippy.toml` bans `Mutex` because no lock order may influence
//! canonical output, and DECISIONS.md #40 §6 names `OnceLock` as the replacement. A map
//! completes exactly once, which is precisely `OnceLock`'s contract.

use std::sync::{Arc, OnceLock};

use wgpu::{Device, Extent3d, Queue, Texture, TextureFormat};

use crate::{RenderError, Result};

/// The clean output, in CPU bytes, tightly packed and ready to compare or encode.
///
/// Spec 15.9: "Export requests a readback-compatible `OutputDestination`". This is that
/// form. `pixels` has no row padding — the padding existed only in the staging buffer and
/// is stripped here, so a consumer never has to know the alignment rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputReadback {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The format the bytes were copied from, so a consumer knows their meaning.
    pub format: TextureFormat,
    /// `width * height * bytes_per_texel`, row-major, top row first, no padding.
    pub pixels: Vec<u8>,
}

impl OutputReadback {
    /// Bytes per pixel for the formats this crate reads back.
    ///
    /// A total function over the formats the graph allocates rather than a `_ => 4` guess:
    /// reading an `Rgba16Float` target with an `Rgba8Unorm` stride would produce a buffer
    /// half the size it should be and shear every row after the first.
    #[must_use]
    pub const fn bytes_per_texel(format: TextureFormat) -> Option<u32> {
        match format {
            TextureFormat::Rgba8Unorm | TextureFormat::Rgba8UnormSrgb => Some(4),
            TextureFormat::Rgba16Float => Some(8),
            _ => None,
        }
    }
}

/// Round `bytes_per_row` up to the copy alignment.
///
/// Pure and public so the alignment rule is testable without a device, and so `graph.rs`
/// and this file cannot disagree about it.
#[must_use]
pub const fn padded_bytes_per_row(width: u32, bytes_per_texel: u32) -> u32 {
    let unpadded = width * bytes_per_texel;
    let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    // `unpadded + alignment - 1` cannot overflow for any extent the adapter would accept:
    // `max_texture_dimension_2d` is far below `u32::MAX / 8`, and `device.rs` checks the
    // limit before a target is allocated.
    unpadded.div_ceil(alignment) * alignment
}

/// The staging buffer size for one texture copy.
#[must_use]
pub const fn staging_size(extent: Extent3d, bytes_per_texel: u32) -> u64 {
    padded_bytes_per_row(extent.width, bytes_per_texel) as u64 * extent.height as u64
}

/// Copy a texture out to tightly packed CPU bytes.
///
/// Submits its own command buffer and waits for it, because a readback that returned before
/// the copy completed would compare a golden against uninitialized memory. The wait is
/// `PollType::Wait` on this submission's index, not a sleep loop.
///
/// # Errors
///
/// [`RenderError::ReadbackFailed`] when the format has no known texel width, the copy is
/// refused, the map never completes, or the mapped range cannot be read. Device loss is
/// *not* inferred here — `device.rs`'s callback is the authority on that, and the caller
/// checks it, because on this machine a stalled map and a power-gated GPU look identical
/// from `poll` (the reason `wp00_gpu` keeps the two apart).
pub fn read_texture(
    device: &Device,
    queue: &Queue,
    texture: &Texture,
    extent: Extent3d,
    format: TextureFormat,
) -> Result<OutputReadback> {
    let purpose = format!("readback {extent:?} {format:?}");
    let bytes_per_texel =
        OutputReadback::bytes_per_texel(format).ok_or_else(|| RenderError::ReadbackFailed {
            purpose: purpose.clone(),
            detail: format!("no declared texel width for {format:?}"),
        })?;
    if extent.width == 0 || extent.height == 0 {
        return Err(RenderError::ZeroExtent {
            width: extent.width,
            height: extent.height,
        });
    }
    let bytes_per_row = padded_bytes_per_row(extent.width, bytes_per_texel);
    let size = staging_size(extent, bytes_per_texel);
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("anim_readback/staging"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("anim_readback/encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(extent.height),
            },
        },
        extent,
    );
    let index = queue.submit([encoder.finish()]);

    let slot = Arc::new(OnceLock::new());
    {
        let slot = Arc::clone(&slot);
        staging.map_async(
            wgpu::MapMode::Read,
            0..size,
            Box::new(
                move |result: std::result::Result<(), wgpu::BufferAsyncError>| {
                    // A second completion would be a wgpu bug; `set` returning Err is ignored
                    // rather than unwrapped, because the first result is the one that matters.
                    let _ = slot.set(result.map_err(|error| format!("{error:?}")));
                },
            ),
        );
    }
    let index_label = format!("{index:?}");
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(index),
            timeout: None,
        })
        .map_err(|error| RenderError::ReadbackFailed {
            purpose: purpose.clone(),
            detail: format!("poll did not reach submission {index_label}: {error:?}"),
        })?;
    let outcome = slot.get().ok_or_else(|| RenderError::ReadbackFailed {
        purpose: purpose.clone(),
        detail: "map_async never invoked its callback".to_owned(),
    })?;
    if let Err(detail) = outcome {
        return Err(RenderError::ReadbackFailed {
            purpose,
            detail: format!("map_async refused: {detail}"),
        });
    }
    let mapped =
        staging
            .get_mapped_range(0..size)
            .map_err(|error| RenderError::ReadbackFailed {
                purpose: purpose.clone(),
                detail: format!("get_mapped_range: {error}"),
            })?;

    let unpadded = (extent.width * bytes_per_texel) as usize;
    let mut pixels = Vec::with_capacity(unpadded * extent.height as usize);
    for row in 0..extent.height as usize {
        let start = row * bytes_per_row as usize;
        pixels.extend_from_slice(&mapped[start..start + unpadded]);
    }
    drop(mapped);
    staging.unmap();
    Ok(OutputReadback {
        width: extent.width,
        height: extent.height,
        format,
        pixels,
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

    /// The case that makes the padding load-bearing: 100 pixels of RGBA8 is 400 bytes,
    /// which is not a multiple of 256, and the copy would shear without the round-up.
    #[test]
    fn an_unaligned_row_is_rounded_up_to_the_copy_alignment() {
        assert_eq!(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT, 256);
        assert_eq!(padded_bytes_per_row(100, 4), 512);
        assert_eq!(padded_bytes_per_row(1, 4), 256);
        assert_eq!(padded_bytes_per_row(64, 4), 256);
        // Already aligned rows must not grow: a round-up that added a row's worth of
        // padding to an aligned image would waste half the staging buffer at 64 pixels.
        assert_eq!(padded_bytes_per_row(65, 4), 512);
        assert_eq!(padded_bytes_per_row(1920, 4), 7680);
        assert_eq!(7680 % 256, 0);
    }

    #[test]
    fn every_padded_row_is_a_multiple_of_the_alignment() {
        for width in [1u32, 2, 7, 63, 64, 65, 100, 255, 256, 640, 1920] {
            for texel in [4u32, 8] {
                let padded = padded_bytes_per_row(width, texel);
                assert_eq!(padded % 256, 0, "{width}x{texel} produced {padded}");
                assert!(padded >= width * texel, "padding may never shrink a row");
                assert!(
                    padded - width * texel < 256,
                    "padding must be less than one alignment unit"
                );
            }
        }
    }

    #[test]
    fn the_staging_size_accounts_for_every_row_not_just_the_pixels() {
        let extent = Extent3d {
            width: 100,
            height: 3,
            depth_or_array_layers: 1,
        };
        // 3 rows of 512, not 3 rows of 400: the difference is the padding the copy writes.
        assert_eq!(staging_size(extent, 4), 1536);
        assert_ne!(staging_size(extent, 4), 1200);
        let aligned = Extent3d {
            width: 64,
            height: 2,
            depth_or_array_layers: 1,
        };
        assert_eq!(staging_size(aligned, 4), 512);
    }

    #[test]
    fn the_two_readback_formats_have_the_widths_the_targets_allocate() {
        assert_eq!(
            OutputReadback::bytes_per_texel(TextureFormat::Rgba8Unorm),
            Some(4)
        );
        assert_eq!(
            OutputReadback::bytes_per_texel(TextureFormat::Rgba16Float),
            Some(8)
        );
        // A depth target is not a colour readback, and guessing 4 for it would silently
        // produce a quarter of the bytes.
        assert_eq!(
            OutputReadback::bytes_per_texel(TextureFormat::Depth32Float),
            None
        );
    }

    /// Stripping the padding is the other half of the rule, and it is checkable without a
    /// GPU by building the padded buffer a copy would have produced.
    #[test]
    fn stripping_padding_restores_the_tightly_packed_image() {
        let width = 100usize;
        let height = 3usize;
        let texel = 4usize;
        let padded = padded_bytes_per_row(width as u32, texel as u32) as usize;
        let mut buffer = vec![0xFFu8; padded * height];
        for row in 0..height {
            for byte in 0..width * texel {
                buffer[row * padded + byte] = ((row * width * texel + byte) % 255) as u8;
            }
        }
        let mut packed = Vec::new();
        for row in 0..height {
            let start = row * padded;
            packed.extend_from_slice(&buffer[start..start + width * texel]);
        }
        assert_eq!(packed.len(), width * height * texel);
        assert!(
            !packed.contains(&0xFF),
            "no padding byte may survive into the output"
        );
        assert_eq!(packed[0], 0);
        assert_eq!(packed[width * texel], ((width * texel) % 255) as u8);
    }
}
