//! Resource records, priorities, sizes and content sources (spec 16.3, 16.8).
//!
//! # The id is a runtime handle, not a canonical identity
//!
//! [`ResourceId`] is a monotonic `u64` assigned by [`ResourceTable`] in registration
//! order. DECISIONS.md #44 ruling 1 keeps it out of `anim_core::id`: it is never
//! persisted, never hashed, and enters no canonical bytes. The eviction tie-break's
//! "stable resource ID" (16.7) is satisfied because registration order is deterministic —
//! the integration seam feeds assets in `AssetId` order and targets in declaration order.
//!
//! # What the table accounts
//!
//! `accounted_bytes` sums **resident** records only. An evicted record keeps its row (the
//! reload path needs its label, source and priority) but contributes nothing to usage,
//! because the GPU handle is gone. 16.7's `effective_usage = max(observed, accounted)`
//! reads this figure.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anim_core::id::AssetId;

use super::MemoryError;

/// A runtime resource handle assigned by [`ResourceTable`] (DECISIONS.md #44 ruling 1).
///
/// Zero is [`Self::UNASSIGNED`]; the table assigns from 1 upward, so an unassigned record
/// can never collide with a registered one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResourceId(pub(crate) u64);

impl ResourceId {
    /// The placeholder a record carries before the table assigns its id.
    pub const UNASSIGNED: Self = Self(0);

    /// The raw counter value, for reports and test assertions.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "resource#{}", self.0)
    }
}

/// Where an evicted resource's bytes come back from (spec 16.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentSource {
    /// Re-decode from the asset store and re-upload. The payload is identified, not
    /// guessed: the reload path resolves this id through `anim_assets`.
    Disk(AssetId),
    /// Recompute without I/O — a render target is re-created by re-rendering into it.
    Regenerable,
    /// A CPU copy exists; reload is an upload with no decode.
    CpuShadow,
    /// No restore source exists. 16.7 excludes "GPU-only-without-restore" from eviction,
    /// and WP07 has no GpuOnly restore path, so every `GpuOnly` record is ineligible.
    GpuOnly,
}

impl ContentSource {
    /// Whether eviction may ever select a record with this source.
    #[must_use]
    pub const fn is_restorable(&self) -> bool {
        !matches!(self, Self::GpuOnly)
    }

    /// Reload-cost class for 16.7's "lowest reload cost" term: upload-only is cheaper
    /// than recompute, which is cheaper than decode-plus-upload. Bytes break ties inside
    /// a class, so the full key is `(class, bytes)`.
    #[must_use]
    pub const fn cost_class(&self) -> u8 {
        match self {
            Self::CpuShadow => 0,
            Self::Regenerable => 1,
            Self::Disk(_) => 2,
            Self::GpuOnly => u8::MAX,
        }
    }
}

/// Eviction resistance, most important first (spec 16.8's declaration order).
///
/// 16.7 orders candidates by *ascending* priority — least important evicted first — so
/// the sort reads [`Self::rank`], not the derived enum order, which would put `Critical`
/// numerically first and evict it first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourcePriority {
    /// Required for the frame to exist at all: targets, pinned export resources.
    Critical,
    /// Named by the current frame's draw list.
    CurrentFrame,
    /// Prefetched for the next frame.
    NextFrame,
    /// Resident for reuse, not named by any frame.
    Cache,
    /// Temporal history a effect may read; the first thing released under pressure.
    OptionalHistory,
}

impl ResourcePriority {
    /// Higher means more important. Eviction sorts ascending on this.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Critical => 4,
            Self::CurrentFrame => 3,
            Self::NextFrame => 2,
            Self::Cache => 1,
            Self::OptionalHistory => 0,
        }
    }

    /// Whether preview degradation may defer or drop this class (16.7's "optional").
    #[must_use]
    pub const fn is_optional(self) -> bool {
        matches!(self, Self::Cache | Self::OptionalHistory)
    }
}

/// The reload lifecycle (spec 16.8 names the type; DECISIONS.md #44 ruling 3 gives it
/// variants). `resident == true` if and only if the state is [`Self::Resident`]; the
/// table validates the pair rather than letting the two fields drift apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadState {
    /// A GPU handle exists.
    Resident,
    /// The handle was dropped; a reload source exists.
    Evicted,
    /// Selected by a [`super::streaming::ReloadBatch`] and awaiting re-upload.
    Queued,
}

/// One owned GPU resource's facts (spec 16.8, plus `mapped` — DECISIONS.md #44 ruling 2:
/// 16.7 and 16.11 exclude mapped resources but 16.8's list has no field for the state,
/// and readback staging is `MAP_READ`).
#[derive(Clone, Debug)]
pub struct ResourceRecord {
    /// Assigned by the table at registration; the input value is ignored.
    pub id: ResourceId,
    /// Human-readable purpose, carried into OOM context (16.11).
    pub label: String,
    /// Estimated committed bytes. For textures this is extent times format width;
    /// alignment padding is excluded, matching WP06's `size_bytes` convention.
    pub bytes: u64,
    /// Where the bytes come back from after eviction.
    pub source: ContentSource,
    /// Eviction resistance.
    pub priority: ResourcePriority,
    /// Highest submission serial that referenced this resource (16.7).
    pub last_used_serial: u64,
    /// Frame counter of the last use, for reports.
    pub last_used_frame: u64,
    /// Whether a GPU handle exists right now.
    pub resident: bool,
    /// An explicit pin — export pins active-frame resources (16.10).
    pub locked: bool,
    /// A CPU mapping is live (readback staging). Never evicted while true.
    pub mapped: bool,
    /// Position in the reload lifecycle.
    pub reload_state: ReloadState,
}

impl ResourceRecord {
    /// A resident record with no use history, unlocked and unmapped.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidRecord`] for an empty label or zero bytes — a zero-byte
    /// GPU allocation is refused by wgpu, so a record claiming one is a bookkeeping
    /// defect, not a resource.
    pub fn new(
        label: impl Into<String>,
        bytes: u64,
        source: ContentSource,
        priority: ResourcePriority,
    ) -> Result<Self, MemoryError> {
        let label = label.into();
        if label.trim().is_empty() {
            return Err(MemoryError::InvalidRecord {
                detail: "resource label is empty".to_owned(),
            });
        }
        if bytes == 0 {
            return Err(MemoryError::InvalidRecord {
                detail: format!("resource {label:?} claims zero bytes"),
            });
        }
        Ok(Self {
            id: ResourceId::UNASSIGNED,
            label,
            bytes,
            source,
            priority,
            last_used_serial: 0,
            last_used_frame: 0,
            resident: true,
            locked: false,
            mapped: false,
            reload_state: ReloadState::Resident,
        })
    }

    /// Whether 16.7's exclusion list bars this record from eviction right now.
    /// `required` is the current-frame set; `completed_serial` is the fence watermark.
    #[must_use]
    pub fn evictable(&self, required: &BTreeSet<ResourceId>, completed_serial: u64) -> bool {
        self.resident
            && !self.locked
            && !self.mapped
            && self.source.is_restorable()
            && self.reload_state == ReloadState::Resident
            && self.last_used_serial <= completed_serial
            && !required.contains(&self.id)
    }

    fn validate_consistency(&self) -> Result<(), MemoryError> {
        if self.resident != (self.reload_state == ReloadState::Resident) {
            return Err(MemoryError::InvalidRecord {
                detail: format!(
                    "{}: resident={} disagrees with reload_state={:?}",
                    self.label, self.resident, self.reload_state
                ),
            });
        }
        Ok(())
    }
}

/// Every record the memory manager knows, in [`ResourceId`] order.
///
/// A `BTreeMap` for the reason WP06's registry gives: iteration order is reproducible,
/// so a resource report and an eviction plan are the same on every run.
#[derive(Debug, Default)]
pub struct ResourceTable {
    records: BTreeMap<ResourceId, ResourceRecord>,
    next_id: u64,
    accounted_bytes: u64,
}

impl ResourceTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Assign the next id and store the record. The input `record.id` is overwritten —
    /// identity is the table's to give (16.8: "Access uses registry IDs").
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidRecord`] for an inconsistent record, and
    /// [`MemoryError::Internal`] if the id counter exhausts (a `u64` of registrations).
    pub fn insert(&mut self, mut record: ResourceRecord) -> Result<ResourceId, MemoryError> {
        record.validate_consistency()?;
        let next = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| MemoryError::Internal {
                stage: "resource_table".to_owned(),
                detail: "resource id counter exhausted".to_owned(),
            })?;
        let id = ResourceId(next);
        self.next_id = next;
        record.id = id;
        if record.resident {
            self.accounted_bytes = self.accounted_bytes.saturating_add(record.bytes);
        }
        self.records.insert(id, record);
        Ok(id)
    }

    /// The record for an id, if the table has it.
    #[must_use]
    pub fn get(&self, id: ResourceId) -> Option<&ResourceRecord> {
        self.records.get(&id)
    }

    /// Mutable access for the manager's state transitions.
    pub fn get_mut(&mut self, id: ResourceId) -> Option<&mut ResourceRecord> {
        self.records.get_mut(&id)
    }

    /// Remove a record entirely (release, not eviction). Returns what was removed.
    pub fn remove(&mut self, id: ResourceId) -> Option<ResourceRecord> {
        let record = self.records.remove(&id)?;
        if record.resident {
            self.accounted_bytes = self.accounted_bytes.saturating_sub(record.bytes);
        }
        Some(record)
    }

    /// Record a use: the **highest** serial wins (16.7), never a regression.
    ///
    /// # Errors
    ///
    /// [`MemoryError::UnknownResource`] when the id was never registered.
    pub fn mark_used(
        &mut self,
        id: ResourceId,
        serial: u64,
        frame: u64,
    ) -> Result<(), MemoryError> {
        let record = self
            .records
            .get_mut(&id)
            .ok_or(MemoryError::UnknownResource { id })?;
        record.last_used_serial = record.last_used_serial.max(serial);
        record.last_used_frame = frame;
        Ok(())
    }

    /// Transition a resident record to evicted and return the bytes released.
    ///
    /// # Errors
    ///
    /// [`MemoryError::UnknownResource`] for a missing id and
    /// [`MemoryError::EvictionIneligible`] when 16.7's exclusion list bars the record at
    /// execution time — the plan was computed earlier and the world may have moved.
    pub fn evict(
        &mut self,
        id: ResourceId,
        required: &BTreeSet<ResourceId>,
        completed_serial: u64,
    ) -> Result<u64, MemoryError> {
        let record = self
            .records
            .get(&id)
            .ok_or(MemoryError::UnknownResource { id })?;
        if !record.evictable(required, completed_serial) {
            return Err(MemoryError::EvictionIneligible {
                id,
                detail: format!(
                    "{}: locked={} mapped={} resident={} required={} source={:?} last_used_serial={} completed={}",
                    record.label,
                    record.locked,
                    record.mapped,
                    record.resident,
                    required.contains(&id),
                    record.source,
                    record.last_used_serial,
                    completed_serial
                ),
            });
        }
        let record = self
            .records
            .get_mut(&id)
            .ok_or(MemoryError::UnknownResource { id })?;
        record.resident = false;
        record.reload_state = ReloadState::Evicted;
        self.accounted_bytes = self.accounted_bytes.saturating_sub(record.bytes);
        Ok(record.bytes)
    }

    /// Mark an evicted record as selected for reload. Idempotent for a record already
    /// queued — two frames may select the same deferred reload.
    ///
    /// # Errors
    ///
    /// [`MemoryError::UnknownResource`] or [`MemoryError::InvalidRecord`] when the
    /// record is resident.
    pub fn queue_reload(&mut self, id: ResourceId) -> Result<(), MemoryError> {
        let record = self
            .records
            .get_mut(&id)
            .ok_or(MemoryError::UnknownResource { id })?;
        match record.reload_state {
            ReloadState::Queued => Ok(()),
            ReloadState::Evicted => {
                record.reload_state = ReloadState::Queued;
                Ok(())
            }
            ReloadState::Resident => Err(MemoryError::InvalidRecord {
                detail: format!("{}: already resident, nothing to reload", record.label),
            }),
        }
    }

    /// Confirm a reload finished: the handle exists again and the bytes are accounted.
    ///
    /// # Errors
    ///
    /// [`MemoryError::UnknownResource`] or [`MemoryError::InvalidRecord`] when the
    /// record was not awaiting reload.
    pub fn confirm_reloaded(&mut self, id: ResourceId) -> Result<u64, MemoryError> {
        let record = self
            .records
            .get_mut(&id)
            .ok_or(MemoryError::UnknownResource { id })?;
        if !matches!(
            record.reload_state,
            ReloadState::Evicted | ReloadState::Queued
        ) {
            return Err(MemoryError::InvalidRecord {
                detail: format!(
                    "{}: not awaiting reload ({:?})",
                    record.label, record.reload_state
                ),
            });
        }
        record.resident = true;
        record.reload_state = ReloadState::Resident;
        self.accounted_bytes = self.accounted_bytes.saturating_add(record.bytes);
        Ok(record.bytes)
    }

    /// Sum of resident bytes — the `accounted_usage` input of 16.7.
    #[must_use]
    pub const fn accounted_bytes(&self) -> u64 {
        self.accounted_bytes
    }

    /// How many records the table holds, resident or not.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the table holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Every record in id order, for reports and eviction planning.
    pub fn iter(&self) -> impl Iterator<Item = &ResourceRecord> {
        self.records.values()
    }

    /// Drop every record and reset the id counter — device loss (16.7: "rebuild from
    /// CPU/disk sources"). Serial assumptions die with the records.
    pub fn clear(&mut self) {
        self.records.clear();
        self.next_id = 0;
        self.accounted_bytes = 0;
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

    fn record(label: &str, bytes: u64, priority: ResourcePriority) -> ResourceRecord {
        ResourceRecord::new(label, bytes, ContentSource::Regenerable, priority).unwrap()
    }

    #[test]
    fn ids_are_assigned_from_one_in_registration_order() {
        let mut table = ResourceTable::new();
        let a = table
            .insert(record("a", 100, ResourcePriority::Cache))
            .unwrap();
        let b = table
            .insert(record("b", 200, ResourcePriority::Cache))
            .unwrap();
        assert_eq!(a.raw(), 1);
        assert_eq!(b.raw(), 2);
        assert_eq!(table.get(a).unwrap().id, a);
        let ids: Vec<ResourceId> = table.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![a, b]);
    }

    #[test]
    fn a_zero_byte_or_unlabelled_record_is_refused() {
        assert!(
            ResourceRecord::new("x", 0, ContentSource::Regenerable, ResourcePriority::Cache)
                .is_err()
        );
        assert!(
            ResourceRecord::new("  ", 8, ContentSource::Regenerable, ResourcePriority::Cache)
                .is_err()
        );
    }

    #[test]
    fn accounted_bytes_counts_resident_records_only() {
        let mut table = ResourceTable::new();
        let a = table
            .insert(record("a", 100, ResourcePriority::Cache))
            .unwrap();
        let _b = table
            .insert(record("b", 200, ResourcePriority::Cache))
            .unwrap();
        assert_eq!(table.accounted_bytes(), 300);
        let required = BTreeSet::new();
        table.evict(a, &required, 0).unwrap();
        assert_eq!(table.accounted_bytes(), 200);
        assert_eq!(table.len(), 2, "the row stays; the reload path needs it");
        table.confirm_reloaded(a).unwrap();
        assert_eq!(table.accounted_bytes(), 300);
    }

    #[test]
    fn the_exclusion_list_bars_every_class_16_7_names() {
        let mut table = ResourceTable::new();
        let required = BTreeSet::new();

        let locked = table
            .insert(record("locked", 8, ResourcePriority::Cache))
            .unwrap();
        table.get_mut(locked).unwrap().locked = true;

        let mapped = table
            .insert(record("mapped", 8, ResourcePriority::Cache))
            .unwrap();
        table.get_mut(mapped).unwrap().mapped = true;

        let in_flight = table
            .insert(record("in_flight", 8, ResourcePriority::Cache))
            .unwrap();
        table.mark_used(in_flight, 5, 0).unwrap();

        let gpu_only = table
            .insert(
                ResourceRecord::new(
                    "gpu_only",
                    8,
                    ContentSource::GpuOnly,
                    ResourcePriority::Cache,
                )
                .unwrap(),
            )
            .unwrap();

        let pinned = table
            .insert(record("pinned", 8, ResourcePriority::Cache))
            .unwrap();
        let mut required_set = BTreeSet::new();
        required_set.insert(pinned);

        for id in [locked, mapped, in_flight, gpu_only] {
            assert!(
                table.evict(id, &required, 4).is_err(),
                "{id} must be ineligible"
            );
        }
        assert!(table.evict(pinned, &required_set, 4).is_err());

        let free = table
            .insert(record("free", 8, ResourcePriority::Cache))
            .unwrap();
        assert_eq!(table.evict(free, &required, 4).unwrap(), 8);
    }

    #[test]
    fn mark_used_keeps_the_highest_serial() {
        let mut table = ResourceTable::new();
        let id = table
            .insert(record("r", 8, ResourcePriority::Cache))
            .unwrap();
        table.mark_used(id, 7, 1).unwrap();
        table.mark_used(id, 3, 2).unwrap();
        assert_eq!(table.get(id).unwrap().last_used_serial, 7);
        assert_eq!(table.get(id).unwrap().last_used_frame, 2);
        assert!(table.mark_used(ResourceId(999), 1, 0).is_err());
    }

    #[test]
    fn priority_rank_puts_optional_history_first_out_the_door() {
        assert!(ResourcePriority::OptionalHistory.rank() < ResourcePriority::Cache.rank());
        assert!(ResourcePriority::Cache.rank() < ResourcePriority::NextFrame.rank());
        assert!(ResourcePriority::NextFrame.rank() < ResourcePriority::CurrentFrame.rank());
        assert!(ResourcePriority::CurrentFrame.rank() < ResourcePriority::Critical.rank());
        assert!(ResourcePriority::Cache.is_optional());
        assert!(!ResourcePriority::CurrentFrame.is_optional());
    }

    #[test]
    fn reload_lifecycle_is_resident_evicted_queued_resident() {
        let mut table = ResourceTable::new();
        let id = table
            .insert(record("r", 64, ResourcePriority::Cache))
            .unwrap();
        let required = BTreeSet::new();
        table.evict(id, &required, 0).unwrap();
        assert_eq!(table.get(id).unwrap().reload_state, ReloadState::Evicted);
        assert!(table.confirm_reloaded(id).is_ok(), "direct reload is legal");
        table.evict(id, &required, 0).unwrap();
        table.queue_reload(id).unwrap();
        assert_eq!(table.get(id).unwrap().reload_state, ReloadState::Queued);
        assert!(!table.get(id).unwrap().resident);
        assert!(
            table.queue_reload(id).is_ok(),
            "queuing twice is idempotent"
        );
        table.confirm_reloaded(id).unwrap();
        assert!(table.get(id).unwrap().resident);
        assert!(table.confirm_reloaded(id).is_err(), "already resident");
        assert!(
            table.queue_reload(id).is_err(),
            "resident has nothing to reload"
        );
    }

    #[test]
    fn clear_resets_ids_and_accounting_for_reconstruction() {
        let mut table = ResourceTable::new();
        table
            .insert(record("a", 100, ResourcePriority::Critical))
            .unwrap();
        table.clear();
        assert!(table.is_empty());
        assert_eq!(table.accounted_bytes(), 0);
        let id = table
            .insert(record("b", 8, ResourcePriority::Cache))
            .unwrap();
        assert_eq!(id.raw(), 1, "ids restart after loss");
    }
}
