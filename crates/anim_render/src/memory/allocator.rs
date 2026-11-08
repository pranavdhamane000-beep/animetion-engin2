//! Allocation preflight, reservations and rollback (spec 16.3, 16.7, 16.8).
//!
//! [`AllocationPermit`] is 16.8's reservation token: it releases its bytes on drop and
//! commits only after successful resource creation. The shared counter is an
//! `Rc<Cell<u64>>` — the renderer is single-threaded (`&mut self` throughout the graph),
//! `Mutex`/`RwLock` are disallowed, and the permit must outlive the preflight borrow so
//! the caller can execute the eviction plan it carries before creating the resource.
//! The permit is deliberately `!Send`: it never leaves the render thread.

use std::cell::Cell;
use std::rc::Rc;

use super::eviction::EvictionPlan;
use super::resource::ResourcePriority;

/// The reservation counter shared between the manager and its live permits.
#[derive(Debug, Default)]
pub struct ReservationCell(Rc<Cell<u64>>);

impl ReservationCell {
    /// A zeroed counter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes currently reserved by uncommitted permits.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.get()
    }

    pub(super) fn reserve(&self, bytes: u64) {
        self.0.set(self.0.get().saturating_add(bytes));
    }

    pub(super) fn release(&self, bytes: u64) {
        self.0.set(self.0.get().saturating_sub(bytes));
    }

    /// Device loss or reconstruction: no permit survives, so the counter zeroes.
    pub fn reset(&self) {
        self.0.set(0);
    }

    pub(super) fn share(&self) -> Rc<Cell<u64>> {
        Rc::clone(&self.0)
    }
}

/// A reserved allocation that rolls back on drop (spec 16.8).
///
/// The flow 16.7 describes: preflight returns the permit, the caller executes
/// [`Self::plan`] if it names anything, attempts the wgpu creation, and calls
/// [`Self::commit`] on success. Any other path — creation failure, an early return, a
/// `?` — drops the permit and the reservation disappears with it.
#[derive(Debug)]
pub struct AllocationPermit {
    reserved: Rc<Cell<u64>>,
    bytes: u64,
    priority: ResourcePriority,
    plan: EvictionPlan,
    committed: bool,
}

impl AllocationPermit {
    pub(super) fn new(
        reserved: Rc<Cell<u64>>,
        bytes: u64,
        priority: ResourcePriority,
        plan: EvictionPlan,
    ) -> Self {
        Self {
            reserved,
            bytes,
            priority,
            plan,
            committed: false,
        }
    }

    /// Bytes this permit reserved.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The priority the allocation was requested at.
    #[must_use]
    pub const fn priority(&self) -> ResourcePriority {
        self.priority
    }

    /// The eviction plan the caller must execute before creating, if any.
    #[must_use]
    pub const fn plan(&self) -> &EvictionPlan {
        &self.plan
    }

    /// Mark the creation successful. Consumes the permit without releasing bytes; the
    /// manager's `register_resource` then moves the figure from reserved to accounted.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for AllocationPermit {
    fn drop(&mut self) {
        if !self.committed {
            self.reserved
                .set(self.reserved.get().saturating_sub(self.bytes));
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

    fn permit(cell: &ReservationCell, bytes: u64) -> AllocationPermit {
        cell.reserve(bytes);
        AllocationPermit::new(
            cell.share(),
            bytes,
            ResourcePriority::Cache,
            EvictionPlan::default(),
        )
    }

    #[test]
    fn drop_releases_the_reservation() {
        let cell = ReservationCell::new();
        {
            let _p = permit(&cell, 100);
            assert_eq!(cell.get(), 100);
        }
        assert_eq!(cell.get(), 0);
    }

    #[test]
    fn commit_keeps_the_reservation_for_registration_to_account() {
        let cell = ReservationCell::new();
        let p = permit(&cell, 100);
        p.commit();
        assert_eq!(
            cell.get(),
            100,
            "registration moves it to accounted, not drop"
        );
        cell.release(100);
        assert_eq!(cell.get(), 0);
    }

    #[test]
    fn two_permits_reserve_independently_and_one_rollback_leaves_the_other() {
        let cell = ReservationCell::new();
        let a = permit(&cell, 100);
        let b = permit(&cell, 200);
        assert_eq!(cell.get(), 300);
        drop(a);
        assert_eq!(cell.get(), 200);
        b.commit();
        assert_eq!(cell.get(), 200);
    }

    #[test]
    fn reset_zeroes_every_outstanding_reservation() {
        let cell = ReservationCell::new();
        let _a = permit(&cell, 100);
        cell.reset();
        assert_eq!(cell.get(), 0);
    }
}
