# AGENTS.md — how to work in this repository

## Resume protocol (do this before any code)

1. Read `BUILD_STATE.md` — environment facts, stage status, current package.
2. Read `DECISIONS.md` — constraints already settled; do not re-open them.
3. Read the spec section for the package in hand. The spec is the `.docx` in
   this directory; its sections are cited as `spec 9.5`, `spec 33`, and so on.
   A `.docx` is a zip of XML and is not directly readable, so extract it first:
   `python tools/spec_dump.py "<the .docx>" .tools/spec_v32.txt` (stdlib only, no
   pandoc). The generated `.txt` is git-ignored output — always regenerate it rather
   than trusting a copy left by an earlier session. This step failed a fresh-session
   resume test on 2026-11-05 because the script itself was unversioned; it is now at
   `tools/spec_dump.py`.
4. Run the gate: `tools\ci.cmd` (spec 9.5 step 2, DECISIONS.md #19). It resolves cargo
   itself, so it works from a shell that has no toolchain on PATH. Add `-WithProbes`
   only when re-certifying the platform — after a driver update, a toolchain bump, or a
   change to the pinned FFmpeg. Trust a failing gate and the committed code over any
   summary, including one you were just given.

A fresh session is expected to be able to complete any unbuilt package from
those four steps alone, with no prior conversation. If it cannot, the state
files are wrong — fix them before writing engine code.

## Authority

| Question | Answered by |
| --- | --- |
| What should the engine do | The spec `.docx` — authoritative, never restate it here |
| Where are we now | `BUILD_STATE.md` |
| Why it was chosen this way | `DECISIONS.md`, then the spec's ADR register (sections 24, 48) |
| What is actually built | git history and passing tests |

These do not overlap on purpose. When two of them disagree, the spec wins over
the files, and the code plus tests win over the files' claims about the code.

## Hard rules

- **Package boundaries are session boundaries.** Finish one work package,
  validate it, then stop. Do not begin the next in a chat that already carries a
  finished package.
- **End of every package, in this order:** append the Completed Work Packages row
  in `BUILD_STATE.md`, add a `DECISIONS.md` entry for anything non-obvious, then
  commit. A package with no commit is not finished.
- **Never break a package in half across chats.** If implementation and
  validation cannot fit in one session, raise it as a spec gap instead of
  carrying half-built work into a new context.
- **Before modifying a package you did not build in this session**, read its code
  and its tests, and run `git log` on that path. Never rebuild something from a
  recollection of what you think was there.
- **If the validation criteria for a package cannot be stated in one sentence**,
  ask before coding it. Do not invent your own acceptance test.
- **Some gates are not yours.** WP10, WP14–WP17 and WP18–WP22 carry visual or
  aesthetic criteria you cannot self-certify. Build to the spec's Test A–E
  fixtures, then record the outstanding approval in `BUILD_STATE.md`. Saying
  "code complete" is correct; saying "passes gate G12" is not, until the user
  approves.

## Context discipline

- **Delegate broad discovery.** Any investigation expected to read more than
  three files, or to compare many options — dependency fitness, driver and SDK
  capabilities, log and artifact triage, spec-wide searches for a constraint —
  goes to a subagent. Return to this conversation one conclusion plus the file
  paths that prove it. Never accumulate that reading in the session that is also
  writing code.
- **Search once, from the main agent**, then pass findings down. Do not run a
  search yourself that a subagent was already given.
- **Independent packages parallelise; coupled packages do not.** WP08 audio and
  WP14 VFX can run side by side. Never parallelise along the time/identity/render
  spine — coordination failure there is more expensive than the lost time.
- **Watch for degradation.** If earlier decisions are being re-litigated, or the
  same file is being re-read to recover details that should be on disk, stop,
  update `BUILD_STATE.md`, and tell the user to open a new chat at the next
  package boundary. Do not push through a fuzzy session to squeeze out more work.

## Not in this file

Code style, crate layout, error taxonomy and determinism requirements live in the
spec. Nothing project-specific belongs here except the rules above — for example,
the spec already forbids shell-launched child processes (spec 9.4) and silent
fallback to system FFmpeg or an unrelated GPU adapter (spec 9.11); those are
enforced by tests, not by this document.
