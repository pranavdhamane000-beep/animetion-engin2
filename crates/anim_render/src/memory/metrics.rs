//! Immutable usage, pressure, eviction and stall metrics (spec 16.3, 16.8).
//!
//! The manager keeps one running [`MetricsSnapshot`] and hands out copies; 16.8's
//! "metrics snapshots are immutable copies" means a report taken mid-frame cannot be
//! mutated by the next allocation. Every counter is saturating — a metric that wraps
//! would report a stall as a recovery.

/// A point-in-time copy of the manager's counters (spec 16.8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Budget samples taken, from any source.
    pub queries: u64,
    /// Calls answered from the cadence cache without a fresh query.
    pub cache_hits: u64,
    /// Samples answered from internal estimates because telemetry was unavailable.
    pub estimated_queries: u64,
    /// Eviction plans produced (a plan may name zero candidates).
    pub evictions_planned: u64,
    /// Records actually transitioned to evicted.
    pub resources_evicted: u64,
    /// Bytes released by eviction.
    pub bytes_evicted: u64,
    /// Reloads confirmed resident again.
    pub reloads: u64,
    /// Bytes re-uploaded by reloads.
    pub bytes_reloaded: u64,
    /// Completion drains performed.
    pub completion_drains: u64,
    /// Drains under pressure that delivered nothing — 16.12's "stalls".
    pub empty_drains_under_pressure: u64,
    /// Allocations refused with an actionable OOM (16.11).
    pub oom_refusals: u64,
    /// Optional uploads deferred by preview degradation (16.7).
    pub uploads_deferred: u64,
    /// Out-of-order fence serials refused (#44 ruling 8).
    pub fence_order_violations: u64,
    /// `mark_used` calls naming an unregistered id — visible, not silent.
    pub unknown_mark_used: u64,
    /// High-water marks since construction (not reset by device loss, so a report can
    /// show what the run peaked at before reconstruction).
    pub peak_accounted_usage: u64,
    pub peak_effective_usage: u64,
}

impl MetricsSnapshot {
    /// Fold one sample's usage figures into the high-water marks.
    pub fn observe_usage(&mut self, accounted: u64, effective: u64) {
        self.peak_accounted_usage = self.peak_accounted_usage.max(accounted);
        self.peak_effective_usage = self.peak_effective_usage.max(effective);
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
    fn a_copy_does_not_share_state_with_the_running_snapshot() {
        let mut running = MetricsSnapshot {
            queries: 3,
            ..Default::default()
        };
        let copy = running;
        running.queries = 9;
        assert_eq!(copy.queries, 3);
        assert_eq!(running.queries, 9);
    }

    #[test]
    fn peaks_only_rise() {
        let mut m = MetricsSnapshot::default();
        m.observe_usage(100, 200);
        m.observe_usage(50, 80);
        assert_eq!(m.peak_accounted_usage, 100);
        assert_eq!(m.peak_effective_usage, 200);
    }
}
