//! OOM degradation and device-loss reconstruction (spec 16.3, 16.5, 16.7, 16.11).
//!
//! Two regimes, one rule each: **preview** may defer optional uploads and shrink
//! optional targets; **export** is fixed-quality — a pinned requirement that cannot fit
//! is an actionable OOM, never a silent drop (16.7's last sentence, 16.11's "never evict
//! required export resources").
//!
//! Device loss is the other half: every residency record and serial assumption dies
//! (16.7), and reconstruction re-registers from CPU/disk sources in
//! [`reconstruction_rank`] order — targets before the assets that draw into them.

use std::collections::BTreeSet;

use super::eviction::{self, EvictionPlan};
use super::resource::{ContentSource, ResourceId, ResourcePriority, ResourceTable};
use super::{BudgetSnapshot, MemoryError};

/// Which regime the manager degrades under (16.5's preview/export split).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryMode {
    /// Editor preview: optional content may be deferred or dropped to stay interactive.
    Preview,
    /// Offline export: fixed quality; insufficient pinned memory is an OOM report.
    Export,
}

/// The context 16.11 requires an OOM to carry: requested bytes, budget, usage, and the
/// failed resource's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OomContext {
    /// Bytes the failed allocation asked for.
    pub requested: u64,
    /// The budget in force at failure.
    pub budget: u64,
    /// Effective usage at failure.
    pub usage: u64,
    /// What was being allocated.
    pub resource: String,
}

impl OomContext {
    /// The error form the manager returns.
    #[must_use]
    pub fn into_error(self) -> MemoryError {
        MemoryError::OutOfMemory {
            requested: self.requested,
            budget: self.budget,
            usage: self.usage,
            context: self.resource,
        }
    }
}

/// What the manager does after an allocation failure (16.7's "one bounded additional
/// eviction/degradation policy, and retry only if the policy allows").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Execute this plan and retry the creation once.
    RetryWithPlan(EvictionPlan),
    /// Preview-only: skip this optional upload and try again next frame.
    DeferOptional {
        /// Bytes the deferred upload asked for.
        requested: u64,
    },
    /// Nothing safe remains: report the OOM with full context.
    Refuse(OomContext),
}

/// Decide the one bounded recovery pass after a failed creation.
///
/// `release_need` is what a retry must free: the requested bytes plus the band that
/// keeps the retry from failing the same way. `already_retried` enforces "one bounded
/// additional" — a second failure goes straight to defer-or-refuse.
#[must_use]
pub fn plan_recovery(
    mode: MemoryMode,
    snapshot: &BudgetSnapshot,
    table: &ResourceTable,
    completed_serial: u64,
    required: &BTreeSet<ResourceId>,
    requested: u64,
    priority: ResourcePriority,
    label: &str,
    release_need: u64,
    already_retried: bool,
) -> RecoveryAction {
    if !already_retried {
        let plan = eviction::plan(snapshot, table, completed_serial, required, release_need);
        if plan.sufficient {
            return RecoveryAction::RetryWithPlan(plan);
        }
    }
    match mode {
        MemoryMode::Preview if priority.is_optional() => {
            RecoveryAction::DeferOptional { requested }
        }
        _ => RecoveryAction::Refuse(OomContext {
            requested,
            budget: snapshot.budget,
            usage: snapshot.effective_usage,
            resource: label.to_owned(),
        }),
    }
}

/// Reconstruction order after device loss: lower goes first.
///
/// Targets are `Regenerable` + `Critical` and must exist before anything draws; disk
/// assets follow by priority. The rank is a pure function of the record's class, so two
/// processes reconstruct in the same order from the same manifest.
#[must_use]
pub fn reconstruction_rank(source: &ContentSource, priority: ResourcePriority) -> (u8, u8) {
    let class = match source {
        ContentSource::Regenerable => 0,
        ContentSource::CpuShadow => 1,
        ContentSource::Disk(_) => 2,
        ContentSource::GpuOnly => 3,
    };
    (class, u8::MAX - priority.rank())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::super::resource::ResourceRecord;
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

    fn table_with_evictable(bytes: u64) -> ResourceTable {
        let mut table = ResourceTable::new();
        table
            .insert(
                ResourceRecord::new(
                    "cache",
                    bytes,
                    ContentSource::Regenerable,
                    ResourcePriority::Cache,
                )
                .unwrap(),
            )
            .unwrap();
        table
    }

    #[test]
    fn a_sufficient_plan_earns_one_retry_in_either_mode() {
        let table = table_with_evictable(500);
        for mode in [MemoryMode::Preview, MemoryMode::Export] {
            let action = plan_recovery(
                mode,
                &snapshot(1000, 900),
                &table,
                0,
                &BTreeSet::new(),
                100,
                ResourcePriority::CurrentFrame,
                "texture",
                200,
                false,
            );
            assert!(matches!(action, RecoveryAction::RetryWithPlan(p) if p.sufficient));
        }
    }

    #[test]
    fn the_second_failure_never_plans_again() {
        let table = table_with_evictable(500);
        let action = plan_recovery(
            MemoryMode::Export,
            &snapshot(1000, 900),
            &table,
            0,
            &BTreeSet::new(),
            100,
            ResourcePriority::CurrentFrame,
            "texture",
            200,
            true,
        );
        assert!(matches!(action, RecoveryAction::Refuse(_)));
    }

    #[test]
    fn preview_defers_only_optional_uploads() {
        let table = ResourceTable::new();
        let optional = plan_recovery(
            MemoryMode::Preview,
            &snapshot(1000, 900),
            &table,
            0,
            &BTreeSet::new(),
            100,
            ResourcePriority::Cache,
            "prefetch",
            200,
            true,
        );
        assert!(matches!(
            optional,
            RecoveryAction::DeferOptional { requested: 100 }
        ));
        let required = plan_recovery(
            MemoryMode::Preview,
            &snapshot(1000, 900),
            &table,
            0,
            &BTreeSet::new(),
            100,
            ResourcePriority::CurrentFrame,
            "frame texture",
            200,
            true,
        );
        assert!(matches!(required, RecoveryAction::Refuse(_)));
    }

    #[test]
    fn export_refuses_with_the_full_context_16_11_names() {
        let table = ResourceTable::new();
        let action = plan_recovery(
            MemoryMode::Export,
            &snapshot(1000, 900),
            &table,
            0,
            &BTreeSet::new(),
            100,
            ResourcePriority::Cache,
            "even optional is fixed quality in export",
            200,
            true,
        );
        let RecoveryAction::Refuse(ctx) = action else {
            panic!("export must refuse, not defer");
        };
        assert_eq!(ctx.requested, 100);
        assert_eq!(ctx.budget, 1000);
        assert_eq!(ctx.usage, 900);
        assert!(!ctx.resource.is_empty());
        let err = ctx.into_error();
        assert!(matches!(
            err,
            MemoryError::OutOfMemory {
                requested: 100,
                budget: 1000,
                usage: 900,
                ..
            }
        ));
    }

    #[test]
    fn reconstruction_puts_regenerable_targets_before_disk_assets() {
        let target = reconstruction_rank(&ContentSource::Regenerable, ResourcePriority::Critical);
        let asset = reconstruction_rank(
            &ContentSource::Disk(anim_core::id::AssetId::from_uuid(uuid::Uuid::from_bytes(
                [1; 16],
            ))),
            ResourcePriority::CurrentFrame,
        );
        assert!(target < asset);
    }
}
