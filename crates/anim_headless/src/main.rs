//! CLI parsing, export execution, reports and stable exit codes (spec 4.2).
//!
//! The export entry point is still a stub, which WP09 fills in. What WP05 changes is that the
//! binary no longer exits silently: it names the one thing that exists, and it points at the
//! process that actually runs it. WP07 adds `budget`: the live memory-telemetry report
//! (16.3), the production consumer that correlates the rendering adapter and queries its
//! local segment once, prints the advisory sample, and exits.
//!
//! Nothing here reads a project file. The process the simulation gate spawns is
//! `crates/anim_test_support/src/bin/wp05_replay.rs`, which builds the authored fixture it
//! compares against — exactly as WP04's `wp04_snapshot` did — so the two fresh processes share
//! one fixture definition and one reporting path rather than approximating each with their own.

/// The usage text, one value so `--help` and the error path cannot disagree.
fn usage() -> &'static str {
    "anim_headless - deterministic offline animation engine\n\n\
     Export execution arrives in WP09 (spec 18). What exists today:\n\
     \x20 anim_headless budget           live WP07 memory telemetry report\n\
     \x20 anim_headless::replay::report  WP05 state-hash report (programmatic)\n\
     run as a separate process by\n\
     \x20 anim_test_support/src/bin/wp05_replay.rs\n"
}

/// Open the certified device, correlate its adapter, query the local segment once and
/// report. The device is opened because correlation must name the adapter the engine
/// renders on (#18 decision 4) — a budget from an unrelated adapter is the failure 16.6
/// forbids, and `device.rs`'s guards are what make "the rendering adapter" a fact.
fn budget_command() -> Result<String, anim_render::RenderError> {
    use anim_render::device::DeviceConfig;
    use anim_render::memory::{BudgetSource, MemoryMode, MemoryPolicy};
    use anim_render::{GpuContext, MemoryManager, VramFloor};

    let gpu = GpuContext::create(&DeviceConfig::default())?;
    if gpu.loss().is_some() {
        return Err(anim_render::RenderError::DeviceLost {
            reason: "device lost before the budget probe".to_owned(),
        });
    }
    let dedicated = match &gpu.vram {
        VramFloor::Verified { bytes } => *bytes,
        _ => 0,
    };
    let mut manager = MemoryManager::new(
        MemoryPolicy::default(),
        MemoryMode::Export,
        BudgetSource::Unavailable {
            reason: "DXGI telemetry not yet attached".to_owned(),
            estimated_budget: dedicated * 3 / 4,
        },
    )?;
    #[cfg(feature = "dxgi-telemetry")]
    manager.attach_dxgi(gpu.record.vendor_id, gpu.record.device_id);
    let snapshot = manager.query_local_memory()?;
    Ok(anim_headless::budget_report::report(&manager, &snapshot))
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--help" | "-h") => print!("{}", usage()),
        Some("budget") => match budget_command() {
            Ok(text) => print!("{text}"),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        },
        Some(other) => {
            eprintln!("unknown option {other:?}\n{}", usage());
            std::process::exit(2);
        }
        None => {
            eprint!("{}", usage());
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    #[test]
    fn the_usage_names_the_real_entry_point() {
        let text = usage();
        assert!(
            text.contains("anim_headless::replay::report"),
            "a stub that says nothing is what this crate shipped for five packages: {text}"
        );
        assert!(text.contains("wp05_replay"), "and the process that uses it");
        assert!(text.contains("anim_headless budget"), "and the WP07 report");
        assert!(
            !text.contains("2026") && !text.contains("time"),
            "a usage string that varies is a usage string a test cannot pin"
        );
    }
}
