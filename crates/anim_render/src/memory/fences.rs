//! Submitted/completed serial tracking (spec 16.3, 16.7's fence lifecycle).
//!
//! Every queue submission takes a serial; a resource is evictable only once
//! `last_used_serial <= completed_serial`. Completions arrive through an `mpsc` channel
//! because `Mutex`/`RwLock` are disallowed workspace-wide and wgpu's
//! `on_submitted_work_done` callback is `FnOnce() + Send` (DECISIONS.md #44 ruling 8):
//! the callback sends its serial, the manager drains on the render thread, and the
//! tracker advances strictly in callback order. An out-of-order serial is counted, never
//! absorbed — 16.9's `on_completed` has no error channel, so the metric is the report.

use std::sync::mpsc::{self, Receiver, Sender};

use super::MemoryError;

/// Monotonic submission serials and the completed-work watermark.
///
/// Serial 0 means "never submitted": a fresh record's `last_used_serial` is 0 and the
/// initial completed watermark is 0, so an unused resource is evictable before the first
/// submission. The first real serial is 1.
#[derive(Debug, Default)]
pub struct SerialTracker {
    next_serial: u64,
    highest_submitted: u64,
    completed: u64,
    violations: u64,
}

impl SerialTracker {
    /// An tracker at the start of a process (or after device loss).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Assign the next submission serial.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Internal`] when the counter exhausts.
    pub fn assign(&mut self) -> Result<u64, MemoryError> {
        self.next_serial =
            self.next_serial
                .checked_add(1)
                .ok_or_else(|| MemoryError::Internal {
                    stage: "fences".to_owned(),
                    detail: "submission serial counter exhausted".to_owned(),
                })?;
        Ok(self.next_serial)
    }

    /// Record that a submission reached the queue. Serials must arrive in assignment
    /// order; a gap or regression is a violation, not an advance.
    pub fn on_submitted(&mut self, serial: u64) {
        if serial == self.highest_submitted + 1 && serial <= self.next_serial {
            self.highest_submitted = serial;
        } else {
            self.violations = self.violations.saturating_add(1);
        }
    }

    /// Advance the completed watermark in callback order (16.7). A serial that is not
    /// exactly `completed + 1`, or that was never submitted, counts as a violation and
    /// leaves the watermark where it was.
    pub fn on_completed(&mut self, serial: u64) {
        if serial == self.completed + 1 && serial <= self.highest_submitted {
            self.completed = serial;
        } else {
            self.violations = self.violations.saturating_add(1);
        }
    }

    /// The highest serial whose work is known complete.
    #[must_use]
    pub const fn completed_serial(&self) -> u64 {
        self.completed
    }

    /// The highest serial handed to the queue.
    #[must_use]
    pub const fn highest_submitted(&self) -> u64 {
        self.highest_submitted
    }

    /// How many out-of-order serials were refused. A nonzero figure is a diagnostic,
    /// never a silent correction.
    #[must_use]
    pub const fn violations(&self) -> u64 {
        self.violations
    }

    /// Device loss: every serial assumption dies (16.7). The next assignment is 1 again.
    pub fn invalidate(&mut self) {
        *self = Self::default();
    }

    /// Roll back an assigned serial whose frame failed before submission. Only the last
    /// assigned serial can be aborted, and only while it is unsubmitted; reusing it keeps
    /// `on_submitted`'s strict order intact. A frame that fails *after* submission keeps
    /// its serial — the completion callback is still coming.
    pub fn abort(&mut self, serial: u64) {
        if serial == self.next_serial && serial > self.highest_submitted {
            self.next_serial -= 1;
        }
    }
}

/// The channel pair the manager drains and the submission callback feeds.
#[derive(Debug)]
pub struct CompletionChannel {
    tx: Sender<u64>,
    rx: Receiver<u64>,
}

impl Default for CompletionChannel {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx }
    }
}

impl CompletionChannel {
    /// A callback for `Queue::on_submitted_work_done` that announces `serial`.
    ///
    /// The send is fire-and-forget: if the manager is gone there is nothing to advance,
    /// and a dropped receiver is not a fence error.
    pub fn callback(&self, serial: u64) -> impl FnOnce() + Send + 'static {
        let tx = self.tx.clone();
        move || {
            let _ = tx.send(serial);
        }
    }

    /// Drain every completion that has arrived, in arrival order, into the tracker.
    /// Non-blocking by construction — 16.11's "never wait indefinitely" and #44 ruling 9.
    /// Returns how many serials were delivered.
    pub fn drain(&mut self, tracker: &mut SerialTracker) -> usize {
        let mut count = 0;
        while let Ok(serial) = self.rx.try_recv() {
            tracker.on_completed(serial);
            count += 1;
        }
        count
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
    fn serials_start_at_one_and_advance_in_order() {
        let mut t = SerialTracker::new();
        assert_eq!(t.completed_serial(), 0);
        let s1 = t.assign().unwrap();
        let s2 = t.assign().unwrap();
        assert_eq!((s1, s2), (1, 2));
        t.on_submitted(s1);
        t.on_submitted(s2);
        assert_eq!(t.highest_submitted(), 2);
        t.on_completed(s1);
        assert_eq!(t.completed_serial(), 1);
        t.on_completed(s2);
        assert_eq!(t.completed_serial(), 2);
        assert_eq!(t.violations(), 0);
    }

    #[test]
    fn an_out_of_order_completion_never_advances_the_watermark() {
        let mut t = SerialTracker::new();
        let s1 = t.assign().unwrap();
        let s2 = t.assign().unwrap();
        t.on_submitted(s1);
        t.on_submitted(s2);
        t.on_completed(s2);
        assert_eq!(t.completed_serial(), 0, "s1 is still in flight");
        assert_eq!(t.violations(), 1);
        t.on_completed(s1);
        assert_eq!(t.completed_serial(), 1);
        t.on_completed(s2);
        assert_eq!(t.completed_serial(), 2);
    }

    #[test]
    fn a_completion_for_unsubmitted_work_is_a_violation() {
        let mut t = SerialTracker::new();
        t.on_completed(1);
        assert_eq!(t.completed_serial(), 0);
        assert_eq!(t.violations(), 1);
        let s1 = t.assign().unwrap();
        t.on_submitted(s1);
        t.on_submitted(99);
        assert_eq!(t.highest_submitted(), 1);
        assert_eq!(t.violations(), 2);
    }

    #[test]
    fn the_channel_delivers_in_submission_order_across_threads() {
        let mut channel = CompletionChannel::default();
        let mut t = SerialTracker::new();
        let s1 = t.assign().unwrap();
        let s2 = t.assign().unwrap();
        t.on_submitted(s1);
        t.on_submitted(s2);
        let cb1 = channel.callback(s1);
        let cb2 = channel.callback(s2);
        let handle = std::thread::spawn(move || {
            cb1();
            cb2();
        });
        handle.join().unwrap();
        assert_eq!(channel.drain(&mut t), 2);
        assert_eq!(t.completed_serial(), 2);
        assert_eq!(
            channel.drain(&mut t),
            0,
            "a second drain is empty, not a block"
        );
    }

    #[test]
    fn device_loss_resets_every_serial_assumption() {
        let mut t = SerialTracker::new();
        let s = t.assign().unwrap();
        t.on_submitted(s);
        t.on_completed(s);
        t.invalidate();
        assert_eq!(t.completed_serial(), 0);
        assert_eq!(t.highest_submitted(), 0);
        assert_eq!(t.assign().unwrap(), 1);
    }

    #[test]
    fn an_aborted_frame_reuses_its_serial_and_submitted_ones_cannot_be_aborted() {
        let mut t = SerialTracker::new();
        let s1 = t.assign().unwrap();
        t.on_submitted(s1);
        let s2 = t.assign().unwrap();
        t.abort(s2);
        assert_eq!(t.assign().unwrap(), s2, "the aborted serial is reused");
        t.on_submitted(s2);
        t.abort(s2);
        assert_eq!(
            t.assign().unwrap(),
            3,
            "a submitted serial is never rolled back"
        );
        t.on_completed(s1);
        t.on_completed(s2);
        assert_eq!(t.completed_serial(), 2);
        assert_eq!(t.violations(), 0);
    }
}
