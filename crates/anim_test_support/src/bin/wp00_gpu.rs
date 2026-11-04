#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "WP00 probe binary, not production code: a panic here fails the probe loudly rather than emitting a wrong golden, and the bounded sleep in the adapter-retry loop is the only permitted use spec'd by clippy.toml"
)]
// crates/anim_test_support/src/bin/wp00_gpu.rs
//! Vulkan adapter enumeration, WGSL compute dispatch and buffer readback
//! (spec 9.3, 9.5 steps 3-4, 9.7 "GPU/readback probe").
//!
//! This is the probe that decides whether the whole rendering architecture is
//! sound on this machine: it is the first code that writes pixels-equivalent data
//! to an NVIDIA Vulkan device and reads exact integers back out of it.
//!
//! Failure kinds stay separate on purpose (spec 9.7): unsupported compute, shader
//! compilation, device loss and mapping failure are four different answers about
//! the platform, and reporting them alike would hide which one is true.

use std::path::{Path, PathBuf};

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
use anim_test_support::probe::{
    finish, AdapterIdentity, GpuProbeReport, ProbeError, REPORT_SCHEMA_VERSION,
};

/// Elements in the compute dispatch. One workgroup, eight invocations, exact
/// integer arithmetic so a comparison has no tolerance to argue about.
const ELEMENTS: u64 = 8;
const WORKGROUP_SIZE: u32 = 8;

/// A compute kernel whose whole purpose is to be checkable: `out[i] = in[i] * 3 + 1`
/// over unsigned integers. Multiplication and addition by constants exercise the
/// storage-buffer read path, the bind group layout, and the dispatch geometry, and
/// the expected output is computable in two lines of Rust without replicating a
/// float pipeline.
const WGSL: &str = r#"
@group(0) @binding(0) var<storage, read> input: array<u32, 8>;
@group(0) @binding(1) var<storage, read_write> output: array<u32, 8>;

@compute @workgroup_size(8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    output[i] = input[i] * 3u + 1u;
}
"#;

/// `probe-out/` at the workspace root. Derived from the crate manifest path rather
/// than the process working directory, so a probe run from any shell writes its
/// evidence to the same place.
fn default_report_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("probe-out")
}

struct Args {
    adapter: Option<usize>,
    report_dir: PathBuf,
    adapter_attempts: u32,
    retry_delay_ms: u64,
}

impl Args {
    /// Hand-rolled parsing. A probe that needs a CLI framework would be a probe
    /// with a second dependency to pin and another way to fail.
    fn from_env() -> Result<Self, ProbeError> {
        let mut adapter = None;
        let mut report_dir = default_report_dir();
        let mut adapter_attempts = 4;
        let mut retry_delay_ms = 750;

        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = |name: &str| -> Result<String, ProbeError> {
                it.next().ok_or_else(|| {
                    ProbeError::Config(format!("{name} requires a value (from {flag})"))
                })
            };
            match flag.as_str() {
                "--adapter" => {
                    let raw = value("--adapter")?;
                    adapter = Some(raw.parse::<usize>().map_err(|_| {
                        ProbeError::Config(format!("--adapter wants an ordinal, got {raw}"))
                    })?);
                }
                "--out" => report_dir = PathBuf::from(value("--out")?),
                "--adapter-attempts" => {
                    adapter_attempts = value("--adapter-attempts")?
                        .parse()
                        .map_err(|e| ProbeError::Config(format!("bad --adapter-attempts: {e}")))?;
                }
                "--retry-delay-ms" => {
                    retry_delay_ms = value("--retry-delay-ms")?
                        .parse()
                        .map_err(|e| ProbeError::Config(format!("bad --retry-delay-ms: {e}")))?;
                }
                other => {
                    return Err(ProbeError::Config(format!(
                        "unrecognized argument {other}; expected --adapter, --out, \
                         --adapter-attempts or --retry-delay-ms"
                    )))
                }
            }
        }
        Ok(Self {
            adapter,
            report_dir,
            adapter_attempts,
            retry_delay_ms,
        })
    }
}

use wgpu::util::DeviceExt as _;

fn main() -> std::process::ExitCode {
    let args = match Args::from_env() {
        Ok(a) => a,
        Err(e) => return finish("wp00_gpu", vec![e.to_diagnostic()], &default_report_dir()),
    };
    match run(&args) {
        Ok((report, diagnostics)) => {
            println!(
                "adapter {} backend={} driver={:?} max_buffer_size={} rgba16f_attachment={} \
                 compute_readback_verified={}",
                report.adapter.name,
                report.backend,
                report.driver,
                report.max_buffer_size,
                report.rgba16f_attachment,
                report.compute_readback_verified
            );
            let mut diagnostics = diagnostics;
            diagnostics.push(Diagnostic::info(
                code("gpu.report"),
                format!("GpuProbeReport schema v{REPORT_SCHEMA_VERSION} recorded for G0 evidence"),
            ));
            finish(
                "wp00_gpu",
                write_report_fields(&report, diagnostics),
                &args.report_dir,
            )
        }
        Err(e) => finish("wp00_gpu", vec![e.to_diagnostic()], &args.report_dir),
    }
}

fn code(raw: &str) -> DiagnosticCode {
    DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
}

/// GpuProbeReport is the spec-9.8 shape; the JSON file on disk uses the same fields
/// plus a `capability`/`success` envelope so a reader can find the outcome without
/// knowing which probe produced it.
fn write_report_fields(report: &GpuProbeReport, diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut out = diagnostics;
    out.push(
        Diagnostic::info(code("gpu.fields"), format!("{report:?}"))
            .with_context("max_buffer_size", report.max_buffer_size.to_string())
            .with_context(
                "compute_readback_verified",
                report.compute_readback_verified.to_string(),
            ),
    );
    out
}

/// Enumerate, select, dispatch, read back. Returns the report plus the findings
/// that qualified it.
fn run(args: &Args) -> Result<(GpuProbeReport, Vec<Diagnostic>), ProbeError> {
    let mut diagnostics = Vec::new();

    // spec 9.6: "GPU selection defaults to an explicitly configured adapter or a
    // validated selection rule; it never assumes the first adapter is the RTX."
    // enumerate_adapters returns adapters in raw driver order with no ranking, so
    // ordinal 0 is whichever vendor's driver enumerated first - on this hybrid
    // Optimus laptop that is the Intel iGPU.
    let instance = build_instance()?;

    let (adapter, mut select_diagnostics) = select_adapter(&instance, args, diagnostics.len())?;
    diagnostics.append(&mut select_diagnostics);

    let info = adapter.get_info();
    if info.backend != wgpu::Backend::Vulkan {
        // wgpu-core swallows a failing backend's init errors and simply omits it,
        // so a successful enumeration is not by itself proof that Vulkan answered.
        // Anything that is not Vulkan here means the probe ran on a fallback, which
        // spec 9.11 forbids.
        return Err(ProbeError::FeatureUnavailable(format!(
            "selected adapter {} reported backend {:?}, not Vulkan",
            info.name, info.backend
        )));
    }

    let identity = AdapterIdentity {
        name: info.name.clone(),
        vendor_id: info.vendor,
        device_id: info.device,
        // The LUID is not available through wgpu at all - it needs the raw query in
        // wp00_adapter.rs. Recording it as absent here is the explicit unavailable
        // state spec 9.11 asks for rather than an invented value.
        luid: None,
        device_pci_bus_id: (!info.device_pci_bus_id.is_empty())
            .then(|| info.device_pci_bus_id.clone()),
        driver: (!info.driver.is_empty()).then(|| info.driver.clone()),
        driver_info: (!info.driver_info.is_empty()).then(|| info.driver_info.clone()),
    };

    let limits = adapter.limits();
    let max_buffer_size = limits.max_buffer_size;
    if limits.max_compute_invocations_per_workgroup < WORKGROUP_SIZE {
        return Err(ProbeError::UnsupportedCompute);
    }

    // rgba16f attachment support is a recorded capability, not a requirement: WP06
    // needs it for HDR intermediate targets, and G0 wants to know before WP06 is
    // written rather than after.
    let rgba16f_attachment = adapter
        .get_texture_format_features(wgpu::TextureFormat::Rgba16Float)
        .allowed_usages
        .contains(wgpu::TextureUsages::RENDER_ATTACHMENT);
    diagnostics.push(Diagnostic::new(
        code("gpu.rgba16f_attachment"),
        if rgba16f_attachment {
            DiagnosticSeverity::Info
        } else {
            // Absent support for a capability this machine is not yet required to
            // have is a warning, not a gate failure - but it must be visible.
            DiagnosticSeverity::Warning
        },
        format!("Rgba16Float RENDER_ATTACHMENT supported: {rgba16f_attachment}"),
    ));

    // The only feature this probe needs. Requesting more would make the probe
    // succeed on a device that cannot do what WP06 will ask, and fail on one that
    // can do everything except what was not needed.
    let required_features = wgpu::Features::empty();
    // Ask for the compute limits this dispatch needs and nothing more. The two
    // 32-byte buffers are far under Limits::default()'s max_buffer_size, so not
    // overriding it is the point: a probe that demands the adapter's full ceiling
    // would pass on any device and prove nothing about the ones WP06 targets.
    let required_limits = wgpu::Limits {
        max_compute_workgroup_size_x: WORKGROUP_SIZE,
        max_compute_workgroup_size_y: 1,
        max_compute_workgroup_size_z: 1,
        max_compute_invocations_per_workgroup: WORKGROUP_SIZE,
        max_compute_workgroups_per_dimension: 1,
        ..Default::default()
    };

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("wp00_gpu"),
        required_features,
        required_limits,
        ..Default::default()
    }))
    .map_err(|e| ProbeError::FeatureUnavailable(format!("request_device refused: {e}")))?;

    // Device loss in wgpu 30 is not observable from poll(): PollError has only
    // Timeout and WrongSubmissionIndex. It arrives asynchronously through
    // set_device_lost_callback. Registering that callback here is what makes
    // "device loss" a distinct detectable outcome (spec 9.7) instead of something
    // that silently shows up as a mapping failure - which on this hybrid laptop,
    // where the dGPU power-gates off mid-session, is the difference between a
    // truthful report and a misleading one.
    let lost = std::sync::Arc::new(std::sync::Mutex::new(
        None::<(wgpu::DeviceLostReason, String)>,
    ));
    {
        let slot = std::sync::Arc::clone(&lost);
        device.set_device_lost_callback(move |reason, message| {
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some((reason, message));
        });
    }

    // The WGSL is validated before submission so that a compilation failure stays
    // a distinct outcome rather than arriving later disguised as device loss (the
    // phantom power-gating on this laptop makes device loss a real alternative
    // explanation, and mislabelling it would hide which one happened).
    let parsed = wgpu::naga::front::wgsl::parse_str(WGSL)
        .map_err(|e| ProbeError::ShaderCompilation(e.emit_to_string(WGSL)))?;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("wp00_gpu_compute"),
        source: wgpu::ShaderSource::Wgsl(WGSL.into()),
    });
    diagnostics.push(Diagnostic::info(
        code("gpu.shader_validated"),
        format!(
            "naga accepted the WGSL entry points: {:?}",
            entry_points(&parsed)
        ),
    ));

    let input: Vec<u32> = (0..ELEMENTS as u32).map(|i| i * 7 + 3).collect();
    let expected: Vec<u32> = input.iter().map(|v| v * 3 + 1).collect();

    let input_bytes = bytemuck_cast(&input);
    let input_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("wp00_gpu_input"),
        contents: &input_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
    });
    let output_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("wp00_gpu_output"),
        size: ELEMENTS * 4,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_SRC
            | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("wp00_gpu_staging"),
        size: ELEMENTS * 4,
        // MAP_READ on a buffer that also has STORAGE requires
        // Features::MAPPABLE_PRIMARY_BUFFERS. This probe deliberately does not
        // request that feature: a staging buffer restricted to COPY_DST|MAP_READ
        // works without it, so the probe proves readback on the narrower and more
        // portable contract.
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("wp00_gpu_bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(ELEMENTS * 4),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(ELEMENTS * 4),
                },
                count: None,
            },
        ],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("wp00_gpu_bg"),
        layout: &bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: input_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: output_buffer.as_entire_binding(),
            },
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("wp00_gpu_pl"),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("wp00_gpu_pipe"),
        layout: Some(&layout),
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: Default::default(),
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("wp00_gpu_encoder"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("wp00_gpu_pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        // One workgroup, exactly as spec 9.7 describes. Eight invocations of a
        // workgroup size of eight is the minimum that proves dispatch geometry.
        pass.dispatch_workgroups(1, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&output_buffer, 0, &staging, 0, ELEMENTS * 4);
    let index = queue.submit([encoder.finish()]);

    // Map the staging buffer and wait. The closure only records the result; a
    // panic inside a wgpu callback would be a panic across an FFI boundary, which
    // is why the outcome is checked afterwards instead.
    let map_slot = std::sync::Arc::new(std::sync::Mutex::new(None::<Result<(), String>>));
    {
        let slot = std::sync::Arc::clone(&map_slot);
        staging.map_async(
            wgpu::MapMode::Read,
            0..ELEMENTS * 4,
            Box::new(move |r: Result<(), wgpu::BufferAsyncError>| {
                *slot.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(r.map_err(|e| format!("{e:?}")));
            }),
        );
    }

    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(index),
            timeout: None,
        })
        .map_err(|e| match e {
            wgpu::PollError::Timeout => {
                ProbeError::MappingFailure("poll waited indefinitely and still timed out".into())
            }
            wgpu::PollError::WrongSubmissionIndex(requested, highest) => {
                ProbeError::MappingFailure(format!(
                    "waited on submission {requested} but the device only knows {highest}"
                ))
            }
        })?;

    // Classified before the mapping result: a buffer map that fails *because* the
    // device went lost is a device-loss finding, not a mapping finding, and the two
    // have different meanings for the render architecture.
    if let Some((reason, message)) = lost.lock().unwrap_or_else(|p| p.into_inner()).take() {
        return Err(ProbeError::DeviceLoss(format!("{reason:?}: {message}")));
    }

    {
        let guard = map_slot.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            None => {
                return Err(ProbeError::MappingFailure(
                    "map_async callback never ran; the submission completed without it".into(),
                ))
            }
            Some(Err(msg)) => return Err(ProbeError::MappingFailure(msg.clone())),
            Some(Ok(())) => {}
        }
    }

    let mapped = staging
        .get_mapped_range(0..ELEMENTS * 4)
        .map_err(|e| ProbeError::MappingFailure(format!("get_mapped_range: {e}")))?;

    // Exact integer comparison, no tolerance (spec 9.7 "compare exact integers").
    // Read with explicit little-endian assembly rather than bytemuck::cast_slice:
    // a mapped pointer carries no alignment guarantee, and a cast that panics
    // inside the probe that exists to remove risk is the wrong trade.
    let width = std::mem::size_of::<u32>();
    let mut got: Vec<u32> = Vec::with_capacity(ELEMENTS as usize);
    let mut at = 0usize;
    // Explicit bounded walk rather than chunks_exact: the mapped region has no
    // alignment guarantee, so the values are assembled from bytes, and the loop
    // bound makes "never read past the mapping" visible at the read itself.
    while at + width <= mapped.len() {
        got.push(u32::from_le_bytes([
            mapped[at],
            mapped[at + 1],
            mapped[at + 2],
            mapped[at + 3],
        ]));
        at += width;
    }
    let mismatch = got.iter().zip(expected.iter()).position(|(g, e)| g != e);
    if let Some(i) = mismatch {
        return Err(ProbeError::ReadbackMismatch(format!(
            "first divergence at element {i}: computed {}, buffer holds {}",
            expected[i], got[i]
        )));
    }
    if got.len() != ELEMENTS as usize {
        return Err(ProbeError::MappingFailure(format!(
            "mapped range yielded {} elements, expected {ELEMENTS}",
            got.len()
        )));
    }
    drop(mapped);
    staging.unmap();

    let report = GpuProbeReport {
        adapter: identity,
        backend: format!("{:?}", info.backend),
        driver: driver_string(&info),
        max_buffer_size,
        rgba16f_attachment,
        compute_readback_verified: true,
    };
    diagnostics.push(Diagnostic::info(
        code("gpu.readback_verified"),
        format!(
            "{ELEMENTS} u32 outputs matched in[] * 3 + 1 exactly on {}",
            info.name
        ),
    ));
    Ok((report, diagnostics))
}

fn driver_string(info: &wgpu::AdapterInfo) -> Option<String> {
    match (info.driver.as_str(), info.driver_info.as_str()) {
        ("", "") => None,
        (d, "") => Some(d.to_owned()),
        ("", i) => Some(i.to_owned()),
        (d, i) => Some(format!("{d} / {i}")),
    }
}

fn entry_points(module: &wgpu::naga::Module) -> Vec<String> {
    module
        .entry_points
        .iter()
        .map(|e| format!("{:?}", e.stage))
        .collect()
}

fn bytemuck_cast(values: &[u32]) -> Vec<u8> {
    bytemuck::cast_slice(values).to_vec()
}

/// Build a Vulkan-only instance.
///
/// Two settings here are load-bearing and easy to get wrong:
///  * `InstanceFlags` defaults include `VALIDATION_INDIRECT_CALL` even in a
///    release build, and validation-active clamps `max_buffer_size` and binding
///    sizes to `u32::MAX`. A report taken under those flags would record a limit
///    the driver never advertised.
///  * `skip_validation_layers` is set because this machine has no Vulkan
///    validation layer installed, so requesting one would either warn or fail
///    depending on wgpu's layer filter and neither outcome says anything about the
///    adapter.
fn build_instance() -> Result<wgpu::Instance, ProbeError> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::VULKAN;
    desc.flags = wgpu::InstanceFlags::empty();
    let instance = wgpu::Instance::new(desc);
    // wgpu-core swallows per-backend init failures and just omits the backend, so
    // `Instance::new` succeeding is not evidence that the Vulkan ICD responded.
    // Enumerating zero adapters is the observable form of that failure, and it has
    // to be reported as such rather than as "no RTX device".
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    if adapters.is_empty() {
        drop(instance);
        return Err(ProbeError::NoAdapter);
    }
    Ok(instance)
}

/// Pick the adapter to probe, retrying across the hybrid-GPU power-gating gap.
///
/// Selection rules, in order:
///  1. An explicit `--adapter` ordinal addresses the enumeration the probe itself
///     performed. The index is not stable across boots, so the chosen adapter's
///     name and ids are recorded.
///  2. Otherwise the rule is "discrete GPU whose vendor is NVIDIA" - and if that
///     matches nothing, the probe reports the absence instead of falling back to
///     the iGPU (spec 9.11 forbids substituting an unrelated adapter, and
///     BUILD_STATE hazard 2 says a one-shot probe here can otherwise conclude
///     there is no discrete GPU at all).
fn select_adapter(
    instance: &wgpu::Instance,
    args: &Args,
    _diagnostics_so_far: usize,
) -> Result<(wgpu::Adapter, Vec<Diagnostic>), ProbeError> {
    let mut diagnostics = Vec::new();
    let mut last_count = 0usize;

    for attempt in 1..=args.adapter_attempts.max(1) {
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
        last_count = adapters.len();

        if adapters.is_empty() {
            diagnostics.push(Diagnostic::warning(
                code("gpu.enumeration_empty"),
                format!("attempt {attempt}: Vulkan enumerated zero adapters"),
            ));
        } else if let Some(wanted) = args.adapter {
            let Some(adapter) = adapters.get(wanted) else {
                return Err(ProbeError::AdapterNotFound(wanted, adapters.len()));
            };
            let info = adapter.get_info();
            diagnostics.push(Diagnostic::info(
                code("gpu.selection"),
                format!(
                    "explicit --adapter {wanted} resolved to {} ({})",
                    info.name,
                    format_ids(&info)
                ),
            ));
            return Ok((adapter.clone(), diagnostics));
        } else {
            let matches: Vec<_> = adapters
                .iter()
                .filter(|a| {
                    let i = a.get_info();
                    i.device_type == wgpu::DeviceType::DiscreteGpu && i.vendor == 0x10DE
                })
                .cloned()
                .collect();
            match matches.len() {
                1 => {
                    let info = matches[0].get_info();
                    diagnostics.push(Diagnostic::info(
                        code("gpu.selection"),
                        format!(
                            "validated rule (discrete NVIDIA) resolved on attempt {attempt} \
                             to {} ({})",
                            info.name,
                            format_ids(&info)
                        ),
                    ));
                    return Ok((
                        matches.into_iter().next().expect("len checked"),
                        diagnostics,
                    ));
                }
                0 => {
                    let names: Vec<String> = adapters
                        .iter()
                        .map(|a| {
                            let i = a.get_info();
                            format!("{} [{:?}, vendor {:04x}]", i.name, i.device_type, i.vendor)
                        })
                        .collect();
                    diagnostics.push(Diagnostic::warning(
                        code("gpu.dgpu_absent"),
                        format!(
                            "attempt {attempt}: no discrete NVIDIA adapter among {} enumerated \
                             adapters: {}. The dGPU power-gates off on this hybrid Optimus \
                             laptop, so absence here is a phantom state, not proof of no dGPU.",
                            adapters.len(),
                            names.join("; ")
                        ),
                    ));
                }
                n => {
                    return Err(ProbeError::Config(format!(
                        "selection rule matched {n} discrete NVIDIA adapters; refusing to guess"
                    )));
                }
            }
        }

        drop(adapters);
        if attempt < args.adapter_attempts {
            // A bounded wait is the retry that BUILD_STATE hazard 2 asks for. This
            // is the only permitted use of thread sleep in the workspace, and it is
            // in a probe rather than in anything canonical.
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(std::time::Duration::from_millis(args.retry_delay_ms));
        }
    }

    Err(ProbeError::NoAdapter).map_err(|e| match e {
        ProbeError::NoAdapter => ProbeError::IdentityUnavailable(format!(
            "no discrete NVIDIA Vulkan adapter after {} attempts; last enumeration saw {} \
             adapter(s). Falling back to the iGPU would violate spec 9.11.",
            args.adapter_attempts, last_count
        )),
        other => other,
    })
}

fn format_ids(info: &wgpu::AdapterInfo) -> String {
    format!(
        "vendor {:04x} device {:04x} pci {}",
        info.vendor,
        info.device,
        if info.device_pci_bus_id.is_empty() {
            "unknown"
        } else {
            &info.device_pci_bus_id
        }
    )
}
