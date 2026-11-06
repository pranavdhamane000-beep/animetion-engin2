# BUILD_STATE

Resume point for this project. A new conversation must be able to finish a work
package from this file plus the spec and the code, with no access to prior chats.

Update rules: append to Completed Work Packages at the end of every package.
Edit Current Work Package while one is in flight. Record every non-obvious
decision in DECISIONS.md the moment it is made, not at the end.

Status vocabulary: `not started` / `in progress` / `code complete` /
`validated` (its validation section in the spec passes end to end).

---

## Project

2D/2.5D animation engine. Rust workspace, Windows 11 only, deterministic
offline export as the primary output path.

## Environment facts

Measured on this machine 2026-11-04. GPU, Vulkan and pen facts come from a
read-only hardware probe; the first C++ assessment was **wrong** and is corrected
below (see DECISIONS.md #6 for how that happened).

| Requirement (WP00 prerequisites, spec 9.2) | State |
| --- | --- |
| Windows 11 | OK — build 26300.9818, ASUS TUF Gaming F16 FX607VBR, 16 GB RAM |
| C++ compiler and linker | **OK — already installed.** VS Build Tools 2026 at `C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools`, MSVC 14.50.35717, `cl.exe` + `link.exe` + `vcvars64.bat` present. **No installation needed.** |
| Windows SDK | OK — Kits\10, Lib/Include 10.0.26100.0, UCRT `libucrt.lib` and `um` headers present |
| Git | OK — 2.54.0 |
| FFmpeg | **Pinned.** `ffmpeg.exe` + `ffprobe.exe` 9.0.2 gyan essentials vendored into `tools/ffmpeg/`, SHA-256 recorded in `tools/ffmpeg/ffmpeg-manifest.json`, both verified to execute from there. Binaries are git-ignored operator inputs; the manifest is versioned. System copy on PATH remains untrusted and unused (spec 9.11) |
| Rust (rustc, cargo, rustup) | **OK — installed 2026-11-04.** rustc/cargo 1.99.1, rustfmt 1.10.0-stable, clippy 0.1.99, host `x86_64-pc-windows-msvc`, `RUSTUP_HOME=D:\rustup`, `CARGO_HOME=D:\cargo`. Proven by building and running a real exe |
| Vulkan-capable NVIDIA driver | OK — loader 1.4.341.0; `vulkaninfo` exit 0 enumerating both adapters. NVIDIA driver 617.14, Vulkan 1.4.351 |
| RTX 3050A reference machine | OK — **RTX 3050 A Laptop GPU confirmed**, PCI bus 01:00.0, Vulkan vendor `0x10DE` device `0x28A3`. Plus Intel iGPU `VEN_8086 DEV_A7AB` |

**Do not key any check on a LUID value.** The 2026-11-04 probe recorded NVIDIA
`00cb5c07-00000000`; the 2026-11-05 WP00 adapter probe measured `0000000000011af8`
on the same machine and the same driver. LUIDs change between enumerations, so spec
9.12's "no passing on a different adapter/driver without revalidation" is keyed on
adapter name, vendor/device id and driver version. See ADR-0002 consequence 2.

**The driver has two spellings.** Vulkan's `driver_version` reads `617.56.0`; wgpu's
`driver_info` and the Windows driver read `617.14`. Both are in the evidence files.
Comparing only one produces a false mismatch.

### Build hazards found by the probe — both must be respected

1. **PATH linker shadowing — real, but narrower than first claimed.** `link.exe`
   resolves first to `D:\moneyprinter v2\Git\usr\bin\link.exe`, the MSYS *hard-link
   file utility*, not a linker. **Measured 2026-11-04: `cargo build` from plain Git
   Bash succeeded anyway and produced a runnable exe**, because rustc locates the
   MSVC toolchain through registry detection rather than PATH. So the hazard does
   not block ordinary cargo builds; it still bites anything that resolves a linker
   off PATH itself — `cc`-crate build scripts, hand-written PowerShell, and
   `Get-Command link`. A `build.cmd` launcher that calls `vcvars64.bat` first
   removes the ambiguity for those cases. Do not restate this as "builds fail
   without vcvars" — that was tested and is false.
2. **The dGPU power-gates off.** On hybrid Optimus the panel is driven by Intel
   and NVIDIA reported `CM_PROB_PHANTOM` with `nvidia-smi` failing on the first
   attempt, then reappeared mid-session. **A one-shot WP00 GPU probe can falsely
   conclude there is no discrete GPU.** Adapter enumeration needs retry/wait and
   must report the phantom state explicitly rather than falling back to the iGPU
   (spec 9.7 forbids substituting the primary desktop adapter).

Also: `deviceLUIDValid = true` on both adapters, so the spec's LUID correlation is
satisfiable here — proven in ADR-0002. No Vulkan validation layers installed (only
Steam/EOS overlays), so every probe result in `docs/adr/evidence/wp00/` is an
**unvalidated** run: it proves the driver accepted the calls, not that the calls were
conformant. ADR-0003 states this as a residual rather than a clean pass. No
`VULKAN_SDK` — wgpu and ash need only the runtime loader, and `ash`'s `linked`
feature must stay off because it would require `vulkan-1.lib` from that absent SDK.

**Shell note for every future session:** `RUSTUP_HOME`/`CARGO_HOME` were persisted
with `setx`, so a shell started before that — including this app's Git Bash — has no
`cargo` on PATH. Prepend it: `export PATH="/d/cargo/bin:$PATH"`. Not a broken
install, a stale environment block.

Display: single built-in panel 1920x1200 @144 Hz, DPI 120 — relevant to WP10's
preview surface and to any golden frame sized against the window.

### Pen input: removed from scope

No pen or touch digitizer exists on this machine (PnP enumeration found only an
ASUS Precision Touchpad and a USB mouse). That originally blocked the WP00 pen
probe, and on 2026-11-04 the response was to **remove pen input from the project
entirely** rather than work around the hardware — spec analysis showed ink strokes
have no consumer anywhere in the pipeline. See DECISIONS.md #8 and Deviations below.

**Consequence: G0 is now fully achievable on this machine.** Pen was the single
prerequisite it could not meet.

Also worth knowing before writing GPU probes: this is a hybrid-graphics Optimus
laptop, so the discrete GPU may be power-gated at probe time (see hazard 2 above).

## Stage plan

Five stages, agreed 2026-11-04. Spec sections 33, 38 and 45 hold the gate
definitions.

| Stage | Packages | Status | Exit evidence |
| --- | --- | --- | --- |
| S1 Deterministic core | WP00–WP05 | in progress — **G0 passed 2026-11-05, G1 passed 2026-11-05, G2 passed 2026-11-06 (WP02+WP03 joint), G3 passed 2026-11-06**, WP02, WP03 and WP04 validated | All platform probes pass, toolchain frozen, identical bytes from any seek point |
| S2 Output path | WP06–WP09 | not started | First deterministic MP4 with synced audio |
| S3 Tool + hardening | WP10–WP11 | not started | Usable editor, benchmarks meeting locked thresholds |
| S4 Character and cinema | WP12–WP17 | not started | Gates G10–G15 |
| S5 Level 3 production | WP18–WP22 | not started | Gates G16 onward, Level 3 acceptance |

## Current Work Package

**WP05 — Deterministic Simulation, Random Streams, and Seeking** (spec 14). Gate **G4 =
WP05** (spec 8.3), and G4 is self-certifiable under DECISIONS.md #21: 14.12's criteria are
Philox golden vectors, cadence-invariance, seek/replay state-hash equality against
sequential execution, chunking independence and fresh-process comparison — all machine
checkable. Nothing is in flight; WP00, WP01, WP02, WP03 and WP04 are validated and G0, G1,
G2 and G3 have passed.

Before starting WP05: read spec section 14 in full (14.4 is the dependency rule, 14.6 the
clock and the "never accumulate mutable seconds" rule, 14.7 the four algorithms — clock and
event scheduling, Philox random access, seek, warm-up/export, checkpoint hashing — 14.8 the
state types, 14.11 the taxonomy, 14.12 the gate), then DECISIONS.md #22, #25, #33 and #34,
then `docs/adr/ADR-0004` (arithmetic limits), `ADR-0007` (**closed by WP04**: occurrence
identity, frame ownership, tie ordering, and the schema/version split WP05 inherits) and
`ADR-0005`'s Decision 4.

**The one thing to decide before coding.** 14.2's prerequisite names
`ProjectDocument.simulation.ticks_per_frame` as "a nonzero u32", and **there is no
`simulation` section in the raw schema** — spec 12.8 lists six document fields and none of
them is simulation settings, so WP03 did not author it and WP04 did not need it. WP04's
ruling-1 precedent applies directly: the settings have to arrive as raw project data, which
means `PROJECT_SCHEMA_VERSION` 2 → 3 plus one real `migrate::chain()` step plus a version 2
fixture, and `SimulationSettings` belongs in `anim_project` beside `event.rs`. The machinery
is now proven by a real step (`V1_TO_V2`) rather than by a test-only chain, so this is
routine. What is *not* routine is the alternative — a `ticks_per_frame` supplied at runtime
by the caller — because 14.7's `scheduled_tick = ceil(t * simulation_rate)` and 14.12's
"changing one stream version must leave unrelated stream outputs unchanged" both make the
tick rate part of what a saved project means, and a per-session setting would let the same
revision replay to two different states. **Do not settle this inside `clock.rs`.** Also
decide, in the same question, whether `ProjectDocument.simulation` carries the random-stream
seed: 14.7 derives every `StreamKey` from a "project seed", which 10.8's no-entropy rule
(DECISIONS.md #23) means must be authored and persisted, not generated.

**What WP04 already provides, so it is not rebuilt.** `anim_scene` is the only read path
from the document to a consumer. `SceneEvaluator::evaluate(document, scene_id, position)`
and `evaluate_frame(document, scene_id, frame)` both return `Arc<SceneSnapshot>`, and
13.6's rule that the two are the same delivery decision is tested, so a simulation built on
either cannot invent a second event rule. `SceneSnapshot` carries `time: Rational` (exact,
from `FrameRate::time_of`, never accumulated), `nodes: Arc<[EvaluatedNode]>`,
`simulation_inputs: Arc<[SimulationInput]>` — one per evaluated node, in 13.7's visit order,
with `node_id`, `world_transform` and `z`, which is what 14.8's `SpringBody.id` keys against
— `audio_cues`, and `events: Arc<[SceneEvent]>` already sorted by 13.7's five-term key.
`anim_scene::events_for_frame(document, scene_id, frame)` gives the same sequence as a slice
without building the rest of a snapshot, which is what 14.5 step 5's "compile scene events
into a deterministic tick schedule" should consume. `snapshot_hash(snapshot)` is
`ContentHash` over `anim_engine/snapshot/v1` and includes `EASING_ALGORITHM_VERSION`, so a
simulation that records the snapshot it stepped from can prove which reading of a curve
produced its state. `SnapshotCache` keys on `(SceneId, Revision, FramePos)` with no lock and
no `HashMap`: 14.8's `SimulationContext` owns one, and 14.12's cadence tests are why the
cache must not be able to change an answer. `SceneData` and `ProjectDocument` remain the only
place authored state lives and `CommandProcessor` remains the only writer; nothing in
`anim_sim` may take a `&mut` to either.

**The rule that decides correctness here, not style.** 14.6 and 14.2 together: the step
duration is computed **once** from exact configuration as `1/(fps*N)` and converted to the
declared working type; "mutable seconds are never accumulated"; `step_once` advances exactly
one tick; and results must not depend on cache cadence, render rate, thread scheduling or
random-call history. That last clause is what makes the random path an address function
rather than a cursor — 14.7's `StreamKey` plus absolute tick plus semantic `draw_index` plus
lane, with "never a 'next random' cursor" — so a stream draw must be a pure function of
those four and nothing else. `anim_core::rational::Rational` has the checked arithmetic and
the new `to_f64`; `anim_core::time::checked_frame_to_ticks` and `tick_duration` already
implement 14.7's clock sentence and were property-tested in WP01 (`time_properties.rs`), so
the conversion exists — using it is the requirement, not rewriting it.

**What WP04 deliberately left for later packages, so it is not rediscovered as a gap.**
Motion layers do not exist at all: 13.5 step 3 is deferred with the seam at
`anim_scene::animation::blend_layers`, because the spec has no scene-animation-layer type
(WP15's `MotionLayer` is camera motion, WP12's pose layers have no struct) — see
DECISIONS.md #33 ruling 2. `Easing` has three variants and no extrapolation modes;
pre/post extrapolation is WP15's (spec 30.7), and `sample_property` holding the end value
outside the key range is the whole behaviour until then. No command authors an event:
12.9's set is WP03's and 13.x adds none, so `SceneData::insert_event` is reachable only from
tests until WP10 gives the editor its commands — and
`every_command_kind_has_an_inverse_that_closes` is the test to extend when they arrive.
`EventKind` has three variants with no payloads. `EvaluatedCamera` carries a node, a matrix
and an `is_on` flag and nothing else; the rig (fov, aperture, shutter, near/far, rails,
shots) is WP15's 30.8. `AudioCue` has no sample index: 13.10 makes that conversion
`anim_audio`'s. The node property paths (`transform.x`, `transform.y`, `transform.rotation`
in degrees, `transform.scale`, `z`, `opacity`, `visible`, `asset`) are WP04's convention
because 13.8's `EvaluatedNode` fields need a source and no spec section names a path; WP12
and WP15 add theirs beside these, in `animation.rs`, in the same commit as the field they
feed.

**Not for WP05, but do not lose it:** DECISIONS.md #18 records the settled adapter identity
design — PCI bus id as the reference anchor, a `DedicatedVideoMemory` floor, a startup
assertion instead of a bare user reminder, LUID kept only for WP07 budget attribution, and
`power_preference: HighPerformance` rejected as a sole rule because it degrades silently to
the iGPU. **WP06 and WP07 must read #18 before designing selection or telemetry.** It is a
written constraint on code that does not exist yet, and nothing enforces it.
## Completed Work Packages

| WP | Gate | Status | Validated | Evidence | What later packages must know |
| --- | --- | --- | --- | --- | --- |
| WP00 Repository and Platform Risk Prototypes | **G0** | **validated** | 2026-11-05, CI gate added same day | `docs/adr/ADR-0001`, `0002`, `0003`, `0006`, `0014`, `0015`; reports in `docs/adr/evidence/wp00/`; gate at `tools/ci.ps1` | Locked toolchain rustc 1.99.1 MSVC. Vulkan compute readback works exactly on the RTX 3050 A; `Rgba16Float` attachable; `max_buffer_size` 1 TiB. Vulkan↔DXGI correlate by LUID — and **ordinal 0 is the iGPU here**, the discrete adapter enumerated at index 1. `MoveFileExW(REPLACE_EXISTING\|WRITE_THROUGH)` satisfies never-mixed-bytes; `ReplaceFileW` cannot, by its own documentation. FFmpeg pinned by SHA-256 and verified before spawn. **FFmpeg exits 0 on truncated video input, and audio frame counts are unreadable from the container (48 000 reads back as 12)** — so WP09 counts video from `-count_frames` on the output and audio from decoded PCM. 5 probes, not 6: `wp00_pen` cancelled. |
| WP01 Canonical Rational Time and Stable Identity | **G1** | **validated** | 2026-11-05, gate green same day | `docs/adr/ADR-0004`; `crates/anim_core/src/{rational,time,id,hash,error}.rs`; `crates/anim_core/src/io/{atomic_replace,windows}.rs`; `crates/anim_core/tests/time_properties.rs` (28 tests) plus 48 unit tests | `Rational` persists as one string `"num/den"` — reduced, positive denominator, zero is `0/1` — and **parsing refuses a non-canonical spelling**, so no downstream loader may "normalize on read". Arithmetic runs in `BigInt` and reduces **before** the `i128` width check. `FramePos::subframe` is **private** with a hand-written `Deserialize`, because `(frame, subframe)` is a snapshot cache key and a map sort key: `5 + 3/2` and `6 + 1/2` are one instant, and letting both exist splits the key (DECISIONS.md #22). Floor, not truncation, so a negative exact boundary selects the preceding frame. Typed IDs are **v5-derived — there is no entropy source anywhere in the graph** — and their order is over the 16 bytes, implemented locally rather than inherited from `Uuid`, which is caret-pinned. Canonical bytes: little-endian widths, `u64` length prefixes, domain tag first; that layout is what spec 11.7's "length-prefixed canonical fields in order" will consume. `anim_core::io::atomic_replace::write_atomic` is the shared replacement API on ADR-0006's sequence, and **deviation row 7 is a live instruction for WP03**. The sample index overflows near frame 5.76e15 while the frame number does not, so clipping is a boundary decision for WP08/WP09, not a type decision here. |
| WP02 Immutable Assets and Content Manifest | **G2 (joint with WP03)** | **validated** (spec 11.12 in full; **G2 claimed jointly at WP03, 2026-11-06**) | 2026-11-06, gate green same day | `docs/adr/ADR-0005` (manifest half closed, the document half left open for WP03); `crates/anim_assets/src/{hash,manifest,store,import,resolver,decode,image,audio}.rs`; `crates/anim_assets/tests/store.rs` (19 tests) plus 72 unit tests; `crates/anim_core/src/io/atomic_replace.rs` (`StagedWriter`/`Staged`); `crates/anim_assets/tests/fixtures/blake3_test_vectors.json` | `ContentHash` is 32 bytes with exactly one text spelling (64 lowercase hex, which is also the object file name), and it hashes into a record **raw — not length-prefixed, not hex**: ADR-0004's UUID rule, not its rational rule (ADR-0005 Decision 3). Object paths are `assets/objects/<2>/<2>/<64 hex>` derived from the digest alone, so a display name can never reach a path, and `AssetStore::parse_object_name` is why a `.import.N.tmp` is not an object. Import streams in 64 KiB chunks, hashes and counts, **re-hashes the staged temporary before publishing** (the lost-write proof belongs there, not after the rename) and then renames without overwrite, so a failure leaves no record, no temporary and no partial object. `manifest_digest` = domain `anim_engine/manifest/v1` → `schema_version u32` → **record count u64** → records in `AssetId` byte order, where the order comes from the `BTreeMap` rather than from a sort step. `AssetManifestEntry.dependencies` is **private** (sorted and deduplicated at construction, hand-written `Deserialize`) on DECISIONS.md #22's reasoning, which overrides spec 11.8's `pub` listing — deviation row 9. `import` returns a record and mutates nothing (spec 11.4); `UpsertAsset` is WP03's. Decoders are **metadata-only**: `image.rs` parses PNG/JPEG headers itself and `audio.rs` uses the already-pinned `hound`, because spec 5.1 names no image decoder and pixel/PCM decode has no consumer until WP06/WP08 (DECISIONS.md #26) — the seam is `Decoder::read_metadata` plus `DecoderConfiguration`. `ContentHash` is the type spec 12.8's `AppliedTransaction.envelope_hash` names, so no second hash type. Two read passes per import is the documented cost (DECISIONS.md #30). |
| WP03 Commands, Undo, and Atomic Project Persistence | **G2 (joint with WP02) — G2 PASSED 2026-11-06** | **validated** | 2026-11-06, gate green same day | `docs/adr/ADR-0005` (**closed**: Decisions 9–12, Consequences 5, 7, 8, schema/version impact), `ADR-0006` (**closed**: Recovery policy section, Consequences 4, 5, 6); `crates/anim_project/src/{document,scene,property,schema,migrate,save,recovery}.rs`; `crates/anim_project/tests/persistence.rs` (14 tests) plus 53 unit tests; `crates/anim_commands/src/{command,envelope,registry,processor,history,dry_run}.rs`; `crates/anim_commands/tests/atomicity.rs` (14 tests) plus 39 unit tests; `schemas/project/v1.json`, `schemas/commands/v1.json`; `crates/anim_test_support/src/bin/wp03_save.rs` + `tests/wp03_fault_injection.rs` (5 scenarios); DECISIONS.md #31, #32 | **The `CommandProcessor` is the only writer** — 12.6's sentence is implemented as one private function, `execute`, that every path (`apply`, `undo`, `redo`) routes through, so "never commit a batch prefix" is structural rather than remembered. The command set is **twelve and closed under inversion**: `Upsert`/`Remove` asset, scene, track; `Add`/`Remove` node; `Set`/`Remove` property key; `Place`/`Remove` marker — 12.12's exact inverse requires both halves of each pair (#31.2). `SetPropertyKey` **refuses an unknown track id** rather than creating one, so no command has two inverses (#31.3). Acceptance runs in 12.7's order with **replay checked before the revision**, because a replay after an undo is legitimately at a different revision (#31.5). `undo`/`redo` **move** a transaction between stacks and change only `resulting_revision`; the first implementation mirrored it and made redo re-apply the inverses (#32.2). `AppliedTransaction` carries a fifth field `transaction_label` and `CommandOutcome.command_id` is `Option` (`None` for undo/redo) — both recorded additions (#31.4). History is **unbounded by ruling** (#31.9). **`serde_json::Value` does not preserve member order** (its map is a `BTreeMap`), so canonical bytes exist only on the direct `to_vec` path; `upgrade_with` returns a current file's bytes unchanged rather than re-emitting a `Value`, and `recovery::CompleteCandidate::canonical` reports a valid-but-non-canonical file instead of rewriting or trusting it (ADR-0005 Consequence 7). Document field order **is** spec 12.8's declaration order (Decision 9); the document's, the manifest's and the envelope's `schema_version`s are three independent numbers, each validated at its own boundary (Decision 10); the record collection is **embedded, not re-encoded**, so `manifest_digest`'s pre-image is unchanged by living in a document and is recomputable from a saved file with no engine (Decisions 11, Consequence 8). `save_atomic` stages through `StagedWriter`/`publish` **and then byte-compares the destination itself**, because `write_atomic`'s boundaries are not observable and 12.12 requires faults and kills at each one (deviation row 7 is now reconciled in code, ADR-0006's policy section). Recovery **reads and never writes** — proven by comparing the whole directory listing with digests before and after (`inspection_and_selection_write_nothing_anywhere`); `AmbiguousLineage` is a **warning**, not an error. The kill suite asserts its own residue: the two mid-sequence boundaries **must** leave exactly one `.tmp`, otherwise a late kill would silently test a completed save (#31.13, #32.6). Every rule the save classifier owns has a corruption that fires it, including a newer autosave (#32.7). **Deferred with a deadline at WP04:** no motion layer, no event definition, no easing beyond `Linear`/`Step`, no camera or audio-cue data — 13/15/17 own those rules, and 13.5/13.7 need them, so WP04's first decision is a raw-schema bump **plus** a real `migrate::chain()` step (the machinery is built and proven by a test-only chain). `schemas/export/` is WP09's. Autosave cadence and retention are nowhere in spec 12 and were not invented. |
| WP04 Evaluated Scene Snapshots and Canonical Events | **G3 — G3 PASSED 2026-11-06** | **validated** | 2026-11-06, gate green same day | `docs/adr/ADR-0007` (**closed**: Decisions 1–6, Consequences 1–10, schema/version impact); `crates/anim_scene/src/{evaluator,properties,hierarchy,animation,camera,events,snapshot,cache}.rs`; `crates/anim_scene/tests/evaluation.rs` (16 tests) plus 83 unit tests; `crates/anim_project/src/event.rs`; `crates/anim_project/src/{property,scene,migrate,document,lib}.rs`; `crates/anim_project/tests/fixtures/project_v1.anim.json`; `schemas/project/v2.json`; `crates/anim_core/src/{id,rational}.rs` (eighth typed id, `Rational::to_f64`); `crates/anim_test_support/src/bin/wp04_snapshot.rs` + `src/wp04_fixture.rs` + `tests/wp04_isolation.rs` (7 scenarios); DECISIONS.md #33, #34 | **`anim_scene` is the only read path from the document to any consumer** — 13.2 is structural: every evaluation entry point takes `&ProjectDocument`, no snapshot type is reachable by `&mut`, and `SceneSnapshot` has no setter and no interior mutability. **The float appears last and once.** `u = (t−t0)/(t1−t0)` and the easing applied to it are exact rationals computed in *frame units*, so the frame rate never enters an interpolation; `properties::value_as_f64` is the only conversion in the crate and `Rational::to_f64` (added to `anim_core` for it) is the only spelling. **Easing is a table, not a curve family** — `Easing::Curve(EasingCurve)`, samples at equal positions, first pinned `0/1` and last pinned `1/1` so a curve can never displace a key value, interior overshoot allowed deliberately; `EASING_ALGORITHM_VERSION = 1` is hashed into **snapshots** and not into the document, because a field addition is a data event and a changed *reading* is invisible in the data but must be visible in the digest. **Events are authored data** (`EventDefinition` in `anim_project::event`, `PROJECT_SCHEMA_VERSION` 1→2, `migrate::V1_TO_V2`, a committed v1 fixture, `schemas/project/v2.json` generated from v1 and v1 kept): no `serde(default)` on `events`, so an unmigrated version 1 file is refused rather than read as version 2. **`occurrence_index` is 0 for everything WP04 can author** — one position per definition — and the index is in the identity anyway so WP14's repetition extends it without moving the first firing's id; the derivation is `derive(DNS namespace, "anim_engine/event_occurrence/v1" ‖ definition bytes ‖ index)`. **Frame ownership is one rule computed in exact time**: `[time_of(n), time_of(n+1))`, `containing_frame(p) = p.frame`, sound only because `FramePos`'s subframe is private and normalized (#22), and `camera::active_camera_at` reads the **whole** cut sequence because the cut that decides frame 9 is normally in frame 4 — a per-frame read would be 13.6's forbidden second delivery rule. **13.5 step 3 is NOT implemented, by decision**: the spec has no scene-animation-layer type (WP15's `MotionLayer` is camera motion, WP12's pose layers have no struct), 13.12 lists no layer criterion, so the seam is `animation::blend_layers` with one base layer. **`EvaluatedNode`'s fields come from a WP04 property-path convention** — `transform.x/y/rotation(degrees)/scale`, `z`, `opacity`, `visible`, `asset` — because 13.8 declares those fields and no spec section names one path; later packages extend the list in `animation.rs` beside the field they feed. **`SceneError::EventOrderOverflow` is declared and unreachable**: with one position per definition the ordering path has no arithmetic to overflow, so 8 of 13.11's 9 variants have a producing test and that one waits for WP14. `sin_cos()` returns `(sin, cos)` — the swapped destructure rotated every unauthored node a quarter turn and is commented at the call site (#34). No command authors an event yet (WP10's), and no camera rig, extrapolation mode or layer priority exists (WP15's, 30.7/30.8). |


## Pending human approvals

Visual gates are batched; nothing is owed yet.

| Gate | After stage | What is needed | Status |
| --- | --- | --- | --- |
| 1 | S2 | Approve golden frames once; hashes become the baseline | not reached |
| 2 | S3 | Use the editor, confirm it is workable | not reached |
| 3–4 | S4 | Judge that action and camera read correctly | not reached |
| 5 | S5 | Level 3 acceptance clips | not reached |

## Deviations from spec

**These override the DOCX. The DOCX still describes cancelled work in five places,
so read this table before starting WP00 or WP10.**

| # | Spec sections | Deviation | Since |
| --- | --- | --- | --- |
| 1 | 9.3, 9.5 step 6, 9.12 (pen line), 19 (WP10 §19 ink), 4.2 (`ink.rs`) | **Pen/ink input cancelled.** Do not build `crates/anim_test_support/src/bin/wp00_pen.rs` or `crates/anim_editor/src/ink.rs`. G0 validation drops the "pressure changes during a physical pen test" requirement — deleted, not waived. Artwork is authored externally and imported through WP02. **Recorded as `docs/adr/ADR-0015-native-ink-routing-CANCELLED.md`, which closes the owed addendum.** | 2026-11-04 |
| 2 | 9.2, 5.1 (toolchain unspecified) | **MSVC ABI chosen** (`stable-x86_64-pc-windows-msvc`, rustc 1.99.1). The spec never names a toolchain, so this is ours to decide. | 2026-11-04 |
| 3 | 5.1 (dependency table) | **`ash 0.38`, `pollster 0.4` and `sha2 0.11.1` added** to the dependency baseline. Spec 9.7 needs `VkPhysicalDeviceIDProperties.deviceLUID` and wgpu 30 exposes no raw handle or LUID; the probes need an executor for wgpu's async entry points; spec 5.1 names SHA-256 for executable verification but its only hash crate is blake3. Spec 3.2 requires an ADR for a dependency exception — all three are recorded in `docs/adr/ADR-0001-rust-and-dependency-lock.md`. | 2026-11-05 |
| 4 | 9.3 (`tests/wp00_smoke.rs`) | **Smoke test relocated** to `crates/anim_test_support/tests/wp00_smoke.rs`. A virtual workspace root does not compile a root `tests/` directory — verified, not assumed — so at the literal path the file would never run. Same name, owning crate. **Spec 4.1's `tests/integration/` and `tests/golden/` have the same problem; decide it in WP11 before writing integration tests.** DECISIONS.md #13. | 2026-11-05 |
| 5 | 9.3 (file list) | **`crates/anim_test_support/src/probe_tools.rs` added** — shared external-process launch, bounded pipe drain, deadline with terminate-and-reap, and manifest digest verification. Duplicating it across the media and export probes means two chances to orphan a child. Stays inside the WP00-only crate. DECISIONS.md #12. | 2026-11-05 |
| 6 | 9.2 ("stable Rust with rustfmt and Clippy") | **`clippy::pedantic` is not enabled**; `all` is denied and the determinism-relevant lints are denied by name. Enabling pedantic produced 131 non-defect diagnostics on WP00's first real code. DECISIONS.md #11 records the measurement and the residual. | 2026-11-05 |
| 7 | 12.7 (`ReplaceFileW` and `project.anim.json.<uuid>.tmp`) | **Atomic replacement uses `MoveFileExW(REPLACE_EXISTING \| WRITE_THROUGH)` with an explicit `.bak` written by us, not `ReplaceFileW`; temp files use a `create_new` counter sequence, not a UUID.** ADR-0006 rejected `ReplaceFileW` from that API's own documentation — `ERROR_UNABLE_TO_MOVE_REPLACEMENT` leaves the destination path absent — and `wp00_replace` proved the alternative on this machine. The UUID temp name is unsatisfiable because `uuid` is v5-only, with no entropy source. DECISIONS.md #23. **Read by WP03: `save_atomic` uses `anim_core::io::atomic_replace` (`StagedWriter` → `Staged::publish` with `Replacement::with_backup`) and the `create_new` counter sequence; ADR-0006's Recovery-policy section records the reconciliation.** | 2026-11-05 |
| 8 | 10.3 (file list) | **`crates/anim_core/src/io.rs` added** — the module declaration file for `io/atomic_replace.rs` and `io/windows.rs`, which 10.3 lists without either `io.rs` or `io/mod.rs`. Chose `src/io.rs` so the added path is the one Rust requires rather than two. Also: `io/windows.rs` is **not** `#[cfg(windows)]`-gated, because the workspace pins one platform and a not-Windows branch would be either a second unproven implementation or the silent fallback spec 9.11 forbids. DECISIONS.md #24. | 2026-11-05 |
| 9 | 11.8 (`pub dependencies: Vec<AssetId>`), 11.8 (`pub records: BTreeMap`), 11.3 (no `error.rs`), 11.11 (ten categories) | **`AssetManifestEntry.dependencies` and `AssetManifest.records` are private**, each behind a validating constructor with a hand-written `Deserialize`: spec 10.8's "fields whose invariants require normalization are private" is applied to a sorted, deduplicated dependency list exactly as DECISIONS.md #22 applied it to `FramePos::subframe`, because a manifest that can hold two byte streams for one record has two digests. **`AssetError` lives in `crates/anim_assets/src/lib.rs`** because 11.3 lists no error file for the crate. **Four variants beyond 11.11's ten are added** — `DuplicateDependency`, `MalformedAsset`, `MalformedManifest`, `SizeOverflow` — each required by a spec rule the list has no row for, and none folded into `DuplicateAssetId`, which would blame the wrong object. DECISIONS.md #29. | 2026-11-06 |

| 10 | 12.3 (file list), 12.12 (validation), 4.2 | **Three test files spec 12.3 does not list**: `crates/anim_project/tests/persistence.rs`, `crates/anim_test_support/src/bin/wp03_save.rs`, `crates/anim_test_support/tests/wp03_fault_injection.rs`. 12.12 requires **both** a functional suite and a fault-injection suite, and lists one file. The persistence clauses cannot live in `anim_commands/tests/atomicity.rs` without reaching across 12.4's dependency boundary into `anim_project`'s save orchestration, and "kill test processes at each save stage" needs a process that dies — 4.2 already assigns fault injection to `anim_test_support`, and `CARGO_MANIFEST_DIR`-relative paths plus `CARGO_BIN_EXE_wp03_save` make the child the same build as the parent. Same class of omission as rows 4, 5 and 8: 12.3 names responsibilities, not test mechanics. DECISIONS.md #31.12, #32. | 2026-11-06 |
| 11 | 12.4 ("anim_commands -> anim_project"), 12.8 (`AppliedTransaction`), 12.9 (`CommandOutcome`) | **`anim_commands` depends on `anim_assets` directly**, and two spec-listed shapes gained or lost a field. The dependency edge is needed for `ContentHash`, `hash_bytes` and `manifest_digest`: BUILD_STATE's reading of 12.4's chain reaches the *type* legally through `anim_project`'s re-export, but forwarding another crate's *functions* through `anim_project` would make it look like the owner of content hashing, which 11.2 assigns to `anim_assets` — the same reasoning that keeps `anim_assets` a direct dependency of `anim_core`. `AppliedTransaction` carries `transaction_label` (12.9's undo/redo outcomes must name the transaction they reversed, and a hash is not a label); `CommandOutcome.command_id` is `Option<CommandId>`, `None` for undo and redo, because 12.8 gives a transaction no envelope id and truncating a digest into one would invent an identity nothing reproduces. `Easing` has two variants rather than 13.11's `InvalidEasing` parameter set, and `SceneData`/`NodeData`/`TrackData`/`Marker`/`PropertyKey`/`PropertyCurve` are field-shaped **here**, not in the spec: no section of spec 12 defines them, which the WP03 spec research pass confirmed by searching the whole document, so 13 and 15 will extend them, and DECISIONS.md #31.1 states what was deliberately left out and why. DECISIONS.md #31.1, #31.4, #31.6. | 2026-11-06 |

| 12 | 12.3 (file list), 10.8 (id list), 13.3 (file list), 12.8 (`SceneData`), 12.5 step 9, 5.1 | **WP04's raw-data additions.** `crates/anim_project/src/event.rs` is a file neither 12.3 nor 13.3 lists: 13.3 assigns events to `anim_scene/src/events.rs`, but 13.5 step 1 validates "event definitions" and 13.7 orders occurrences by `priority`, `source_node_id` and `local_sequence`, which only exist as authored data — an evaluator cannot validate what nobody wrote, so the authored half is in the crate that owns authored state and the occurrence half stays in WP04. `SceneData` gained a fifth collection (`events`, a `BTreeMap` keyed by `EventDefinitionId`), which 12.8 does not list at all because **no section of the spec defines `SceneData`'s fields** — the same omission row 11 records, extended by the fields 13 needs. `EventOccurrenceId` is an eighth typed id where spec 10.8 lists seven; it lives in `anim_core::id` because `typed_id!` is private to that file and the alternative is exporting the macro (which changes `anim_core` anyway) or duplicating the strict-parse and 16-byte-`Ord` rules in the tie-break term of the event ordering. `PROJECT_SCHEMA_VERSION` 1→2 with `migrate::V1_TO_V2` and the committed `tests/fixtures/project_v1.anim.json`, which is what 12.5 step 9's "fixtures for every schema version" asked for and WP03 could not yet provide. `schemas/project/v2.json` is generated from `v1.json` rather than retyped, and v1 is kept because the files it describes still exist on disk. `Easing` gained `Curve(EasingCurve)` with a validated endpoint rule, so row 11's "`Easing` has two variants" is superseded: 13.11's `InvalidEasing` now has a condition that can hold. DECISIONS.md #33 rulings 1, 3, 4, 6; `ADR-0007`. | 2026-11-06 |
| 13 | 13.3 (test file list), 13.8 (unspecified types), 13.6 (property paths) | **Two test files and one support module spec 13.3 does not list**, and three types 13.8 names without fields. `crates/anim_scene/tests/evaluation.rs` is listed; `crates/anim_test_support/src/bin/wp04_snapshot.rs` and `crates/anim_test_support/tests/wp04_isolation.rs` are not, and G3's "identical canonical snapshots and event logs from **two fresh processes**" cannot be done from inside one: agreement in a single process is structural, so the clause needs a process that starts again. Same class as rows 4, 5, 8 and 10, and `src/wp04_fixture.rs` exists so the child and the parent share one authored state rather than two builders. `EvaluatedCamera`, `AudioCue` and `SimulationInput` are 13.8 field-lists-less types; what is written here is only what 13.5's steps 4–5 can produce (a node, its world matrix, whether a cut has it on; a cue's occurrence identity, node, exact position and source asset; a simulation input per evaluated node), and 14.8, 15.8 and 30.8 are where their real field lists belong. **The node property paths are this package's convention** — `transform.x`, `transform.y`, `transform.rotation` in degrees, `transform.scale`, `z`, `opacity`, `visible`, `asset` — because 13.8 declares those `EvaluatedNode` fields and no section of the spec names one path; the spellings continue what WP03's own schema fixtures already author. DECISIONS.md #33 ruling 7, #34. | 2026-11-06 |
| 14 | 13.5 step 3 | **Animation-layer blending is not implemented, and the deferral is the decision the user made on 2026-11-06.** 13.5's third step asks for "blend animation layers in authored priority order" and the spec has no scene-animation-layer type anywhere: WP15's `MotionLayer` (30.8) is camera motion (`MotionKind::Pan/Tilt/Truck/Dolly/Rail/WhipPan/…`), WP12's pose layers (27.5) have no struct, and 13.12's G3 criteria list no layer clause. The seam is `anim_scene::animation::blend_layers`, called by the evaluator with the one base layer, so the step is a named absence rather than a missing file. DECISIONS.md #33 ruling 2 records the survey and the reason a type invented here would be replaced by WP12 or WP15 at the cost of a second schema bump. **How to apply:** the package that authors scene layers replaces that body and bumps the schema in the same commit; nothing downstream may assume `blend_layers` stays the identity. | 2026-11-06 |

Rationale and rejected alternatives: DECISIONS.md #6, #8, #9, #10, #11–#16, #22–#34, and the ADRs. Rows 7 and 11 are the two WP03 packages touch; row 7 is now reconciled in code and ADR-0006 carries the policy. Rows 12, 13 and 14 are WP04’s, and row 11’s `Easing has two variants` clause is superseded by row 12.

## Risks and open questions

1. Whether a Vulkan render pipeline built without live visual feedback converges
   through golden-frame hashing alone. **Partly answered, 2026-11-05:** the compute
   readback path returns exact integers on the real adapter, so the data path in and
   out of the GPU is sound. Nothing yet exercises a *render pass* — no rasterization,
   no blending, no attachment ordering — so the aesthetic risk that motivated this
   question is still open and will be first tested by WP06's goldens.
2. Subtle determinism or identity bugs can pass their own tests and surface much
   later disguised as a different package's failure. **Partly answered by WP01
   (2026-11-05) and again by WP02 (2026-11-06):** WP01's `tests/time_properties.rs`
   computes every expected value from hand-written `BigInt` arithmetic in the test file
   itself, and that method found two defects review had missed — a 1001× error in
   `tick_duration` and an overflow message that carried no value. WP02 used the same
   rule (hand-rebuilt canonical byte layout, the reference implementation's own published
   digests, an independent length arithmetic in the streaming test) and found five more,
   including one that made every 256-pixel-wide PNG look corrupt — DECISIONS.md #30.
   The exposure is now WP04–WP05, and WP03 held the rule with profit: the field-order test
   that failed on a `serde_json::Value` round trip, the byte-swap corruption that was a no-op
   because the file ends in `}}`, the undo/redo stack bookkeeping that only a state-equality
   check could catch, and the revision count `1 + 8*3` all came from computing the expectation
   independently instead of asking the code what it did (DECISIONS.md #32). An oracle that
   calls the code under test proves only self-consistency.
3. ~~FFmpeg must be vendored and pinned.~~ **Closed 2026-11-05** — see DECISIONS.md
   #10 and `tools/ffmpeg/ffmpeg-manifest.json`.
4. **New, from WP00:** every probe result on this machine is an *unvalidated* Vulkan
   run, because no validation layer is installed. A driver-tolerated misuse would be
   certified green here. Installing the Vulkan SDK would close it; it is not required
   by spec 9.2, so it remains a stated residual rather than a blocker.
5. **New, from WP00:** the dGPU enumeration order is not stable and LUIDs change
   between runs. Any future automation that selects an adapter by ordinal, or gates
   on a recorded LUID, will intermittently target the iGPU.
6. **The gate is enforced by a hook that lives outside the repo.** `tools\ci.cmd` runs
   the three code steps and was shown to block a commit (HEAD unchanged, gate exit 1),
   installed by `tools\install-git-hooks.cmd`. But `.git/hooks` is not versioned, so
   **a fresh clone that skips the installer commits unenforced and displays no sign of
   that** — AGENTS.md step 5 is the only thing standing between a new session and
   believing it was protected. Spec 9.5 step 2 is met in local-script + local-hook
   form; hosted CI remains unmet, because this repository has no remote. DECISIONS #20.
7. **Nothing re-certifies the platform after a driver update.** GeForce Experience
   updates NVIDIA silently; G0's evidence then describes hardware that no longer exists
   and no alarm fires. `-WithProbes` is the remedy and it is manual.
8. **Nothing in DECISIONS.md #18 is enforced by code.** The adapter-identity rules
   (PCI anchor, dedicated-VRAM floor, startup assertion) are constraints on packages
   that do not exist yet. WP06 must read #18; if it doesn't, no test will notice.
9. **`write_atomic` reads the payload back in full, twice** — once to prove the
   staged temporary is durable, once to prove the destination is the new state and not
   a blend. For `project.anim.json` that is noise. For WP09's export it is a
   multi-gigabyte read on every publication, and the code says so at the call site:
   WP09 should hash-compare rather than byte-compare, and must not drop the check
   entirely, because "never mixed bytes" is what the destination verification is
   for. DECISIONS.md #25.
10. **Directory-entry durability is asserted, not pinned.**
    `io::atomic_replace` reports `DirectoryFlush::{Flushed, Unsupported}` and
    `durability_warning()` surfaces the difference, but the test accepts either
    outcome, because which one NTFS returns here was never measured. So the claim
    "the rename is durable in the directory entry" is not certified by any test on
    this machine — the file bytes and the rename are. Spec 18.4 asks only that the
    limitation be visible rather than silent, which it is; a WP11 fault-injection run
    on real hardware is what would settle which branch this machine takes.

11. **The derived-cache path has no consumer yet.** `AssetStore::derived_path` and
    `DecoderConfiguration` implement spec 11.6's three-axis key, and their tests prove the
    layout and the axis independence, but nothing writes a cache until WP06 asks for a
    texture. A key that is wrong in a way only a real cache exposes — most plausibly a
    configuration name that changes meaning without changing its spelling — would surface
    two packages later as a stale GPU texture. WP06 is where this becomes a test rather
    than a claim.
12. **An import reads its payload twice, and a dedupe hit reads the existing object once.**
    Source→temporary with a running digest, then the staged file re-hashed to prove the
    bytes on disk match the bytes that were hashed (DECISIONS.md #28), then the rename.
    For a 4 GiB asset that is 8 GiB of reads. It is deliberate — the alternative is
    certifying an object after it is already published — but WP09's export verification
    budget should treat these as one class of cost rather than discover it mid-export.
13. **Pixel and sample decode is deferred by ruling, not by accident (DECISIONS.md #26),
    and the deferral has a deadline.** `image.rs` and `audio.rs` read headers only, so
    WP06 builds its texture path on `Decoder::read_metadata` plus a decode method it adds
    itself, and WP08 does the same for PCM and for the `symphonia` formats spec 5.1 lists
    (`wav`, `ogg`/`vorbis`, `flac`) — `symphonia` is pinned and still unused in this
    build. Neither package should treat a missing decode as a WP02 defect; both should
    treat an unrecorded change to `DecoderConfiguration`'s spelling as one.

14. ~~WP04 needs raw data WP03 deliberately did not author.~~ **Settled and built 2026-11-06.**
    The four questions were put to the user as one bundled dialog and are recorded in
    DECISIONS.md #33 and `ADR-0007`: events arrived as authored data (schema 1→2 with a real
    migration step), no animation-layer type was created, `EventOccurrenceId` went into
    `anim_core::id`, and easing gained one parameterized variant. Deviations 12, 13 and 14
    are the record of what that obliged. The layer half of the original question is *still*
    open — deferred, not forgotten — and is item 14 of the deviation table plus #33 ruling 2.

15. **Two spec-listed shapes now carry one field more than spec 12.8 lists, and one less than
    spec 12.9 implies.** `AppliedTransaction.transaction_label` and
    `CommandOutcome.command_id: Option<CommandId>` (deviation row 11). A future session
    comparing code against 12.8 will find a difference that is deliberate. **Why:** an undo
    list with no label is a guess, and an undo with a fabricated id is an identity nothing
    reproduces. **How to apply:** do not "correct" either back toward the listing; extend them
    in the same direction, and read #31.4 first.
16. **The registry is the only authorization surface, and nothing narrows it yet.**
    `CommandProcessor::with_registry` and `Registry::new` make spec 12.10's "no privileged
    mutation route" enforceable, and one test proves a narrowed registry refuses a legal
    command (`a_registry_narrowed_by_the_caller_refuses_commands_it_does_not_carry`). But every
    caller today builds `CommandProcessor::new`, which registers all twelve. **Why:** the
    capability exists; the policy that uses it belongs to WP10's session and to any automation
    front end. **How to apply:** WP10 and anything that submits envelopes on a user's behalf
    must be given a narrowed registry, or 12.10's sentence stays a facility rather than a
    control.
17. **`serde_json::Value` reorders object members, and any code that emits one writes
    non-canonical bytes.** Discovered by WP03's own field-order test (ADR-0005 Consequence 7,
    DECISIONS.md #32.1). The engine's canonical path is `to_vec` on the struct, and
    `migrate::upgrade_with` deliberately passes a current file through unchanged for this
    reason. **Why:** a migrated or hand-constructed file is valid, loadable, and a *second
    byte stream* for one state, which is two digests for one revision — the exact failure
    WP04's snapshot identity and WP09's job hash cannot recover from. **How to apply:** any
    future code that builds a project document as JSON must go through
    `schema::serialize_canonical`, not through `json!` or `to_value`; the two schema-drift
    tests and `verify_canonical_bytes` are what catch it, and `recovery`'s
    `CompleteCandidate::canonical` is what reports it.
18. **The killed-process scenarios depend on timing.** `wp03_save` parks inside a save
    boundary and waits up to 30 s to be killed; the parent waits for a stage marker, then
    terminates the child. `expect_residue` is what makes a mistimed kill visible rather than
    silently benign (#31.13), and `kill_while_parked` refuses to proceed if the child has
    already exited. **Why:** the alternative — trusting that the child is still parked — would
    let a late kill pass every content assertion while testing a completed save. **How to
    apply:** if these tests ever flake on a loaded machine, raise both timeouts together; do
    not relax `expect_residue`, because it is the assertion that the scenario is the scenario.


19. **One of 13.11's nine error variants has no producing test.**
    `SceneError::EventOrderOverflow` is declared because 13.11 lists it, and nothing in
    WP04's authored data can reach it: one event definition carries exactly one position, so
    `occurrence_index` is a constant `0` and the other input to that path is a literal domain
    tag. The eight other variants each have a test that produces them, and the file's own
    module comment says so rather than implying nine. **Why:** a declared variant with no
    producer reads, in a later audit, like covered error handling — the same shape as
    #31.1's "two variants are deliberately absent" note, in reverse. **How to apply:** do not
    add a fake call site to make it look reached. WP14's emitter scheduling gives the index
    something to exceed; until then this line and `ADR-0007` consequence 6 are where the gap
    lives.

20. **Rotation reaches the matrix through the platform's `sin`/`cos`, not through a versioned
    fixed-point algorithm.** `animation::LocalTransform::matrix` converts authored degrees to
    radians and calls `f64::sin_cos`, which is a `libm` call, and `[[f64; 3]; 3]` entries flow
    into snapshot bytes. Two processes on this build agree bit-for-bit — G3 proves it, and
    ADR-0001 pins rustc 1.99.1 MSVC — so *within* the certified environment the digest is
    stable. A different C runtime or a different toolchain patch level is not covered by any
    test here. **Why:** every other float in WP04's path comes from `Rational::to_f64`, which
    is correctly-rounded by the language definition and therefore portable; the two trig
    results are the only values whose bits depend on a library the spec does not pin.
    **How to apply:** if golden frames ever disagree across machines, look here first, and
    settle it by adding a versioned fixed-point sine to `anim_core` and moving the algorithm
    number into `EASING_ALGORITHM_VERSION`'s successor rather than by loosening a tolerance.

21. **The node property-path convention is load-bearing and nothing enforces it.**
    `transform.x`, `transform.y`, `transform.rotation`, `transform.scale`, `z`, `opacity`,
    `visible`, `asset` are the names this package chose because 13.8's `EvaluatedNode`
    fields need a source and the spec names none (deviation 13). A document authored with
    `transform.position` instead evaluates as though nothing were authored there: the field
    falls to the engine default and no error is raised, because an absent property is a
    legal state under 13.6. **Why:** the failure is silent and looks like an animation that
    "does nothing", which is the class of defect that costs a session. **How to apply:**
    the constants in `crates/anim_scene/src/animation.rs` are the single registry; a later
    package adding a field adds its path beside them in the same commit, and a WP10 property
    picker should offer those names rather than free text. If a validation pass over
    unrecognised paths is ever wanted, it belongs in `SceneData::validate`, not in the
    evaluator, because the evaluator must keep evaluating whatever the document permits.

22. **Nothing in production can author an event yet.** `SceneData::insert_event` and
    `remove_event` exist and are exercised by tests, but 12.9's command set is WP03's twelve
    and spec 13 adds none, so no envelope can carry an event today. The raw collection is
    therefore reachable only through mutators that `anim_commands` calls, which is the
    intended boundary, and the `CommandProcessor` stays the only writer in a running
    session. **Why:** the data shape was needed by 13.5 step 1 and 13.7, the commands were
    not needed by anything in WP04's gate, and inventing a fourteenth command to demo a
    schema change would put WP04's convenience ahead of 12.9's list. **How to apply:**
    WP10's session owns `PlaceEvent`/`RemoveEvent`-shaped commands, must give them exact
    inverses, must extend `every_command_kind_has_an_inverse_that_closes` rather than trust
    it forward, and must be handed a *narrowed* `Registry` (item 16) like everything else
    that submits envelopes on a user's behalf.

23. **A migrated file is valid, loadable, and not canonical — and WP04 made that a live path
    rather than a hypothetical.** `migrate::upgrade_with` returns a current file's bytes
    unchanged, but a file it actually migrates comes back from a `serde_json::Value`, whose
    member order is alphabetical rather than this build's declaration order. So loading
    `project_v1.anim.json` through `V1_TO_V2` produces a correct document whose bytes are not
    the bytes `serialize_canonical` would write, exactly as ADR-0005 Consequence 7 and
    deviation 11 describe. **Why:** before WP04 no such file existed, so the behaviour was
    proven only by a test-only chain; now it is what happens to a real user's project on
    first open. **How to apply:** `recovery::CompleteCandidate::canonical` is what reports
    it, and "loads without error" must never be read as "is canonical". The first save after
    a migration rewrites the file canonically, which is the correct outcome and should be
    recorded in the session log rather than hidden, and no test may compare a migrated file's
    bytes against a freshly built one without expecting the difference.


## Handover note

Read in order: this file, DECISIONS.md, the spec's section for the package in
work, then the code. Run the workspace test command in DECISIONS.md before
changing anything — a failing test is a faster and more truthful report of
state than any summary.
