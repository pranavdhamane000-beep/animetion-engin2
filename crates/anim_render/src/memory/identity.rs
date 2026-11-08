//! Vulkan/DXGI LUID correlation and segment selection (spec 16.3, 16.6, 16.8).
//!
//! # Why a second VkInstance exists here, once per process
//!
//! wgpu 30 exposes no LUID (deviation row 3's finding), and `DXGI_ADAPTER_DESC1` has no
//! PCI field (#18 decision 4), so pointing a memory budget at the right card needs the
//! one value both APIs share: the LUID. DECISIONS.md #18 decision 4 confines this to
//! budget attribution, once per process, gated on `dxgi-telemetry`; #43 ruling 1 chose
//! it over a static budget. `device.rs`'s PCI anchor stays ash-free — this module is the
//! narrow half ADR-0002 consequence 6 reopened.
//!
//! The call sequence is the one `wp00_adapter` proved on this machine: an Vulkan 1.1
//! instance (properties2 core, no extensions, no layers), enumerate, match vendor and
//! device id, chain `PhysicalDeviceIDProperties`, read `deviceLUID`, destroy. On the
//! DXGI side: `CreateDXGIFactory1`, `EnumAdapters1`, `GetDesc1`, normalize both LUIDs to
//! the same eight bytes, require **exactly one** match, then cast to `IDXGIAdapter3`.
//!
//! # Nothing here is a correctness check
//!
//! A LUID match passes on the iGPU if the OS handed the process the iGPU (#18). The
//! correctness half — PCI bus id, name, driver, VRAM floor — already ran in
//! `device.rs` before this module is asked anything. Correlation only answers "are
//! Vulkan and DXGI naming the same silicon", so a failure degrades to internal
//! accounting (16.6) rather than refusing the frame.

#[cfg(feature = "dxgi-telemetry")]
use super::MemoryError;

/// The correlated adapter's identity (spec 16.8). No DXGI pointer appears in this
/// record or any public type (16.4); the COM handle lives privately in [`super::dxgi`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterIdentity {
    /// The LUID both APIs agreed on, as the normalized 64-bit value.
    pub luid: i64,
    /// DXGI's adapter description, NUL-trimmed.
    pub description: String,
    /// PCI vendor id — cross-checked against the Vulkan properties, not trusted alone.
    pub vendor_id: u32,
    /// PCI device id.
    pub device_id: u32,
    /// `DXGI_ADAPTER_DESC1::DedicatedVideoMemory`. The *budget* is smaller
    /// (ADR-0002 consequence 4); this figure only feeds the unavailable-path estimate.
    pub dedicated_video_memory: u64,
}

/// Normalize a Windows `LUID` (low `DWORD`, high `LONG`) to the eight bytes Vulkan's
/// `deviceLUID` carries: low part little-endian, then high part little-endian. The
/// `i32` high part's sign is the trap the WP00 probe documented — `to_le_bytes` on the
/// signed value is what makes a negative high part compare equal to itself.
#[cfg(feature = "dxgi-telemetry")]
pub(crate) fn luid_bytes(low_part: u32, high_part: i32) -> [u8; 8] {
    let mut bytes = [0_u8; 8];
    bytes[..4].copy_from_slice(&low_part.to_le_bytes());
    bytes[4..].copy_from_slice(&high_part.to_le_bytes());
    bytes
}

/// Read the Vulkan LUID of the adapter matching `vendor_id`/`device_id`.
///
/// The instance is destroyed on every path. No extensions, layers or features are
/// enabled — the probe proved that is sufficient for `deviceLUID` on this driver.
#[cfg(feature = "dxgi-telemetry")]
pub(crate) fn read_vulkan_luid(vendor_id: u32, device_id: u32) -> Result<[u8; 8], MemoryError> {
    use ash::vk;

    let fail = |detail: String| MemoryError::QueryFailed { detail };
    // SAFETY: `Entry::load` dlopens the same vulkan-1.dll wgpu already mapped; the
    // workspace forbids the `linked` feature, so no SDK import library is involved.
    let entry = unsafe { ash::Entry::load() }.map_err(|e| fail(format!("Vulkan loader: {e}")))?;
    let app_info = vk::ApplicationInfo {
        api_version: vk::make_api_version(0, 1, 1, 0),
        ..Default::default()
    };
    let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
    // SAFETY: create_info references app_info which outlives the call; no invalid
    // pointers or extensions are passed.
    let instance = unsafe { entry.create_instance(&create_info, None) }
        .map_err(|e| fail(format!("correlation VkInstance: {e}")))?;

    let result = (|| {
        // SAFETY: the instance is live and owns the enumeration.
        let physical = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| fail(format!("enumerate_physical_devices: {e}")))?;
        let mut found: Option<[u8; 8]> = None;
        for device in physical {
            // SAFETY: device is from this instance's enumeration.
            let props = unsafe { instance.get_physical_device_properties(device) };
            if props.vendor_id != vendor_id || props.device_id != device_id {
                continue;
            }
            let mut id_props = vk::PhysicalDeviceIDProperties::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id_props);
            // SAFETY: props2 chains id_props, both live for the call; the device
            // belongs to this instance.
            unsafe { instance.get_physical_device_properties2(device, &mut props2) };
            if id_props.device_luid_valid != vk::TRUE {
                return Err(fail(format!(
                    "adapter {vendor_id:#06x}/{device_id:#06x} reports deviceLUIDValid=false"
                )));
            }
            if found.is_some() {
                return Err(fail(format!(
                    "two Vulkan adapters match {vendor_id:#06x}/{device_id:#06x}; correlation is ambiguous"
                )));
            }
            found = Some(id_props.device_luid);
        }
        found.ok_or_else(|| {
            fail(format!(
                "no Vulkan adapter matches {vendor_id:#06x}/{device_id:#06x}"
            ))
        })
    })();

    // SAFETY: the instance was created above and is destroyed exactly once, on every
    // path through the closure.
    unsafe { instance.destroy_instance(None) };
    result
}

/// Find the one DXGI adapter whose LUID and vendor/device ids match the Vulkan side,
/// and return its identity plus the `IDXGIAdapter3` the budget query needs.
///
/// The COM handle in the return is `pub(crate)` — internal plumbing to [`super::dxgi`],
/// not a public record (16.4). Exactly one match is required: zero means the APIs
/// disagree, two means the compare cannot attribute a budget, and both degrade to
/// telemetry-unavailable rather than guessing (16.6: "never query an unrelated
/// adapter").
#[cfg(feature = "dxgi-telemetry")]
pub(crate) fn correlate_dxgi(
    vk_luid: &[u8; 8],
    vendor_id: u32,
    device_id: u32,
) -> Result<
    (
        AdapterIdentity,
        windows::Win32::Graphics::Dxgi::IDXGIAdapter3,
    ),
    MemoryError,
> {
    use windows::core::Interface as _;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory1, DXGI_ERROR_NOT_FOUND,
    };

    let fail = |detail: String| MemoryError::QueryFailed { detail };
    // SAFETY: CreateDXGIFactory1 takes no pointers and returns a ref-counted factory.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| fail(format!("CreateDXGIFactory1: {e}")))?;
    let mut matched: Option<(AdapterIdentity, IDXGIAdapter3)> = None;
    for index in 0..16_u32 {
        // SAFETY: the factory is live; EnumAdapters1 returns a ref-counted adapter or
        // DXGI_ERROR_NOT_FOUND, which ends the enumeration.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(fail(format!("EnumAdapters1({index}): {e}"))),
        };
        // SAFETY: the adapter is live; GetDesc1 fills a plain struct.
        let desc =
            unsafe { adapter.GetDesc1() }.map_err(|e| fail(format!("GetDesc1({index}): {e}")))?;
        if desc.VendorId != vendor_id || desc.DeviceId != device_id {
            continue;
        }
        let bytes = luid_bytes(desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart);
        if &bytes != vk_luid {
            continue;
        }
        // SAFETY: `cast` is windows-rs's safe QueryInterface wrapper; E_NOINTERFACE
        // means the adapter predates IDXGIAdapter3 and telemetry is unavailable.
        let adapter3 = adapter
            .cast::<IDXGIAdapter3>()
            .map_err(|e| fail(format!("IDXGIAdapter3 unsupported on matched adapter: {e}")))?;
        let description = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_owned();
        let identity = AdapterIdentity {
            luid: i64::from_le_bytes(bytes),
            description,
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
            dedicated_video_memory: desc.DedicatedVideoMemory as u64,
        };
        if matched.is_some() {
            return Err(fail(
                "two DXGI adapters match the Vulkan LUID; budget attribution is ambiguous"
                    .to_owned(),
            ));
        }
        matched = Some((identity, adapter3));
    }
    matched.ok_or_else(|| {
        fail(format!(
            "no DXGI adapter matches Vulkan LUID for {vendor_id:#06x}/{device_id:#06x}"
        ))
    })
}

/// Run the whole correlation once: Vulkan LUID in, correlated DXGI adapter out.
#[cfg(feature = "dxgi-telemetry")]
pub(crate) fn correlate(
    vendor_id: u32,
    device_id: u32,
) -> Result<
    (
        AdapterIdentity,
        windows::Win32::Graphics::Dxgi::IDXGIAdapter3,
    ),
    MemoryError,
> {
    let luid = read_vulkan_luid(vendor_id, device_id)?;
    correlate_dxgi(&luid, vendor_id, device_id)
}

#[cfg(all(test, feature = "dxgi-telemetry"))]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    #[test]
    fn a_negative_high_part_normalizes_to_its_own_bytes() {
        let bytes = luid_bytes(0x0001_1af8, -1);
        assert_eq!(bytes[..4], [0xf8, 0x1a, 0x01, 0x00]);
        assert_eq!(bytes[4..], [0xff, 0xff, 0xff, 0xff]);
        assert_eq!(i64::from_le_bytes(bytes), 0xffff_ffff_0001_1af8_u64 as i64);
    }

    #[test]
    fn the_luid_layout_is_low_then_high_little_endian() {
        let bytes = luid_bytes(0x0102_0304, 0x0506_0708);
        assert_eq!(bytes, [0x04, 0x03, 0x02, 0x01, 0x08, 0x07, 0x06, 0x05]);
    }

    /// The correlation the WP00 probe proved, run against the certified adapter.
    /// Refuses on any other machine rather than skipping — spec 9.12's rule, the same
    /// one the golden suite enforces.
    #[test]
    fn the_certified_adapter_correlates_exactly_once() {
        let (identity, _adapter3) = correlate(0x10DE, 0x28A3).expect(
            "the pinned RTX 3050 A must correlate; on other hardware this test is a refusal, not a pass",
        );
        assert_eq!(identity.vendor_id, 0x10DE);
        assert_eq!(identity.device_id, 0x28A3);
        assert!(
            identity.description.contains("NVIDIA"),
            "{}",
            identity.description
        );
        assert!(identity.dedicated_video_memory > 0);
        assert_ne!(identity.luid, 0);
    }

    #[test]
    fn an_adapter_that_does_not_exist_degrades_to_a_named_refusal() {
        let err = correlate(0xDEAD, 0xBEEF).unwrap_err();
        assert!(matches!(err, MemoryError::QueryFailed { .. }));
        assert!(err.to_string().contains("0xdead"), "{err}");
    }
}
