//! Public memory API and policy orchestration (spec 16.3, 16.6–16.9).
//!
//! # The shape of the thing
//!
//! [`MemoryManager`] owns records, serials, reservations and policy — never a GPU
//! handle. The handle owner (WP06's renderer) asks for a [`AllocationPermit`], executes
//! the eviction plan the permit carries, creates the resource, and commits; drop is
//! rollback (16.8, DECISIONS.md #44 ruling 6). Eviction and reload are plans and batches,
//! executed by the same owner and confirmed back here.
//!
//! # Advisory telemetry, authoritative accounting
//!
//! 16.4: DXGI values are advisory; internal accounting and actual allocation errors are
//! authoritative. Every snapshot therefore carries `effective_usage =
//! max(observed_usage, accounted_usage)` (16.7), and a query failure degrades the source
//! to [`BudgetSource::Unavailable`] with a conservative estimate rather than failing the
//! frame (16.6, 16.11). The difference between observed and accounted bytes is a
//! diagnostic, not proof another process owns them.
//!
//! # No waiting
//!
//! "Bounded wait" is a non-blocking drain of known completions (#44 ruling 9): the
//! manager never blocks on the GPU, never sleeps, and 16.11's "never wait indefinitely"
//! holds by construction.

pub mod allocator;
#[cfg(feature = "dxgi-telemetry")]
pub mod dxgi;
pub mod eviction;
pub mod fences;
pub mod identity;
pub mod metrics;
pub mod recovery;
pub mod resource;
pub mod streaming;

use std::cell::Cell;
use std::collections::BTreeSet;
use std::time::Duration;

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};

pub use allocator::{AllocationPermit, ReservationCell};
pub use eviction::{EvictionEntry, EvictionPlan, Hysteresis};
pub use fences::{CompletionChannel, SerialTracker};
pub use identity::AdapterIdentity;
pub use metrics::MetricsSnapshot;
pub use recovery::{MemoryMode, OomContext, RecoveryAction};
pub use resource::{
    ContentSource, ReloadState, ResourceId, ResourcePriority, ResourceRecord, ResourceTable,
};
pub use streaming::ReloadBatch;

/// The four DXGI local-segment values, before internal accounting is folded in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawBudget {
    /// The OS-granted budget for this process on the correlated adapter.
    pub budget: u64,
    /// The adapter's observed usage — not necessarily this process's (16.7).
    pub current_usage: u64,
    /// The adapter's current reservation.
    pub current_reservation: u64,
    /// What the adapter says is available for reservation.
    pub available_for_reservation: u64,
}

/// An immutable budget sample (spec 16.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetSnapshot {
    /// Budget in force at sample time.
    pub budget: u64,
    /// Observed usage from the source (or the internal estimate).
    pub current_usage: u64,
    /// Observed reservation from the source.
    pub current_reservation: u64,
    /// Observed availability from the source.
    pub available_for_reservation: u64,
    /// What the registry accounts as resident.
    pub accounted_usage: u64,
    /// `max(current_usage, accounted_usage)` — 16.7's conservative policy input.
    pub effective_usage: u64,
}

impl BudgetSnapshot {
    /// Fold internal accounting into a raw sample.
    #[must_use]
    pub fn new(raw: RawBudget, accounted_usage: u64) -> Self {
        Self {
            budget: raw.budget,
            current_usage: raw.current_usage,
            current_reservation: raw.current_reservation,
            available_for_reservation: raw.available_for_reservation,
            accounted_usage,
            effective_usage: raw.current_usage.max(accounted_usage),
        }
    }

    /// 16.7's soft target: `min(budget * target_fraction, budget - safety_reserve)`,
    /// saturating. The fraction multiply floors (the `as` cast on a non-negative
    /// product), which is the conservative direction.
    #[must_use]
    pub fn soft_target(&self, policy: &MemoryPolicy) -> u64 {
        let fraction = (self.budget as f64 * f64::from(policy.target_fraction)) as u64;
        fraction.min(self.budget.saturating_sub(policy.safety_reserve_bytes))
    }

    /// The lower watermark eviction releases to: soft target minus the release band
    /// (16.6), so one pass does not leave the next sample immediately over again.
    #[must_use]
    pub fn release_watermark(&self, policy: &MemoryPolicy) -> u64 {
        self.soft_target(policy)
            .saturating_sub(policy.release_band_bytes)
    }

    /// Whether this sample is over the soft target.
    #[must_use]
    pub fn over_budget(&self, policy: &MemoryPolicy) -> bool {
        self.effective_usage > self.soft_target(policy)
    }
}

/// Validated memory-policy configuration (spec 16.8). Defaults are ADR-0012's register
/// and DECISIONS.md #44 ruling 7 — configuration defaults, not hardware promises (16.6).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemoryPolicy {
    /// Fraction of the budget the soft target claims. 16.6's initial value is 0.80.
    pub target_fraction: f32,
    /// Absolute bytes kept free below the budget, whichever binds first.
    pub safety_reserve_bytes: u64,
    /// Hysteresis band below the soft target that eviction releases to.
    pub release_band_bytes: u64,
    /// Consecutive over-budget samples before pressure eviction starts (16.6).
    pub over_budget_samples: u8,
    /// Minimum spacing between fresh budget queries; calls inside it reuse the cache.
    pub query_interval: Duration,
    /// Per-frame cap on reload/upload bytes (16.7's bounded streaming).
    pub max_upload_bytes_per_frame: u64,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            target_fraction: 0.80,
            safety_reserve_bytes: 256 * 1024 * 1024,
            release_band_bytes: 128 * 1024 * 1024,
            over_budget_samples: 3,
            query_interval: Duration::from_millis(100),
            max_upload_bytes_per_frame: 64 * 1024 * 1024,
        }
    }
}

impl MemoryPolicy {
    /// Refuse a policy whose arithmetic would be meaningless.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidPolicy`] naming the field that broke.
    pub fn validate(&self) -> Result<(), MemoryError> {
        if !self.target_fraction.is_finite()
            || self.target_fraction <= 0.0
            || self.target_fraction > 1.0
        {
            return Err(MemoryError::InvalidPolicy {
                detail: format!("target_fraction {} is not in (0, 1]", self.target_fraction),
            });
        }
        if self.over_budget_samples == 0 {
            return Err(MemoryError::InvalidPolicy {
                detail: "over_budget_samples is zero; pressure eviction would never start"
                    .to_owned(),
            });
        }
        if self.query_interval.is_zero() {
            return Err(MemoryError::InvalidPolicy {
                detail: "query_interval is zero; every call would query the adapter".to_owned(),
            });
        }
        if self.max_upload_bytes_per_frame == 0 {
            return Err(MemoryError::InvalidPolicy {
                detail: "max_upload_bytes_per_frame is zero; no reload could ever run".to_owned(),
            });
        }
        Ok(())
    }
}

/// A synthetic budget source for 16.12's tests (DECISIONS.md #44 ruling 4: compiled
/// unconditionally so the ordinary gate runs the synthetic suite). Mutate the fields
/// between samples to simulate budget shrinkage; set `fail_query` to inject the
/// unavailable-DXGI path.
#[derive(Clone, Debug)]
pub struct SyntheticBudget {
    /// The budget the source reports.
    pub budget: u64,
    /// The usage the source reports.
    pub current_usage: u64,
    /// The reservation the source reports.
    pub current_reservation: u64,
    /// The availability the source reports.
    pub available_for_reservation: u64,
    /// When true, `query` fails and the manager degrades to internal estimates.
    pub fail_query: bool,
}

impl SyntheticBudget {
    /// A source reporting `budget` and `usage`, with availability derived.
    #[must_use]
    pub fn new(budget: u64, current_usage: u64) -> Self {
        Self {
            budget,
            current_usage,
            current_reservation: 0,
            available_for_reservation: budget.saturating_sub(current_usage),
            fail_query: false,
        }
    }

    /// Report the configured values, or the injected failure.
    ///
    /// # Errors
    ///
    /// [`MemoryError::QueryFailed`] when `fail_query` is set.
    pub fn query(&self) -> Result<RawBudget, MemoryError> {
        if self.fail_query {
            return Err(MemoryError::QueryFailed {
                detail: "synthetic query failure injected (spec 16.12)".to_owned(),
            });
        }
        Ok(RawBudget {
            budget: self.budget,
            current_usage: self.current_usage,
            current_reservation: self.current_reservation,
            available_for_reservation: self.available_for_reservation,
        })
    }
}

/// Where budget samples come from. An enum, not a trait (#44 ruling 4): one implementor
/// per variant, and the synthetic path must run in the ordinary gate.
#[derive(Debug)]
pub enum BudgetSource {
    /// Live, correlated DXGI telemetry (spec 5.2's default feature).
    #[cfg(feature = "dxgi-telemetry")]
    Telemetry(dxgi::DxgiTelemetry),
    /// Deterministic test input (16.12).
    Synthetic(SyntheticBudget),
    /// No telemetry: internal accounting against a conservative estimate (16.6).
    Unavailable {
        /// Why telemetry is not answering.
        reason: String,
        /// The budget estimate policy runs against.
        estimated_budget: u64,
    },
}

/// What diagnostics report about telemetry (16.10's "telemetry availability").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TelemetryStatus {
    /// Correlated and live.
    Live(AdapterIdentity),
    /// Answering from a synthetic source (tests only).
    Synthetic,
    /// Degraded to internal estimates, with the reason.
    Unavailable(String),
}

/// Every refusal the memory manager produces (spec 16.11).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    /// 16.11: requested bytes, budget, usage and the failed resource's context.
    #[error("out of memory allocating {requested} bytes for {context}: budget {budget}, effective usage {usage}")]
    OutOfMemory {
        /// Bytes the allocation asked for.
        requested: u64,
        /// Budget in force at refusal.
        budget: u64,
        /// Effective usage at refusal.
        usage: u64,
        /// What was being allocated.
        context: String,
    },
    /// Preview degradation: an optional upload skipped this frame (16.7).
    #[error("optional upload of {requested} bytes deferred: {detail}")]
    UploadDeferred {
        /// Bytes the deferred upload asked for.
        requested: u64,
        /// Why it was deferred.
        detail: String,
    },
    /// 16.7: serial assumptions and residency records are invalid until reconstruction.
    #[error(
        "device lost ({reason}): residency and serials are invalid; reconstruct before allocating"
    )]
    DeviceLost {
        /// The loss reason the device callback recorded.
        reason: String,
    },
    /// An id the table never assigned.
    #[error("unknown resource {id}")]
    UnknownResource {
        /// The id that was asked for.
        id: ResourceId,
    },
    /// A record or transition the table's invariants refuse.
    #[error("invalid resource record: {detail}")]
    InvalidRecord {
        /// What broke.
        detail: String,
    },
    /// A policy whose arithmetic would be meaningless.
    #[error("invalid memory policy: {detail}")]
    InvalidPolicy {
        /// The field and value that broke.
        detail: String,
    },
    /// A budget source failure. The manager converts this to telemetry-unavailable
    /// (16.11) rather than surfacing it; it is public so synthetic sources can produce it.
    #[error("budget query failed: {detail}")]
    QueryFailed {
        /// Which query failed and how.
        detail: String,
    },
    /// Execution-time recheck: the plan named a record the exclusion list now bars.
    #[error("resource {id} is not evictable: {detail}")]
    EvictionIneligible {
        /// The record that was refused.
        id: ResourceId,
        /// Which exclusion fired.
        detail: String,
    },
    /// An state the module's own invariants exclude. Returned, never panicked.
    #[error("internal memory invariant violated in {stage}: {detail}")]
    Internal {
        /// Where it broke.
        stage: String,
        /// What broke.
        detail: String,
    },
}

impl MemoryError {
    /// Stable diagnostic code, matching the crate convention.
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        let raw = match self {
            Self::OutOfMemory { .. } => "memory.out_of_memory",
            Self::UploadDeferred { .. } => "memory.upload_deferred",
            Self::DeviceLost { .. } => "memory.device_lost",
            Self::UnknownResource { .. } => "memory.unknown_resource",
            Self::InvalidRecord { .. } => "memory.invalid_record",
            Self::InvalidPolicy { .. } => "memory.invalid_policy",
            Self::QueryFailed { .. } => "memory.query_failed",
            Self::EvictionIneligible { .. } => "memory.eviction_ineligible",
            Self::Internal { .. } => "memory.internal",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    /// A deferred upload asks for a retry next frame; everything else stops the caller.
    #[must_use]
    pub fn severity(&self) -> DiagnosticSeverity {
        match self {
            Self::UploadDeferred { .. } => DiagnosticSeverity::Warning,
            _ => DiagnosticSeverity::Error,
        }
    }

    /// The diagnostic form with 16.11's context carried as fields.
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        let base = Diagnostic::new(self.code(), self.severity(), self.to_string());
        match self {
            Self::OutOfMemory {
                requested,
                budget,
                usage,
                context,
            } => base
                .with_context("requested", requested.to_string())
                .with_context("budget", budget.to_string())
                .with_context("usage", usage.to_string())
                .with_context("context", context.clone()),
            Self::UnknownResource { id } | Self::EvictionIneligible { id, .. } => {
                base.with_context("resource", id.to_string())
            }
            _ => base,
        }
    }
}

impl From<MemoryError> for crate::RenderError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::OutOfMemory {
                requested,
                budget,
                usage,
                context,
            } => Self::OutOfMemory {
                purpose: context,
                requested,
                detail: format!("budget {budget}, effective usage {usage}"),
            },
            MemoryError::DeviceLost { reason } => Self::DeviceLost { reason },
            other => Self::Internal {
                stage: "memory".to_owned(),
                detail: other.to_string(),
            },
        }
    }
}

/// The cadence cache. `Instant` is disallowed workspace-wide to keep canonical paths
/// clock-free; this is the one scoped exception #44 ruling 5 records: telemetry is
/// advisory (16.4), `query_interval` is a `Duration`, and nothing here reaches
/// canonical bytes.
#[allow(
    clippy::disallowed_types,
    reason = "advisory telemetry cadence only (spec 16.4, DECISIONS.md #44 ruling 5); never reaches canonical bytes"
)]
#[derive(Debug, Default)]
struct QueryCache {
    last: Option<(std::time::Instant, BudgetSnapshot)>,
}

#[allow(
    clippy::disallowed_types,
    reason = "advisory telemetry cadence only (spec 16.4, DECISIONS.md #44 ruling 5); never reaches canonical bytes"
)]
impl QueryCache {
    fn fresh(&self, interval: Duration) -> Option<BudgetSnapshot> {
        self.last
            .and_then(|(when, snapshot)| (when.elapsed() < interval).then_some(snapshot))
    }

    fn store(&mut self, snapshot: BudgetSnapshot) {
        self.last = Some((std::time::Instant::now(), snapshot));
    }

    fn clear(&mut self) {
        self.last = None;
    }
}

/// GPU allocation accounting, telemetry, eviction and recovery (spec 16).
///
/// Owned by `anim_render` and consumed by application/export diagnostics (16.4). The
/// renderer constructs one per device, attaches telemetry once, and routes every
/// allocation through [`Self::preflight_allocation`].
#[derive(Debug)]
pub struct MemoryManager {
    policy: MemoryPolicy,
    mode: MemoryMode,
    table: ResourceTable,
    fences: SerialTracker,
    channel: CompletionChannel,
    reserved: ReservationCell,
    source: BudgetSource,
    estimate_fallback: u64,
    cache: QueryCache,
    hysteresis: Hysteresis,
    required: BTreeSet<ResourceId>,
    metrics: Cell<MetricsSnapshot>,
    loss: Option<String>,
}

impl MemoryManager {
    /// A manager over a budget source. The policy is validated here, not at first use.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidPolicy`] when the policy's arithmetic would be meaningless.
    pub fn new(
        policy: MemoryPolicy,
        mode: MemoryMode,
        source: BudgetSource,
    ) -> Result<Self, MemoryError> {
        policy.validate()?;
        let estimate_fallback = match &source {
            #[cfg(feature = "dxgi-telemetry")]
            BudgetSource::Telemetry(telemetry) => {
                telemetry.identity().dedicated_video_memory * 3 / 4
            }
            BudgetSource::Synthetic(synthetic) => synthetic.budget,
            BudgetSource::Unavailable {
                estimated_budget, ..
            } => *estimated_budget,
        };
        Ok(Self {
            policy,
            mode,
            table: ResourceTable::new(),
            fences: SerialTracker::new(),
            channel: CompletionChannel::default(),
            reserved: ReservationCell::new(),
            source,
            estimate_fallback,
            cache: QueryCache::default(),
            hysteresis: Hysteresis::new(),
            required: BTreeSet::new(),
            metrics: Cell::new(MetricsSnapshot::default()),
            loss: None,
        })
    }

    /// Correlate the rendering adapter and attach live telemetry, once per process
    /// (#18 decision 4). A correlation failure is **not** fatal: the source degrades to
    /// unavailable with the reason recorded (16.6), and the status reports it.
    #[cfg(feature = "dxgi-telemetry")]
    pub fn attach_dxgi(&mut self, vendor_id: u32, device_id: u32) {
        match dxgi::DxgiTelemetry::correlate(vendor_id, device_id) {
            Ok(telemetry) => {
                self.estimate_fallback = telemetry.identity().dedicated_video_memory * 3 / 4;
                self.source = BudgetSource::Telemetry(telemetry);
            }
            Err(error) => {
                self.source = BudgetSource::Unavailable {
                    reason: format!("DXGI correlation unavailable: {error}"),
                    estimated_budget: self.estimate_fallback,
                };
            }
        }
    }

    /// The policy in force.
    #[must_use]
    pub const fn policy(&self) -> &MemoryPolicy {
        &self.policy
    }

    /// The degradation regime.
    #[must_use]
    pub const fn mode(&self) -> MemoryMode {
        self.mode
    }

    /// The record table, for reports and tests.
    #[must_use]
    pub const fn table(&self) -> &ResourceTable {
        &self.table
    }

    /// An immutable copy of the counters (16.8).
    #[must_use]
    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.get()
    }

    /// What telemetry is doing right now.
    #[must_use]
    pub fn telemetry_status(&self) -> TelemetryStatus {
        match &self.source {
            #[cfg(feature = "dxgi-telemetry")]
            BudgetSource::Telemetry(telemetry) => {
                TelemetryStatus::Live(telemetry.identity().clone())
            }
            BudgetSource::Synthetic(_) => TelemetryStatus::Synthetic,
            BudgetSource::Unavailable { reason, .. } => {
                TelemetryStatus::Unavailable(reason.clone())
            }
        }
    }

    /// The last sample, without querying. `None` before the first query.
    #[must_use]
    pub fn last_snapshot(&self) -> Option<BudgetSnapshot> {
        self.cache.last.map(|(_, snapshot)| snapshot)
    }

    /// The synthetic source, if this manager runs on one — 16.12's shrinkage and
    /// competing-process scenarios mutate it between samples. `None` for live or
    /// unavailable sources; production code has no reason to call this.
    pub fn synthetic_mut(&mut self) -> Option<&mut SyntheticBudget> {
        match &mut self.source {
            BudgetSource::Synthetic(synthetic) => Some(synthetic),
            _ => None,
        }
    }

    /// Forget the cadence cache so the next call queries fresh. Synthetic scenarios call
    /// it after mutating the source; a budget change inside the query interval would
    /// otherwise stay invisible until the interval elapsed.
    pub fn invalidate_query_cache(&mut self) {
        self.cache.clear();
    }

    /// The completed-work watermark.
    #[must_use]
    pub const fn completed_serial(&self) -> u64 {
        self.fences.completed_serial()
    }

    fn record(&self, f: impl FnOnce(&mut MetricsSnapshot)) {
        let mut snapshot = self.metrics.get();
        f(&mut snapshot);
        self.metrics.set(snapshot);
    }

    fn check_loss(&self) -> Result<(), MemoryError> {
        match &self.loss {
            Some(reason) => Err(MemoryError::DeviceLost {
                reason: reason.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Sample the budget (16.9). Fresh queries respect `query_interval`; calls inside
    /// the interval reuse the cached source values with fresh internal accounting.
    /// A source failure degrades to unavailable and still returns a snapshot (16.11).
    ///
    /// # Errors
    ///
    /// [`MemoryError::DeviceLost`] when reconstruction has not completed.
    pub fn query_local_memory(&mut self) -> Result<BudgetSnapshot, MemoryError> {
        self.check_loss()?;
        let accounted = self.table.accounted_bytes();
        if let Some(cached) = self.cache.fresh(self.policy.query_interval) {
            let raw = RawBudget {
                budget: cached.budget,
                current_usage: cached.current_usage,
                current_reservation: cached.current_reservation,
                available_for_reservation: cached.available_for_reservation,
            };
            let snapshot = BudgetSnapshot::new(raw, accounted);
            self.record(|m| {
                m.cache_hits = m.cache_hits.saturating_add(1);
                m.observe_usage(accounted, snapshot.effective_usage);
            });
            return Ok(snapshot);
        }
        let (raw, estimated) = self.query_source(accounted);
        let snapshot = BudgetSnapshot::new(raw, accounted);
        self.cache.store(snapshot);
        self.record(|m| {
            m.queries = m.queries.saturating_add(1);
            if estimated {
                m.estimated_queries = m.estimated_queries.saturating_add(1);
            }
            m.observe_usage(accounted, snapshot.effective_usage);
        });
        self.hysteresis
            .sample(snapshot.over_budget(&self.policy), &self.policy);
        Ok(snapshot)
    }

    fn query_source(&mut self, accounted: u64) -> (RawBudget, bool) {
        let unavailable_estimate = match &self.source {
            BudgetSource::Unavailable {
                estimated_budget, ..
            } => Some(*estimated_budget),
            _ => None,
        };
        let queried = match &self.source {
            BudgetSource::Unavailable { .. } => None,
            #[cfg(feature = "dxgi-telemetry")]
            BudgetSource::Telemetry(telemetry) => Some(telemetry.query()),
            BudgetSource::Synthetic(synthetic) => Some(synthetic.query()),
        };
        match queried {
            Some(Ok(raw)) => (raw, false),
            Some(Err(error)) => {
                self.source = BudgetSource::Unavailable {
                    reason: error.to_string(),
                    estimated_budget: self.estimate_fallback,
                };
                (Self::internal_raw(self.estimate_fallback, accounted), true)
            }
            None => (
                Self::internal_raw(
                    unavailable_estimate.unwrap_or(self.estimate_fallback),
                    accounted,
                ),
                true,
            ),
        }
    }

    /// The unavailable-path sample: the estimate is the budget, and the only usage we
    /// can claim is our own (16.6's "continue with internal estimates").
    fn internal_raw(estimated_budget: u64, accounted: u64) -> RawBudget {
        RawBudget {
            budget: estimated_budget,
            current_usage: accounted,
            current_reservation: 0,
            available_for_reservation: estimated_budget.saturating_sub(accounted),
        }
    }

    /// Register a record after successful creation (16.9). The table assigns the id;
    /// a committed permit's reservation moves from reserved to accounted here.
    ///
    /// # Errors
    ///
    /// [`MemoryError::DeviceLost`] or [`MemoryError::InvalidRecord`].
    pub fn register_resource(&mut self, record: ResourceRecord) -> Result<ResourceId, MemoryError> {
        self.check_loss()?;
        let bytes = record.bytes;
        let id = self.table.insert(record)?;
        self.reserved.release(bytes);
        Ok(id)
    }

    /// Release a record entirely — a per-frame buffer whose work completed.
    pub fn release_resource(&mut self, id: ResourceId) -> Option<ResourceRecord> {
        self.table.remove(id)
    }

    /// Mark a use (16.9). An unknown id is counted, not panicked over: the signature
    /// 16.9 gives has no error channel (#44 ruling 8's precedent).
    pub fn mark_used(&mut self, id: ResourceId, serial: u64, frame: u64) {
        if self.table.mark_used(id, serial, frame).is_err() {
            self.record(|m| m.unknown_mark_used = m.unknown_mark_used.saturating_add(1));
        }
    }

    /// Pin or unpin a record — export pins active-frame resources (16.10).
    ///
    /// # Errors
    ///
    /// [`MemoryError::UnknownResource`].
    pub fn set_locked(&mut self, id: ResourceId, locked: bool) -> Result<(), MemoryError> {
        let record = self
            .table
            .get_mut(id)
            .ok_or(MemoryError::UnknownResource { id })?;
        record.locked = locked;
        Ok(())
    }

    /// Mark a CPU mapping live or finished — readback staging (16.7's mapped exclusion).
    ///
    /// # Errors
    ///
    /// [`MemoryError::UnknownResource`].
    pub fn set_mapped(&mut self, id: ResourceId, mapped: bool) -> Result<(), MemoryError> {
        let record = self
            .table
            .get_mut(id)
            .ok_or(MemoryError::UnknownResource { id })?;
        record.mapped = mapped;
        Ok(())
    }

    /// Assign the next submission serial (16.7's fence lifecycle).
    ///
    /// # Errors
    ///
    /// [`MemoryError::DeviceLost`] or a counter-exhaustion [`MemoryError::Internal`].
    pub fn begin_submission(&mut self) -> Result<u64, MemoryError> {
        self.check_loss()?;
        self.fences.assign()
    }

    /// Record that a submission reached the queue (16.9).
    pub fn on_submitted(&mut self, serial: u64) {
        self.fences.on_submitted(serial);
        self.sync_fence_metrics();
    }

    /// Roll back a serial whose frame failed before submission, so the next frame reuses
    /// it and the strict completion order has no gap to refuse.
    pub fn abort_submission(&mut self, serial: u64) {
        self.fences.abort(serial);
    }

    /// Advance the completed watermark in callback order (16.9).
    pub fn on_completed(&mut self, serial: u64) {
        self.fences.on_completed(serial);
        self.sync_fence_metrics();
    }

    /// The callback to hand `Queue::on_submitted_work_done` for a submission.
    pub fn submission_callback(&self, serial: u64) -> impl FnOnce() + Send + 'static {
        self.channel.callback(serial)
    }

    /// Drain known completions without blocking (#44 ruling 9). Returns how many
    /// serials arrived; the fence watermark advances only in order.
    pub fn drain_completions(&mut self) -> usize {
        self.record(|m| m.completion_drains = m.completion_drains.saturating_add(1));
        let count = self.channel.drain(&mut self.fences);
        self.sync_fence_metrics();
        count
    }

    fn sync_fence_metrics(&self) {
        let violations = self.fences.violations();
        self.record(|m| m.fence_order_violations = violations);
    }

    /// The current-frame required set — eviction's exclusion input (16.7).
    pub fn set_required(&mut self, required: BTreeSet<ResourceId>) {
        self.required = required;
    }

    /// The required set in force.
    #[must_use]
    pub const fn required(&self) -> &BTreeSet<ResourceId> {
        &self.required
    }

    /// Release one id from the required set — an explicit eviction is the caller's
    /// statement that the last frame's claim no longer holds. The next frame re-adds
    /// whatever it names.
    pub fn remove_required(&mut self, id: ResourceId) {
        self.required.remove(&id);
    }

    /// Plan eviction down to the release watermark (16.9). Pure with respect to the
    /// records: execution is [`Self::apply_eviction`], after the handles are dropped.
    #[must_use]
    pub fn plan_eviction(
        &self,
        snapshot: &BudgetSnapshot,
        required: &BTreeSet<ResourceId>,
    ) -> EvictionPlan {
        let release_target = snapshot
            .effective_usage
            .saturating_sub(snapshot.release_watermark(&self.policy));
        let plan = eviction::plan(
            snapshot,
            &self.table,
            self.fences.completed_serial(),
            required,
            release_target,
        );
        self.record(|m| m.evictions_planned = m.evictions_planned.saturating_add(1));
        plan
    }

    /// The hysteresis-driven pass (16.6): query, and if the over-budget streak has
    /// earned it, plan down to the release watermark. `None` when pressure has not
    /// accumulated or nothing needs releasing. The renderer calls this once a frame.
    ///
    /// # Errors
    ///
    /// [`MemoryError::DeviceLost`].
    pub fn pressure_eviction_plan(&mut self) -> Result<Option<EvictionPlan>, MemoryError> {
        let snapshot = self.query_local_memory()?;
        if !self.hysteresis.should_evict(&self.policy) {
            return Ok(None);
        }
        let release_target = snapshot
            .effective_usage
            .saturating_sub(snapshot.release_watermark(&self.policy));
        if release_target == 0 {
            return Ok(None);
        }
        let plan = eviction::plan(
            &snapshot,
            &self.table,
            self.fences.completed_serial(),
            &self.required,
            release_target,
        );
        self.record(|m| m.evictions_planned = m.evictions_planned.saturating_add(1));
        Ok(Some(plan))
    }

    /// Execute an eviction plan's record transitions after the owner dropped the
    /// handles. Each record is re-checked against 16.7's exclusions at execution time.
    ///
    /// # Errors
    ///
    /// [`MemoryError::EvictionIneligible`] when the world moved since the plan, and
    /// [`MemoryError::UnknownResource`] for an id the table does not have. Records
    /// transitioned before the refusal stay evicted — each transition is atomic.
    pub fn apply_eviction(&mut self, ids: &[ResourceId]) -> Result<u64, MemoryError> {
        let completed = self.fences.completed_serial();
        let mut released = 0_u64;
        for &id in ids {
            let bytes = self.table.evict(id, &self.required, completed)?;
            released = released.saturating_add(bytes);
            self.record(|m| {
                m.resources_evicted = m.resources_evicted.saturating_add(1);
                m.bytes_evicted = m.bytes_evicted.saturating_add(bytes);
            });
        }
        Ok(released)
    }

    /// Preflight an allocation (16.7, 16.9): fresh snapshot, reservation, and an
    /// eviction plan when projected usage crosses the hard trigger (the budget itself;
    /// the soft target drives hysteresis, ADR-0012). The permit rolls back on drop.
    ///
    /// # Errors
    ///
    /// [`MemoryError::OutOfMemory`] when nothing safe can be released and the mode
    /// forbids deferral; [`MemoryError::UploadDeferred`] for optional preview content;
    /// [`MemoryError::DeviceLost`] before reconstruction; [`MemoryError::InvalidRecord`]
    /// for zero bytes.
    pub fn preflight_allocation(
        &mut self,
        bytes: u64,
        priority: ResourcePriority,
    ) -> Result<AllocationPermit, MemoryError> {
        self.check_loss()?;
        if bytes == 0 {
            return Err(MemoryError::InvalidRecord {
                detail: "preflight for zero bytes".to_owned(),
            });
        }
        let snapshot = self.query_local_memory()?;
        let projected = snapshot
            .effective_usage
            .saturating_add(self.reserved.get())
            .saturating_add(bytes);
        let mut plan = EvictionPlan::default();
        if projected > snapshot.budget {
            // The bounded wait: known completions only, non-blocking. Draining advances
            // the watermark, which is what makes in-flight resources evictable.
            let drained = self.drain_completions();
            if drained == 0 {
                self.record(|m| {
                    m.empty_drains_under_pressure = m.empty_drains_under_pressure.saturating_add(1);
                });
            }
            let release_target = projected.saturating_sub(snapshot.release_watermark(&self.policy));
            plan = eviction::plan(
                &snapshot,
                &self.table,
                self.fences.completed_serial(),
                &self.required,
                release_target,
            );
            self.record(|m| m.evictions_planned = m.evictions_planned.saturating_add(1));
            if !plan.sufficient {
                // already_retried = true: the plan above was the one pass; recovery now
                // chooses defer-or-refuse, never a second plan (16.7).
                let action = recovery::plan_recovery(
                    self.mode,
                    &snapshot,
                    &self.table,
                    self.fences.completed_serial(),
                    &self.required,
                    bytes,
                    priority,
                    "preflight allocation",
                    release_target,
                    true,
                );
                match action {
                    RecoveryAction::DeferOptional { requested } => {
                        self.record(|m| m.uploads_deferred = m.uploads_deferred.saturating_add(1));
                        return Err(MemoryError::UploadDeferred {
                            requested,
                            detail: format!(
                                "budget {} cannot fit {requested} optional bytes",
                                snapshot.budget
                            ),
                        });
                    }
                    RecoveryAction::Refuse(context) => {
                        self.record(|m| m.oom_refusals = m.oom_refusals.saturating_add(1));
                        return Err(context.into_error());
                    }
                    RecoveryAction::RetryWithPlan(replan) => plan = replan,
                }
            }
        }
        self.reserved.reserve(bytes);
        Ok(AllocationPermit::new(
            self.reserved.share(),
            bytes,
            priority,
            plan,
        ))
    }

    /// Bytes currently reserved by live permits.
    #[must_use]
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved.get()
    }

    /// Select required evicted records within the frame budget (16.9). Selected
    /// records move to `Queued`; the caller re-uploads and confirms each.
    ///
    /// # Errors
    ///
    /// [`MemoryError::DeviceLost`] or [`MemoryError::UnknownResource`].
    pub fn reload_required(
        &mut self,
        required: &[ResourceId],
        frame_budget: u64,
    ) -> Result<ReloadBatch, MemoryError> {
        self.check_loss()?;
        let batch = streaming::plan_reload(
            &self.table,
            required,
            frame_budget,
            self.policy.max_upload_bytes_per_frame,
        )?;
        for &id in &batch.entries {
            self.table.queue_reload(id)?;
        }
        Ok(batch)
    }

    /// Confirm a reload finished and account its bytes again.
    ///
    /// # Errors
    ///
    /// [`MemoryError::UnknownResource`] or [`MemoryError::InvalidRecord`].
    pub fn confirm_reloaded(&mut self, id: ResourceId) -> Result<u64, MemoryError> {
        let bytes = self.table.confirm_reloaded(id)?;
        self.record(|m| {
            m.reloads = m.reloads.saturating_add(1);
            m.bytes_reloaded = m.bytes_reloaded.saturating_add(bytes);
        });
        Ok(bytes)
    }

    /// Device loss (16.7, 16.11): every residency record and serial assumption dies.
    /// Telemetry survives — the adapter outlives the device — but the loss latch refuses
    /// allocation until [`Self::reconstruction_complete`].
    pub fn on_device_loss(&mut self, reason: impl Into<String>) {
        self.table.clear();
        self.fences.invalidate();
        self.reserved.reset();
        self.required.clear();
        self.cache.clear();
        self.hysteresis.reset();
        self.loss = Some(reason.into());
    }

    /// The owner has re-registered targets and reload sources; allocation may resume.
    pub fn reconstruction_complete(&mut self) {
        self.loss = None;
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

    const MIB: u64 = 1024 * 1024;

    fn manager(budget: u64, usage: u64) -> MemoryManager {
        MemoryManager::new(
            MemoryPolicy::default(),
            MemoryMode::Export,
            BudgetSource::Synthetic(SyntheticBudget::new(budget, usage)),
        )
        .unwrap()
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
    fn effective_usage_is_the_max_of_observed_and_accounted() {
        let raw = RawBudget {
            budget: 1000,
            current_usage: 300,
            ..RawBudget::default()
        };
        assert_eq!(BudgetSnapshot::new(raw, 500).effective_usage, 500);
        assert_eq!(BudgetSnapshot::new(raw, 100).effective_usage, 300);
    }

    #[test]
    fn the_soft_target_is_the_min_of_fraction_and_reserve_saturating() {
        let policy = MemoryPolicy {
            target_fraction: 0.8,
            safety_reserve_bytes: 100,
            ..MemoryPolicy::default()
        };
        let raw = RawBudget {
            budget: 1000,
            ..RawBudget::default()
        };
        let snapshot = BudgetSnapshot::new(raw, 0);
        assert_eq!(snapshot.soft_target(&policy), 800);
        let tight = MemoryPolicy {
            safety_reserve_bytes: 500,
            ..policy
        };
        assert_eq!(snapshot.soft_target(&tight), 500);
        let absurd = MemoryPolicy {
            safety_reserve_bytes: 5000,
            ..policy
        };
        assert_eq!(snapshot.soft_target(&absurd), 0, "saturates, never wraps");
    }

    #[test]
    fn an_invalid_policy_is_refused_at_construction() {
        for fraction in [0.0, -0.5, 1.5, f32::NAN] {
            let policy = MemoryPolicy {
                target_fraction: fraction,
                ..MemoryPolicy::default()
            };
            assert!(MemoryManager::new(
                policy,
                MemoryMode::Export,
                BudgetSource::Synthetic(SyntheticBudget::new(1, 0))
            )
            .is_err());
        }
        let zero_samples = MemoryPolicy {
            over_budget_samples: 0,
            ..MemoryPolicy::default()
        };
        assert!(zero_samples.validate().is_err());
    }

    #[test]
    fn a_query_failure_degrades_to_unavailable_and_still_answers() {
        let mut synthetic = SyntheticBudget::new(100 * MIB, 0);
        synthetic.fail_query = true;
        let mut m = MemoryManager::new(
            MemoryPolicy::default(),
            MemoryMode::Export,
            BudgetSource::Synthetic(synthetic),
        )
        .unwrap();
        let snapshot = m.query_local_memory().unwrap();
        assert_eq!(
            snapshot.budget,
            100 * MIB,
            "the estimate is the last configured budget"
        );
        assert!(matches!(
            m.telemetry_status(),
            TelemetryStatus::Unavailable(_)
        ));
        assert_eq!(m.metrics().estimated_queries, 1);
    }

    #[test]
    fn preflight_reserves_and_drop_rolls_back() {
        let mut m = manager(100 * MIB, 0);
        {
            let permit = m
                .preflight_allocation(10 * MIB, ResourcePriority::CurrentFrame)
                .unwrap();
            assert_eq!(m.reserved_bytes(), 10 * MIB);
            assert!(permit.plan().is_empty());
        }
        assert_eq!(m.reserved_bytes(), 0);
    }

    #[test]
    fn commit_then_register_moves_reserved_to_accounted() {
        let mut m = manager(100 * MIB, 0);
        let permit = m
            .preflight_allocation(10 * MIB, ResourcePriority::CurrentFrame)
            .unwrap();
        permit.commit();
        assert_eq!(m.reserved_bytes(), 10 * MIB);
        m.register_resource(cache_record("tex", 10 * MIB)).unwrap();
        assert_eq!(m.reserved_bytes(), 0);
        assert_eq!(m.table().accounted_bytes(), 10 * MIB);
    }

    #[test]
    fn an_unfittable_required_allocation_is_an_actionable_oom() {
        let mut m = manager(10 * MIB, 9 * MIB);
        let err = m
            .preflight_allocation(8 * MIB, ResourcePriority::Critical)
            .unwrap_err();
        let MemoryError::OutOfMemory {
            requested,
            budget,
            usage,
            context,
        } = err
        else {
            panic!("required content must OOM, not defer");
        };
        assert_eq!(requested, 8 * MIB);
        assert_eq!(budget, 10 * MIB);
        assert!(usage >= 9 * MIB);
        assert!(!context.is_empty());
        assert_eq!(m.reserved_bytes(), 0, "a refusal reserves nothing");
        assert_eq!(m.metrics().oom_refusals, 1);
    }

    #[test]
    fn preview_defers_optional_content_where_export_refuses() {
        let mut preview = MemoryManager::new(
            MemoryPolicy::default(),
            MemoryMode::Preview,
            BudgetSource::Synthetic(SyntheticBudget::new(10 * MIB, 9 * MIB)),
        )
        .unwrap();
        let err = preview
            .preflight_allocation(8 * MIB, ResourcePriority::Cache)
            .unwrap_err();
        assert!(
            matches!(err, MemoryError::UploadDeferred { requested, .. } if requested == 8 * MIB)
        );
        assert_eq!(preview.metrics().uploads_deferred, 1);
    }

    #[test]
    fn pressure_plans_eviction_of_cache_but_never_the_required_set() {
        let mut m = manager(100 * MIB, 0);
        let pinned = m
            .register_resource(cache_record("pinned", 30 * MIB))
            .unwrap();
        let cache = m
            .register_resource(cache_record("cache", 30 * MIB))
            .unwrap();
        m.set_locked(pinned, true).unwrap();
        let mut required = BTreeSet::new();
        required.insert(pinned);
        m.set_required(required);
        // Shrink the budget until the streak earns a pass (16.12's shrinkage scenario).
        if let BudgetSource::Synthetic(s) = &mut m.source {
            s.budget = 50 * MIB;
            s.current_usage = 45 * MIB;
        }
        m.cache.clear();
        let mut plan = None;
        for _ in 0..MemoryPolicy::default().over_budget_samples {
            m.cache.clear();
            plan = m.pressure_eviction_plan().unwrap();
        }
        let plan = plan.expect("the streak must earn an eviction pass");
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].id, cache);
        let released = m.apply_eviction(&[cache]).unwrap();
        assert_eq!(released, 30 * MIB);
        assert_eq!(m.table().accounted_bytes(), 30 * MIB);
        assert!(m.table().get(pinned).unwrap().resident);
    }

    #[test]
    fn the_reload_round_trip_restores_accounting_exactly() {
        let mut m = manager(100 * MIB, 0);
        let id = m.register_resource(cache_record("tex", 10 * MIB)).unwrap();
        m.apply_eviction(&[id]).unwrap();
        assert_eq!(m.table().accounted_bytes(), 0);
        let batch = m.reload_required(&[id], 20 * MIB).unwrap();
        assert_eq!(batch.entries, vec![id]);
        assert_eq!(m.table().get(id).unwrap().reload_state, ReloadState::Queued);
        m.confirm_reloaded(id).unwrap();
        assert_eq!(m.table().accounted_bytes(), 10 * MIB);
        assert_eq!(m.metrics().reloads, 1);
        assert_eq!(m.metrics().bytes_reloaded, 10 * MIB);
    }

    #[test]
    fn device_loss_refuses_until_reconstruction_and_resets_the_serials() {
        let mut m = manager(100 * MIB, 0);
        let id = m.register_resource(cache_record("tex", 10 * MIB)).unwrap();
        let serial = m.begin_submission().unwrap();
        m.on_submitted(serial);
        m.on_completed(serial);
        m.on_device_loss("driver reset");
        assert!(m
            .preflight_allocation(MIB, ResourcePriority::Cache)
            .is_err());
        assert!(m.query_local_memory().is_err());
        assert!(m.table().is_empty());
        assert_eq!(m.completed_serial(), 0);
        assert_eq!(m.reserved_bytes(), 0);
        m.reconstruction_complete();
        let new_id = m.register_resource(cache_record("tex", 10 * MIB)).unwrap();
        assert_eq!(new_id.raw(), id.raw(), "ids restart with the rebuilt table");
        assert_eq!(m.begin_submission().unwrap(), 1);
    }

    #[test]
    fn an_unknown_mark_used_is_counted_not_absorbed() {
        let mut m = manager(100 * MIB, 0);
        m.mark_used(ResourceId(999), 1, 0);
        assert_eq!(m.metrics().unknown_mark_used, 1);
    }

    #[test]
    fn fence_violations_reach_the_metrics() {
        let mut m = manager(100 * MIB, 0);
        let s1 = m.begin_submission().unwrap();
        let s2 = m.begin_submission().unwrap();
        m.on_submitted(s1);
        m.on_submitted(s2);
        m.on_completed(s2);
        assert_eq!(m.completed_serial(), 0);
        assert_eq!(m.metrics().fence_order_violations, 1);
    }
}
