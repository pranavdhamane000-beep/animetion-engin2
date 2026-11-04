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
| RTX 3050A reference machine | OK — **RTX 3050 A Laptop GPU confirmed**, 4094 MiB, PCI bus 01:00.0, LUID `00cb5c07-00000000`. Plus Intel iGPU `VEN_8086 DEV_A7AB`, LUID `6b0f0100-00000000` |

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
satisfiable here. No Vulkan validation layers installed (only Steam/EOS overlays),
so the readback probe runs unvalidated. No `VULKAN_SDK` — wgpu needs only the
runtime loader. Single built-in panel 1920x1200 @144 Hz, DPI 120.

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
| S1 Deterministic core | WP00–WP05 | not started | All platform probes pass, toolchain frozen, identical bytes from any seek point |
| S2 Output path | WP06–WP09 | not started | First deterministic MP4 with synced audio |
| S3 Tool + hardening | WP10–WP11 | not started | Usable editor, benchmarks meeting locked thresholds |
| S4 Character and cinema | WP12–WP17 | not started | Gates G10–G15 |
| S5 Level 3 production | WP18–WP22 | not started | Gates G16 onward, Level 3 acceptance |

## Current Work Package

WP00 — Repository and Platform Risk Prototypes (spec 9), gate **G0**. Stage S1
covers G0–G4: G0=WP00 · G1=WP01 · G2=WP02+WP03 · G3=WP04 · G4=WP05.

WP00–WP05 execute strictly sequentially — spec 6.1 and 6.2 permit no
concurrency below WP06, and WP02/WP03 cannot even overlap with each other. See
DECISIONS.md #7.

Environment setup status, ahead of any code:

| Step | State |
| --- | --- |
| C++ compiler and linker | **Already present** — VS Build Tools 2026, MSVC 14.50.35717. The planned 5–8 GB install was unnecessary |
| rustup + `stable-x86_64-pc-windows-msvc`, `RUSTUP_HOME`/`CARGO_HOME` on D: | **Done** — rustc/cargo 1.99.1, build verified |
| FFmpeg/FFprobe candidate binaries (spec 9.2) | **Done** — vendored and hash-pinned, see DECISIONS.md #10 |
| Baseline commit | **Done** — `3d60b84` carries spec + state files; state corrections commit precedes WP00 |
| Pen probe hardware | **Moot** — pen input removed from scope; G0 is now fully achievable here |

**Shell note for every future session:** `RUSTUP_HOME`/`CARGO_HOME` were persisted with
`setx`, so a shell started before that (including this app's Git Bash) has no `cargo` on
PATH. Prepend it: `export PATH="/d/cargo/bin:$PATH"`. Not a broken install — a stale
environment block.

Workspace layout to scaffold is extracted verbatim from spec 4.1 and 9.3: 13 crates
under `crates/` (`anim_core` … `anim_test_support`), plus `config/`, `schemas/`,
`docs/adr/`, `docs/benchmarks/`, `tests/`, `benchmarks/`, `tools/ffmpeg/`,
`packaging/windows/`, and **five** `wp00_*.rs` probe binaries in `anim_test_support`
(`wp00_pen.rs` is cancelled — see Deviations).

Blocked on: nothing. All spec 9.2 prerequisites are met on this machine.

## Completed Work Packages

No package validated yet.

## Pending human approvals

Visual gates are batched; nothing is owed yet.

| Gate | After stage | What is needed | Status |
| --- | --- | --- | --- |
| 1 | S2 | Approve golden frames once; hashes become the baseline | not reached |
| 2 | S3 | Use the editor, confirm it is workable | not reached |
| 3–4 | S4 | Judge that action and camera read correctly | not reached |
| 5 | S5 | Level 3 acceptance clips | not reached |

## Deviations from spec

**These override the DOCX. Read this section before starting WP00 or WP10 — the
spec still describes cancelled work, and an ADR addendum removing it is owed.**

| # | Spec sections | Deviation | Since |
| --- | --- | --- | --- |
| 1 | 9.3, 9.5 step 6, 9.12 (pen line), 19 (WP10 §19 ink), 4.2 (`ink.rs`) | **Pen/ink input cancelled.** Do not build `crates/anim_test_support/src/bin/wp00_pen.rs` or `crates/anim_editor/src/ink.rs`. G0 validation drops the "pressure changes during a physical pen test" requirement. Artwork is authored externally and imported through WP02. | 2026-11-04 |
| 2 | 9.2, 5.1 (toolchain unspecified) | **MSVC ABI chosen** (`stable-x86_64-pc-windows-msvc`, rustc 1.99.1). The spec never names a toolchain, so this is ours to decide and needs a `docs/adr/` entry. | 2026-11-04 |

Rationale and rejected alternatives: DECISIONS.md #6, #8, #9.

## Risks and open questions

1. Whether a D3D12/Vulkan render pipeline built without live visual feedback
   converges through golden-frame hashing alone. WP00's readback probe is the
   first evidence either way.
2. Subtle determinism or identity bugs can pass their own tests and surface
   much later disguised as a different package's failure.
3. FFmpeg must be vendored and pinned; the system copy on PATH is not the
   build input.

## Handover note

Read in order: this file, DECISIONS.md, the spec's section for the package in
work, then the code. Run the workspace test command in DECISIONS.md before
changing anything — a failing test is a faster and more truthful report of
state than any summary.
