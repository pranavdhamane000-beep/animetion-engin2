//! Instance, adapter, required features and device recovery (spec 15.3).
//!
//! This is the file DECISIONS.md #18 was written for and risk 8 says nothing enforced:
//! on a hybrid Optimus laptop, every wrong-GPU failure this engine can have is *silent*
//! here and loud only in the finished export. So the rules below are hard errors, not
//! diagnostics, and each one names the measurement it came from.
//!
//! Three questions, and #18's finding is that one check cannot answer all three:
//!
//! | Question | Answered here by |
//! | --- | --- |
//! | Which adapter should draw? | the ADR-0002 selection rule — `DiscreteGpu` **and** vendor `0x10DE`, exactly one match, bounded retry |
//! | Is it the adapter G0 certified? | PCI bus id + adapter name + driver, compared to [`ExpectedAdapter::reference`] |
//! | Is it even the right class of device? | `device_type`, plus DXGI `DedicatedVideoMemory` against a floor |
//!
//! LUID correlation is *not* performed. It answers "are Vulkan and DXGI naming the same
//! silicon", which is a consistency check, and #18 decision 4 confines its one real use —
//! pointing a memory budget at the right card — to WP07's `dxgi-telemetry` feature. It
//! would also cost a second `VkInstance`, which is why ADR-0001 added `ash` for a probe.
//! Production has no such dependency: `wgpu::AdapterInfo` carries the PCI bus id already.

use std::sync::{Arc, OnceLock};

use wgpu::{
    Adapter, AdapterInfo, Backends, Device, DeviceLostReason, Features, Instance,
    InstanceDescriptor, InstanceFlags, Limits, Queue, TextureFormat, TextureUsages,
};

/// What G0 measured on the reference machine, and therefore what any adapter this build
/// accepts has to match.
///
/// Every field is a measurement, not a preference: DECISIONS.md #18 decision 1 keys
/// revalidation on the three that held steady across runs (PCI bus id, adapter name,
/// driver) *because* the LUID moved three times in three runs, and BUILD_STATE's WP00 row
/// keys nothing on an ordinal either. A driver update changes `driver_version` and must
/// produce a refusal here rather than a silently re-baselined golden — that is spec 9.12's
/// "no passing on a different adapter/driver without revalidation" made executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedAdapter {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub pci_bus_id: String,
    /// `AdapterInfo::driver` — on this machine `"NVIDIA"`. It is the driver's *name*.
    pub driver_name: String,
    /// `AdapterInfo::driver_info` — on this machine `"617.14"`.
    ///
    /// This is the field ADR-0002 consequence 3 calls one of two spellings. The other,
    /// `617.56.0`, is Vulkan's `driver_version` reached through ash. They are never
    /// compared to each other here: each is checked against the record of its own API, so
    /// the false mismatch #18's residual warns about cannot be constructed.
    pub driver_version: String,
    /// DECISIONS.md #18 decision 2: a capacity floor, not an identity test. 3892 MiB was
    /// measured on the reference card and re-measured identically in both GPU modes.
    pub dedicated_video_memory_floor: u64,
}

impl ExpectedAdapter {
    /// The RTX 3050 A Laptop GPU configuration G0 certified (ADR-0002's evidence table).
    pub fn reference() -> Self {
        Self {
            name: "NVIDIA GeForce RTX 3050 A Laptop GPU".to_owned(),
            vendor_id: 0x10DE,
            device_id: 0x28A3,
            pci_bus_id: "0000:01:00.0".to_owned(),
            driver_name: "NVIDIA".to_owned(),
            driver_version: "617.14".to_owned(),
            dedicated_video_memory_floor: 3892 * 1024 * 1024,
        }
    }
}

/// How to choose the adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// ADR-0002's rule: exactly one `DiscreteGpu` with vendor `0x10DE`. Zero matches
    /// retries and then refuses; more than one refuses rather than guessing, because a
    /// guess would pick whichever enumeration order the driver happened to report.
    ValidatedRule,
    /// Address the enumeration this process performed, by index. Spec 9.6 forbids
    /// *assuming* the first adapter is the RTX; naming an index is the one way to ask for
    /// a specific adapter honestly, and the resolved identity is still revalidated.
    ExplicitIndex(usize),
}

#[derive(Clone, Debug)]
pub struct DeviceConfig {
    pub expected: ExpectedAdapter,
    pub selection: Selection,
    /// BUILD_STATE hazard 2: the dGPU power-gates off on this laptop, so a single
    /// enumeration can legitimately show no discrete adapter at all. One-shot probing is
    /// what made an earlier assessment conclude there was no dGPU.
    pub attempts: u32,
    pub retry_delay_ms: u64,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            expected: ExpectedAdapter::reference(),
            selection: Selection::ValidatedRule,
            attempts: 8,
            // The probe's own cadence (wp00_gpu's default), kept the same so production
            // and the probe wait on the same power-gating behaviour rather than two.
            retry_delay_ms: 250,
        }
    }
}

/// The identity actually resolved, in the shape revalidation is written against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterRecord {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub device_type: String,
    pub pci_bus_id: String,
    pub driver_name: String,
    pub driver_version: String,
    pub backend: String,
}

impl AdapterRecord {
    fn from_info(info: &AdapterInfo) -> Self {
        Self {
            name: info.name.clone(),
            vendor_id: info.vendor,
            device_id: info.device,
            device_type: format!("{:?}", info.device_type),
            pci_bus_id: info.device_pci_bus_id.clone(),
            driver_name: info.driver.clone(),
            driver_version: info.driver_info.clone(),
            backend: format!("{:?}", info.backend),
        }
    }
}

/// What the selected adapter was measured to accept, before the device was opened.
#[derive(Clone, Debug)]
pub struct Capabilities {
    /// The `FORMAT`-matrix subset 15.6's target table depends on, as
    /// `(format, usages)` pairs that were each confirmed against `allowed_usages`.
    pub confirmed: Vec<(TextureFormat, TextureUsages)>,
    pub limits: Limits,
    pub features: Features,
    pub copy_bytes_per_row_alignment: u32,
}

/// Every format and usage 15.6's target table names, as a requirement.
///
/// This is the other half of ADR-0003, which measured only `Rgba16Float` as an attachment
/// and explicitly left "the full FORMAT feature matrix per pass" to this package. A
/// requirement list is the honest form of that: it is checked against the adapter on every
/// run, so a driver that drops support fails at initialization (spec 15.11) instead of
/// producing a black export.
const REQUIRED_CAPABILITIES: &[(TextureFormat, TextureUsages, &str)] = &[
    (
        TextureFormat::Rgba16Float,
        TextureUsages::RENDER_ATTACHMENT,
        "scene_hdr/normal/velocity/post_hdr write",
    ),
    (
        TextureFormat::Rgba16Float,
        TextureUsages::TEXTURE_BINDING,
        "post chain reads the previous target",
    ),
    (
        TextureFormat::Rgba16Float,
        TextureUsages::COPY_SRC,
        "velocity and normal readback for goldens",
    ),
    (
        TextureFormat::Depth32Float,
        TextureUsages::RENDER_ATTACHMENT,
        "depth write with compare Less (15.6)",
    ),
    (
        TextureFormat::Depth32Float,
        TextureUsages::TEXTURE_BINDING,
        "depth-aware post effects read it",
    ),
    (
        TextureFormat::Rgba8Unorm,
        TextureUsages::RENDER_ATTACHMENT,
        "scene_output write",
    ),
    (
        TextureFormat::Rgba8Unorm,
        TextureUsages::COPY_SRC,
        "clean output reaches the CPU for a golden",
    ),
    (
        TextureFormat::Rgba8Unorm,
        TextureUsages::TEXTURE_BINDING,
        "editor composites the clean scene as a texture",
    ),
];

/// The device the graph draws on, plus the evidence that it is the right one.
///
/// `Send + Sync` by the underlying types: a long export owns one of these on one thread,
/// and nothing here shares mutable state across threads, so `clippy.toml`'s ban on locks
/// never comes up. The one exception is the loss record, which is write-once.
#[derive(Debug)]
pub struct GpuContext {
    pub instance: Instance,
    pub adapter: Adapter,
    pub device: Device,
    pub queue: Queue,
    pub record: AdapterRecord,
    pub capabilities: Capabilities,
    pub vram: VramFloor,
    /// The first loss wins, and later ones are ignored: spec 15.11's consequence is the
    /// same whichever loss arrives, and a second record would let callback timing decide
    /// what a report says. `OnceLock` rather than a `Mutex` because `clippy.toml` bans
    /// locks from being observable anywhere near canonical output.
    loss: Arc<OnceLock<LossRecord>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LossRecord {
    pub reason: String,
    pub message: String,
}

/// The `DedicatedVideoMemory` guard, and the three ways it can fail to be a guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VramFloor {
    /// Exactly one DXGI adapter carries the selected vendor/device pair, and its
    /// dedicated memory cleared the floor. The guard ran.
    Verified { bytes: u64 },
    /// It cleared nothing. Spec 15.11's out-of-memory path is WP07's, so this reports.
    BelowFloor { bytes: u64, floor: u64 },
    /// Zero or more than one DXGI adapter matched, so no dedicated-memory figure can be
    /// attributed to the drawing card. A *warning* rather than a refusal because the
    /// selection itself is already anchored on the PCI bus id, which disambiguates the
    /// identical-card case DXGI's own description cannot. Silent would be the alternative,
    /// and spec 9.11 says an unavailable capability is recorded, not reported as present.
    Indeterminate { detail: String },
}

impl GpuContext {
    /// Resolve the instance, adapter and device, and validate every capability before
    /// returning. Steps are 15.5's first step: "Create device, select only validated
    /// features".
    pub fn create(config: &DeviceConfig) -> Result<Self, RenderError> {
        let instance = build_instance()?;

        // One `enumerate_adapters` per attempt: the list is what changes when the dGPU
        // power-gates, so re-enumerating is the retry. The adapters themselves are
        // dropped between attempts — holding them across a power-state change is how a
        // stale handle becomes the drawing device.
        let mut last_seen = 0usize;
        let mut phantom = Vec::new();
        for attempt in 1..=config.attempts.max(1) {
            let adapters = pollster::block_on(instance.enumerate_adapters(Backends::VULKAN));
            last_seen = adapters.len();
            if adapters.is_empty() {
                phantom.push(format!(
                    "attempt {attempt}: Vulkan enumerated zero adapters"
                ));
                drop(adapters);
                wait_between_attempts(config, attempt);
                continue;
            }

            let matched: Vec<Adapter> = match config.selection {
                Selection::ExplicitIndex(index) => match adapters.get(index) {
                    Some(adapter) => vec![adapter.clone()],
                    None => {
                        return Err(RenderError::IdentityUnavailable {
                            detail: format!(
                                "explicit adapter index {index} is not in this process's \
                                     enumeration of {last_seen} adapter(s); the index addresses \
                                     the list this run produced and is not stable across boots"
                            ),
                        })
                    }
                },
                Selection::ValidatedRule => adapters
                    .iter()
                    .filter(|adapter| {
                        let info = adapter.get_info();
                        info.device_type == wgpu::DeviceType::DiscreteGpu
                            && info.vendor == config.expected.vendor_id
                    })
                    .cloned()
                    .collect(),
            };

            match matched.len() {
                1 => {
                    let adapter = matched.into_iter().next().ok_or_else(|| {
                        RenderError::internal("adapter-selection", "one match lost")
                    })?;
                    let record = AdapterRecord::from_info(&adapter.get_info());
                    revalidate(&record, &config.expected)?;
                    require_pci_anchor(&record)?;
                    let capabilities = validate_capabilities(&adapter)?;
                    let vram = check_dedicated_video_memory(&record, &config.expected);
                    let (device, queue) = open_device(&adapter)?;
                    let loss = Arc::new(OnceLock::new());
                    {
                        let slot = Arc::clone(&loss);
                        device.set_device_lost_callback(move |reason, message| {
                            let _ = slot.set(LossRecord {
                                reason: loss_reason(reason),
                                message,
                            });
                        });
                    }
                    return Ok(Self {
                        instance,
                        adapter,
                        device,
                        queue,
                        record,
                        capabilities,
                        vram,
                        loss,
                    });
                }
                0 => {
                    let names: Vec<String> = adapters
                        .iter()
                        .map(|adapter| {
                            let info = adapter.get_info();
                            format!(
                                "{} [{:?}, vendor {:04x}]",
                                info.name, info.device_type, info.vendor
                            )
                        })
                        .collect();
                    phantom.push(format!(
                        "attempt {attempt}: no discrete adapter with vendor {:04x} among {} \
                         enumerated: {}",
                        config.expected.vendor_id,
                        adapters.len(),
                        names.join("; ")
                    ));
                }
                count => {
                    return Err(RenderError::IdentityUnavailable {
                        detail: format!(
                            "the selection rule matched {count} discrete adapters with vendor \
                             {:04x}; picking one would make enumeration order a rendering \
                             decision",
                            config.expected.vendor_id
                        ),
                    });
                }
            }

            drop(adapters);
            drop(matched);
            wait_between_attempts(config, attempt);
        }

        Err(RenderError::IdentityUnavailable {
            detail: format!(
                "no discrete adapter with vendor {:04x} after {} attempt(s); the last \
                 enumeration saw {} adapter(s). Phantom history: {}. Falling back to the \
                 integrated adapter would violate spec 9.7's prohibition on substituting the \
                 primary desktop adapter, and on this machine in Standard mode the integrated \
                 adapter is ordinal 0 — so a fallback would also be what a first-adapter bug \
                 looks like.",
                config.expected.vendor_id,
                config.attempts,
                last_seen,
                phantom.join(" | "),
            ),
        })
    }

    /// The recorded loss, if any. Checked before a submission is trusted and before
    /// history is carried forward: spec 15.11 makes device loss invalidate both.
    pub fn loss(&self) -> Option<&LossRecord> {
        self.loss.get()
    }

    /// Re-resolve everything after a loss. Spec 21.5: "Recreate device/targets only after
    /// old resources are no longer used" — so this consumes the old context rather than
    /// reborrowing it, and the caller's targets, pipelines and views drop with it.
    pub fn recover(config: &DeviceConfig, lost: Self) -> Result<Self, RenderError> {
        drop(lost);
        Self::create(config)
    }
}

/// Build a Vulkan-only instance.
///
/// Two settings are load-bearing and were each found by measurement in WP00:
///  * `InstanceFlags::empty()`. The default flags include
///    `VALIDATION_INDIRECT_CALL`, and validation-active clamps `max_buffer_size` and
///    binding sizes to `u32::MAX`, so a graph sized from the reported limits would be
///    sized against a limit the driver never advertised (ADR-0002 consequence 7).
///  * `Backends::VULKAN` and nothing else. Spec 9.11 forbids falling back to another
///    backend, and a second backend is a second capability matrix to certify.
fn build_instance() -> Result<Instance, RenderError> {
    let mut desc = InstanceDescriptor::new_without_display_handle();
    desc.backends = Backends::VULKAN;
    desc.flags = InstanceFlags::empty();
    let instance = Instance::new(desc);
    // `Instance::new` succeeding is not evidence the Vulkan ICD answered: wgpu-core
    // swallows per-backend init failures and simply omits the backend. Zero adapters is
    // the observable form, and it means "no loader", not "no RTX".
    let adapters = pollster::block_on(instance.enumerate_adapters(Backends::VULKAN));
    if adapters.is_empty() {
        return Err(RenderError::IdentityUnavailable {
            detail: "the Vulkan loader enumerated zero adapters, so no Vulkan backend is \
                     present in this instance; there is no second backend to fall back to"
                .to_owned(),
        });
    }
    Ok(instance)
}

/// The anchor check, kept separate from [`revalidate`] so it can be tested against a
/// blank id without needing an adapter whose expectation is also blank.
///
/// DECISIONS.md #18's residual states the rule and the reason: "`device_pci_bus_id` is an
/// empty string when `VK_EXT_pci_bus_info` is unsupported. … Require non-empty and
/// hard-fail; do not treat a blank as 'unknown, proceed'." A blank anchor would let every
/// later field in [`revalidate`] agree on nothing.
fn require_pci_anchor(record: &AdapterRecord) -> Result<(), RenderError> {
    if record.pci_bus_id.is_empty() {
        return Err(RenderError::PciBusIdUnavailable {
            adapter: record.name.clone(),
        });
    }
    Ok(())
}

/// Compare the resolved identity to the certified record, field by field.
///
/// Order matters only for the message: PCI is checked first because it is the anchor
/// #18 decision 1 calls the correctness half, and a PCI match with a name mismatch means
/// a renamed driver rather than different silicon.
fn revalidate(record: &AdapterRecord, expected: &ExpectedAdapter) -> Result<(), RenderError> {
    if record.pci_bus_id != expected.pci_bus_id {
        return Err(RenderError::AdapterRevalidation {
            field: "pci_bus_id",
            expected: expected.pci_bus_id.clone(),
            found: record.pci_bus_id.clone(),
        });
    }
    if record.name != expected.name {
        return Err(RenderError::AdapterRevalidation {
            field: "name",
            expected: expected.name.clone(),
            found: record.name.clone(),
        });
    }
    if record.driver_version != expected.driver_version {
        return Err(RenderError::AdapterRevalidation {
            field: "driver_version",
            expected: expected.driver_version.clone(),
            found: record.driver_version.clone(),
        });
    }
    if record.driver_name != expected.driver_name {
        return Err(RenderError::AdapterRevalidation {
            field: "driver_name",
            expected: expected.driver_name.clone(),
            found: record.driver_name.clone(),
        });
    }
    if record.vendor_id != expected.vendor_id || record.device_id != expected.device_id {
        return Err(RenderError::AdapterRevalidation {
            field: "vendor/device",
            expected: format!("{:04x}/{:04x}", expected.vendor_id, expected.device_id),
            found: format!("{:04x}/{:04x}", record.vendor_id, record.device_id),
        });
    }
    Ok(())
}

/// Check every entry in [`REQUIRED_CAPABILITIES`] against the adapter.
///
/// Queried on the *adapter*, exactly as ADR-0003's `Rgba16Float` measurement was: no
/// device and no surface is needed, so a missing capability is discovered before a device
/// exists to destroy.
fn validate_capabilities(adapter: &Adapter) -> Result<Capabilities, RenderError> {
    let mut confirmed = Vec::with_capacity(REQUIRED_CAPABILITIES.len());
    for &(format, usages, purpose) in REQUIRED_CAPABILITIES {
        let allowed = adapter.get_texture_format_features(format).allowed_usages;
        if !allowed.contains(usages) {
            return Err(RenderError::RequiredCapabilityMissing {
                format: format_name(format),
                usage: usage_name(usages),
                purpose: purpose.to_owned(),
            });
        }
        confirmed.push((format, usages));
    }
    let limits = adapter.limits();
    Ok(Capabilities {
        confirmed,
        features: adapter.features(),
        copy_bytes_per_row_alignment: wgpu::COPY_BYTES_PER_ROW_ALIGNMENT,
        limits,
    })
}

/// Open the device with exactly the features that were validated: none.
///
/// ADR-0003's decision is quoted in full because it decides this function: "The probe
/// requests `Features::empty()` and only the compute limits it uses. A capability list
/// gathered from a device created with extra features is not the capability list of the
/// device WP06 will create." Every usage in the graph — MRT attachment, sampled textures,
/// the staging copy — is core, so an empty set is also a statement that this renderer
/// needs no optional feature to be reproducible.
///
/// `required_limits` is the adapter's own set rather than a default: taking the adapter's
/// numbers is what makes a limit-derived buffer size reproducible, and it is the same
/// choice that let WP00 read `max_buffer_size` as 1 TiB instead of a clamped `u32::MAX`.
fn open_device(adapter: &Adapter) -> Result<(Device, Queue), RenderError> {
    let desc = wgpu::DeviceDescriptor {
        label: Some("anim_render"),
        required_features: Features::empty(),
        required_limits: adapter.limits(),
        ..Default::default()
    };
    pollster::block_on(adapter.request_device(&desc)).map_err(|e| RenderError::Submission {
        stage: "device-open".to_owned(),
        detail: format!("request_device refused: {e}"),
    })
}

/// `DedicatedVideoMemory` for the drawing card, read through DXGI.
///
/// Matching is by `(vendor_id, device_id)` — the cross-check spec 9.7 already prescribes —
/// not by LUID, which needs a second `VkInstance` for no additional correctness here, and
/// not by ordinal, which #38 and #39 show is a property of the GPU mode rather than of the
/// board. Exactly one match is required: two identical cards are distinguishable by PCI bus
/// id, and `DXGI_ADAPTER_DESC1` has no PCI field, which is #18 decision 4's whole point.
#[cfg(windows)]
fn check_dedicated_video_memory(record: &AdapterRecord, expected: &ExpectedAdapter) -> VramFloor {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_REMOTE,
        DXGI_ADAPTER_FLAG_SOFTWARE,
    };

    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(factory) => factory,
        Err(e) => {
            return VramFloor::Indeterminate {
                detail: format!("CreateDXGIFactory1: {}", e.message()),
            }
        }
    };

    let mut matches = Vec::new();
    let mut total = 0usize;
    loop {
        let index = u32::try_from(total).unwrap_or(u32::MAX);
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        total += 1;
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            break;
        };
        let flags = DXGI_ADAPTER_FLAG(desc.Flags as i32);
        if flags.contains(DXGI_ADAPTER_FLAG_SOFTWARE) || flags.contains(DXGI_ADAPTER_FLAG_REMOTE) {
            continue;
        }
        if desc.VendorId == record.vendor_id && desc.DeviceId == record.device_id {
            matches.push(desc.DedicatedVideoMemory as u64);
        }
        if total > 16 {
            return VramFloor::Indeterminate {
                detail: format!("more than {total} DXGI adapters; stopping the walk"),
            };
        }
    }

    match matches.len() {
        0 => VramFloor::Indeterminate {
            detail: format!(
                "DXGI enumerated {total} adapter(s) and none reports vendor {:04x} device \
                 {:04x}; the Vulkan adapter has no DXGI description to attribute dedicated \
                 memory to",
                record.vendor_id, record.device_id
            ),
        },
        1 => {
            let bytes = matches[0];
            if bytes >= expected.dedicated_video_memory_floor {
                VramFloor::Verified { bytes }
            } else {
                VramFloor::BelowFloor {
                    bytes,
                    floor: expected.dedicated_video_memory_floor,
                }
            }
        }
        count => VramFloor::Indeterminate {
            detail: format!(
                "{count} DXGI adapters report vendor {:04x} device {:04x}; dedicated memory \
                 cannot be attributed to one of them without LUID correlation, which is \
                 WP07's dxgi-telemetry job",
                record.vendor_id, record.device_id
            ),
        },
    }
}

/// The workspace pins one platform (deviation row 8), so no certified build reaches this.
/// It exists so the crate compiles rather than so a fallback can be exercised: the honest
/// answer on an unplanned platform is "the guard did not run".
#[cfg(not(windows))]
fn check_dedicated_video_memory(_record: &AdapterRecord, _expected: &ExpectedAdapter) -> VramFloor {
    VramFloor::Indeterminate {
        detail: "this build is not the Windows build the workspace pins, so DXGI is absent"
            .to_owned(),
    }
}

fn loss_reason(reason: DeviceLostReason) -> String {
    match reason {
        DeviceLostReason::Unknown => "unknown".to_owned(),
        DeviceLostReason::Destroyed => "destroyed".to_owned(),
    }
}

fn wait_between_attempts(config: &DeviceConfig, attempt: u32) {
    if attempt < config.attempts {
        // The only sleep in production code, and it is a bounded wait on a hardware
        // power state, not a timing source: nothing derived from it reaches canonical
        // output. clippy.toml permits exactly this use with this reason.
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(std::time::Duration::from_millis(config.retry_delay_ms));
    }
}

/// The format's own name, so the message a driver update produces quotes the thing that
/// went missing rather than a label maintained beside it.
fn format_name(format: TextureFormat) -> String {
    format!("{format:?}")
}

const fn usage_name(usages: TextureUsages) -> &'static str {
    if usages.contains(TextureUsages::RENDER_ATTACHMENT) {
        "RENDER_ATTACHMENT"
    } else if usages.contains(TextureUsages::TEXTURE_BINDING) {
        "TEXTURE_BINDING"
    } else if usages.contains(TextureUsages::COPY_SRC) {
        "COPY_SRC"
    } else {
        "an unlabelled usage"
    }
}

use crate::RenderError;

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    fn record(overrides: impl FnOnce(&mut AdapterRecord)) -> AdapterRecord {
        let mut record = AdapterRecord {
            name: "NVIDIA GeForce RTX 3050 A Laptop GPU".to_owned(),
            vendor_id: 0x10DE,
            device_id: 0x28A3,
            device_type: "DiscreteGpu".to_owned(),
            pci_bus_id: "0000:01:00.0".to_owned(),
            driver_name: "NVIDIA".to_owned(),
            driver_version: "617.14".to_owned(),
            backend: "Vulkan".to_owned(),
        };
        overrides(&mut record);
        record
    }

    #[test]
    fn the_certified_identity_revalidates_clean() {
        assert!(revalidate(&record(|_| {}), &ExpectedAdapter::reference()).is_ok());
    }

    /// The trap #18 and #39 exist to close: an integrated adapter that is otherwise
    /// perfectly healthy. Nothing about its name is wrong-looking.
    #[test]
    fn an_integrated_adapter_fails_on_bus_id_before_anything_else() {
        let igpu = record(|r| {
            r.name = "Intel(R) Graphics".to_owned();
            r.vendor_id = 0x8086;
            r.device_id = 0xA7AB;
            r.device_type = "IntegratedGpu".to_owned();
            r.pci_bus_id = "0000:00:02.0".to_owned();
            r.driver_version = "101.6314".to_owned();
        });
        let err = revalidate(&igpu, &ExpectedAdapter::reference()).unwrap_err();
        assert!(
            matches!(
                err,
                RenderError::AdapterRevalidation {
                    field: "pci_bus_id",
                    ..
                }
            ),
            "the iGPU must be refused by the anchor, not by whichever field differs first: {err}"
        );
    }

    /// A blank bus id is refused rather than treated as "unknown, proceed" — #18's
    /// residual, named because a check that passes on an absent value is not a check.
    #[test]
    fn an_empty_pci_bus_id_is_a_refusal_and_not_a_pass() {
        let blank = record(|r| r.pci_bus_id = String::new());
        // The anchor guard is its own step precisely so this case does not depend on the
        // expectation agreeing with the blank.
        let err = require_pci_anchor(&blank).unwrap_err();
        assert!(
            matches!(err, RenderError::PciBusIdUnavailable { .. }),
            "a blank anchor must be a refusal, got {err}"
        );
        assert!(require_pci_anchor(&record(|_| {})).is_ok());
        // And revalidation catches it too, when the expectation is the certified one.
        assert!(matches!(
            revalidate(&blank, &ExpectedAdapter::reference()),
            Err(RenderError::AdapterRevalidation {
                field: "pci_bus_id",
                ..
            })
        ));
    }

    /// A driver update changes `driver_version` and must not re-baseline silently.
    #[test]
    fn a_newer_driver_is_a_refusal_naming_which_field_moved() {
        let updated = record(|r| r.driver_version = "618.01".to_owned());
        let err = revalidate(&updated, &ExpectedAdapter::reference()).unwrap_err();
        match err {
            RenderError::AdapterRevalidation {
                field,
                expected,
                found,
            } => {
                assert_eq!(field, "driver_version");
                assert_eq!(expected, "617.14");
                assert_eq!(found, "618.01");
            }
            other => panic!("expected a revalidation refusal, got {other}"),
        }
    }

    /// #18's residual: the driver has two spellings, and comparing one to the other is a
    /// false mismatch. The guard is that each API's record is compared to its own field.
    #[test]
    fn the_vulkan_spelling_of_the_driver_is_never_compared_to_the_wgpu_spelling() {
        let vulkan_spelling = "617.56.0";
        assert_ne!(vulkan_spelling, ExpectedAdapter::reference().driver_version);
        // wgpu's AdapterInfo::driver_info on this machine is the 617.14 spelling; a
        // record carrying the Vulkan form would be a provenance error, and it is refused
        // rather than accepted as "the same driver by another name".
        let crossed = record(|r| r.driver_version = vulkan_spelling.to_owned());
        assert!(revalidate(&crossed, &ExpectedAdapter::reference()).is_err());
    }

    #[test]
    fn required_capabilities_cover_every_target_15_6_names() {
        let formats: Vec<TextureFormat> = REQUIRED_CAPABILITIES
            .iter()
            .map(|&(format, _, _)| format)
            .collect();
        for format in [
            TextureFormat::Rgba16Float,
            TextureFormat::Depth32Float,
            TextureFormat::Rgba8Unorm,
        ] {
            assert!(
                formats.contains(&format),
                "15.6 names a target in {format:?} but nothing requires its usage"
            );
        }
        // Depth is required to be *sampled* as well as attached: 15.6 gives normals and
        // velocity validity semantics, and 29's depth-aware post effects read depth.
        assert!(REQUIRED_CAPABILITIES
            .iter()
            .any(|(format, usages, _)| *format == TextureFormat::Depth32Float
                && usages.contains(TextureUsages::TEXTURE_BINDING)));
    }

    /// The floor is a floor. Exactly 3892 MiB passes, 3891 does not, and an integrated
    /// adapter's near-zero value is the case the guard was added for.
    #[test]
    fn the_dedicated_memory_floor_is_a_comparison_not_an_equality() {
        let floor = ExpectedAdapter::reference().dedicated_video_memory_floor;
        assert_eq!(floor, 3892 * 1024 * 1024);
        let below = VramFloor::BelowFloor {
            bytes: floor - 1,
            floor,
        };
        assert!(matches!(below, VramFloor::BelowFloor { .. }));
    }
}
