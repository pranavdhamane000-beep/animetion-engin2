//! Device, graph, passes, GPU resources, memory policy and readback (spec 4.2).
//!
//! Owns the production GPU and DXGI code. Nothing in this crate imports a WP00
//! probe binary (spec 9.10); the probes prove capabilities this crate later uses.
