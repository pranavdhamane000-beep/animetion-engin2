//! Session, transport, panels and preview quality (spec 4.2).
//!
//! Spec 4.2 also lists "pen strokes" here. Pen/ink input was removed from scope
//! on 2026-11-04 (`BUILD_STATE.md` Deviations #1, `DECISIONS.md` #8): the spec gives
//! a committed stroke no consumer anywhere in the pipeline, and artwork arrives
//! through WP02 as imported cels and named poses. Do not add `ink.rs` to this
//! crate.
