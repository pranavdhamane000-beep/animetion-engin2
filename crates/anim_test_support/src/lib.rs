//! Scene builders, deterministic runners, golden comparison and fault injection
//! (spec 4.2).
//!
//! Also hosts the WP00 platform probes (`probe.rs` and `src/bin/wp00_*.rs`). That
//! code is risk-reduction only: no production crate may import it, and no
//! production behaviour may be derived from it (spec 9.1, 9.10).

pub mod probe;
pub mod probe_tools;
pub mod wp04_fixture;
