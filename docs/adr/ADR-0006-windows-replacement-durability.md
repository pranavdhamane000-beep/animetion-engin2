# ADR-0006 — Windows replacement and recovery durability

Status: **closed** — mechanism accepted in WP00/WP01, recovery *policy* accepted in WP03
Date: 2026-11-05 (mechanism), 2026-11-06 (policy, WP03)
Work package: WP00 / WP03 (spec register: "ADR-0006 · Windows replacement and recovery durability · WP00/WP03")
Evidence: `docs/adr/evidence/wp00/wp00_replace_fault-before-commit.json`,
`..._fault-after-commit.json`, `..._fault-none.json`. WP03:
`crates/anim_core/src/io/atomic_replace.rs`, `crates/anim_project/src/{save,recovery}.rs`,
`crates/anim_project/tests/persistence.rs`,
`crates/anim_test_support/src/bin/wp03_save.rs`,
`crates/anim_test_support/tests/wp03_fault_injection.rs`

## Problem

`project.anim.json` is the authoritative commit record (spec 4.3). A partially
written project file is the one failure this engine cannot re-derive out of: there is
no upstream state to replay it from. Spec 6.3 therefore prohibits writing final
destinations incrementally and prohibits deleting an existing destination before
replacement is known to be possible, and spec 9.7 sets the test a fault must pass:
"every injected replacement fault leaves recoverable old or new bytes" (9.12).

The question is which Win32 primitive actually delivers that.

## Alternatives considered

- **`ReplaceFileW`.** Rejected, and the reason is on its own documentation page.
  It is specified as three steps — rename the target aside, rename the new file in,
  delete — and lists intermediate failures in which the destination path *ceases to
  exist*: `ERROR_UNABLE_TO_MOVE_REPLACEMENT` ("the replaced file no longer exists")
  and `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` ("the file to be replaced still exists
  with a different name"). A fault before commit must leave the old bytes readable
  **at the destination**, which this cannot promise. Its `REPLACEFILE_WRITE_THROUGH`
  value is additionally documented as "This value is not supported", so it cannot
  offer a durable rename either.
  Kept as the alternative *if* a later requirement appears for preserving the
  destination's creation time, short name, DACL, encryption or named streams —
  `ReplaceFileW` preserves all of those and a rename does not.
- **`std::fs::rename`.** Rejected: it is a thin wrapper over `MoveFileExW` with
  `MOVEFILE_REPLACE_EXISTING` and nothing else, so it cannot express the durability
  requirement, and the engine would lose the ability to say which flag it relied on.
  It is on `clippy.toml`'s disallowed list for that reason.
- **Write in place.** Prohibited by spec 6.3.
- **`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`.**
  Chosen. On NTFS, same volume, this is a directory-entry swap: a reader sees the old
  file or the new one, never a blend, and the call does not return until the rename
  is on disk. It also does not require the destination to pre-exist.

## Decision

The sequence WP03 will implement, proven by `wp00_replace` on this machine:

1. Write the new state to a temporary **beside** its destination.
2. `WriteFile` → `FlushFileBuffers` on the open handle. Flushing after close is not
   possible, so this step has to be in the sequence rather than implied.
3. Write `project.anim.json.bak` from the previous state — an explicit backup, which
   is what spec 4.3 already prescribes and what makes `ReplaceFileW`'s internal
   backup unnecessary.
4. Verify the temporary and the destination share a volume, by comparing
   `BY_HANDLE_FILE_INFORMATION.dwVolumeSerialNumber`, not by comparing path prefixes.
5. `MoveFileExW(temp → destination, REPLACE_EXISTING | WRITE_THROUGH)`.
6. Reopen destination and backup and compare both against the states they should hold.

## Evidence

Three fault points injected on NTFS, 64 KiB distinct states, 2026-11-05:

| Fault point | Destination | Backup | Temporary | Verdict |
| --- | --- | --- | --- | --- |
| `before-commit` | complete **old** (blake3 `0cb98c1a…`) | complete old | still present | required outcome |
| `after-commit` | complete **new** (blake3 `230bff58…`) | complete old | — | permitted (new, never mixed) |
| `none` (full run) | complete new | complete old | consumed, absent | clean |

Volume serial compared was `0xfea2d1fe` on both files. A deliberately cross-volume
run (destination on `D:`, temporary on `C:`) was refused as `replace.cross_volume`
**before** any rename was attempted, which is the behaviour spec 6.3 asks for rather
than a failure discovered afterwards.

No scenario produced a destination that was neither complete state, which is the
assertion 9.12 requires.

## Consequences

1. **The surviving file carries the temporary's ACLs and creation time**, not the
   destination's. If a later requirement needs the original security descriptor or
   timestamp preserved, this decision reopens toward `ReplaceFileW` and its
   documented intermediate states must be handled explicitly.
2. **The `.bak` is written by us, before the rename.** It is therefore a real
   recovery input with a defined content (the previous committed state), not an
   artifact of the replacement API. `spec 4.3`'s statement that backups are recovery
   inputs, not alternate current projects, is what makes this the right place.
3. **Same-volume is checked, not assumed.** Config must not point the temporary at a
   different drive; the check is by volume serial because a path prefix proves
   nothing about mount points or junctions.
4. **The probe's logic no longer lives in the binary.** Closed in WP01, which moved the
   sequence into `anim_core::io::atomic_replace` and inherited the `unwrap_used` and
   `expect_used` denials; WP02 extended it with `StagedWriter`/`Staged` for streamed
   payloads (DECISIONS.md #28) and WP03 consumes it for `project.anim.json`. `wp00_replace`
   remains as the platform proof that this machine's `MoveFileExW` behaves as claimed; it is
   not the production path and nothing imports it.
5. **Fault injection was simulated by stopping, not by crashing.** WP03 closes the half of
   this that a probe could not reach: `crates/anim_test_support/tests/wp03_fault_injection.rs`
   spawns `wp03_save` as a child, waits for it to announce the boundary it has reached, and
   terminates it there. Nothing in the child runs a destructor, so the surviving
   `TemporaryGuard` is absent rather than honored, and what the assertions check is the
   directory. A disk-level fault — cache loss after a returned `FlushFileBuffers` — remains
   outside what any process can demonstrate about itself, and BUILD_STATE risk 10 keeps that
   stated.
6. **A killed process is allowed to leave a temporary, and that is a tested state rather
   than a leak.** The two boundaries between the temporary's creation and its rename must
   leave exactly one `.project.anim.json.<n>.tmp` behind (`expect_residue`), which is
   positive evidence the kill landed mid-sequence: a run that left nothing would mean the
   rename had already completed and the scenario was proving a finished save. The next save
   takes a fresh exclusive name rather than adopting the abandoned bytes, so an orphan costs
   directory entries and nothing else. Spec 4.3 defers unreferenced-object garbage
   collection and forbids running it implicitly, and the same rule applies here: cleanup of
   another process's residue is not a save's business.

## Recovery policy, accepted in WP03

The mechanism above says a replacement is atomic. The policy says what may be done with the
files it leaves, and spec 12.7 gives three rules and one silence:

1. **`save_atomic` uses `StagedWriter`/`Staged::publish` rather than `write_atomic`, and
   then re-reads the destination in full.** `write_atomic` takes the payload as one slice and
   owns every step internally, so its boundaries are not observable — and spec 12.12 requires
   failures and kills *at each stage*. Staging the bytes by hand makes each step a
   [`Boundary`] an observer is told about. The cost is that `Staged::publish`'s post-rename
   check is length-only, which is the right economy for a multi-gigabyte streamed payload and
   the wrong one for a project file, so `save_atomic_observed` performs the byte comparison
   itself after publication (DECISIONS.md #28's asymmetry, resolved in favor of the stronger
   check where it is affordable).
2. **Recovery reads and never writes.** `recovery::inspect` validates the current, backup and
   autosave files independently, and `recovery::select` returns a decision, not a file
   operation. `inspection_and_selection_write_nothing_anywhere` proves the claim as a
   property of the directory — every file listed, with its digest, identical before and after
   a full inspection and selection — rather than as a statement about the code. "Never
   silently overwrite a newer manual save with an older autosave" is then satisfied by
   construction: there is no code path in that module that could.
3. **Ambiguity is a returned value.** Two complete candidates at the same revision with
   different digests, or a readable current file with a recovery input ahead of it, produce
   `RecoveryDecision::Choose` with every candidate intact. `ProjectError::AmbiguousLineage`
   is a **warning**, on DECISIONS.md #29's reasoning about `Cancelled`: being asked to choose
   is the behaviour spec 12.7 requires, and calling it an error teaches an operator to
   ignore the one diagnostic that protects a newer save.
4. **"Explicitly recorded revision/generation" means `ProjectDocument::revision`.** No file
   in a project directory carries a generation counter — every other "generation" in the
   spec is a particle pool or a GPU target — and spec 7.1 keeps wall-clock values out of
   deterministic state and out of hashes. So the ordering recovery compares on is recorded
   inside each candidate, and `mtime` is read nowhere in `save.rs` or `recovery.rs`.
   `RecoveryMetadata` carries the revision, the project id, the schema version, the manifest
   digest and the digest of the canonical bytes together for exactly that reason.
5. **A file that loads but is not canonical is reported, not repaired.**
   `CompleteCandidate::canonical` is the round-trip result of ADR-0005 Decision 12 applied to
   a recovery input, so a report can distinguish "recovered" from "recovered and already
   rewritten". Rewriting is a save's decision, made later and visibly.
6. **`spec 12.7`'s `ReplaceFileW` and `.<uuid>.tmp` are superseded, in the repository's own
   terms.** Deviation row 7 said WP03 must reconcile them; that row now stands as the
   record, and `save_atomic`'s documentation cites it rather than 12.7's text. Spec 9.4's
   prohibition on shell-launched children is why the kill scenario spawns its own binary by
   path with an argument array, the same shape `probe_tools` uses for FFmpeg.

## Schema/version impact

None. `project.anim.json` and `project.anim.json.bak` names come from spec 4.3 and
do not change.
