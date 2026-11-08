//! Throttled upload, eviction and reload queues (spec 16.3, 16.7, 16.9).
//!
//! The three queues are data, not threads: an [`super::eviction::EvictionPlan`] is the
//! eviction queue, a [`ReloadBatch`] is the reload queue, and the per-frame upload cap
//! (`MemoryPolicy.max_upload_bytes_per_frame`) is the throttle. The caller executes a
//! batch within the frame and confirms each reload; what does not fit is `deferred`,
//! never silently dropped — 16.7's "defer optional uploads" has a visible list.

use std::collections::BTreeSet;

use super::resource::{ReloadState, ResourceId, ResourceTable};
use super::MemoryError;

/// The reloads one frame may perform, and the ones it may not (16.9's `ReloadBatch`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReloadBatch {
    /// Evicted records to re-upload this frame, most important first.
    pub entries: Vec<ResourceId>,
    /// Total bytes the entries claim.
    pub bytes: u64,
    /// Required-but-evicted records that did not fit the frame budget. The caller
    /// decides: another frame, or an OOM report if they are pinned export requirements.
    pub deferred: Vec<ResourceId>,
}

/// Select required evicted records within `min(frame_budget, policy_cap)`.
///
/// Order is descending priority rank (Critical first), then id — the inverse of
/// eviction's order, because reload spends a scarce frame budget on what the frame
/// cannot run without. Already-resident requirements are skipped, not errors: the
/// caller asked for a set, and residency is the answer for most of it.
///
/// # Errors
///
/// [`MemoryError::UnknownResource`] when a required id was never registered.
pub fn plan_reload(
    table: &ResourceTable,
    required: &[ResourceId],
    frame_budget: u64,
    policy_cap: u64,
) -> Result<ReloadBatch, MemoryError> {
    let cap = frame_budget.min(policy_cap);
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for &id in required {
        if !seen.insert(id) {
            continue;
        }
        let record = table.get(id).ok_or(MemoryError::UnknownResource { id })?;
        if matches!(
            record.reload_state,
            ReloadState::Evicted | ReloadState::Queued
        ) {
            candidates.push(record);
        }
    }
    candidates.sort_by(|a, b| {
        b.priority
            .rank()
            .cmp(&a.priority.rank())
            .then(a.id.cmp(&b.id))
    });
    let mut batch = ReloadBatch::default();
    for record in candidates {
        if batch.bytes.saturating_add(record.bytes) <= cap {
            batch.bytes = batch.bytes.saturating_add(record.bytes);
            batch.entries.push(record.id);
        } else {
            batch.deferred.push(record.id);
        }
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::super::resource::{ContentSource, ResourcePriority, ResourceRecord};
    use super::*;

    fn evicted_table() -> (ResourceTable, Vec<ResourceId>) {
        let mut table = ResourceTable::new();
        let mut ids = Vec::new();
        for (label, bytes, priority) in [
            ("cache", 100, ResourcePriority::Cache),
            ("critical", 100, ResourcePriority::Critical),
            ("frame", 100, ResourcePriority::CurrentFrame),
        ] {
            let id = table
                .insert(
                    ResourceRecord::new(label, bytes, ContentSource::Regenerable, priority)
                        .unwrap(),
                )
                .unwrap();
            table.evict(id, &BTreeSet::new(), 0).unwrap();
            ids.push(id);
        }
        (table, ids)
    }

    #[test]
    fn reload_spends_the_budget_on_the_most_important_first() {
        let (table, ids) = evicted_table();
        let batch = plan_reload(&table, &ids, 200, 1000).unwrap();
        assert_eq!(
            batch.entries,
            vec![ids[1], ids[2]],
            "critical then current frame"
        );
        assert_eq!(batch.bytes, 200);
        assert_eq!(batch.deferred, vec![ids[0]]);
    }

    #[test]
    fn the_policy_cap_binds_even_when_the_frame_offers_more() {
        let (table, ids) = evicted_table();
        let batch = plan_reload(&table, &ids, 10_000, 100).unwrap();
        assert_eq!(batch.entries.len(), 1);
        assert_eq!(batch.bytes, 100);
        assert_eq!(batch.deferred.len(), 2);
    }

    #[test]
    fn resident_requirements_are_skipped_and_duplicates_collapse() {
        let (mut table, ids) = evicted_table();
        table.confirm_reloaded(ids[1]).unwrap();
        let batch = plan_reload(&table, &[ids[1], ids[1], ids[0]], 1000, 1000).unwrap();
        assert_eq!(batch.entries, vec![ids[0]]);
        assert!(batch.deferred.is_empty());
    }

    #[test]
    fn an_unknown_requirement_is_named_not_ignored() {
        let (table, _) = evicted_table();
        let err = plan_reload(&table, &[ResourceId(999)], 1000, 1000).unwrap_err();
        assert!(matches!(err, MemoryError::UnknownResource { id } if id.raw() == 999));
    }

    #[test]
    fn a_zero_budget_defers_everything_without_erroring() {
        let (table, ids) = evicted_table();
        let batch = plan_reload(&table, &ids, 0, 1000).unwrap();
        assert!(batch.entries.is_empty());
        assert_eq!(batch.deferred.len(), 3);
    }
}
