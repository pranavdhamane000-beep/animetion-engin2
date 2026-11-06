//! Validation and canonical serialization (spec 12.3).
//!
//! One document has one byte stream. That is the claim this file exists to make, and
//! it is the claim spec 12.12 tests under the name "canonical serialization": a saved
//! `project.anim.json`, read back and written again, must produce the same bytes. If
//! it does not, then the digest of a project depends on the accident of which order
//! fields arrived, and every later identity in the engine — WP04's snapshot hash,
//! WP09's job hash — inherits the ambiguity.
//!
//! Two properties make it true, and neither is a convention:
//!
//! 1. Every object is written by a `Serialize` impl whose field order is the
//!    declaration order of the struct as spec 12.8 and spec 11.8 list it. Derived impls
//!    do this, and the impls written by hand in this crate do it in the same order.
//! 2. Every collection is either a `BTreeMap` — whose iteration order is the key's own
//!    `Ord`, and whose keys are ids ordered by their 16 bytes or paths ordered by their
//!    text — or a `Vec` that a constructor sorted and proved unique. There is no
//!    `HashMap` anywhere in the persisted shape, which `clippy.toml` enforces rather
//!    than this file trusting.
//!
//! The output is `serde_json`'s compact form. The pretty printer would be a `serde_json`
//! feature this build does not enable, and an indent policy is a second thing to
//! version; the file is a commit record, not a document a person edits by hand — the
//! only way to change it is an envelope.

use crate::document::ProjectDocument;
use crate::migrate;
use crate::{ProjectError, Result};

/// Validate a document and write its canonical bytes.
///
/// Validation is inside the serializer rather than at each call site because spec
/// 12.7's order — "Validate and serialize before touching disk" — is the only order in
/// which a save is safe, and a `serialize` that could be called without it would let a
/// second caller get that order wrong once.
///
/// # Errors
///
/// Whatever [`ProjectDocument::validate`] refuses, plus a serialization failure.
pub fn serialize_canonical(document: &ProjectDocument) -> Result<Vec<u8>> {
    document.validate()?;
    serde_json::to_vec(document).map_err(|e| ProjectError::Validation {
        location: "project document".to_string(),
        detail: format!("could not be serialized: {e}"),
    })
}

/// Read bytes into a document, migrating them if they are older (spec 12.5 step 9).
///
/// # Errors
///
/// See [`migrate::load_document`].
pub fn deserialize_canonical(bytes: &[u8]) -> Result<ProjectDocument> {
    migrate::load_document(bytes)
}

/// Read bytes and prove they are the bytes this build would write.
///
/// Returns the document when the round trip holds. The check is not a nicety: the
/// failure it catches is a document that parses, validates and then re-serializes
/// differently, which means two byte streams describe one state — exactly the
/// ambiguity ADR-0005's Consequence 5 was left open to prevent.
///
/// # Errors
///
/// [`ProjectError::Validation`] naming how the bytes disagreed, or any load failure.
pub fn verify_canonical_bytes(bytes: &[u8]) -> Result<ProjectDocument> {
    let document = deserialize_canonical(bytes)?;
    let written = serialize_canonical(&document)?;
    if written != bytes {
        return Err(ProjectError::Validation {
            location: "project document".to_string(),
            detail: format!(
                "is not canonical: {} bytes read, {} bytes written by this build, first difference at {first}",
                bytes.len(),
                written.len(),
                first = common_prefix(bytes, &written),
            ),
        });
    }
    Ok(document)
}

/// The offset of the first differing byte, or the shorter length when one is a prefix.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b.iter())
        .position(|(x, y)| x != y)
        .unwrap_or_else(|| a.len().min(b.len()))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use crate::property::{Easing, PropertyCurve, PropertyKey, PropertyPath, PropertyValue};
    use crate::scene::{Marker, NodeData, SceneData, TrackData};
    use anim_assets::{AssetKind, AssetManifestEntry};
    use anim_core::id::{AssetId, NodeId, ProjectId, SceneId, TrackId};
    use anim_core::rational::Rational;
    use anim_core::time::{FramePos, FrameRate};
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn derived(label: &str) -> Uuid {
        Uuid::new_v5(&namespace(), label.as_bytes())
    }

    fn node_id(label: &str) -> NodeId {
        NodeId::from_uuid(derived(label))
    }

    fn scene_id(label: &str) -> SceneId {
        SceneId::from_uuid(derived(label))
    }

    fn asset_id(label: &str) -> AssetId {
        AssetId::from_uuid(derived(label))
    }

    fn track_id(label: &str) -> TrackId {
        TrackId::from_uuid(derived(label))
    }

    /// A document with every persisted shape this crate has: two asset records with a
    /// dependency edge, a two-level hierarchy, a keyed property with a default, and
    /// two markers.
    fn rich_document_with_nodes_in(order: &[&str]) -> ProjectDocument {
        let mut document = ProjectDocument::new(
            ProjectId::derive(&namespace(), b"fixture"),
            FrameRate::new(30000, 1001).unwrap(),
        );

        let first = asset_id("cel-a");
        let second = asset_id("cel-b");
        // Inserted in the order the caller happens to hold them; the manifest's digest
        // and JSON both come out of the `BTreeMap`, not of this sequence.
        for (id, name, dependencies, payload) in [
            (second, "cel-b", vec![], &b"second"[..]),
            (first, "cel-a", vec![second], &b"a"[..]),
        ] {
            document.assets.insert(
                AssetManifestEntry::new(
                    id,
                    anim_assets::hash_bytes(payload),
                    payload.len() as u64,
                    AssetKind::Image,
                    1,
                    dependencies,
                    name,
                )
                .unwrap(),
            );
        }

        let nodes: Vec<NodeData> = order
            .iter()
            .map(|label| match *label {
                "root" => NodeData::new(node_id("root"), None, 0, "root"),
                other => NodeData::new(node_id(other), Some(node_id("root")), 0, other),
            })
            .collect();
        let mut scene = SceneData::from_parts("shot01", nodes, [], [], []).unwrap();
        scene
            .insert_track(TrackData::new(
                track_id("x"),
                node_id("root"),
                PropertyPath::new("transform.x").unwrap(),
                PropertyCurve::new(
                    [
                        PropertyKey::new(
                            FramePos::at_frame(4),
                            PropertyValue::Rational(Rational::new(1, 4).unwrap()),
                            Easing::Step,
                        ),
                        PropertyKey::new(
                            FramePos::at_frame(2),
                            PropertyValue::Text("hold".to_string()),
                            Easing::Linear,
                        ),
                    ],
                    Some(PropertyValue::Bool(false)),
                )
                .unwrap(),
            ))
            .unwrap();
        for (frame, label) in [(9, "hit"), (1, "start")] {
            scene
                .insert_marker(Marker::new(FramePos::at_frame(frame), label))
                .unwrap();
        }
        assert!(document.insert_scene(scene_id("shot01"), scene).is_none());
        document
    }

    fn rich_document() -> ProjectDocument {
        rich_document_with_nodes_in(&["root", "child"])
    }

    #[test]
    fn a_saved_document_reproduces_its_own_bytes() {
        let bytes = serialize_canonical(&rich_document()).unwrap();
        let document = verify_canonical_bytes(&bytes).expect("canonical bytes round trip");
        assert_eq!(document, rich_document());
        assert_eq!(serialize_canonical(&document).unwrap(), bytes);
    }

    #[test]
    fn bytes_do_not_depend_on_the_order_nodes_arrived_in() {
        let forward =
            serialize_canonical(&rich_document_with_nodes_in(&["root", "child"])).unwrap();
        let reversed =
            serialize_canonical(&rich_document_with_nodes_in(&["child", "root"])).unwrap();
        assert_eq!(
            forward, reversed,
            "a scene loaded in either order is one document, and two byte streams for it \
             would be two digests for one revision"
        );
    }

    #[test]
    fn an_invalid_document_produces_no_bytes() {
        // The types make most invalid states unconstructable, which is the point; the
        // reachable one is a version this build does not write, arrived at through a
        // field spec 12.8 lists as public.
        let mut document = rich_document();
        document.schema_version = 99;
        assert!(
            serialize_canonical(&document).is_err(),
            "spec 12.7's order is validate, then serialize, then disk"
        );
    }

    #[test]
    fn a_hand_edited_graph_that_parses_is_still_refused_on_load() {
        let bytes = serialize_canonical(&rich_document()).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let absent = node_id("absent").to_canonical_string();
        let scene_key = scene_id("shot01").to_canonical_string();
        let track_key = track_id("x").to_canonical_string();
        value["scenes"][&scene_key]["tracks"][&track_key]["node_id"] =
            serde_json::Value::from(absent);
        let edited = serde_json::to_vec(&value).unwrap();
        assert!(
            deserialize_canonical(&edited).is_err(),
            "a track bound to a node the scene does not have must not load"
        );
    }

    #[test]
    fn non_canonical_bytes_are_reported_with_the_offset_that_says_where() {
        let bytes = serialize_canonical(&rich_document()).unwrap();
        // One space after the opening brace. Swapping two trailing bytes would not
        // have proved anything, because the file ends in `}}` and the swap is then a
        // no-op — which is the failure this test had when it was first written.
        let mut padded = Vec::with_capacity(bytes.len() + 1);
        padded.extend_from_slice(&bytes[..1]);
        padded.push(b' ');
        padded.extend_from_slice(&bytes[1..]);
        let Err(ProjectError::Validation { detail, .. }) = verify_canonical_bytes(&padded) else {
            panic!("a file that does not re-serialize to itself is not canonical");
        };
        assert!(
            detail.contains("first difference at 1"),
            "the report names where the two spellings part: {detail}"
        );
        // And a file with the same values in a different field order is refused the
        // same way, even though it loads.
        assert!(deserialize_canonical(&padded).is_ok());
    }

    #[test]
    fn a_document_that_only_differs_in_field_order_is_refused() {
        let mut value = serde_json::to_value(rich_document()).unwrap();
        let object = value.as_object_mut().unwrap();
        let reordered = serde_json::json!({
            "scenes": object["scenes"].clone(),
            "assets": object["assets"].clone(),
            "frame_rate": object["frame_rate"].clone(),
            "revision": object["revision"].clone(),
            "project_id": object["project_id"].clone(),
            "simulation": object["simulation"].clone(),
            "schema_version": object["schema_version"].clone(),
        });
        let bytes = serde_json::to_vec(&reordered).unwrap();
        // Every value is correct and the document loads; it is still not canonical,
        // because a second spelling of one document is two digests.
        assert!(deserialize_canonical(&bytes).is_ok());
        assert!(verify_canonical_bytes(&bytes).is_err());
    }

    #[test]
    fn common_prefix_names_the_first_difference_not_the_last() {
        assert_eq!(common_prefix(b"abcd", b"abcd"), 4);
        assert_eq!(common_prefix(b"abcd", b"ab"), 2);
        assert_eq!(common_prefix(b"abcd", b"abXd"), 2);
    }
}
