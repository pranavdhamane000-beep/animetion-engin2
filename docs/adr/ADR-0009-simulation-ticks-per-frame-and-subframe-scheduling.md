# ADR-0009 — Simulation ticks per frame and subframe scheduling

Status: **accepted.** Written as WP05's deliverable on 2026-11-06. Decision 1 was put to the
user before any code in this package was written, as one bundled question with two parts, and
his answers are Decisions 1 and 2 below; the rest are what those two answers obliged.

Date: 2026-11-06
Work package: WP05 (spec register: "ADR-0009 · Simulation ticks per frame and subframe
scheduling · WP05")
Evidence: `crates/anim_project/src/simulation.rs`, `crates/anim_project/src/document.rs`,
`crates/anim_project/src/migrate.rs` (`V2_TO_V3`, and the frozen-history change to
`V1_TO_V2`), `crates/anim_project/tests/fixtures/project_v2.anim.json`,
`schemas/project/v3.json`, `tools/make_v3_schema.py`,
`crates/anim_sim/src/clock.rs`, `crates/anim_sim/src/events.rs`,
`crates/anim_sim/src/step.rs`, `crates/anim_sim/src/state.rs`,
`crates/anim_sim/src/particles.rs`, `crates/anim_sim/src/springs.rs`,
`crates/anim_sim/tests/determinism.rs`,
`crates/anim_test_support/src/wp05_fixture.rs`,
`crates/anim_test_support/tests/wp05_isolation.rs`.
Spec 10.6, 10.7, 12.8, 13.6, 13.7, 14.2, 14.6, 14.7, 14.8, 14.9, 14.11, 14.12. Continues
ADR-0004 (arithmetic limits), ADR-0005 (version-number separation), ADR-0007 (event
occurrences and the phase rule) and ADR-0008 (the streams this clock addresses).

## Problem

Spec 14.2 states the prerequisite in one clause — "`ProjectDocument.simulation.ticks_per_frame`
is a nonzero u32; simulation rate equals `frame_rate*ticks_per_frame`" — and 14.6, 14.7 and
14.8 then depend on that number for four different things at once. Five questions are open:

1. **The path 14.2 names does not exist.** Spec 12.8 declares six `ProjectDocument` fields and
   none of them is a simulation section. WP03 built the raw document to that list and WP04 had
   no reason to add one. So WP05 was handed a prerequisite that nothing authors, and it cannot
   be built without deciding where the value comes from.
2. **Where the random-stream seed lives.** 14.7 derives every `StreamKey` from a "project
   seed". DECISIONS.md #23 established that there is no entropy source anywhere in the
   dependency graph, so a seed generated at open would be unreproducible and a saved project
   could not replay its own random streams.
3. **What a tick is, and which tick a step is *at*.** 14.6 says `SimTick(0)` is the
   initialized state before any step, that whole frame `f` begins at tick `f*N`, and that
   `step_once` advances exactly one tick. 14.9's `step_once(state, input, mode)` takes an
   input frame "for one tick". Whether `input.tick` is the tick being left or the tick being
   arrived at is not stated, and the two readings disagree about which events fire on which
   step.
4. **What the subframe age means for a newborn.** 14.7 says "compute exact
   `age = time_of_tick(k) - t`; convert once to working precision and use the same force
   function to initialize its state. Negative age is an internal error." 14.6's stage order
   then integrates every particle, including the one that has just been emitted. Whether the
   age replaces that first `dt` or adds to it changes every position in the run.
5. **Where `solver_iterations` comes from.** 14.8 puts it in `SimState`, 14.6 calls it "a
   fixed number", 14.5 step 6 says "fixed solver counts", and 14.11 demands an error for an
   "invalid solver count". A value that must be invalidatable is normally authored data; a
   value described as fixed is normally a constant.

`Spring` — 14.8's `pub springs: Vec<Spring>`, which 14.7 requires be hashed "in stable ID
order" — has no field list anywhere in 5,400 lines of specification, in this section or in
any later one. WP14's secondary motion (spec 29) defines `SecondaryChainComponent`,
`ChainParams`, `ChainNode` and `ChainRuntime` and never mentions `Spring` or `SpringBody`.

## Decisions

1. **`ticks_per_frame` is authored project data, not a runtime value.** Taken with the user
   before coding, recommended option. `PROJECT_SCHEMA_VERSION` 2 → 3, one real step
   `migrate::V2_TO_V3`, a committed version-2 fixture, `schemas/project/v3.json`, and the
   section type `SimulationSettings` in `crates/anim_project/src/simulation.rs` beside
   `event.rs`.
   **Rejected:** a rate the caller supplies at process start. 14.12's own criteria settle it
   rather than taste — "changing checkpoint cadence or warm-up chunk size cannot change the
   exported state" and "compare sequential execution with seeks to every tick" are both
   statements that one saved revision replays to one state. A per-session rate makes that
   false by construction, and `scheduled_tick = ceil(t * simulation_rate)` (14.7) is derived
   from the same number, so an event's tick placement and every subframe age move with it too.
   Spec 40/WP18's line "the tick rate is project-configured, not a global 1200 Hz constant"
   is consistent with this and was not the reason for it.

2. **The project seed lives in the same section, persisted and authored.** Also taken with the
   user before coding, recommended option, and the reason is 10.8's no-entropy rule: a seed
   generated at open makes a saved project's random streams unreproducible from its own bytes,
   and 14.11 forbids substituting one silently. `INITIAL_PROJECT_SEED` — all zero — is what a
   document this build creates starts with. That is a **value**, visible in the file and
   changed by authoring, not 14.11's substitution: substitution would be choosing something
   else where the data named a seed, and a project that has never been saved names nothing.
   **Rejected:** a seed derived from the document digest. It would need no authoring, but any
   edit re-derives it and moves every random stream in the project, which breaks 14.12's
   per-stream stability and makes warm-up replay depend on unrelated content changes.

3. **The frame rate is not copied into the section.** `ProjectDocument.frame_rate` is the one
   rate and the section holds exactly the two fields 14.2 and 14.7 name. `anim_sim`'s resolved
   form — `SimSettings { clock, project_seed, solver_iterations }` — is built by
   `SimSettings::new(document.frame_rate, &document.simulation)`, which is the consumer that
   legitimately holds both.
   **Rejected:** a `frame_rate` member inside `simulation`, or passing `ProjectDocument` to
   `initial_state`. The first would give one timeline two rate bindings that can disagree,
   which is the same objection `crates/anim_project/src/event.rs` raises about a cue's asset;
   the second would duplicate a value that already exists for a reason the spec states.
   `stream_version` aside, this is the reason 14.9's parameter type is spelled `SimSettings`
   here and `SimulationSettings` in `anim_project`, and it is recorded as a deviation.

4. **`scheduled_tick = ceil(t · simulation_rate)`, with exact boundaries keeping their own
   tick** — 14.7 read literally. Ceiling is the whole difference between a spawn that runs
   before its authored instant and one that runs at or after it, and it is what lets Decision
   6's age be provably non-negative rather than clamped.

5. **The age is exact rational arithmetic, held in the schedule, and converted once.**
   `ScheduledEvent` carries `exact_time: Rational`, `tick: SimTick` and `age: Rational`. The
   conversion to the working type happens in `particles::spawn_for_event` and nowhere else.
   A float age would be a different number at NTSC than at 24 fps for the same authored event,
   and both are one event; `BUILD_STATE.md` records that a 1001× error in exactly this class of
   quantity was found in this repository's real code by hand-computing the expectation.

6. **A newborn integrates by its age *and* by the step's `dt`, in that order, and the two sum
   to the correct elapsed time.** The derivation, which is why 14.6's stage order is emit
   (stage 3) before integrate (stage 5):
   `age + dt = (time_of_tick(k) − t) + (time_of_tick(k+1) − time_of_tick(k)) = time_of_tick(k+1) − t`.
   That is precisely how long a particle born at `t` has lived when the clock reads `k+1`.
   **Rejected:** exempting the newborn from its first `dt` on the grounds that the age already
   moved it. Under that reading a particle born exactly on a tick boundary (`age = 0`) would
   not move at all during the step it was born in, and would lag the authored timeline by one
   tick for its whole life.
   `age_ticks` counts integration stages, so a newborn reads `age_ticks == 1` after the step
   that created it: the age is initialization, not a lifetime increment, which is 14.7's own
   wording ("use the same force function to **initialize its state**").
   A negative age is `SimError::UnknownEventTarget`, and the branch is a bug report rather
   than a data error: `tick_of_position` ceils and `time_of_tick` floors, so a negative
   residual means the two directions of one conversion disagree.

7. **`state.tick` is the tick the state has reached; `input.tick` is the tick being left.**
   `step_once` refuses a mismatch and leaves the state at `input.tick + 1`. 14.6's "advances
   exactly one tick" is thereby a property of the function's ending, and a caller that lost
   track gets an error instead of a silently shifted event boundary.

8. **`solver_iterations` is an engine constant, carried in the state and re-validated on
   restore** — `DEFAULT_SOLVER_ITERATIONS = 4`, bounded by `MAX_SOLVER_ITERATIONS = 1024`,
   refused at zero. 14.6's word is "a **fixed** number of solver iterations", and the same
   section says the tick rate is the thing that comes from exact configuration — so the two
   are deliberately different in kind. 14.11's `InvalidSolverCount` still has a producer,
   because the count lives in `SimState`, enters the digest, and is checked when a checkpoint
   is restored: an entry carrying a count this build does not use is a mismatch, not a value
   to adopt.
   **Rejected:** authoring it in `ProjectDocument.simulation`. That is the right move
   eventually, and it needs a schema bump, a migration step and a fixture; doing it now would
   invent a knob 14.6 calls fixed, before anything can be said about what it is for.
   `DEFAULT_SOLVER_ITERATIONS`'s doc comment is where that argument gets re-opened.

9. **`Spring` and its topology are WP05's, and the topology is derived from the parent links
   the snapshot already carries.** A body exists for every evaluated node that has a parent,
   and a spring for every parent-child edge, with `rest_length` measured from the two authored
   transforms at initialization. `EvaluatedNode.parent` is the only chain structure a snapshot
   contains — `SimulationInput` deliberately has no parent field — and a chain whose rest
   length is the distance it starts at is a chain at rest at tick zero, which is the only
   initial state two fresh processes can be expected to agree on.
   The consequence is stated rather than left to be discovered: **any authored node hierarchy
   is a spring chain in WP05.** It changes nothing the scene evaluator reports, because
   14.8's `SimulationFrame` is a separate extraction and WP06 composes the two. WP12's bones
   and WP14's authored `SecondaryChainComponent` will narrow which nodes get bodies when the
   component data arrives, and that is the expected pattern — the same one WP04 followed when
   it wrote the project's first easing model because 13.x needed one and no section defined it.

10. **The emitter is the event's source node, and a spawn is an `EventKind::Signal`.** No
    section of the spec authors an emitter for WP05. `Signal` is "a signal for a consumer to
    react to" and is the one kind with no other consumer — a cut is read by 13.5's camera
    evaluation, an audio cue by 13.10's `anim_audio`. Everything else the burst needs
    (direction, speed, lifetime, size, rotation) is drawn from the streams ADR-0008 addresses,
    over constants inside `SIMULATION_MODEL_VERSION`.

11. **Physics parameters are engine constants behind one version number, and the float set is
    closed.** `+ - * /` and `sqrt` only, no trigonometry, for ADR-0008 Decision 9's reason.

12. **`SimulationContext` and `SimulationInputFrame` get a twelfth file, `context.rs`.** 14.3
    lists eleven and 14.9 defines both types. The alternative — putting a driver for every
    other module inside one of them — would make `state.rs` or `replay.rs` mean two things.

13. **`seek` and `prepare_export` take `&mut SimulationContext` where 14.9 writes `&`.** The
    context owns a `SnapshotCache`, which WP04 made owned-and-mutable rather than locked
    because `clippy.toml` denies `Mutex` and `RwLock` workspace-wide. A shared reference cannot
    fill that cache, so the literal signature is unimplementable without reintroducing the
    interior mutability #22 rejects.

14. **`V1_TO_V2` now writes the version it produces (2), not this build's number.** WP04 wrote
    `PROJECT_SCHEMA_VERSION` there while version 2 was the end of the chain, so the two were
    indistinguishable. With three versions in the chain, a step that writes the current number
    leaves an intermediate document claiming a version it has not passed through, and a failure
    in the next step gets reported against a file that already looked current. `V0_TO_V1`'s
    own comment already stated the correct rule; this brings the production step in line with
    the test-only one.

## Consequences

1. A saved project now says how it steps. Two processes, two machines and two cache
   configurations cannot reach different states from one revision, and 14.12's fresh-process
   row is what re-tests that claim at 10,000 frames.
2. A `ticks_per_frame` edit is a schema-visible change. It moves every event's tick and every
   subframe age, which is why it is authored rather than configured per session, and why a
   migration step carries the old meaning forward.
3. Version 2 files on disk still open. `V2_TO_V3` fills `ticks_per_frame = 1` and an all-zero
   seed — the only values a file that recorded neither admits to — and rewrites nothing else;
   `the_v2_step_adds_the_section_and_rewrites_nothing_else` checks that against the committed
   fixture rather than against a fixture the test wrote.
4. A version 2 file is **not** readable as version 3 without the step. `simulation` has no
   `serde(default)`, so the strictness WP04 established for `events` holds for the new section
   too, and `a_version_2_file_is_not_readable_as_version_3_without_the_step` proves it from the
   typed parse as well as from the chain.
5. A migrated file is a valid, loadable, non-canonical second spelling of one state —
   `BUILD_STATE.md` risk 23, now on a third path. The first save after a migration rewrites it
   canonically, which is correct and should be logged rather than hidden.
6. Every project with a hierarchy has spring bodies and every project with a signal event has
   particles. A later package narrowing that set is a schema bump, and Decision 9 is the
   record of what it will be replacing.
7. A `DEFAULT_SOLVER_ITERATIONS` change moves every state digest in the project, which is the
   point of carrying the count in the state rather than re-reading it from a constant per step.
8. The age rule (Decision 6) means a burst is integrated twice on its first tick. A later
   reading that "corrects" this will make every particle lag its authored instant by a tick.
9. `tools/make_v3_schema.py` is versioned, where WP04's equivalent generator was not. The
   schema files are inputs (`.gitignore` says so), and a generator that is deleted after use
   leaves "generated from the previous version, not retyped" as a claim no one can repeat.

## Schema and version impact

| Number | Before | After | Why |
| --- | --- | --- | --- |
| `PROJECT_SCHEMA_VERSION` | 2 | 3 | `ProjectDocument` gained the `simulation` section |
| `migrate::chain()` | `[V1_TO_V2]` | `[V1_TO_V2, V2_TO_V3]` | one step per version pair, spec 12.5 step 9 |
| `V1_TO_V2` output version | `PROJECT_SCHEMA_VERSION` | literal `2` | Decision 14, frozen history |
| document fixtures | `project_v1.anim.json` | v1 retained, `project_v2.anim.json` added | a fixture per version the chain can start from |
| `schemas/project/` | `v1.json`, `v2.json` | both retained, `v3.json` added | v1 and v2 still describe files that exist on disk |
| `MANIFEST_SCHEMA_VERSION` | 1 | 1 unchanged | no asset field changed |
| `DOCUMENT_DOMAIN` | `anim_engine/document/v1` | unchanged | a field addition is a data event, not a layout change (ADR-0005 Decision 4) |
| `EASING_ALGORITHM_VERSION` | 1 | 1 unchanged | WP05 authors no easing |
| `PHILOX_ALGORITHM_VERSION` | (none) | 1 | **ADR-0008** |
| `SIMULATION_MODEL_VERSION` | (none) | 1 | **ADR-0008**; the constants and stage order of Decisions 6, 8, 9, 10, 11 |
| `CHECKPOINT_FORMAT_VERSION` | (none) | 1 | **ADR-0008** |
| `schemas/commands/v1.json` | 1 | unchanged | 12.9's command set is unchanged; no command authors simulation settings yet |

Nothing is persisted that a later version of this ADR would misread: the section holds two
authored integers and thirty-two authored seed bytes, and every derived value — the clock, the
rate, the ticks, the ages — is recomputed from them.

## Outstanding, for a later package

1. **No command authors `ProjectDocument.simulation`.** `ProjectDocument::simulation` is a
   public field and the `CommandProcessor` remains the only writer by 12.6's rule, but 12.9's
   set is WP03's twelve and spec 14 adds none — the same gap WP04 recorded for events, and
   WP10 owes `SetSimulationSettings`-shaped commands with exact inverses, a bumped
   `every_command_kind_has_an_inverse_that_closes`, and a *narrowed* `Registry` like everything
   else that submits envelopes on a user's behalf.
2. **`SimError::TickOverflow`'s `"schedule size"` operation is unreachable** at any event count
   a document can hold, and is declared because the arithmetic path is shared. Stated here
   rather than papered over, on the same reasoning as `BUILD_STATE.md` risk 19.
3. **Particle capacity is unbounded in WP05.** 29's "Capacity limits truncate emission
   deterministically and report truncation; truncation settings must be part of the export
   profile" belongs to WP14 and WP09, and `ExportSettings` (18.8) has no such field either — a
   contradiction in the spec, not a gap in this package.
