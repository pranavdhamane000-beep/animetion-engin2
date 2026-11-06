//! Read-only resolution and resolution diagnostics (spec 11.3, 11.7, 11.11).
//!
//! Resolution is the read half of the package, and the rule that shapes it is spec
//! 11.7's: export verifies everything before frame zero, while preview may reuse a
//! verified result but must *report* what is missing or mismatched. Those are two
//! policies over one path, so they are one function with a policy argument rather
//! than two functions that could drift.
//!
//! Nothing in here writes, creates or deletes. That is what lets WP04's scene
//! evaluation call it while producing a canonical snapshot: a resolution that touched
//! the filesystem could make the snapshot depend on a cache state, and spec 7.1's rule
//! is that canonical state never includes what happened to be warm.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anim_core::diagnostic::Diagnostic;
use anim_core::id::AssetId;

use crate::hash::{hash_file, ContentHash};
use crate::manifest::{AssetManifest, AssetManifestEntry};
use crate::store::AssetStore;
use crate::AssetError;

/// How much to trust an object before handing its path back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolutionPolicy {
    /// Check the record and the path grammar, and that the file exists.
    ///
    /// The default for [`resolve`], and the only one that is cheap enough to run per
    /// scene evaluation. Spec 11.7's preview path: the bytes were verified when they
    /// were imported, and re-hashing a gigabyte of cels to draw a frame is not what
    /// "may use a verified cached result" means.
    Presence,
    /// Also compare the length on disk with the record's `byte_length`.
    ///
    /// O(1) and catches the common corruption — a truncated or appended file — which
    /// is why it is separate from content verification.
    Length,
    /// Also read the object and compare its digest.
    ///
    /// What spec 11.7 requires of export before frame zero, and what WP11's
    /// verification pass uses.
    Content,
}

/// One record, its path, and the policy that was applied to it.
///
/// `policy` is part of the value rather than a caller's memory: "resolved" under
/// `Presence` is not a claim that the bytes were read, and a report that says the
/// asset was verified when it was only found would be a false assurance with a
/// timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAsset {
    pub entry: AssetManifestEntry,
    pub canonical_path: PathBuf,
    pub policy: ResolutionPolicy,
}

/// Resolve one id to its record and canonical object path, applying
/// [`ResolutionPolicy::Presence`] (spec 11.9).
pub fn resolve(
    store: &AssetStore,
    manifest: &AssetManifest,
    asset_id: AssetId,
) -> Result<ResolvedAsset, AssetError> {
    resolve_with(store, manifest, asset_id, ResolutionPolicy::Presence)
}

/// Resolve one id under an explicit verification policy.
///
/// # Errors
///
/// [`AssetError::MissingAssetId`] when the manifest holds no such record,
/// [`AssetError::MissingObject`] when it holds one whose object is not on disk,
/// [`AssetError::SizeMismatch`] or [`AssetError::HashMismatch`] when the policy asks
/// for a check the bytes do not pass, and [`AssetError::Io`] when the path is not
/// inside the store.
pub fn resolve_with(
    store: &AssetStore,
    manifest: &AssetManifest,
    asset_id: AssetId,
    policy: ResolutionPolicy,
) -> Result<ResolvedAsset, AssetError> {
    let entry = manifest
        .get(&asset_id)
        .ok_or_else(|| AssetError::MissingAssetId {
            asset_id: asset_id.to_canonical_string(),
        })?;
    let canonical_path = store.record_path(entry)?;

    match policy {
        ResolutionPolicy::Presence => {
            if !canonical_path.is_file() {
                return Err(AssetError::MissingObject {
                    asset_id: asset_id.to_canonical_string(),
                    path: canonical_path.display().to_string(),
                });
            }
        }
        ResolutionPolicy::Length | ResolutionPolicy::Content => {
            let (length, actual) = stat_and_hash(entry, &canonical_path, policy)?;
            if length != entry.byte_length {
                return Err(AssetError::SizeMismatch {
                    asset_id: asset_id.to_canonical_string(),
                    expected: entry.byte_length,
                    actual: length,
                });
            }
            if let Some(actual_hash) = actual {
                if actual_hash != entry.content_hash {
                    return Err(AssetError::HashMismatch {
                        asset_id: asset_id.to_canonical_string(),
                        expected: entry.content_hash.to_hex(),
                        actual: actual_hash.to_hex(),
                    });
                }
            }
        }
    }

    Ok(ResolvedAsset {
        entry: entry.clone(),
        canonical_path,
        policy,
    })
}

/// The length of an object, and its digest when the policy asks for one.
fn stat_and_hash(
    entry: &AssetManifestEntry,
    path: &std::path::Path,
    policy: ResolutionPolicy,
) -> Result<(u64, Option<ContentHash>), AssetError> {
    let metadata = std::fs::metadata(path).map_err(|_| AssetError::MissingObject {
        asset_id: entry.asset_id.to_canonical_string(),
        path: path.display().to_string(),
    })?;
    let length = metadata.len();
    if policy == ResolutionPolicy::Content {
        let (hash, hashed_len) = hash_file(path)?;
        // A file that changed size between the stat and the read is reported by the
        // caller's length comparison, which is the finding; nothing retries here.
        return Ok((hashed_len, Some(hash)));
    }
    Ok((length, None))
}

/// Verify one record against the store (spec 11.9's `verify`).
///
/// Delegated to [`AssetStore::verify`] so that the crate-level signature 11.9 names
/// and the method on the store are the same code rather than two readings of it.
pub fn verify(store: &AssetStore, entry: &AssetManifestEntry) -> Result<(), AssetError> {
    store.verify(entry)
}

/// The dependency closure of one asset, in traversal order, refusing a cycle.
///
/// Depth-first over each record's dependency list in its canonical order, so the
/// result is a function of the manifest alone. Spec 11.7 requires that duplicate
/// dependencies be rejected at construction, so each edge is visited once per parent;
/// a shared dependency reached by two paths still appears once in the closure.
pub fn dependency_closure(
    manifest: &AssetManifest,
    asset_id: AssetId,
) -> Result<Vec<AssetId>, AssetError> {
    let mut closure = BTreeSet::new();
    let mut path = Vec::new();
    visit(manifest, asset_id, &mut closure, &mut path)?;
    // The set, not the traversal stack: `visit` pops as it unwinds, and the closure
    // is the set of assets reachable from this one, in canonical order.
    Ok(closure.into_iter().collect())
}

fn visit(
    manifest: &AssetManifest,
    asset_id: AssetId,
    closure: &mut BTreeSet<AssetId>,
    path: &mut Vec<AssetId>,
) -> Result<(), AssetError> {
    if path.contains(&asset_id) {
        let mut cycle: Vec<String> = path
            .iter()
            .skip_while(|id| **id != asset_id)
            .map(ToString::to_string)
            .collect();
        cycle.push(asset_id.to_string());
        return Err(AssetError::DependencyCycle {
            asset_id: asset_id.to_canonical_string(),
            path: cycle.join(" -> "),
        });
    }
    if closure.contains(&asset_id) {
        return Ok(());
    }
    let identifier = asset_id.to_canonical_string();
    let entry = manifest.get(&asset_id).ok_or_else(|| {
        // A dependency edge to a record the manifest does not hold: the asset cannot
        // be resolved, and the referring record is where the fault is.
        AssetError::MissingAssetId {
            asset_id: identifier.clone(),
        }
    })?;
    path.push(asset_id);
    for dependency in entry.dependencies() {
        visit(manifest, *dependency, closure, path)?;
    }
    path.pop();
    closure.insert(asset_id);
    Ok(())
}

/// Check every record's dependency edges: no dangling reference, no cycle.
///
/// Called at manifest construction by WP03's command validation, and here for tests
/// and for a recovery scan that has to know whether a file is trustworthy before
/// anything resolves through it.
pub fn validate_manifest(manifest: &AssetManifest) -> Result<(), AssetError> {
    for asset_id in manifest.asset_ids() {
        dependency_closure(manifest, asset_id)?;
    }
    Ok(())
}

/// What preview needs to draw: the verified set, and one finding per asset it could
/// not verify (spec 11.11's "preview … reports missing or mismatched content").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerificationReport {
    /// Records whose object passed the policy, in canonical order.
    pub verified: Vec<AssetId>,
    /// Records with no object on disk. The preview draws a placeholder for each.
    pub missing: Vec<AssetId>,
    /// Records whose object exists but disagrees with the manifest, each with the
    /// finding that says how. The preview must not draw these at all, because a stale
    /// object is worse than a placeholder: it looks like the frame the operator asked
    /// for.
    pub mismatched: Vec<(AssetId, AssetError)>,
}

impl VerificationReport {
    /// True when every record verified, which is what export checks before frame zero.
    #[must_use]
    pub fn is_complete(&self, manifest: &AssetManifest) -> bool {
        self.missing.is_empty()
            && self.mismatched.is_empty()
            && self.verified.len() == manifest.len()
    }

    /// The findings as diagnostics, in canonical order, for a panel or a report.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut out: Vec<Diagnostic> = self
            .missing
            .iter()
            .map(|id| {
                AssetError::MissingObject {
                    asset_id: id.to_canonical_string(),
                    path: String::new(),
                }
                .to_diagnostic()
            })
            .chain(self.mismatched.iter().map(|(_, e)| e.to_diagnostic()))
            .collect();
        out.sort_by(|a, b| a.code.as_str().cmp(b.code.as_str()));
        out
    }
}

/// Verify every record under `policy` and collect the findings.
///
/// Unlike [`AssetStore::verify_all`], this does not stop at the first failure: a
/// preview needs the whole set so that it can mark every affected cel, and an export
/// report needs to name all of them rather than one at a time per run.
pub fn verify_report(
    store: &AssetStore,
    manifest: &AssetManifest,
    policy: ResolutionPolicy,
) -> VerificationReport {
    let mut report = VerificationReport::default();
    for entry in manifest.iter() {
        match resolve_with(store, manifest, entry.asset_id, policy) {
            Ok(_) => report.verified.push(entry.asset_id),
            Err(AssetError::MissingObject { .. }) => report.missing.push(entry.asset_id),
            Err(e) => report.mismatched.push((entry.asset_id, e)),
        }
    }
    report
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
    use crate::manifest::AssetKind;
    use uuid::Uuid;

    fn id(label: &str) -> AssetId {
        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        AssetId::derive(&namespace, label.as_bytes())
    }

    struct Fixture {
        dir: tempfile::TempDir,
        store: AssetStore,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = AssetStore::init(dir.path()).unwrap();
        Fixture { dir, store }
    }

    fn publish(fixture: &Fixture, bytes: &[u8]) -> ContentHash {
        let hash = hash_bytes(bytes);
        fixture.store.create_object_parent(&hash).unwrap();
        std::fs::write(fixture.store.object_path(&hash), bytes).unwrap();
        hash
    }

    fn entry(label: &str, hash: ContentHash, length: u64, deps: &[AssetId]) -> AssetManifestEntry {
        AssetManifestEntry::new(
            id(label),
            hash,
            length,
            AssetKind::Image,
            1,
            deps.to_vec(),
            format!("{label}.png"),
        )
        .unwrap()
    }

    #[test]
    fn resolve_returns_the_record_and_its_canonical_path() {
        let fixture = fixture();
        let hash = publish(&fixture, b"cel bytes");
        let mut manifest = AssetManifest::new();
        manifest.insert(entry("a", hash, 9, &[]));
        let resolved = resolve(&fixture.store, &manifest, id("a")).unwrap();
        assert_eq!(resolved.entry, manifest.get(&id("a")).unwrap().clone());
        assert_eq!(resolved.canonical_path, fixture.store.object_path(&hash));
        assert_eq!(resolved.policy, ResolutionPolicy::Presence);
        assert!(resolved.canonical_path.is_file());
    }

    #[test]
    fn resolve_distinguishes_the_three_kinds_of_absent() {
        let fixture = fixture();
        let published = publish(&fixture, b"cel bytes");
        let mut manifest = AssetManifest::new();
        manifest.insert(entry("a", published, 9, &[]));
        // An object that is not stored.
        manifest.insert(entry("gone", hash_bytes(b"never written"), 13, &[]));

        let e = resolve(&fixture.store, &manifest, id("nonsense")).unwrap_err();
        assert!(matches!(e, AssetError::MissingAssetId { .. }), "{e}");
        let e = resolve(&fixture.store, &manifest, id("gone")).unwrap_err();
        assert!(matches!(e, AssetError::MissingObject { .. }), "{e}");
        assert!(e.to_string().contains("objects"), "{e}");
        // And the id appears in the message, since the operator's question is which one.
        assert!(e.to_string().contains(&id("gone").to_string()), "{e}");
    }

    #[test]
    fn policies_differ_in_what_they_catch() {
        let fixture = fixture();
        let bytes = b"0123456789".to_vec();
        let hash = publish(&fixture, &bytes);
        let mut manifest = AssetManifest::new();
        manifest.insert(entry("a", hash, bytes.len() as u64, &[]));
        let path = fixture.store.object_path(&hash);

        // All three agree while the object is intact.
        for policy in [
            ResolutionPolicy::Presence,
            ResolutionPolicy::Length,
            ResolutionPolicy::Content,
        ] {
            assert!(resolve_with(&fixture.store, &manifest, id("a"), policy).is_ok());
        }

        // Corruption of the same length: only a content check can see it.
        let mut flipped = bytes.clone();
        flipped[4] ^= 0xFF;
        std::fs::write(&path, &flipped).unwrap();
        assert!(resolve_with(
            &fixture.store,
            &manifest,
            id("a"),
            ResolutionPolicy::Presence
        )
        .is_ok());
        assert!(resolve_with(&fixture.store, &manifest, id("a"), ResolutionPolicy::Length).is_ok());
        let e = resolve_with(
            &fixture.store,
            &manifest,
            id("a"),
            ResolutionPolicy::Content,
        )
        .unwrap_err();
        assert!(matches!(e, AssetError::HashMismatch { .. }), "{e}");
        assert!(e.to_string().contains(&hash.to_hex()), "{e}");

        // Truncation: the length check sees it without reading the payload.
        std::fs::write(&path, &bytes[..4]).unwrap();
        let e =
            resolve_with(&fixture.store, &manifest, id("a"), ResolutionPolicy::Length).unwrap_err();
        assert!(matches!(e, AssetError::SizeMismatch { .. }), "{e}");
        assert!(e.to_string().contains("10"), "{e}");
        assert!(e.to_string().contains('4'), "{e}");
    }

    #[test]
    fn closure_is_depth_first_with_shared_dependencies_listed_once() {
        //   rig ── cel ──┐
        //    │           ├── pose
        //    └── mat ────┘
        let fixture = fixture();
        let hash = publish(&fixture, b"x");
        let mut manifest = AssetManifest::new();
        manifest.insert(entry("pose", hash, 1, &[]));
        manifest.insert(entry("cel", hash, 1, &[id("pose")]));
        manifest.insert(entry("mat", hash, 1, &[id("pose")]));
        manifest.insert(entry("rig", hash, 1, &[id("cel"), id("mat")]));

        let closure = dependency_closure(&manifest, id("rig")).unwrap();
        let mut expected = vec![id("cel"), id("pose"), id("mat"), id("rig")];
        expected.sort();
        assert_eq!(closure, expected, "each asset once, in id order");
        // A leaf's closure is itself.
        assert_eq!(
            dependency_closure(&manifest, id("pose")).unwrap(),
            vec![id("pose")]
        );
    }

    #[test]
    fn a_dependency_cycle_is_named_as_one_and_not_traversed() {
        // The constructor refuses a self-edge, so the reachable case is two records
        // pointing at each other.
        let fixture = fixture();
        let hash = publish(&fixture, b"x");
        let mut manifest = AssetManifest::new();
        manifest.insert(entry("a", hash, 1, &[id("b")]));
        manifest.insert(entry("b", hash, 1, &[id("a")]));

        let e = dependency_closure(&manifest, id("a")).unwrap_err();
        assert!(matches!(e, AssetError::DependencyCycle { .. }), "{e}");
        let text = e.to_string();
        assert!(text.contains(" -> "), "{text}");
        assert!(text.contains(&id("a").to_string()), "{text}");
        assert!(text.contains(&id("b").to_string()), "{text}");
        // The same cycle is reported from either end, and validation finds it without
        // being told where to look.
        assert!(matches!(
            dependency_closure(&manifest, id("b")).unwrap_err(),
            AssetError::DependencyCycle { .. }
        ));
        assert!(matches!(
            validate_manifest(&manifest).unwrap_err(),
            AssetError::DependencyCycle { .. }
        ));
        drop(fixture);
    }

    #[test]
    fn a_dangling_dependency_edge_is_reported_as_a_missing_asset() {
        let fixture = fixture();
        let hash = publish(&fixture, b"x");
        let mut manifest = AssetManifest::new();
        manifest.insert(entry("a", hash, 1, &[id("absent")]));
        let e = dependency_closure(&manifest, id("a")).unwrap_err();
        assert!(matches!(e, AssetError::MissingAssetId { .. }), "{e}");
        assert!(e.to_string().contains(&id("absent").to_string()), "{e}");
        assert!(matches!(
            validate_manifest(&manifest).unwrap_err(),
            AssetError::MissingAssetId { .. }
        ));
    }

    #[test]
    fn an_acyclic_manifest_validates_in_any_shape() {
        let fixture = fixture();
        let hash = publish(&fixture, b"x");
        let mut manifest = AssetManifest::new();
        manifest.insert(entry("a", hash, 1, &[]));
        manifest.insert(entry("b", hash, 1, &[id("a")]));
        manifest.insert(entry("c", hash, 1, &[id("a"), id("b")]));
        validate_manifest(&manifest).unwrap();
        // Deeply nested but finite, with the same leaf reached many ways.
        let mut wide = AssetManifest::new();
        wide.insert(entry("leaf", hash, 1, &[]));
        for i in 0..12 {
            wide.insert(entry(&format!("branch-{i}"), hash, 1, &[id("leaf")]));
        }
        let deps: Vec<AssetId> = (0..12).map(|i| id(&format!("branch-{i}"))).collect();
        wide.insert(entry("root", hash, 1, &deps));
        validate_manifest(&wide).unwrap();
        assert_eq!(
            dependency_closure(&wide, id("root")).unwrap().len(),
            14,
            "leaf + 12 branches + root"
        );
    }

    #[test]
    fn verify_report_classifies_every_record_and_says_which() {
        let fixture = fixture();
        let good = publish(&fixture, b"good bytes");
        let corrupt = publish(&fixture, b"corrupt-me");
        let mut manifest = AssetManifest::new();
        manifest.insert(entry("good", good, 10, &[]));
        manifest.insert(entry("corrupt", corrupt, 12, &[]));
        manifest.insert(entry("absent", hash_bytes(b"never written"), 13, &[]));
        // Break the second object while keeping its length.
        let path = fixture.store.object_path(&corrupt);
        std::fs::write(&path, b"XXXXXXXXXXXX").unwrap();

        let report = verify_report(&fixture.store, &manifest, ResolutionPolicy::Content);
        assert_eq!(report.verified, vec![id("good")]);
        assert_eq!(report.missing, vec![id("absent")]);
        assert_eq!(report.mismatched.len(), 1);
        let (bad_id, e) = &report.mismatched[0];
        assert_eq!(*bad_id, id("corrupt"));
        assert!(matches!(e, AssetError::HashMismatch { .. }), "{e}");
        assert!(!report.is_complete(&manifest));

        let diagnostics = report.diagnostics();
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].code.as_str(), "asset.hash_mismatch");
        assert_eq!(diagnostics[1].code.as_str(), "asset.missing_object");
        // Each names the asset it is about, which is what a panel filters on.
        for d in &diagnostics {
            assert!(
                d.context
                    .values()
                    .any(|v| v.contains(&id("corrupt").to_string())
                        || v.contains(&id("absent").to_string())),
                "{d:?}"
            );
        }

        // A complete manifest reports itself as one.
        let only_good = AssetManifest::from_records(
            crate::manifest::MANIFEST_SCHEMA_VERSION,
            [entry("good", good, 10, &[])],
        )
        .unwrap();
        let clean = verify_report(&fixture.store, &only_good, ResolutionPolicy::Content);
        assert!(clean.is_complete(&only_good));
        assert!(clean.diagnostics().is_empty());
    }

    #[test]
    fn resolution_never_writes() {
        // The read path must not create, replace or touch a byte: WP04's snapshot
        // identity cannot depend on a cache that a read warmed. Content rather than
        // modification time is the comparison, because a timestamp proves less than a
        // digest and `SystemTime` is a denied type for good reason.
        let fixture = fixture();
        let hash = publish(&fixture, b"cel");
        let mut manifest = AssetManifest::new();
        manifest.insert(entry("a", hash, 3, &[]));
        let before = snapshot(&fixture.dir);
        resolve_with(
            &fixture.store,
            &manifest,
            id("a"),
            ResolutionPolicy::Content,
        )
        .unwrap();
        let after = snapshot(&fixture.dir);
        assert_eq!(
            before, after,
            "the store is byte-identical after a resolution"
        );
    }

    /// Every file under the fixture, as `(path, digest, length)`, in path order.
    fn snapshot(dir: &tempfile::TempDir) -> Vec<(String, String, u64)> {
        walk(dir.path())
            .into_iter()
            .map(|path| {
                let (digest, length) = hash_file(&path).unwrap();
                (path.display().to_string(), digest.to_hex(), length)
            })
            .collect()
    }

    fn walk(path: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![path.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }
}
