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
| FFmpeg | Present — 9.0.2 gyan essentials on PATH. Spec forbids depending on system FFmpeg; a pinned binary must be chosen before WP09 |
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

### Prerequisite that CANNOT be met here

**No pen or touch digitizer exists on this machine.** Zero matches for
pen/wacom/digitizer/stylus across all present PnP devices; only an ASUS Precision
Touchpad and USB mouse. The WP00 native-pen probe (pressure, tilt, coalesced
history, cancellation — spec 9.12 requires "pressure changes during a physical pen
test") cannot pass, and spec 9.11 forbids a mouse-only substitute.

**Resolved 2026-11-04:** build the probe, validate the code path against synthetic
`WM_POINTER*` packets, record G0 as *partial — pen unverified on this machine*, and
close the gap when a tablet is available. Details and residual risk in DECISIONS.md
#8. The synthetic test must never be reported as the physical one.

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
| Baseline commit of the scaffold files | Not authorized yet |
| Pen probe hardware | **Cannot pass** — no digitizer on this machine. Awaiting user decision |

Workspace layout to scaffold is extracted verbatim from spec 4.1 and 9.3 into the
conversation record: 13 crates under `crates/` (`anim_core` … `anim_test_support`),
plus `config/`, `schemas/`, `docs/adr/`, `docs/benchmarks/`, `tests/`,
`benchmarks/`, `tools/ffmpeg/`, `packaging/windows/`, and six `wp00_*.rs` probe
binaries in `anim_test_support`.

Blocked on: toolchain installation, then the answer on the baseline commit.

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

None recorded.

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
