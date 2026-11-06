# `crates/anim_render/wip/` — unvalidated WP06 sketches, not a reference implementation

Cargo does not compile this directory: it is not `src/`, not a target path, and
`crates/anim_render/src/lib.rs` declares no modules from it. It exists so a session that wrote
this code under a context limit did not delete it, and so the next session can keep or ignore it
on purpose rather than inheriting it by accident.

**Contract, not code.** The rulings these files were written against are **DECISIONS.md #40** —
the PCI anchor source, the asset-metadata schema bump, `scene_output = Rgba8Unorm` with the
transfer in the shader, the post chain stopping at one tone map, the `DedicatedVideoMemory` floor
via DXGI, `Features::empty()`. Spec **15.5–15.9** is the shape of the graph. Where this directory
and those two disagree, they win and these files are wrong.

## Status per file

| File | State |
| --- | --- |
| `device.rs` | Most complete. Adapter selection, #18 revalidation, the PCI hard-fail, `Features::empty()`, the required-format list, the DXGI floor, device-loss via `OnceLock`. **Never executed** — no `GpuContext::create` call has run on this machine. Unit tests written for the pure logic only. |
| `targets.rs` | 15.6's six targets, formats, usages, `Rgba8Unorm` output, atomic reallocation. Never executed. |
| `camera.rs` | The coordinate convention, `Invalidation`, `HistoryState`. Its tests were never run. |
| `alpha.rs` | 15.7's blend descriptors, the premultiply partition, the pass-routing rule. Its tests were never run. |
| `graph.rs` | **Invented API.** References pass-module functions with signatures no module declares, a `resources::textures::instance_bindings()` that does not exist, a `_queue_marker` field never declared, and a no-op `.map(|record| record)`. |
| `draw_list.rs` | **Invented API and one dead branch.** `DrawItem::opacity()` reads a field `opacity_override` that was never added (does not compile); `claims_for()` always returns `None`, which would make **every extraction fail** with `UnclassifiedAssetMetadata`; `extract_frame` builds a `HistoryState` from `PreviousFrameState` in a way that contradicts `camera.rs`'s own `detect`; the extent is hard-coded `1×1`. |
| `passes/`, `resources.rs`, `readback.rs` | One-line stubs. No content. |
| `lib.rs` | `RenderError` as drafted, including the two additions #40 §7 records and the `UnsupportedAlphaMode` variant that #40 §7 decides **not** to keep. |
| `Cargo.toml.notes` | 15.4's dependency list as a manifest, with the reasons. Not applied — an unimplemented crate should not claim edges it does not use. |

## What was never started

`resources.rs`, `readback.rs`, the five pass modules, all five `.wgsl` files, `tests/golden.rs`,
ADR-0010, ADR-0011. Spec 15.3 lists eighteen files; three and a half existed.

## How to un-park, and what to expect

`git mv` the six files back into `src/`, restore the `pub mod` lines from `wip/lib.rs`, and take
the dependency block from `wip/Cargo.toml.notes` rather than re-deriving it — that file is spec
15.4 plus the two additions #40 records (`pollster` for wgpu 30's `impl Future` entry points, and
`anim_project` for `EventKind`, which 15.4 omits for the same reason deviation row 17(a) records
for `anim_sim`).

Two expectations worth setting before the first build, because neither is guessable from the files:

- **The error count is unknown, and it is not small.** The only `cargo check` ever run against
  these files died inside `anim_assets` — the schema work was mid-edit — so `anim_render`'s own
  errors were only partly surfaced. What came back before that was ~9 errors across `targets.rs`
  and `lib.rs` and it was never re-run to zero afterwards. `graph.rs` alone calls five pass-module
  functions that no module declares. Budget for a real compile-fix cycle, not for deleting a `mod`
  line and continuing.
- **`cargo fmt` never touched these files.** They were outside cargo's view the whole time, so
  their whitespace is whatever the editor produced. Run `cargo fmt` immediately after un-parking,
  as its own step, so the formatting diff does not land inside the review diff of the real changes.

## The one design question these files did not answer

`extract_frame`'s 15.9 signature receives a `SceneSnapshot` and a `SimulationFrame`, but the alpha
mode and color space a `DrawItem` needs are `ImageAssetClaims` recorded on the **manifest**, which
lives in the document the snapshot does not carry (WP04's 13.8 `EvaluatedNode` has an `AssetId` and
nothing else). `draw_list.rs` fakes its way out with a stub that always fails. The real options are
to widen 15.9's parameters, to pass a resolver, or to add the claims to the snapshot — the last
changes `SNAPSHOT_DOMAIN`'s pre-image and so every snapshot hash, which is WP04's identity, not
WP06's to spend. Decide it before writing the passes, and record it in DECISIONS.md.
