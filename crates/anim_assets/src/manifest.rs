//! Ordered manifest records and the manifest digest (spec 11.3, 11.7, 11.8).
//!
//! The manifest is the engine's inventory of what it can prove it has: one record per
//! asset, each naming the exact bytes its object must contain. Two properties carry
//! the whole package, and both are structural rather than polite convention.
//!
//! 1. **Order is not an accident.** Records live in a `BTreeMap` keyed by `AssetId`,
//!    whose order [`anim_core::id`] publishes as the 16 bytes, so the digest of a
//!    manifest cannot depend on the order records were inserted in (spec 11.12).
//! 2. **A record cannot be invalid, only refused.** Dependencies are sorted and
//!    deduplicated at construction (spec 11.7), so the field is private with a
//!    hand-written deserializer — the same resolution DECISIONS.md #22 reached for
//!    `FramePos::subframe`, for the same reason: a value that can be written into an
//!    unnormalized state gives one manifest two byte streams, and the digest of a
//!    saved file stops being the digest of that file.
//!
//! The encoding rules themselves — little-endian widths, `u64` length prefixes on
//! variable-length fields, raw fixed-width fields, the domain tag first — come from
//! [`anim_core::hash`], because spec 11.7's "length-prefixed canonical fields in
//! order" and ADR-0004 Decision 4 are the same contract. ADR-0005 records the
//! document-level choices this file makes: record framing, and why a schema version
//! and a domain tag are both hashed.

use std::collections::{btree_map, BTreeMap};

use anim_core::hash::CanonicalBytes;
use anim_core::id::AssetId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::hash::ContentHash;
use crate::image::ImageAssetClaims;
use crate::AssetError;

/// The manifest record encoding this build writes and calls current.
///
/// The value is part of the digest, so an unnoticed bump is a changed identity for
/// every project that has ever been hashed. Migrations belong to WP03's document
/// layer (spec 12.3's `migrate.rs`); this constant is what they migrate between.
///
/// Version 2 is WP06's: every record gained the `image` claims spec 15.2 makes a
/// renderer prerequisite. The bump is not cosmetic and is not meant to be — this number
/// is in the digest's pre-image, so *every* manifest's digest moves, including one that
/// holds no image record at all. That is the correct outcome and the point of the
/// number: a record that carries a colour claim is not the record that carried nothing,
/// and a project whose identity did not move would be hashed as though the engine still
/// meant the same thing by it. `the_schema_bump_moves_every_manifest_digest` is the
/// assertion a later auditor goes looking for.
pub const MANIFEST_SCHEMA_VERSION: u32 = 2;

/// The domain tag for a manifest digest (ADR-0004 consequence 5's shape).
///
/// Names the *encoding*, not the document version: `v1` here means "the record
/// framing ADR-0005 describes", and changes only if the byte layout changes, whereas
/// [`MANIFEST_SCHEMA_VERSION`] is a field inside those bytes.
///
/// Schema version 2 did not move this tag, and the distinction is the reason both
/// numbers exist. The framing — domain, `u32` version, `u64` record count, records back
/// to back in `AssetId` byte order with every field self-delimiting — is untouched; what
/// changed is one more field *inside* a record, appended last, so each field that was
/// there before keeps the byte position it had. ADR-0005 Consequence 2 is the rule that
/// keeps a domain tag with the layout and never with a field addition, and it is the same
/// rule WP04 applied to `DOCUMENT_DOMAIN` when it added the event section.
pub const MANIFEST_DOMAIN: &str = "anim_engine/manifest/v1";

/// The asset classes spec 11.8 names.
///
/// The lowercase spelling is the persisted form and the canonical-encoding form, so
/// there is one string per kind rather than a table of integers that a reordered
/// variant would silently redefine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    Image,
    Audio,
    Font,
    Vector,
    Rig,
    Other,
}

impl AssetKind {
    /// The canonical spelling, identical to the JSON form.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Font => "font",
            Self::Vector => "vector",
            Self::Rig => "rig",
            Self::Other => "other",
        }
    }
}

impl std::fmt::Display for AssetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One asset's claim about its own bytes.
///
/// `dependencies` is the only private field: it is the only one with an invariant
/// (sorted, deduplicated, no self-edge). The rest are data as spec 11.8 lists them, plus
/// `image`, which spec 11.8 does not list and spec 15.2 requires — recorded last because
/// it is the addition, and recorded as `Option` because only an image asset can make a
/// colour claim at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetManifestEntry {
    pub asset_id: AssetId,
    pub content_hash: ContentHash,
    pub byte_length: u64,
    pub kind: AssetKind,
    pub importer_version: u32,
    dependencies: Vec<AssetId>,
    pub display_name: String,
    /// The colour space and alpha mode recorded at import, and `None` for every other
    /// kind.
    ///
    /// This is the manifest's answer, not a hint: spec 15.2's prerequisite says image
    /// assets *carry* this metadata, and the renderer reads it here rather than
    /// re-sniffing the object's header. Re-reads at render time would put a second
    /// source of one fact beside this one, which gives a project two frames from one
    /// file — the failure class DECISIONS.md #22 and ADR-0005 exist to close.
    pub image: Option<ImageAssetClaims>,
}

impl AssetManifestEntry {
    /// Build one record, normalizing and validating its dependency list.
    ///
    /// # Errors
    ///
    /// [`AssetError::DuplicateDependency`] if `dependencies` repeats an id,
    /// [`AssetError::DependencyCycle`] if it names `asset_id` itself — the shortest
    /// cycle there is, and refusing it here means no traversal downstream has to
    /// special-case it — [`AssetError::MissingImageClaims`] if `kind` is
    /// [`AssetKind::Image`] with `image` absent, and [`AssetError::UnexpectedImageClaims`]
    /// in the other direction.
    pub fn new(
        asset_id: AssetId,
        content_hash: ContentHash,
        byte_length: u64,
        kind: AssetKind,
        importer_version: u32,
        dependencies: impl IntoIterator<Item = AssetId>,
        display_name: impl Into<String>,
        image: Option<ImageAssetClaims>,
    ) -> Result<Self, AssetError> {
        let mut deps: Vec<AssetId> = dependencies.into_iter().collect();
        deps.sort();
        if let Some(duplicate) = deps.windows(2).find(|w| w[0] == w[1]).map(|w| w[0]) {
            return Err(AssetError::DuplicateDependency {
                asset_id: asset_id.to_canonical_string(),
                dependency: duplicate.to_canonical_string(),
            });
        }
        if deps.binary_search(&asset_id).is_ok() {
            return Err(AssetError::DependencyCycle {
                asset_id: asset_id.to_canonical_string(),
                path: format!("{asset_id} -> {asset_id}"),
            });
        }
        // `image.is_some() == (kind == Image)`, checked rather than documented. Two of
        // the three mismatch shapes are reachable and each blames a different object: a
        // kind that promises colour data and carries none leaves the renderer with
        // nothing to read, and claims attached to a WAV describe bytes no colour rule
        // here ever produced.
        if kind == AssetKind::Image && image.is_none() {
            return Err(AssetError::MissingImageClaims {
                asset_id: asset_id.to_canonical_string(),
            });
        }
        if kind != AssetKind::Image && image.is_some() {
            return Err(AssetError::UnexpectedImageClaims {
                asset_id: asset_id.to_canonical_string(),
                kind: kind.to_string(),
            });
        }
        Ok(Self {
            asset_id,
            content_hash,
            byte_length,
            kind,
            importer_version,
            dependencies: deps,
            display_name: display_name.into(),
            image,
        })
    }

    /// The asset ids this record depends on, in canonical order.
    #[must_use]
    pub fn dependencies(&self) -> &[AssetId] {
        &self.dependencies
    }

    /// The record's canonical field stream, with no domain tag.
    ///
    /// Exposed because a record is also a unit WP03 embeds in a command payload, and
    /// a payload that hashes a record must hash these exact bytes rather than a
    /// second rendering of them.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AssetError> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp);
        Ok(cp.into_vec())
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.fixed_bytes(self.asset_id.as_bytes());
        cp.fixed_bytes(self.content_hash.as_bytes());
        cp.u64(self.byte_length);
        cp.text(self.kind.as_str());
        cp.u32(self.importer_version);
        // The dependency list is delimited by its own count, which is why the order
        // it was written in is part of the identity: `new` sorts it, so this is the
        // only order a record can hold.
        cp.u64(self.dependencies.len() as u64);
        for dependency in &self.dependencies {
            cp.fixed_bytes(dependency.as_bytes());
        }
        cp.text(&self.display_name);
        // The claims are a pure suffix, and that is a load-bearing property rather than
        // tidiness: every field above keeps the byte position ADR-0005 Decision 2 lists,
        // so a reader comparing a v2 record against the layout it migrated from can see
        // the addition is the whole difference. An absent claim is one `false` with
        // nothing after it, which is the same framing `NodeData::parent` uses for an
        // optional id — `None` is a state, not a zero value that could also be real.
        match &self.image {
            None => cp.bool(false),
            Some(claims) => {
                cp.bool(true);
                cp.text(claims.color_space.as_str());
                cp.text(claims.alpha.as_str());
            }
        }
    }
}

impl Serialize for AssetManifestEntry {
    /// Derived in effect: the field names are spec 11.8's, in that order, with the
    /// version 2 `image` claims last, and every value written came through
    /// [`AssetManifestEntry::new`].
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireEntry {
            asset_id: self.asset_id,
            content_hash: self.content_hash,
            byte_length: self.byte_length,
            kind: self.kind,
            importer_version: self.importer_version,
            dependencies: self.dependencies.clone(),
            display_name: self.display_name.clone(),
            image: self.image,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AssetManifestEntry {
    /// Hand-written so that persisted data passes the constructor's gate: a derived
    /// impl would write the private field directly, which is how a project file
    /// would smuggle an unsorted or duplicated dependency list in from disk.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = WireEntry::deserialize(deserializer)?;
        Self::new(
            raw.asset_id,
            raw.content_hash,
            raw.byte_length,
            raw.kind,
            raw.importer_version,
            raw.dependencies,
            raw.display_name,
            raw.image,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// The persisted shape of a record: spec 11.8's field names, in that order, plus the
/// claims last.
///
/// `image` deliberately has no `serde(default)`. A version 1 record simply did not have
/// the member, so filling one in on read would let an old file load as a current one with
/// claims nobody wrote — and the claims are the renderer's colour input, so guessing them
/// at the parse boundary is 15.11's forbidden silent fallback with a JSON field's face.
/// The only supported way in is [`crate::migrate`]'s version 3 to version 4 step, which
/// writes `unclassified` explicitly: an authored absence, visible in the file, and the
/// same strictness WP04 and WP05 applied to `events` and `simulation`.
#[derive(Serialize, Deserialize)]
struct WireEntry {
    asset_id: AssetId,
    content_hash: ContentHash,
    byte_length: u64,
    kind: AssetKind,
    importer_version: u32,
    dependencies: Vec<AssetId>,
    display_name: String,
    image: Option<ImageAssetClaims>,
}

/// The set of asset records a document revision refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetManifest {
    schema_version: u32,
    records: BTreeMap<AssetId, AssetManifestEntry>,
}

impl AssetManifest {
    /// An empty manifest at the current schema version. Spec 11.7: "Empty manifests
    /// are valid."
    #[must_use]
    pub fn new() -> Self {
        Self::with_schema_version(MANIFEST_SCHEMA_VERSION)
    }

    /// An empty manifest that claims to be at `schema_version`.
    ///
    /// Public because WP03's migrations build a manifest at the version they are
    /// migrating *to*, and the version is data the caller owns, not something this
    /// crate should second-guess. Nothing here validates the number against a
    /// supported range: that is a migration policy question and belongs with the
    /// migration chain.
    #[must_use]
    pub const fn with_schema_version(schema_version: u32) -> Self {
        Self {
            schema_version,
            records: BTreeMap::new(),
        }
    }

    /// Build from a sequence of records, rejecting two records for one id.
    ///
    /// The only way that can happen is an unvalidated construction — a deserialized
    /// array of records, or a hand-built list — which is exactly when it is worth
    /// reporting rather than letting the last one silently win. A *replacement* of an
    /// existing id is not this case: that is spec 11.6's reimport and it goes through
    /// [`AssetManifest::insert`].
    pub fn from_records(
        schema_version: u32,
        records: impl IntoIterator<Item = AssetManifestEntry>,
    ) -> Result<Self, AssetError> {
        let mut manifest = Self::with_schema_version(schema_version);
        for record in records {
            let asset_id = record.asset_id;
            if manifest.records.contains_key(&asset_id) {
                return Err(AssetError::DuplicateAssetId {
                    asset_id: asset_id.to_canonical_string(),
                });
            }
            manifest.records.insert(asset_id, record);
        }
        Ok(manifest)
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Insert or replace one record, returning what it displaced.
    ///
    /// The key comes from the record itself, so a record cannot be filed under another
    /// asset's id. Replacing is the normal operation here: spec 11.6 makes a reimport
    /// of the same `AssetId` with new bytes a new revision, not an error.
    pub fn insert(&mut self, record: AssetManifestEntry) -> Option<AssetManifestEntry> {
        let asset_id = record.asset_id;
        self.records.insert(asset_id, record)
    }

    pub fn remove(&mut self, asset_id: &AssetId) -> Option<AssetManifestEntry> {
        self.records.remove(asset_id)
    }

    #[must_use]
    pub fn get(&self, asset_id: &AssetId) -> Option<&AssetManifestEntry> {
        self.records.get(asset_id)
    }

    #[must_use]
    pub fn contains(&self, asset_id: &AssetId) -> bool {
        self.records.contains_key(asset_id)
    }

    /// Records in `AssetId` byte order, which is the order the digest hashes them in.
    pub fn iter(&self) -> btree_map::Values<'_, AssetId, AssetManifestEntry> {
        self.records.values()
    }

    /// Ids in the same order.
    pub fn asset_ids(&self) -> impl Iterator<Item = AssetId> + '_ {
        self.records.keys().copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Every dependency edge in the manifest, as (from, to) pairs in canonical order.
    ///
    /// Ordered by the pairs rather than by insertion so that a report — and a golden —
    /// over the dependency graph is reproducible.
    #[must_use]
    pub fn dependency_edges(&self) -> Vec<(AssetId, AssetId)> {
        let mut edges: Vec<(AssetId, AssetId)> = self
            .records
            .values()
            .flat_map(|record| {
                record
                    .dependencies()
                    .iter()
                    .map(move |dependency| (record.asset_id, *dependency))
            })
            .collect();
        edges.sort_unstable();
        edges
    }

    /// The full canonical byte stream the digest is taken over.
    ///
    /// Split out from [`manifest_digest`] because spec 11.12's shuffle test wants to
    /// compare bytes, not only digests, and because a report that publishes the
    /// pre-image lets a reviewer check a recorded digest without running the engine.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AssetError> {
        // The domain tag is a constant this file owns, so `with_domain` cannot
        // legitimately fail; `constant_is_a_valid_domain_tag` is what keeps that
        // statement honest, and the mapping exists so an unreachable branch reports
        // rather than panics.
        let mut cp = CanonicalBytes::with_domain(MANIFEST_DOMAIN).map_err(|e| {
            AssetError::MalformedManifest {
                detail: e.to_string(),
            }
        })?;
        cp.u32(self.schema_version);
        cp.u64(self.records.len() as u64);
        for record in self.records.values() {
            record.encode(&mut cp);
        }
        Ok(cp.into_vec())
    }
}

impl Default for AssetManifest {
    fn default() -> Self {
        Self::new()
    }
}

impl Serialize for AssetManifest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("AssetManifest", 2)?;
        state.serialize_field("schema_version", &self.schema_version)?;
        // A map keyed by the canonical id text, ordered by id bytes: `BTreeMap`'s own
        // iteration, so the JSON of a manifest is as reproducible as its digest.
        state.serialize_field("records", &self.records)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for AssetManifest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireManifest {
            schema_version: u32,
            records: BTreeMap<AssetId, AssetManifestEntry>,
        }

        let raw = WireManifest::deserialize(deserializer)?;
        let mut manifest = Self::with_schema_version(raw.schema_version);
        for (key, record) in raw.records {
            // The map key and the record's own id are the same value by
            // construction whenever the file was written by `Serialize` above; a
            // hand-edited file is where they can disagree, and guessing which one to
            // trust would redefine an asset.
            if record.asset_id != key {
                return Err(serde::de::Error::custom(AssetError::MalformedManifest {
                    detail: format!(
                        "record filed under {key} declares itself {asset_id}",
                        asset_id = record.asset_id
                    ),
                }));
            }
            manifest.insert(record);
        }
        Ok(manifest)
    }
}

/// The domain-separated digest of a manifest (spec 11.9).
///
/// Sort records by `AssetId` byte order, hash a domain separator, the schema version,
/// and each record's length-prefixed canonical fields in order. The sort is not a
/// step here: it is the `BTreeMap`'s invariant, which is why an insertion order that
/// varies between two processes cannot vary the result.
pub fn manifest_digest(manifest: &AssetManifest) -> Result<ContentHash, AssetError> {
    let bytes = manifest.canonical_bytes()?;
    Ok(ContentHash::from_bytes(*blake3::hash(&bytes).as_bytes()))
}

/// Every object this manifest claims exists, in canonical order.
///
/// WP09's export takes this list as the set it MUST verify before frame zero
/// (spec 11.7), and it is derived here so that the ordering rule lives in one place.
#[must_use]
pub fn referenced_objects(manifest: &AssetManifest) -> Vec<(AssetId, ContentHash, u64)> {
    manifest
        .iter()
        .map(|record| (record.asset_id, record.content_hash, record.byte_length))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use crate::image::{ImageAlpha, ImageColorSpace};
    use anim_core::id::AssetId;
    use uuid::Uuid;

    fn id(label: &str) -> AssetId {
        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        AssetId::derive(&namespace, label.as_bytes())
    }

    fn hash(tag: u8) -> ContentHash {
        ContentHash::from_bytes([tag; 32])
    }

    /// The claims an image record carries in these tests, as an `Option` so that a call
    /// site reads as the kind test it is: `claims()` for an image, `None` for everything
    /// else. Nothing here uses the migration's `Unclassified`, because a record built by
    /// this build's own rules describes a file whose header was read.
    fn claims() -> Option<ImageAssetClaims> {
        Some(ImageAssetClaims {
            color_space: ImageColorSpace::Srgb,
            alpha: ImageAlpha::Straight,
        })
    }

    fn record(label: &str, tag: u8) -> AssetManifestEntry {
        AssetManifestEntry::new(
            id(label),
            hash(tag),
            1024,
            AssetKind::Image,
            1,
            [],
            format!("{label}.png"),
            claims(),
        )
        .unwrap()
    }

    #[test]
    fn kind_spelling_is_one_string_for_json_and_for_hashing() {
        for kind in [
            AssetKind::Image,
            AssetKind::Audio,
            AssetKind::Font,
            AssetKind::Vector,
            AssetKind::Rig,
            AssetKind::Other,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(
                json,
                format!("\"{}\"", kind.as_str()),
                "the persisted and canonical spellings must be one string"
            );
            assert_eq!(serde_json::from_str::<AssetKind>(&json).unwrap(), kind);
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn dependencies_are_sorted_and_deduplicated_at_construction() {
        let deps = [id("z"), id("a"), id("m")];
        let entry = AssetManifestEntry::new(
            id("owner"),
            hash(0x11),
            10,
            AssetKind::Rig,
            2,
            deps,
            "rig.anim",
            None,
        )
        .unwrap();
        let mut sorted = deps.to_vec();
        sorted.sort();
        assert_eq!(entry.dependencies(), sorted);
        // A different input order is a different construction of the same record.
        let other = AssetManifestEntry::new(
            id("owner"),
            hash(0x11),
            10,
            AssetKind::Rig,
            2,
            [id("m"), id("z"), id("a")],
            "rig.anim",
            None,
        )
        .unwrap();
        assert_eq!(
            entry.canonical_bytes().unwrap(),
            other.canonical_bytes().unwrap()
        );
    }

    #[test]
    fn a_repeated_dependency_is_refused_not_collapsed() {
        // Spec 11.7: "duplicate dependencies are rejected". Silently deduplicating
        // would let a command payload and the record it produced disagree about how
        // many edges the author asked for.
        let e = AssetManifestEntry::new(
            id("owner"),
            hash(0x11),
            10,
            AssetKind::Image,
            1,
            [id("dep"), id("dep")],
            "a.png",
            claims(),
        )
        .unwrap_err();
        assert!(matches!(e, AssetError::DuplicateDependency { .. }), "{e}");
        assert_eq!(e.code().as_str(), "asset.duplicate_dependency");
    }

    #[test]
    fn a_self_dependency_is_the_shortest_cycle_and_is_refused() {
        let e = AssetManifestEntry::new(
            id("owner"),
            hash(0x11),
            10,
            AssetKind::Image,
            1,
            [id("owner")],
            "a.png",
            claims(),
        )
        .unwrap_err();
        assert!(matches!(e, AssetError::DependencyCycle { .. }), "{e}");
        assert!(e.to_string().contains(&id("owner").to_string()), "{e}");
    }

    #[test]
    fn a_record_survives_json_only_through_the_validating_constructor() {
        let entry = AssetManifestEntry::new(
            id("asset"),
            hash(0x40),
            1024,
            AssetKind::Rig,
            1,
            [id("dep"), id("cel")],
            "asset.png",
            None,
        )
        .unwrap();
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            serde_json::from_str::<AssetManifestEntry>(&json).unwrap(),
            entry
        );

        // The persisted form cannot hold an unnormalized dependency list: the
        // deserializer runs the same gate the constructor does.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let deps = value["dependencies"].as_array().unwrap().clone();
        value["dependencies"] = serde_json::Value::Array([deps[0].clone(), deps[0].clone()].into());
        let smuggled = serde_json::to_string(&value).unwrap();
        assert!(
            serde_json::from_str::<AssetManifestEntry>(&smuggled).is_err(),
            "a duplicated dependency list must not load"
        );

        // And a kind spelled another way is not a kind.
        let bad_kind = json
            .replace("\"rig\"", "\"Rig\"")
            .replace("\"image\"", "\"Image\"");
        assert!(serde_json::from_str::<AssetManifestEntry>(&bad_kind).is_err());
    }

    #[test]
    fn a_version_1_record_is_refused_rather_than_read_as_version_2() {
        // The strictness rule WP04 established for `events` and WP05 extended to
        // `simulation`, now applied to `image`: the member has no `serde(default)`, so a
        // record written before the bump cannot load as one written after it. The
        // temptation this closes is specific — an absent colour claim has a plausible
        // default (`srgb`, `opaque`), and filling one in at the parse boundary would give
        // the renderer a colour space no author and no importer ever chose.
        let image = AssetManifestEntry::new(
            id("asset"),
            hash(0x40),
            1024,
            AssetKind::Image,
            1,
            [],
            "cel.png",
            claims(),
        )
        .unwrap();
        let mut value = serde_json::to_value(&image).unwrap();
        value.as_object_mut().unwrap().remove("image");
        let v1 = serde_json::to_string(&value).unwrap();
        let e = serde_json::from_str::<AssetManifestEntry>(&v1).unwrap_err();
        assert!(e.to_string().contains("image"), "{e}");

        // The `null` a non-image record writes *is* the version 2 spelling of absence, so
        // dropping the member is the only way to state version 1. The two half-mismatched
        // forms are what the constructor refuses, and the persisted route has to reach
        // them: claims on a kind that cannot carry them, and none on a kind that must.
        let with_claims = serde_json::to_value(&image).unwrap();
        let mut kind_switched = with_claims.clone();
        kind_switched["kind"] = serde_json::json!("rig");
        let e = serde_json::from_str::<AssetManifestEntry>(&kind_switched.to_string()).unwrap_err();
        assert!(
            e.to_string().contains("cannot carry image"),
            "claims under a non-image kind: {e}"
        );
    }

    #[test]
    fn image_claims_are_present_if_and_only_if_the_kind_is_image() {
        // The two halves of the rule are different mistakes and are reported as two
        // categories, because the operator's remedy differs: one record is missing data
        // that a re-import supplies, the other describes a file whose kind was written
        // wrong.
        let e = AssetManifestEntry::new(
            id("asset"),
            hash(0x40),
            1024,
            AssetKind::Image,
            1,
            [],
            "cel.png",
            None,
        )
        .unwrap_err();
        assert!(matches!(e, AssetError::MissingImageClaims { .. }), "{e}");
        assert_eq!(e.code().as_str(), "asset.missing_image_claims");
        assert!(e.to_string().contains(&id("asset").to_string()), "{e}");

        let e = AssetManifestEntry::new(
            id("drum"),
            hash(0x41),
            1024,
            AssetKind::Audio,
            1,
            [],
            "drum.wav",
            claims(),
        )
        .unwrap_err();
        assert!(matches!(e, AssetError::UnexpectedImageClaims { .. }), "{e}");
        assert_eq!(e.code().as_str(), "asset.unexpected_image_claims");
        assert!(e.to_string().contains(&id("drum").to_string()), "{e}");
        assert!(e.to_string().contains("audio"), "{e}");
    }

    #[test]
    fn digest_is_independent_of_insertion_order() {
        // Spec 11.12's named test. Records are keyed by id, so a manifest built by
        // three different processes in three different orders hashes alike.
        let labels = ["a", "b", "c", "d", "e", "f", "g"];
        let base = labels.iter().map(|l| record(l, 0x20)).collect::<Vec<_>>();

        let forward = AssetManifest::from_records(MANIFEST_SCHEMA_VERSION, base.clone()).unwrap();
        let mut reversed = base.clone();
        reversed.reverse();
        let backward = AssetManifest::from_records(MANIFEST_SCHEMA_VERSION, reversed).unwrap();

        let mut shuffled = base;
        // A fixed shuffle, not a random one: the assertion has to be reproducible
        // when it fails, and there is no entropy source in this graph anyway.
        shuffled.rotate_left(3);
        shuffled.swap(0, 5);
        let shuffle = AssetManifest::from_records(MANIFEST_SCHEMA_VERSION, shuffled).unwrap();

        assert_eq!(
            manifest_digest(&forward).unwrap(),
            manifest_digest(&backward).unwrap()
        );
        assert_eq!(
            manifest_digest(&forward).unwrap(),
            manifest_digest(&shuffle).unwrap()
        );
        assert_eq!(
            forward.canonical_bytes().unwrap(),
            backward.canonical_bytes().unwrap()
        );
    }

    #[test]
    fn empty_manifest_is_valid_and_distinct_from_nothing() {
        let empty = AssetManifest::new();
        assert!(empty.is_empty());
        let digest = manifest_digest(&empty).unwrap();
        assert_ne!(
            digest.as_bytes(),
            &[0u8; 32],
            "a real digest, not an uninitialized one"
        );
        // One record must not hash like zero records, which a missing count prefix
        // would allow.
        let one =
            AssetManifest::from_records(MANIFEST_SCHEMA_VERSION, [record("only", 0x01)]).unwrap();
        assert_ne!(digest, manifest_digest(&one).unwrap());
    }

    #[test]
    fn digest_changes_with_every_field_that_defines_a_record() {
        let baseline =
            AssetManifest::from_records(MANIFEST_SCHEMA_VERSION, [record("asset", 0x30)]).unwrap();
        let base_digest = manifest_digest(&baseline).unwrap();

        let variants: Vec<AssetManifest> = {
            let mut list = Vec::new();

            // Content.
            let mut m = AssetManifest::new();
            m.insert(
                AssetManifestEntry::new(
                    id("asset"),
                    hash(0x31),
                    1024,
                    AssetKind::Image,
                    1,
                    [],
                    "asset.png",
                    claims(),
                )
                .unwrap(),
            );
            list.push(m);

            // Length, with the same hash.
            let mut m = AssetManifest::new();
            m.insert(
                AssetManifestEntry::new(
                    id("asset"),
                    hash(0x30),
                    1025,
                    AssetKind::Image,
                    1,
                    [],
                    "asset.png",
                    claims(),
                )
                .unwrap(),
            );
            list.push(m);

            // Kind.
            let mut m = AssetManifest::new();
            m.insert(
                AssetManifestEntry::new(
                    id("asset"),
                    hash(0x30),
                    1024,
                    AssetKind::Audio,
                    1,
                    [],
                    "asset.png",
                    None,
                )
                .unwrap(),
            );
            list.push(m);

            // Importer version.
            let mut m = AssetManifest::new();
            m.insert(
                AssetManifestEntry::new(
                    id("asset"),
                    hash(0x30),
                    1024,
                    AssetKind::Image,
                    2,
                    [],
                    "asset.png",
                    claims(),
                )
                .unwrap(),
            );
            list.push(m);

            // Display name: metadata, but the record says it, so the digest does too.
            let mut m = AssetManifest::new();
            m.insert(
                AssetManifestEntry::new(
                    id("asset"),
                    hash(0x30),
                    1024,
                    AssetKind::Image,
                    1,
                    [],
                    "renamed.png",
                    claims(),
                )
                .unwrap(),
            );
            list.push(m);

            // Dependencies.
            let mut m = AssetManifest::new();
            m.insert(
                AssetManifestEntry::new(
                    id("asset"),
                    hash(0x30),
                    1024,
                    AssetKind::Rig,
                    1,
                    [id("dep")],
                    "asset.png",
                    None,
                )
                .unwrap(),
            );
            list.push(m);

            // The colour claim, which is the whole reason version 2 exists: two records of
            // one file read differently about its pixels are two records, and the renderer
            // has to be able to tell them apart from the digest alone.
            let mut m = AssetManifest::new();
            m.insert(
                AssetManifestEntry::new(
                    id("asset"),
                    hash(0x30),
                    1024,
                    AssetKind::Image,
                    1,
                    [],
                    "asset.png",
                    Some(ImageAssetClaims {
                        color_space: ImageColorSpace::Linear,
                        alpha: ImageAlpha::Straight,
                    }),
                )
                .unwrap(),
            );
            list.push(m);

            // And alpha, on its own, because the two halves are encoded as two fields and
            // one of them moving must not require the other to move.
            let mut m = AssetManifest::new();
            m.insert(
                AssetManifestEntry::new(
                    id("asset"),
                    hash(0x30),
                    1024,
                    AssetKind::Image,
                    1,
                    [],
                    "asset.png",
                    Some(ImageAssetClaims {
                        color_space: ImageColorSpace::Srgb,
                        alpha: ImageAlpha::Opaque,
                    }),
                )
                .unwrap(),
            );
            list.push(m);

            // Schema version, without touching a record. Version 1 rather than version 2,
            // because `MANIFEST_SCHEMA_VERSION` *is* 2 now — and the fact that this line
            // had to move is the version bump's own proof. `an_unreadable_manifest_version_
            // inside_a_current_document_is_refused` in `anim_project` is the load-path half.
            list.push(AssetManifest::from_records(1, [record("asset", 0x30)]).unwrap());
            list
        };

        for variant in variants {
            assert_ne!(
                base_digest,
                manifest_digest(&variant).unwrap(),
                "digest must move with {}",
                variant
                    .iter()
                    .next()
                    .map_or("schema version", |r| r.display_name.as_str())
            );
        }
    }

    #[test]
    fn digest_layout_is_what_adr_0005_says_it_is() {
        // Rebuilt by hand from the documented rules, the way `anim_core`'s layout
        // test does it, so an encoder that drifts from its own contract is caught here
        // rather than by a golden that merely agrees with itself.
        let entry = AssetManifestEntry::new(
            id("asset"),
            hash(0xAB),
            7,
            AssetKind::Audio,
            3,
            [id("dep")],
            "drum.wav",
            None,
        )
        .unwrap();
        let manifest =
            AssetManifest::from_records(MANIFEST_SCHEMA_VERSION, [entry.clone()]).unwrap();
        let bytes = manifest.canonical_bytes().unwrap();

        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(&(MANIFEST_DOMAIN.len() as u64).to_le_bytes());
        expected.extend_from_slice(MANIFEST_DOMAIN.as_bytes());
        expected.extend_from_slice(&MANIFEST_SCHEMA_VERSION.to_le_bytes());
        expected.extend_from_slice(&1u64.to_le_bytes()); // record count
        expected.extend_from_slice(entry.asset_id.as_bytes());
        expected.extend_from_slice(entry.content_hash.as_bytes());
        expected.extend_from_slice(&7u64.to_le_bytes());
        let kind = "audio";
        expected.extend_from_slice(&(kind.len() as u64).to_le_bytes());
        expected.extend_from_slice(kind.as_bytes());
        expected.extend_from_slice(&3u32.to_le_bytes());
        expected.extend_from_slice(&1u64.to_le_bytes()); // dependency count
        expected.extend_from_slice(id("dep").as_bytes());
        let name = "drum.wav";
        expected.extend_from_slice(&(name.len() as u64).to_le_bytes());
        expected.extend_from_slice(name.as_bytes());
        // ADR-0005 Decision 2's stream, and then version 2's one addition: a presence
        // flag, and for a non-image record nothing after it. The bytes above are exactly
        // what version 1 wrote, so this test is also the proof that the suffix is a
        // suffix — an earlier field that moved position would part with `expected` here.
        expected.push(0);

        assert_eq!(bytes, expected);
        assert_eq!(
            manifest_digest(&manifest).unwrap(),
            ContentHash::from_bytes(*blake3::hash(&expected).as_bytes())
        );
    }

    #[test]
    fn recorded_claims_follow_the_display_name_and_are_two_prefixed_texts() {
        // The other half of the same claim, for the branch the layout test above cannot
        // reach: a record that *does* carry claims. Rebuilt by hand, in the order ADR-0005
        // Decision 2 fixes plus the claims last.
        let entry = AssetManifestEntry::new(
            id("cel"),
            hash(0x9C),
            40,
            AssetKind::Image,
            1,
            [],
            "cel-001.png",
            Some(ImageAssetClaims {
                color_space: ImageColorSpace::Linear,
                alpha: ImageAlpha::Straight,
            }),
        )
        .unwrap();
        let mut expected = entry.asset_id.as_bytes().to_vec();
        expected.extend_from_slice(entry.content_hash.as_bytes());
        expected.extend_from_slice(&40u64.to_le_bytes());
        let kind = "image";
        expected.extend_from_slice(&(kind.len() as u64).to_le_bytes());
        expected.extend_from_slice(kind.as_bytes());
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&0u64.to_le_bytes()); // no dependencies
        let name = "cel-001.png";
        expected.extend_from_slice(&(name.len() as u64).to_le_bytes());
        expected.extend_from_slice(name.as_bytes());
        expected.push(1); // the claims are present
        for text in ["linear", "straight"] {
            expected.extend_from_slice(&(text.len() as u64).to_le_bytes());
            expected.extend_from_slice(text.as_bytes());
        }

        let bytes = entry.canonical_bytes().unwrap();
        assert_eq!(bytes, expected);
        // The spelling is the JSON spelling, so there is one string per claim rather than
        // a table between the two forms — ADR-0005 Decision 5's rule, extended.
        assert_eq!(
            ImageColorSpace::Linear.as_str(),
            "linear",
            "the canonical and persisted colour spellings are one string"
        );
        assert_eq!(ImageAlpha::Straight.as_str(), "straight");
    }

    #[test]
    fn the_schema_bump_moves_every_manifest_digest() {
        // The assertion a later auditor wants, because it is the one that cannot be
        // checked by reading the record: version 2 is in the digest's pre-image, so a
        // manifest that holds not one image record — nothing but fonts — still hashes to a
        // different value than it did under version 1. If this ever stops being true, the
        // bump was cosmetic and the projects saved before it are indistinguishable from
        // projects saved after, which is the state ADR-0005 Consequence 1 exists to avoid.
        let records = [AssetManifestEntry::new(
            id("font"),
            hash(0x01),
            512,
            AssetKind::Font,
            1,
            [],
            "arial.ttf",
            None,
        )
        .unwrap()];
        let before = AssetManifest::from_records(1, records.clone()).unwrap();
        let now = AssetManifest::from_records(MANIFEST_SCHEMA_VERSION, records).unwrap();
        assert_ne!(
            manifest_digest(&before).unwrap(),
            manifest_digest(&now).unwrap(),
            "the version number is hashed, so the bump moves the digest"
        );
        assert_eq!(
            MANIFEST_SCHEMA_VERSION, 2,
            "and the number this build writes is the one the migration chain ends at"
        );
        // The framing did not move with it: the record gained a field, not a layout.
        assert_eq!(MANIFEST_DOMAIN, "anim_engine/manifest/v1");
        assert_ne!(
            before.canonical_bytes().unwrap(),
            now.canonical_bytes().unwrap()
        );
    }

    #[test]
    fn manifest_json_round_trips_and_refuses_a_key_that_disagrees() {
        let mut manifest = AssetManifest::new();
        manifest.insert(record("a", 0x01));
        manifest.insert(record("b", 0x02));
        let json = serde_json::to_string(&manifest).unwrap();
        assert_eq!(
            serde_json::from_str::<AssetManifest>(&json).unwrap(),
            manifest
        );

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let keys: Vec<String> = value["records"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        // Move the first record under the second record's key.
        let records = value["records"].as_object_mut().unwrap();
        let first = records.remove(&keys[0]).unwrap();
        records.insert(keys[1].clone(), first);
        let smuggled = serde_json::to_string(&value).unwrap();
        let e = serde_json::from_str::<AssetManifest>(&smuggled).unwrap_err();
        assert!(e.to_string().contains("filed under"), "{e}");
        assert_eq!(
            AssetError::MalformedManifest { detail: "x".into() }
                .code()
                .as_str(),
            "asset.malformed_manifest"
        );
    }

    #[test]
    fn replacing_a_record_reports_what_it_displaced() {
        // Spec 11.6's reimport: the id stays, the bytes move, the record is replaced.
        let mut manifest = AssetManifest::new();
        manifest.insert(record("pose", 0x0A));
        let displaced = manifest
            .insert(
                AssetManifestEntry::new(
                    id("pose"),
                    hash(0x0B),
                    2048,
                    AssetKind::Image,
                    1,
                    [],
                    "pose.png",
                    claims(),
                )
                .unwrap(),
            )
            // A replacement always displaces the record it replaced.
            .unwrap();
        assert_eq!(displaced.content_hash, hash(0x0A));
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest.get(&id("pose")).unwrap().content_hash, hash(0x0B));
    }

    #[test]
    fn two_records_for_one_id_are_refused_not_collapsed() {
        // `insert` above treats a replaced key as a reimport; a construction from a
        // sequence that carries one id twice is a different defect and must not be
        // resolved by whichever record came last.
        let mut manifest = AssetManifest::new();
        manifest.insert(record("same", 0x01));
        let e = AssetManifest::from_records(
            MANIFEST_SCHEMA_VERSION,
            [record("same", 0x01), record("same", 0x02)],
        )
        .unwrap_err();
        assert!(matches!(e, AssetError::DuplicateAssetId { .. }), "{e}");
        assert_eq!(manifest.len(), 1, "the caller's manifest is untouched");
    }

    #[test]
    fn dependency_edges_are_ordered_and_referenced_objects_follow_the_records() {
        let child = AssetManifestEntry::new(
            id("child"),
            hash(0x10),
            8,
            AssetKind::Image,
            1,
            [id("cel"), id("rig")],
            "child.png",
            claims(),
        )
        .unwrap();
        let mut manifest = AssetManifest::new();
        manifest.insert(child);
        manifest.insert(record("rig", 0x20));
        manifest.insert(record("cel", 0x30));

        let edges = manifest.dependency_edges();
        assert_eq!(edges.len(), 2);
        for pair in edges.windows(2) {
            assert!(pair[0] <= pair[1], "edges must be canonically ordered");
        }
        let objects = referenced_objects(&manifest);
        let ids: Vec<AssetId> = objects.iter().map(|o| o.0).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            ids, sorted,
            "the export's verification list is in record order"
        );
    }
}
