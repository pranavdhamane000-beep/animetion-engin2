# ADR-0011 — Render pass and post-effect order

Status: **accepted.** Written as WP06's deliverable on 2026-11-07. Decision 1's scope was
ruled by the operator in DECISIONS.md #40 §4 before any render code was written; the rest is
what that ruling obliged.

Date: 2026-11-07
Work package: WP06 (spec register: "ADR-0011 · Render pass and post-effect order · WP06")
Evidence: `crates/anim_render/src/graph.rs`, `crates/anim_render/src/targets.rs`,
`crates/anim_render/src/passes/`, `crates/anim_render/src/shaders/`,
`crates/anim_render/src/draw_list.rs`, `crates/anim_render/src/camera.rs`,
`crates/anim_render/tests/golden.rs`
Spec 15.3, 15.5, 15.6, 15.7, 15.8, 15.9, 15.11, 15.12. Continues ADR-0003 (formats),
ADR-0010 (alpha) and ADR-0002 (the adapter the graph is pinned to).

## Problem

1. **15.7 gives a post order and 15.5 asks for a chain, but three of the five stages have no
   kernel in this package.** Spec 29 (WP14) and 32 (WP17) own the effect parameters. Writing
   DOF, motion blur or bloom now invents parameters those packages will replace.

2. **A disabled pass must be distinguishable from a forgotten one.** 15.7: "A disabled pass
   declares no reads/writes and returns its input target." If a disabled stage simply
   vanished from the command log, 15.12's pass-order verification could not tell a
   deliberately skipped stage from a stage the graph never considered.

3. **15.6's permission list creates a limitation the post chain has to inherit.** Transparent
   objects "do not write depth, normals or velocity, and post effects document the resulting
   limitation". A motion-blur kernel reading the velocity target will not see transparent
   content, and that is a consequence of the spec rather than a defect to fix by writing
   auxiliaries the pass is told not to write.

4. **The coordinate convention is nowhere in spec 15, and everything downstream depends on
   it.** The velocity formula, the depth quantization and the y orientation of every golden
   all read it. Deciding it in a WGSL file is where a convention becomes invisible.

5. **15.7's sort is "descending quantized_view_depth", and the sign of "depth" is unstated.**
   Reading it as the authored z sorts front-to-back, which blends to a different image and is
   finite and plausible at every step.

## Decisions

1. **The chain is implemented; the effects are declared and disabled.** `PostStage::ORDER` is
   `[DepthAware, MotionBlur, Bloom, ToneMap, Grade]` and `POST_CHAIN_VERSION = 1`.
   `PostChainConfig::validate` refuses to enable any stage except `ToneMap`, with
   `RenderError::PostStageUnavailable` naming WP14/WP17. The three effect entry points exist
   in `post.wgsl` as the disabled-stage identity and are unreachable, because no pipeline is
   ever created against them.

   **Rejected:** omitting the entry points. 15.3 names `post.wgsl` as their home, and a later
   package needs the declaration to already exist. An identity is the only body that does not
   invent a parameter.

   **Rejected:** implementing a plausible bloom. It would look like coverage and would be
   replaced, and the replacement would move a golden with nothing in the project data to
   explain why.

2. **A disabled stage produces a `PassRecord` with empty reads and writes.** The execution
   loop walks `ORDER` and records every stage, enabled or not, so the log shows it was
   considered. `a_disabled_stage_declares_no_reads_and_no_writes` and
   `the_command_log_records_15_6_and_15_7_s_order` assert the emptiness rather than a comment
   claiming it.

3. **Exactly one tone map, and a chain without one is refused.** `PostChainConfig::tone_map`
   must be true; `validate` returns `GraphInvalid` otherwise, because a chain with no tone map
   would write scene-referred HDR into an 8-bit output and clamp it — the silent quality
   fallback 15.11 forbids. The operator is luminance-based Reinhard with Rec.709 weights,
   `TONE_MAP_VERSION = 1` in `post.wgsl`. Luminance-based preserves hue where per-channel
   Reinhard shifts it, which for an animation engine is the difference between a graded look
   and a colour cast.

4. **The full pass order is opaque → ordered 2D → transparent → post chain → output.**
   `the_command_log_records_15_6_and_15_7_s_order` asserts the nine-element sequence exactly,
   including the four disabled records. Only the opaque pass writes `normal` and `velocity`;
   the test asserts that too, so 15.6's permission list is enforced by an artifact rather than
   by five shader comments.

5. **The coordinate convention is authored units in canvas pixels, +x right, +y down, +z
   toward the viewer, orthographic over the extent.** `camera.rs` states it and
   `DEPTH_BUCKETS_PER_PIXEL = 256` is the "documented camera units" 15.7 asks for. The
   projection maps `[Z_NEAR, Z_FAR]` onto Vulkan's `[0, 1]` and negates y, so 15.7's
   `uv = ndc * vec2(0.5, -0.5) + 0.5` puts NDC +y on the top rows.

   There is no perspective divide anywhere in WP06, which is what makes 15.7's `previous
   w <= 0` test mean exactly one thing. WP15's camera rig (30.8) is where the projection stops
   being a function of extent alone; `EvaluatedCamera` carries no intrinsics today, only
   `{node, world_transform, is_on}`.

   **Rejected:** deciding the convention in a shader. It would be invisible to every CPU-side
   test, and the depth quantization and the velocity formula would each carry their own copy.

6. **View depth is distance along the camera's forward axis, larger meaning farther, so
   descending is back-to-front.** `draw_list` negates the view-space z, because the authored
   convention is +z toward the viewer. The sign is commented at the computation and tested by
   `the_comparator_is_15_7_s_tuple_in_15_7_s_order`.

7. **The total comparator is applied to the whole list, not only the transparent subset.**
   `(layer_order, descending quantized_view_depth, material_pass, entity_id, draw_index)`,
   exactly 15.7's tuple. A total order over everything costs one sort and makes the opaque
   pass's submission order reproducible too, which matters for a command log even where the
   depth test makes it invisible. `layer_order` is the node's position in WP04's canonical
   visit order; `material_pass` is `alpha::mode_id`, the only material fact an item carries
   before WP12.

8. **History is written in exactly one place, after `submit`.** `render_scene` calls
   `queue.submit` and only then `HistoryState::commit_submission` and
   `PreviousFrameState::record`. There is no other writer of either, so 15.7's "previous
   transforms update only after successful submission" is structural.

   The frame carries its **own** `history_valid`, computed by `extract_frame` from `detect`.
   Reading the stored verdict at draw time would be one frame stale: after the first
   submission the stored verdict is false (`SceneLoad`), so the second frame's velocity would
   be zeroed even though its history is contiguous. The GPU golden
   `a_moving_object_writes_velocity_only_when_history_is_valid` is what caught this.

9. **`SceneLoad` is detected from the absence of a previous tick, not from `!valid`.** The
   latter latches: the first commit reports `SceneLoad`, which sets `valid` false, which makes
   the second commit report `SceneLoad` again, forever. `previous_tick` is what the commit
   actually fills in.

10. **Editor UI is a separate operation that export cannot reach.** `OutputDestination` is
    `Readback | View`; `composite_editor_ui` refuses a `Readback`-backed scene with
    `GraphInvalid`. 15.9's "export ... cannot call `composite_editor_ui`" is therefore enforced
    by the type rather than by a discipline note. The UI frame carries its own pipeline and
    bind group, because 15.9's signature receives no `Device` and so cannot create them;
    `anim_editor` builds them through `create_ui_pipeline`.

## Consequences

1. **Transparent content gets no motion blur and no DOF.** It is in neither the velocity nor
   the depth target, by 15.6's own permission list. This is the limitation 15.6 asks to be
   documented, and it is recorded here rather than left for a WP14 kernel to discover.
2. The ping-pong does not alias: both `post_hdr` targets are allocated for the whole chain.
   15.7 permits aliasing when lifetimes do not overlap, and with one enabled stage a reuse
   analysis would have no consumer.
3. `scene_output` is `Rgba8Unorm` with the encode in `output.wgsl` (#40 §3), so
   `the_pass_log_shows_exactly_one_output_transfer_and_it_is_last` is a claim about a stage.
4. A resize bumps the generation and invalidates history; a zero extent is refused before
   anything is allocated, so 15.11's "without destroying the last valid targets" holds
   (`a_resize_bumps_the_generation_and_a_zero_extent_leaves_it_alone`).
5. The uniform layouts are declared once in `resources.rs` as `#[repr(C, align(16))]` and
   repeated in three shaders; `the_uniform_structs_agree_across_every_shader` compares the
   copies byte for byte, so drift is a test failure rather than a sheared golden.
6. **No numeric tolerance exists in this package.** The goldens are render-against-render
   byte equality, analytic where the arithmetic is exact, or structural. Cross-device
   tolerance is WP11's ADR-0016 and approval is Gate 1 after S2.

## Schema and version impact

| Number | Before | After | Why |
| --- | --- | --- | --- |
| `POST_CHAIN_VERSION` | — | 1 | new; the order is an algorithm identity, enablement is per-frame config |
| `TONE_MAP_VERSION` (WGSL) | — | 1 | new; a golden reproduces across a config change but not an operator change |
| `DEPTH_BUCKETS_PER_PIXEL` | — | 256.0 | new; 15.7's "documented camera units" |
| `Z_NEAR` / `Z_FAR` | — | −32000 / 32000 | new; an engine constant until WP15 authors a rig |
| `SNAPSHOT_DOMAIN` | v1 | v1 | **unchanged** — #41 rejected claims-in-snapshot precisely to keep WP04's identity |
| `RenderFrame` | 15.8's six fields | + `history_valid` | 15.7's velocity rule needs the current frame's verdict, not the previous one's |
| `DrawItem` | 15.8's nine fields | + `previous_valid`, `asset`, `opacity`, `pass` | #40 §7 plus the three the algorithms read; deviation row 20 |
| `RenderTargets` | 15.8's seven fields | + `scene_output` | 15.6's table names it and 15.8's struct omits it (#40 §7) |

## Outstanding, for a later package

1. **WP14/WP17** own the DOF, motion blur, bloom and grade kernels, and bump
   `POST_CHAIN_VERSION` if they change the order.
2. **WP15** owns camera intrinsics; `projection` stops being a function of extent alone.
3. **WP07** consumes `ResourceRegistry`'s size and ownership records. WP06 registers nothing
   with a module that does not exist.
4. **WP10** builds the editor UI that `EditorUiFrame` carries, and the startup assertion #18
   decision 3 asks for.
5. **Gate 1 after S2** approves the goldens. G5 is code-complete-plus-outstanding-approval,
   not a self-certified pass (DECISIONS.md #21).