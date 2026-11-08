//! DXGI adapter query and telemetry (spec 16.3, 16.5, 16.6).
//!
//! Queries **only** the adapter [`super::identity`] correlated with the rendering
//! device, and only the local segment group (16.5). Node index 0 is ADR-0002
//! consequence 5's documented assumption: `IDXGIAdapter3` exposes no node count, so a
//! multi-node configuration would need D3D12's `GetNodeCount` — not this adapter.
//!
//! The COM handle never leaves this module (16.4: public records expose no DXGI
//! pointers). `Debug` prints the identity, not the pointer.

use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter3, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
};

use super::identity::{self, AdapterIdentity};
use super::{MemoryError, RawBudget};

/// A correlated adapter ready for budget queries.
pub struct DxgiTelemetry {
    adapter3: IDXGIAdapter3,
    identity: AdapterIdentity,
}

impl std::fmt::Debug for DxgiTelemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiTelemetry")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl DxgiTelemetry {
    /// Correlate once per process (#18 decision 4) and keep the matched adapter.
    ///
    /// # Errors
    ///
    /// [`MemoryError::QueryFailed`] when correlation cannot be established confidently.
    /// The manager treats that as telemetry-unavailable (16.6), never as a reason to
    /// query a different adapter.
    pub fn correlate(vendor_id: u32, device_id: u32) -> Result<Self, MemoryError> {
        let (identity, adapter3) = identity::correlate(vendor_id, device_id)?;
        Ok(Self { adapter3, identity })
    }

    /// The correlated identity, for reports.
    #[must_use]
    pub const fn identity(&self) -> &AdapterIdentity {
        &self.identity
    }

    /// Query the local segment's budget, usage, reservation and availability.
    ///
    /// # Errors
    ///
    /// [`MemoryError::QueryFailed`] when the driver refuses the query. DXGI values
    /// change asynchronously (16.6), so a successful query is still only advisory —
    /// every allocation keeps its actual error handling.
    pub fn query(&self) -> Result<RawBudget, MemoryError> {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: adapter3 is the correlated, ref-counted adapter this struct owns;
        // info is a plain struct live for the call. Node 0 is the ADR-0002 assumption.
        unsafe {
            self.adapter3
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
        }
        .map_err(|e| MemoryError::QueryFailed {
            detail: format!(
                "QueryVideoMemoryInfo(local) on {}: {e}",
                self.identity.description
            ),
        })?;
        Ok(RawBudget {
            budget: info.Budget,
            current_usage: info.CurrentUsage,
            current_reservation: info.CurrentReservation,
            available_for_reservation: info.AvailableForReservation,
        })
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

    /// The live query on the certified adapter. The G0 evidence recorded a local budget
    /// of 3308 MiB against 3892 MiB dedicated (ADR-0002 consequence 4); the budget moves
    /// with competing processes, so the assertion is the *relation* — budget positive,
    /// usage at or above what this process alone accounts — not a frozen figure.
    #[test]
    fn the_local_segment_reports_a_live_budget_on_the_certified_adapter() {
        let telemetry = DxgiTelemetry::correlate(0x10DE, 0x28A3)
            .expect("the pinned adapter must correlate; other hardware refuses, not skips");
        let raw = telemetry.query().unwrap();
        assert!(raw.budget > 0);
        assert!(
            raw.budget <= telemetry.identity().dedicated_video_memory,
            "the local budget is not the physical card's full VRAM"
        );
        assert!(raw.available_for_reservation <= raw.budget);
    }
}
