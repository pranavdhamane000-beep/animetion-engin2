# ADR-0003 — Required formats, depth and HDR capabilities

Status: **closed.** WP00's half was measured on 2026-11-05; WP06's render-graph half closed on
2026-11-07 and is recorded below.
Date: 2026-11-05, closed 2026-11-07
Work package: WP00 / WP06 (spec register: "ADR-0003 · Required formats, depth and HDR capabilities · WP00/WP06")
Evidence: `docs/adr/evidence/wp00/wp00_gpu.json`; for the closure,
`crates/anim_render/src/device.rs`, `crates/anim_render/src/targets.rs`,
`crates/anim_render/tests/golden.rs`, DECISIONS.md #40 §3 and §6, ADR-0011

## Problem

Spec 9.3 assigns `docs/adr/` the duty of recording accepted platform capabilities,
and WP00 must establish which texture formats the reference adapter will actually
accept before WP06 designs passes around them. Choosing an intermediate format and
then discovering the attachment is not usable would mean redesigning the graph.

## What WP00 measured

| Capability | Result | How |
| --- | --- | --- |
| `Rgba16Float` as `RENDER_ATTACHMENT` | **supported** | `Adapter::get_texture_format_features(...).allowed_usages` queried on the *adapter*, so no device and no surface was needed |
| Compute dispatch and storage-buffer readback | **supported, verified** | WGSL compute, one workgroup, 8 `u32` outputs compared exactly |
| Readback without `MAPPABLE_PRIMARY_BUFFERS` | **supported** | storage buffer `STORAGE\|COPY_SRC` → copy → staging `COPY_DST\|MAP_READ`; the feature was never requested |
| `max_buffer_size` | 1099511627776 (1 TiB) | adapter limits, with `InstanceFlags` emptied |
| Backend | Vulkan only, by construction | `Backends::VULKAN`, and the probe aborts if `AdapterInfo.backend` is not Vulkan |
| Validation layers | **not installed on this machine** | so every probe result above is an *unvalidated* run |

## Decision

- `Rgba16Float` is accepted as the working-set intermediate format for WP06, on the
  strength of the attachment query above.
- The `COPY_DST | MAP_READ` staging pattern is the readback path of record. Not
  requesting `MAPPABLE_PRIMARY_BUFFERS` is deliberate: it is a feature whose absence
  on some adapters would otherwise be discovered during a long export rather than in
  a probe.
- The probe requests `Features::empty()` and only the compute limits it uses. A
  capability list gathered from a device created with extra features is not the
  capability list of the device WP06 will create.

## What WP06 closed (2026-11-07)

The open half was: depth-stencil format selection, sRGB variants, HDR output and the
tone-mapping path, the full `FORMAT` feature matrix per pass, and multisample support.
DECISIONS.md #40 §3 and §6 ruled the choices before any render code was written; this section
records them as implemented, not as new decisions.

**The requirement list, checked on the *adapter* before `request_device`** (`device.rs`):

| Format | Required usages | Where it is used |
| --- | --- | --- |
| `Rgba16Float` | `RENDER_ATTACHMENT`, `TEXTURE_BINDING`, `COPY_SRC`, `COPY_DST` | `scene_hdr`, `normal`, `velocity`, both `post_hdr`, and every uploaded image |
| `Depth32Float` | `RENDER_ATTACHMENT`, `TEXTURE_BINDING` | `depth`, clear 1.0, compare `Less` (15.6) |
| `Rgba8Unorm` | `RENDER_ATTACHMENT`, `TEXTURE_BINDING`, `COPY_SRC` | `scene_output` |

A missing usage is `RenderError::RequiredCapabilityMissing` at initialization, not a
mid-export discovery — 15.11's first sentence. `sample_count` is 1 and **no multisampling**:
15.6's table names no MSAA target and a resolve pass would have no declared owner and no
golden.

**`required_features` stays `Features::empty()`.** This ADR's own decision says a capability
list gathered from a device opened with extra features is not the list WP06's device has, and
every usage above is core. The consequence is visible in the graph: a binding array of
per-item textures would need `BINDING_ARRAY`, so each item carries its own bind group and its
own `ItemUniform` buffer instead (ADR-0011, `resources.rs`).

**sRGB variants: the output is `Rgba16Float`'s opposite choice.** `scene_output` is
`Rgba8Unorm`, **not** `Rgba8UnormSrgb`, with the linear→sRGB encode performed in
`output.wgsl` (#40 §3). An `Srgb` format would put one transfer in fixed-function state where
the golden cannot attribute it, and a second transfer anywhere in the chain would then be
indistinguishable in the output. With a linear-storage format the transfer is a named stage in
the pass log, so 15.12's "exactly one output transfer" is a claim about a stage.
`the_pass_log_shows_exactly_one_output_transfer_and_it_is_last` checks it.

**HDR output and the tone-mapping path.** Scene color is linear scene-referred `Rgba16Float`
end to end until the output pass. One luminance-based Reinhard tone map runs in `post.wgsl`
(`TONE_MAP_VERSION = 1`); DOF, motion blur, bloom and grade are declared and disabled, and
enabling one is refused with its owning package named. ADR-0011 carries the order.

**Readback** is the `COPY_DST | MAP_READ` staging pattern this ADR chose, now with the padded
row math in `readback.rs`: `bytes_per_row` rounds up to `COPY_BYTES_PER_ROW_ALIGNMENT` and the
padding is stripped before the bytes leave the crate. `MAPPABLE_PRIMARY_BUFFERS` is still not
requested.

**Consequence for G5.** The golden-frame gate in stage S2 depended on this ADR being finished
rather than merely opened; it is now finished. That does **not** certify G5: the images still
need the operator's approval at Gate 1 after S2, and DECISIONS.md #21's self-certification
ruling stops at WP05.

**Still honest about the limitation below.** The closure is a set of format and usage choices
plus a passing suite on the pinned adapter. It is not a conformance proof, because the
validation layers are still not installed.

## Honest limitation

Because no Vulkan validation layer is installed, nothing here proves the API was
*used correctly* — only that it was *accepted* by the driver. A misuse that NVIDIA
tolerates and the Khronos validator would reject is not excluded by these results.
Installing the SDK would have closed that gap; the probes were run anyway, and that
is the residual rather than a claim of full conformance.

## Schema/version impact

None. Format choices do not reach persisted project data.
