//! Synthetic-budget and fault-injection tests for WP07 (spec 16.3, 16.12, gate G6).
//!
//! # What this module proves
//!
//! 16.12's policy clauses, with no GPU in the loop: budget shrinkage converges below the
//! configured target when evictable content suffices; insufficient content produces an
//! actionable OOM rather than false success; competing-process usage raises
//! `effective_usage` above what the engine accounts; unavailable telemetry keeps internal
//! accounting alive; the hysteresis streak gates pressure eviction while an actual
//! allocation need bypasses it; and a 10 000-frame export-shaped run logs usage,
//! evictions, reloads and stalls without a single fence-order violation.
//!
//! The GPU half of 16.12 — evicted resources reload exactly and match a residency-stable
//! render reference, repeated resize, scrubbing, device-loss reconstruction — lives in
//! `crates/anim_render/tests/memory.rs`, where a real device is available.
//!
//! The budgets here are deliberately small and the policy reserve is 1 MiB, not the
//! 256 MiB production default: the arithmetic under test is the formula, and a reserve
//! larger than the whole synthetic budget would saturate every target to zero and test
//! nothing.

use std::collections::BTreeSet;

use anim_render::memory::{
    BudgetSource, ContentSource, MemoryMode, MemoryPolicy, ResourceId, ResourcePriority,
    ResourceRecord, SyntheticBudget,
};
use anim_render::{MemoryError, MemoryManager};

const MIB: u64 = 1024 * 1024;

/// A policy whose reserve and band are small enough to exercise the formula at MiB
/// scale. The fraction and streak are the production defaults.
#[must_use]
pub fn test_policy() -> MemoryPolicy {
    MemoryPolicy {
        safety_reserve_bytes: MIB,
        release_band_bytes: MIB,
        ..MemoryPolicy::default()
    }
}

/// A manager on a synthetic budget, in the fixed-quality regime unless told otherwise.
///
/// # Errors
///
/// [`MemoryError::InvalidPolicy`] — unreachable with [`test_policy`].
pub fn synthetic_manager(
    budget: u64,
    usage: u64,
    mode: MemoryMode,
) -> Result<MemoryManager, MemoryError> {
    MemoryManager::new(
        test_policy(),
        mode,
        BudgetSource::Synthetic(SyntheticBudget::new(budget, usage)),
    )
}

/// Register `count` cache records of `bytes` each, returning their ids in order.
///
/// # Errors
///
/// [`MemoryError::InvalidRecord`] for a zero byte count.
pub fn fill_cache(
    manager: &mut MemoryManager,
    count: u64,
    bytes: u64,
) -> Result<Vec<ResourceId>, MemoryError> {
    let mut ids = Vec::new();
    for index in 0..count {
        let record = ResourceRecord::new(
            format!("cache/{index}"),
            bytes,
            ContentSource::Regenerable,
            ResourcePriority::Cache,
        )?;
        ids.push(manager.register_resource(record)?);
    }
    Ok(ids)
}

/// What a convergence run ended with (G6's two outcomes, distinguished).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConvergenceReport {
    /// Whether effective usage finished at or below the soft target.
    pub converged: bool,
    /// Eviction passes executed.
    pub rounds: u64,
    /// Effective usage at exit.
    pub final_effective_usage: u64,
    /// The soft target at exit.
    pub soft_target: u64,
}

/// Evict until below the soft target or until nothing safe remains. Bounded at 64
/// rounds: a policy that cannot converge in 64 passes over a finite table is looping,
/// and the report says so rather than hanging the gate.
///
/// # Errors
///
/// [`MemoryError::DeviceLost`] or an execution-time eviction refusal.
pub fn converge(
    manager: &mut MemoryManager,
    required: &BTreeSet<ResourceId>,
) -> Result<ConvergenceReport, MemoryError> {
    let mut report = ConvergenceReport::default();
    for _ in 0..64 {
        let snapshot = manager.query_local_memory()?;
        report.soft_target = snapshot.soft_target(manager.policy());
        report.final_effective_usage = snapshot.effective_usage;
        if snapshot.effective_usage <= report.soft_target {
            report.converged = true;
            return Ok(report);
        }
        let plan = manager.plan_eviction(&snapshot, required);
        if plan.entries.is_empty() {
            return Ok(report);
        }
        let ids: Vec<ResourceId> = plan.entries.iter().map(|entry| entry.id).collect();
        manager.apply_eviction(&ids)?;
        report.rounds += 1;
    }
    Ok(report)
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
    fn budget_shrinkage_converges_and_never_touches_locked_or_required() {
        let mut manager = synthetic_manager(100 * MIB, 0, MemoryMode::Export).unwrap();
        let locked = manager
            .register_resource(
                ResourceRecord::new(
                    "target",
                    20 * MIB,
                    ContentSource::Regenerable,
                    ResourcePriority::Critical,
                )
                .unwrap(),
            )
            .unwrap();
        manager.set_locked(locked, true).unwrap();
        let required_id = manager
            .register_resource(
                ResourceRecord::new(
                    "frame",
                    10 * MIB,
                    ContentSource::Regenerable,
                    ResourcePriority::CurrentFrame,
                )
                .unwrap(),
            )
            .unwrap();
        let cache = fill_cache(&mut manager, 8, 10 * MIB).unwrap();
        let mut required = BTreeSet::new();
        required.insert(required_id);
        manager.set_required(required.clone());

        // Shrink: 110 MiB accounted against a 60 MiB budget.
        manager.synthetic_mut().unwrap().budget = 60 * MIB;
        manager.invalidate_query_cache();
        let report = converge(&mut manager, &required).unwrap();
        assert!(report.converged, "{report:?}");
        assert!(report.final_effective_usage <= report.soft_target);
        assert_eq!(report.final_effective_usage, 40 * MIB);
        assert!(manager.table().get(locked).unwrap().resident);
        assert!(manager.table().get(required_id).unwrap().resident);
        let evicted = cache
            .iter()
            .filter(|&&id| !manager.table().get(id).unwrap().resident)
            .count();
        assert_eq!(evicted, 7);
        assert_eq!(manager.metrics().bytes_evicted, 70 * MIB);
    }

    #[test]
    fn insufficient_evictable_content_is_an_actionable_oom_not_false_success() {
        let mut manager = synthetic_manager(100 * MIB, 0, MemoryMode::Export).unwrap();
        let pinned = manager
            .register_resource(
                ResourceRecord::new(
                    "pinned",
                    40 * MIB,
                    ContentSource::Regenerable,
                    ResourcePriority::Critical,
                )
                .unwrap(),
            )
            .unwrap();
        manager.set_locked(pinned, true).unwrap();
        let cache = fill_cache(&mut manager, 1, 10 * MIB).unwrap();
        manager.synthetic_mut().unwrap().budget = 30 * MIB;
        manager.invalidate_query_cache();
        let report = converge(&mut manager, &BTreeSet::new()).unwrap();
        assert!(
            !report.converged,
            "nothing safe remains; the report must say so"
        );
        assert!(report.final_effective_usage > report.soft_target);
        assert!(!manager.table().get(cache[0]).unwrap().resident);

        let error = manager
            .preflight_allocation(5 * MIB, ResourcePriority::Critical)
            .unwrap_err();
        let MemoryError::OutOfMemory {
            requested,
            budget,
            usage,
            context,
        } = error
        else {
            panic!("export must refuse with an actionable OOM");
        };
        assert_eq!(requested, 5 * MIB);
        assert_eq!(budget, 30 * MIB);
        assert!(usage >= 40 * MIB);
        assert!(!context.is_empty());
        assert_eq!(manager.reserved_bytes(), 0);
        assert_eq!(manager.metrics().oom_refusals, 1);
    }

    #[test]
    fn competing_process_usage_is_effective_but_not_evictable() {
        let mut manager = synthetic_manager(100 * MIB, 90 * MIB, MemoryMode::Export).unwrap();
        let cache = fill_cache(&mut manager, 1, 10 * MIB).unwrap();
        let snapshot = manager.query_local_memory().unwrap();
        assert_eq!(snapshot.accounted_usage, 10 * MIB);
        assert_eq!(
            snapshot.effective_usage,
            90 * MIB,
            "max(observed, accounted)"
        );
        let plan = manager.plan_eviction(&snapshot, &BTreeSet::new());
        manager.apply_eviction(&[cache[0]]).unwrap();
        let after = manager.query_local_memory().unwrap();
        assert_eq!(
            after.effective_usage,
            90 * MIB,
            "another process's bytes are not ours to free"
        );
        assert!(!plan.sufficient);
        assert_eq!(after.accounted_usage, 0);
    }

    #[test]
    fn unavailable_telemetry_keeps_accounting_and_the_estimate_conservative() {
        let mut synthetic = SyntheticBudget::new(100 * MIB, 0);
        synthetic.fail_query = true;
        let mut manager = MemoryManager::new(
            test_policy(),
            MemoryMode::Export,
            BudgetSource::Synthetic(synthetic),
        )
        .unwrap();
        fill_cache(&mut manager, 1, 10 * MIB).unwrap();
        let snapshot = manager.query_local_memory().unwrap();
        assert_eq!(snapshot.budget, 100 * MIB);
        assert_eq!(snapshot.effective_usage, 10 * MIB);
        assert!(matches!(
            manager.telemetry_status(),
            anim_render::memory::TelemetryStatus::Unavailable(reason) if reason.contains("synthetic query failure")
        ));
        assert!(manager.metrics().estimated_queries >= 1);
        // The estimate still refuses what cannot fit: 10 accounted + 95 requested > 100.
        let error = manager
            .preflight_allocation(95 * MIB, ResourcePriority::Critical)
            .unwrap_err();
        assert!(matches!(error, MemoryError::OutOfMemory { .. }));
    }

    #[cfg(feature = "dxgi-telemetry")]
    #[test]
    fn an_adapter_mismatch_degrades_rather_than_querying_the_wrong_card() {
        let mut manager = synthetic_manager(100 * MIB, 0, MemoryMode::Export).unwrap();
        manager.attach_dxgi(0xDEAD, 0xBEEF);
        assert!(matches!(
            manager.telemetry_status(),
            anim_render::memory::TelemetryStatus::Unavailable(reason) if reason.contains("correlation")
        ));
        // Accounting still answers after the degradation.
        let snapshot = manager.query_local_memory().unwrap();
        assert_eq!(snapshot.budget, 100 * MIB);
    }

    #[test]
    fn the_streak_gates_pressure_but_an_allocation_need_bypasses_it() {
        let mut manager = synthetic_manager(50 * MIB, 0, MemoryMode::Export).unwrap();
        fill_cache(&mut manager, 5, 9 * MIB).unwrap();
        // 45 MiB against a 40 MiB soft target: over, but the streak has just begun.
        assert!(manager.pressure_eviction_plan().unwrap().is_none());
        // An actual allocation need does not wait for the streak (16.6).
        let permit = manager
            .preflight_allocation(10 * MIB, ResourcePriority::Critical)
            .unwrap();
        assert!(!permit.plan().is_empty(), "preflight must plan immediately");
        assert!(permit.plan().sufficient);
    }

    #[test]
    fn a_rollback_after_a_successful_query_leaves_no_reservation() {
        let mut manager = synthetic_manager(100 * MIB, 0, MemoryMode::Export).unwrap();
        let snapshot = manager.query_local_memory().unwrap();
        assert_eq!(snapshot.budget, 100 * MIB);
        {
            let permit = manager
                .preflight_allocation(30 * MIB, ResourcePriority::CurrentFrame)
                .unwrap();
            assert_eq!(permit.bytes(), 30 * MIB);
            assert_eq!(manager.reserved_bytes(), 30 * MIB);
            // The creation "fails": the permit drops without committing.
        }
        assert_eq!(manager.reserved_bytes(), 0);
        assert_eq!(manager.table().accounted_bytes(), 0);
    }

    #[test]
    fn a_long_export_shape_run_logs_every_class_and_keeps_the_fences_ordered() {
        let mut manager = synthetic_manager(64 * MIB, 0, MemoryMode::Export).unwrap();
        let cache = fill_cache(&mut manager, 8, 4 * MIB).unwrap();
        for frame in 0..10_000_u64 {
            let serial = manager.begin_submission().unwrap();
            let record = ResourceRecord::new(
                format!("frame/{frame}"),
                1024,
                ContentSource::Regenerable,
                ResourcePriority::CurrentFrame,
            )
            .unwrap();
            let id = manager.register_resource(record).unwrap();
            manager.mark_used(id, serial, frame);
            manager.on_submitted(serial);
            let callback = manager.submission_callback(serial);
            callback();
            manager.drain_completions();
            manager.release_resource(id);

            // Periodic pressure: shrink, converge, reload one evicted record.
            if frame % 500 == 0 {
                manager.synthetic_mut().unwrap().budget = 30 * MIB;
                manager.invalidate_query_cache();
                let report = converge(&mut manager, &BTreeSet::new()).unwrap();
                assert!(report.converged, "frame {frame}: {report:?}");
                let evicted: Vec<ResourceId> = cache
                    .iter()
                    .copied()
                    .filter(|&id| !manager.table().get(id).unwrap().resident)
                    .collect();
                if let Some(&first) = evicted.first() {
                    let batch = manager.reload_required(&[first], 8 * MIB).unwrap();
                    assert_eq!(batch.entries, vec![first]);
                    manager.confirm_reloaded(first).unwrap();
                }
                manager.synthetic_mut().unwrap().budget = 64 * MIB;
                manager.invalidate_query_cache();
            }
        }
        let metrics = manager.metrics();
        assert_eq!(metrics.fence_order_violations, 0);
        assert_eq!(metrics.unknown_mark_used, 0);
        assert!(metrics.queries > 0);
        assert!(metrics.completion_drains >= 10_000);
        assert!(metrics.resources_evicted > 0);
        assert!(metrics.reloads > 0);
        assert_eq!(manager.completed_serial(), 10_000);
        // The shrink/reload cycle settles at seven of eight cache records resident:
        // each pass evicts down to the watermark and reloads exactly one.
        assert_eq!(manager.table().accounted_bytes(), 28 * MIB);
    }
}
