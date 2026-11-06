//! Streaming transactional source import (spec 11.3, 11.7, 11.11).
//!
//! One function, and its whole job is ordering. Spec 11.7's sequence is what makes a
//! crash survivable, so it is followed literally: the bytes become durable before the
//! record exists, the record is only ever *returned*, and nothing here touches a
//! project (spec 11.4: "Import returns a record; it never directly mutates the
//! project"). WP03's `UpsertAsset` command is what commits it, and by the time that
//! command runs the object it names is already on disk and already hashed.
//!
//! The order is:
//!
//! 1. stage the stream into a temporary inside the object store, hashing and counting
//!    as the bounded chunks arrive;
//! 2. flush, device-sync, and prove the staged file's *length* matches the count;
//! 3. re-read the staged file and prove its digest matches the running digest — the
//!    proof that what is on disk is what was hashed, taken *before* any rename;
//! 4. derive the final path from the digest; if that object already exists, verify its
//!    length and digest and discard the temporary;
//! 5. otherwise create the parent directories and rename without overwrite;
//! 6. return the record.
//!
//! Step 3 is the one that looks redundant and is not: the running digest proves the
//! bytes that were *written*, and only re-reading proves the bytes that *survived*.
//! Spec 11.12's "failure immediately before object rename" and 11.8's "failed imports
//! leave no manifest entry" both land on that gap, and a lost write caught at step 3
//! costs an orphaned temporary rather than an object whose record can never verify.
//!
//! Step 6 is where an image's colour and alpha claims come from (spec 15.2, and the
//! ruling that came with schema version 2): they are read from the staged object,
//! classified by the one header pass in `image.rs`, and recorded. Nothing downstream
//! re-reads the header, because a second source for one fact is two renders for one
//! project. The read happens *before* publication rather than after, which is the order
//! that keeps 11.11's promise: an asset handed over as an `Image` whose bytes have no
//! readable header fails with no record, no temporary, and no orphan object.

use std::io::Read;

use anim_core::diagnostic::Diagnostic;
use anim_core::id::AssetId;
use anim_core::io::atomic_replace::Replacement;

use crate::hash::{ContentHash, StreamingHash};
use crate::image::{classify_image, image_metadata, ImageAssetClaims};
use crate::manifest::AssetKind;
use crate::manifest::AssetManifestEntry;
use crate::store::AssetStore;
use crate::{io_error, AssetError, CHUNK_SIZE};

/// Import one source stream into the object store and describe it as a record.
///
/// `name` is display metadata only (spec 11.6: "Original filenames are display
/// metadata and never determine object paths"), so two assets imported from
/// `cel-001.png` with different bytes get one digest each and neither file name
/// appears in a path.
///
/// This is spec 11.9's signature, which has no channel for a finding that is not a
/// failure — see [`import_observed`] for the version that reports them.
///
/// # Errors
///
/// [`AssetError::Cancelled`] if the source returns
/// [`std::io::ErrorKind::Interrupted`], which is this crate's cancellation contract:
/// a UI-driven import (WP10) passes a reader that reports Interrupted when the
/// operator stops it. Note the deliberate asymmetry with
/// [`crate::hash::hash_stream`], which retries the same error kind — a read of bytes
/// already decided upon may resume, but a source that is being stopped must not.
///
/// [`AssetError::Io`] for a staging or publication failure, [`AssetError::HashMismatch`]
/// or [`AssetError::SizeMismatch`] when the staged bytes, or an existing object of the
/// same digest, do not match what the stream says they are, and [`AssetError::MalformedAsset`]
/// or [`AssetError::UnsupportedDecoder`] when an `Image` record cannot be given claims
/// because the stored bytes have no header this build reads. Every one of these leaves
/// no record, no manifest change, and no temporary behind (spec 11.11).
pub fn import<R: Read>(
    store: &AssetStore,
    asset_id: AssetId,
    name: String,
    kind: AssetKind,
    importer_version: u32,
    source: R,
) -> Result<AssetManifestEntry, AssetError> {
    import_observed(
        store,
        asset_id,
        name,
        kind,
        importer_version,
        source,
        &mut |_| {},
    )
}

/// [`import`], with a channel for the findings that are not failures.
///
/// Today there is exactly one: an image carrying an ICC profile this engine does not
/// model, which is recorded as `Unclassified` and reported by name rather than guessed at
/// (spec 15.11's ban on a silent transfer-function fallback). 11.9 gives `import` no
/// parameter for it, and changing that signature is a spec change rather than this
/// package's to make, so the reporting-capable caller — WP10's import UI, an export
/// verification pass — takes this one. The record itself always carries the claim, so a
/// caller that drops the finding loses the *warning*, never the fact.
///
/// # Errors
///
/// As [`import`].
pub fn import_observed<R: Read>(
    store: &AssetStore,
    asset_id: AssetId,
    name: String,
    kind: AssetKind,
    importer_version: u32,
    mut source: R,
    observe: &mut dyn FnMut(Diagnostic),
) -> Result<AssetManifestEntry, AssetError> {
    let identifier = asset_id.to_canonical_string();

    // 1 and 2: stream, hash, count, stage, synchronize.
    let mut hasher = StreamingHash::new();
    let mut writer = store.stage()?;
    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                writer
                    .write(&buffer[..read])
                    .map_err(|e| io_error("stage source chunk", writer.path(), e.detail()))?;
                hasher.update(&buffer[..read])?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                // `writer` is dropped here, which deletes the temporary.
                return Err(AssetError::Cancelled {
                    asset_id: identifier,
                });
            }
            Err(e) => {
                return Err(AssetError::Io {
                    context: "read source stream",
                    path: String::new(),
                    detail: e.to_string(),
                });
            }
        }
    }
    let staged = writer.synchronize().map_err(|e| {
        io_error(
            "synchronize staged object",
            &store.objects_dir(),
            e.detail(),
        )
    })?;
    let expected_hash = hasher.finalize();
    let byte_length = hasher.bytes();

    // 3: the staged file on disk must reproduce the digest computed while writing it.
    let (staged_hash, staged_len) = crate::hash::hash_file(staged.path())?;
    if staged_len != byte_length {
        return Err(AssetError::SizeMismatch {
            asset_id: identifier,
            expected: byte_length,
            actual: staged_len,
        });
    }
    if staged_hash != expected_hash {
        return Err(AssetError::HashMismatch {
            asset_id: identifier,
            expected: expected_hash.to_hex(),
            actual: staged_hash.to_hex(),
        });
    }

    // 4: the claims an `Image` record must carry, read from the staged file before it
    // becomes an object. The order is what keeps 11.11's promise whole: a header that
    // cannot be classified is a refusal, and a refusal that happens after the rename
    // would leave a durable object with no record — legal under 11.8, but a store full of
    // them is not what a failed import should look like.
    let content_hash = ContentHash::from_bytes(*staged_hash.as_bytes());
    let image = match kind {
        AssetKind::Image => Some(image_claims(staged.path(), &identifier, observe)?),
        _ => None,
    };

    // 5: the final path is a function of the digest alone; either the object was already
    // there and gets verified, or the temporary becomes it.
    let destination = store.object_path(&content_hash);
    if destination.is_file() {
        // Same content already stored: verify it, then discard the temporary.
        // Spec 11.6 lets identical content share one immutable object, so this is the
        // dedupe path rather than an error — and the verification is why a corrupt
        // existing object is reported instead of silently inherited.
        verify_existing(&identifier, &content_hash, byte_length, &destination)?;
        drop(staged);
    } else {
        store.create_object_parent(&content_hash)?;
        staged
            .publish(&Replacement::create_only(&destination))
            .map_err(|e| io_error("publish object", &destination, e.detail()))?;
    }

    // 6: the record only after persistence. Both branches above end with the object
    // durable at `destination`, which is what lets this be one construction rather than
    // two, and the claims came from the same bytes the object now holds.
    AssetManifestEntry::new(
        asset_id,
        content_hash,
        byte_length,
        kind,
        importer_version,
        [],
        name,
        image,
    )
}

/// Read and classify one stored image's colour and alpha claims.
///
/// The header pass is bounded by `image.rs`'s own scan budget rather than by the file
/// size, so this is a few hundred bytes for an ordinary cel — the third read pass a
/// large asset costs (DECISIONS.md #30) is bounded, unlike the two hashing passes.
///
/// # Errors
///
/// Whatever [`image_metadata`] reports for a path it cannot parse, with the asset id
/// attached; `Io` if the object the store just named is not there.
fn image_claims(
    path: &std::path::Path,
    asset_id: &str,
    observe: &mut dyn FnMut(Diagnostic),
) -> Result<ImageAssetClaims, AssetError> {
    let metadata = image_metadata(path).map_err(|e| name_asset(e, asset_id))?;
    let (claims, finding) = classify_image(&metadata, asset_id);
    if let Some(finding) = finding {
        observe(finding);
    }
    Ok(claims)
}

/// Attach the asset id to a header fault.
///
/// [`image_metadata`] is also called on a bare path by a resolver that has no record in
/// hand, so it leaves the field empty and puts the path in the detail instead. Here the
/// record being built *is* the answer to "which asset", and spec 7.4 wants that as a
/// field rather than as a substring of a message.
fn name_asset(error: AssetError, asset_id: &str) -> AssetError {
    match error {
        AssetError::MalformedAsset { kind, detail, .. } => AssetError::MalformedAsset {
            asset_id: asset_id.to_owned(),
            kind,
            detail,
        },
        other => other,
    }
}

/// The existing object at a content-addressed path must be the bytes the new stream
/// hashes to, or the store is lying about one of its objects.
fn verify_existing(
    asset_id: &str,
    expected_hash: &ContentHash,
    expected_len: u64,
    path: &std::path::Path,
) -> Result<(), AssetError> {
    let (actual_hash, actual_len) = crate::hash::hash_file(path)?;
    if actual_len != expected_len {
        return Err(AssetError::SizeMismatch {
            asset_id: asset_id.to_owned(),
            expected: expected_len,
            actual: actual_len,
        });
    }
    if actual_hash != *expected_hash {
        return Err(AssetError::HashMismatch {
            asset_id: asset_id.to_owned(),
            expected: expected_hash.to_hex(),
            actual: actual_hash.to_hex(),
        });
    }
    Ok(())
}
