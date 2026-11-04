# ADR-0003 — Required formats, depth and HDR capabilities

Status: **partially closed.** WP00's half is measured; the render-graph half closes in WP06.
Date: 2026-11-05
Work package: WP00 / WP06 (spec register: "ADR-0003 · Required formats, depth and HDR capabilities · WP00/WP06")
Evidence: `docs/adr/evidence/wp00/wp00_gpu.json`

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

## What is NOT decided here, and belongs to WP06

Depth-stencil format selection, sRGB variants, HDR output and the tone-mapping path,
the full `FORMAT` feature matrix per pass, and multisample support. WP00 has no
render pass to attach them to, and recording an opinion about them now would be a
guess presented as evidence.

**Consequence for G5.** The golden-frame gate in stage S2 depends on this ADR being
finished, not merely opened.

## Honest limitation

Because no Vulkan validation layer is installed, nothing here proves the API was
*used correctly* — only that it was *accepted* by the driver. A misuse that NVIDIA
tolerates and the Khronos validator would reject is not excluded by these results.
Installing the SDK would have closed that gap; the probes were run anyway, and that
is the residual rather than a claim of full conformance.

## Schema/version impact

None. Format choices do not reach persisted project data.
