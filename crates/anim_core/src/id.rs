//! Typed stable identifiers and document revisions (spec 10.8).
//!
//! Seven ID types, one per referenced object class. They are separate types
//! rather than one `Id` wrapper because the spec's error taxonomy and diagnostics
//! name them individually — an `AssetId` where a `NodeId` was expected is a bug in
//! the caller, and a type system that accepts it silently would move that bug into
//! the data.
//!
//! Identity is *created once and copied*, never regenerated: spec 10.8 says IDs
//! are "generated once at creation and copied through save/load, undo and
//! evaluation", and spec 11.6 requires `AssetId` to survive reimport while a
//! changed content hash produces a new asset revision. So the logical identity and
//! the content digest are deliberately different axes, and this file implements
//! only the first one.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::error::CoreError;

/// The one canonical spelling of a UUID: 36 characters, lowercase, hyphenated
/// (spec 5.1's serialization rule for the `uuid` dependency).
///
/// `Uuid::parse_str` is deliberately permissive — it also accepts braces, a
/// `urn:uuid:` prefix and the 32-digit simple form, and `uuid`'s own serde support
/// routes through it. Every one of those spellings denotes a valid UUID but only
/// one of them is canonical, and a document that accepted several would contain two
/// byte streams with the same meaning, which is what breaks hash equality against
/// a saved file. The round-trip test below is the strictness check: re-rendering a
/// parsed value must reproduce the input exactly.
fn parse_canonical_uuid(raw: &str) -> Result<Uuid, CoreError> {
    let rejected = || CoreError::IdentifierParse {
        raw: raw.to_owned(),
    };
    let parsed = Uuid::parse_str(raw).map_err(|_| rejected())?;
    if parsed.to_string() != raw {
        return Err(rejected());
    }
    Ok(parsed)
}

/// Define one typed identifier over a UUID.
///
/// A macro rather than seven copies because the bodies are required to be
/// identical: any drift between how `SceneId` and `NodeId` parse, order or
/// serialize would be a silent asymmetry in the one thing every package trusts.
macro_rules! typed_id {
    ($(#[$attr:meta])* $name:ident, $label:literal) => {
        $(#[$attr])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(pub Uuid);

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl std::cmp::Ord for $name {
            /// Compared over the 16 bytes, spelled out here rather than inherited
            /// from `derive(Ord)` on `Uuid`. `uuid` is caret-pinned, so a minor bump
            /// that changed its internal representation would otherwise silently
            /// reorder every manifest record, scene child and event tie-break in the
            /// engine (spec 13.7: "UUID comparison uses its 16 bytes").
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.0.as_bytes().cmp(other.0.as_bytes())
            }
        }

        impl $name {
            /// The kind name used in diagnostics and canonical domain tags.
            #[must_use]
            pub const fn kind(&self) -> &'static str {
                $label
            }

            /// Derive this identity from a namespace and a name, deterministically
            /// and with no entropy source (spec 5.1 pins `uuid` to the v5 feature
            /// only; see DECISIONS.md #23).
            ///
            /// Deliberately *not* content-addressed for assets: spec 11.6 keeps an
            /// `AssetId` stable across a reimport whose bytes changed, so asset
            /// identity and asset content are two axes. Callers choose the name —
            /// an import path, a logical label, an existing id plus a discriminator
            /// — and WP03 decides that policy.
            #[must_use]
            pub fn derive(namespace: &Uuid, name: &[u8]) -> Self {
                Self(Uuid::new_v5(namespace, name))
            }

            /// Wrap an already-decided UUID. Public because WP03's document loader
            /// holds UUIDs parsed from a manifest and must not re-derive them.
            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// The underlying UUID, by value.
            #[must_use]
            pub const fn uuid(&self) -> Uuid {
                self.0
            }

            /// The 16 bytes `Ord` compares, exposed so a caller can use the same
            /// key the type uses (spec 13.7: "UUID comparison uses its 16 bytes").
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }

            /// Parse the canonical lowercase hyphenated form. Strict: see
            /// [`parse_canonical_uuid`].
            pub fn parse(raw: &str) -> Result<Self, CoreError> {
                Ok(Self(parse_canonical_uuid(raw)?))
            }

            /// The canonical text. Kept as a method so callers that must embed an
            /// id in a path or a JSON key do not reach through `.0` and pick a
            /// different format off `uuid`'s several display types.
            #[must_use]
            pub fn to_canonical_string(&self) -> String {
                self.0.to_string()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            /// String-only and strict. `uuid`'s own deserializer would also accept
            /// a 16-element sequence and any of the loose spellings; neither can
            /// become a persisted form, or a saved document stops being comparable
            /// to its own hash.
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Self::parse(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

typed_id!(
    /// Identity of one project document, stable across every save of that document.
    /// Spec 11.6 admits a recovered file only on a "matching project ID", so this is
    /// the value that distinguishes one project's history from another's.
    ProjectId, "project");
typed_id!(
    /// Identity of one scene within a project.
    SceneId, "scene");
typed_id!(
    /// Identity of one node in the scene graph. Ordered by byte value, which is the
    /// final tie-break for evaluation when priorities match (spec 31).
    NodeId, "node");
typed_id!(
    /// Identity of one property track.
    TrackId, "track");
typed_id!(
    /// Identity of one logical asset. Survives reimport with changed bytes; the
    /// content hash moves, this does not (spec 11.6).
    AssetId, "asset");
typed_id!(
    /// Identity of one command instance. Replay dedup compares the canonical
    /// envelope hash, not this alone (spec 12.7), but the id is what a diagnostic
    /// names when it reports a rejected command.
    CommandId, "command");
typed_id!(
    /// Identity of an event *definition*, as authored. Distinct from an occurrence:
    /// spec 13.7 derives `EventOccurrenceId` from `(EventDefinitionId,
    /// occurrence_index)`, and that derivation lives with the event log in WP04, not
    /// here.
    EventDefinitionId, "event_definition");

/// A monotonically increasing document revision.
///
/// Zero is valid for a new document (spec 10.8). Increment is checked, so a
/// revision that reaches `u64::MAX` reports rather than wrapping onto zero — a
/// wrapped revision would make an old cache entry, an expired command and a stale
/// optimistic-concurrency check all look current at once.
///
/// The value is owned by `anim_project`'s document (spec 11.7's `save_atomic` and
/// the `ProjectError` variant `RevisionOverflow`); this type is where the
/// arithmetic and the no-wrap guarantee live, so that the document layer cannot
/// implement increment as `+= 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Revision(pub u64);

impl Revision {
    /// The revision of a document that has never been changed.
    pub const ZERO: Self = Self(0);

    /// The next revision, or an error at the width limit. Spec 6.4 requires exactly
    /// one increment per successful transaction, so this is the only way forward.
    pub fn checked_next(self) -> Result<Self, CoreError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(CoreError::ArithmeticOverflow {
                operation: "revision increment",
                value: self.0.to_string(),
            })
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

    /// A v5 UUID over a fixed namespace, so every fixture here is reproducible.
    fn fixture(label: &str) -> Uuid {
        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        Uuid::new_v5(&namespace, label.as_bytes())
    }

    #[test]
    fn canonical_text_round_trips_and_rejects_other_spellings() {
        let id = NodeId::from_uuid(fixture("node-a"));
        let text = id.to_canonical_string();
        assert_eq!(text.to_lowercase(), text, "must be lowercase");
        assert_eq!(text.len(), 36, "must be hyphenated, got {text}");
        assert_eq!(NodeId::parse(&text).unwrap(), id);

        for bad in [
            text.to_uppercase(),
            format!("{{{text}}}"),
            format!("urn:uuid:{text}"),
            text.replace('-', ""),
            "not-a-uuid".to_owned(),
            String::new(),
            text[..35].to_owned(),
        ] {
            assert!(
                NodeId::parse(&bad).is_err(),
                "must reject non-canonical id: {bad}"
            );
        }
    }

    #[test]
    fn serde_is_string_only_and_canonical() {
        let id = AssetId::from_uuid(fixture("asset"));
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.to_canonical_string()));
        assert_eq!(serde_json::from_str::<AssetId>(&json).unwrap(), id);
        // The byte-sequence form uuid's own support would accept is not a persisted
        // representation here.
        let bytes = serde_json::to_string(id.as_bytes()).unwrap();
        assert!(serde_json::from_str::<AssetId>(&bytes).is_err());
    }

    #[test]
    fn ordering_is_the_16_byte_order_not_a_crate_default() {
        // Every ordered container in the engine keys on this: manifest records by
        // AssetId, scene children by NodeId, ties by stable id. Comparing through
        // the bytes the type publishes is what makes that reproducible.
        let labels = ["a", "b", "c", "z", "asset/1", "asset/22", "asset/3"];
        let mut ids: Vec<NodeId> = labels
            .iter()
            .map(|l| NodeId::from_uuid(fixture(l)))
            .collect();
        ids.sort();
        for pair in ids.windows(2) {
            assert_eq!(
                pair[0].cmp(&pair[1]),
                pair[0].as_bytes().cmp(pair[1].as_bytes()),
                "Ord must agree with byte order"
            );
        }
        // And a hand-built pair at the extremes, so the ordering is not accidentally
        // inherited from insertion order.
        let low = NodeId::from_uuid(Uuid::from_bytes([0x00; 16]));
        let high = NodeId::from_uuid(Uuid::from_bytes([0xff; 16]));
        assert!(low < high);
        assert_eq!(low.as_bytes()[0], 0x00);
    }

    #[test]
    fn derivation_is_deterministic_and_input_sensitive() {
        let namespace = fixture("namespace");
        let a = TrackId::derive(&namespace, b"track-one");
        let b = TrackId::derive(&namespace, b"track-one");
        let c = TrackId::derive(&namespace, b"track-two");
        let d = TrackId::derive(&fixture("other-namespace"), b"track-one");
        assert_eq!(a, b, "same inputs must give the same id");
        assert_ne!(a, c, "different names must differ");
        assert_ne!(a, d, "different namespaces must differ");
    }

    #[test]
    fn distinct_id_types_do_not_substitute() {
        let raw = fixture("same-bytes");
        // Compilable only as separate types; this asserts the values are equal
        // while the types are not, which is the point of the seven wrappers.
        let scene = SceneId::from_uuid(raw);
        let track = TrackId::from_uuid(raw);
        assert_eq!(scene.uuid(), track.uuid());
        assert_eq!(SceneId::kind(&scene), "scene");
        assert_eq!(TrackId::kind(&track), "track");
        fn takes_scene(_: SceneId) {}
        takes_scene(scene);
    }

    #[test]
    fn revision_starts_at_zero_and_never_wraps() {
        assert_eq!(Revision::ZERO.0, 0);
        assert_eq!(Revision(0).checked_next().unwrap(), Revision(1));
        assert_eq!(
            Revision(u64::MAX - 1).checked_next().unwrap(),
            Revision(u64::MAX)
        );
        let e = Revision(u64::MAX).checked_next().unwrap_err();
        assert!(matches!(e, CoreError::ArithmeticOverflow { .. }), "{e}");
        assert!(e.to_string().contains(&u64::MAX.to_string()), "{e}");
    }
}
