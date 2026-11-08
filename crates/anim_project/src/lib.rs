//! Versioned authoritative document, schema, migration, save and recovery
//! (spec 4.2).
//!
//! This crate owns the only copy of the project's state that is trusted. Everything
//! downstream — snapshots, simulation, render, audio, export, editor — reads a
//! revision of what is here and never mutates it (spec 12.6, 13.2). Mutations arrive
//! only as `anim_commands` envelopes applied by the `CommandProcessor`; this crate
//! provides the data, the validation that says whether a value is legal, and the
//! persistence that makes a revision durable.
//!
//! Spec 12.3 lists seven files and no `error.rs`, so [`ProjectError`] lives at the
//! crate root — the same resolution `anim_assets` reached for `AssetError`
//! (DECISIONS.md #29) rather than a second precedent for the same shape.
//!
//! [`event`] is the eighth file, added by WP04: spec 13.5 step 1 validates event
//! definitions and 13.7 orders their occurrences by authored fields, and no file 12.3
//! or 13.3 owns that data. Recorded as a deviation row in `BUILD_STATE.md`.
//!
//! [`simulation`] is the ninth, added by WP05 for the same reason: spec 14.2 makes
//! `ProjectDocument.simulation.ticks_per_frame` a WP05 prerequisite, 12.8 does not list a
//! simulation section, and a tick rate supplied per session instead would let one saved
//! revision replay to two different states.
//!
//! [`audio`] is the tenth, added by WP08 for the same reason again: spec 17.2 makes
//! authored audio-clip semantics a prerequisite and no section of the spec defines a type
//! that carries them, so the authored half lives here and `anim_scene` exposes it through
//! the snapshot (17.4 forbids `anim_audio` depending on this crate). DECISIONS.md #46.

pub mod audio;
pub mod document;
pub mod event;
pub mod migrate;
pub mod property;
pub mod recovery;
pub mod save;
pub mod scene;
pub mod schema;
pub mod simulation;

pub use anim_assets::{
    AssetKind, AssetManifest, AssetManifestEntry, ContentHash, MANIFEST_SCHEMA_VERSION,
};
use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};
use anim_core::error::CoreError;
pub use anim_core::id::{
    AssetId, EventDefinitionId, EventOccurrenceId, NodeId, Revision, SceneId, TrackId,
};
pub use anim_core::time::{FramePos, FrameRate};

pub use audio::{AudioClip, ChannelLayout};
pub use document::ProjectDocument;
pub use event::{EventDefinition, EventKind};
pub use property::{
    Easing, EasingCurve, PropertyCurve, PropertyKey, PropertyPath, PropertyValue,
    EASING_ALGORITHM_VERSION, EASING_CURVE_MAX_POINTS,
};
pub use scene::{Marker, NodeData, SceneData, TrackData};
pub use simulation::{
    SimulationSettings, DEFAULT_TICKS_PER_FRAME, INITIAL_PROJECT_SEED, PROJECT_SEED_BYTES,
};

/// The domain separator for the document's own canonical byte stream (ADR-0004
/// consequence 5, applied by ADR-0005's closure in this package).
///
/// The suffix is the *encoding* version and moves only when the byte layout moves;
/// [`crate::document::PROJECT_SCHEMA_VERSION`] is the *data* version and moves when a
/// field is added. ADR-0005 Decision 4 keeps those two numbers separate for the
/// manifest, and the document follows the same rule rather than inventing a third
/// convention.
pub const DOCUMENT_DOMAIN: &str = "anim_engine/document/v1";

/// The identifier namespace every derived id in this crate's fixtures is anchored to.
///
/// A constant rather than a per-test value because spec 10.8 says ids are "generated
/// once at creation and copied through save/load, undo and evaluation": two sessions
/// creating the same named scene must agree on its id, which a random namespace could
/// not do. This is the RFC 4122 DNS namespace, the same one `anim_core::id`'s tests and
/// `anim_assets/tests/store.rs` derive through. `uuid` is a dev-dependency here for
/// exactly that reason: the production path receives its ids from the caller, because
/// `anim_core` keeps the untyped `Uuid` behind the typed constructors.
pub const ID_NAMESPACE: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

/// The package's error taxonomy (spec 12.11).
///
/// The eleven categories spec 12.11 lists span both WP03 crates; each name appears
/// exactly once, in the crate that raises it. `CommandError` in `anim_commands`
/// carries `RevisionConflict`, `CommandReplayMismatch`, `EmptyBatch`,
/// `UnknownCommand`, `Validation`, `RevisionOverflow`, `UndoEmpty` and `RedoEmpty`,
/// and reaches the two listed here that it can also surface — `UnsupportedSchema` and
/// `MigrationFailure` — by chaining [`ProjectError`].
///
/// Two variants are additions, recorded the way DECISIONS.md #29 recorded `AssetError`'s
/// four rather than folded into a neighbour that would blame the wrong thing:
/// `AmbiguousLineage`, which spec 12.7 requires ("present recovery choices when
/// lineage is ambiguous") with no category for it, and the two chaining variants,
/// which spec 5.1 asks of thiserror ("stable variants and source chaining") and which
/// `Validation { detail }` would erase.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProjectError {
    /// A schema version this build cannot read. Names which schema, because spec 12.8
    /// has three independent version numbers — the document's, the manifest's inside
    /// it and the envelope's — and "unsupported schema" that does not say which one is
    /// a bug report the reader cannot act on.
    #[error(
        "{which} schema version {found} is not supported by this build (supported: {supported})"
    )]
    UnsupportedSchema {
        which: &'static str,
        found: u32,
        supported: u32,
    },

    /// A value that parses but is not a legal project: a dangling parent, a repeated
    /// property time, a track that targets no node.
    #[error("project state is not valid at {location}: {detail}")]
    Validation { location: String, detail: String },

    /// A migration step refused, or no step path between two versions.
    #[error("migration from schema version {from} to {to} failed: {detail}")]
    MigrationFailure { from: u32, to: u32, detail: String },

    /// A filesystem failure during save, backup or autosave, naming the operation.
    /// Spec 12.11 requires the original destination to survive each of these, which is
    /// what the boundary names in [`crate::save::Boundary`] are for.
    #[error("could not {context} {path}: {detail}")]
    Persistence {
        context: &'static str,
        path: String,
        detail: String,
    },

    /// Recovery found two candidates it may not choose between.
    ///
    /// A warning rather than an error, on `AssetError::Cancelled`'s reasoning
    /// (DECISIONS.md #29): being asked to choose is the behaviour spec 12.7 requires,
    /// not a fault. Reporting it as an error would teach an operator to ignore the
    /// one diagnostic that protects a newer manual save from an older autosave.
    #[error("recovery lineage is ambiguous: {detail}")]
    AmbiguousLineage { detail: String },

    /// A manifest failure reached through the embedded record collection. Kept typed
    /// rather than flattened into `Validation` because the asset id inside it is the
    /// answer to "which record", and spec 7.4 wants that machine-selectable.
    #[error(transparent)]
    Asset(#[from] anim_assets::AssetError),

    /// A checked-arithmetic or canonical-text failure from `anim_core`. Every rational,
    /// frame position and revision in this crate is built through those constructors.
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl ProjectError {
    /// The stable category for this error (spec 7.4). Append-only and dot-separated,
    /// matching `anim_core::error` and `anim_assets`.
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        let raw = match self {
            Self::UnsupportedSchema { .. } => "project.unsupported_schema",
            Self::Validation { .. } => "project.validation",
            Self::MigrationFailure { .. } => "project.migration_failure",
            Self::Persistence { .. } => "project.persistence",
            Self::AmbiguousLineage { .. } => "project.ambiguous_lineage",
            Self::Asset(_) => "project.asset",
            Self::Core(_) => "project.core",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    #[must_use]
    pub fn severity(&self) -> DiagnosticSeverity {
        match self {
            Self::AmbiguousLineage { .. } => DiagnosticSeverity::Warning,
            _ => DiagnosticSeverity::Error,
        }
    }

    /// The structured form a panel or a report renders (spec 7.4's safe context:
    /// ids, paths, expected and actual versions — never file contents).
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        let base = Diagnostic::new(self.code(), self.severity(), self.to_string());
        match self {
            Self::UnsupportedSchema {
                which,
                found,
                supported,
            } => base
                .with_context("schema", (*which).to_string())
                .with_context("found", found.to_string())
                .with_context("supported", supported.to_string()),
            Self::Validation { location, .. } => base.with_context("location", location.clone()),
            Self::MigrationFailure { from, to, .. } => base
                .with_context("from", from.to_string())
                .with_context("to", to.to_string()),
            Self::Persistence { path, context, .. } => base
                .with_context("path", path.clone())
                .with_context("context", (*context).to_string()),
            Self::AmbiguousLineage { .. } => base,
            Self::Asset(error) => error.to_diagnostic(),
            Self::Core(error) => error.to_diagnostic(),
        }
    }
}

/// The result type every fallible function in this crate returns.
pub type Result<T, E = ProjectError> = std::result::Result<T, E>;
