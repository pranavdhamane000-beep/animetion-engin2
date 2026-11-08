//! Asset import, verification, resolution, and decoder registry (spec 4.2, WP02).
//!
//! The package's whole claim is that an asset's *identity* and its *bytes* are two
//! different axes: `AssetId` survives a reimport whose content changed (spec 11.6),
//! while the content hash moves and produces a new manifest revision. Nothing here
//! mutates a project — [`import`] returns a record and the caller decides whether to
//! commit it (spec 11.4), which is what keeps the payload durable before any manifest
//! command references it.
//!
//! Module map, as spec 11.3 fixes it: [`hash`] is content hashing and the fixed-width
//! hex form, [`manifest`] is ordered records and the manifest digest, [`store`] is the
//! root-confined object layout and verification, [`import`] is the streaming
//! transactional import, [`resolver`] is read-only resolution and its diagnostics,
//! [`decode`] is the decoder registry and importer-version dispatch, and [`image`] and
//! [`audio`] are the metadata readers behind it.
//!
//! Types that WP01 already owns are imported, never re-derived: `AssetId` comes from
//! [`anim_core::id`], and every canonical byte layout — little-endian widths, `u64`
//! length prefixes, the domain tag first — comes from [`anim_core::hash`], because
//! spec 11.7's "length-prefixed canonical fields in order" is the same contract
//! ADR-0004 Decision 4 published.

pub mod audio;
pub mod decode;
pub mod hash;
pub mod image;
pub mod import;
pub mod manifest;
pub mod resolver;
pub mod store;

pub use audio::{SampleFormat, WavDecoder, WavMetadata};
pub use decode::{
    derived_key, AssetMetadata, Decoder, DecoderConfiguration, DecoderRegistry,
    OpaqueMetadataDecoder, CURRENT_IMPORTER_VERSION, OPAQUE_KINDS,
};
pub use hash::{hash_bytes, hash_file, hash_stream, ContentHash, StreamingHash};
pub use image::{
    classify_image, image_metadata, DecodedImage, EmbeddedProfile, ImageAlpha, ImageAssetClaims,
    ImageColorMarkers, ImageColorSpace, ImageDecoder, ImageFormat, ImageMetadata, PixelLayout,
    UNMODELLED_PROFILE_CODE,
};
pub use import::{import, import_observed};
pub use manifest::{
    manifest_digest, referenced_objects, AssetKind, AssetManifest, AssetManifestEntry,
    MANIFEST_DOMAIN, MANIFEST_SCHEMA_VERSION,
};
pub use resolver::{
    dependency_closure, resolve, resolve_with, validate_manifest, verify, verify_report,
    ResolutionPolicy, ResolvedAsset, VerificationReport,
};
pub use store::{AssetStore, DerivedKey};

use anim_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticSeverity};

/// Bounded chunk size for every stream this crate reads or writes (spec 11.7's
/// "read bounded chunks").
///
/// One constant for the whole crate so that "larger than memory buffers" means the
/// same thing in an import, a verification pass and a metadata read, and so the
/// 1 MiB asset in spec 11.12's test is provably handled in sixteen passes rather than
/// one allocation.
pub(crate) const CHUNK_SIZE: usize = 64 * 1024;

/// Every rejection this crate produces (spec 11.11).
///
/// The ten categories 11.11 names are all present. Four additions carry rejections
/// the list has no row for but the spec's own rules require:
/// [`AssetError::DuplicateDependency`] (11.7: "duplicate dependencies are rejected"),
/// [`AssetError::MalformedAsset`] (a stored object whose bytes do not match its
/// declared kind, which `image.rs` and `audio.rs` hit before any decoder is
/// registered), [`AssetError::MalformedManifest`] (a document that describes itself
/// inconsistently) and [`AssetError::SizeOverflow`] (11.7's "checked byte count").
/// Following the precedent `anim_core::error` set, additions are recorded rather than
/// smuggled in. In particular `DuplicateAssetId` is deliberately *not* reused for a
/// repeated dependency or an inconsistent document, because the ids involved name
/// different objects and a diagnostic that blames the wrong one sends the reader to
/// the wrong record.
///
/// [`AssetError::MissingImageClaims`] and [`AssetError::UnexpectedImageClaims`] are the
/// two that arrived with schema version 2's colour and alpha claims. Both are constructor
/// refusals rather than file faults — the record disagrees with itself, not with its
/// bytes — which is why neither one borrows `MalformedAsset`'s message.
///
/// [`AssetError::UnclassifiedImageClaims`] and
/// [`AssetError::IncompatibleDecodeConfiguration`] arrived with WP06's pixel decode
/// (DECISIONS.md #41 ruling 2). The first is the render-time refusal spec 15.11 requires:
/// a record whose colour space or alpha mode is `Unclassified` cannot be decoded without
/// guessing a transfer function, so the asset is named and the remedy (re-import to
/// classify) is stated rather than approximated. It is distinct from `MissingImageClaims`,
/// which names a record that carries *no* claims; this one carries a claim that declines to
/// answer. The second names a decode configuration that does not match the asset's kind —
/// an audio PCM target requested for an image — which no existing variant describes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AssetError {
    /// Filesystem failure, with the operation that attempted it. Never carries file
    /// contents, only the path and the OS message (spec 7.4's "safe context").
    #[error("{context}: {path}: {detail}")]
    Io {
        context: &'static str,
        path: String,
        detail: String,
    },

    /// The bytes at an object path are not the bytes the record promises.
    #[error("asset {asset_id} hashes to {actual}, not the {expected} its record states")]
    HashMismatch {
        asset_id: String,
        expected: String,
        actual: String,
    },

    /// The object's length disagrees with the record.
    #[error("asset {asset_id} is {actual} bytes on disk, not the {expected} its record states")]
    SizeMismatch {
        asset_id: String,
        expected: u64,
        actual: u64,
    },

    /// A content hash that is not exactly 64 lowercase hex characters.
    #[error("content hash is not a fixed-size lowercase hex digest: {raw}")]
    InvalidHash { raw: String },

    /// Two records claiming one `AssetId`, which only a construction from an
    /// unvalidated sequence can produce.
    #[error("asset id {asset_id} appears in more than one record")]
    DuplicateAssetId { asset_id: String },

    /// A lookup for an id the manifest does not hold.
    #[error("no manifest record for asset {asset_id}")]
    MissingAssetId { asset_id: String },

    /// A record whose object is not on disk. Distinct from a mismatch: one is a
    /// missing file an operator can restore, the other is corruption.
    #[error("asset {asset_id} resolves to {path}, which does not exist")]
    MissingObject { asset_id: String, path: String },

    /// A repeated dependency edge, which spec 11.7 rejects at construction.
    #[error("asset {asset_id} lists dependency {dependency} more than once")]
    DuplicateDependency {
        asset_id: String,
        dependency: String,
    },

    /// A dependency edge that reaches an id already on the path.
    #[error("asset dependency cycle: {path}")]
    DependencyCycle { asset_id: String, path: String },

    /// An `Image` record built with no colour or alpha claims, which spec 15.2's
    /// prerequisite makes a record the renderer cannot use. Kept separate from
    /// `MalformedAsset` because the bytes may be a perfectly good PNG: what is broken is
    /// the record, and the operator's remedy is to re-import, not to re-export the file.
    #[error("asset {asset_id} is an image but records no colour space or alpha mode")]
    MissingImageClaims { asset_id: String },

    /// Colour and alpha claims attached to a kind that has none. Distinct from the
    /// variant above and from `MalformedAsset` because it names a different object: the
    /// file is whatever its bytes are, and it is the record's *kind* or its claims that
    /// disagree — a manifest entry for a WAV saying "srgb straight" would send a
    /// diagnostic to the audio path.
    #[error("asset {asset_id} of kind {kind} cannot carry image colour and alpha claims")]
    UnexpectedImageClaims { asset_id: String, kind: String },

    /// A pixel decode requested against a record whose colour space or alpha mode is
    /// `Unclassified`. Spec 15.11 forbids a silent transfer-function fallback, and an
    /// inferred default is that fallback (DECISIONS.md #41 ruling 2), so the decode is
    /// refused rather than guessed. `axis` is the claim that declined to answer
    /// (`color_space` or `alpha`); the remedy is to re-import the asset so the importer
    /// classifies it, which is a different action from `MissingImageClaims`' absent record.
    #[error("asset {asset_id} records an unclassified {axis}; re-import the asset to classify it before decoding")]
    UnclassifiedImageClaims { asset_id: String, axis: String },

    /// A decode configuration that does not match the asset's kind — the audio PCM target
    /// requested for an image, or an image target for audio. No decoder can honour it, and
    /// the configuration is named so the caller sees which axis was wrong rather than a
    /// generic "unsupported" report.
    #[error(
        "asset {asset_id} of kind {kind} cannot be decoded with configuration {configuration}"
    )]
    IncompatibleDecodeConfiguration {
        asset_id: String,
        kind: String,
        configuration: String,
    },

    /// No decoder is registered for this kind, or the record's importer version is
    /// not one this build dispatches.
    #[error("no decoder registered for asset kind {kind} at importer version {importer_version}")]
    UnsupportedDecoder { kind: String, importer_version: u32 },

    /// The stored bytes do not match the declared kind: a header that is not the
    /// format it claims, or ends before the fields its format requires.
    #[error("asset {asset_id} is not a readable {kind}: {detail}")]
    MalformedAsset {
        asset_id: String,
        kind: String,
        detail: String,
    },

    /// The caller stopped an in-flight import. Like every other failure here it
    /// leaves no manifest entry and no temporary behind (spec 11.11).
    #[error("import of asset {asset_id} was cancelled")]
    Cancelled { asset_id: String },

    /// A canonical-encoding failure from the shared encoder in `anim_core`, or a
    /// manifest whose persisted shape disagrees with itself — a document that cannot
    /// be trusted rather than a file that is missing.
    #[error("manifest encoding is not canonical: {detail}")]
    MalformedManifest { detail: String },

    /// A streamed byte count that cannot be represented in the `u64` length the
    /// record stores. Spec 11.7 asks for a "checked byte count" precisely so this
    /// cannot wrap into a length that verifies against a shorter object.
    #[error("streamed byte count overflowed the u64 length field at {bytes} bytes")]
    SizeOverflow { bytes: u64 },
}

impl AssetError {
    /// Stable category (spec 7.4). Append-only and dot-separated, so a report written
    /// now stays readable after the next package adds variants.
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        let raw = match self {
            Self::Io { .. } => "asset.io",
            Self::HashMismatch { .. } => "asset.hash_mismatch",
            Self::SizeMismatch { .. } => "asset.size_mismatch",
            Self::InvalidHash { .. } => "asset.invalid_hash",
            Self::DuplicateAssetId { .. } => "asset.duplicate_asset_id",
            Self::MissingAssetId { .. } => "asset.missing_asset_id",
            Self::MissingObject { .. } => "asset.missing_object",
            Self::DuplicateDependency { .. } => "asset.duplicate_dependency",
            Self::DependencyCycle { .. } => "asset.dependency_cycle",
            Self::MissingImageClaims { .. } => "asset.missing_image_claims",
            Self::UnexpectedImageClaims { .. } => "asset.unexpected_image_claims",
            Self::UnclassifiedImageClaims { .. } => "asset.unclassified_image_claims",
            Self::IncompatibleDecodeConfiguration { .. } => {
                "asset.incompatible_decode_configuration"
            }
            Self::UnsupportedDecoder { .. } => "asset.unsupported_decoder",
            Self::MalformedAsset { .. } => "asset.malformed_asset",
            Self::Cancelled { .. } => "asset.cancelled",
            Self::MalformedManifest { .. } => "asset.malformed_manifest",
            Self::SizeOverflow { .. } => "asset.size_overflow",
        };
        DiagnosticCode::new(raw).unwrap_or_else(DiagnosticCode::unspecified)
    }

    /// `Cancelled` is a warning rather than an error: an operator who stopped an
    /// import did something supported, and a report that calls it an error teaches
    /// them to ignore reports.
    #[must_use]
    pub fn severity(&self) -> DiagnosticSeverity {
        match self {
            Self::Cancelled { .. } => DiagnosticSeverity::Warning,
            _ => DiagnosticSeverity::Error,
        }
    }

    /// The structured form WP03's command results and WP10's panels render.
    ///
    /// The identifiers are repeated as context fields rather than left in the
    /// message, because spec 7.4 wants them machine-selectable: "which asset" is a
    /// filter, not a substring search.
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        let base = Diagnostic::new(self.code(), self.severity(), self.to_string());
        match self {
            Self::Io { path, .. } => base.with_context("path", path.clone()),
            Self::HashMismatch {
                asset_id,
                expected,
                actual,
            } => base
                .with_context("asset_id", asset_id.clone())
                .with_context("expected", expected.clone())
                .with_context("actual", actual.clone()),
            Self::SizeMismatch {
                asset_id,
                expected,
                actual,
            } => base
                .with_context("asset_id", asset_id.clone())
                .with_context("expected", expected.to_string())
                .with_context("actual", actual.to_string()),
            Self::InvalidHash { raw } => base.with_context("raw", raw.clone()),
            Self::DuplicateAssetId { asset_id }
            | Self::MissingAssetId { asset_id }
            | Self::Cancelled { asset_id }
            | Self::MalformedAsset { asset_id, .. } => {
                base.with_context("asset_id", asset_id.clone())
            }
            Self::MissingObject { asset_id, path } => base
                .with_context("asset_id", asset_id.clone())
                .with_context("path", path.clone()),
            Self::DuplicateDependency {
                asset_id,
                dependency,
            } => base
                .with_context("asset_id", asset_id.clone())
                .with_context("dependency", dependency.clone()),
            Self::DependencyCycle { asset_id, path } => base
                .with_context("asset_id", asset_id.clone())
                .with_context("cycle", path.clone()),
            Self::MissingImageClaims { asset_id } => {
                base.with_context("asset_id", asset_id.clone())
            }
            Self::UnexpectedImageClaims { asset_id, kind } => base
                .with_context("asset_id", asset_id.clone())
                .with_context("kind", kind.clone()),
            Self::UnclassifiedImageClaims { asset_id, axis } => base
                .with_context("asset_id", asset_id.clone())
                .with_context("axis", axis.clone()),
            Self::IncompatibleDecodeConfiguration {
                asset_id,
                kind,
                configuration,
            } => base
                .with_context("asset_id", asset_id.clone())
                .with_context("kind", kind.clone())
                .with_context("configuration", configuration.clone()),
            Self::UnsupportedDecoder {
                kind,
                importer_version,
            } => base
                .with_context("kind", kind.clone())
                .with_context("importer_version", importer_version.to_string()),
            Self::SizeOverflow { bytes } => base.with_context("bytes", bytes.to_string()),
            Self::MalformedManifest { detail } => base.with_context("detail", detail.clone()),
        }
    }
}

/// Build an [`AssetError::Io`] from anything filesystem-shaped.
pub(crate) fn io_error(
    context: &'static str,
    path: &std::path::Path,
    detail: impl Into<String>,
) -> AssetError {
    AssetError::Io {
        context,
        path: path.display().to_string(),
        detail: detail.into(),
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

    /// One of every variant, so the taxonomy is checked as a set rather than as
    /// whichever arm a test happened to reach.
    fn every_variant() -> Vec<AssetError> {
        vec![
            AssetError::Io {
                context: "open",
                path: "D:\\p\\assets\\objects\\ab\\cd\\ff".into(),
                detail: "denied".into(),
            },
            AssetError::HashMismatch {
                asset_id: "a".into(),
                expected: "e".into(),
                actual: "x".into(),
            },
            AssetError::SizeMismatch {
                asset_id: "a".into(),
                expected: 1,
                actual: 2,
            },
            AssetError::InvalidHash { raw: "ABCD".into() },
            AssetError::DuplicateAssetId {
                asset_id: "a".into(),
            },
            AssetError::MissingAssetId {
                asset_id: "a".into(),
            },
            AssetError::MissingObject {
                asset_id: "a".into(),
                path: "p".into(),
            },
            AssetError::DuplicateDependency {
                asset_id: "a".into(),
                dependency: "b".into(),
            },
            AssetError::DependencyCycle {
                asset_id: "a".into(),
                path: "a -> b -> a".into(),
            },
            AssetError::MissingImageClaims {
                asset_id: "a".into(),
            },
            AssetError::UnexpectedImageClaims {
                asset_id: "a".into(),
                kind: "audio".into(),
            },
            AssetError::UnclassifiedImageClaims {
                asset_id: "a".into(),
                axis: "color_space".into(),
            },
            AssetError::IncompatibleDecodeConfiguration {
                asset_id: "a".into(),
                kind: "image".into(),
                configuration: "pcm-f32-interleaved".into(),
            },
            AssetError::UnsupportedDecoder {
                kind: "image".into(),
                importer_version: 3,
            },
            AssetError::MalformedAsset {
                asset_id: "a".into(),
                kind: "image".into(),
                detail: "no IHDR".into(),
            },
            AssetError::Cancelled {
                asset_id: "a".into(),
            },
            AssetError::MalformedManifest {
                detail: "record filed under another asset id".into(),
            },
            AssetError::SizeOverflow { bytes: u64::MAX },
        ]
    }

    #[test]
    fn every_variant_has_a_distinct_stable_code() {
        let mut codes: Vec<String> = every_variant()
            .iter()
            .map(|e| e.code().as_str().to_owned())
            .collect();
        assert_eq!(codes.len(), 18, "one arm per variant");
        codes.sort();
        let before = codes.len();
        codes.dedup();
        assert_eq!(before, codes.len(), "codes must distinguish variants");
        for code in &codes {
            assert!(code.starts_with("asset."), "prefix: {code}");
        }
    }

    #[test]
    fn diagnostics_carry_the_identifying_context_not_only_the_message() {
        // Spec 7.4: "which asset" must be filterable, so it appears as a field.
        let d = AssetError::MissingObject {
            asset_id: "id-1".into(),
            path: "D:\\p\\objects\\ab".into(),
        }
        .to_diagnostic();
        assert_eq!(d.code.as_str(), "asset.missing_object");
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        let fields: Vec<(&str, &str)> = d
            .context
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert!(
            fields.contains(&("asset_id", "id-1")),
            "context: {fields:?}"
        );
        assert!(fields.contains(&("path", "D:\\p\\objects\\ab")));
        assert!(Diagnostic::any_error(std::slice::from_ref(&d)));
    }

    #[test]
    fn cancellation_is_a_warning_and_everything_else_is_an_error() {
        let d = AssetError::Cancelled {
            asset_id: "id-1".into(),
        }
        .to_diagnostic();
        assert_eq!(d.severity, DiagnosticSeverity::Warning);
        for e in every_variant() {
            if matches!(e, AssetError::Cancelled { .. }) {
                continue;
            }
            assert_eq!(
                e.severity(),
                DiagnosticSeverity::Error,
                "unexpected severity for {e}"
            );
        }
    }

    #[test]
    fn messages_name_the_values_an_operator_asks_for() {
        // Spec 23's requirement, applied to this crate: the rejected values are
        // recoverable from the error text without re-deriving anything.
        let e = AssetError::HashMismatch {
            asset_id: "asset-1".into(),
            expected: "aa".into(),
            actual: "bb".into(),
        };
        for fragment in ["asset-1", "aa", "bb"] {
            assert!(e.to_string().contains(fragment), "{e}");
        }
        let e = AssetError::SizeMismatch {
            asset_id: "asset-1".into(),
            expected: 4096,
            actual: 4095,
        };
        assert!(e.to_string().contains("4096"), "{e}");
        assert!(e.to_string().contains("4095"), "{e}");
        let e = AssetError::UnsupportedDecoder {
            kind: "vector".into(),
            importer_version: 7,
        };
        assert!(e.to_string().contains("vector"), "{e}");
        assert!(e.to_string().contains('7'), "{e}");
        // The two claims refusals, which are the only way an operator hears about a
        // record built the wrong way round: which asset, and what its kind said.
        let e = AssetError::MissingImageClaims {
            asset_id: "asset-9".into(),
        };
        assert!(e.to_string().contains("asset-9"), "{e}");
        assert!(e.to_string().contains("image"), "{e}");
        let e = AssetError::UnexpectedImageClaims {
            asset_id: "asset-9".into(),
            kind: "audio".into(),
        };
        for fragment in ["asset-9", "audio"] {
            assert!(e.to_string().contains(fragment), "{e}");
        }
    }
}
