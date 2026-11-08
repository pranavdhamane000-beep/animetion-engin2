# ADR-0010 — Alpha import/runtime/output conventions

Status: **accepted.** Written as WP06's deliverable on 2026-11-07. The import half was
already ruled by DECISIONS.md #40 §2 and is recorded here rather than re-decided; the
runtime and output halves are this package's.

Date: 2026-11-07
Work package: WP06 (spec register: "ADR-0010 · Alpha import/runtime/output conventions · WP06")
Evidence: `crates/anim_assets/src/image.rs`, `crates/anim_render/src/alpha.rs`,
`crates/anim_render/src/draw_list.rs`, `crates/anim_render/src/resources.rs`,
`crates/anim_render/src/shaders/output.wgsl`, `crates/anim_render/tests/golden.rs`
Spec 15.2, 15.6, 15.7, 15.8, 15.11, 15.12. Continues ADR-0005 (canonical encoding),
ADR-0003 (the output format this convention writes into) and DECISIONS.md #40 §2, #41.

## Problem

1. **Spec 15.7's alpha paragraph spans three stages and names no owner for any of them.**
   "Authoring straight-alpha color is decoded to linear and premultiplied as
   `rgb_premul = rgb_linear * alpha`. Assets already marked premultiplied are not multiplied
   again." Import, decode and draw each touch that sentence, and a convention split across
   three crates with no record is a convention that drifts.

2. **`ImageAlpha` has three variants and `AlphaMode` has five.** 15.8's enum is verbatim and
   closed; the manifest's claim enum is `Opaque | Straight | Unclassified`. Nothing in the
   authored data carries an alpha-test cutoff or a premultiplication intent, so three of the
   five modes are not authorable in WP06. The mapping has to be written down or a later
   package will invent a second one.

3. **15.7 requires zero-alpha RGB to be "explicitly defined" and does not define it.** At
   alpha zero a straight-alpha pixel's RGB is meaningless, so converting premultiplied output
   to straight divides by zero unless a value is chosen.

4. **Premultiplication must happen in linear light, and the order is not stylistic.**
   Multiplying gamma-encoded values and then decoding attenuates the wrong quantity: a 50 %
   layer comes out near 21 % instead of 50 %. Which stage does the multiply decides whether
   that defect exists.

## Decisions

1. **The transfer is applied once, at decode, from the manifest's recorded claim.**
   `anim_assets::ImageDecoder::decode_pixels` reads `record.image.color_space` and applies
   the sRGB EOTF for `Srgb`, passes through for `Linear`, and refuses `Unclassified`. The GPU
   texture is `Rgba16Float` carrying **linear** pixels, and no shader between decode and
   output touches the transfer function.

   The reason is #40 §2's: a renderer that decided the transfer itself could change its mind
   on a machine, driver or toolchain move, and a golden would stop reproducing with nothing
   in the project data to explain it. Putting it at decode also makes it a CPU-testable
   golden — `srgb_eotf_matches_the_specification_at_known_points` needs no device.

   **Rejected:** sampling an `Rgba8UnormSrgb` texture and letting the hardware decode. It is
   the shorter path, but it puts a transfer in fixed-function state where the pass log cannot
   name it, which is the same objection #40 §3 raised against an `Srgb` *output* format.

2. **Premultiplication is the decode configuration's job, in linear space, after the
   transfer.** `DecoderConfiguration::Rgba8Premultiplied` produces `rgb * a` on the linear
   values; `Rgba8Unpremultiplied` leaves alpha straight. The variant names contain "Rgba8",
   which describes the **8-bit source**, not the `f32` output; they are not renamed, because
   their `as_str()` spellings feed `derived_key` and renaming would move cache keys for no
   reason.

   **Rejected:** premultiplying in the shader for every mode. It would make
   `AlphaMode::Premultiplied` indistinguishable from `Straight`, and 15.7's "not multiplied
   again" would have nothing to refer to.

3. **`AlphaMode` maps from the claim through one total function, and is a closed enum with
   no unsupported variant.** `alpha::mode_from_claims` maps `Opaque → Opaque` and
   `Straight → Straight`. `AlphaTest`, `Premultiplied` and `AdditivePremultiplied` are
   reachable — `blend_descriptor` is total over all five and `RenderFrame`'s fields are
   public, so the goldens exercise them — but they become *authorable* with WP12's material
   contract, and `mode_from_claims` is the single place that extension happens.

   `RenderError::UnsupportedAlphaMode` is **not declared**, on #40 §7's ruling: all five
   variants have descriptors, so no condition can produce the refusal, and a
   declared-but-unreachable variant reads as covered error handling in a later audit. The
   reachable half of 15.7's "supported alpha modes" sentence is `InvalidAlphaCutoff`, which
   has a producer and a test.

4. **The shader multiply is computed on the CPU, not branched in five shaders.**
   `alpha::premultiply_for(mode, texture_premultiplied)` is
   `mode.premultiply_in_shader() && !texture_premultiplied`, and its result travels in
   `ItemUniform::params.y`. `graph.rs` additionally refuses a pairing where
   `alpha::expects_premultiplied(mode)` disagrees with how the texture was actually decoded,
   because a `Premultiplied` item sampled from a straight texture blends unattenuated RGB —
   brighter than authored, and finite and plausible at every step.

5. **Zero-alpha RGB is zero.** `alpha::ZERO_ALPHA_RGB = [0, 0, 0]`, and `output.wgsl`'s
   `to_straight` returns it explicitly at `alpha <= 0`. Zero is the only choice that makes
   `premul → straight → premul` a fixed point: `premul = straight * alpha` is zero at alpha
   zero for *any* straight colour, so dividing back out needs a rule, and a non-zero rule
   would mean a round trip through a straight-alpha codec changes the image.

6. **The blend descriptors are 15.7's, verbatim, and are data rather than pipeline literals.**
   Standard premultiplied-over is `One`/`OneMinusSrcAlpha` for **both** colour and alpha;
   additive is `One`/`One` for both. `alpha::blend_descriptor` is the only producer, so a
   pipeline cannot acquire a slightly different blend by being typed in beside it.

7. **The final alpha conversion is two entry points, not a uniform branch.**
   `output.wgsl` has `fs_premultiplied` and `fs_straight`, and `OutputPass` holds one pipeline
   each. Which ran is therefore visible in the pipeline and in the pass log, which is what
   makes 15.12's "exactly one output transfer" a claim about a stage.

## Consequences

1. A `Straight` and a `Premultiplied` asset holding the same pixels produce **byte-identical**
   images. `straight_and_premultiplied_produce_the_same_image` in `tests/golden.rs` compares
   two readbacks, so it needs no expected colour and no tolerance.
2. A zero-alpha standard-blend item over black leaves the frame **exactly** black
   (`a_zero_alpha_straight_item_contributes_no_color`), and converting it to straight alpha
   yields the defined zero (`a_zero_alpha_output_converts_to_the_defined_zero_rgb`).
3. At full coverage the two final alpha modes agree byte for byte, because dividing by 1.0 is
   exact (`the_two_final_alpha_modes_agree_at_full_coverage`).
4. An alpha-test discard happens before any attachment write, so a discarded fragment leaves
   no depth hole shaped like its quad (`an_alpha_test_above_the_coverage_discards_every_fragment`).
5. `Unclassified` has exactly two producers — an embedded ICC profile and `V3_TO_V4` — and one
   consumer rule: extraction refuses it with `UnclassifiedAssetMetadata` naming the asset and
   the remedy. WP06 adds no third producer.
6. **WP12 replaces `mode_from_claims`, not the passes it feeds.** The three unauthorable modes
   already have descriptors, pipelines and goldens; what is missing is authored data.

## Schema and version impact

| Number | Before | After | Why |
| --- | --- | --- | --- |
| `MANIFEST_SCHEMA_VERSION` | 2 | 2 | unchanged — #40 §2's bump already landed as WP06's prerequisite |
| `PROJECT_SCHEMA_VERSION` | 4 | 4 | unchanged, same reason |
| `ImageAlpha` | 3 variants | 3 variants | unchanged; `Unclassified` stays migration-only |
| `AlphaMode` | not built | 5 variants, verbatim 15.8 | new, and closed |
| `DecoderConfiguration` | 3 variants | 3 variants | unchanged; the `Rgba8` names now documented as describing the source width |
| `TONE_MAP_VERSION` (WGSL) | — | 1 | new, in `post.wgsl`; see ADR-0011 |
| `POST_CHAIN_VERSION` | — | 1 | new; see ADR-0011 |

No persisted encoding changes. Every addition is runtime-only, so no migration step and no
new fixture are owed.

## Outstanding, for a later package

1. **WP12** owns the material contract that makes `AlphaTest`, `Premultiplied` and
   `AdditivePremultiplied` authorable, and extends `mode_from_claims`.
2. **WP14/WP17** own the effect kernels that read the alpha channel reactively; the velocity
   target's `w` is validity alone until then.
3. **WP09** resolves `FinalAlpha` from the selected codec. WP06 takes it as an explicit field
   of `RenderQuality` and never infers it.
4. **Gate 1 after S2** approves the golden frames. This ADR records the conventions; it does
   not certify the images, and G5 is not self-certifiable (DECISIONS.md #21).