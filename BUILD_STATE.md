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
| S1 Deterministic core | WP00–WP05 | in progress — **G0 passed 2026-11-05, G1 passed 2026-11-05**, G2 next | All platform probes pass, toolchain frozen, identical bytes from any seek point |
| S2 Output path | WP06–WP09 | not started | First deterministic MP4 with synced audio |
| S3 Tool + hardening | WP10–WP11 | not started | Usable editor, benchmarks meeting locked thresholds |
| S4 Character and cinema | WP12–WP17 | not started | Gates G10–G15 |
| S5 Level 3 production | WP18–WP22 | not started | Gates G16 onward, Level 3 acceptance |

## Current Work Package

**WP02 — Immutable Assets and Content Manifest** (spec 11). Gate **G2 is WP02+WP03
together** (spec 8.3), so G2 cannot be claimed by WP02 alone. Nothing is in flight;
WP00 and WP01 are validated — see Completed Work Packages.

WP00–WP05 execute strictly sequentially — spec 6.1 and 6.2 permit no concurrency
below WP06, and WP02/WP03 cannot even overlap with each other. See DECISIONS.md #7.
Stage S1 covers G0–G4: G0=WP00 ✅ · G1=WP01 ✅ · G2=WP02+WP03 · G3=WP04 · G4=WP05.

Before starting WP02: read spec section 11, then `docs/adr/ADR-0004`. That ADR is not
background reading for WP02 — its Decision 4 (little-endian widths, `u64` length
prefixes, canonical rational text, 16-byte UUIDs, domain tag first) **is** the
specification WP02's manifest digest has to satisfy, and spec 11.7 asks for
"length-prefixed canonical fields in order", which is the same contract. Then read
`crates/anim_core/src/{rational,time,id,hash}.rs` before defining any asset type:
spec 10.4 forbids a second time or identity model, so a WP02-local `u64` counter or
`f64` timestamp is a defect on arrival.

**ADR-0005 ("canonical project and manifest encoding") is open; the register closes it
across WP01–WP03.** WP01 recorded the value-level encoding and deliberately did not
decide document-level framing — record ordering, where the schema version lives, how
manifest records are framed — because those questions have answers only once the
records exist.

**Do not rebuild what WP01 already proves.** `anim_core::io::atomic_replace` is the
shared replacement API; spec 12.7 tells WP03 to use `ReplaceFileW`, ADR-0006
and deviation row 7 say otherwise, and WP03 is where that reconciliation is written
down. `wp00_replace` stays as G0 evidence and is not refactored to call the new API —
spec 9.10's rule is that production crates do not import WP00 binaries, and the probe
has already done its job.

**Not for WP01, but do not lose it:** DECISIONS.md #18 records the settled adapter
identity design — PCI bus id as the reference anchor, a `DedicatedVideoMemory` floor, a
startup assertion instead of a bare user reminder, LUID kept only for WP07 budget
attribution, and `power_preference: HighPerformance` rejected as a sole rule because it
degrades silently to the iGPU. **WP06 and WP07 must read it before designing selection
or telemetry.** It is a written constraint on code that does not exist yet.

**What already exists, so you do not rediscover it by reading code.** A fresh-session
resume test on 2026-11-05 had to open `crates/anim_core/src/lib.rs` and its
`Cargo.toml` to work this out:

- `anim_core` is the only production crate with content: `lib.rs`, `diagnostic.rs`
  (WP00) and `rational.rs`, `time.rs`, `id.rs`, `hash.rs`, `error.rs`, `io.rs`,
  `io/atomic_replace.rs`, `io/windows.rs` (WP01), plus
  `tests/time_properties.rs`. It depends on `serde`, `thiserror`, `uuid`,
  `num-bigint`, `num-integer`, `num-traits`, `blake3`, `hex` and target-gated
  `windows` — spec 10.4's set, exactly — and on **no workspace crate**, which is what
  lets every other crate import these types instead of defining their own.
  `proptest`, `tempfile`, `serde_json`, and a dev-only copy of `num-bigint`, `blake3`
  and `uuid` are dev-dependencies only; G1's tests compute their own references so the
  oracle is never the code under test.
- `uuid`, `num-bigint`, `num-integer`, `num-traits` and `blake3` were pinned in
  `[workspace.dependencies]` by WP00 but referenced by no crate, so they were absent
  from `Cargo.lock` — an unused `workspace.dependencies` entry is pruned from
  resolution. WP01 is their first consumer and `Cargo.lock` has grown accordingly,
  which was expected, not drift. `proptest` and `tempfile` are now used as
  dev-dependencies only (spec 5.1 keeps them out of library graphs).
- The other ten crates are doc-comment stubs. `anim_app` and `anim_headless` are
  `fn main() {}` stubs.
- The spec `.docx` is versioned but unreadable directly: extract it with
  `python tools/spec_dump.py "<docx>" .tools/spec_v32.txt` before reading a section.

## Completed Work Packages

| WP | Gate | Status | Validated | Evidence | What later packages must know |
| --- | --- | --- | --- | --- | --- |
| WP00 Repository and Platform Risk Prototypes | **G0** | **validated** | 2026-11-05, CI gate added same day | `docs/adr/ADR-0001`, `0002`, `0003`, `0006`, `0014`, `0015`; reports in `docs/adr/evidence/wp00/`; gate at `tools/ci.ps1` | Locked toolchain rustc 1.99.1 MSVC. Vulkan compute readback works exactly on the RTX 3050 A; `Rgba16Float` attachable; `max_buffer_size` 1 TiB. Vulkan↔DXGI correlate by LUID — and **ordinal 0 is the iGPU here**, the discrete adapter enumerated at index 1. `MoveFileExW(REPLACE_EXISTING\|WRITE_THROUGH)` satisfies never-mixed-bytes; `ReplaceFileW` cannot, by its own documentation. FFmpeg pinned by SHA-256 and verified before spawn. **FFmpeg exits 0 on truncated video input, and audio frame counts are unreadable from the container (48 000 reads back as 12)** — so WP09 counts video from `-count_frames` on the output and audio from decoded PCM. 5 probes, not 6: `wp00_pen` cancelled. |
| WP01 Canonical Rational Time and Stable Identity | **G1** | **validated** | 2026-11-05, gate green same day | `docs/adr/ADR-0004`; `crates/anim_core/src/{rational,time,id,hash,error}.rs`; `crates/anim_core/src/io/{atomic_replace,windows}.rs`; `crates/anim_core/tests/time_properties.rs` (28 tests) plus 48 unit tests | `Rational` persists as one string `"num/den"` — reduced, positive denominator, zero is `0/1` — and **parsing refuses a non-canonical spelling**, so no downstream loader may "normalize on read". Arithmetic runs in `BigInt` and reduces **before** the `i128` width check. `FramePos::subframe` is **private** with a hand-written `Deserialize`, because `(frame, subframe)` is a snapshot cache key and a map sort key: `5 + 3/2` and `6 + 1/2` are one instant, and letting both exist splits the key (DECISIONS.md #22). Floor, not truncation, so a negative exact boundary selects the preceding frame. Typed IDs are **v5-derived — there is no entropy source anywhere in the graph** — and their order is over the 16 bytes, implemented locally rather than inherited from `Uuid`, which is caret-pinned. Canonical bytes: little-endian widths, `u64` length prefixes, domain tag first; that layout is what spec 11.7's "length-prefixed canonical fields in order" will consume. `anim_core::io::atomic_replace::write_atomic` is the shared replacement API on ADR-0006's sequence, and **deviation row 7 is a live instruction for WP03**. The sample index overflows near frame 5.76e15 while the frame number does not, so clipping is a boundary decision for WP08/WP09, not a type decision here. |

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

Rationale and rejected alternatives: DECISIONS.md #6, #8, #9, #10, #11–#16, #22, #23, #24, #25, and the ADRs.

## Risks and open questions

1. Whether a Vulkan render pipeline built without live visual feedback converges
   through golden-frame hashing alone. **Partly answered, 2026-11-05:** the compute
   readback path returns exact integers on the real adapter, so the data path in and
   out of the GPU is sound. Nothing yet exercises a *render pass* — no rasterization,
   no blending, no attachment ordering — so the aesthetic risk that motivated this
   question is still open and will be first tested by WP06's goldens.
2. Subtle determinism or identity bugs can pass their own tests and surface much
   later disguised as a different package's failure. **Partly answered by WP01,
   2026-11-05:** `tests/time_properties.rs` computes every expected value from
   hand-written `BigInt` arithmetic in the test file itself, and that method found two
   defects review had missed — a 1001× error in `tick_duration` and an overflow message
   that carried no value. The exposure is now WP02–WP05, where the same rule applies:
   an oracle that calls the code under test proves only self-consistency.
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

## Handover note

Read in order: this file, DECISIONS.md, the spec's section for the package in
work, then the code. Run the workspace test command in DECISIONS.md before
changing anything — a failing test is a faster and more truthful report of
state than any summary.
