//! Native executable composition and shutdown (spec 4.2).
//!
//! WP00 shipped the entry point as a stub so the workspace has a real binary target.
//! Composition arrives in WP10; the editor is the only thing that may create a window.
//! WP07 adds [`settings`], the validated memory-policy configuration 16.3 assigns to this
//! crate, as a library module the composition will consume.

pub mod settings;
