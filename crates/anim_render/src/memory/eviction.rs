//! Candidate scoring, hysteresis and eviction plans (spec 16.3, 16.6, 16.7).
//!
//! The plan is pure data: it names records and predicts bytes, and the handle owner
//! executes it (DECISIONS.md #44 ruling 6). Execution re-checks eligibility, because the
//! world moves between plan and drop — a resource marked used after the plan was computed
//! is refused at execution, not freed.
//!
//! # The order is 16.7's, term by term
//!
//! Ascending priority rank (least important first), oldest `last_used_serial`, lowest
//! reload cost, stable [`ResourceId`]. The class preference — disk-backed/regenerable
//! before CPU-shadowed — is the eligibility tier: both tiers are evictable, but the
//! reload-cost term already ranks upload-only (CpuShadow) cheapest, so the spec's
//! "prefer disk-backed/regenerable, then CPU-shadowed" is expressed as *safety first*:
//! a GpuOnly record is never a candidate at all, and among restorable records the
//! cheapest reload wins the tie.

use std::collections::BTreeSet;

use super::resource::{ResourceId, ResourceTable};
use super::{BudgetSnapshot, MemoryPolicy};

/// One record the plan names, in execution order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvictionEntry {
    /// The record to evict.
    pub id: ResourceId,
    /// Bytes its drop releases.
    pub bytes: u64,
}

/// A computed eviction plan (16.7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvictionPlan {
    /// Candidates in the order they should be dropped.
    pub entries: Vec<EvictionEntry>,
    /// Sum of `entries` bytes.
    pub predicted_release: u64,
    /// Bytes the policy asked to release (usage minus the release watermark).
    pub release_target: u64,
    /// Whether `predicted_release >= release_target`. When false, 16.7's fallbacks
    /// apply: defer optional uploads, reduce preview targets, or report OOM.
    pub sufficient: bool,
}

impl EvictionPlan {
    /// Whether executing this plan would change anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Compute the plan for a snapshot. `release_target` is the bytes that must go; the
/// caller derives it from the release watermark (or from a failed allocation's need).
///
/// The snapshot parameter is kept for 16.7 input-list fidelity and named `_snapshot`:
/// the caller has already reduced it to `release_target`, and consuming it twice would
/// give the watermark two producers — the same reasoning deviation row 20(g) records
/// for `create_renderer`'s `_queue`.
#[must_use]
pub fn plan(
    _snapshot: &BudgetSnapshot,
    table: &ResourceTable,
    completed_serial: u64,
    required: &BTreeSet<ResourceId>,
    release_target: u64,
) -> EvictionPlan {
    let mut candidates: Vec<&super::resource::ResourceRecord> = table
        .iter()
        .filter(|r| r.evictable(required, completed_serial))
        .collect();
    // 16.7's four terms, in 16.7's order. `cost_class` then bytes is "lowest reload
    // cost"; the id is the stable last resort that makes the plan reproducible.
    candidates.sort_by(|a, b| {
        a.priority
            .rank()
            .cmp(&b.priority.rank())
            .then(a.last_used_serial.cmp(&b.last_used_serial))
            .then(a.source.cost_class().cmp(&b.source.cost_class()))
            .then(a.bytes.cmp(&b.bytes))
            .then(a.id.cmp(&b.id))
    });
    let mut entries = Vec::new();
    let mut predicted_release = 0_u64;
    for record in candidates {
        if predicted_release >= release_target {
            break;
        }
        entries.push(EvictionEntry {
            id: record.id,
            bytes: record.bytes,
        });
        predicted_release = predicted_release.saturating_add(record.bytes);
    }
    EvictionPlan {
        entries,
        predicted_release,
        release_target,
        sufficient: predicted_release >= release_target,
    }
}

/// Consecutive over-budget sampling with a lower release watermark (16.6).
///
/// Eviction starts only after `over_budget_samples` consecutive samples above the soft
/// target, and a plan releases down to `soft_target - release_band_bytes` so the next
/// sample is not immediately over again. An actual allocation failure bypasses the
/// streak entirely — that bypass lives in the allocator, which calls [`plan`] directly.
#[derive(Debug, Default)]
pub struct Hysteresis {
    streak: u8,
}

impl Hysteresis {
    /// A fresh streak.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one sample in. `over` is `effective_usage > soft_target`.
    pub fn sample(&mut self, over: bool, policy: &MemoryPolicy) {
        if over {
            self.streak = self
                .streak
                .saturating_add(1)
                .min(policy.over_budget_samples);
        } else {
            self.streak = 0;
        }
    }

    /// Whether the streak has earned an eviction pass.
    #[must_use]
    pub fn should_evict(&self, policy: &MemoryPolicy) -> bool {
        self.streak >= policy.over_budget_samples
    }

    /// The streak length, for reports.
    #[must_use]
    pub const fn streak(&self) -> u8 {
        self.streak
    }

    /// Device loss or a policy change forgets the pressure history.
    pub fn reset(&mut self) {
        self.streak = 0;
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::super::resource::{ContentSource, ResourcePriority, ResourceRecord};
    use super::super::RawBudget;
    use super::*;

    fn snapshot(budget: u64, usage: u64) -> BudgetSnapshot {
        BudgetSnapshot::new(
            RawBudget {
                budget,
                current_usage: usage,
                current_reservation: 0,
                available_for_reservation: budget.saturating_sub(usage),
            },
            usage,
        )
    }

    fn table_with(records: Vec<ResourceRecord>) -> (ResourceTable, Vec<ResourceId>) {
        let mut table = ResourceTable::new();
        let ids = records
            .into_iter()
            .map(|r| table.insert(r).unwrap())
            .collect();
        (table, ids)
    }

    fn cache_record(label: &str, bytes: u64) -> ResourceRecord {
        ResourceRecord::new(
            label,
            bytes,
            ContentSource::Regenerable,
            ResourcePriority::Cache,
        )
        .unwrap()
    }

    #[test]
    fn the_plan_stops_once_the_release_target_is_met() {
        let (table, ids) = table_with(vec![
            cache_record("a", 100),
            cache_record("b", 100),
            cache_record("c", 100),
        ]);
        let p = plan(&snapshot(1000, 900), &table, 0, &BTreeSet::new(), 150);
        assert_eq!(p.entries.len(), 2);
        assert_eq!(p.predicted_release, 200);
        assert!(p.sufficient);
        assert_eq!(p.entries[0].id, ids[0], "id order breaks the full tie");
    }

    #[test]
    fn insufficient_candidates_report_insufficient_rather_than_pretending() {
        let (table, _) = table_with(vec![cache_record("a", 10)]);
        let p = plan(&snapshot(1000, 900), &table, 0, &BTreeSet::new(), 500);
        assert_eq!(p.predicted_release, 10);
        assert!(!p.sufficient);
    }

    #[test]
    fn priority_then_age_then_cost_then_id_is_the_order() {
        let mut critical = cache_record("critical", 100);
        critical.priority = ResourcePriority::Critical;
        let mut history = cache_record("history", 100);
        history.priority = ResourcePriority::OptionalHistory;
        let mut old = cache_record("old", 100);
        old.last_used_serial = 1;
        let mut young = cache_record("young", 100);
        young.last_used_serial = 5;
        let mut shadow = cache_record("shadow", 100);
        shadow.source = ContentSource::CpuShadow;
        let (table, ids) = table_with(vec![critical, young, shadow, old, history]);
        // ids: 0 critical, 1 young, 2 shadow, 3 old, 4 history
        let p = plan(&snapshot(1000, 900), &table, 10, &BTreeSet::new(), 1000);
        let order: Vec<u64> = p.entries.iter().map(|e| e.id.raw()).collect();
        assert_eq!(
            order,
            vec![ids[4].raw(), ids[2].raw(), ids[3].raw(), ids[1].raw(), ids[0].raw()],
            "history first; among cache, oldest serial wins before reload cost (shadow's 0 beats old's 1); critical last"
        );
    }

    #[test]
    fn in_flight_and_required_records_are_not_candidates() {
        let mut busy = cache_record("busy", 100);
        busy.last_used_serial = 9;
        let free = cache_record("free", 100);
        let pinned = cache_record("pinned", 100);
        let (table, ids) = table_with(vec![busy, free, pinned]);
        let mut required = BTreeSet::new();
        required.insert(ids[2]);
        let p = plan(&snapshot(1000, 900), &table, 5, &required, 1000);
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].id, ids[1]);
        assert!(!p.sufficient);
    }

    #[test]
    fn hysteresis_waits_for_the_streak_and_resets_below_target() {
        let policy = MemoryPolicy::default();
        let mut h = Hysteresis::new();
        for _ in 0..policy.over_budget_samples - 1 {
            h.sample(true, &policy);
            assert!(!h.should_evict(&policy));
        }
        h.sample(true, &policy);
        assert!(h.should_evict(&policy));
        h.sample(false, &policy);
        assert!(!h.should_evict(&policy));
        assert_eq!(h.streak(), 0);
    }

    #[test]
    fn the_streak_saturates_at_the_configured_count() {
        let policy = MemoryPolicy::default();
        let mut h = Hysteresis::new();
        for _ in 0..300 {
            h.sample(true, &policy);
        }
        assert_eq!(h.streak(), policy.over_budget_samples);
    }
}
