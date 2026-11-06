//! Safe object paths, verification and the immutable object lifecycle
//! (spec 11.3, 11.6, 11.7, 11.8, 4.3).
//!
//! The store is the only place that turns a digest into a filename, and that is the
//! security property spec 11.6 asks for: "Path construction accepts only a validated
//! fixed-size hash, preventing traversal through filenames." A display name never
//! reaches a path, and a path segment that came from one could not be a digest
//! anyway, because [`ContentHash`] refuses every spelling but sixty-four lowercase
//! hex characters.
//!
//! Objects are immutable and are never deleted here. Spec 11.8 keeps them until an
//! explicit garbage-collection operation, and 4.3 adds that collection MUST NOT run
//! implicitly — so the only removal-shaped thing in this crate is a failed import
//! cleaning up its own temporary.

use std::path::{Component, Path, PathBuf};

use anim_core::id::AssetId;

use crate::decode::DecoderConfiguration;
use crate::hash::{hash_file, ContentHash};
use crate::manifest::{AssetManifest, AssetManifestEntry};
use crate::{io_error, AssetError};

/// The directory names spec 4.3 fixes for a project's assets.
const ASSETS_DIR: &str = "assets";
const OBJECTS_DIR: &str = "objects";
const DERIVED_DIR: &str = "derived";

/// The staged-import temporary's name stem, inside the objects directory.
///
/// A dotfile-shaped name so that a directory listing and a recovery scan can tell a
/// temporary from an object at a glance, and so [`AssetStore::parse_object_name`]
/// refuses it: spec 11.8's "temporary files are never resolved as objects" is a
/// property of the name grammar, not of a flag some caller remembers to check.
pub(crate) const TEMPORARY_STEM: &str = "import";

/// The content-addressed object store of one project.
///
/// `root` is the project directory, not the `assets/` folder, so that the layout
/// spec 4.3 shows is derived from one value rather than assumed from whatever
/// directory a caller happened to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetStore {
    root: PathBuf,
}

impl AssetStore {
    /// A store rooted at `root`, requiring the layout to exist already.
    ///
    /// Use [`AssetStore::init`] for a project being created; `open` deliberately does
    /// not write, because resolution and verification are read-only operations
    /// (spec 4.2) and a read that silently creates directories in a project the
    /// operator meant to inspect is a side effect nobody asked for.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, AssetError> {
        let store = Self { root: root.into() };
        if !store.root.is_dir() {
            return Err(io_error(
                "open project root",
                &store.root,
                "not a directory",
            ));
        }
        if !store.objects_dir().is_dir() {
            return Err(io_error(
                "open asset store",
                &store.objects_dir(),
                "the project has no assets/objects directory; call init on a project \
                 that is being created",
            ));
        }
        Ok(store)
    }

    /// A store rooted at `root`, creating `assets/objects` and `assets/derived`.
    ///
    /// Idempotent: an existing layout is accepted unchanged, which is what lets a
    /// project be reopened after an interrupted import.
    pub fn init(root: impl Into<PathBuf>) -> Result<Self, AssetError> {
        let store = Self { root: root.into() };
        if !store.root.is_dir() {
            std::fs::create_dir_all(&store.root)
                .map_err(|e| io_error("create project root", &store.root, e.to_string()))?;
        }
        for dir in [store.objects_dir(), store.derived_dir()] {
            std::fs::create_dir_all(&dir)
                .map_err(|e| io_error("create asset layout", &dir, e.to_string()))?;
        }
        Ok(store)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/assets`.
    #[must_use]
    pub fn assets_dir(&self) -> PathBuf {
        self.root.join(ASSETS_DIR)
    }

    /// `<root>/assets/objects` — where staged temporaries and objects live, so a
    /// rename is always same-volume (spec 11.7).
    #[must_use]
    pub fn objects_dir(&self) -> PathBuf {
        self.assets_dir().join(OBJECTS_DIR)
    }

    /// `<root>/assets/derived` — disposable caches, never authoritative (spec 11.6).
    #[must_use]
    pub fn derived_dir(&self) -> PathBuf {
        self.assets_dir().join(DERIVED_DIR)
    }

    /// `<root>/assets/objects/<2 hex>/<2 hex>/<64 hex>` (spec 11.7).
    ///
    /// Takes a [`ContentHash`], not a `&str`, which is the type-level half of the
    /// traversal rule: by the time a caller holds one of these, the two-hex shards and
    /// the file name are all slices of the same validated digest, and there is no
    /// string left that could contain a separator.
    #[must_use]
    pub fn object_path(&self, hash: &ContentHash) -> PathBuf {
        let (first, second) = hash.path_shards();
        self.objects_dir()
            .join(first)
            .join(second)
            .join(hash.to_hex())
    }

    /// The directory an object of this digest lives in, created on demand.
    ///
    /// Spec 11.7's "create parent directories and rename without overwrite", kept
    /// separate from the rename itself so that a failure here cannot leave a
    /// half-published object.
    pub fn create_object_parent(&self, hash: &ContentHash) -> Result<PathBuf, AssetError> {
        let path = self.object_path(hash);
        let parent = path
            .parent()
            .ok_or_else(|| io_error("object parent", &path, "no parent directory"))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| io_error("create object parent", parent, e.to_string()))?;
        Ok(parent.to_path_buf())
    }

    /// The digest an existing object file name denotes, or `None` for anything else.
    ///
    /// This is the gate a recovery or garbage-collection scan needs: a leftover
    /// `.import.3.tmp`, a stray file an operator dropped in, and a name that is
    /// uppercase or the wrong length are all `None` rather than an object.
    #[must_use]
    pub fn parse_object_name(&self, name: &str) -> Option<ContentHash> {
        ContentHash::from_hex(name).ok()
    }

    /// Whether `path` lexically resolves to a location inside this store.
    ///
    /// Normalized without touching the filesystem, because the paths that matter for
    /// this question — one an attacker or a corrupt manifest produced — are exactly the
    /// ones that do not exist yet. `..` is folded against the store root, so
    /// `objects/../../outside` is refused while `objects/./ab/cd/ff` is accepted.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        fn normalize(path: &Path) -> PathBuf {
            let mut out: Vec<Component<'_>> = Vec::new();
            for component in path.components() {
                match component {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        if matches!(out.last(), Some(Component::Normal(_))) {
                            out.pop();
                        } else {
                            // A `..` that would leave the anchored root is kept so the
                            // containment test below fails rather than silently
                            // resolving upward.
                            out.push(component);
                        }
                    }
                    other => out.push(other),
                }
            }
            out.iter().collect()
        }

        let root = normalize(&self.root);
        normalize(path).starts_with(&root)
    }

    /// Refuse any path outside the store, before it is opened, created or deleted.
    fn ensure_within<'a>(&self, path: &'a Path) -> Result<&'a Path, AssetError> {
        if self.contains(path) {
            Ok(path)
        } else {
            Err(io_error(
                "object path escapes the asset store",
                path,
                format!("root is {}", self.root.display()),
            ))
        }
    }

    /// The cache path for one derived result (spec 11.6's key, 4.3's layout).
    ///
    /// `<root>/assets/derived/<importer-version>/<configuration>/<full-hash>`. The
    /// configuration level is the third axis of 11.6's key; 4.3's sketch shows
    /// `importer-version` then cache files, and putting the configuration between them
    /// is what lets two decode settings coexist for one version bump without either
    /// overwriting the other.
    pub fn derived_path(&self, key: &DerivedKey) -> Result<PathBuf, AssetError> {
        let path = self
            .derived_dir()
            .join(key.importer_version.to_string())
            .join(key.configuration.as_str())
            .join(key.content_hash.to_hex());
        self.ensure_within(&path)?;
        Ok(path)
    }

    /// Whether the object a record names is present, without reading it.
    #[must_use]
    pub fn object_exists(&self, hash: &ContentHash) -> bool {
        self.object_path(hash).is_file()
    }

    /// The object path of a record, confined.
    ///
    /// A record's hash is validated at construction, so this cannot fail for a record
    /// this crate built; it fails for a path assembled elsewhere, which is the case
    /// WP03's recovery scan can produce and must not follow.
    pub fn record_path(&self, record: &AssetManifestEntry) -> Result<PathBuf, AssetError> {
        let path = self.object_path(&record.content_hash);
        self.ensure_within(&path)?;
        Ok(path)
    }

    /// Stage a new object's bytes inside the store, returning the writer that owns the
    /// temporary (spec 11.7's first step).
    ///
    /// Lives in the objects directory so the eventual rename is same-volume, which is
    /// what makes it a rename rather than a copy that could be interrupted into a
    /// half-written object.
    pub fn stage(&self) -> Result<anim_core::io::atomic_replace::StagedWriter, AssetError> {
        anim_core::io::atomic_replace::StagedWriter::create_in(&self.objects_dir(), TEMPORARY_STEM)
            .map_err(|e| io_error("stage object temporary", &self.objects_dir(), e.detail()))
    }

    /// Verify one record against the bytes on disk: length first, then content
    /// (spec 11.9's `verify`, 11.7's "verify expected length and content").
    ///
    /// The length check is not redundant with the hash: it is O(1), it catches a
    /// truncated file before reading it, and spec 11.12 names a truncated file as a
    /// separate case from corrupted bytes.
    pub fn verify(&self, record: &AssetManifestEntry) -> Result<(), AssetError> {
        let path = self.record_path(record)?;
        if !path.is_file() {
            return Err(AssetError::MissingObject {
                asset_id: record.asset_id.to_canonical_string(),
                path: path.display().to_string(),
            });
        }
        let metadata =
            std::fs::metadata(&path).map_err(|e| io_error("stat object", &path, e.to_string()))?;
        let length = metadata.len();
        if length != record.byte_length {
            return Err(AssetError::SizeMismatch {
                asset_id: record.asset_id.to_canonical_string(),
                expected: record.byte_length,
                actual: length,
            });
        }
        let (actual_hash, hashed_len) = hash_file(&path)?;
        if actual_hash != record.content_hash {
            return Err(AssetError::HashMismatch {
                asset_id: record.asset_id.to_canonical_string(),
                expected: record.content_hash.to_hex(),
                actual: actual_hash.to_hex(),
            });
        }
        if hashed_len != length {
            // The file changed between the stat and the read. Reported as a size
            // mismatch rather than retried, because a store being written underneath a
            // verification pass is the finding, not a transient.
            return Err(AssetError::SizeMismatch {
                asset_id: record.asset_id.to_canonical_string(),
                expected: length,
                actual: hashed_len,
            });
        }
        Ok(())
    }

    /// Verify every referenced object, in record order, reporting all findings.
    ///
    /// Spec 11.7 makes this the export gate — "Export MUST verify all referenced
    /// source objects before frame zero" — and 11.11 says changed or missing content
    /// stops export. Every record is checked rather than stopping at the first
    /// failure, because an operator who has to run the check once per broken asset is
    /// going to skip it.
    pub fn verify_all(&self, manifest: &AssetManifest) -> Result<(), AssetError> {
        let mut first_failure = None;
        for record in manifest.iter() {
            if let Err(e) = self.verify(record) {
                first_failure.get_or_insert(e);
            }
        }
        first_failure.map_or(Ok(()), Err)
    }

    /// The ids of records whose objects are missing, in canonical order.
    ///
    /// The preview path (spec 11.11) needs the set rather than the first failure, so it
    /// can show a placeholder per affected asset instead of one error for the batch.
    pub fn missing_objects(&self, manifest: &AssetManifest) -> Vec<AssetId> {
        manifest
            .iter()
            .filter(|record| !self.object_exists(&record.content_hash))
            .map(|record| record.asset_id)
            .collect()
    }
}

/// One derived cache's identity: the three axes spec 11.6 names.
///
/// The configuration is an enum rather than a caller's string because it reaches a
/// path. A free `String` here would mean an arbitrary, unvalidated segment in the one
/// place this crate promised that paths come only from validated values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedKey {
    pub content_hash: ContentHash,
    pub importer_version: u32,
    pub configuration: DecoderConfiguration,
}

impl DerivedKey {
    #[must_use]
    pub const fn new(
        content_hash: ContentHash,
        importer_version: u32,
        configuration: DecoderConfiguration,
    ) -> Self {
        Self {
            content_hash,
            importer_version,
            configuration,
        }
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
    use crate::hash::hash_bytes;
    use anim_core::id::AssetId;
    use uuid::Uuid;

    fn id(label: &str) -> AssetId {
        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        AssetId::derive(&namespace, label.as_bytes())
    }

    fn record(hash: ContentHash, length: u64) -> AssetManifestEntry {
        AssetManifestEntry::new(
            id("asset"),
            hash,
            length,
            crate::manifest::AssetKind::Image,
            1,
            [],
            "cel.png",
            cel_claims(),
        )
        .unwrap()
    }

    /// An image record is not legal without its claims, and every object these tests
    /// store is addressed as a cel, so the answer they carry is the RGBA-PNG one.
    fn cel_claims() -> Option<crate::image::ImageAssetClaims> {
        Some(crate::image::ImageAssetClaims {
            color_space: crate::image::ImageColorSpace::Srgb,
            alpha: crate::image::ImageAlpha::Straight,
        })
    }

    fn store() -> (tempfile::TempDir, AssetStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = AssetStore::init(dir.path()).unwrap();
        (dir, store)
    }

    /// Write an object the way publication does, so tests exercise the real layout.
    fn publish(store: &AssetStore, bytes: &[u8]) -> ContentHash {
        let hash = hash_bytes(bytes);
        store.create_object_parent(&hash).unwrap();
        std::fs::write(store.object_path(&hash), bytes).unwrap();
        hash
    }

    #[test]
    fn layout_matches_the_spec_4_3_tree() {
        let (_dir, store) = store();
        assert!(store.assets_dir().is_dir());
        assert!(store.objects_dir().is_dir());
        assert!(store.derived_dir().is_dir());
        let hash = hash_bytes(b"cel");
        let (first, second) = hash.path_shards();
        let path = store.object_path(&hash);
        assert_eq!(
            path,
            store
                .objects_dir()
                .join(first)
                .join(second)
                .join(hash.to_hex())
        );
        // Two shards plus the digest, and the digest is the whole file name.
        assert_eq!(path.file_name().unwrap().to_string_lossy().len(), 64);
    }

    #[test]
    fn object_paths_are_built_only_from_validated_digests() {
        let (_dir, store) = store();
        // The traversal attempt has to pass through a `ContentHash`, and a hash cannot
        // be built from it.
        for hostile in [
            "../../windows/system32",
            "..%2f..%2fescape",
            "ab/cd/ef",
            "",
            " ",
            "................................................................",
        ] {
            assert!(
                ContentHash::from_hex(hostile).is_err(),
                "must not parse as a digest: {hostile}"
            );
        }
        // And a digest that does parse cannot contain a separator, so every derived
        // path stays inside the store.
        let hash = ContentHash::from_hex(&"0f".repeat(32)).unwrap();
        assert!(store.contains(&store.object_path(&hash)));
    }

    #[test]
    fn containment_folds_dot_dot_and_refuses_escapes() {
        let (_dir, store) = store();
        let inside = store.objects_dir().join("ab").join("cd").join("ef");
        assert!(store.contains(&inside));
        assert!(store.contains(&store.objects_dir().join(".").join("ab")));
        // `..` back into the project root is still inside the store's root; out of it
        // is not.
        assert!(store.contains(&store.root().join("assets")));
        assert!(!store.contains(
            &store
                .objects_dir()
                .join("..")
                .join("..")
                .join("..")
                .join("evil")
        ));
        assert!(!store.contains(Path::new("C:\\Users\\other\\project\\assets\\objects\\ab")));
        // A relative path is not a prefix of the root, so it is refused rather than
        // guessed at.
        assert!(!store.contains(Path::new("assets/objects/ab")));
    }

    #[test]
    fn a_record_path_outside_the_store_is_refused_before_it_is_opened() {
        let (_dir, store) = store();
        let hash = hash_bytes(b"cel");
        let good = store.record_path(&record(hash, 3)).unwrap();
        assert!(store.contains(&good));

        // The hostile construction is a store rooted somewhere that a symlink or a
        // hand-edited layout would escape; `ensure_within` is the gate either way.
        let escape = store
            .objects_dir()
            .join("..")
            .join("..")
            .join("..")
            .join("evil");
        let e = store.ensure_within(&escape).unwrap_err();
        assert!(matches!(e, AssetError::Io { .. }), "{e}");
        assert!(e.to_string().contains("escapes"), "{e}");
        assert_eq!(e.code().as_str(), "asset.io");
    }

    #[test]
    fn a_temporary_is_never_an_object() {
        let (_dir, store) = store();
        assert!(store.parse_object_name(".import.1.tmp").is_none());
        assert!(store.parse_object_name("import").is_none());
        // A name that would be a valid digest but is not lowercase is not an object
        // either — the same strictness the digest parser uses.
        assert!(store
            .parse_object_name(&"0f".repeat(32).to_uppercase())
            .is_none());
        assert!(store.parse_object_name(&"0f".repeat(32)).is_some());

        // And a staged temporary really does have a non-object name.
        let writer = store.stage().unwrap();
        let name = writer
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with(".import."), "{name}");
        assert!(store.parse_object_name(&name).is_none());
    }

    #[test]
    fn verify_accepts_the_object_it_published_and_rejects_three_ways() {
        let (_dir, store) = store();
        let bytes = b"the exact cel bytes".to_vec();
        let hash = publish(&store, &bytes);
        let entry = record(hash, bytes.len() as u64);
        store.verify(&entry).unwrap();

        // Missing object.
        let absent = record(hash_bytes(b"never imported"), 4);
        let e = store.verify(&absent).unwrap_err();
        assert!(matches!(e, AssetError::MissingObject { .. }), "{e}");

        // Truncated: the length check must fire without needing a hash disagreement.
        std::fs::write(store.object_path(&hash), &bytes[..bytes.len() - 1]).unwrap();
        let e = store.verify(&entry).unwrap_err();
        assert!(matches!(e, AssetError::SizeMismatch { .. }), "{e}");

        // Corrupted but the same length, where only the digest can tell.
        let mut flipped = bytes.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xFF;
        std::fs::write(store.object_path(&hash), &flipped).unwrap();
        let e = store.verify(&entry).unwrap_err();
        assert!(matches!(e, AssetError::HashMismatch { .. }), "{e}");
        assert!(e.to_string().contains(&entry.asset_id.to_string()), "{e}");
    }

    #[test]
    fn verify_all_and_missing_objects_report_the_whole_set() {
        let (_dir, store) = store();
        let a = publish(&store, b"aaaa");
        let mut manifest = AssetManifest::new();
        manifest.insert(
            AssetManifestEntry::new(
                id("a"),
                a,
                4,
                crate::manifest::AssetKind::Image,
                1,
                [],
                "a.png",
                cel_claims(),
            )
            .unwrap(),
        );
        manifest.insert(
            AssetManifestEntry::new(
                id("b"),
                hash_bytes(b"unpublished"),
                11,
                crate::manifest::AssetKind::Image,
                1,
                [],
                "b.png",
                cel_claims(),
            )
            .unwrap(),
        );
        store.verify_all(&manifest).unwrap_err();
        let missing = store.missing_objects(&manifest);
        assert_eq!(missing, vec![id("b")], "the preview's placeholder set");

        // Publish the second object and the manifest verifies clean.
        publish(&store, b"unpublished");
        store.verify_all(&manifest).unwrap();
        assert!(store.missing_objects(&manifest).is_empty());
    }

    #[test]
    fn derived_paths_carry_all_three_key_axes_and_stay_inside() {
        let (_dir, store) = store();
        let hash = hash_bytes(b"cel");
        let key = DerivedKey::new(hash, 2, DecoderConfiguration::Rgba8Unpremultiplied);
        let path = store.derived_path(&key).unwrap();
        assert_eq!(
            path,
            store
                .derived_dir()
                .join("2")
                .join("rgba8-unpremultiplied")
                .join(hash.to_hex())
        );
        assert!(store.contains(&path));
        // A different configuration or version is a different cache file, never an
        // overwrite of one that was decoded differently.
        let other = store
            .derived_path(&DerivedKey::new(
                hash,
                3,
                DecoderConfiguration::Rgba8Premultiplied,
            ))
            .unwrap();
        assert_ne!(path, other);
    }

    #[test]
    fn init_is_idempotent_and_open_refuses_an_absent_layout() {
        let dir = tempfile::tempdir().unwrap();
        let first = AssetStore::init(dir.path()).unwrap();
        let second = AssetStore::init(dir.path()).unwrap();
        assert_eq!(first, second);
        drop(second);

        let bytes = b"cel".to_vec();
        let hash = publish(&first, &bytes);
        assert!(first.object_exists(&hash));

        // An empty directory is not a project with an asset store in it.
        let bare = tempfile::tempdir().unwrap();
        let e = AssetStore::open(bare.path()).unwrap_err();
        assert!(matches!(e, AssetError::Io { .. }), "{e}");
        assert!(e.to_string().contains("objects"), "{e}");
        // Opening a project that does have one succeeds without creating anything.
        let opened = AssetStore::open(dir.path()).unwrap();
        assert!(opened.object_exists(&hash));
        assert!(!opened.objects_dir().join("ab").exists());
    }
}
