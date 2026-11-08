//! Headless memory telemetry output (spec 16.3, 16.10's diagnostics).
//!
//! # What this report is, and is not
//!
//! [`crate::replay`] produces canonical hashes a second process must reproduce byte for
//! byte. This report cannot make that claim and does not try: DXGI budgets move with
//! competing processes (16.6), so the values are advisory diagnostics, not identity.
//! What *is* pinned is the shape — one tag, `key value` lines, a trailing newline, no
//! clock, no paths, no locale — and purity: the report is a function of the immutable
//! snapshot and counters it is handed, so the same inputs print the same text.
//!
//! The renderer dependency this module needs is recorded as a deviation: 14.3's
//! "no renderer" comment governed the canonical replay report; 16.4 explicitly makes
//! the memory manager consumable by application/export diagnostics, and this is that
//! consumer.

use anim_render::memory::{BudgetSnapshot, MemoryManager, TelemetryStatus};

/// The report's domain tag, versioned like the replay report's.
pub const REPORT_TAG: &str = "anim_engine/wp07-budget/1";

/// Render one budget sample and the manager's counters as stable text.
#[must_use]
pub fn report(manager: &MemoryManager, snapshot: &BudgetSnapshot) -> String {
    let mut lines = vec![format!("tag {REPORT_TAG}")];
    match manager.telemetry_status() {
        TelemetryStatus::Live(identity) => {
            lines.push("telemetry live".to_owned());
            lines.push(format!("adapter_description {}", identity.description));
            lines.push(format!("adapter_luid {:016x}", identity.luid as u64));
            lines.push(format!("adapter_vendor {:#06x}", identity.vendor_id));
            lines.push(format!("adapter_device {:#06x}", identity.device_id));
            lines.push(format!(
                "dedicated_video_memory {}",
                identity.dedicated_video_memory
            ));
        }
        TelemetryStatus::Synthetic => lines.push("telemetry synthetic".to_owned()),
        TelemetryStatus::Unavailable(reason) => {
            lines.push("telemetry unavailable".to_owned());
            lines.push(format!("telemetry_reason {reason}"));
        }
    }
    lines.push(format!("budget {}", snapshot.budget));
    lines.push(format!("current_usage {}", snapshot.current_usage));
    lines.push(format!(
        "current_reservation {}",
        snapshot.current_reservation
    ));
    lines.push(format!(
        "available_for_reservation {}",
        snapshot.available_for_reservation
    ));
    lines.push(format!("accounted_usage {}", snapshot.accounted_usage));
    lines.push(format!("effective_usage {}", snapshot.effective_usage));
    lines.push(format!(
        "soft_target {}",
        snapshot.soft_target(manager.policy())
    ));
    lines.push(format!(
        "release_watermark {}",
        snapshot.release_watermark(manager.policy())
    ));
    lines.push(format!("reserved_bytes {}", manager.reserved_bytes()));
    lines.push(format!("completed_serial {}", manager.completed_serial()));
    let resident = manager.table().iter().filter(|r| r.resident).count();
    lines.push(format!("records_total {}", manager.table().len()));
    lines.push(format!("records_resident {resident}"));
    let metrics = manager.metrics();
    lines.push(format!("queries {}", metrics.queries));
    lines.push(format!("cache_hits {}", metrics.cache_hits));
    lines.push(format!("estimated_queries {}", metrics.estimated_queries));
    lines.push(format!("evictions_planned {}", metrics.evictions_planned));
    lines.push(format!("resources_evicted {}", metrics.resources_evicted));
    lines.push(format!("bytes_evicted {}", metrics.bytes_evicted));
    lines.push(format!("reloads {}", metrics.reloads));
    lines.push(format!("bytes_reloaded {}", metrics.bytes_reloaded));
    lines.push(format!("completion_drains {}", metrics.completion_drains));
    lines.push(format!(
        "empty_drains_under_pressure {}",
        metrics.empty_drains_under_pressure
    ));
    lines.push(format!("oom_refusals {}", metrics.oom_refusals));
    lines.push(format!("uploads_deferred {}", metrics.uploads_deferred));
    lines.push(format!(
        "fence_order_violations {}",
        metrics.fence_order_violations
    ));
    lines.push(format!("unknown_mark_used {}", metrics.unknown_mark_used));
    lines.push(format!(
        "peak_accounted_usage {}",
        metrics.peak_accounted_usage
    ));
    lines.push(format!(
        "peak_effective_usage {}",
        metrics.peak_effective_usage
    ));
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use anim_render::memory::{
        BudgetSource, ContentSource, MemoryMode, MemoryPolicy, ResourcePriority, ResourceRecord,
        SyntheticBudget,
    };
    use anim_render::MemoryManager;

    use super::*;

    fn manager_with_one_record() -> (MemoryManager, BudgetSnapshot) {
        let mut manager = MemoryManager::new(
            MemoryPolicy::default(),
            MemoryMode::Export,
            BudgetSource::Synthetic(SyntheticBudget::new(1024 * 1024, 0)),
        )
        .unwrap();
        manager
            .register_resource(
                ResourceRecord::new(
                    "tex",
                    4096,
                    ContentSource::Regenerable,
                    ResourcePriority::Cache,
                )
                .unwrap(),
            )
            .unwrap();
        let snapshot = manager.query_local_memory().unwrap();
        (manager, snapshot)
    }

    #[test]
    fn the_report_is_the_same_text_twice_and_says_what_it_sampled() {
        let (manager, snapshot) = manager_with_one_record();
        let text = report(&manager, &snapshot);
        assert_eq!(text, report(&manager, &snapshot));
        assert!(text.starts_with("tag anim_engine/wp07-budget/1\n"));
        assert!(text.ends_with('\n'));
        assert!(text.contains("telemetry synthetic\n"));
        assert!(text.contains("budget 1048576\n"));
        assert!(text.contains("accounted_usage 4096\n"));
        assert!(text.contains("effective_usage 4096\n"));
        assert!(text.contains("records_resident 1\n"));
    }

    #[test]
    fn a_different_budget_is_a_different_report() {
        let (manager, snapshot) = manager_with_one_record();
        let mut other = snapshot;
        other.budget /= 2;
        assert_ne!(report(&manager, &snapshot), report(&manager, &other));
    }

    #[test]
    fn the_unavailable_reason_reaches_the_report() {
        let mut synthetic = SyntheticBudget::new(1024, 0);
        synthetic.fail_query = true;
        let mut manager = MemoryManager::new(
            MemoryPolicy::default(),
            MemoryMode::Export,
            BudgetSource::Synthetic(synthetic),
        )
        .unwrap();
        let snapshot = manager.query_local_memory().unwrap();
        let text = report(&manager, &snapshot);
        assert!(text.contains("telemetry unavailable\n"));
        assert!(text.contains("telemetry_reason budget query failed:"));
        assert!(text.contains("estimated_queries 1\n"));
    }
}
