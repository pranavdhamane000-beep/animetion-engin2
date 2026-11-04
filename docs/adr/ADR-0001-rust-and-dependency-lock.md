# ADR-0001 — Exact Rust and dependency lock

Status: accepted
Date: 2026-11-05
Work package: WP00 (spec register line "ADR-0001 · Exact Rust and dependency lock")
Closes: gate G0 prerequisite "exact versions are locked" (spec 8.3)

## Problem

The spec fixes the *shape* of the toolchain requirement — "stable Rust with rustfmt
and Clippy" (9.2), a `rust-toolchain.toml` (5.1), a committed `Cargo.lock`, and no
floating versions in any later package — but it never names an ABI, a target triple,
a compiler version, or a single dependency version. WP00's probe reports are only
reproducible if those are pinned before the first probe runs, because a probe that
records "the Vulkan readback works" is worthless if the next build resolves a
different `wgpu`.

A full-document search of v3.2 found zero occurrences of MSVC, GNU,
`x86_64-pc-windows-*`, any target triple, `rustup`, `channel =`, the contents of
`rust-toolchain.toml`, a Windows SDK version, or VS Build Tools. The ABI choice is
ours.

## Alternatives considered

- **`x86_64-pc-windows-gnu`** (mingw-w64 via chocolatey). Rejected: the `windows`
  crate on the GNU target links against import stubs that can be absent, which
  spreads a linking-error tax across every later Win32 package (WP00 adapter probe,
  WP03 atomic replacement, WP07 DXGI telemetry, WP10 editor). Also unnecessary — the
  MSVC toolchain was already installed.
- **`x86_64-pc-windows-msvc`.** Chosen. VS Build Tools 2026 present at
  `C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools` with MSVC
  14.50.35717, `cl.exe`, `link.exe`, `vcvars64.bat`, and Windows SDK 10.0.26100.0
  (UCRT `libucrt.lib` and `um` headers verified). No install was needed.
- **Rustup default profile and `CARGO_HOME` on C:**. Rejected: C: had 18 GB free of
  232 GB. `RUSTUP_HOME=D:\rustup`, `CARGO_HOME=D:\cargo`.

## Decision

- Toolchain: `rust-toolchain.toml` with `channel = "1.99.1"`, `components =
  ["rustfmt", "clippy"]`, `targets = ["x86_64-pc-windows-msvc"]`.
- Resolved compiler: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, cargo 1.99.1
  (`5f94df478 2026-08-27`), rustfmt 1.10.0-stable, clippy 0.1.99.
- Every version in `[workspace.dependencies]` was read from crates.io on 2026-11-05
  (`max_stable_version`), not recalled.

### Three dependency additions beyond the spec 5.1 table

Spec 3.2: "A dependency exception requires an ADR." These are that record.

| Crate | Version | Why it is not in spec 5.1 |
| --- | --- | --- |
| `ash` | `=0.38.0+1.3.281` | Spec 9.7 makes `VkPhysicalDeviceIDProperties.deviceLUID` the input to adapter correlation. `wgpu` 30.0.1 never queries that struct: `wgpu::Adapter::as_hal::<hal::api::Vulkan>()` exists but `wgpu_hal::vulkan::Adapter`'s fields are private, its `raw: vk::PhysicalDevice` has no `as_raw()`, and `AdapterInfo` carries no LUID field. A case-insensitive scan of the whole `wgpu-hal` 30.0.0 tree for `luid`/`PhysicalDeviceIDProperties`/`id_properties` returns zero matches. `ash` is what `wgpu-hal` itself depends on, so the process binds one Vulkan loader and one copy of the types. |
| `pollster` | `0.4` | `Instance::enumerate_adapters` and `Adapter::request_device` are `async` in wgpu 30, and the probes are synchronous CLI binaries with no executor. Cheapest possible executor. |
| `sha2` | `0.11.1` | Spec 5.1 requires verifying the FFmpeg executable's **SHA-256** against `tools/ffmpeg/ffmpeg-manifest.json`, but the table's only hash crate is `blake3`, which is not SHA-256. `blake3` stays the internal content hash; `sha2` is used solely where the spec names the algorithm, because the digest must be reproducible by an operator with any standard tool rather than only by this engine. |

Rejected alternatives to `ash`: hand-rolled `extern "system"` Vulkan FFI (adds ~120
lines of unsafe binding to the exact probe whose job is to remove risk, and the
binding then needs its own proof); correlating by PCI bus id only (`AdapterInfo.
device_pci_bus_id` is populated from `VK_EXT_pci_bus_info` and is `""` when
unsupported — a fallback that fails silently is what spec 9.11 forbids).

`ash`'s `linked` feature is NOT enabled: it requires `vulkan-1.lib` from a Vulkan
SDK, and this machine has only the runtime loader (1.4.341.0). Default features
(`loaded`, `debug`, `std`) `libloading`-open the single process-wide
`vulkan-1.dll`.

### Pinning style

`wgpu`, `ash`, `windows`, `symphonia`, `rubato`, `hound` are `=`-pinned. They carry
native bindings or have recently shipped breaking minor releases, and an accidental
`cargo update -p wgpu` would invalidate a Vulkan probe that has already been
certified against a driver. Everything else uses caret so a semver-compatible
security fix can land without a manifest edit. `Cargo.lock` plus
`cargo test --locked` is what actually makes a build reproducible; the `=` pins
exist to stop the *lock* from being refreshed casually, not to duplicate it.

## Evidence

- Empty 13-crate workspace compiles: `cargo build --workspace` finished clean.
- `cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets --
  -D warnings` clean.
- Versions verified against `https://crates.io/api/v1/crates/<name>` on 2026-11-05.
- wgpu/ash API claims verified against published sources by a research pass
  (docs.rs 30.0.1 plus the downloaded crate tarball), not from recall.

## Consequences

1. **A toolchain change is a locked-dependency change.** Bumping `channel` means
   regenerating `Cargo.lock`, re-running the whole test command, and revalidating
   G0 — spec 9.12 says G0 cannot pass on a different adapter or driver without
   revalidation, and by the same argument not on a different compiler.
2. **`ash` must stay confined to `anim_test_support`.** It is a WP00 risk-reduction
   binding. When WP06 owns production GPU and DXGI code (spec 9.10), that crate
   re-decides the question with ADR-0002's evidence in hand.
3. **Formatter and lint baselines are stable-toolchain-only.** Six `rustfmt.toml`
   options (`wrap_comments`, `format_strings`, `match_arm_blocks`,
   `force_multiline_blocks`, `trailing_comma`, `blank_lines_upper_bound`) are
   nightly-only and are therefore *not* set; on stable they emit
   "unstable features are only available in nightly channel", which would make the
   `cargo fmt --check` gate noisy and the effective style undefined.
   `newline_style` is deliberately unset — pinning it to `Native` forces CRLF and
   then reports every LF file as "Incorrect newline style", which fights
   `core.autocrlf`.
4. **`clippy::doc_markdown` is live and enforced** by `all = "deny"`. Domain nouns
   in doc comments must be backticked (`` `FFmpeg` ``, `` `BUILD_STATE.md` ``). This
   was discovered by the gate itself on the first day, which is the intended
   behaviour.

## Schema/version impact

None. No persisted format is defined yet. `Cargo.lock` becomes authoritative from
this point forward (spec 4.1).
