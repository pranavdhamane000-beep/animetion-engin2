//! Atomic file replacement, shared by save and export (spec 4.2, 10.3).
//!
//! Two files, split by what they may expose. [`atomic_replace`] is the API every
//! later crate calls and contains no Win32 types; [`windows`] is `pub(crate)`
//! because spec 5.1's rule for the `windows` dependency is "no cross-platform API
//! leakage", and a `HANDLE` reaching a caller's signature would leak the platform
//! into code that is supposed to be portable.

pub mod atomic_replace;
pub(crate) mod windows;
