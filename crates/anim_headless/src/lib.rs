//! CLI parsing, export execution, reports and stable exit codes (spec 4.2).
//!
//! WP00 shipped this crate as an empty `main`. WP05 adds the one thing its own gate needs:
//! [`replay`], which turns a document into a byte-stable state-hash report. A library target
//! exists alongside the binary so that target is reachable from
//! `crates/anim_test_support/src/bin/wp05_replay.rs` — the process the fresh-process half of
//! 14.12 spawns. `CARGO_BIN_EXE_` only names a binary in the *same* package, so without the
//! library the isolation test would have to copy the reporting code, and a copied report
//! format is a second thing that has to agree with the first.

pub mod replay;
