# ADR-0002 — Vulkan adapter selection and LUID/DXGI correlation

Status: accepted
Date: 2026-11-05
Work package: WP00 (spec register: "ADR-0002 · Vulkan adapter selection and LUID/DXGI correlation · WP00")
Evidence: `docs/adr/evidence/wp00/wp00_gpu.json`, `docs/adr/evidence/wp00/wp00_adapter.json`

## Problem

The engine chooses a device through wgpu/Vulkan but has to size its memory policy
through DXGI. Those are two enumerations of the same hardware, performed by two APIs
with no shared handle type, on a laptop whose second GPU can disappear when the
panel is driven by the integrated adapter. WP07 needs to reserve and evict against a
budget. If that budget belongs to an adapter that is not the one drawing, every
number in the policy is about the wrong device.

Spec 9.7 fixes the method: compare `VkPhysicalDeviceIDProperties.deviceLUID` to
`IDXGIAdapter`'s `AdapterLuid`, compare **all 64 bits**, cross-check vendor/device
ids, and on mismatch return `IdentityUnavailable` — "never substitute the primary
desktop adapter."

## Alternatives considered

- **Take adapter ordinal 0.** Rejected on the facts: `enumerate_adapters` in wgpu 30
  returns raw driver order with no ranking, and on this machine the iGPU enumerates
  first. The probe's own selection rule exists because spec 9.6 says selection "never
  assumes the first adapter is the RTX."
- **Match on adapter name.** Rejected: the strings agree here ("NVIDIA GeForce RTX
  3050 A Laptop GPU" from both APIs), but names are driver-authored marketing text
  and can differ between Vulkan and DXGI for the same silicon.
- **Match on PCI bus id.** Rejected as the primary rule. `AdapterInfo.device_pci_bus_id`
  is populated from `VK_EXT_pci_bus_info` and is an empty string when the extension
  is absent — a fallback that degrades silently is what 9.11 forbids. Retained as
  secondary evidence.
- **Read the LUID through wgpu.** Impossible, and this is the finding that forced
  ADR-0001's `ash` addition: wgpu 30.0.1 never queries `VkPhysicalDeviceIDProperties`,
  `wgpu_hal::vulkan::Adapter`'s `raw: vk::PhysicalDevice` is private with no
  `as_raw()`, and `AdapterInfo` has no LUID field.

## Decision

Selection rule, fixed in `wp00_gpu.rs` and reused by `wp00_adapter.rs`:

1. An explicit `--adapter` ordinal addresses the enumeration the probe itself
   performed, and the resolved adapter's name and ids are recorded so the index is
   not treated as stable across boots.
2. Otherwise: `device_type == DiscreteGpu` **and** `vendor == 0x10DE`. Exactly one
   match is required; two matches abort rather than guess.
3. No match after bounded retries produces `IdentityUnavailable`. The iGPU is never
   substituted.

Correlation compares the LUID as a 64-bit value reconstructed the same way on both
sides, then cross-checks vendor and device ids. Budget is read from the *matched*
DXGI adapter only.

## Evidence

Measured on the RTX 3050 A reference machine, 2026-11-05:

| Observation | Value |
| --- | --- |
| Vulkan device | `NVIDIA GeForce RTX 3050 A Laptop GPU`, vendor `0x10DE`, device `0x28A3` |
| Vulkan `deviceLUIDValid` | true |
| Correlated DXGI adapter | index **1** (not 0), same name, same ids |
| LUID both sides | `0000000000011af8` |
| DXGI adapters enumerable | 3 |
| Vulkan api / driver version | `1.4.351` / `617.56.0` |
| Budget, local segment | 3308 MiB budget, 0 in use, 1751 MiB available for reservation |
| Budget, non-local segment | 7349 MiB budget, 0 in use, 3802 MiB available for reservation |
| Dedicated video memory (DXGI) | 3892 MiB |
| wgpu `max_buffer_size` | 1099511627776 (1 TiB) |

The `--force-unavailable` run (same evidence file, second invocation) discards the
Vulkan LUID and confirms the branch: exit 1, `adapter.identity_unavailable`, and the
report states that 3 DXGI adapters were enumerable but that matching by name or
taking the first would substitute an adapter the renderer did not choose. That is
spec 9.5 step 5's "test the unavailable-identity path", satisfied synthetically
because both real adapters here report valid LUIDs.

## Consequences

1. **Ordinal 0 is the wrong default on this machine.** Correlation landed on DXGI
   index 1. Anything later that assumes "first adapter" is wrong, including any
   script that picks an adapter to run a golden capture on.
2. **LUID is not stable across enumerations, so nothing may key on its value.**
   `BUILD_STATE.md` recorded NVIDIA `00cb5c07-00000000` from a 2026-11-04 probe; this
   run measured `0000000000011af8` on the same machine and the same driver, with no
   reboot between. Spec 9.12's "G0 cannot pass using reports from a different
   adapter/driver without revalidation" is therefore keyed on **PCI bus id
   (`0000:01:00.0`), adapter name, and driver version** — the three that held steady —
   and not on the LUID. A comparison written against the LUID would reject a legitimate
   rerun on identical hardware. See DECISIONS.md #18 for the full split between what
   LUID proves and what it cannot.
3. **The driver has two spellings.** Vulkan reports `617.56.0`; wgpu's `driver_info`
   and the Windows driver report `617.14`. Both are recorded. A revalidation check
   that compares only one will report a mismatch where there is none.
4. **DXGI `QueryVideoMemoryInfo` budgets are smaller than the physical card.** Local
   budget 3308 MiB against 3892 MiB dedicated. WP07 must size reservations against
   the budget, not the VRAM figure in the product brief.
5. **Node index 0 is an assumption, not a proof.** `IDXGIAdapter3` exposes no node
   count, so a future multi-node configuration would need D3D12's `GetNodeCount`.
   Recorded here so WP07 does not read "budget for node 0" as "budget for the
   adapter".
6. **`ash` stays in `anim_test_support` only.** When WP06 owns production GPU and
   DXGI code (spec 9.10), it re-decides whether the same raw-Vulkan dependency is
   acceptable in a production path, with this evidence in hand.
7. **Validation flags must stay cleared in probes.** `max_buffer_size` was readable
   as 1 TiB only because `InstanceFlags` was emptied; the default
   `VALIDATION_INDIRECT_CALL` clamps it to `u32::MAX`, which would have recorded a
   limit this driver never advertised.

## Schema/version impact

None. The LUID and budget figures appear only in probe reports, which are evidence
files, not persisted project or export formats.
