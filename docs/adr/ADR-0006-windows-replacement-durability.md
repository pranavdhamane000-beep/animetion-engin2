# ADR-0006 — Windows replacement and recovery durability

Status: accepted for the mechanism; the recovery *policy* closes in WP03
Date: 2026-11-05
Work package: WP00 / WP03 (spec register: "ADR-0006 · Windows replacement and recovery durability · WP00/WP03")
Evidence: `docs/adr/evidence/wp00/wp00_replace_fault-before-commit.json`,
`..._fault-after-commit.json`, `..._fault-none.json`

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
4. **The probe's logic still lives in the binary.** Spec 9.9 presents
   `run_replacement_probe(dir, fault)` as a callable interface and spec 4.2 assigns
   atomic file replacement to `anim_core`. Moving it there is WP03's job, which is
   also where it becomes production code and inherits the `unwrap_used`/`expect_used`
   denials that currently apply only to production crates. Recorded so the next
   session does not treat the bin-local implementation as final.
5. **Fault injection is simulated by stopping, not by crashing.** A real power loss
   at these points is a different test from a process that returns early; the
   sequence is proven to be interruptible safely, which is what WP03's structure
   needs, and a disk-level fault remains outside what a probe can demonstrate.

## Schema/version impact

None. `project.anim.json` and `project.anim.json.bak` names come from spec 4.3 and
do not change.
