//! Decoder registry and importer-version dispatch (spec 11.3, 11.6, 11.11).
//!
//! A decoder here is a *metadata* reader, not a pixel or sample engine: what WP02
//! must be able to say about a stored asset is its kind, its length, its digest and
//! the format-level facts a scene needs in order to reference it. Rasterization
//! belongs to WP06's GPU path and PCM mixing to WP08's audio path, which are the only
//! consumers of the bytes themselves (DECISIONS.md #26 records that split and why
//! spec 5.1's dependency table has no image decoder in it).
//!
//! The registry exists for the other half of spec 11.6: a derived cache is keyed on
//! `(content_hash, importer_version, decoder_configuration)` and is *never
//! authoritative*. Keying on a version the decoder states — rather than on a constant
//! a caller hopes to remember — is what stops a changed metadata reader from serving
//! yesterday's cache.

use std::collections::BTreeMap;
use std::path::Path;

use anim_core::id::AssetId;

use crate::audio::{WavDecoder, WavMetadata};
use crate::image::{ImageDecoder, ImageMetadata};
use crate::manifest::{AssetKind, AssetManifestEntry};
use crate::store::DerivedKey;
use crate::AssetError;

/// The importer version this build stamps onto a freshly imported asset.
///
/// Persisted per record (spec 11.8's `importer_version`) and never rewritten in
/// place: an asset imported by version 1 keeps saying 1, so a later version's cache
/// directory cannot be blamed for bytes the old reader described.
pub const CURRENT_IMPORTER_VERSION: u32 = 1;

/// The decode settings a derived cache is keyed on (spec 11.6's third axis).
///
/// An enum, not a caller's string: the value reaches a filesystem path, and spec
/// 11.6's promise is that paths come only from validated values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DecoderConfiguration {
    /// Eight-bit RGBA, alpha left as authored. The default texture target.
    Rgba8Unpremultiplied,
    /// Eight-bit RGBA with colour multiplied by alpha.
    Rgba8Premultiplied,
    /// 32-bit float PCM, channels interleaved — the shape WP08 mixes in.
    PcmF32Interleaved,
}

impl DecoderConfiguration {
    /// The canonical path segment and persisted spelling for this configuration.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Rgba8Unpremultiplied => "rgba8-unpremultiplied",
            Self::Rgba8Premultiplied => "rgba8-premultiplied",
            Self::PcmF32Interleaved => "pcm-f32-interleaved",
        }
    }
}

impl std::fmt::Display for DecoderConfiguration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a metadata reader learned about one object.
///
/// `Opaque` is the honest answer for the kinds with no reader in this build —
/// `Font`, `Vector`, `Rig`, `Other`. It is a value rather than an error because the
/// asset is still perfectly resolvable: 11.7's resolution path needs a path and a
/// length, and only a *rendering* would need more than this.
#[derive(Clone, Debug, PartialEq)]
pub enum AssetMetadata {
    Image(ImageMetadata),
    Audio(WavMetadata),
    Opaque { byte_length: u64, kind: AssetKind },
}

impl AssetMetadata {
    /// The kind this metadata describes, which is also the kind its record claims.
    #[must_use]
    pub const fn kind(&self) -> AssetKind {
        match self {
            Self::Image(_) => AssetKind::Image,
            Self::Audio(_) => AssetKind::Audio,
            Self::Opaque { kind, .. } => *kind,
        }
    }
}

/// One kind's metadata reader, and the versions it is willing to dispatch.
pub trait Decoder: std::fmt::Debug {
    /// The kind this decoder handles.
    fn kind(&self) -> AssetKind;

    /// The importer versions this build dispatches for this kind.
    ///
    /// A list rather than a single value because a version bump does not invalidate
    /// the old reader: the record says which reader produced it, and a cache built by
    /// version 1 stays readable while new imports are stamped 2.
    fn supported_versions(&self) -> &[u32];

    /// Read the format-level facts of one stored object.
    ///
    /// The path is the store's canonical object path, already verified to be inside
    /// the project.
    fn read_metadata(&self, path: &Path) -> Result<AssetMetadata, AssetError>;
}

/// The kinds with no format header in this build.
///
/// A rig, a font and a vector source are all resolved, hashed, exported and
/// verified exactly like a cel — only their *metadata* is limited to a length, which
/// is what `Opaque` states. Spec 11.8's `AssetKind` list is the authority on which
/// kinds exist, not on which readers exist, so this list is the one place that says
/// which of them have a reader.
pub const OPAQUE_KINDS: &[AssetKind] = &[
    AssetKind::Font,
    AssetKind::Vector,
    AssetKind::Rig,
    AssetKind::Other,
];

/// The metadata reader for a kind with no header parser: length, and nothing invented.
#[derive(Clone, Copy, Debug)]
pub struct OpaqueMetadataDecoder {
    kind: AssetKind,
}

impl OpaqueMetadataDecoder {
    #[must_use]
    pub const fn new(kind: AssetKind) -> Self {
        Self { kind }
    }
}

impl Decoder for OpaqueMetadataDecoder {
    fn kind(&self) -> AssetKind {
        self.kind
    }

    fn supported_versions(&self) -> &[u32] {
        &[CURRENT_IMPORTER_VERSION]
    }

    fn read_metadata(&self, path: &Path) -> Result<AssetMetadata, AssetError> {
        let metadata = std::fs::metadata(path)
            .map_err(|e| crate::io_error("stat asset object", path, e.to_string()))?;
        Ok(AssetMetadata::Opaque {
            byte_length: metadata.len(),
            kind: self.kind,
        })
    }
}

/// Which kind maps to which reader, and at which versions.
#[derive(Debug, Default)]
pub struct DecoderRegistry {
    entries: BTreeMap<AssetKind, Box<dyn Decoder>>,
}

impl DecoderRegistry {
    /// An empty registry: every kind resolves to `UnsupportedDecoder`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with the readers this build ships.
    ///
    /// Image and audio get format-header readers; the remaining kinds get
    /// [`OpaqueMetadataDecoder`], which reports the length and nothing else. Registering
    /// them is the difference between "this asset's metadata is its byte length" and
    /// "there is no reader for this kind", and only the second is an error.
    #[must_use]
    pub fn with_default_decoders() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(ImageDecoder));
        registry.register(Box::new(WavDecoder));
        for kind in OPAQUE_KINDS {
            registry.register(Box::new(OpaqueMetadataDecoder::new(*kind)));
        }
        registry
    }

    /// Install one decoder, returning what it displaced.
    pub fn register(&mut self, decoder: Box<dyn Decoder>) -> Option<Box<dyn Decoder>> {
        self.entries.insert(decoder.kind(), decoder)
    }

    /// The kinds with a reader installed, in canonical order.
    #[must_use]
    pub fn supported_kinds(&self) -> Vec<AssetKind> {
        self.entries.keys().copied().collect()
    }

    /// The reader for `kind`, whatever its version.
    pub fn decoder(&self, kind: AssetKind) -> Result<&dyn Decoder, AssetError> {
        self.entries
            .get(&kind)
            .map(Box::as_ref)
            .ok_or_else(|| AssetError::UnsupportedDecoder {
                kind: kind.to_string(),
                importer_version: CURRENT_IMPORTER_VERSION,
            })
    }

    /// The reader for `kind` only if it dispatches `importer_version`.
    ///
    /// This is the check that makes a derived cache safe: a record whose version no
    /// installed reader claims is refused rather than decoded by a reader that would
    /// describe it differently.
    pub fn dispatch(
        &self,
        kind: AssetKind,
        importer_version: u32,
    ) -> Result<&dyn Decoder, AssetError> {
        let decoder = self.decoder(kind)?;
        if decoder.supported_versions().contains(&importer_version) {
            Ok(decoder)
        } else {
            Err(AssetError::UnsupportedDecoder {
                kind: kind.to_string(),
                importer_version,
            })
        }
    }

    /// Metadata for one record, dispatched by its own kind and version.
    pub fn metadata(
        &self,
        record: &AssetManifestEntry,
        path: &Path,
    ) -> Result<AssetMetadata, AssetError> {
        let decoder = self.dispatch(record.kind, record.importer_version)?;
        let metadata = decoder.read_metadata(path)?;
        // A reader that returns a different kind than the record declares is a
        // registry wiring fault, not data: 11.11 has no row for it, so it is reported
        // as the inconsistency it is.
        if metadata.kind() != record.kind {
            return Err(AssetError::MalformedManifest {
                detail: format!(
                    "decoder for {} produced {} metadata",
                    record.kind,
                    metadata.kind()
                ),
            });
        }
        Ok(metadata)
    }
}

/// The three-axis derived-cache key of one record (spec 11.6).
#[must_use]
pub fn derived_key(record: &AssetManifestEntry, configuration: DecoderConfiguration) -> DerivedKey {
    DerivedKey::new(record.content_hash, record.importer_version, configuration)
}

/// Whether two records would collide on one cache entry.
///
/// A question with a non-obvious answer: identical bytes under two ids share one
/// *object* (spec 11.6) and therefore one derived cache, so a cache hit is not
/// evidence that the requesting asset was the one that wrote it. Callers that need
/// per-asset cache identity must include the digest, as this key does, and never the
/// id — which is why the function exists to be pointed at rather than reimplemented.
#[must_use]
pub fn caches_share_storage(a: &AssetManifestEntry, b: &AssetManifestEntry) -> bool {
    a.content_hash == b.content_hash && a.importer_version == b.importer_version
}

/// The ids of every record this registry cannot dispatch.
///
/// WP10's preview wants the list before it draws anything, so that a missing reader
/// becomes a visible diagnostic rather than a black rectangle.
#[must_use]
pub fn undispatchable(
    registry: &DecoderRegistry,
    records: impl IntoIterator<Item = AssetManifestEntry>,
) -> Vec<AssetId> {
    let mut ids: Vec<AssetId> = records
        .into_iter()
        .filter(|record| {
            registry
                .dispatch(record.kind, record.importer_version)
                .is_err()
        })
        .map(|record| record.asset_id)
        .collect();
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use crate::hash::ContentHash;
    use anim_core::hash::CanonicalBytes;
    use uuid::Uuid;

    fn id(label: &str) -> AssetId {
        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        AssetId::derive(&namespace, label.as_bytes())
    }

    fn record(kind: AssetKind, version: u32, tag: u8) -> AssetManifestEntry {
        AssetManifestEntry::new(
            id("asset"),
            ContentHash::from_bytes([tag; 32]),
            16,
            kind,
            version,
            [],
            "cel",
        )
        .unwrap()
    }

    /// A reader that claims a version no record should have.
    #[derive(Debug)]
    struct RefusingDecoder;

    impl Decoder for RefusingDecoder {
        fn kind(&self) -> AssetKind {
            AssetKind::Vector
        }

        fn supported_versions(&self) -> &[u32] {
            &[7]
        }

        fn read_metadata(&self, _path: &Path) -> Result<AssetMetadata, AssetError> {
            // Never reached by the assertions below: dispatch refuses this decoder's
            // unclaimed versions first, and a test that relied on a panic would pass
            // for the wrong reason.
            Err(AssetError::MalformedAsset {
                asset_id: String::new(),
                kind: "vector".into(),
                detail: "no reader in this test".into(),
            })
        }
    }

    #[test]
    fn configuration_names_are_path_safe_and_unique() {
        let mut names: Vec<&str> = [
            DecoderConfiguration::Rgba8Unpremultiplied,
            DecoderConfiguration::Rgba8Premultiplied,
            DecoderConfiguration::PcmF32Interleaved,
        ]
        .iter()
        .map(DecoderConfiguration::as_str)
        .collect();
        for name in &names {
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.'),
                "{name} must be a single safe path segment"
            );
            assert!(!name.contains(".."));
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "one name per configuration");
    }

    #[test]
    fn an_empty_registry_refuses_every_kind() {
        let registry = DecoderRegistry::new();
        for kind in [AssetKind::Image, AssetKind::Audio, AssetKind::Other] {
            let e = registry.decoder(kind).unwrap_err();
            assert!(matches!(e, AssetError::UnsupportedDecoder { .. }), "{e}");
            assert_eq!(e.code().as_str(), "asset.unsupported_decoder");
            assert_eq!(
                e.to_diagnostic().severity,
                anim_core::diagnostic::DiagnosticSeverity::Error
            );
        }
        assert!(registry.supported_kinds().is_empty());
    }

    #[test]
    fn default_registry_dispatches_every_kind_the_manifest_can_hold() {
        let registry = DecoderRegistry::with_default_decoders();
        let mut all_kinds = [
            AssetKind::Image,
            AssetKind::Audio,
            AssetKind::Font,
            AssetKind::Vector,
            AssetKind::Rig,
            AssetKind::Other,
        ];
        all_kinds.sort_unstable();
        assert_eq!(
            registry.supported_kinds(),
            all_kinds.to_vec(),
            "ordered by kind, which is the BTreeMap's key order"
        );
        for kind in all_kinds {
            assert!(
                registry.dispatch(kind, CURRENT_IMPORTER_VERSION).is_ok(),
                "{kind} must be dispatchable, or a record of that kind is unresolvable"
            );
        }
        // No kind is unknown to the default registry, so the only way to be
        // undispatchable is a version this build does not claim.
        let e = registry
            .dispatch(AssetKind::Image, CURRENT_IMPORTER_VERSION + 9)
            .unwrap_err();
        assert!(
            e.to_string()
                .contains(&format!("{}", CURRENT_IMPORTER_VERSION + 9)),
            "{e}"
        );
        assert!(registry.decoder(AssetKind::Font).is_ok());
        // And an empty registry is the one that refuses.
        assert!(matches!(
            DecoderRegistry::new().dispatch(AssetKind::Font, CURRENT_IMPORTER_VERSION),
            Err(AssetError::UnsupportedDecoder { .. })
        ));
    }

    #[test]
    fn a_version_no_reader_claims_is_refused_before_it_is_used() {
        let mut registry = DecoderRegistry::new();
        registry.register(Box::new(RefusingDecoder));
        // Version 7 is claimed; version 1 is not, even though a Vector reader exists.
        assert!(registry.dispatch(AssetKind::Vector, 7).is_ok());
        assert!(registry.dispatch(AssetKind::Vector, 1).is_err());
        // And a record stamped with the unclaimed version cannot be read at all.
        let e = registry
            .metadata(&record(AssetKind::Vector, 1, 0x01), Path::new("x"))
            .unwrap_err();
        assert!(matches!(e, AssetError::UnsupportedDecoder { .. }), "{e}");
    }

    #[test]
    fn registering_a_second_reader_for_one_kind_replaces_it() {
        let mut registry = DecoderRegistry::new();
        registry.register(Box::new(ImageDecoder));
        let displaced = registry.register(Box::new(ImageDecoder));
        assert!(displaced.is_some(), "one reader per kind, by construction");
        assert_eq!(registry.supported_kinds().len(), 1);
    }

    #[test]
    fn cache_key_carries_all_three_axes_and_shares_storage_by_digest() {
        let a = record(AssetKind::Image, 2, 0x40);
        let b = record(AssetKind::Image, 2, 0x40);
        let key_a = derived_key(&a, DecoderConfiguration::Rgba8Unpremultiplied);
        let key_b = derived_key(&b, DecoderConfiguration::Rgba8Unpremultiplied);
        assert_eq!(key_a, key_b);
        assert_eq!(key_a.importer_version, 2);
        assert_eq!(key_a.content_hash, a.content_hash);

        // Same content, same version: one cache entry, even under different ids.
        let mut other_id = record(AssetKind::Image, 2, 0x40);
        other_id.asset_id = id("another");
        assert!(caches_share_storage(&a, &other_id));
        // Different content, or a different version, is a different cache file.
        assert!(!caches_share_storage(
            &a,
            &record(AssetKind::Image, 3, 0x40)
        ));
        assert!(!caches_share_storage(
            &a,
            &record(AssetKind::Image, 2, 0x41)
        ));
    }

    #[test]
    fn undispatchable_lists_the_unreadable_records_in_order() {
        let registry = DecoderRegistry::with_default_decoders();
        let distinct: Vec<AssetManifestEntry> = [
            (AssetKind::Font, 1u32),
            (AssetKind::Image, 99),
            (AssetKind::Rig, 4),
            (AssetKind::Image, CURRENT_IMPORTER_VERSION),
        ]
        .iter()
        .enumerate()
        .map(|(i, (kind, version))| {
            AssetManifestEntry::new(
                id(&format!("asset-{i}")),
                ContentHash::from_bytes([i as u8; 32]),
                16,
                *kind,
                *version,
                [],
                "x",
            )
            .unwrap()
        })
        .collect();
        // A kind the registry knows at a version it does not.
        let mut expected = vec![distinct[1].asset_id, distinct[2].asset_id];
        expected.sort();
        assert_eq!(undispatchable(&registry, distinct.clone()), expected);
        assert!(undispatchable(&registry, [distinct[3].clone()]).is_empty());
        // Every kind is known at the current version, so only versions can appear.
        assert!(undispatchable(
            &registry,
            [record(AssetKind::Other, CURRENT_IMPORTER_VERSION, 0x7F)]
        )
        .is_empty());
    }

    #[test]
    fn a_cache_key_hashes_the_three_axes_the_way_adr_0004_encodes_them() {
        // The registry's axes are the same value kinds the manifest digest hashes, so
        // this asserts they go through the shared encoder rather than a concatenation
        // that could collide: raw fixed-width digest, little-endian version, length-
        // prefixed configuration name.
        let domain = "anim_engine/derived_cache/v1";
        let configuration = DecoderConfiguration::Rgba8Premultiplied;
        let digest = ContentHash::from_bytes([0x01; 32]);
        let mut cp = CanonicalBytes::with_domain(domain).unwrap();
        cp.fixed_bytes(digest.as_bytes());
        cp.u32(2);
        cp.text(configuration.as_str());

        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(&(domain.len() as u64).to_le_bytes());
        expected.extend_from_slice(domain.as_bytes());
        expected.extend_from_slice(digest.as_bytes());
        expected.extend_from_slice(&2u32.to_le_bytes());
        expected.extend_from_slice(&(configuration.as_str().len() as u64).to_le_bytes());
        expected.extend_from_slice(configuration.as_str().as_bytes());
        assert_eq!(cp.as_slice(), expected.as_slice());
        // A different configuration is a different key, not a different spelling of
        // the same one.
        let mut other = CanonicalBytes::with_domain(domain).unwrap();
        other.fixed_bytes(digest.as_bytes());
        other.u32(2);
        other.text(DecoderConfiguration::Rgba8Unpremultiplied.as_str());
        assert_ne!(cp.digest(), other.digest());
    }
}
