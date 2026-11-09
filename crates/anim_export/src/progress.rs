//! Progress and cancellation contracts (spec 18.3, 18.9, 18.10).
//!
//! `ProgressSink` reports stage and completed/total frames and, per 18.9, "cannot alter
//! timing or quality": every method here is fire-and-forget over an immutable snapshot, and
//! the pipeline never branches on what a sink returns. `CancellationToken` is the 18.10
//! cooperative stop — atomics only, so it stays lock-free like the WP08 preview clock and is
//! safe to trip from a UI or signal thread while the export loop spins on a different one.

use std::sync::atomic::{AtomicBool, Ordering};

/// The coarse pipeline stages 18.5 walks, reported so an operator sees where a long export
/// is rather than only a percentage. Order is fixed and matches the implementation sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Referenced source assets verified against the manifest before any frame is produced.
    VerifyAssets,
    /// Deterministic simulation warm-up / replay to the first export frame.
    PrepareSimulation,
    /// Offline canonical WAV rendered and its sample count checked.
    Audio,
    /// Per-frame evaluate → render → read back → pack → write to the encoder.
    Render,
    /// Encoder input closed and the process reaped.
    Encode,
    /// Video and audio mapped into the container without re-encoding.
    Mux,
    /// Output decoded and counted/metadata-checked.
    Verify,
    /// Same-volume temporary atomically published to the destination.
    Publish,
}

/// One immutable progress observation. `total_frames` is the resolved `N = end − first`
/// fixed when the job starts, so a sink can render a bar without knowing the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    pub completed_frames: u64,
    pub total_frames: u64,
}

/// Callback for stage/frame progress (18.9). Implementations must not block the export loop
/// for longer than the readback ring can absorb; the pipeline calls it once per frame boundary
/// at most. `Send + Sync` because a job may run on a worker while a UI thread owns the sink.
pub trait ProgressSink: Send + Sync {
    fn report(&self, progress: &Progress);
}

/// A sink that discards every observation — the default for tests and headless one-shot runs
/// that only read the final `ExportReport`.
pub struct NullProgress;

impl ProgressSink for NullProgress {
    fn report(&self, _progress: &Progress) {}
}

/// Cooperative cancellation (18.10, 18.11). `cancel` may be called from any thread; the
/// pipeline checks `is_cancelled` at each stage boundary and, in the frame loop, between
/// acquiring a readback slot and submitting the next frame. Cancellation is terminal: once
/// set, `execute` unwinds through the 18.10 shutdown path (close encoder input, terminate and
/// reap any child, unmap buffers, drop temporaries).
#[derive(Debug, Default)]
pub struct CancellationToken {
    cancelled: AtomicBool,
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the token. Idempotent.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only"
    )]

    use super::*;

    #[test]
    fn a_fresh_token_is_not_cancelled_and_cancel_is_terminal() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
        token.cancel(); // idempotent
        assert!(token.is_cancelled());
    }

    #[test]
    fn a_null_sink_accepts_every_stage() {
        // Compilation-level: the sink is object-safe and callable from a `&dyn`.
        let sink: &dyn ProgressSink = &NullProgress;
        for stage in [
            Stage::VerifyAssets,
            Stage::PrepareSimulation,
            Stage::Audio,
            Stage::Render,
            Stage::Encode,
            Stage::Mux,
            Stage::Verify,
            Stage::Publish,
        ] {
            sink.report(&Progress {
                stage,
                completed_frames: 3,
                total_frames: 10,
            });
        }
    }
}
