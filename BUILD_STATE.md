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
| S1 Deterministic core | WP00–WP05 | in progress — **G0 passed 2026-11-05, G1 passed 2026-11-05**, WP02 validated 2026-11-06, **G2 needs WP02+WP03** | All platform probes pass, toolchain frozen, identical bytes from any seek point |
| S2 Output path | WP06–WP09 | not started | First deterministic MP4 with synced audio |
| S3 Tool + hardening | WP10–WP11 | not started | Usable editor, benchmarks meeting locked thresholds |
| S4 Character and cinema | WP12–WP17 | not started | Gates G10–G15 |
| S5 Level 3 production | WP18–WP22 | not started | Gates G16 onward, Level 3 acceptance |

## Current Work Package

**WP03 — Commands, Undo, and Atomic Project Persistence** (spec 12). Gate **G2 is
WP02+WP03 together** (spec 8.3), so G2 cannot be claimed until this package lands:
8.3's row is "Asset verification, atomic commands, save faults, and migrations pass",
and 12.12 requires **both** a functional suite and a fault-injection suite. Nothing is in
flight; WP00, WP01 and WP02 are validated.

WP00–WP05 execute strictly sequentially — spec 6.1 and 6.2 permit no concurrency
below WP06. Stage S1 covers G0–G4: G0=WP00 ✅ · G1=WP01 ✅ · G2=WP02+WP03 · G3=WP04 ·
G4=WP05.

Before starting WP03: read spec section 12 in full (12.5 is the sequence, 12.7 the three
algorithms, 12.11 the taxonomy, 12.12 the gate), then `docs/adr/ADR-0005`. That ADR is
**half-closed**: its Decision 7 gives the record collection's JSON shape and its
Consequence 5 lists exactly the four document-level questions this package answers —
field ordering in the document object, where the document's schema version sits relative
to `AssetManifest.schema_version`, how a record collection is *embedded* rather than
re-encoded, and how canonical serialization is verified byte-for-byte against a saved
file. WP03 closes ADR-0005.

**What WP02 already provides, so it is not rebuilt.** `anim_assets::AssetManifest` is the
`assets` field of spec 12.8's `ProjectDocument` — its `Serialize`/`Deserialize` are
already implemented and round-trip tested, its `records` and each record's
`dependencies` are private with validating constructors (`insert` replaces and returns
what it displaced, which *is* 11.6's reimport; `from_records` refuses two records for one
id). `AssetManifest::with_schema_version` exists precisely so a migration in
`migrate.rs` can build a manifest at the version it is migrating *to*; nothing in
`anim_assets` validates the version number, so that range question is WP03's.
`anim_assets::ContentHash` is the type spec 12.8's `AppliedTransaction.envelope_hash`
names — 12.4's dependency chain (`anim_commands → anim_project → anim_assets`) reaches it
legally, and a second hash type would be a defect. `anim_assets::validate_manifest` is
what `UpsertAsset`'s validation should call, and `import` returns a record and never
touches a document (spec 11.4), so the command is the only writer.

**Deviation 7 is a live instruction here, not background.** Spec 12.7's `save_atomic`
prescribes `ReplaceFileW` and a `.<uuid>.tmp` name; ADR-0006, DECISIONS.md #23 and #29
and deviation rows 7 and 8 say otherwise, and this is the package where that
reconciliation is written down. Use `anim_core::io::atomic_replace::write_atomic` with
`Replacement::with_backup`, and the `create_new` counter sequence for the temporary. The
sibling API added for WP02, `StagedWriter` → `Staged::publish`, is for *streamed*
payloads (11.7's import, and eventually WP09's export) — a project document is
in-memory bytes, so `write_atomic` remains the right call. ADR-0006's own status line,
"the recovery *policy* closes in WP03", is this package's duty too.

**What WP02 deliberately left for later packages, so it is not rediscovered as a gap.**
Pixel and sample decode (`DECISIONS.md #26`): the decoder registry, importer-version
dispatch and the three-axis cache key are complete, `image.rs` and `audio.rs` read
headers only, and the seam WP06/WP08 extend is `Decoder::read_metadata` plus
`DecoderConfiguration`. `AssetStore::derived_path` has no writer yet. `verify_all` and
`verify_report` exist for WP09's pre-frame-zero gate; nothing enforces that call site
until WP09 exists.

**Not for WP03, but do not lose it:** DECISIONS.md #18 records the settled adapter
identity design — PCI bus id as the reference anchor, a `DedicatedVideoMemory` floor, a
startup assertion instead of a bare user reminder, LUID kept only for WP07 budget
attribution, and `power_preference: HighPerformance` rejected as a sole rule because it
degrades silently to the iGPU. **WP06 and WP07 must read it before designing selection
or telemetry.** It is a written constraint on code that does not exist yet.

**What already exists, so you do not rediscover it by reading code.**

- `anim_core` is the time/identity/encoding/io foundation: `lib.rs`, `diagnostic.rs`,
  `error.rs`, `rational.rs`, `time.rs`, `id.rs`, `hash.rs` (now including
  `CanonicalBytes::fixed_bytes`), `io.rs`, `io/atomic_replace.rs` (now also
  `StagedWriter`/`Staged`), `io/windows.rs`, plus `tests/time_properties.rs`. 55 unit
  tests, 28 property tests. It depends on no workspace crate.
- `anim_assets` is the second crate with content — 91 tests. Spec 11.3's eight files
  exist as listed (`hash`, `manifest`, `store`, `import`, `resolver`, `decode`, `image`,
  `audio`, plus `tests/store.rs`), with `AssetError` at the crate root because 11.3 lists
  no `error.rs`. Dependencies: `anim_core`, `blake3`, `hex`, `hound`, `serde`,
  `thiserror`; dev-only `serde_json`, `tempfile`, `uuid`. **No new package entered
  `Cargo.lock`** — every crate in that list was already resolved (`hound` by WP00's
  `wp00_export` probe, the rest by `anim_core`), so the lock diff is `anim_assets`'
  dependency list and nothing else. That is the opposite of the situation DECISIONS.md
  #25 recorded for WP01, where first consumers did grow the lock.
- `crates/anim_assets/tests/fixtures/blake3_test_vectors.json` is a vendored upstream
  artifact; its provenance and the 131-byte extended-output trap are in DECISIONS.md #27.
- The other ten crates are doc-comment stubs; `anim_app` and `anim_headless` are
  `fn main() {}`. `anim_project` and `anim_commands` are WP03's.
- The spec `.docx` is versioned but unreadable directly: extract it with
  `python tools/spec_dump.py "<docx>" .tools/spec_v32.txt` before reading a section.

## Completed Work Packages

| WP | Gate | Status | Validated | Evidence | What later packages must know |
| --- | --- | --- | --- | --- | --- |
| WP00 Repository and Platform Risk Prototypes | **G0** | **validated** | 2026-11-05, CI gate added same day | `docs/adr/ADR-0001`, `0002`, `0003`, `0006`, `0014`, `0015`; reports in `docs/adr/evidence/wp00/`; gate at `tools/ci.ps1` | Locked toolchain rustc 1.99.1 MSVC. Vulkan compute readback works exactly on the RTX 3050 A; `Rgba16Float` attachable; `max_buffer_size` 1 TiB. Vulkan↔DXGI correlate by LUID — and **ordinal 0 is the iGPU here**, the discrete adapter enumerated at index 1. `MoveFileExW(REPLACE_EXISTING\|WRITE_THROUGH)` satisfies never-mixed-bytes; `ReplaceFileW` cannot, by its own documentation. FFmpeg pinned by SHA-256 and verified before spawn. **FFmpeg exits 0 on truncated video input, and audio frame counts are unreadable from the container (48 000 reads back as 12)** — so WP09 counts video from `-count_frames` on the output and audio from decoded PCM. 5 probes, not 6: `wp00_pen` cancelled. |
| WP01 Canonical Rational Time and Stable Identity | **G1** | **validated** | 2026-11-05, gate green same day | `docs/adr/ADR-0004`; `crates/anim_core/src/{rational,time,id,hash,error}.rs`; `crates/anim_core/src/io/{atomic_replace,windows}.rs`; `crates/anim_core/tests/time_properties.rs` (28 tests) plus 48 unit tests | `Rational` persists as one string `"num/den"` — reduced, positive denominator, zero is `0/1` — and **parsing refuses a non-canonical spelling**, so no downstream loader may "normalize on read". Arithmetic runs in `BigInt` and reduces **before** the `i128` width check. `FramePos::subframe` is **private** with a hand-written `Deserialize`, because `(frame, subframe)` is a snapshot cache key and a map sort key: `5 + 3/2` and `6 + 1/2` are one instant, and letting both exist splits the key (DECISIONS.md #22). Floor, not truncation, so a negative exact boundary selects the preceding frame. Typed IDs are **v5-derived — there is no entropy source anywhere in the graph** — and their order is over the 16 bytes, implemented locally rather than inherited from `Uuid`, which is caret-pinned. Canonical bytes: little-endian widths, `u64` length prefixes, domain tag first; that layout is what spec 11.7's "length-prefixed canonical fields in order" will consume. `anim_core::io::atomic_replace::write_atomic` is the shared replacement API on ADR-0006's sequence, and **deviation row 7 is a live instruction for WP03**. The sample index overflows near frame 5.76e15 while the frame number does not, so clipping is a boundary decision for WP08/WP09, not a type decision here. |
| WP02 Immutable Assets and Content Manifest | **G2 (joint with WP03)** | **validated** (spec 11.12 in full; **G2 is not claimable until WP03 lands**) | 2026-11-06, gate green same day | `docs/adr/ADR-0005` (manifest half closed, the document half left open for WP03); `crates/anim_assets/src/{hash,manifest,store,import,resolver,decode,image,audio}.rs`; `crates/anim_assets/tests/store.rs` (19 tests) plus 72 unit tests; `crates/anim_core/src/io/atomic_replace.rs` (`StagedWriter`/`Staged`); `crates/anim_assets/tests/fixtures/blake3_test_vectors.json` | `ContentHash` is 32 bytes with exactly one text spelling (64 lowercase hex, which is also the object file name), and it hashes into a record **raw — not length-prefixed, not hex**: ADR-0004's UUID rule, not its rational rule (ADR-0005 Decision 3). Object paths are `assets/objects/<2>/<2>/<64 hex>` derived from the digest alone, so a display name can never reach a path, and `AssetStore::parse_object_name` is why a `.import.N.tmp` is not an object. Import streams in 64 KiB chunks, hashes and counts, **re-hashes the staged temporary before publishing** (the lost-write proof belongs there, not after the rename) and then renames without overwrite, so a failure leaves no record, no temporary and no partial object. `manifest_digest` = domain `anim_engine/manifest/v1` → `schema_version u32` → **record count u64** → records in `AssetId` byte order, where the order comes from the `BTreeMap` rather than from a sort step. `AssetManifestEntry.dependencies` is **private** (sorted and deduplicated at construction, hand-written `Deserialize`) on DECISIONS.md #22's reasoning, which overrides spec 11.8's `pub` listing — deviation row 9. `import` returns a record and mutates nothing (spec 11.4); `UpsertAsset` is WP03's. Decoders are **metadata-only**: `image.rs` parses PNG/JPEG headers itself and `audio.rs` uses the already-pinned `hound`, because spec 5.1 names no image decoder and pixel/PCM decode has no consumer until WP06/WP08 (DECISIONS.md #26) — the seam is `Decoder::read_metadata` plus `DecoderConfiguration`. `ContentHash` is the type spec 12.8's `AppliedTransaction.envelope_hash` names, so no second hash type. Two read passes per import is the documented cost (DECISIONS.md #30). |

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
| 7 | 12.7 (`ReplaceFileW` and `project.anim.json.<uuid>.tmp`) | **Atomic replacement uses `MoveFileExW(REPLACE_EXISTING \| WRITE_THROUGH)` with an explicit `.bak` written by us, not `ReplaceFileW`; temp files use a `create_new` counter sequence, not a UUID.** ADR-0006 rejected `ReplaceFileW` from that API's own documentation — `ERROR_UNABLE_TO_MOVE_REPLACEMENT` leaves the destination path absent — and `wp00_replace` proved the alternative on this machine. The UUID temp name is unsatisfiable because `uuid` is v5-only, with no entropy source. DECISIONS.md #23. **WP03 must read this row before writing `save_atomic`.** | 2026-11-05 |
| 8 | 10.3 (file list) | **`crates/anim_core/src/io.rs` added** — the module declaration file for `io/atomic_replace.rs` and `io/windows.rs`, which 10.3 lists without either `io.rs` or `io/mod.rs`. Chose `src/io.rs` so the added path is the one Rust requires rather than two. Also: `io/windows.rs` is **not** `#[cfg(windows)]`-gated, because the workspace pins one platform and a not-Windows branch would be either a second unproven implementation or the silent fallback spec 9.11 forbids. DECISIONS.md #24. | 2026-11-05 |
| 9 | 11.8 (`pub dependencies: Vec<AssetId>`), 11.8 (`pub records: BTreeMap`), 11.3 (no `error.rs`), 11.11 (ten categories) | **`AssetManifestEntry.dependencies` and `AssetManifest.records` are private**, each behind a validating constructor with a hand-written `Deserialize`: spec 10.8's "fields whose invariants require normalization are private" is applied to a sorted, deduplicated dependency list exactly as DECISIONS.md #22 applied it to `FramePos::subframe`, because a manifest that can hold two byte streams for one record has two digests. **`AssetError` lives in `crates/anim_assets/src/lib.rs`** because 11.3 lists no error file for the crate. **Four variants beyond 11.11's ten are added** — `DuplicateDependency`, `MalformedAsset`, `MalformedManifest`, `SizeOverflow` — each required by a spec rule the list has no row for, and none folded into `DuplicateAssetId`, which would blame the wrong object. DECISIONS.md #29. | 2026-11-06 |

Rationale and rejected alternatives: DECISIONS.md #6, #8, #9, #10, #11–#16, #22–#30, and the ADRs.

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
   The exposure is now WP03–WP05, where the rule still applies: an oracle that calls the
   code under test proves only self-consistency.
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

## Handover note

Read in order: this file, DECISIONS.md, the spec's section for the package in
work, then the code. Run the workspace test command in DECISIONS.md before
changing anything — a failing test is a faster and more truthful report of
state than any summary.
