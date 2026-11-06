//! Raw scenes, nodes, tracks and markers (spec 12.3).
//!
//! This is authored data, not evaluated data. Spec 13.2 states the division: WP04's
//! snapshots are built *from* what is here, and "no downstream system may read mutable
//! project internals directly". So nothing in this file computes a world transform,
//! resolves a blend or samples a curve — it stores what an operator authored, in the
//! order they authored it, with the two ordering rules spec 13.7 requires:
//!
//! - roots are visited in `NodeId` order, which is `BTreeMap`'s own iteration over an
//!   id whose `Ord` is the 16 bytes (spec 13.7, DECISIONS.md #22);
//! - children are visited in *persisted sibling order*, which is a separate number
//!   from identity and cannot be derived from it, because an operator's draw order is
//!   exactly the thing they change without renaming a node.
//!
//! Every rule here is enforced by one `validate`, and every mutation applies to a clone
//! and swaps only on success. Two rule sets — one incremental, one for load — would
//! disagree eventually, and a document that loads with a state its own commands refuse
//! is the failure spec 12.12's canonical-serialization clause exists to catch.

use std::collections::BTreeMap;

use anim_core::hash::CanonicalBytes;
use anim_core::id::{NodeId, TrackId};
use anim_core::time::FramePos;
use serde::{Deserialize, Serialize};

use crate::property::{PropertyCurve, PropertyKey, PropertyPath, PropertyValue};
use crate::{ProjectError, Result};

/// One node of a scene's hierarchy.
///
/// `sibling_order` is what spec 13.7's "children in persisted sibling order" means, and
/// it is unique among the children of one parent. For a root it is stored but never
/// consulted: root order is `NodeId` order, and giving roots a second ordering that
/// nothing reads would be a rule no test can prove.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeData {
    /// Identity, generated once and copied through save, load and undo (spec 10.8).
    pub node_id: NodeId,
    /// `None` is a root. A parent that does not exist is a validation error, never a
    /// silent re-parent.
    pub parent: Option<NodeId>,
    /// Position among its siblings, lowest first.
    pub sibling_order: u32,
    /// Display metadata only (spec 11.6's rule for asset names applies here too: a
    /// name never determines an ordering, a path or a hash).
    pub display_name: String,
    /// Spec 13.6: "Empty properties use authored defaults." Keyed by path, so a node's
    /// defaults have one order in JSON and in bytes.
    pub defaults: BTreeMap<PropertyPath, PropertyValue>,
}

impl NodeData {
    /// A node with no defaults.
    #[must_use]
    pub fn new(
        node_id: NodeId,
        parent: Option<NodeId>,
        sibling_order: u32,
        display_name: impl Into<String>,
    ) -> Self {
        Self {
            node_id,
            parent,
            sibling_order,
            display_name: display_name.into(),
            defaults: BTreeMap::new(),
        }
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.fixed_bytes(self.node_id.as_bytes());
        // An absent parent is a distinct state from any id, so it gets its own flag
        // rather than a zero uuid that could also be a real one.
        match self.parent {
            None => {
                cp.bool(false);
            }
            Some(parent) => {
                cp.bool(true);
                cp.fixed_bytes(parent.as_bytes());
            }
        }
        cp.u32(self.sibling_order);
        cp.text(&self.display_name);
        cp.u64(self.defaults.len() as u64);
        for (path, value) in &self.defaults {
            path.encode(cp);
            value.encode(cp);
        }
    }

    /// The node's canonical field stream with no domain tag, so that a command payload
    /// holding a node hashes these bytes rather than a second rendering of them. This is
    /// ADR-0005 Decision 8's rule for a manifest record, extended to every project object
    /// a payload can carry (spec 12.12's "canonical serialization" clause reaches command
    /// envelopes as well as documents).
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp);
        cp.into_vec()
    }
}

/// One property's key list, bound to one node and one path.
///
/// A track exists so that spec 13.2's "unique canonical times within each property"
/// has a container to be unique *within*: the property is the pair `(node, path)`, and
/// `SceneData` refuses a second track for one pair rather than merging two key lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackData {
    pub track_id: TrackId,
    pub node_id: NodeId,
    pub property: PropertyPath,
    pub curve: PropertyCurve,
}

impl TrackData {
    #[must_use]
    pub fn new(
        track_id: TrackId,
        node_id: NodeId,
        property: PropertyPath,
        curve: PropertyCurve,
    ) -> Self {
        Self {
            track_id,
            node_id,
            property,
            curve,
        }
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.fixed_bytes(self.track_id.as_bytes());
        cp.fixed_bytes(self.node_id.as_bytes());
        self.property.encode(cp);
        self.curve.encode(cp);
    }

    /// The track's canonical field stream, with no domain tag.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp);
        cp.into_vec()
    }
}

/// A named point on the timeline.
///
/// Identified by the pair, because spec 10.8's id list has no `MarkerId` and inventing
/// an identifier type is not this package's call: a marker that hash-distributed its
/// own identity would have to be derived from its position, which changes whenever the
/// operator drags it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Marker {
    pub position: FramePos,
    pub label: String,
}

impl Marker {
    #[must_use]
    pub fn new(position: FramePos, label: impl Into<String>) -> Self {
        Self {
            position,
            label: label.into(),
        }
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.i64(self.position.frame);
        cp.rational(&self.position.subframe());
        cp.text(&self.label);
    }

    /// The marker's canonical field stream, with no domain tag.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp);
        cp.into_vec()
    }
}

/// One scene: a hierarchy, its property tracks, and its markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneData {
    /// Non-empty, validated: an unnamed scene cannot be reported on.
    pub name: String,
    nodes: BTreeMap<NodeId, NodeData>,
    tracks: BTreeMap<TrackId, TrackData>,
    /// Sorted by `(position, label)` and unique in that pair, which is why this is
    /// private (DECISIONS.md #22's reading of spec 10.8's normalization rule).
    markers: Vec<Marker>,
}

impl SceneData {
    /// An empty scene.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if `name` is empty.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let scene = Self {
            name: name.into(),
            nodes: BTreeMap::new(),
            tracks: BTreeMap::new(),
            markers: Vec::new(),
        };
        scene.check_name()?;
        Ok(scene)
    }

    /// Rebuild a scene from its collections and validate the whole graph.
    ///
    /// This is the load path, and it is separate from [`SceneData::new`] plus
    /// [`SceneData::insert_node`] for one reason: a scene read from a file is inserted
    /// in `NodeId` order, so a child can arrive before its parent. Validating one node
    /// at a time would refuse a perfectly legal document. The aggregate rules are the
    /// same rules either way, which is why every mutator below runs this `validate`
    /// over a clone before committing.
    ///
    /// # Errors
    ///
    /// Any of [`SceneData::validate`]'s failures.
    pub fn from_parts(
        name: impl Into<String>,
        nodes: impl IntoIterator<Item = NodeData>,
        tracks: impl IntoIterator<Item = TrackData>,
        markers: impl IntoIterator<Item = Marker>,
    ) -> Result<Self> {
        let mut marker_list: Vec<Marker> = markers.into_iter().collect();
        marker_list.sort();
        let scene = Self {
            name: name.into(),
            nodes: nodes.into_iter().map(|node| (node.node_id, node)).collect(),
            tracks: tracks
                .into_iter()
                .map(|track| (track.track_id, track))
                .collect(),
            markers: marker_list,
        };
        scene.validate()?;
        Ok(scene)
    }

    /// The nodes, in `NodeId` order — which is the root visit order spec 13.7 names,
    /// because a root's position in this map is its identity.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeData> {
        self.nodes.values()
    }

    #[must_use]
    pub fn node(&self, node_id: &NodeId) -> Option<&NodeData> {
        self.nodes.get(node_id)
    }

    #[must_use]
    pub fn contains_node(&self, node_id: &NodeId) -> bool {
        self.nodes.contains_key(node_id)
    }

    /// The roots, in `NodeId` order (spec 13.7).
    pub fn roots(&self) -> impl Iterator<Item = &NodeData> {
        self.nodes
            .values()
            .filter(move |node| node.parent.is_none())
    }

    /// The children of one node, in persisted sibling order (spec 13.7).
    ///
    /// The `node_id` tie-break makes the order total even if a document ever reached
    /// here with two siblings sharing one order; `validate` refuses that state, so the
    /// branch is unreachable rather than a licence.
    #[must_use]
    pub fn children_of(&self, parent: &NodeId) -> Vec<&NodeData> {
        let mut children: Vec<&NodeData> = self
            .nodes
            .values()
            .filter(|node| node.parent.as_ref() == Some(parent))
            .collect();
        children.sort_by(|a, b| {
            a.sibling_order
                .cmp(&b.sibling_order)
                .then_with(|| a.node_id.cmp(&b.node_id))
        });
        children
    }

    /// Where a new child of `parent` would sort last.
    #[must_use]
    pub fn next_sibling_order(&self, parent: Option<&NodeId>) -> u32 {
        let highest = match parent {
            None => self
                .roots()
                .map(|node| node.sibling_order)
                .max()
                .unwrap_or(0),
            Some(parent) => self
                .children_of(parent)
                .into_iter()
                .map(|node| node.sibling_order)
                .max()
                .unwrap_or(0),
        };
        highest.saturating_add(1)
    }

    pub fn tracks(&self) -> impl Iterator<Item = &TrackData> {
        self.tracks.values()
    }

    #[must_use]
    pub fn track(&self, track_id: &TrackId) -> Option<&TrackData> {
        self.tracks.get(track_id)
    }

    /// The track bound to one `(node, property)` pair, if one exists.
    #[must_use]
    pub fn track_for(&self, node_id: &NodeId, property: &PropertyPath) -> Option<&TrackData> {
        self.tracks
            .values()
            .find(|track| &track.node_id == node_id && &track.property == property)
    }

    #[must_use]
    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    /// Insert or replace one node, returning what it displaced.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if the parent does not exist, if the node is its
    /// own parent, or if another child of the same parent already holds that sibling
    /// order.
    pub fn insert_node(&mut self, node: NodeData) -> Result<Option<NodeData>> {
        self.change(|scene| scene.nodes.insert(node.node_id, node))
    }

    /// Remove one node, returning it.
    ///
    /// Refused while the node has children or a track bound to it, rather than
    /// cascading. Two reasons, both spec-mandated: spec 13.11's "never skip invalid
    /// nodes silently" (a cascade would delete authored animation to complete one
    /// delete), and an exact inverse — spec 12.6 wants a command's inverse to restore
    /// the state, and a subtree deleted as a side effect cannot be restored by the
    /// single node this command removed.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if a child or a track still refers to the node.
    pub fn remove_node(&mut self, node_id: &NodeId) -> Result<Option<NodeData>> {
        if self.nodes.contains_key(node_id) {
            if self
                .nodes
                .values()
                .any(|other| other.parent.as_ref() == Some(node_id))
            {
                return Err(ProjectError::Validation {
                    location: format!("node {node_id}"),
                    detail: "still has children; reparent or remove them first".to_string(),
                });
            }
            if self.tracks.values().any(|track| &track.node_id == node_id) {
                return Err(ProjectError::Validation {
                    location: format!("node {node_id}"),
                    detail: "still has a track bound to it; remove the track first".to_string(),
                });
            }
        }
        self.change(|scene| scene.nodes.remove(node_id))
    }

    /// Insert or replace one track, returning what it displaced.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if the target node does not exist, or if a
    /// *different* track already binds the same `(node, property)` pair — one property
    /// has one key list (spec 13.2).
    pub fn insert_track(&mut self, track: TrackData) -> Result<Option<TrackData>> {
        self.change(|scene| scene.tracks.insert(track.track_id, track))
    }

    /// Remove one track, returning it.
    pub fn remove_track(&mut self, track_id: &TrackId) -> Result<Option<TrackData>> {
        self.change(|scene| scene.tracks.remove(track_id))
    }

    /// Insert one marker, refusing a duplicate `(position, label)` pair.
    pub fn insert_marker(&mut self, marker: Marker) -> Result<bool> {
        self.change(|scene| {
            let insert_at = scene.markers.partition_point(|existing| existing < &marker);
            let already = scene.markers.get(insert_at) == Some(&marker);
            if !already {
                scene.markers.insert(insert_at, marker);
            }
            !already
        })
    }

    /// Remove one marker, returning whether it was there.
    pub fn remove_marker(&mut self, marker: &Marker) -> Result<bool> {
        self.change(|scene| {
            let before = scene.markers.len();
            scene.markers.retain(|existing| existing != marker);
            scene.markers.len() != before
        })
    }

    /// Every rule this type exists to hold.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] naming the first offending object and the rule it
    /// broke: empty name, missing parent, self-parent, a hierarchy cycle, a repeated
    /// sibling order among one parent's children, a track targeting no node, two tracks
    /// on one property.
    pub fn validate(&self) -> Result<()> {
        self.check_name()?;

        for node in self.nodes.values() {
            match node.parent {
                None => {}
                Some(parent) if parent == node.node_id => {
                    return Err(ProjectError::Validation {
                        location: format!("node {}", node.node_id),
                        detail: "is its own parent".to_string(),
                    });
                }
                Some(parent) if !self.nodes.contains_key(&parent) => {
                    return Err(ProjectError::Validation {
                        location: format!("node {}", node.node_id),
                        detail: format!("names parent {parent}, which the scene does not have"),
                    });
                }
                Some(_) => {}
            }
        }

        if let Some(cycle) = self.find_cycle() {
            return Err(ProjectError::Validation {
                location: format!("node {}", cycle),
                detail: "is inside a parent cycle, which spec 13.11 rejects".to_string(),
            });
        }

        let mut orders: BTreeMap<(Option<NodeId>, u32), NodeId> = BTreeMap::new();
        for node in self.nodes.values() {
            if let Some(previous) = orders.insert((node.parent, node.sibling_order), node.node_id) {
                return Err(ProjectError::Validation {
                    location: format!("node {}", node.node_id),
                    detail: format!(
                        "and node {previous} both claim sibling order {} under the same parent",
                        node.sibling_order
                    ),
                });
            }
        }

        let mut bound: BTreeMap<(&NodeId, &PropertyPath), TrackId> = BTreeMap::new();
        for track in self.tracks.values() {
            if !self.nodes.contains_key(&track.node_id) {
                return Err(ProjectError::Validation {
                    location: format!("track {}", track.track_id),
                    detail: format!(
                        "targets node {}, which the scene does not have",
                        track.node_id
                    ),
                });
            }
            if let Some(previous) = bound.insert((&track.node_id, &track.property), track.track_id)
            {
                return Err(ProjectError::Validation {
                    location: format!("track {}", track.track_id),
                    detail: format!(
                        "and track {previous} both animate {path} on the same node, and spec 13.2 allows one key list per property",
                        path = track.property
                    ),
                });
            }
        }

        for pair in self.markers.windows(2) {
            if pair[0] == pair[1] {
                return Err(ProjectError::Validation {
                    location: format!("marker {:?}", pair[0].label),
                    detail: format!("appears twice at frame {}", pair[0].position.frame),
                });
            }
        }
        Ok(())
    }

    /// A node on a parent cycle, if one exists.
    ///
    /// The walk is bounded by the node count rather than by a visited set: from each
    /// start it follows parents for at most `nodes.len() + 1` hops, so a hostile
    /// document can neither loop this function, nor make it allocate, nor make it cost
    /// more than a quadratic number of pointer chases. Reporting the first start that
    /// reaches itself again is enough, because every node on a cycle returns to itself
    /// within the cycle's own length, and a cycle therefore always has a start in this
    /// map that detects it.
    fn find_cycle(&self) -> Option<NodeId> {
        let limit = self.nodes.len();
        for start in self.nodes.keys() {
            let mut current = *start;
            for _ in 0..=limit {
                let Some(parent) = self.nodes.get(&current).and_then(|node| node.parent) else {
                    break;
                };
                if parent == *start {
                    return Some(*start);
                }
                current = parent;
            }
        }
        None
    }

    fn check_name(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(ProjectError::Validation {
                location: "scene name".to_string(),
                detail: "must not be empty; a scene with no name cannot be reported on".to_string(),
            });
        }
        Ok(())
    }

    /// Put one key on a track, returning the key it displaced.
    ///
    /// The scene performs this rather than handing out `&mut TrackData`, because a
    /// curve's sorted-and-unique invariant and the scene's one-track-per-property rule
    /// are the same rule seen from two levels; an accessor that let a caller write
    /// through to the curve would leave `validate` as a polite suggestion.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if no track has `track_id`.
    pub fn set_track_key(
        &mut self,
        track_id: &TrackId,
        key: PropertyKey,
    ) -> Result<Option<PropertyKey>> {
        self.try_change(|scene| {
            let track = scene
                .tracks
                .get_mut(track_id)
                .ok_or_else(|| ProjectError::Validation {
                    location: format!("track {track_id}"),
                    detail: "does not exist in this scene".to_string(),
                })?;
            Ok(track.curve.set(key))
        })
    }

    /// Remove the key at one position from a track, returning it.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if no track has `track_id`.
    pub fn remove_track_key(
        &mut self,
        track_id: &TrackId,
        position: FramePos,
    ) -> Result<Option<PropertyKey>> {
        self.try_change(|scene| {
            let track = scene
                .tracks
                .get_mut(track_id)
                .ok_or_else(|| ProjectError::Validation {
                    location: format!("track {track_id}"),
                    detail: "does not exist in this scene".to_string(),
                })?;
            Ok(track.curve.remove(position))
        })
    }

    /// Apply one structural change to a clone, validate the result, and commit only if
    /// it is legal. This is spec 12.6's clone-and-swap rule at the scale of a scene, so
    /// that a rejected command leaves this value exactly as it was.
    fn change<T>(&mut self, change: impl FnOnce(&mut Self) -> T) -> Result<T> {
        let mut candidate = self.clone();
        let outcome = change(&mut candidate);
        candidate.validate()?;
        *self = candidate;
        Ok(outcome)
    }

    /// [`SceneData::change`] for a change that can itself refuse.
    ///
    /// Two functions rather than one with an error-joining step because the difference
    /// is observable: a closure that returns `T` cannot say "the object you named is
    /// not here", and quietly committing nothing while reporting success is the silent
    /// skip spec 13.11 forbids.
    fn try_change<T>(&mut self, change: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let mut candidate = self.clone();
        let outcome = change(&mut candidate)?;
        candidate.validate()?;
        *self = candidate;
        Ok(outcome)
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.text(&self.name);
        cp.u64(self.nodes.len() as u64);
        for node in self.nodes.values() {
            node.encode(cp);
        }
        cp.u64(self.tracks.len() as u64);
        for track in self.tracks.values() {
            track.encode(cp);
        }
        cp.u64(self.markers.len() as u64);
        for marker in &self.markers {
            marker.encode(cp);
        }
    }

    /// The record of this scene in ADR-0004's canonical byte layout, with no domain
    /// tag, so a command payload can hash these exact bytes.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp);
        cp.into_vec()
    }
}

impl Serialize for SceneData {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SceneData", 4)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("nodes", &self.nodes)?;
        state.serialize_field("tracks", &self.tracks)?;
        state.serialize_field("markers", &self.markers)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SceneData {
    /// Hand-written so a saved scene passes `validate` on the way in: the collections
    /// are private precisely because they hold invariants, and a derived impl would fill
    /// them without asking.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawScene {
            name: String,
            nodes: BTreeMap<NodeId, NodeData>,
            tracks: BTreeMap<TrackId, TrackData>,
            markers: Vec<Marker>,
        }
        let raw = RawScene::deserialize(deserializer)?;
        Self::from_parts(
            raw.name,
            raw.nodes.into_values(),
            raw.tracks.into_values(),
            raw.markers,
        )
        .map_err(serde::de::Error::custom)
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
    use crate::property::PropertyKey;
    use anim_core::rational::Rational;
    use uuid::Uuid;

    fn node_id(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    fn track_id(label: &str) -> TrackId {
        TrackId::derive(&namespace(), label.as_bytes())
    }

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn node(label: &str, parent: Option<&str>, order: u32) -> NodeData {
        NodeData::new(
            node_id(label),
            parent.map(node_id),
            order,
            label.to_string(),
        )
    }

    #[test]
    fn an_empty_scene_name_is_refused() {
        assert!(matches!(
            SceneData::new(""),
            Err(ProjectError::Validation { .. })
        ));
    }

    #[test]
    fn roots_follow_node_id_order_and_children_follow_persisted_order() {
        let mut scene = SceneData::new("shot01").unwrap();
        let root = node("root", None, 0);
        let root_id = root.node_id;
        scene.insert_node(root).unwrap();
        for (label, order) in [("cel-a", 5u32), ("cel-b", 1), ("cel-c", 9)] {
            scene.insert_node(node(label, Some("root"), order)).unwrap();
        }
        let ordered: Vec<String> = scene
            .children_of(&root_id)
            .into_iter()
            .map(|n| n.display_name.clone())
            .collect();
        assert_eq!(ordered, vec!["cel-b", "cel-a", "cel-c"]);

        let root_ids: Vec<NodeId> = scene.roots().map(|n| n.node_id).collect();
        let mut sorted = root_ids.clone();
        sorted.sort();
        assert_eq!(root_ids, sorted, "root iteration order is NodeId order");
    }

    #[test]
    fn a_parent_outside_the_scene_is_refused_and_changes_nothing() {
        let mut scene = SceneData::new("shot01").unwrap();
        let before = scene.clone();
        let orphan = NodeData::new(node_id("orphan"), Some(node_id("absent")), 0, "orphan");
        assert!(scene.insert_node(orphan).is_err());
        assert_eq!(scene, before, "a rejected insert must not touch the scene");
    }

    #[test]
    fn two_children_cannot_claim_one_sibling_order() {
        let mut scene = SceneData::new("shot01").unwrap();
        let root = node("root", None, 0);
        let root_id = root.node_id;
        scene.insert_node(root).unwrap();
        scene.insert_node(node("a", Some("root"), 3)).unwrap();
        assert!(scene.insert_node(node("b", Some("root"), 3)).is_err());
        assert_eq!(scene.children_of(&root_id).len(), 1);
    }

    #[test]
    fn a_loaded_scene_may_arrive_before_its_parents() {
        // The reason `from_parts` exists: valid authored data, inserted in NodeId
        // order, can name a parent that has not arrived yet. A mutator would refuse it.
        let parent = node("parent", None, 0);
        let parent_id = parent.node_id;
        let child = node("child", Some("parent"), 0);
        let built = SceneData::from_parts("shot01", [child, parent], [], []);
        let scene = built.expect("a legal graph loads regardless of arrival order");
        assert_eq!(scene.children_of(&parent_id).len(), 1);
    }

    #[test]
    fn a_parent_cycle_is_found_rather_than_walked_into() {
        let a = node_id("a");
        let b = node_id("b");
        let nodes = [
            NodeData::new(a, Some(b), 0, "a"),
            NodeData::new(b, Some(a), 1, "b"),
        ];
        let rejected = SceneData::from_parts("shot01", nodes, [], []);
        assert!(
            matches!(rejected, Err(ProjectError::Validation { .. })),
            "a two-node cycle must be refused at load, not livelock the visitor"
        );
    }

    #[test]
    fn a_node_with_children_or_a_track_cannot_be_removed() {
        let mut scene = SceneData::new("shot01").unwrap();
        let root = node("root", None, 0);
        let root_id = root.node_id;
        scene.insert_node(root).unwrap();
        scene.insert_node(node("child", Some("root"), 0)).unwrap();
        assert!(scene.remove_node(&root_id).is_err());

        let leaf = node("leaf", Some("root"), 1);
        let leaf_id = leaf.node_id;
        scene.insert_node(leaf).unwrap();
        scene
            .insert_track(TrackData::new(
                track_id("t"),
                leaf_id,
                PropertyPath::new("opacity").unwrap(),
                PropertyCurve::with_default(Some(PropertyValue::Rational(Rational::from_i128(1)))),
            ))
            .unwrap();
        assert!(scene.remove_node(&leaf_id).is_err());
        assert_eq!(
            scene
                .remove_track(&track_id("t"))
                .unwrap()
                .map(|t| t.track_id),
            Some(track_id("t"))
        );
        assert!(scene.remove_node(&leaf_id).unwrap().is_some());
    }

    #[test]
    fn one_property_holds_one_key_list() {
        let mut scene = SceneData::new("shot01").unwrap();
        let root = node("root", None, 0);
        let root_id = root.node_id;
        scene.insert_node(root).unwrap();
        let path = PropertyPath::new("opacity").unwrap();
        scene
            .insert_track(TrackData::new(
                track_id("first"),
                root_id,
                path.clone(),
                PropertyCurve::with_default(None),
            ))
            .unwrap();
        let second = TrackData::new(
            track_id("second"),
            root_id,
            path,
            PropertyCurve::with_default(None),
        );
        assert!(matches!(
            scene.insert_track(second),
            Err(ProjectError::Validation { .. })
        ));
        assert_eq!(scene.tracks().count(), 1);
    }

    #[test]
    fn markers_stay_sorted_and_a_repeated_pair_is_not_added_twice() {
        let mut scene = SceneData::new("shot01").unwrap();
        scene
            .insert_marker(Marker::new(FramePos::at_frame(9), "hit"))
            .unwrap();
        scene
            .insert_marker(Marker::new(FramePos::at_frame(2), "start"))
            .unwrap();
        let added_again = scene
            .insert_marker(Marker::new(FramePos::at_frame(9), "hit"))
            .unwrap();
        assert!(!added_again);
        assert_eq!(scene.markers().len(), 2);
        assert_eq!(scene.markers()[0].label, "start");
        assert!(
            scene
                .remove_marker(&Marker::new(FramePos::at_frame(2), "start"))
                .unwrap(),
            "removing a marker that is there reports that it was"
        );
        assert_eq!(scene.markers().len(), 1);
    }

    #[test]
    fn a_scene_reproduces_its_own_bytes_after_a_json_round_trip() {
        let mut scene = SceneData::new("shot01").unwrap();
        let root = node("root", None, 0);
        let root_id = root.node_id;
        scene.insert_node(root).unwrap();
        scene.insert_node(node("child", Some("root"), 0)).unwrap();
        scene
            .insert_track(TrackData::new(
                track_id("t"),
                root_id,
                PropertyPath::new("transform.x").unwrap(),
                PropertyCurve::new(
                    [PropertyKey::new(
                        FramePos::at_frame(4),
                        PropertyValue::Rational(Rational::new(1, 3).unwrap()),
                        crate::property::Easing::Step,
                    )],
                    None,
                )
                .unwrap(),
            ))
            .unwrap();
        scene
            .insert_marker(Marker::new(FramePos::at_frame(0), "in"))
            .unwrap();

        let bytes = serde_json::to_vec(&scene).unwrap();
        let loaded: SceneData = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded, scene);
        assert_eq!(serde_json::to_vec(&loaded).unwrap(), bytes);
        assert_eq!(loaded.canonical_bytes(), scene.canonical_bytes());
    }

    #[test]
    fn a_scene_written_with_two_markers_at_one_place_refuses_to_load() {
        let json = r#"{"name":"shot01","nodes":{},"tracks":{},"markers":[
            {"position":{"frame":3,"subframe":"0/1"},"label":"hit"},
            {"position":{"frame":3,"subframe":"0/1"},"label":"hit"}
        ]}"#;
        assert!(serde_json::from_str::<SceneData>(json).is_err());
    }

    #[test]
    fn canonical_bytes_do_not_depend_on_insertion_order() {
        let mut a = SceneData::new("shot01").unwrap();
        a.insert_node(node("x", None, 0)).unwrap();
        a.insert_node(node("y", None, 1)).unwrap();

        let mut b = SceneData::new("shot01").unwrap();
        b.insert_node(node("y", None, 1)).unwrap();
        b.insert_node(node("x", None, 0)).unwrap();

        assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    }
}
