# ADR-0007 — Event phases, occurrence identity and tie ordering

Status: **closed.** Accepted 2026-11-06 as WP04's deliverable. The four questions this ADR
answers were put to the user before `evaluator.rs` was written, as one bundled decision, and
the answers are Decisions 1, 2, 4 and 5 below; Decision 3 is the derivation detail those
answers obliged. `BUILD_STATE.md`'s completed-work row for WP04 points here, and
DECISIONS.md #33 carries the same rulings in their short form.

Date: 2026-11-06
Work package: WP04 (spec register: "ADR-0007 · Event phases, occurrence identity and tie
ordering · WP04")
Evidence: `crates/anim_core/src/id.rs` (`EventOccurrenceId`, `typed_id!`),
`crates/anim_project/src/event.rs`, `crates/anim_project/src/scene.rs` (the `events`
collection and its validation), `crates/anim_project/src/migrate.rs` (`V1_TO_V2`),
`crates/anim_scene/src/events.rs`, `crates/anim_scene/src/evaluator.rs`,
`crates/anim_scene/tests/evaluation.rs`, `crates/anim_test_support/src/bin/wp04_snapshot.rs`,
`crates/anim_test_support/tests/wp04_isolation.rs`,
`crates/anim_project/tests/fixtures/project_v1.anim.json`, `schemas/project/v2.json`.
Spec 10.8, 13.5, 13.6, 13.7, 13.8, 13.9, 13.11, 13.12. Depends on ADR-0004 (identity and
canonical pre-images) and continues ADR-0005 (the field-addition-versus-layout split).

## Problem

Spec 13.7 gives event ordering in one sentence:

> For each source event occurrence in the requested range, derive `EventOccurrenceId` from
> `(EventDefinitionId, occurrence_index)`. Sort by `(event_time, priority, source_node_id,
> local_sequence, occurrence_id)`. UUID comparison uses its 16 bytes. `local_sequence` is
> persisted or derived from a stable source key, never hash-map or filesystem enumeration.

Six nouns in it are undefined anywhere in the document, and each one is a thing that gets
written into saved files and into snapshot digests, so a wrong reading is not recoverable
after G3:

1. **What an occurrence is.** An `EventDefinitionId` names one authored thing. If one
   definition *is* one occurrence, then `occurrence_index` is always zero and
   `EventOccurrenceId` is a pointless second identifier. If a definition can fire more than
   once, something has to say how many times and when, and spec 13 authors no repetition
   field.
2. **What `EventDefinitionId` refers to.** Spec 10.8's id list contains
   `EventDefinitionId`, and spec 12.3's file list contains no event type, no event
   collection and no way to author one. WP03 built the raw scene small and left
   `Marker{position, label}` as the only timeline annotation. So the identifier exists and
   the thing it identifies does not.
3. **Where `EventOccurrenceId` is defined.** It is not in spec 10.8's list of seven id
   types. It appears in 13.7 and in 13.8's `SceneEvent` struct. The only implementation of
   the rules that make an id safe to depend on — the strict canonical parse, the `Ord` over
   the 16 bytes rather than over `uuid`'s internal representation, the shared
   `Serialize`/`Deserialize` — is a `macro_rules!` that is private to
   `crates/anim_core/src/id.rs`.
4. **What "phases" means in this ADR's own title.** Spec 24 asks for a decision about event
   *phases*. 13.6 describes one: a frame snapshot owns `[time_of(n), time_of(n+1))` and an
   event exactly at the upper boundary belongs to the next frame; arbitrary-position
   evaluation "does not independently redefine event delivery"; simulation uses the
   canonical event sequence. Spec 14.x then schedules those events onto ticks. Nothing states
   which layer owns which of those three moments.
5. **How the ordering is made total**, and therefore what makes 13.12's "occurrence ordering
   under shuffled source insertion" a testable claim rather than a hope.
6. **Which of 13.8's five `SceneEvent` fields are authored and which are derived**, since
   `source_id` is a plain `NodeId` there while a natural authoring model would make an
   event's source optional.

## Alternatives considered

**On the authored event data (Problem 1, 2, 6).**

- *Promote `Marker` to the event carrier*: add `id`, `priority`, `local_sequence` and a kind
  to the existing marker type. One collection instead of two, and no new field in
  `SceneData`. Rejected because a marker's identity is its `(position, label)` pair and its
  `Ord` is that pair; giving it an id changes both, and it fuses "an annotation a person
  reads on a timing sheet" to "a side effect `anim_sim`, `anim_audio` and `anim_render`
  consume" (13.10). Spec 10.8 lists no `MarkerId` either, so the id would be invented to
  make a rename work.
- *Synthesize occurrences from tracks* — derive events from `Step` keys, cut flags and
  visibility switches already authored on existing tracks, with the occurrence key coming
  from `(TrackId, key index)`. No schema change at all. Rejected: 13.7's ordering key then
  has no `priority` and no `local_sequence`, `EventDefinitionId` stays a type with no
  referent, and the packages that reference one as authored data — spec 28.8's
  `effect_cue: Option<EventDefinitionId>` and `audio_cue: Option<EventDefinitionId>` — have
  nothing to point at.
- *Author event definitions as raw project data* (accepted). `EventDefinition` in
  `anim_project`, `EventDefinitionId` from spec 10.8, and the `events` collection on
  `SceneData`. This is the reading under which 13.5 step 1's "validate … event definitions"
  describes an action rather than a fiction.

**On where `EventOccurrenceId` lives (Problem 3).**

- *In `anim_scene::events`*, keeping WP01's crate byte-for-byte frozen and matching spec
  10.8's list being exactly seven. Rejected on a fact rather than a preference: `typed_id!`
  is private to `anim_core/src/id.rs`, so this option requires either exporting the macro —
  which changes `anim_core` anyway, and changes its public API — or hand-writing a second
  implementation of the parse-and-order rules. The macro's own doc comment gives the reason
  it exists: "any drift between how `SceneId` and `NodeId` parse, order or serialize would
  be a silent asymmetry in the one thing every package trusts." A second implementation is
  that drift, in the tie-break term of the event ordering.
- *Not an id type at all* — store a bare `Uuid` or a `(EventDefinitionId, u64)` tuple in
  `SceneEvent`. Rejected: 13.8 types the field, 13.7 says it is *derived*, and the
  engine-wide rule that every referenced object class has its own type is what makes
  "an `AssetId` where a `NodeId` was expected" a compile error rather than a data bug.
- *In `anim_core::id`, with the other eight* (accepted), as one additive `typed_id!`
  invocation. No serialization version moves, because an occurrence id is never stored in a
  project file — it is computed from data that is.

**On the occurrence itself (Problem 1).**

- *`EventOccurrenceId` = the definition id.* Rejected: an event log entry that is identical
  to its definition cannot distinguish a second firing, so the day an emitter fires twice
  the identity of the first occurrence moves. That is the failure this ADR exists to
  prevent, because occurrence ids are in snapshot digests.
- *Author repetition now* — a `repeat` count or an interval on the definition. Rejected:
  spec 13 authors none, and the package that does schedule repetition by name is WP14's
  emitter scheduling (spec 29.5).
- *One authored position per definition, therefore `occurrence_index = 0` today, with the
  index inside the identity* (accepted). The derivation is written as
  `(definition, index)` so that WP14 extends the index rather than redefining the identity,
  and the first occurrence's id never changes when a second one appears.

**On the phases (Problem 4).**

- *Let each consumer decide frame membership.* Rejected outright: 13.6 forbids it in words.
- *One ownership rule, computed once, in exact time* (accepted). `EventDefinition::position`
  is owned by whole frame `n` when `time_of(n) <= time_of(position) < time_of(n+1)`, with
  both boundaries from `FrameRate::time_of` over rationals. `containing_frame` gives the
  frame an arbitrary position belongs to, and is the only place that mapping is written.
  The equivalence of the two is a consequence of DECISIONS.md #22's private normalized
  subframe, and it is asserted rather than assumed (`every_reported_position_is_the_
  normalized_spelling_of_its_instant`).
- *Camera-on determination from the current frame's events.* Rejected with a reason worth
  keeping: the cut that decides what frame 9 looks like is usually in frame 4. A per-frame
  read would answer "which camera is on" with whichever cut shared the frame — a second
  delivery rule, which is exactly what 13.6's sentence forbids. So
  `EventSources::all_occurrences` exists, and its agreement with the frame-by-frame walk is
  tested (`the_whole_scene_sequence_is_the_frame_by_frame_sequence`).

**On the sort (Problem 5).**

- *Sort by `(time, id)`.* Rejected: it discards two authored intentions, `priority` and
  `local_sequence`, which is what the operator wrote down to say which of two simultaneous
  cues came first.
- *Sort by the five terms, with `occurrence_id` last as the total tie-break* (accepted).
  Totality holds because the last term is an identifier, identifiers are unique within the
  scene's id-keyed map, and `anim_core::id`'s `Ord` is the 16 bytes rather than `uuid`'s
  internal representation, which is caret-pinned and therefore not this project's to depend
  on. The comparator is implemented as `Ord for SceneEvent` and not only as a sort key, so a
  consumer that puts events into a set or map gets the same order the extraction gives.

## Decision

1. **Events are authored project data.** `EventDefinition { id: EventDefinitionId, position:
   FramePos, priority: i32, source: NodeId, kind: EventKind, local_sequence: u32 }` lives in
   `crates/anim_project/src/event.rs` and is held by `SceneData` as a
   `BTreeMap<EventDefinitionId, EventDefinition>`. `Marker` is unchanged and remains an
   annotation with no identity and no consequences. `source` is required, because 13.8 types
   `SceneEvent.source_id` as a plain `NodeId`, and `SceneData::validate` refuses a source the
   scene does not have — the same rule tracks already obey, for the same reason.
2. **`EventOccurrenceId` is the eighth typed id in `anim_core::id`**, defined by the same
   macro as the other seven. Spec 10.8's list stops at seven; this is recorded as a
   deviation. Its `kind` label is `"event_occurrence"` and a test asserts that all eight
   labels are distinct, because `kind` is what a diagnostic says when it reports which
   identity it found.
3. **The occurrence identity is `derive(NAMESPACE, domain ‖ definition bytes ‖ index u64)`.**
   The domain is `anim_engine/event_occurrence/v1`, length-prefixed, first — ADR-0004
   consequence 5's registry, which this adds one entry to. The namespace is the RFC 4122 DNS
   namespace every other derived id in the workspace uses; distinctness comes from the domain
   inside the name bytes, not from a new namespace, because a second namespace would be a
   second convention nobody can check. `derive` is deterministic by construction: there is no
   entropy source in this workspace at all (DECISIONS.md #23).
4. **Frame ownership is decided in exact time, once.** `[time_of(n), time_of(n+1))` with
   rational boundaries; `containing_frame(FramePos) = position.frame` is the inverse mapping
   and is sound only because the subframe is private and normalized; camera-on is decided by
   the latest cut in the whole ordered sequence at or before the position.
5. **Ordering is 13.7's five terms, implemented as `Ord`.** `(event_time, priority,
   source_node_id, local_sequence, occurrence_id)`, `local_sequence` persisted rather than
   derived, and no sorting step anywhere that depends on the order authored data arrived in.
6. **The snapshot's identity includes the easing algorithm version and the document's schema
   version does not include it.** `snapshot_hash`'s pre-image leads with
   `EASING_ALGORITHM_VERSION`; `SceneData`'s new `events` section moved
   `PROJECT_SCHEMA_VERSION` from 1 to 2 and left `DOCUMENT_DOMAIN` at
   `anim_engine/document/v1`. ADR-0005 Decision 4's split, applied to a new object: a field
   addition is a data event, a change to how a curve is *read* is nothing in the document at
   all and everything in the snapshot, and two snapshots that differ must not share a digest.

## Consequences

1. `PROJECT_SCHEMA_VERSION` is 2, `migrate::chain()` has its first real step (`V1_TO_V2`,
   which adds an empty `events` map to every scene), and
   `crates/anim_project/tests/fixtures/project_v1.anim.json` is the committed version 1
   file. Spec 12.5 step 9's "fixtures for every schema version" stopped being hypothetical.
   A version 1 file is now refused rather than silently read, because the Rust `Deserialize`
   has no `serde(default)` on `events` — that strictness is the migration's teeth.
2. Every snapshot digest changed, including empty-scene ones, because the encoding gained a
   section. No golden exists yet (WP06 is the first), so nothing was invalidated; after WP06
   the same move requires re-certification, which is why it is written down here.
3. `anim_scene` depends on `anim_project` and on nothing else except `anim_core` and `uuid`,
   and does not depend on `wgpu`, `egui`, `cpal`, FFmpeg or `serde` — 13.4's prohibition,
   kept in the manifest comment so a later session does not "tidy" it away.
4. Snapshots have no mutation route at all: built from a `&ProjectDocument`, no `&mut`
   anywhere in the evaluation API, no setter, no interior mutability. 13.2's rule is a
   property of the types.
5. The snapshot cache is owned and takes `&mut self`. `clippy.toml` denies `Mutex` and
   `RwLock` workspace-wide, so a concurrent cache is a design question for WP11's
   benchmarking rather than something WP04 could quietly add.
6. `SceneError::EventOrderOverflow` is declared because 13.11 lists it, and **no test reaches
   it**: with one authored position per definition, the only arithmetic in the ordering path
   is a constant index and a literal domain tag. It is recorded in `BUILD_STATE.md` as an
   open item, and it becomes reachable with WP14's emitter scheduling. The other eight
   variants each have a test that produces them.
7. Spec 13.5 step 3's layer blending is **not implemented**, by decision rather than by
   omission, and G3 does not depend on it: 13.12 lists no layer criterion. The seam is
   `anim_scene::animation::blend_layers`, called by the evaluator with the one layer that
   exists. A scene-animation-layer type was not authored here because the spec has none —
   WP15's `MotionLayer` (30.8) is camera motion and WP12's pose layers (27.5) have no struct
   — and inventing one would have moved `PROJECT_SCHEMA_VERSION` again for a shape WP12 or
   WP15 would replace.
8. No command authors an event yet. 12.9's command set is WP03's and 13.x adds none, so the
   raw `events` collection is reachable today only through `SceneData`'s mutators, which
   tests use. WP10's session, which owns the editor, owes `PlaceEvent`-shaped commands; the
   closed-under-inversion test `every_command_kind_has_an_inverse_that_closes` is the one to
   extend when they arrive.
9. Camera and audio-cue *authoring* is minimal by the same argument. `EvaluatedCamera` exists
   because 13.8 lists the array and 13.12 tests cuts, and it is derived from `CameraCut`
   events; the rig — `CameraPose`, `CameraTrack`, `ShotSpec`, `PathId`, `CameraId`,
   `MotionLayerId` — is WP15's (30.8) and none of its ids are in spec 10.8's list.
   `AudioCue` carries the cue's occurrence identity, node, exact position and the asset its
   source node is showing, which is everything 13.10's sample-position conversion needs from
   this layer.
10. Node property paths are this package's convention (`transform.x`, `transform.y`,
    `transform.rotation` in **degrees**, `transform.scale`, `z`, `opacity`, `visible`,
    `asset`) because 13.8's `EvaluatedNode` fields have to come from somewhere and no spec
    section names a single path. The names continue what WP03's own schema fixtures already
    author. A later package that adds an `EvaluatedNode` field adds its path in the same
    commit.

## Schema and version impact

| Number | Before | After | Why |
| --- | --- | --- | --- |
| `PROJECT_SCHEMA_VERSION` | 1 | 2 | `SceneData` gained the `events` section |
| `MANIFEST_SCHEMA_VERSION` | 1 | 1 unchanged | WP04 adds no asset field |
| `DOCUMENT_DOMAIN` | `anim_engine/document/v1` | unchanged | framing rules unchanged; a field addition is a data event (ADR-0005 Decision 4) |
| `EASING_ALGORITHM_VERSION` | (none) | 1 | new: 13.7's "versioned numeric algorithm", hashed into snapshots |
| event domain | (none) | `anim_engine/event_occurrence/v1` | new entry in ADR-0004 consequence 5's registry |
| snapshot domain | (none) | `anim_engine/snapshot/v1` | new entry in the same registry, used by `snapshot_hash` |
| `schemas/project/` | `v1.json` | `v1.json` retained, `v2.json` added | v1 still describes the files that exist on disk at that version |
| `schemas/commands/v1.json` | 1 | unchanged | command payloads describe embedded project objects loosely; no envelope field changed |

Nothing persisted an `EventOccurrenceId`, so no saved file holds a value this ADR changes.
Everything derived can be re-derived, which is the point of Decision 3.
