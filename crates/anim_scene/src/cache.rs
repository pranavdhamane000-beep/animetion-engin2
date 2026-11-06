//! The evaluated snapshot cache (spec 13.3, 13.5 step 9, 13.8).
//!
//! 13.8's last paragraph is the whole contract: "Cache entries are `Arc<SceneSnapshot>`
//! keyed by `(SceneId, Revision, FramePos)`. Existing snapshots remain valid for their
//! original revision after undo or new edits but cannot be presented as current." That
//! second sentence is why no invalidation protocol exists in this file: a snapshot carries
//! the revision it was evaluated from, so an old entry is not stale data, it is a correct
//! answer to a question nobody is asking any more. Dropping it would be a performance
//! question, and 13.5 step 9 asks only for "revision/position-keyed caching and isolation
//! tests".
//!
//! Two design points that are decisions rather than defaults.
//!
//! **The key is a three-tuple and one of its parts is what makes it unique.**
//! `FramePos`'s subframe is private and normalized precisely so that `FramePos{5, 3/2}`
//! and `FramePos{6, 1/2}` are one instant and one key (DECISIONS.md #22, spec 7.2). A cache
//! keyed on an unnormalized pair would answer a re-asked question with a fresh evaluation
//! and would report two digests for one position.
//!
//! **There is no lock.** `clippy.toml` denies `std::sync::Mutex` and `RwLock` workspace-wide
//! because the order of concurrent access must never reach exported bytes (spec 6.3), and
//! this crate is on the deterministic path. The cache is therefore owned, and its
//! get-or-evaluate takes `&mut self`. A caller that wants concurrent evaluation owns the
//! synchronization question — spec 6.3 assigns shared mutable state to `anim_sim`'s owned
//! step state, and WP11's benchmarking is where a partitioned design would be measured
//! enough to justify arguing about it.

use std::collections::BTreeMap;
use std::sync::Arc;

use anim_core::id::{Revision, SceneId};
use anim_core::time::FramePos;
use anim_project::ProjectDocument;

use crate::evaluator::SceneEvaluator;
use crate::snapshot::SceneSnapshot;
use crate::Result;

/// One cache key: the triple spec 13.8 names, in the order it names them.
pub type CacheKey = (SceneId, Revision, FramePos);

/// A `SceneEvaluator` with remembered results (spec 13.3, 13.5 step 9).
#[derive(Default, Debug)]
pub struct SnapshotCache {
    entries: BTreeMap<CacheKey, Arc<SceneSnapshot>>,
    /// Counted rather than inferred from `entries`, because the two claims a report needs
    /// are "how much is remembered" and "how often remembering was enough", and a map
    /// length can answer only the first.
    hits: u64,
    evaluations: u64,
}

impl SnapshotCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The snapshot at one position, evaluated once and remembered under its revision.
    ///
    /// # Errors
    ///
    /// Whatever [`SceneEvaluator::evaluate`] raises; a failed evaluation caches nothing.
    pub fn snapshot(
        &mut self,
        document: &ProjectDocument,
        scene_id: SceneId,
        position: FramePos,
    ) -> Result<Arc<SceneSnapshot>> {
        let key = (scene_id, document.revision, position);
        if let Some(found) = self.entries.get(&key) {
            self.hits += 1;
            return Ok(Arc::clone(found));
        }
        let snapshot = SceneEvaluator::evaluate(document, scene_id, position)?;
        self.evaluations += 1;
        self.entries.insert(key, Arc::clone(&snapshot));
        Ok(snapshot)
    }

    /// A remembered snapshot, without evaluating.
    ///
    /// The isolation test's instrument: it is what proves an entry survived an edit at all,
    /// which `snapshot` cannot show because a miss there is indistinguishable from an
    /// eviction in a report.
    #[must_use]
    pub fn peek(
        &self,
        scene_id: SceneId,
        revision: Revision,
        position: FramePos,
    ) -> Option<Arc<SceneSnapshot>> {
        self.entries.get(&(scene_id, revision, position)).cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many asks were answered from memory.
    #[must_use]
    pub const fn hits(&self) -> u64 {
        self.hits
    }

    /// How many asks cost an evaluation.
    #[must_use]
    pub const fn evaluations(&self) -> u64 {
        self.evaluations
    }

    /// Forget everything.
    ///
    /// Correctness never depends on this: 13.8's rule is that an entry stays *valid* for
    /// the revision it names, so clearing is a memory decision and not a safety one. It is
    /// here because WP11's benchmarks need a cold cache to be obtainable without
    /// rebuilding the session around it.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the workspace denial governs production code"
    )]

    use super::*;
    use anim_core::id::{EventDefinitionId, NodeId, ProjectId};
    use anim_core::rational::Rational;
    use anim_core::time::FrameRate;
    use anim_project::{
        EventDefinition, EventKind, NodeData, PropertyCurve, PropertyKey, PropertyPath,
        PropertyValue, SceneData, TrackData,
    };
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn scene_id() -> SceneId {
        SceneId::derive(&namespace(), b"shot01")
    }

    fn node_id(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    fn document() -> ProjectDocument {
        let mut out = ProjectDocument::new(
            ProjectId::derive(&namespace(), b"p"),
            FrameRate::new(24, 1).unwrap(),
        );
        assert!(
            out.insert_scene(scene_id(), scene()).is_none(),
            "a fresh document displaces nothing"
        );
        out
    }

    fn scene() -> SceneData {
        let mut built = SceneData::new("shot01").unwrap();
        built
            .insert_node(NodeData::new(node_id("hero"), None, 0, "hero"))
            .unwrap();
        built
            .insert_track(TrackData::new(
                anim_core::id::TrackId::derive(&namespace(), b"x"),
                node_id("hero"),
                PropertyPath::new("transform.x").unwrap(),
                PropertyCurve::new(
                    [
                        PropertyKey::new(
                            FramePos::at_frame(0),
                            PropertyValue::Rational(Rational::from_i128(0)),
                            anim_project::Easing::Linear,
                        ),
                        PropertyKey::new(
                            FramePos::at_frame(10),
                            PropertyValue::Rational(Rational::from_i128(100)),
                            anim_project::Easing::Linear,
                        ),
                    ],
                    None,
                )
                .unwrap(),
            ))
            .unwrap();
        built
    }

    #[test]
    fn one_position_is_evaluated_once_and_asked_twice() {
        let mut cache = SnapshotCache::new();
        let doc = document();
        let first = cache
            .snapshot(&doc, scene_id(), FramePos::at_frame(4))
            .unwrap();
        let second = cache
            .snapshot(&doc, scene_id(), FramePos::at_frame(4))
            .unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "the same allocation, not a copy"
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.evaluations(), 1);
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn two_spellings_of_one_instant_are_one_key() {
        // DECISIONS.md #22's rule, arriving at the cache: `5 + 3/2` and `6 + 1/2` are the
        // same `FramePos`, so a cache that remembered both would be proving the type's
        // normalization is not actually load-bearing.
        let mut cache = SnapshotCache::new();
        let doc = document();
        let a = FramePos::normalized(5, Rational::new(3, 2).unwrap()).unwrap();
        let b = FramePos::normalized(6, Rational::new(1, 2).unwrap()).unwrap();
        assert_eq!(a, b, "and the key they hash to is one key");
        cache.snapshot(&doc, scene_id(), a).unwrap();
        cache.snapshot(&doc, scene_id(), b).unwrap();
        assert_eq!(
            cache.len(),
            1,
            "one position, one entry, whatever the caller's spelling was"
        );
        assert_eq!(cache.evaluations(), 1);
    }

    #[test]
    fn an_edit_makes_a_new_entry_rather_than_rewriting_the_old_one() {
        // 13.12's "cache isolation after undo/new revisions", in the direction that a
        // map-with-revision-in-its-key makes easy: two revisions of one scene at one
        // position are two entries, and neither is the other.
        let mut cache = SnapshotCache::new();
        let before = document();
        let old = cache
            .snapshot(&before, scene_id(), FramePos::at_frame(5))
            .unwrap();
        assert_eq!(old.revision, before.revision);

        let mut after = document();
        after.revision = Revision(9);
        let new = cache
            .snapshot(&after, scene_id(), FramePos::at_frame(5))
            .unwrap();
        assert_eq!(new.revision, Revision(9));
        assert_ne!(old.revision, new.revision);
        assert_eq!(cache.len(), 2, "one position, two revisions, two entries");
        assert!(
            cache
                .peek(scene_id(), before.revision, FramePos::at_frame(5))
                .is_some(),
            "the old snapshot is still valid for the revision it names"
        );
    }

    #[test]
    fn an_undo_returns_to_the_revision_whose_entry_is_already_there() {
        // The isolation claim from the other side, and the reason an undo is not an
        // invalidation event: after editing forward and back, the answer at the original
        // revision is the same object, still correct, and never re-derived.
        let mut cache = SnapshotCache::new();
        let original = document();
        let first = cache
            .snapshot(&original, scene_id(), FramePos::at_frame(3))
            .unwrap();
        let mut forward = original.clone();
        forward.revision = Revision(1);
        cache
            .snapshot(&forward, scene_id(), FramePos::at_frame(3))
            .unwrap();
        let back = cache
            .snapshot(&original, scene_id(), FramePos::at_frame(3))
            .unwrap();
        assert!(
            Arc::ptr_eq(&first, &back),
            "the same snapshot object is still the right answer for revision 0"
        );
        assert_eq!(
            cache.evaluations(),
            2,
            "and it was not evaluated a third time"
        );
    }

    #[test]
    fn a_different_scene_never_shares_an_entry_with_another() {
        let mut doc = document();
        let other = SceneId::derive(&namespace(), b"shot02");
        assert!(doc.insert_scene(other, scene()).is_none());
        let mut cache = SnapshotCache::new();
        let at = FramePos::at_frame(1);
        cache.snapshot(&doc, scene_id(), at).unwrap();
        cache.snapshot(&doc, other, at).unwrap();
        assert_eq!(
            cache.len(),
            2,
            "one position in two scenes is two questions"
        );
    }

    #[test]
    fn clearing_is_a_memory_decision_and_not_a_correctness_one() {
        let mut cache = SnapshotCache::new();
        let doc = document();
        let before = cache
            .snapshot(&doc, scene_id(), FramePos::at_frame(7))
            .unwrap();
        cache.clear();
        assert!(cache.is_empty());
        let after = cache
            .snapshot(&doc, scene_id(), FramePos::at_frame(7))
            .unwrap();
        assert_eq!(
            *before, *after,
            "re-evaluation reproduces the same snapshot, which is what makes `clear` safe"
        );
        assert_eq!(cache.evaluations(), 2);
    }

    #[test]
    fn a_failed_evaluation_caches_nothing() {
        // Spec 13.11's "never emit a snapshot stamped with another revision" has a cache
        // consequence: a position whose evaluation refused must not leave a partial or
        // wrong-revision entry behind for the next caller to find.
        let mut doc = document();
        let absent = SceneId::derive(&namespace(), b"absent");
        let mut cache = SnapshotCache::new();
        assert!(cache.snapshot(&doc, absent, FramePos::at_frame(0)).is_err());
        assert!(cache.is_empty());
        assert_eq!(cache.evaluations(), 0);
        // And the same cache still works afterwards.
        doc.revision = Revision(3);
        cache
            .snapshot(&doc, scene_id(), FramePos::at_frame(0))
            .unwrap();
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn an_event_only_scene_is_cached_like_any_other() {
        let mut built = SceneData::new("shot01").unwrap();
        let hero = node_id("hero");
        built
            .insert_node(NodeData::new(hero, None, 0, "hero"))
            .unwrap();
        built
            .insert_event(EventDefinition::new(
                EventDefinitionId::derive(&namespace(), b"cue"),
                FramePos::at_frame(2),
                0,
                hero,
                EventKind::Signal,
                0,
            ))
            .unwrap();
        let mut doc = document();
        // A second scene id, not a replacement of the fixture's: `insert_scene` returns
        // what it displaced, and displacing the base scene here would make this test
        // about the document rather than about the cache.
        let shot = SceneId::derive(&namespace(), b"shot02");
        assert!(doc.insert_scene(shot, built).is_none());
        let mut cache = SnapshotCache::new();
        let at = FramePos::at_frame(2);
        let first = cache.snapshot(&doc, shot, at).unwrap();
        let second = cache.snapshot(&doc, shot, at).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.events.len(), 1);
    }
}
