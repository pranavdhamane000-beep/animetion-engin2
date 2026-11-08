//! CLI parsing, export execution, reports and stable exit codes (spec 4.2).
//!
//! WP00 shipped this crate as an empty `main`. WP05 adds the one thing its own gate needs:
//! [`replay`], which turns a document into a byte-stable state-hash report. A library target
//! exists alongside the binary so that target is reachable from
//! `crates/anim_test_support/src/bin/wp05_replay.rs` — the process the fresh-process half of
//! 14.12 spawns. `CARGO_BIN_EXE_` only names a binary in the *same* package, so without the
//! library the isolation test would have to copy the reporting code, and a copied report
//! format is a second thing that has to agree with the first.
//!
//! WP07 adds [`budget_report`], the advisory memory-telemetry output 16.3 assigns here.
//! Unlike the replay report it makes no cross-process byte-stability claim — DXGI budgets
//! move with competing processes — and it is the reason this crate now depends on
//! `anim_render` (16.4's diagnostics consumer; deviation recorded at closeout).

pub mod budget_report;
pub mod replay;
