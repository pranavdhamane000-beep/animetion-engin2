#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "WP00 probe binary, not production code: a panic here fails the probe loudly rather than emitting a wrong golden, and the bounded sleep in the adapter-retry loop is the only permitted use spec'd by clippy.toml"
)]
// crates/anim_test_support/src/bin/wp00_adapter.rs
//! Vulkan `deviceLUID` to DXGI `AdapterLuid` correlation and video-memory budget
//! query (spec 9.3, 9.5 step 5, 9.7 "Adapter correlation").
//!
//! Why this exists as a separate probe: `wgpu` never queries
//! `VkPhysicalDeviceIDProperties` at all, so a render backend that can pick an
//! adapter cannot yet tell you *which* OS adapter it picked. WP07's memory budget
//! policy is driven by DXGI (`IDXGIAdapter3`), while the device that draws is
//! chosen through Vulkan. If those two are not proven to name the same silicon,
//! every budget decision is being made about a device that is not rendering.
//!
//! Spec 9.7: "Compare all 64 LUID bits and cross-check vendor/device IDs. Never
//! substitute the primary desktop adapter on mismatch."

use std::ffi::c_char;
use std::path::{Path, PathBuf};

use anim_core::diagnostic::{Diagnostic, DiagnosticCode};
use anim_test_support::probe::{finish, AdapterIdentity, ProbeError};

/// Bytes in a Windows LUID. Vulkan exposes it as a fixed 8-byte array
/// (`ash::vk::LUID_SIZE`), and DXGI exposes it as `LowPart`/`HighPart`.
const LUID_BYTES: usize = 8;
/// NVIDIA's PCI vendor id, which is what the selection rule filters on.
const NVIDIA_VENDOR: u32 = 0x10DE;

#[derive(Clone, Debug)]
struct Args {
    report_dir: PathBuf,
    adapter_attempts: u32,
    retry_delay_ms: u64,
    /// Force the Vulkan side to report its LUID as unavailable, so the
    /// `IdentityUnavailable` branch is exercised on a machine where both adapters
    /// legitimately have valid LUIDs.
    force_unavailable: bool,
}

/// One adapter as named by Vulkan.
#[derive(Clone, Debug)]
struct VulkanIdentity {
    name: String,
    vendor_id: u32,
    device_id: u32,
    device_luid: Option<[u8; LUID_BYTES]>,
    driver_version: Option<String>,
    api_version: Option<String>,
}

/// One adapter as named by DXGI.
#[derive(Clone, Debug)]
struct DxgiIdentity {
    description: String,
    vendor_id: u32,
    device_id: u32,
    luid: [u8; LUID_BYTES],
    dedicated_video_memory: u64,
    dedicated_system_memory: u64,
    shared_system_memory: u64,
    is_software_or_remote: bool,
}

/// One memory segment's budget as reported by `IDXGIAdapter3`.
#[derive(Clone, Debug)]
struct Budget {
    segment: &'static str,
    budget_bytes: u64,
    current_usage_bytes: u64,
    available_for_reservation_bytes: u64,
    current_reservation_bytes: u64,
}

fn main() -> std::process::ExitCode {
    let dir_on_failure = default_report_dir();
    let args = match Args::from_env() {
        Ok(a) => a,
        Err(e) => return finish("wp00_adapter", vec![e.to_diagnostic()], &dir_on_failure),
    };
    match run(&args) {
        Ok(diagnostics) => finish("wp00_adapter", diagnostics, &args.report_dir),
        Err(e) => finish("wp00_adapter", vec![e.to_diagnostic()], &args.report_dir),
    }
}

impl Args {
    fn from_env() -> Result<Self, ProbeError> {
        let mut out = Self {
            report_dir: default_report_dir(),
            adapter_attempts: 4,
            retry_delay_ms: 750,
            force_unavailable: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || -> Result<String, ProbeError> {
                it.next()
                    .ok_or_else(|| ProbeError::Config(format!("{flag} requires a value")))
            };
            match flag.as_str() {
                "--out" => out.report_dir = PathBuf::from(value()?),
                "--adapter-attempts" => {
                    out.adapter_attempts = parse(&value()?, "--adapter-attempts")?;
                }
                "--retry-delay-ms" => out.retry_delay_ms = parse(&value()?, "--retry-delay-ms")?,
                "--force-unavailable" => out.force_unavailable = true,
                other => {
                    return Err(ProbeError::Config(format!(
                        "unrecognized argument {other}; expected --out, \
                         --adapter-attempts, --retry-delay-ms or --force-unavailable"
                    )));
                }
            }
        }
        Ok(out)
    }
}

fn parse<T: std::str::FromStr>(raw: &str, name: &str) -> Result<T, ProbeError>
where
    T::Err: std::fmt::Display,
{
    raw.parse::<T>()
        .map_err(|e| ProbeError::Config(format!("{name} wants a number, got {raw}: {e}")))
}

fn default_report_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(|p| p.join("probe-out"))
        .unwrap_or_else(|| PathBuf::from("probe-out"))
}

fn code(raw: &str) -> DiagnosticCode {
    DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
}

/// LUID bytes to the canonical lowercase hex of the 64-bit value.
///
/// The sign trap is why this is a function and not a cast at the call site:
/// `LUID.HighPart` is an `i32`, so `high as u64` sign-extends and corrupts bits
/// 32-63 for any LUID whose high half has its top bit set. Vulkan's byte array is
/// the same little-endian layout, so both sides pass through here and the
/// comparison is genuinely over all 64 bits (spec 9.7).
fn luid_to_hex(bytes: &[u8; LUID_BYTES]) -> String {
    let low = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let high = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    format!("{:08x}{:08x}", high, low)
}

fn luid_from_windows(luid: &windows::Win32::Foundation::LUID) -> [u8; LUID_BYTES] {
    let mut b = [0u8; LUID_BYTES];
    b[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    // i32::to_le_bytes writes the same four bytes an u32 of the same bit pattern
    // would; the pattern is what identifies the adapter, not the signed reading.
    b[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    b
}

fn run(args: &Args) -> Result<Vec<Diagnostic>, ProbeError> {
    let mut diagnostics = Vec::new();

    // Retry across the hybrid-GPU power-gating gap (BUILD_STATE hazard 2): a
    // one-shot probe on this machine can otherwise conclude there is no discrete
    // GPU. The last attempt's observation is what gets reported.
    let mut attempts = Vec::new();
    let mut vulkan = None;
    for attempt in 1..=args.adapter_attempts.max(1) {
        match read_vulkan_identity()? {
            Some(identity) => {
                attempts.push(format!("attempt {attempt}: found {}", identity.name));
                vulkan = Some((identity, attempt));
                break;
            }
            None => attempts.push(format!("attempt {attempt}: no discrete NVIDIA device")),
        }
        if attempt < args.adapter_attempts {
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(std::time::Duration::from_millis(args.retry_delay_ms));
        }
    }
    let (mut vulkan, vk_attempt) = vulkan.ok_or_else(|| {
        ProbeError::IdentityUnavailable(format!(
            "no discrete NVIDIA Vulkan device after {} attempts ({}). Reporting the \
             absence rather than probing the iGPU, which spec 9.11 forbids.",
            args.adapter_attempts,
            attempts.join("; ")
        ))
    })?;

    diagnostics.push(Diagnostic::info(
        code("adapter.vulkan_side"),
        format!(
            "Vulkan reports {} (vendor {:04x} device {:04x}) on attempt {vk_attempt}; \
             deviceLUIDValid={}",
            vulkan.name,
            vulkan.vendor_id,
            vulkan.device_id,
            vulkan.device_luid.is_some()
        ),
    ));

    if args.force_unavailable {
        // Spec 9.5 step 5 requires testing the unavailable-identity path. Both
        // adapters here report a valid LUID, so the branch cannot be reached
        // naturally. Discarding the value exercises the same code, and the report
        // says it was synthetic rather than letting it read as a driver defect.
        vulkan.device_luid = None;
        diagnostics.push(Diagnostic::warning(
            code("adapter.identity_forced_unavailable"),
            "--force-unavailable discarded the Vulkan LUID to exercise the \
             IdentityUnavailable branch; this is a synthetic condition, not a \
             limitation of this machine",
        ));
    }

    let dxgi = read_dxgi_identities(&mut diagnostics)?;

    let Some(vk_luid) = vulkan.device_luid else {
        // Spec 9.7's second output. Reporting it is a legitimate probe result;
        // choosing an adapter anyway is the failure spec 9.11 forbids.
        diagnostics.push(Diagnostic::error(
            code("adapter.identity_unavailable"),
            format!(
                "Vulkan reported no valid deviceLUID for {}, so no DXGI adapter can be \
                 matched to it. {} DXGI adapter(s) were enumerable, but matching by name \
                 or taking the first would substitute an adapter the renderer did not \
                 choose (spec 9.7).",
                vulkan.name,
                dxgi.len()
            ),
        ));
        return Ok(diagnostics);
    };

    let vk_hex = luid_to_hex(&vk_luid);
    let matches: Vec<(usize, &DxgiIdentity)> = dxgi
        .iter()
        .enumerate()
        .filter(|(_, d)| luid_to_hex(&d.luid) == vk_hex)
        .collect();

    let (dxgi_index, dxgi_match) = match matches.len() {
        0 => {
            let listed: Vec<String> = dxgi
                .iter()
                .map(|d| format!("{} [luid {}]", d.description, luid_to_hex(&d.luid)))
                .collect();
            diagnostics.push(Diagnostic::error(
                code("adapter.luid_unmatched"),
                format!(
                    "Vulkan luid {vk_hex} for {} matched no DXGI adapter. DXGI reported: \
                     {}. The primary desktop adapter is deliberately not substituted.",
                    vulkan.name,
                    listed.join("; ")
                ),
            ));
            return Ok(diagnostics);
        }
        1 => (matches[0].0, matches[0].1),
        n => {
            return Err(ProbeError::Config(format!(
                "{n} DXGI adapters carry LUID {vk_hex}; identity is not unique, so \
                 correlation cannot be trusted"
            )));
        }
    };

    if dxgi_match.vendor_id != vulkan.vendor_id || dxgi_match.device_id != vulkan.device_id {
        // A LUID match with disagreeing ids means one API is reporting a stale or
        // fabricated identity. Correlation is not established, so it is not reported.
        diagnostics.push(Diagnostic::error(
            code("adapter.vendor_device_mismatch"),
            format!(
                "LUID {vk_hex} matched, but Vulkan says vendor {:04x} device {:04x} and \
                 DXGI says vendor {:04x} device {:04x}",
                vulkan.vendor_id, vulkan.device_id, dxgi_match.vendor_id, dxgi_match.device_id
            ),
        ));
        return Ok(diagnostics);
    }

    diagnostics.push(Diagnostic::info(
        code("adapter.correlated"),
        format!(
            "Vulkan {} and DXGI adapter #{dxgi_index} {} are the same silicon: luid \
             {vk_hex}, vendor {:04x}, device {:04x}",
            vulkan.name, dxgi_match.description, vulkan.vendor_id, vulkan.device_id
        ),
    ));
    diagnostics.push(
        Diagnostic::info(
            code("adapter.vulkan_versions"),
            format!(
                "api_version {:?}, driver_version {:?}",
                vulkan.api_version, vulkan.driver_version
            ),
        )
        .with_context("luid", vk_hex.clone()),
    );

    // Budget telemetry comes from the matched adapter only.
    let budgets = query_budgets(dxgi_index)?;
    for b in &budgets {
        let d = Diagnostic::info(
            code("adapter.budget"),
            format!(
                "{} budget {} MiB, current usage {} MiB, available for reservation \
                 {} MiB, current reservation {} MiB",
                b.segment,
                b.budget_bytes / 1048576,
                b.current_usage_bytes / 1048576,
                b.available_for_reservation_bytes / 1048576,
                b.current_reservation_bytes / 1048576
            ),
        )
        .with_context("segment", b.segment);
        diagnostics.push(d);
    }
    if budgets.is_empty() {
        diagnostics.push(Diagnostic::warning(
            code("adapter.budget_unavailable"),
            "IDXGIAdapter3::QueryVideoMemoryInfo returned nothing for the matched \
             adapter, so WP07 would have to run without budget telemetry",
        ));
    }

    let identity = AdapterIdentity {
        name: dxgi_match.description.clone(),
        vendor_id: dxgi_match.vendor_id,
        device_id: dxgi_match.device_id,
        luid: Some(vk_hex.clone()),
        // ash's PCI bus info is a separate extension (VK_EXT_pci_bus_info) and this
        // probe correlates by LUID, so the field stays empty rather than being
        // filled from the DXGI description, which does not contain a bus id.
        device_pci_bus_id: None,
        driver: vulkan.driver_version.clone(),
        driver_info: vulkan.api_version.clone(),
    };
    diagnostics.push(
        Diagnostic::info(
            code("adapter.identity_recorded"),
            format!("AdapterIdentity: {identity:?}"),
        )
        .with_context("luid", vk_hex),
    );

    diagnostics.push(Diagnostic::info(
        code("adapter.dedicated_memory"),
        format!(
            "DXGI dedicated video memory {} MiB, dedicated system {} MiB, shared system \
             {} MiB on the correlated adapter",
            dxgi_match.dedicated_video_memory / 1048576,
            dxgi_match.dedicated_system_memory / 1048576,
            dxgi_match.shared_system_memory / 1048576
        ),
    ));
    if dxgi_match.is_software_or_remote {
        diagnostics.push(Diagnostic::warning(
            code("adapter.software_or_remote"),
            "the matched DXGI adapter is flagged software or remote, so it is not \
             physical GPU memory and cannot be the rendering device",
        ));
    }

    Ok(diagnostics)
}

/// Vulkan side: the physical device wp00_gpu would select, plus its LUID.
///
/// One device is chosen by the same rule as wp00_gpu (discrete NVIDIA) because the
/// question WP07 asks is "which DXGI adapter is the one wgpu will draw on", not
/// "what is on the bus". `None` means no discrete NVIDIA device was enumerated -
/// the phantom-power-gating case - which the caller retries rather than resolves
/// against the iGPU.
fn read_vulkan_identity() -> Result<Option<VulkanIdentity>, ProbeError> {
    // ash's default features use `loaded`, so this dlopen's the one vulkan-1.dll
    // already mapped into the process by wgpu rather than linking a second loader.
    let entry = unsafe { ash::Entry::load() }.map_err(|e| {
        ProbeError::IdentityUnavailable(format!("could not open the Vulkan loader: {e}"))
    })?;

    // api_version 1.1 is what makes get_physical_device_properties2 a core entry
    // point instead of requiring VK_KHR_get_physical_device_properties2.
    let app_info = ash::vk::ApplicationInfo::default()
        .application_name(c"wp00_adapter")
        .application_version(1)
        .engine_name(c"animation_engine")
        .engine_version(1)
        .api_version(ash::vk::make_api_version(0, 1, 1, 0));
    let create = ash::vk::InstanceCreateInfo::default().application_info(&app_info);
    let instance = unsafe { entry.create_instance(&create, None) }
        .map_err(|e| ProbeError::IdentityUnavailable(format!("vkCreateInstance: {e}")))?;

    let result = read_from_instance(&instance);
    // Destroyed on every path, including the error paths below: spec 9.8 requires
    // native handles released on every return path.
    unsafe { instance.destroy_instance(None) };
    result
}

fn read_from_instance(instance: &ash::Instance) -> Result<Option<VulkanIdentity>, ProbeError> {
    let devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| ProbeError::IdentityUnavailable(format!("vkEnumeratePhysicalDevices: {e}")))?;
    if devices.is_empty() {
        return Ok(None);
    }

    for device in &devices {
        let base = unsafe { instance.get_physical_device_properties(*device) };
        if base.vendor_id != NVIDIA_VENDOR
            || base.device_type != ash::vk::PhysicalDeviceType::DISCRETE_GPU
        {
            continue;
        }

        let mut id_props = ash::vk::PhysicalDeviceIDProperties::default();
        let mut props2 = ash::vk::PhysicalDeviceProperties2::default().push_next(&mut id_props);
        // The pNext chain is what makes this a 1.1 query: the driver only fills
        // device_luid when the structure is reachable through properties2.
        unsafe { instance.get_physical_device_properties2(*device, &mut props2) };

        return Ok(Some(VulkanIdentity {
            name: c_array_to_string(&base.device_name),
            vendor_id: base.vendor_id,
            device_id: base.device_id,
            // Bool32 is a u32 in ash 0.38, not an enum, so it compares to vk::TRUE.
            device_luid: (id_props.device_luid_valid == ash::vk::TRUE)
                .then_some(id_props.device_luid),
            driver_version: Some(split_version(base.driver_version)),
            api_version: Some(split_version(base.api_version)),
        }));
    }
    Ok(None)
}

/// Vulkan packs a version into variant/major/minor/patch bits.
fn split_version(raw: u32) -> String {
    format!(
        "{}.{}.{}",
        (raw >> 22) & 0x3FF,
        (raw >> 12) & 0x3FF,
        raw & 0xFFF
    )
}

fn c_array_to_string(raw: &[c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn read_dxgi_identities(
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<DxgiIdentity>, ProbeError> {
    use windows::Win32::Graphics::Dxgi::*;

    // CreateDXGIFactory1 takes no arguments and needs no COM initialisation.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| {
        ProbeError::IdentityUnavailable(format!("CreateDXGIFactory1: {}", e.message()))
    })?;

    let mut out = Vec::new();
    let mut index = 0u32;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => {
                return Err(ProbeError::IdentityUnavailable(format!(
                    "EnumAdapters1({index}): {}",
                    e.message()
                )));
            }
        };
        let desc = unsafe { adapter.GetDesc1() }.map_err(|e| {
            ProbeError::IdentityUnavailable(format!("GetDesc1({index}): {}", e.message()))
        })?;
        // DESC1 reports Flags as u32 while the flag newtype wraps i32. The bits are
        // the adapter's own, so the reinterpretation is exact for the two flags read
        // here - SOFTWARE (1) and REMOTE (2) never reach the sign bit.
        let flags = DXGI_ADAPTER_FLAG(desc.Flags as i32);
        out.push(DxgiIdentity {
            description: wide_to_string(&desc.Description),
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
            luid: luid_from_windows(&desc.AdapterLuid),
            // DESC1 has no shared-video-memory field at all: the members are
            // DedicatedVideoMemory, DedicatedSystemMemory and SharedSystemMemory.
            // Treating the last as "shared video" would overstate what WP07 can
            // evict, so the names are kept as DXGI states them.
            dedicated_video_memory: desc.DedicatedVideoMemory as u64,
            dedicated_system_memory: desc.DedicatedSystemMemory as u64,
            shared_system_memory: desc.SharedSystemMemory as u64,
            is_software_or_remote: flags.contains(DXGI_ADAPTER_FLAG_SOFTWARE)
                || flags.contains(DXGI_ADAPTER_FLAG_REMOTE),
        });
        index += 1;
        if index > 16 {
            diagnostics.push(Diagnostic::warning(
                code("adapter.enumeration_suspect"),
                "more than 16 DXGI adapters enumerated; stopping the walk",
            ));
            break;
        }
    }
    if out.is_empty() {
        return Err(ProbeError::IdentityUnavailable(
            "DXGI enumerated zero adapters, which alongside a working Vulkan loader \
             means the DXGI path itself is unavailable"
                .to_owned(),
        ));
    }
    Ok(out)
}

/// Budget for both memory segments of the adapter at `matched_index`.
///
/// An empty `Vec` is the honest answer for a runtime without DXGI 1.4:
/// `IDXGIAdapter3` then fails the cast with `E_NOINTERFACE`, and WP07 has to know
/// that telemetry is absent rather than see a zero budget.
fn query_budgets(matched_index: usize) -> Result<Vec<Budget>, ProbeError> {
    use windows::core::Interface;
    use windows::Win32::Foundation::E_NOINTERFACE;
    use windows::Win32::Graphics::Dxgi::*;

    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|e| ProbeError::IdentityUnavailable(format!("factory: {}", e.message())))?;
    let adapter = unsafe { factory.EnumAdapters1(matched_index as u32) }.map_err(|e| {
        ProbeError::IdentityUnavailable(format!("EnumAdapters1({matched_index}): {}", e.message()))
    })?;
    // Interface::cast is a safe fn in windows 0.62: the COM call behind it can only
    // report whether the object supports the requested interface id.
    let adapter3: IDXGIAdapter3 = match adapter.cast() {
        Ok(a3) => a3,
        Err(e) if e.code() == E_NOINTERFACE => return Ok(Vec::new()),
        Err(e) => {
            return Err(ProbeError::IdentityUnavailable(format!(
                "cast to IDXGIAdapter3: {}",
                e.message()
            )));
        }
    };

    let mut out = Vec::new();
    // Node index 0, which is what Microsoft specifies for single-GPU operation.
    // IDXGIAdapter3 exposes no node count of its own, so a multi-node adapter would
    // need D3D12's GetNodeCount; recorded so WP07 does not read "budget for node 0"
    // as "budget for the whole adapter".
    for (name, segment) in [
        ("local", DXGI_MEMORY_SEGMENT_GROUP_LOCAL),
        ("non_local", DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL),
    ] {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        unsafe { adapter3.QueryVideoMemoryInfo(0, segment, &mut info) }.map_err(|e| {
            ProbeError::IdentityUnavailable(format!(
                "QueryVideoMemoryInfo({name}): {}",
                e.message()
            ))
        })?;
        out.push(Budget {
            segment: name,
            budget_bytes: info.Budget,
            current_usage_bytes: info.CurrentUsage,
            available_for_reservation_bytes: info.AvailableForReservation,
            current_reservation_bytes: info.CurrentReservation,
        });
    }
    Ok(out)
}

fn wide_to_string(raw: &[u16]) -> String {
    let end = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}
