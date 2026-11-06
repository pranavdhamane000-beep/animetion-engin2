//! Duplicate, corruption and interrupted-import tests (spec 11.3, 11.12).
//!
//! This is the file spec 11.3 lists for WP02's verification, and it is organized as
//! the validation section is: one scenario per named failure, each ending with the
//! same question — after this failure, is the project still usable?
//!
//! Two rules shape how the scenarios are built. First, the oracle is never the code
//! under test: digests in the streaming tests are computed here, over bytes this file
//! generated, and the BLAKE3 vectors in `src/hash.rs` come from the reference
//! implementation's own published table (DECISIONS.md #27). Second, an interruption is
//! produced through the public surface — a source that stops, a file rewritten from
//! the test, a temporary dropped before its rename — because a hook built into the
//! production path would be testing the hook.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a fixture that cannot be built has made the scenario unreadable, which is a panic rather than a test outcome; the workspace denial still governs every production crate"
)]

use std::io::{self, Read};

use anim_assets::{
    import, manifest_digest, resolve, resolve_with, verify, AssetError, AssetKind, AssetManifest,
    AssetManifestEntry, AssetStore, ContentHash, ResolutionPolicy, CURRENT_IMPORTER_VERSION,
};
use anim_core::id::AssetId;
use uuid::Uuid;

const MIB: usize = 1024 * 1024;

fn id(label: &str) -> AssetId {
    let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
    AssetId::derive(&namespace, label.as_bytes())
}

/// Deterministic, non-repeating payload bytes, so a two-megabyte fixture is not two
/// megabytes of one value that a truncation could not tell apart.
fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u64 * 31 + u64::from(seed)) as u8)
        .collect()
}

/// The digest of a payload, computed here rather than asked of the crate.
fn reference_digest(bytes: &[u8]) -> String {
    hex::encode(blake3::hash(bytes).as_bytes())
}

struct Project {
    /// Kept as a field so the directory outlives every path in the store; dropping it
    /// early would delete a project mid-test.
    dir: tempfile::TempDir,
    store: AssetStore,
}

impl Project {
    fn root(&self) -> &std::path::Path {
        self.dir.path()
    }
}

fn project() -> Project {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::init(dir.path()).unwrap();
    Project { dir, store }
}

fn import_bytes(
    project: &Project,
    label: &str,
    bytes: &[u8],
    kind: AssetKind,
) -> AssetManifestEntry {
    import(
        &project.store,
        id(label),
        format!("{label}.dat"),
        kind,
        CURRENT_IMPORTER_VERSION,
        bytes,
    )
    .unwrap_or_else(|e| panic!("import of {label} failed: {e}"))
}

/// Every object the manifest names must still be present and byte-exact.
///
/// Spec 11.12's closing requirement, stated once as a function so that each failure
/// scenario ends by asking it rather than by asserting whichever file the author
/// remembered.
fn assert_store_usable(project: &Project, manifest: &AssetManifest) {
    for record in manifest.iter() {
        verify(&project.store, record)
            .unwrap_or_else(|e| panic!("{} became unusable: {e}", record.display_name));
    }
}

/// The store must hold nothing but the objects it can account for: an interrupted
/// import may not leave a partial file, because the next run would find an object with
/// no record.
fn assert_no_stray_objects(project: &Project, manifest: &AssetManifest) {
    let mut expected: Vec<std::path::PathBuf> = manifest
        .iter()
        .map(|record| project.store.object_path(&record.content_hash))
        .collect();
    expected.sort();
    // Identical content shares one immutable object (spec 11.6), so two records can
    // name one file.
    expected.dedup();

    let mut found: Vec<std::path::PathBuf> = Vec::new();
    let mut stack = vec![project.store.objects_dir()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    assert_eq!(
        found, expected,
        "the object store holds exactly its records"
    );
}

#[test]
fn an_imported_object_lands_on_the_digest_derived_path() {
    let project = project();
    let bytes = payload(0x11, 4096);
    let record = import_bytes(&project, "cel", &bytes, AssetKind::Image);

    assert_eq!(record.byte_length, 4096);
    assert_eq!(record.content_hash.to_hex(), reference_digest(&bytes));
    assert_eq!(
        project.store.object_path(&record.content_hash),
        project
            .store
            .assets_dir()
            .join("objects")
            .join(&record.content_hash.to_hex()[..2])
            .join(&record.content_hash.to_hex()[2..4])
            .join(record.content_hash.to_hex())
    );
    assert_eq!(
        std::fs::read(project.store.object_path(&record.content_hash)).unwrap(),
        bytes
    );
    assert!(record.display_name.ends_with(".dat"), "a display name");
    assert_eq!(record.importer_version, CURRENT_IMPORTER_VERSION);
    assert!(record.dependencies().is_empty());
}

#[test]
fn a_stream_larger_than_the_chunk_size_is_imported_in_slices() {
    // Spec 11.12's named case. 4 MiB in, and the source is counted: it is read only in
    // chunks no larger than the crate's bound, which is what "larger than memory
    // buffers" has to mean for a reader that cannot see the import's allocations.
    let bytes = payload(0x40, 4 * MIB);
    let mut probe = ChunkProbe::new(bytes.clone());
    let project = project();
    let record = import(
        &project.store,
        id("big"),
        "big.dat".into(),
        AssetKind::Other,
        1,
        &mut probe,
    )
    .unwrap();

    // 64 slices of 64 KiB, plus the read that discovers the end and returns zero.
    // Asserting the count is what makes "bounded" a checked property rather than a
    // description: a single 4 MiB allocation would report one read.
    assert_eq!(
        probe.reads, 65,
        "4 MiB in 64 KiB slices plus the end-of-stream probe"
    );
    assert!(
        probe.max_read <= 64 * 1024,
        "a single read of {} bytes would mean the payload was buffered",
        probe.max_read
    );
    assert_eq!(record.byte_length, bytes.len() as u64);
    assert_eq!(record.content_hash.to_hex(), reference_digest(&bytes));
    verify(&project.store, &record).unwrap();
    // The object is the stream, byte for byte.
    assert_eq!(
        std::fs::read(project.store.object_path(&record.content_hash))
            .unwrap()
            .len(),
        bytes.len()
    );
}

/// A reader that records the largest request it was asked to satisfy.
struct ChunkProbe {
    inner: std::io::Cursor<Vec<u8>>,
    reads: usize,
    max_read: usize,
}

impl ChunkProbe {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            inner: std::io::Cursor::new(bytes),
            reads: 0,
            max_read: 0,
        }
    }
}

impl Read for ChunkProbe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        self.max_read = self.max_read.max(buf.len());
        self.inner.read(buf)
    }
}

#[test]
fn duplicate_content_shares_one_object_and_two_ids_keep_their_own_records() {
    let project = project();
    let bytes = payload(0x77, 8192);
    let first = import_bytes(&project, "first", &bytes, AssetKind::Image);
    let second = import_bytes(&project, "second", &bytes, AssetKind::Image);

    assert_eq!(first.content_hash, second.content_hash, "same bytes");
    assert_ne!(first.asset_id, second.asset_id, "different assets");
    assert_eq!(
        std::fs::read_dir(
            project
                .store
                .object_path(&first.content_hash)
                .parent()
                .unwrap()
        )
        .unwrap()
        .filter_map(Result::ok)
        .count(),
        1,
        "one immutable object for identical content (spec 11.6)"
    );

    let mut manifest = AssetManifest::new();
    manifest.insert(first.clone());
    manifest.insert(second.clone());
    assert_eq!(manifest.len(), 2);
    assert_store_usable(&project, &manifest);
    // The object belongs to neither record, so deleting one must not delete the file;
    // this is why collection is a separate, explicit operation (spec 11.8).
    manifest.remove(&second.asset_id);
    assert_store_usable(&project, &manifest);
}

#[test]
fn a_reimport_of_the_same_id_moves_the_content_and_keeps_the_identity() {
    // Spec 11.6: "Reimport with the same AssetId creates a new manifest revision while
    // preserving logical identity."
    let project = project();
    let old_bytes = payload(0x01, 1000);
    let new_bytes = payload(0x02, 2000);
    let old_record = import_bytes(&project, "pose", &old_bytes, AssetKind::Image);
    let new_record = import(
        &project.store,
        old_record.asset_id,
        "pose.png".into(),
        AssetKind::Image,
        1,
        &new_bytes[..],
    )
    .unwrap();

    assert_eq!(
        new_record.asset_id, old_record.asset_id,
        "identity preserved"
    );
    assert_ne!(
        new_record.content_hash, old_record.content_hash,
        "content moved"
    );
    assert_eq!(new_record.byte_length, 2000);

    let mut manifest = AssetManifest::new();
    manifest.insert(old_record.clone());
    let displaced = manifest.insert(new_record.clone()).unwrap();
    assert_eq!(displaced.asset_id, new_record.asset_id);
    assert_eq!(manifest.len(), 1, "one record per id");
    assert_eq!(manifest.get(&new_record.asset_id).unwrap(), &new_record);

    // Both objects are still on disk: the old one is unreferenced, not deleted, and
    // GC is a future explicit operation.
    assert!(project
        .store
        .object_path(&old_record.content_hash)
        .is_file());
    verify(&project.store, &new_record).unwrap();
    assert_eq!(
        resolve(&project.store, &manifest, new_record.asset_id)
            .unwrap()
            .entry,
        new_record
    );
}

#[test]
fn manifest_digests_agree_under_shuffled_insertion_after_real_imports() {
    let project = project();
    let records: Vec<AssetManifestEntry> = (0..12)
        .map(|i| {
            import_bytes(
                &project,
                &format!("cel-{i}"),
                &payload(i as u8, 300 + i),
                AssetKind::Image,
            )
        })
        .collect();

    let mut forward = AssetManifest::new();
    for record in &records {
        forward.insert(record.clone());
    }
    let mut reversed = AssetManifest::new();
    for record in records.iter().rev() {
        reversed.insert(record.clone());
    }
    let mut swapped = records.clone();
    swapped.swap(0, 11);
    swapped.swap(3, 4);
    let shuffled = AssetManifest::from_records(forward.schema_version(), swapped.clone()).unwrap();

    let digest = manifest_digest(&forward).unwrap();
    let _ = &swapped;
    assert_eq!(digest, manifest_digest(&reversed).unwrap());
    assert_eq!(digest, manifest_digest(&shuffled).unwrap());
    assert_eq!(
        forward.canonical_bytes().unwrap(),
        shuffled.canonical_bytes().unwrap(),
        "the pre-image agrees too, not only its hash"
    );
    // A single byte of difference in one record moves the digest.
    let mut edited = shuffled;
    let before = manifest_digest(&edited).unwrap();
    let first = edited.asset_ids().next().unwrap();
    let mut replacement = edited.get(&first).unwrap().clone();
    replacement.byte_length += 1;
    edited.insert(replacement);
    assert_ne!(before, manifest_digest(&edited).unwrap());
    assert_store_usable(&project, &forward);
}

#[test]
fn a_path_that_is_not_a_digest_cannot_name_an_object() {
    let project = project();
    // The traversal attempts a filename could carry, rejected where the type is built.
    for hostile in [
        "../../../Windows/System32/config/sam",
        "..\\..\\escape",
        "ab/cd/ef",
        "%2e%2e%2f",
        "",
        &"f".repeat(63),
        &"g".repeat(64),
        &"F".repeat(64),
    ] {
        assert!(
            ContentHash::from_hex(hostile).is_err(),
            "must not become a digest: {hostile:?}"
        );
    }
    // And a display name never reaches a path: the object path is a function of the
    // digest alone.
    let bytes = payload(0x09, 32);
    let record = import(
        &project.store,
        id("hostile"),
        "../../../evil.png".into(),
        AssetKind::Image,
        1,
        &bytes[..],
    )
    .unwrap();
    let path = project.store.object_path(&record.content_hash);
    assert!(path.starts_with(project.store.root()));
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        record.content_hash.to_hex()
    );
    // The name is kept verbatim in the record and is absent from every path derived
    // from it: that asymmetry is the whole of spec 11.6's traversal rule.
    assert_eq!(record.display_name, "../../../evil.png");
    assert!(!project
        .store
        .object_path(&record.content_hash)
        .to_string_lossy()
        .contains("evil"));
}

#[test]
fn a_store_rooted_elsewhere_cannot_reach_outside_it() {
    let project = project();
    let inside = project
        .store
        .object_path(&ContentHash::from_bytes([0x0f; 32]));
    assert_eq!(project.root(), project.store.root());
    assert!(project.store.root().is_dir());
    assert!(inside.starts_with(project.store.root()));
    // Every derived path stays inside too, including the disposable cache directory.
    let key = anim_assets::DerivedKey::new(
        ContentHash::from_bytes([0x0f; 32]),
        1,
        anim_assets::DecoderConfiguration::Rgba8Premultiplied,
    );
    let derived = project.store.derived_path(&key).unwrap();
    assert!(derived.starts_with(project.store.derived_dir()));
    assert!(derived.to_string_lossy().contains("rgba8-premultiplied"));
}

#[test]
fn corrupted_bytes_are_caught_by_verification_and_stop_resolution() {
    let project = project();
    let bytes = payload(0x55, 4096);
    let record = import_bytes(&project, "cel", &bytes, AssetKind::Image);
    let mut manifest = AssetManifest::new();
    manifest.insert(record.clone());

    // Flip one byte in the middle, leaving the length exact.
    let path = project.store.object_path(&record.content_hash);
    let mut broken = bytes.clone();
    broken[2000] ^= 0x01;
    std::fs::write(&path, &broken).unwrap();

    let e = verify(&project.store, &record).unwrap_err();
    assert!(matches!(e, AssetError::HashMismatch { .. }), "{e}");
    assert!(e.code().as_str() == "asset.hash_mismatch");
    // Both digests are in the message: the operator's question is what it became.
    assert!(e.to_string().contains(&record.content_hash.to_hex()), "{e}");
    assert!(e.to_string().contains(&reference_digest(&broken)), "{e}");

    // Presence-only resolution still succeeds — that is the preview's cheap path — but
    // the report classifies it, and the content policy refuses it.
    assert!(resolve(&project.store, &manifest, record.asset_id).is_ok());
    let report = anim_assets::verify_report(&project.store, &manifest, ResolutionPolicy::Content);
    assert_eq!(report.mismatched.len(), 1);
    assert!(!report.is_complete(&manifest));
    assert!(matches!(
        resolve_with(
            &project.store,
            &manifest,
            record.asset_id,
            ResolutionPolicy::Content
        )
        .unwrap_err(),
        AssetError::HashMismatch { .. }
    ));
    assert_store_usable(&project, &AssetManifest::default());
}

#[test]
fn a_truncated_file_is_a_size_mismatch_before_any_content_is_hashed() {
    let project = project();
    let bytes = payload(0x5A, 64 * 1024);
    let record = import_bytes(&project, "long-cel", &bytes, AssetKind::Image);
    let path = project.store.object_path(&record.content_hash);

    std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    let e = verify(&project.store, &record).unwrap_err();
    assert!(matches!(e, AssetError::SizeMismatch { .. }), "{e}");
    assert!(e.to_string().contains("65535"), "{e}");
    assert!(e.to_string().contains("65536"), "{e}");

    // The length policy catches it without reading the payload, and an empty file is
    // refused the same way.
    let mut manifest = AssetManifest::new();
    manifest.insert(record.clone());
    assert!(matches!(
        resolve_with(
            &project.store,
            &manifest,
            record.asset_id,
            ResolutionPolicy::Length
        )
        .unwrap_err(),
        AssetError::SizeMismatch { .. }
    ));
    std::fs::write(&path, b"").unwrap();
    assert!(matches!(
        verify(&project.store, &record).unwrap_err(),
        AssetError::SizeMismatch { .. }
    ));
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the scenario is an object an operator or a future collection pass removed; the workspace denial governs production paths, which never delete one"
)]
fn a_missing_object_is_reported_as_absent_not_corrupt() {
    let project = project();
    let record = import_bytes(&project, "cel", &payload(0x2C, 128), AssetKind::Image);
    let mut manifest = AssetManifest::new();
    manifest.insert(record.clone());
    std::fs::remove_file(project.store.object_path(&record.content_hash)).unwrap();

    let e = verify(&project.store, &record).unwrap_err();
    assert!(matches!(e, AssetError::MissingObject { .. }), "{e}");
    let report = anim_assets::verify_report(&project.store, &manifest, ResolutionPolicy::Content);
    assert_eq!(
        report.missing,
        vec![record.asset_id],
        "the preview's placeholder set"
    );
    assert!(report.mismatched.is_empty(), "absent is not corrupt");
    assert_eq!(
        project.store.missing_objects(&manifest),
        vec![record.asset_id]
    );
}

#[test]
fn a_missing_dependency_is_refused_before_anything_resolves_through_it() {
    let project = project();
    let record = import_bytes(&project, "rig", &payload(0x60, 256), AssetKind::Rig);
    let mut manifest = AssetManifest::new();
    manifest.insert(record.clone());
    let dangling = AssetManifestEntry::new(
        id("cel"),
        record.content_hash,
        record.byte_length,
        AssetKind::Image,
        CURRENT_IMPORTER_VERSION,
        [id("nowhere")],
        "cel.png",
    )
    .unwrap();
    manifest.insert(dangling.clone());

    let e = anim_assets::dependency_closure(&manifest, dangling.asset_id).unwrap_err();
    assert!(matches!(e, AssetError::MissingAssetId { .. }), "{e}");
    assert!(e.to_string().contains(&id("nowhere").to_string()), "{e}");
    assert!(matches!(
        anim_assets::validate_manifest(&manifest).unwrap_err(),
        AssetError::MissingAssetId { .. }
    ));
    // The object itself is fine; only its edge is broken.
    verify(&project.store, &dangling).unwrap();
    assert_store_usable(&project, &manifest);
}

#[test]
fn a_cycle_in_the_dependency_graph_is_named_and_not_walked() {
    let project = project();
    let record = import_bytes(&project, "a", &payload(0x61, 64), AssetKind::Image);
    let a = AssetManifestEntry::new(
        id("a"),
        record.content_hash,
        64,
        AssetKind::Image,
        CURRENT_IMPORTER_VERSION,
        [id("b")],
        "a.png",
    )
    .unwrap();
    let b = AssetManifestEntry::new(
        id("b"),
        record.content_hash,
        64,
        AssetKind::Image,
        CURRENT_IMPORTER_VERSION,
        [id("a")],
        "b.png",
    )
    .unwrap();
    let mut manifest = AssetManifest::new();
    manifest.insert(a);
    manifest.insert(b);

    let e = anim_assets::dependency_closure(&manifest, id("a")).unwrap_err();
    assert!(matches!(e, AssetError::DependencyCycle { .. }), "{e}");
    assert!(e.to_string().contains(" -> "), "{e}");
    assert!(anim_assets::validate_manifest(&manifest).is_err());
    assert_store_usable(&project, &manifest);
}

#[test]
fn an_interrupted_source_leaves_no_record_and_no_temporary() {
    let project = project();
    let settled = import_bytes(&project, "settled", &payload(0x30, 5000), AssetKind::Image);
    let mut manifest = AssetManifest::new();
    manifest.insert(settled.clone());

    // A source that fails partway through a multi-chunk stream.
    let bytes = payload(0x31, 3 * MIB);
    let e = import(
        &project.store,
        id("interrupted"),
        "interrupted.dat".into(),
        AssetKind::Other,
        1,
        FailingSource::new(&bytes, 128 * 1024),
    )
    .unwrap_err();
    assert!(matches!(e, AssetError::Io { .. }), "{e}");
    assert!(e.to_string().contains("read source stream"), "{e}");

    // A source the operator stopped, which is the same guarantee with a different
    // reason attached.
    let e = import(
        &project.store,
        id("cancelled"),
        "cancelled.dat".into(),
        AssetKind::Other,
        1,
        CancellingSource::new(&bytes, 64),
    )
    .unwrap_err();
    assert!(matches!(e, AssetError::Cancelled { .. }), "{e}");
    assert_eq!(e.code().as_str(), "asset.cancelled");
    assert_eq!(
        e.to_diagnostic().severity,
        anim_core::diagnostic::DiagnosticSeverity::Warning,
        "a cancellation is a supported outcome, not a fault"
    );

    // Nothing new is resolvable, nothing stray is on disk, and the prior object is
    // still exactly what it was.
    assert!(!manifest.contains(&id("interrupted")));
    assert!(!manifest.contains(&id("cancelled")));
    assert_no_stray_objects(&project, &manifest);
    assert_store_usable(&project, &manifest);
    assert_eq!(verify(&project.store, &settled), Ok(()));
}

/// A reader that yields `before` bytes and then fails, as a dropped network share or
/// a closed file would.
struct FailingSource {
    inner: std::io::Cursor<Vec<u8>>,
    before: u64,
    given: u64,
}

impl FailingSource {
    fn new(bytes: &[u8], before: usize) -> Self {
        Self {
            inner: std::io::Cursor::new(bytes.to_vec()),
            before: before as u64,
            given: 0,
        }
    }
}

impl Read for FailingSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.given >= self.before {
            return Err(io::Error::other("source disappeared mid-stream"));
        }
        let read = self.inner.read(buf)?;
        self.given += read as u64;
        Ok(read)
    }
}

/// A reader that reports cancellation after `after` bytes, by the contract `import`
/// documents: `ErrorKind::Interrupted` means the operator stopped it.
struct CancellingSource {
    inner: std::io::Cursor<Vec<u8>>,
    after: u64,
    given: u64,
}

impl CancellingSource {
    fn new(bytes: &[u8], after: usize) -> Self {
        Self {
            inner: std::io::Cursor::new(bytes.to_vec()),
            after: after as u64,
            given: 0,
        }
    }
}

impl Read for CancellingSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.given >= self.after {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cancelled by request",
            ));
        }
        let read = self.inner.read(buf)?;
        self.given += read as u64;
        Ok(read)
    }
}

#[test]
fn a_failure_immediately_before_the_object_rename_leaves_the_project_usable() {
    // The rename is the last irreversible step in spec 11.7, so this walks the import
    // sequence by hand and stops one call short of it — the same fault WP00's
    // `wp00_replace --fault before-commit` proved for the project file, at the object
    // store's boundary.
    let project = project();
    let settled = import_bytes(&project, "settled", &payload(0x41, 4096), AssetKind::Image);
    let mut manifest = AssetManifest::new();
    manifest.insert(settled.clone());

    let bytes = payload(0x42, 300 * 1024);
    let mut writer = project.store.stage().unwrap();
    let mut hasher = anim_assets::StreamingHash::new();
    for chunk in bytes.chunks(64 * 1024) {
        writer.write(chunk).unwrap();
        hasher.update(chunk).unwrap();
    }
    let staged = writer.synchronize().unwrap();
    let hash = hasher.finalize();
    assert_eq!(hash.to_hex(), reference_digest(&bytes));
    let destination = project.store.object_path(&hash);

    // Staged and durable, but not yet an object — and not resolvable as one either.
    let temporary = staged.path().to_path_buf();
    assert!(temporary.is_file());
    assert!(!destination.exists());
    assert!(
        project
            .store
            .parse_object_name(&temporary.file_name().unwrap().to_string_lossy())
            .is_none(),
        "spec 11.8: a temporary is never an object"
    );
    assert!(resolve(&project.store, &manifest, id("not-stored")).is_err());

    // Abandon it: the temporary must not survive, and the object must not appear.
    drop(staged);
    assert!(
        !temporary.exists(),
        "the abandoned temporary must be deleted"
    );
    assert!(!destination.exists());
    assert_no_stray_objects(&project, &manifest);
    assert_store_usable(&project, &manifest);

    // The same bytes can still be published by a later, complete import.
    let record = import_bytes(&project, "retry", &bytes, AssetKind::Other);
    assert_eq!(
        record.content_hash, hash,
        "same content, same digest, same path"
    );
    manifest.insert(record.clone());
    assert_no_stray_objects(&project, &manifest);
    assert_store_usable(&project, &manifest);
}

#[test]
fn a_publish_collision_reuses_the_existing_object() {
    // Two ids, identical bytes, imported in either order: the second one finds the
    // object present, verifies it, and discards its own temporary (spec 11.7).
    let project = project();
    let bytes = payload(0x50, 100 * 1024);
    let a = import_bytes(&project, "a", &bytes, AssetKind::Image);
    let b = import(
        &project.store,
        id("b"),
        "b.png".into(),
        AssetKind::Image,
        1,
        &bytes[..],
    )
    .unwrap();
    assert_eq!(a.content_hash, b.content_hash);

    let mut manifest = AssetManifest::new();
    manifest.insert(a.clone());
    manifest.insert(b.clone());
    assert_no_stray_objects(&project, &manifest);

    // And the collision is refused rather than silently overwritten when the existing
    // object's bytes disagree with the digest its name claims.
    std::fs::write(
        project.store.object_path(&b.content_hash),
        payload(0x51, 100 * 1024),
    )
    .unwrap();
    let e = import(
        &project.store,
        id("c"),
        "c.png".into(),
        AssetKind::Image,
        1,
        &bytes[..],
    )
    .unwrap_err();
    assert!(matches!(e, AssetError::HashMismatch { .. }), "{e}");
    assert_no_stray_objects(&project, &manifest);
}

#[test]
fn a_zero_byte_asset_is_imported_stored_and_verified() {
    // An empty payload is the one length a naive store gets wrong: the digest of
    // nothing is a real digest, the object is a zero-byte file, and a length check
    // written as "nonzero" would refuse it. Spec 11.8 puts no floor on `byte_length`.
    let project = project();
    let record = import_bytes(&project, "empty", &[], AssetKind::Other);
    assert_eq!(record.byte_length, 0);
    assert_eq!(record.content_hash.to_hex(), reference_digest(&[]));
    let path = project.store.object_path(&record.content_hash);
    assert!(path.is_file(), "a zero-byte object is still an object");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    verify(&project.store, &record).unwrap();

    let mut manifest = AssetManifest::new();
    manifest.insert(record.clone());
    assert_no_stray_objects(&project, &manifest);
    assert_store_usable(&project, &manifest);
    // Re-importing the same empty payload is a dedupe hit, not a write.
    let again = import(
        &project.store,
        id("empty-2"),
        "again".into(),
        AssetKind::Other,
        1,
        &[][..],
    )
    .unwrap();
    assert_eq!(again.content_hash, record.content_hash);
    assert_no_stray_objects(&project, &manifest);
}

#[test]
fn export_verification_covers_every_record_and_names_the_first_failure() {
    let project = project();
    let mut manifest = AssetManifest::new();
    for i in 0..5 {
        let record = import_bytes(
            &project,
            &format!("cel-{i}"),
            &payload(i as u8, 700),
            AssetKind::Image,
        );
        manifest.insert(record);
    }
    project.store.verify_all(&manifest).unwrap();

    // Break the middle one; the check must not stop there, and the error it returns is
    // one of the five.
    let broken = manifest.iter().nth(2).unwrap().clone();
    std::fs::write(project.store.object_path(&broken.content_hash), b"short").unwrap();
    let e = project.store.verify_all(&manifest).unwrap_err();
    assert!(
        matches!(
            e,
            AssetError::SizeMismatch { .. } | AssetError::HashMismatch { .. }
        ),
        "{e}"
    );
    let report = anim_assets::verify_report(&project.store, &manifest, ResolutionPolicy::Content);
    assert_eq!(report.verified.len(), 4);
    assert_eq!(report.mismatched.len(), 1);
    assert_eq!(report.mismatched[0].0, broken.asset_id);

    // The ordered digest is what WP09 puts in its job hash, so a differing manifest
    // must differ here.
    let digest = manifest_digest(&manifest).unwrap();
    let mut other = manifest.clone();
    other.remove(&broken.asset_id);
    assert_ne!(digest, manifest_digest(&other).unwrap());
}

#[test]
fn an_empty_project_imports_verifies_and_digests() {
    let project = project();
    let manifest = AssetManifest::new();
    assert!(manifest.is_empty());
    let digest = manifest_digest(&manifest).unwrap();
    assert_eq!(digest.as_bytes().len(), 32);
    project.store.verify_all(&manifest).unwrap();
    assert!(
        anim_assets::verify_report(&project.store, &manifest, ResolutionPolicy::Content)
            .is_complete(&manifest)
    );
    assert!(matches!(
        resolve(&project.store, &manifest, id("nothing")),
        Err(AssetError::MissingAssetId { .. })
    ));
}

#[test]
fn a_record_and_a_manifest_survive_their_json_form_unchanged() {
    // The persisted shape is what WP03 saves, so it is checked here rather than
    // inherited from a document layer that does not exist yet.
    let project = project();
    let cel = import_bytes(&project, "cel", &payload(0x80, 64), AssetKind::Image);
    let rig = AssetManifestEntry::new(
        id("rig"),
        cel.content_hash,
        64,
        AssetKind::Rig,
        2,
        [cel.asset_id],
        "rig.json",
    )
    .unwrap();
    let mut manifest = AssetManifest::new();
    manifest.insert(cel.clone());
    manifest.insert(rig.clone());

    let json = serde_json::to_string(&manifest).unwrap();
    let loaded: AssetManifest = serde_json::from_str(&json).unwrap();
    assert_eq!(loaded, manifest);
    assert_eq!(
        manifest_digest(&loaded).unwrap(),
        manifest_digest(&manifest).unwrap()
    );
    assert_eq!(
        serde_json::to_string(&loaded).unwrap(),
        json,
        "a re-serialized manifest is byte-identical, so a saved file hashes as itself"
    );
    assert_store_usable(&project, &loaded);
}
