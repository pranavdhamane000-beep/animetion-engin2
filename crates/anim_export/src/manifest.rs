//! The job manifest (spec 18.3, 18.5, 18.8).
//!
//! A manifest is the reproducible identity of one export: the deterministic hashes (project
//! revision, asset manifest, resolved settings) plus an *environment* block (FFmpeg version,
//! adapter identity) kept strictly separate. 18.8 draws the line explicitly — "Reports contain
//! environment metadata separately from deterministic settings hashes" — because the whole point
//! of a deterministic export is that two runs on the pinned stack agree on the deterministic
//! half while the environment half documents *which* pinned stack produced it. Nothing in the
//! deterministic hashes may read a wall clock or a machine path; the environment block is
//! descriptive metadata a verifier can compare without re-deriving any of it.

use anim_assets::{manifest_digest, ContentHash};
use anim_core::hash::CanonicalBytes;
use anim_project::ProjectDocument;

use crate::ExportError;
use crate::ResolvedExport;

/// Schema version of the manifest record itself.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Canonical-bytes domain for the deterministic manifest hash.
pub const MANIFEST_DOMAIN: &str = "anim_engine/export_manifest/v1";

/// The environment half of the manifest: descriptive, never hashed into the deterministic set,
/// and reproduced only to say "this run happened on this tool and adapter." Populated from the
/// FFmpeg probe and `anim_render`'s adapter record at the [`crate::run`] call site; kept as
/// plain owned data so the manifest does not depend on the render crate's types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportEnvironment {
    pub ffmpeg_version: String,
    pub ffprobe_version: String,
    pub adapter_name: String,
    pub adapter_vendor_id: u32,
    pub adapter_device_id: u32,
    pub adapter_pci_bus_id: String,
    pub driver_version: String,
    pub gpu_backend: String,
}

/// The full job manifest (18.5's "project/asset/settings/environment job manifest").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobManifest {
    pub schema_version: u32,
    /// Identity of the exact document revision the export binds to (18.9: "ExportJob owns the
    /// immutable document revision, manifest and resolved settings").
    pub project_hash: ContentHash,
    /// `anim_assets::manifest_digest` of the project's embedded manifest.
    pub asset_manifest_hash: ContentHash,
    /// The resolved delivery parameters (from [`ResolvedExport`]).
    pub settings_hash: ContentHash,
    pub environment: ExportEnvironment,
}

impl JobManifest {
    /// Build the deterministic manifest from a document and a resolved export. The environment
    /// block is supplied by the caller (it comes from the FFmpeg/adapter probes, which the pure
    /// manifest must not itself run).
    pub fn build(
        document: &ProjectDocument,
        resolved: &ResolvedExport,
        environment: ExportEnvironment,
    ) -> Result<Self, ExportError> {
        Ok(Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            project_hash: project_identity_hash(document)?,
            asset_manifest_hash: manifest_digest(&document.assets)?,
            settings_hash: resolved.settings_hash,
            environment,
        })
    }

    /// The deterministic identity of a run: project + asset + settings hashes folded into one
    /// digest so a report can quote a single value while each part stays individually present.
    /// The environment block is deliberately excluded — comparing two runs' determinism must not
    /// be perturbed by a driver version bump that did not change a single output byte.
    pub fn deterministic_hash(&self) -> Result<ContentHash, ExportError> {
        let mut cp = CanonicalBytes::with_domain(MANIFEST_DOMAIN)?;
        cp.u32(self.schema_version);
        cp.bytes(self.project_hash.as_bytes());
        cp.bytes(self.asset_manifest_hash.as_bytes());
        cp.bytes(self.settings_hash.as_bytes());
        Ok(ContentHash::from_bytes(cp.digest()))
    }
}

/// Blake3 over the document's stable identity: schema version, project id and revision, each in
/// a fixed canonical order. This is the "immutable document revision" 18.9 binds the job to.
fn project_identity_hash(document: &ProjectDocument) -> Result<ContentHash, ExportError> {
    let mut cp = CanonicalBytes::with_domain("anim_engine/export_project/v1")?;
    cp.u32(document.schema_version);
    cp.bytes(document.project_id.as_bytes());
    cp.u64(document.revision.0);
    Ok(ContentHash::from_bytes(cp.digest()))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only"
    )]

    use super::*;

    #[test]
    fn deterministic_hash_ignores_the_environment_block() {
        // Two manifests identical on the deterministic halves but differing only on a driver
        // version must hash equal — that is the whole environment-separation guarantee.
        let base = JobManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            project_hash: ContentHash::from_bytes([1; 32]),
            asset_manifest_hash: ContentHash::from_bytes([2; 32]),
            settings_hash: ContentHash::from_bytes([3; 32]),
            environment: ExportEnvironment {
                ffmpeg_version: "9.0.2".to_owned(),
                ffprobe_version: "9.0.2".to_owned(),
                adapter_name: "RTX 3050 A".to_owned(),
                adapter_vendor_id: 0x10DE,
                adapter_device_id: 0x28A3,
                adapter_pci_bus_id: "0000:01:00.0".to_owned(),
                driver_version: "617.14".to_owned(),
                gpu_backend: "Vulkan".to_owned(),
            },
        };
        let mut bumped = base.clone();
        bumped.environment.driver_version = "999.99".to_owned();

        assert_eq!(
            base.deterministic_hash().unwrap(),
            bumped.deterministic_hash().unwrap()
        );

        let mut moved = base.clone();
        moved.settings_hash = ContentHash::from_bytes([4; 32]);
        assert_ne!(
            base.deterministic_hash().unwrap(),
            moved.deterministic_hash().unwrap()
        );
    }
}
