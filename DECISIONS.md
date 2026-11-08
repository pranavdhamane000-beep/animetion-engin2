# DECISIONS

Append-only log of decisions the spec does not already make for us. It is not a
copy of the spec's ADR register (sections 24 and 48) — those are formal
architecture decision records delivered as part of the work packages. This file
is the working trail: why a dependency was chosen, where we departed from the
spec, what commands to run, and what constraints this machine imposes.

Entry format: date, sequence number, work package, decision, why, alternatives
rejected, and whether the spec needs an addendum.

Write an entry when the decision is made. A decision that exists only in a
conversation is a decision that will be silently reversed later.

---

## 2026-11-04

### #1 — WP00 — Continuity model for the whole build

Working state lives in files, never in conversation memory. One work package per
chat session, two at most, and a package boundary is where a session ends.
`BUILD_STATE.md` records status, `DECISIONS.md` records rationale, git records
completed code.

**Why:** the context window is a property of the model and cannot be extended
away. The build is made safe by making context exhaustion cheap — any future
session resumes from disk.

**Rejected:** one long session for the entire engine (compresses into a lossy
summary long before S5); relying on chat history or agent memory as the source of
truth (both are summaries of intent, not records of fact).

**Spec addendum:** none. Complements the spec's own validation sections.

### #2 — WP00 — Rust toolchain is not installed

Measured: no `rustc`, no `cargo`, no `rustup`, no `C:\Users\Pranav\.cargo`, no
rustup directory under `AppData\Local`. Windows SDK and Visual Studio are
present, so the MSVC linker should already be available and the GNU toolchain is
not needed.

**Decision pending user approval:** install rustup with the stable MSVC host,
then immediately pin the exact version in `rust-toolchain.toml` and record the
resolved `cargo --version` here. Pinning satisfies spec 9.5 step 2 ("pin the
compiler, generate Cargo.lock") and makes every later probe reproducible.

**Why:** WP00 cannot start without it, and an unpinned toolchain invalidates the
probe reports it will produce.

**Rejected:** installing via a package manager or a standalone MSVC build (harder
to pin, and rustup is what `rust-toolchain.toml` requires to be honoured);
winget/choco choices deferred until the user states a preference.

### #3 — WP00 — FFmpeg on PATH is not the build input

FFmpeg 9.0.2 gyan essentials is reachable on PATH. Spec 9.11 forbids silently
falling back to system FFmpeg, so production probes and WP09 export must take an
explicit executable path, and the binary must be vendored or pinned by hash.

**Decision:** defer choosing the pinned build until the media probe (spec 9.5
step 7) can compare candidates, but treat the PATH copy as untrusted input from
now on. Record its path and version only as environment evidence.

### #4 — Project — File roles, to prevent duplicate sources of truth

Three artifacts, non-overlapping:

| Question | Answered by |
| --- | --- |
| What should the engine do? | The spec DOCX (authoritative, never restated here) |
| Where are we right now? | `BUILD_STATE.md` |
| Why was it chosen this way? | this file |
| What is built and working? | git history, one commit per validated package |

**Why:** overlap guarantees drift, and drift guarantees a future session acts on
the stale copy.

### #5 — WP00 — `AGENTS.md` carries the resume protocol

Added a root `AGENTS.md` holding the read order, the authority table, session
boundaries, and context-delegation rules. The state files describe project
condition; this file tells an agent how to work.

**Why:** the failure mode the three files did not cover is that a fresh
conversation does not know it should read them. Instructions that must be
re-told by hand each session eventually aren't.

**Unverified assumption:** that this app auto-loads root `AGENTS.md` into every
new session's context. Not confirmed on this machine. If it is not loaded, the
file loses its automation but keeps its content, and each new chat must open with
one line: read `AGENTS.md`, then follow it.

**Residual:** no mechanism here prevents the agent from ignoring these rules. The
weakest link is the end-of-package state update, which relies on habit and is only
detected later by a git diff that disagrees with what `BUILD_STATE.md` claims.

**Verification owed:** exercise the real resume path once WP00 and WP01 are
committed — start a fresh session with no context and confirm it resumes from
disk alone. Until then this model is designed, not proven.

### #6 — WP00 — MSVC toolchain chosen, but the spec does not require it

Full-document search found zero occurrences of MSVC, GNU, `x86_64-pc-windows-*`,
any target triple, `rustup`, `channel = "…"`, the contents of
`rust-toolchain.toml`, a Windows SDK version, or VS Build Tools. The spec names
only "stable Rust with rustfmt and Clippy" (9.2) and requires a
`rust-toolchain.toml` (5.1). So the ABI choice is ours, not the spec's.

**Decision:** `stable-x86_64-pc-windows-msvc`. The C++ toolchain was **already on
this machine** and no install was needed: VS Build Tools 2026 at
`C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools`, MSVC 14.50.35717,
with `cl.exe`, `link.exe`, `vcvars64.bat`, UCRT and Windows SDK 10.0.26100.0
headers and libs verified present. `RUSTUP_HOME`/`CARGO_HOME` go on D: because C:
has only 18 GB free of 232 GB.

**How the error happened, so it is not repeated:** I searched
`C:\Program Files\Microsoft Visual Studio\18\Community` for `vcvars64.bat`, found
none, and concluded the C++ workload was missing. Build Tools installs to a
**separate instance** under `C:\Program Files (x86)\Microsoft Visual Studio\18\
BuildTools`, which I had listed but never descended into, and I ran no
`vswhere -all -products '*'` until later — that command names every instance and
would have settled it in one call. Absence of evidence in one guessed directory is
not absence. A read-only probe that had been run a few minutes earlier cost one
download attempt we did not need.

**Rejected:** modern mingw-w64 via chocolatey — it was moot, and the `windows`
crate on the GNU target needs import stubs that can go missing, a debugging tax
spread across every later Win32 package.

**Build hazard discovered alongside it:** plain `link.exe` on this PATH resolves to
`D:\moneyprinter v2\Git\usr\bin\link.exe`, the MSYS hard-link file utility, not a
linker. Every build must therefore run from a `vcvars64.bat`-initialised shell (or
equivalent) so the MSVC `bin\Hostx64\x64` directory precedes Git's `usr\bin`.
Capture this as a `build.cmd`/`.bat` launcher rather than relying on whoever opens
a terminal.

**Owed:** this belongs in `docs/adr/` as an accepted-platform decision (spec 9.3
assigns exactly that duty to that directory), including the linker-path rule.
Create the ADR during WP00, not now.

### #7 — S1 — Work packages WP00–WP05 are a strict chain, not a DAG

Spec 6.1 makes them a single sequential arrow, and 6.2 grants no exceptions below
WP06. Checked against the per-package prerequisites: WP02 and WP03 cannot run
concurrently either, because 12.2 requires "WP02 manifest" and 11.4 states
`anim_project` owns the manifest and depends on `anim_assets`.

**Consequence:** parallel subagents cannot build S1 packages side by side. The
spec permits concurrency only from WP06 upward — 6.1 allows WP06/WP07 overlap for
instrumentation, and WP08 offline audio may proceed in parallel with WP06/WP07
after WP04, provided no integration gate is skipped.

**How subagents are still used in S1:** for research, spec extraction, hardware
probing, and reviewing an implementation after it is written — never for
concurrent construction of coupled packages.

**Gate mapping for stage S1** (spec 8.3): G0=WP00 · G1=WP01 · G2=WP02+WP03 ·
G3=WP04 · G4=WP05. G0 cannot pass using reports from a different adapter or driver
without revalidation (9.12).

### #8 — WP00/WP10 — Pen input removed from scope (supersedes the earlier deferral)

This laptop has no pen or touch digitizer (PnP enumeration found only an ASUS
Precision Touchpad and a USB mouse). An earlier entry in this section recorded a
decision to build the probe and defer its hardware proof; that was made before
checking whether anything consumes ink strokes at all. It does not, so the deferral
was the wrong frame — the question was scope, not scheduling.

**Gap analysis (searched the full v3.2 spec):** pen appears in exactly two places —
the `wp00_pen.rs` platform probe and `anim_editor/src/ink.rs` in WP10 §19. WP10
specifies *capture* only: `GetPointerPenInfo`, pressure normalised to `[0,1]`,
coalesced history, frozen view transform at pointer-down, transient preview that
"commits exactly once" as one undo entry. Nothing in the document gives a committed
stroke a consumer. Terms for the artwork role are absent outright: `corrective` and
`brush` have zero hits, `cleanup` appears only in transaction and resource contexts,
`paint` only in a dope-sheet file path, `raster` only in CPU silhouette analysis.

Meanwhile the artwork path the engine actually uses is imported assets: WP02's
content-addressed store feeds named poses and replacement cels ("Named poses and
replacement cels occur at exact frames"), and WP14 generates smears procedurally
("Stylized smear. Use driver velocity magnitude for stretch, clamped to max aspect").
The spec also states Level 3 does not guarantee hand-drawn smears or original
choreography — those are Level 4, and they arrive as cels.

**Decision (user-approved 2026-11-04):** drop `wp00_pen.rs` and `anim_editor/src/
ink.rs`. Artwork is authored externally in Krita or Clip Studio Paint and imported
through WP02. This removes the digitizer prerequisite, the deferred-G0 caveat, and a
portion of WP10.

**Why:** building a capture mechanism with no consumer spends two work packages to
produce a feature nothing uses, and it keeps an unsatisfiable hardware requirement
alive on a machine that cannot meet it.

**Rejected:** strokes as timeline annotation — retains the digitizer debt for
convenience the mouse already provides; strokes as renderable artwork — needs a whole
paint module (rasterisation, layer blending, alpha, colour management, brush model)
that the spec does not contain, and makes the tablet urgent rather than optional.

**Consequences to carry:**
1. Spec §9.3, §9.5 step 6, §9.12 and WP10 §19 now describe work we have deliberately
   cancelled. **The DOCX is still authoritative and still contains them**, so this
   entry and the Deviations section in `BUILD_STATE.md` are the only things standing
   between a future session and rebuilding the pen probe. A spec addendum removing
   them is owed; until it exists, read Deviations before starting WP00 or WP10.
2. WP10 keeps everything else — scene editing, transport, export launch, preview
   quality adaptation. Only ink is removed.
3. G0 becomes fully achievable on this machine, since pen was the one prerequisite
   it could not meet.
4. Level 4 remains dependent on hand-drawn frames. This decision changes *where* they
   are drawn, not whether they are needed.
5. Level 3 gates still require "review annotations" as evidence (spec §45). Those are
   producible with mouse, text and frame markers. If marking up clips by hand ever
   feels slow, a pen tablet reopens this decision rather than reversing it.

### #9 — WP00 — Rust toolchain installed and proven

`RUSTUP_HOME=D:\rustup`, `CARGO_HOME=D:\cargo` (persisted with `setx`, user scope),
host `x86_64-pc-windows-msvc`, profile default. Resolved versions: **rustc 1.99.1
(58cf8f9c3 2026-10-13)**, cargo 1.99.1, rustfmt 1.10.0-stable, clippy 0.1.99.

Proven by compiling and running a hello-world exe — no vcvars shell, no admin.

**Pin for `rust-toolchain.toml`:** `channel = "1.99.1"` with `components =
["rustfmt", "clippy"]`, satisfying spec 5.1 and 9.5 step 2. Record the exact
`rustc --version` string in the WP00 ADR alongside it.

**Claim corrected the same session:** I first wrote that every build must run from a
`vcvars64.bat` shell because PATH resolves `link.exe` to the MSYS hard-link utility.
Testing showed `cargo build` succeeds from plain Git Bash — rustc finds MSVC through
registry detection, not PATH. The residual is narrower: `cc`-crate build scripts and
hand-written shell commands that resolve a linker off PATH can still get the wrong
binary, which is why a `build.cmd` wrapper is still worth having. Written down so the
overstatement is not repeated by the next session that reads this.

### #10 — WP00 — FFmpeg vendored into `tools/ffmpeg/` and pinned by SHA-256

Supersedes the deferral in #3. Spec 9.2 requires candidate FFmpeg/FFprobe binaries to
be available before WP00; this machine had exactly one candidate
(`D:\ffmp\ffmpeg-9.0.2-essentials_build\bin\`), so "compare candidates" had nothing to
compare. Chosen on 2026-11-04 (user-approved) to copy that build into `tools/ffmpeg/`
rather than point the probes at the external folder.

**What was actually done:** `ffmpeg.exe` and `ffprobe.exe` copied into `tools/ffmpeg/`,
`ffplay.exe` deliberately left out. Both copies executed from the new location. Pinned
digests:

| Executable | SHA-256 | Version |
| --- | --- | --- |
| `tools/ffmpeg/ffmpeg.exe` | `3256173f3f8bffd7df12227c68adf68025edb1832273a9530688a7bb1ed8edec` | 9.0.2-essentials_build-www.gyan.dev |
| `tools/ffmpeg/ffprobe.exe` | `f0d36ecbbdd3bcfac3efa078c96c7271c2e68b3810595552ac3b7f17e9a65c52` | 9.0.2-essentials_build-www.gyan.dev |

Recorded in `tools/ffmpeg/ffmpeg-manifest.json` (versioned) together with the
capability list read from the binary itself: `ffv1`, `libx264`, `libx264rgb`, `mpeg4`,
`libx265`, `libvpx`, `libaom-av1`, `openjpeg` for video; `aac`, `pcm_s16le`, `libopus`,
`libvorbis`, `libmp3lame` for audio. The build is `--enable-static`, so there are no
DLLs to stage.

The `.exe` files are git-ignored — 210 MB has no place in this repo's history — so the
manifest is the versioned record and the binaries are an operator-supplied input. That
asymmetry is deliberate and is stated in the manifest's `vendoring` field.

**Why:** spec 9.11 forbids silently falling back to system FFmpeg, and an in-repo
pinned copy with a recorded digest makes "which binary produced this probe report"
answerable years later. BUILD_STATE risk #3 closes.

**Rejected:** referencing the external path (works, but the probe breaks the moment
that folder moves and the pin is not self-contained); downloading a different pinned
build (needs network and a distributor decision, and essentials already contains every
codec WP00 and WP09 discuss).

**Encoder choice for the export probe:** `ffv1` over `libx264` for the raw-video
round-trip. x264's default lookahead and B-frame decisions make an exact frame count a
less direct proof of the pipeline; ffv1 is lossless, intra-frame, and in every FFmpeg
build. If WP09 needs delivery codecs it tests libx264 explicitly, which is a different
question from "does our readback→pipe→encode→mux→decode path hold counts".

**Consequence for commit hygiene (agreed same exchange):** the working-tree edits to
`BUILD_STATE.md`/`DECISIONS.md` that had been sitting uncommitted since the baseline
commit get their own commit before WP00 code lands, so a package commit contains only
that package's work. This matches the precedent set by `3d60b84`.


## 2026-11-05

### #11 — WP00 — `clippy::pedantic` enabled, then removed the same day

Enabled at the workspace level on the theory that a determinism-critical engine
should hold itself to the strictest available lint set. Measured against the first
real code in the workspace: **131 diagnostics**, whose substance was
`similar_names` (`argv` beside `args`), `must_use_candidate` on accessors, and
`missing_errors_doc` asking every function to restate the shared `ProbeError`
taxonomy in rustdoc. Zero defects. Spec 9.2 requires "stable Rust with rustfmt and
Clippy" and 9.5 step 2 requires warnings-as-errors; neither names a lint group.

**Decision:** `all = "deny"` stays. `pedantic` is allowed, and the lints that express
this project's *actual* risks are denied by name instead: `unwrap_used`,
`expect_used`, `await_holding_lock`, plus `clippy.toml`'s disallowed types and
methods (`HashMap`, `SystemTime`, `Instant`, `Mutex`, `RwLock`, `fs::rename`,
`fs::remove_file`, `thread::sleep`).

**Why:** a gate that produces 131 non-defects gets worked around rather than honoured,
which loses the lints that matter. The `#[must_use]` attributes and `# Errors`
sections that pedantic did ask for were kept anyway - they were worth writing, it was
the volume that was wrong.

**Residual:** pedantic would have caught real things in later packages
(`arithmetic_side_effects` among them). If a defect class appears that pedantic covers
and `all` does not, re-enable that single lint by name rather than the group.

### #12 — WP00 — `probe_tools.rs`: a file spec 9.3 does not list

9.3 names `probe.rs` for "structured probe reports and exit handling" and the five
binaries. Two probes need identical external-process machinery - argument-array
spawn, bounded stdout/stderr drain, deadline with terminate **and** reap - and
duplicating it means two chances to get the reap wrong, which is the half that
matters (spec 9.8: "child processes are released on every return path").

**Decision:** a shared `crates/anim_test_support/src/probe_tools.rs`. It stays inside
the WP00-only crate, so spec 9.10's "no production crate imports a WP00 binary"
still holds and nothing portable gains a dependency on it.

### #13 — WP00 — `tests/wp00_smoke.rs` relocated into the crate

9.3 names the path `tests/wp00_smoke.rs`. This workspace root is a **virtual**
manifest (members only, no `[package]`), and cargo does not compile a root `tests/`
directory for a virtual workspace. Verified on this workspace rather than assumed:
a file placed there produced `error: no test target named ...`.

**Decision:** the file lives at `crates/anim_test_support/tests/wp00_smoke.rs`, same
name, inside the crate that owns test support (spec 4.2). The alternative - making the
root a package - would add a 14th phantom crate that 4.2 does not list, and the
third alternative - leaving a file at `tests/` that never runs - produces a test that
reports coverage while executing nothing.

**Consequence for WP11:** spec 4.1's `tests/integration/` and `tests/golden/` have the
same problem. Cross-crate integration tests need either a member crate that owns them
or a root package. Decide once, in WP11, before writing integration tests.

### #14 — WP00 — `config/default.toml` read by a line scanner, not a TOML parser

The config file is required by 9.3 and its `[tooling.ffmpeg]` paths are the explicit
external-tool setting 5.1 demands. Spec 5.1's dependency table lists `serde` and
`serde_json` but **no TOML reader**, so reading the file properly would be a fourth
dependency exception.

**Decision:** the probes read exactly the two path keys they need with a line scanner
over the one section, and `wp00_smoke.rs` asserts the shape (both keys present, both
repo-relative, both resolve inside the workspace, and `shell_launch = false` stated).
Spec 18.5 - "parse configuration and validate external-tool paths" - is WP09, where a
typed config is genuinely needed; whether `toml = 1.2.0` joins the baseline is that
package's decision and is not made silently here.

### #15 — WP00 — G0 evidence placement

9.5 step 10 requires machine-readable probe reports and benchmark-machine metadata to
be recorded, and 8.2 requires release evidence to be retained, but 4.1 says probe
output is not a source-of-truth input - which is why `probe-out/` is git-ignored.

**Decision:** the five `wp00_*.json` reports plus `TOOLCHAIN.txt` are copied to
`docs/adr/evidence/wp00/` and versioned, adjacent to the ADRs that cite them.
Generated output stays unversioned; the specific reports that certify G0 become
evidence, and the distinction is the one 8.2 draws between scratch and retained
failure artifacts.

### #16 — WP00 — G0 result summary

All five probes pass on the reference machine and the three machine-checkable gates
are green (`cargo fmt --check`, `clippy --workspace --all-targets -- -D warnings`,
`test --workspace --locked`, 17 tests). Numbers and the six ADRs are in
`docs/adr/`. Two findings change later packages' designs and are recorded there
rather than here: LUIDs are not stable across boots (ADR-0002 consequence 2), and
FFmpeg exits 0 on truncated video input while audio frame counts cannot be read from
the container at all (ADR-0014 facts 2 and 3).


### #17 — Project — Resume protocol exercised against a fresh session, and it failed one step

DECISIONS.md #5 recorded the continuity model as "designed, not proven" and owed a
test: start a session with no context and confirm it resumes from disk alone. Run on
2026-11-05, immediately after WP00's commit, by delegating the four protocol steps to
an agent with no conversation history and instructing it to be skeptical rather than
confirming.

**What held:** steps 1, 2 and 4 worked verbatim. The named package (WP01/G1), the gate
mapping, the must-not-do list, and the test command were all recoverable and true; the
four commands were re-run by the fresh session and all exited 0 with 17 passing tests.
The probe flags it read out of the source (`--force-unavailable`, the three fault
names) matched the state files.

**What failed — step 3, and it failed structurally, not cosmetically.** "Read the spec
section" points at a `.docx`, which is a zip of XML and not directly readable. The
extraction script and its output lived in `.tools/`, which `.gitignore` excludes, and
neither file was mentioned in `AGENTS.md`, `BUILD_STATE.md`, `DECISIONS.md` or any ADR.
On this machine the test passed only because a stale extraction happened to be present;
on a fresh clone step 3 is unsatisfiable, so the entire continuity model was resting on
an unversioned, undocumented artifact.

**Fixes, all committed with this entry:**
1. `tools/spec_dump.py` is now versioned (`tools/` is not ignored; only
   `/tools/ffmpeg/*.exe` is). The generated `.txt` stays unversioned output and
   `AGENTS.md` step 3 now says to regenerate rather than trust a leftover copy.
2. `AGENTS.md` step 3 states the extraction command inline.
3. `BUILD_STATE.md`'s resume point now lists what already exists in `crates/` — the
   test had to open source files to learn that `anim_core` holds only `diagnostic.rs`,
   and that `uuid`/`num-bigint`/`blake3` are pinned but referenced by no crate, so
   their first `Cargo.lock` appearance in WP01 is expected rather than drift.
4. Three factual errors found by the same review: `#16` said "four ADRs" (there are
   six); `ADR-0015` said "two artifacts" in one place and "five locations" in another;
   corrected to a consistent count.

**Residual, not fixed:** `AGENTS.md:44-48` lists WP10, WP14-17 and WP18-22 as gates an
agent may not self-certify, and **omits G1**. Whether WP01's gate is self-certifiable is
therefore genuinely ambiguous on the text. It is not mine to settle — the list is a
boundary the user drew. Ask before treating G1 as passed.

**Why this test belongs in the protocol rather than in one conversation:** the model's
whole claim is that a fresh session can resume from disk. A claim about fresh sessions
cannot be verified by the session that wrote the files, because that session knows what
it meant. Only a session without that context tests it. Re-run this after each stage
boundary, not each package.

### #18 — WP06/WP07 design input — adapter identity: what to check, what not to build

Settled during the WP00 closeout discussion, before any render code exists, so WP06
does not have to re-litigate it. Nothing here is built; it constrains how WP06, WP07
and WP10 implement.

**Three separate questions, which is why one check is not enough:**

| Question | Mechanism | Cost |
| --- | --- | --- |
| "Are Vulkan and DXGI naming the same silicon?" | LUID compare (WP00 proved it: both `0000000000011af8`) | one-time, needs a second `VkInstance` |
| "Is that the card we want to be on?" | PCI bus id + adapter name + driver vs the recorded G0 evidence | free — already on `AdapterInfo` |
| "Is this even the right class of device?" | `DedicatedVideoMemory` floor | free — already read by `GetDesc1` |

A LUID match passes on the iGPU if the OS handed the app the iGPU, because both APIs
then agree on the wrong card. So correlation is a *consistency* check and must not be
mistaken for a correctness check. PCI bus id supplies the correctness half for the
reference machine; `device_type` + vendor `0x10DE` supplies it portably.

**Decisions:**

1. **G0/WP06 revalidation keys on PCI bus id + adapter name + driver version, not on
   LUID.** Measured: LUID moved between two runs on the same machine and driver
   (`00cb5c07` in the 2026-11-04 probe, `0000000000011af8` on 2026-11-05), while PCI sat
   at `0000:01:00.0` both times, matching the board fact in BUILD_STATE. ADR-0002
   consequence 2 is amended to say this.
2. **A `DedicatedVideoMemory` floor is a required guard, not an optional one.** 3892 MiB
   here. Borrowed deliberately from how shipped games handle hybrid laptops: they check
   *capacity* ("requires 4 GB VRAM") rather than identity, because an integrated adapter
   reports near-zero dedicated memory and that single fact catches the wrong-GPU case
   with no identity logic at all. Costs nothing since we already call `GetDesc1`.
3. **Startup assertion in WP10, not a user reminder.** The proposal was to print a
   message at launch telling the operator to select the dGPU. Kept, but grounded: read
   the selected adapter once, compare name + driver + PCI to the committed evidence, and
   on mismatch print exactly that instruction. A reminder depends on a setting the engine
   cannot see; this version still tells the human what to do, but only after checking.
   The Windows per-app graphics preference is per-user, invisible to the process, and
   silently reverted by driver updates.
4. **LUID correlation stays, but only for WP07 budget attribution, once per process,
   gated on `dxgi-telemetry`.** Never per frame. `QueryVideoMemoryInfo` per segment is
   cheap but not free if polled; use `RegisterVideoMemoryBudgetChangeNotificationEvent`
   or rate-limit. This is the one job PCI bus id cannot do: `DXGI_ADAPTER_DESC1` carries
   no PCI field (verified against the struct we read: `Description`, `VendorId`,
   `DeviceId`, `SubSysId`, `Revision`, three memory sizes, `AdapterLuid`, `Flags`), so
   mapping a bus address to a DXGI adapter would need SetupAPI or WMI and be both more
   code and more fragile than the compare we already have working.
5. **`RequestAdapterOptions::power_preference = HighPerformance` is NOT sufficient
   alone** and must not become the only rule. It is a field rather than a vendor check,
   which is why it is tempting, but wgpu ranks by `device_type` and when no discrete
   device is enumerable it does not error — it degrades to the best available, which is
   the iGPU. That is precisely the silent failure this entry exists to prevent. Verify
   what it returns on this machine in WP06 rather than trusting the name.

**Why the engine needs more rigour than a game or a diffusion model.** Both hit the same
hardware, but their failures are self-announcing: torch reaches NVIDIA only through
CUDA, so the Intel adapter is invisible and a missing dGPU means zero CUDA devices, a
crash, or a CPU fallback an hour slow. A game on the wrong GPU gets low framerate. Ours
produces a complete, successful-looking export whose golden hashes silently stop
reproducing. Same physics, opposite loudness, and the loudness is the whole argument.

**Residuals, stated rather than hidden:**

- The cost figures above are *shape*, not measurement. I know correlation creates a
  second `VkInstance` and that this dominates, but nothing was ever timed. If WP07 needs
  a startup budget, instrument the existing probe and report milliseconds.
- `device_pci_bus_id` is an empty string when `VK_EXT_pci_bus_info` is unsupported. It
  is populated on this machine. Require non-empty and hard-fail; do not treat a blank as
  "unknown, proceed".
- An environment-level fix may delete this class of problem: a MUX switch / Advanced
  Optimus dGPU-only mode would stop the iGPU ever being selectable. **Unverified whether
  this machine has one** — worth checking in BIOS or Armoury Crate before WP06, since it
  is free and moves the decision out of code entirely. Our own 2026-11-04 measurement
  (Intel driving the panel, NVIDIA reported phantom) is classic switching behaviour.
- None of this is enforced by anything yet. It is a written constraint on packages that
  do not exist. WP06 owns turning it into code.

### #19 — WP00 closeout — CI: what spec 9.5 step 2 could actually mean here

Spec 9.5 step 2 says "…and enable warnings-as-errors in **CI**." WP00 was committed
with that clause unmet, and G0 was reported as passed anyway. That was too generous of
me: the exit text of G0 (8.3) is "all WP00 platform probes pass and exact versions are
locked", which the local commands satisfied, but step 2 of the implementation sequence
asked for more than I had delivered.

**The finding that decides the shape:** `git remote -v` returns nothing and
`@{u}` reports no upstream. There is no remote, so there is no hosted CI. A
`.github/workflows/*.yml` file would be a declaration of coverage that never executes
— the identical failure mode as the root `tests/` file in #13, which cargo does not
compile. Writing one would have looked like closing the gap while making it worse,
because now it would be documented as passing.

**Decision:** the gate is a script that runs here — `tools/ci.ps1`, launched by
`tools/ci.cmd`.

- Default run: `fmt --check`, `clippy --all-targets -- -D warnings`, `test --workspace
  --locked`. Measured 0.4s / 3–4s / 8–21s depending on cache state.
- `-WithProbes` re-runs the eight probe scenarios, ~50s total.
- It resolves `cargo` explicitly before use, because `RUSTUP_HOME`/`CARGO_HOME` were
  set with `setx` and do not reach an already-running shell — the failure this session
  hit twice, once in each direction.
- The two scenarios that must FAIL are asserted on the inverted exit code
  (`wp00_adapter --force-unavailable`). An expected-failure case that stops failing has
  stopped testing anything, and without inversion it reports as a clean pass.

**Verified that it can fail, not just that it passes.** I injected unformatted code
into `anim_core/src/diagnostic.rs`: `fmt` reported exit 1, `clippy` exit 101, and the
summary printed FAIL and exited nonzero. Then the file was restored from a copy and
`git diff` came back empty. A gate that only ever prints PASS is worse than no gate,
because it launders the doubt away.

**Why the probes stay out of any future hosted runner:** spec 9.12 forbids G0 passing
on reports from a different adapter or driver. Cloud Windows runners have different —
usually no discrete — GPU. Probes belong on the reference machine; only fmt, clippy and
test are hardware-independent. The script prints this warning after a probe run so the
constraint travels with the tool rather than living only here.

**Rejected:** GitHub Actions today (nothing pushes to it); a `build.cmd` calling
`vcvars64.bat` on every run — DECISIONS #9 established that cargo builds fine without
it, and adding an environment initialiser for a hypothetical C build script is a
fallback for a failure that was measured and found absent. If a crate with a `cc`
build script enters the graph and picks up the MSYS `link.exe`, that is the moment to
revisit it.

**Residual:** this is a *local* gate. If "CI" in the spec meant a hosted pipeline with
enforcement, the gap is unmet and closing it needs a remote plus a decision about
pushing this repository somewhere. Not mine to make.

**Also unresolved, and unrelated to CI:** the Vulkan validation layer gap (spec 9.12's
silent partner) still stands — every probe result here is an unvalidated run. Installing
the SDK is the fix and is not required by 9.2.


### #20 — S1 — Pre-commit enforcement, and the gate's two silent failure modes

Spec 9.5 step 2 asks for warnings-as-errors enforced, and G0 shipped with the gate as
a suggestion. #19 built the gate; a gate nobody runs is the same gap with better
documentation. User-approved 2026-11-05 to install a pre-commit hook.

**Generated, not committed.** `.git/hooks` is inside the git directory and is not
versioned, so the hook is written by `tools/install-git-hooks.ps1` and a fresh clone
must run `tools\install-git-hooks.cmd` once. That leaves a real hole and it is stated
rather than glossed: **cloning without running the installer yields a repository with
no enforcement and no visible sign of that.**

**Two failure modes that would otherwise pass unnoticed:**

1. **Line endings.** The hook is `/bin/sh`, and Git for Windows' sh refuses a
   CRLF-terminated shebang. A hook that cannot parse is indistinguishable from a
   hook that passed. So the installer writes explicit LF with no BOM via
   `[IO.File]::WriteAllText`, not through the PowerShell file cmdlets, which are
   what produced the bug in the first attempt.
   Verified on the installed file: 0 `0x0d` bytes, 47 `0x0a`, `sh -n` parses.
2. **`core.hooksPath` silently disables it.** If that config is set, Git ignores
   `.git/hooks` entirely and the install looks successful while enforcing nothing.
   The installer reads the setting after writing and warns loudly when it is
   non-empty. On this repo it is unset, which `git config --get core.hooksPath`
   confirms.

**Proven to block, not just to run.** Committed an attempt with unformatted code
staged: the hook ran the gate, all three steps failed, HEAD did not move. It blocked a
second time after a botched revert — `git checkout -- <file>` restores from the
**index**, which still held the injected line, so the working tree came back correct
while the staged content stayed broken. Caught by `git diff --cached`, fixed with
`git checkout HEAD -- <file>`, which resets both. That is the same lesson as #13:
verifying the thing you can see is not verifying the thing that runs.

**Probes excluded from the hook deliberately.** ~50s on every commit, and spec 9.12
ties their validity to this adapter, driver and FFmpeg build, so they belong at
re-certification (`tools\ci.ps1 -WithProbes`), not in a commit path. The hook runs
fmt, clippy and `test --locked` only.

**Known cost, accepted:** the gate inspects the working tree, not the staged diff —
`cargo fmt` and `cargo clippy` have no staged-snapshot mode. Committing a subset of
local edits therefore checks more than is being committed. That errs on the safe side,
but it does mean a commit can be blocked by something you never intended to stage.

**Bypass exists and is meant to be visible:** `git commit --no-verify`. Nothing in
this project will pretend that is equivalent to passing.

### #21 — S1 — G1 through G4 are self-certifiable

AGENTS.md listed WP10, WP14–17 and WP18–22 as gates an agent may not certify and said
nothing about G1. That left WP01's definition of "done" undefined, which is the wrong
kind of ambiguity to carry into the package that decides the engine's numeric core.
Answered by the user 2026-11-05: **WP01–WP05 gates may be self-certified.**

**Why that is the correct split, not a convenience.** G1–G4 assert arithmetic and
identity properties — rational round-trips, NTSC and subframe boundaries, negative
time, stable ids, snapshot and simulation hash equality. A test either passes or it
does not, and no human judgement sits between the run and the verdict. The gates held
back for approval are the ones whose exit criteria are "this reads as weight", "this
cut lands", "the action is legible" — properties a green suite cannot establish.

**Boundary, stated so it does not spread:** the ruling covers S1 only. From WP06 the
default reverts to code-complete-plus-outstanding-approval, because G5 introduces
golden frames and colour, and ADR-0016's tolerance question is a human call about
what counts as visually identical.

**Consequence:** WP01 should say "G1 passed" outright once its tests run green.
Hedging with "code complete" after a passing suite would be the same false modesty
that let an unmet clause look satisfied in #19 — understating a result is how a
requirement quietly goes unverified.

### #22 — WP01 — `FramePos::subframe` is private; the rule overrides the listing

Spec 10.8 writes `pub struct FramePos { pub frame: i64, pub subframe: Rational }`
and, two lines later, "Fields whose invariants require normalization are private."
Spec 10.6 uses the same verb of this exact field: "a subframe **normalized** to
[0,1). **Normalization** carries overflow into the frame." WP01's first draft read
the closing rule as being about `Rational` only, on the grounds that 10.8 marks
`subframe` `pub` and leaves `Rational`'s `num`/`den` unmarked — that the two
markings were a deliberate contrast. That reading is rejected: the rule names the
same thing 10.6 names, so it is not about `Rational` alone.

**Decision.** `frame` stays `pub` — it needs no normalization, and since spec 10.6
supports negative positions outright, every `i64` is legal, so reading or writing
it cannot produce an invalid value. `subframe` is private, read through
`FramePos::subframe()`, written only by `at_frame`, `new` (validating) and
`normalized` (carrying). `Deserialize` for `FramePos` is hand-written so that
persisted data passes the same gate: a derived impl could reach the private field,
which is how `project.anim.json` would smuggle `{"frame":5,"subframe":"3/2"}` in
from disk. `Serialize` stays derived, because it can only ever write a value that
already holds the invariant.

**Why the range is not cosmetic.** `(frame, subframe)` has to be a **unique** name
for one instant. `FramePos` is a sort key in three places — snapshot caches keyed
on `(scene_id, revision, FramePos)` (spec 7.2) and `(SceneId, Revision, FramePos)`
(spec 13.8), and "Sort keys by FramePos" for track sampling (spec 30) — and
because `clippy.toml` rules out `HashMap`, those are ordered maps keyed on the
type's own `Ord`. `FramePos{5, 3/2}` and `FramePos{6, 1/2}` are the same instant:
`time_of` returns an equal `Rational` for both, while `Eq` reports them different
and lexicographic `Ord` ranks them by representation rather than by time. One
timeline position, two cache keys, and a keyframe order that depends on how a
number happened to be written. `time_properties` must assert collapse, ordering,
time-equality and idempotence together.

**Also settled, from the same sentence.** `subframe` is `[0,1)` and never `1`,
because spec 13.6 makes a frame's end the next frame's start, so `1/1` must carry.
And it stays non-negative for negative times: spec 10.7's floor rule exists
precisely so a time just below a negative boundary gives frame `-2` with a
*positive* remainder, not frame `-1` approached from below.

**Residual.** Spec 25's definition of done wants every listed file to exist, and 10.8
lists field names; both are unchanged — only one `pub` is dropped. If a later package
wants to assign a subframe directly, it goes through a constructor and this entry
is the reason it must.

### #23 — WP01 — three rulings taken before coding

Answered by the user 2026-11-05 and written down because each is a constraint on
code in this package, and one of them contradicts the spec.

1. **`anim_core::io` builds on ADR-0006's `MoveFileExW`, not on spec 12.7.**
   The spec (line 749) prescribes `ReplaceFileW` with `lpBackupFileName`. ADR-0006
   rejected it from that API's own documentation — `ERROR_UNABLE_TO_MOVE_REPLACEMENT`
   leaves the destination path *absent*, which violates 9.7's requirement that a
   fault before commit leave the old bytes readable at the destination — and
   `wp00_replace` proved `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` with an
   explicit `.bak` on this machine. WP01 implements the proven sequence. **This is
   a stated deviation from spec 12.7, left for WP03 to reconcile** alongside
   ADR-0006's own status line, "the recovery *policy* closes in WP03".
2. **Typed IDs are derived, not random.** `uuid` is locked to
   `default-features = false, features = ["v5", "serde"]` (ADR-0001) and no spec
   text names a creation mechanism — 10.8 says only "generated once at creation".
   So `id.rs` exposes `derive(namespace, name)` over `Uuid::new_v5` and provides no
   random constructor. Verified rather than assumed: resolving the pinned `1.29.0`
   pulled `sha1_smol` and no entropy source, so v5-only is executable as designed.
   An earlier note in this session proposed `uuid = "1.10.0"`; that was wrong, the
   existing workspace pin was already correct, and 10.4's dependency list is
   unchanged.
   **Consequence for WP10:** spec 43's "Paste assigns new IDs" needs a fresh ID with
   no random source available, so it must derive from something the document owns
   (source id plus transaction seed, or an equivalent). That is WP10's decision to
   make, not a gap in WP01.
3. **Temp files cannot use spec 12.7's `.<uuid>.tmp`.** With no random UUID
   available, uniqueness comes from a `create_new` open attempt with a bounded
   counter, giving `.project.anim.json.<seq>.tmp` beside the destination. Same
   purpose — no two writers collide — deterministic, and no new dependency.

### #24 — WP01 — two files spec 10.3 does not list, and no cfg gate

1. **`crates/anim_core/src/io.rs` added.** Spec 10.3 lists
   `src/io/atomic_replace.rs` and `src/io/windows.rs` but neither a `io/mod.rs` nor
   an `io.rs`, and Rust needs one of them to declare the children. Chose the
   `src/io.rs` + `src/io/` form because it adds no path to the list 10.3 fixes. The
   same class of omission as deviation 4 (`tests/` needing an owning crate) and
   deviation 5 (`probe_tools.rs`): the spec's file tables name responsibilities, not
   module mechanics.
2. **`io/windows.rs` is not `#[cfg(windows)]`-gated.** The workspace pins
   `x86_64-pc-windows-msvc` and spec 2.1 names no other platform, so a cfg gate
   would guard a build that never happens, and its `#[cfg(not(windows))]` branch
   would have to be either a second unproven implementation or a silent fallback —
   which spec 9.11 forbids. The Windows-only constraint lives in
   `[target.'cfg(windows)'.dependencies]` in `Cargo.toml`, where it is one line
   instead of two code paths.

**Residual:** if a non-Windows target is ever added, `io/` needs a second
implementation and this entry is the place that decision starts.

### #25 — WP01 — G1 result summary

G1 passed 2026-11-05, self-certified under DECISIONS.md #21. `tools\ci.cmd`: fmt,
clippy `--all-targets -D warnings` and `test --workspace --locked` all green;
`anim_core` carries 48 unit tests plus 28 in `tests/time_properties.rs`, against 17
tests workspace-wide before this package. Probes were not re-run: the adapter,
driver, toolchain and pinned FFmpeg are unchanged, so `-WithProbes` would re- certify
hardware nobody moved.

**Two defects, both found by the reference-arithmetic method rather than by review.**
First, `tick_duration` computed `1/(num*N)` instead of `den/(num*N)` — `1/48000` where
the answer is `1001/48000`, a 1001× error in every step size WP05 would take; its own
first test asserted the wrong value and passed, which is why the test now checks the
property (`step × fps × N == 1`) instead of a literal. Second, `to_i64` reported
`TryFromBigIntError`'s message — "number too large to fit in target type" — so an
overflow error carried no value, breaking spec 23's "offending value context"
requirement exactly where it matters most.

**One assertion that was wrong for arithmetic reasons, not code reasons:** I expected
1001 frames to be one second at 24000/1001. It is not; frame 24 is 1.001 s, frame
24000 is 1001 s, and no whole frame lands on exactly one second. Also recorded: my
estimate that `i64::MAX/2` had a representable sample index was off by 1000×, and
`long_timelines_do_not_accumulate_error` is what proved it — the sample axis runs out
of width near frame 5.76e15 while the timeline itself is still fine.

**Consequence for WP02–WP05:** these are the types. Nothing downstream may define a
local seconds value, a `f64` time, a plain `u64` counter or an untyped id, because
spec 10.4 says every other crate imports these rather than inventing equivalents, and
the ones that need a narrower view (WP08's non-negative `SampleOffset`, spec 17.5, and
WP03's `RevisionOverflow`, spec 12.11) wrap these rather than reimplement them.

**Citation audit, same day.** Building this package surfaced that eleven of my
`spec x.y` references pointed at the wrong section: `ReplaceFileW` cited as 12.5 step 5
when it is **12.7**; the directory-flush rule as 18.4 when it is **12.11**; snapshot
cache keys as 6.4 when they are **7.2**; little-endian float bits as 22.5 when they are
**14.7**; `EventOccurrenceId` derivation as 15.4 when it is **13.7**; "sort keys by
FramePos" as 26.6 when it is **30**; paste's new IDs as spec 50 when it is **43**;
asset-store rename as 12.3 when it is **11.7**. All corrected across the source,
ADR-0004 and the state files.

**Why this is recorded rather than quietly fixed:** every citation in `BUILD_STATE.md`,
`DECISIONS.md` and the ADRs is load-bearing for a session that has to extract the DOCX
to check it, and a wrong section number is worse than a missing one — it sends the next
reader to a paragraph that does not say that. **How to re-check:** after extracting the
text, resolve the owning section with *both* heading levels, e.g.
`awk -v t=749 '/^# /{s=$0} /^## /{ss=$0} NR==t{print s" / "ss; exit}' .tools/spec_v32.txt`.
Tracking only `^## ` caused several of these errors, because top-level sections are `# `
and some WP subsections ("Algorithms", "Prerequisites") are unnumbered.

---

## 2026-11-06

### #26 — WP02 — decoders read metadata, and no image dependency was added

Spec 11.3 lists `image.rs` for "Image metadata, color and alpha decode" and `audio.rs`
for "PCM metadata and source decoding". Spec 5.1's dependency table names `symphonia`,
`hound`, `rubato`, `cpal`, `ringbuf` — and **no image decoder at all**. WP02 therefore
either bought a dependency the baseline does not carry, or did not do what 11.3's
responsibility column says. Ruled by the user 2026-11-06: **metadata only, no new
dependency.**

**What that means concretely.** `image.rs` reads PNG `IHDR` and the chunks up to the
first `IDAT`, and JPEG marker segments up to a start-of-frame, for dimensions, bit
depth, colour type, alpha presence and interlacing — header parsing written here, in
roughly the same line count a `png` crate call would have cost, with no new supply
chain. `audio.rs` reads the WAV container with `hound`, which 5.1 already pins, and
reports rate, channels, width, frame count and whether the payload is linear at all.
`decode.rs` carries the registry, the importer-version dispatch, the
`DecoderConfiguration` enum and the three-axis derived-cache key in full — that half is
not deferred.

**Why the split is the right one and not a dodge.** Spec 11.12's validation list — BLAKE3
vectors, streaming, duplicates, same-ID replacement, shuffled digests, confinement,
corruption, truncation, missing dependencies, failure before rename — contains no pixel
or sample check. The first consumer of decoded pixels is WP06's texture path and the
first consumer of PCM is WP08's mix; both own a *configuration* to decode into
(premultiplied or not, f32 or integer), which is exactly what
`(content_hash, importer_version, decoder_configuration)` keys on. Decoding to RGBA in
WP02 would pick that configuration a stage early and produce a buffer nothing reads for
two stages.

**Rejected:** adding `png` (a dependency exception, an ADR, and a supply-chain surface
for a feature with no consumer yet); stubbing the two files to registry entries only
(11.3's responsibilities would then be unmet, and WP06 would inherit undeclared work).

**Consequence for WP06 and WP08:** the seam is `Decoder::read_metadata` plus
`DecoderConfiguration`. Adding a real decode path means adding a method to that trait or
a second trait beside it, and the cache key already has the axis the new decoder needs
to vary on. Neither package should re-sniff the format: `ImageFormat::sniff` and
`SampleFormat::from_spec` are the published answers.

### #27 — WP02 — published BLAKE3 vectors vendored, and which column is used

Spec 11.12 opens with "Verify published BLAKE3 vectors". The `blake3` crate's published
source contains no test-vector file, so the table came from the reference
implementation: `https://raw.githubusercontent.com/BLAKE3-team/BLAKE3/master/test_vectors/test_vectors.json`,
fetched 2026-11-06 and vendored at
`crates/anim_assets/tests/fixtures/blake3_test_vectors.json` (35 cases, 0 to 102 400
bytes). The test regenerates each input from the format the file's own `_comment`
states — a repeating 251-byte sequence `0, 1, 2, …, 250` — rather than shipping 3 MB of
inputs.

**Two things worth recording so a later session does not re-derive them.** The file's
`hash` field is 262 hex characters: **131 bytes of extended output**, of which the first
32 are the digest this engine stores. Asserting the whole field against a 32-byte digest
fails, and a session that assumed the field was the digest would conclude the
implementation is wrong. And only the unkeyed `hash` column is used, because ADR-0004
chose a length-prefixed domain tag over `derive_key` precisely so a published digest can
be checked by hand — the `keyed_hash` and `derive_key` columns would certify a mode this
project does not use. `published_blake3_vectors_match_the_streaming_path` asserts the
`key` field matches the published string, which is what tells a reader the fixture is the
real table and not a hand-picked subset.

**Consequence:** the same test runs the one-shot, the 64 KiB streaming and the
file-on-disk path for every case, so it doubles as the proof that
`StreamingHash::update` and `hash_file` agree with `blake3::hash`. An update to the
upstream table is a deliberate change with no automated path; it is a vendored artifact
and its provenance is this entry.

### #28 — WP02 — anim_core gained a staged-stream API rather than anim_assets gaining a dependency

Spec 11.7's import ends in a rename-without-overwrite whose destination is not knowable
until the whole stream has been hashed, and spec 11.4 gives `anim_assets`
`anim_core, blake3, serde, hex, thiserror`, decoder libraries and `tempfile` — **no
`windows` crate**. `anim_core::io::atomic_replace::write_atomic` takes the payload as
`&[u8]`, so it cannot serve a stream. Ruled by the user 2026-11-06: extend the shared
API.

**What was added, and where it lives.** Inside `crates/anim_core/src/io/atomic_replace.rs`
— no new path, so spec 10.3's file list is unchanged — `StagedWriter::create_in` →
`write(chunk)` → `synchronize()` → `Staged` → `publish(&Replacement)`. It reuses the
existing exclusive-name loop (`open_unused_temporary`, extracted from `stage`), the
existing volume guard, the existing `MoveFileExW` sequence and the existing
`TemporaryGuard`. `write_atomic` and the streamed path now share
`publish_staged`, which takes an `Expectation`: byte-identical for an in-memory payload,
length-identical for a staged file. `Replacement::create_only` already existed for
exactly this call and is what `import` uses.

**One asymmetry to know about, because it looks like a hole.** In the streamed path the
destination check after the rename compares *length*, not content — holding a
multi-gigabyte payload to byte-compare it is the cost BUILD_STATE risk 9 names. The
content proof is taken one step earlier instead: `import` re-reads the staged temporary
and re-hashes it before publishing, so the bytes that are renamed are known to reproduce
the digest, and the rename is atomic. That is two passes over a large import rather than
three, and `store.verify` (spec 11.9) remains available when a caller wants the
destination re-checked too. `a_staged_file_clobbered_before_publish_is_refused` is the
test that the length check is real; it also documents what it cannot catch.

**Also added:** `CanonicalBytes::fixed_bytes::<N>` in `anim_core::hash`, and `uuid()` now
delegates to it. ADR-0004's rule "a width fixed by the type is not length-prefixed" had
no method to express it, and `ContentHash` needed it (ADR-0005 Decision 3).

**Rejected:** `anim_assets` depending on `windows` (spec 5.1's rule for that crate is
"target-gated features, no cross-platform API leakage", and `io::windows` is `pub(crate)`
by that decision); buffering the payload (fails 11.12's streaming test outright); a
`WP02-local` rename implementation, which is the duplication DECISIONS.md #12 was written
to prevent.

**Residual:** this modifies a package whose gate was certified yesterday. WP01's 83 tests
were run unchanged and still pass, and the refactor is behaviour-preserving for
`write_atomic`, but the rule in `AGENTS.md` — read the code and its tests first, run
`git log` on the path — is what made that edit safe rather than lucky.

### #29 — WP02 — four error additions, one privacy resolution, and how cancellation is signalled

**Taxonomy.** Spec 11.11 names ten categories; all ten are present
(`crates/anim_assets/src/lib.rs`, where 11.3 lists no `error.rs`, so `AssetError` lives
at the crate root rather than adding a path). Four were added because the spec requires
the rejection but the list has no row for it: `DuplicateDependency` (11.7 "duplicate
dependencies are rejected" — `DuplicateAssetId` was *not* reused, because the two ids
name different objects and a diagnostic that blames the wrong one sends the reader to the
wrong record), `MalformedAsset` (a stored object whose bytes do not match its declared
kind), `MalformedManifest` (a manifest that describes itself inconsistently — a record
filed under another id's key, or a domain tag that will not encode), and `SizeOverflow`
(11.7's "checked byte count"). The precedent for stating additions rather than inventing
rows is `anim_core::error`, which added `MalformedCanonical` and `NonFiniteValue` past
10.11's six.

**`dependencies` is private.** Spec 11.8 lists `pub dependencies: Vec<AssetId>` and
spec 10.8 says "fields whose invariants require normalization are private". DECISIONS.md
#22 settled that conflict for `FramePos::subframe` on the grounds that the rule names the
same thing the normalization does; the same reading applies here, because 11.7's sort and
deduplicate rule is a normalization. `AssetManifestEntry::new` validates, the field is
private with a `dependencies()` accessor, and `Deserialize` is hand-written so a
`project.anim.json` cannot smuggle an unsorted list in from disk — the failure mode #22
named. `records` in `AssetManifest` is private for the adjacent reason: the map key and
the record's own id must not be allowed to disagree.

**Cancellation has no parameter, so it has a contract.** Spec 11.9's `import` takes
`source: R` and nothing else, yet 11.11 requires a `Cancelled` outcome. It is produced
when the reader returns `std::io::ErrorKind::Interrupted`, which is how WP10's UI-driven
import will stop one. Note the deliberate asymmetry with `hash_stream`, which *retries*
that error kind as std requires: re-reading bytes already decided upon may resume, but a
stream the operator stopped must not. `Cancelled` is a **warning**, not an error — an
operator who cancelled an import did something supported, and calling it a fault teaches
them to ignore diagnostics.

**One addition to a spec-listed struct.** Spec 11.8 gives
`ResolvedAsset { entry, canonical_path }`; the implemented type carries a third field,
`policy: ResolutionPolicy`, because "resolved" is not one claim. A `ResolvedAsset` under
`Presence` says the file exists; under `Content` it says the bytes were read and hashed.
Dropping the field would make a preview's cheap resolution indistinguishable from
export's verification pass in a report, which is the asymmetry 11.7 exists to draw.

**Object-store temporaries are named `.import.<n>.tmp` inside `assets/objects/`.** Same
bounded-counter sequence as deviation 7 (no entropy source, so spec 12.7's `.<uuid>.tmp`
remains unsatisfiable), and deliberately in the objects directory so the rename is
same-volume. What makes 11.8's "temporary files are never resolved as objects" structural
rather than a convention is `AssetStore::parse_object_name`, which accepts only a
validated 64-hex-character digest: no temporary name can pass it, and the resolver only
ever reaches paths derived from a record's `ContentHash`.

### #30 — WP02 — result summary, and the half of G2 this package cannot claim

`tools\ci.cmd` green 2026-11-06: fmt, clippy `--workspace --all-targets -D warnings`,
`test --workspace --locked`. The workspace suite is **188 tests**: `anim_assets` 91 (72
unit plus 19 in `tests/store.rs`), `anim_core` 83 (55 unit plus 28 in
`tests/time_properties.rs` — 7 added for the staged path and `fixed_bytes`),
`anim_test_support` 14 unchanged. Probes were not re-run: adapter, driver, toolchain and
pinned FFmpeg are unchanged.

**G2 is WP02+WP03 together** (spec 8.3, DECISIONS.md #7). What this package can state is
that **WP02's own validation section passes**: every clause of 11.12 has a named test,
listed in the `BUILD_STATE.md` row. What it cannot state is "G2 passed", because
11.5 step 8 — connecting record insertion to WP03 commands — and 8.3's other three
clauses (atomic commands, save faults, migrations) have no code yet. That is the joint
gate working as designed, not a gap in this package.

**Five defects, all found by the tests rather than by review, and what each one means.**

1. `image.rs` checked `header[3]`/`header[4]` for PNG's compression and filter methods.
   Those bytes are the low order of the width and the height. A 256-pixel-wide image was
   refused as "unknown compression method"; the offsets are 10 and 11. Eight fixtures
   failed at once, which is what made it obvious rather than mysterious.
2. `dependency_closure` returned the traversal *stack*, which `visit` empties as it
   unwinds — so every closure came back empty and the test that asserted four assets
   asserted an order nobody had checked. The set is returned now.
3. `AssetManifest::insert` was first written to refuse a replacement whose content hash
   differed. That is spec 11.6's *normal* reimport path, so the first correct use of the
   API would have failed. `insert` replaces; `from_records` is where two records for one
   id is a defect worth reporting.
4. The JPEG fixtures declared segment length 6 where the payload is 6 bytes and the
   length field counts itself, so every start-of-frame parsed as truncated. Fixed in the
   fixture, with the framing arithmetic written down so it is not re-derived by hand.
5. `assert_no_stray_objects` was called on a scenario where a staged temporary *should*
   exist, and it failed correctly. The assertion now checks what 11.8 actually requires
   there — that the temporary is not resolvable as an object — and that it is gone after
   the abandonment.

**Carried forward, not hidden.** `AssetStore::derived_path` has no consumer in this build,
so it is exercised only by its own tests; WP06 is where a real cache is written and
where the key's third axis gets proven. `payload_too_short` for a JPEG and the
`hound` `Unsupported` mapping are typed-error paths with no fixture, because no
compressed-codec WAV or partial-frame JPEG was worth hand-building this package; WP11's
fault-injection pass is where the exotic inputs live. The import path does two read
passes over every asset (source→temp, temp→digest) plus the rename, and WP09's big-export
verification should read that as the budget, not as a bug to fix in passing.



---

## 2026-11-06 (WP03)

### #31 — WP03 — rulings taken while building the mutation boundary

Answered by the user 2026-11-06 as four bundled choices, then settled in code. Each is a
constraint on later packages, and two are additions to spec-listed shapes that must not be
rediscovered as mistakes.

1. **The raw scene model is minimal but real.** Spec 12.3 assigns `scene.rs` "raw scenes,
   nodes, tracks and markers" and `property.rs` "rational-time property keys", and no
   section anywhere gives their fields — searched. Ruled: build what 12.12's commands need
   to mutate and what WP04's evaluator needs to read, and stop. So `SceneData` holds nodes
   keyed by `NodeId` (which *is* the root order spec 13.7 wants), each node carrying `parent`
   plus a separate persisted `sibling_order` (13.7's two orderings, not one), one
   `PropertyCurve` per `(node, property)` pair, tracks as the container that makes 13.2's
   "unique canonical times within each property" unique *within something*, and markers
   identified by `(position, label)`.
   **Deferred on purpose:** motion-layer priority, pre/post extrapolation modes and key
   provenance tags. Spec 15 and 21 assign those rules, and authoring their data model here
   would decide a later package's semantics by accident.
   **Rejected:** placeholders until WP04 — 12.3's responsibility column would be unmet and
   "rollback from every command index" would have nothing to roll back.
2. **The command set is twelve, because it has to close under inversion.** 12.12 asks for an
   exact inverse per command, so `Upsert` needs the `Remove` half and vice versa:
   upsert/remove asset, upsert/remove scene, add/remove node, upsert/remove track,
   set/remove property key, place/remove marker. The user chose "six to seven document
   commands"; six cannot state its own inverses, and the extra six are not new capabilities
   but the other face of each of the first six. `UpsertAsset` is the one command the spec
   names for this package (11.5 step 8), and its inverse is the record 11.6's reimport
   displaced — which is why `AssetManifest::insert` returns what it replaced.
   **Rejected:** forward-loading `AddKeyframe`/`AddMotionLayer`/`SetCameraParams`; those names
   are WP15's (spec 15's command list), as are WP12/13/14/18–21's.
3. **`SetPropertyKey` never creates a track.** If setting a first key also created the track,
   that command would need two different inverses depending on state the caller did not
   choose. Track creation is `UpsertTrack`, and `set_property_key` refuses an unknown track id.
4. **Two spec-listed shapes gained a field, and one return type lost one.**
   `AppliedTransaction` (12.8 lists four fields) carries `transaction_label`: 12.9 makes
   `undo`/`redo` return a `CommandOutcome`, and an undo list row whose label is empty is a
   guess. The alternative was a parallel hash→label map in `History`, i.e. a second copy of a
   fact that belongs with the transaction. `CommandOutcome.command_id` is
   `Option<CommandId>`, `None` for undo and redo: 12.8 gives a transaction no envelope id, and
   deriving one by truncating a digest would be an identity nothing else can reproduce.
   Both are recorded the way #29 recorded `AssetError`'s four additions rather than folded
   into something adjacent.
5. **Acceptance checks run in 12.7's order, and replay comes before the revision.** 12.7 lists
   "schema, command ID replay, project ID, expected revision, nonempty batch and revision
   capacity". Checking the revision first would make a legitimate replay after an undo a
   `RevisionConflict`, because the document is then at a different revision than the one the
   original result names — and 12.7 promises "the original result". `a_new_forward_edit_
   clears_redo_and_the_retired_ids_stay_idempotent` asserts the consequence: a retired
   transaction's id is still idempotent, and recognizing it must not re-apply it.
6. **The registry is the authorization surface, so it needed a dependency edge.** 12.10's
   "future LLM automation … receives no privileged mutation route" is enforced by
   `CommandProcessor::check_registered` refusing a command the injected registry does not
   dispatch, before any of it reaches a document. That makes the registry a data structure a
   caller can narrow rather than a decode helper with a warning printed above it. It also
   made `anim_commands → anim_assets` explicit in `Cargo.toml`: 12.4 names only
   `anim_commands → anim_project`, and BUILD_STATE read that chain as reaching `ContentHash`
   legally — which it does for the *type*, via `anim_project`'s re-export — but the envelope
   hash needs `hash_bytes`, and forwarding another crate's *functions* would make
   `anim_project` look like the owner of content hashing, which 11.2 assigns to
   `anim_assets`. Deviation row 11.
7. **The schemas are hand-written artifacts, and the drift test is the security.** Ruled with
   the user: JSON Schema draft 2020-12 files under `schemas/project/` and
   `schemas/commands/`, with acceptance remaining at the typed boundary — a non-canonical
   rational, an uppercase uuid, an unsorted dependency list or a duplicate key time is already
   refused by the constructor that reads it. No validator dependency, because 5.1 names none
   and it would need an ADR for a feature the types already enforce. What makes the artifacts
   mean something instead of decorate is `the_committed_schema_matches_what_the_serializers_write`
   and `the_committed_command_schema_matches_the_encoder`: both compare the declared field set
   against the members actually emitted, so a rename that missed the file fails the gate.
8. **Undo and redo *move* a transaction between stacks; they do not mirror it.** The first
   implementation built the redo entry from the sequence the undo step had just applied, which
   made redo re-apply the inverses; two tests caught it (#32). An entry now means the same
   thing wherever it sits — `forward` is what redo applies, `inverse` is what undo applies
   backwards — so stepping back changes only `resulting_revision`, and the invariant "a
   transaction lives in exactly one history stack" (12.8) becomes the API's shape rather than
   a discipline. The collected inverses still go into the moved entry, so the pairing reflects
   what the run produced rather than what the stored copy asserts.
9. **History is unbounded, deliberately.** An earlier draft capped the undo stack at 4096 with
   a `discarded` counter. The cap was an engine policy 12.1 does not ask for, and what it
   traded away was the guarantee the package exists to provide: the transaction it forgets is
   the one an operator is about to be handed back. A session's history is bounded by the
   transactions the session ran. If this ever becomes a real memory problem the fix is a
   reportable eviction policy with a spec clause behind it, not a silent number in a
   constructor.
10. **Scenes own their curve mutations.** `SceneData::set_track_key` and
    `remove_track_key` exist instead of a `track_mut`, because a curve's sorted-and-unique
    rule and the scene's one-track-per-property rule are the same rule seen from two levels.
    Every scene mutator clones, changes, re-validates and swaps (`change`/`try_change`), so a
    refused command leaves the scene exactly as it was. `commit_if_valid` in
    `anim_commands` is the same discipline at document scale, and it is the only writer.
11. **`save_atomic` uses `StagedWriter`/`publish` plus its own byte comparison, not
    `write_atomic`.** 12.12 requires failures and kills at every persistence boundary, and
    `write_atomic`'s boundaries are inside the call. Staging by hand makes each step an
    observable `Boundary`; the price is `publish`'s length-only post-rename check (#28's
    economy for streamed payloads), so the save re-reads and compares the destination itself.
    For a project file that is a few kilobytes, and "never mixed bytes" is a claim about
    content. ADR-0006's new Recovery-policy section records the reasoning.
12. **Three files spec 12.3 does not list.** `crates/anim_project/tests/persistence.rs`
    (12.12 requires a fault-injection suite alongside the functional one, and the persistence
    clauses cannot live in `anim_commands/tests/atomicity.rs` without reaching across 12.4's
    boundary), `crates/anim_test_support/src/bin/wp03_save.rs` and
    `crates/anim_test_support/tests/wp03_fault_injection.rs` (12.12's "kill test processes at
    each save stage" needs a process that dies; 4.2 already assigns fault injection to that
    crate, and `CARGO_BIN_EXE_wp03_save` makes the child the same build as the parent).
    Deviation row 10.
13. **The kill scenario asserts its own residue.** `expect_residue` requires exactly one
    surviving `.tmp` at the two boundaries between staging and rename, and none elsewhere.
    Without it, a kill that landed after the rename would still satisfy every content
    assertion while proving nothing about interruption — the same class of gap #13 and #20
    describe, where the thing you can see is not the thing that runs.

### #32 — WP03 — G2 result summary, and what the tests caught

**G2 passed 2026-11-06**, self-certified under DECISIONS.md #21. G2 is the joint gate of
WP02 and WP03 (spec 8.3: "Asset verification, atomic commands, save faults, and migrations
pass"), and both halves are now evidenced: 11.12 in full at WP02's commit, and 12.12's
functional suite plus fault-injection suite here. `tools\ci.cmd`: fmt, clippy
`--workspace --all-targets -D warnings`, `test --workspace --locked` all green. Probes were
not re-run — adapter, driver, toolchain and pinned FFmpeg are unchanged, so `-WithProbes`
would re-certify hardware nobody moved.

**The workspace suite is 313 tests**, up from 188: `anim_project` 67 (53 unit + 14 in
`tests/persistence.rs`), `anim_commands` 53 (39 unit + 14 in `tests/atomicity.rs`),
`anim_test_support` 19 (6 unit, 8 `wp00_smoke`, 5 in `tests/wp03_fault_injection.rs`),
`anim_core` 83 and `anim_assets` 91 unchanged. **`Cargo.lock` gained 22 lines and no new
crate**: the additions are dependency edges for `anim_project`, `anim_commands` and
`anim_test_support`, over versions already resolved by WP00–WP02. `dev-dependencies`
`tempfile` and `uuid` were already in the graph.

**Nine defects and wrong assertions, all found by tests or by the scenarios they describe
rather than by review.**

1. `serde_json::Value` does not preserve object member order — its map is a
   `BTreeMap<String, _>`, so members come out alphabetical. The first version of
   `the_json_field_order_is_the_declaration_order_of_spec_12_8` proved the ordering through a
   `Value` and failed, which is what forced the rule to be stated on the `to_vec` path
   instead (ADR-0005 Consequence 7). It immediately mattered twice: `migrate::upgrade_with`
   would have re-serialized a current-version file through a `Value` on every load,
   rewriting bytes for a document that needed nothing done to it and breaking the
   canonical-bytes claim for its own test, and `recovery` needed
   `CompleteCandidate::canonical` so a file in the `Value` spelling is reported as usable-but-
   non-canonical rather than trusted or silently rewritten.
2. **The undo/redo stack bookkeeping was wrong in the way that only an equality test
   catches.** The first implementation built the redo entry's `forward` list from the sequence
   the undo step had just applied — the inverses — so redo faithfully re-applied the undo. Two
   tests failed: `undo_then_redo_restores_the_document_byte_for_byte` and
   `undoing_a_multi_command_batch_reverses_it_and_redo_reapplies_it`. The corrected model is
   in #31.8. A `mirrored()` helper was left in `history.rs` as the relationship *description*,
   with a comment saying the processor must not derive its entry from it.
3. `load_document` reported an out-of-range embedded manifest version as a generic parse
   failure, because `ProjectDocument`'s `Deserialize` validates and serde boxes the error into
   a string. `UnsupportedSchema`'s machine-selectable context (spec 7.4) was gone by the time a
   caller saw it. Fixed by checking both version numbers from the parsed `Value` *before* the
   typed read, in `migrate::load_document_with_chain`.
4. A negative test that was not negative: `non_canonical_bytes` perturbed the file by swapping
   its last two bytes, and the document ends in `}}`, so the "damage" was a no-op and the
   classifier accepted the file happily. Replaced with a whitespace insertion plus an
   assertion on the reported byte offset.
5. `SceneData::remove_node` pre-checked, mutated `self`, then validated — so a document that
   arrived inconsistent could be left half-changed by a removal that reported failure. Moved
   onto the same clone-and-swap path as every other mutator.
6. `expect_residue` exists because the killed-process suite would otherwise pass for the wrong
   reason: if the kill landed after the rename, every content assertion still holds while the
   scenario silently becomes a test of a completed save. Requiring one surviving temporary at
   the two mid-sequence boundaries is what makes the kill observable.
7. The backup negative control was pointed at `before-replacement`, where no backup exists yet,
   because `anim_core` writes the backup inside `publish`, immediately before the rename. The
   control moved to `after-replacement`. Related: the classifier's parse and canonical
   re-serialization rules were **removed** rather than kept, because the "is it one of the two
   byte streams" rule subsumes them and a rule nothing can violate cannot be tested — the
   surviving rules each have a corruption that fires them, including a newer autosave, which
   is what proves the recovery clause is live rather than always agreeing.
8. Two expectations that were arithmetic rather than code: the revision after eight
   apply/undo/redo cycles is `1 + 8*3`, not 17 — undo and redo each commit a new revision,
   which is 12.6's "commits a new revision" and something a test that only looked at state
   would never have noticed — and a fixture that gave every root the same `sibling_order`
   failed for the right rule and the wrong reason, which is why `add_node` derives its order
   from its id.
9. `history::is_empty` disagreed with a test's expectation about a drained stack, and the
   disagreement was the real thing: stepping back through every transaction empties the stacks
   but must **not** erase the memory that an id was accepted, or a retried batch after an undo
   would apply twice. The test was corrected to assert both halves.

**Carried forward, not hidden.** Four public helpers I added to `anim_commands` in this
package had no caller — `CommandEnvelope::check_form`, `envelope::current_payload_version`,
`Registry::payload_schemas` and `Handler::schema` — and were removed the same day in a
follow-up commit after he asked what had gone wrong. `AGENTS.md` and the system rules both
say not to build for a hypothetical caller, and BUILD_STATE risk 16 makes the same point
about unexercised capability: an accessor nobody calls is surface that has to be kept
correct forever and proven by nothing. One doc citation in the removed set pointed at a test
name that does not exist (`closed_set_is_the_inverse_of_itself`), corrected to the real one.
Audited afterwards by script: every backticked identifier in a WP03 doc comment resolves to
something defined in the workspace — the same class of error #25's citation audit found
eleven of.

**Carried forward, not hidden.** The command set will grow, and 12.12's
"rollback from every command index" will need re-running over the new kinds rather than
trusting this package's fixture batch; `every_command_kind_has_an_inverse_that_closes` is the
test to extend. `schemas/export/` (spec 4.1, 12.3 lists only `project/` and `commands/` here)
is still unauthored and is WP09's. Autosave *policy* — when one fires, how many are kept — is
not in spec 12 and is not invented here; 12.7's recovery rules are implemented, and WP10's
session owns the cadence. Spec 12.7's "attempt directory flushing through a backup-semantics
handle" is `DirectoryFlush::{Flushed, Unsupported}` and BUILD_STATE risk 10, unchanged: which
branch this machine takes is still asserted by no test, and what is certified is the file bytes
and the rename.

---

## 2026-11-06 (WP04)

### #33 — WP04 — the four deferred questions, settled before `evaluator.rs` was written

`BUILD_STATE.md` said WP04's first decision must be put to the user and not settled inside
the evaluator. It was, as one bundled four-question dialog, after a spec-wide survey for
which later packages name real curve types. The survey result is the reason three of the
answers are what they are, so it is recorded first.

**Survey.** No section of the spec enumerates easing variants — not once in 5 400 lines.
Curve types are *named* by later packages and never *defined* by any of them:
`Curve<Rational>` (30.8), `SpeedRampCurve` (32.8), `EaseKind` (41.8, never enumerated),
`SetVfxIntensityCurve` (29.5). The named algorithms are centripetal Catmull-Rom and
pre/post extrapolation (30.7, 30.12), "a cubic or table-based ramp" (32.7), "fixed easing
tables" (32.5). Bézier, per-key in/out handles, `EaseIn`/`EaseOut`/`SmoothStep` and
`prescale` do not appear anywhere. **The correction that came with it:** the extrapolation
modes are WP15's, spec section 30 — not WP05's, and not spec section 15, which is WP06's
renderer. `crates/anim_project/src/property.rs` cited "spec 15" for them and that comment is
now rewritten; a reader following the old citation would have looked for extrapolation in
the render graph.

**Ruling 1 — events arrive as raw project data.** `EventDefinition` in a new
`crates/anim_project/src/event.rs`, held by `SceneData` as a `BTreeMap` keyed by
`EventDefinitionId`. Chosen over "extend `Marker`" and over "synthesize from tracks". The
decisive evidence was 28.8: WP13 carries `effect_cue: Option<EventDefinitionId>` and
`audio_cue: Option<EventDefinitionId>` — later packages reference an authored event
identity, so the identity needs something to reference. `Marker` stays an annotation with no
id and no consequences; 13.10 gives events to `anim_sim`, `anim_audio` and `anim_render`,
and fusing the two would make every note on a timing sheet a side effect. `source` is
`NodeId`, not `Option<NodeId>`, because 13.8 types `SceneEvent.source_id` as a plain
`NodeId`.

**Ruling 2 — no animation-layer type, and the claim that motivated it was wrong.**
`BUILD_STATE.md` pointed at "30's `(priority, MotionLayerId)`" as the model for 13.5 step 3.
Spec 30 is WP15 and its `MotionLayer` is *camera* motion: its `MotionKind` is `Pan`, `Tilt`,
`Roll`, `Truck`, `Pedestal`, `Dolly`, `Crane`, `Rail`, `Orbit`, `Zoom`, `WhipPan`, `Shake`,
`Handheld`. WP12's pose layers (27.5, "blend pose layers using per-bone masks") have no data
structure at all. There is no scene-animation-layer type anywhere in the document, so 13.5
step 3 has nothing to be implemented *from*. Deferred, with the seam named at
`anim_scene::animation::blend_layers`, because 13.12's G3 list contains no layer criterion
and a layer type invented here moves `PROJECT_SCHEMA_VERSION` a second time for a shape WP12
or WP15 would replace.

**Ruling 3 — `EventOccurrenceId` goes in `anim_core::id`, as the eighth typed id.**
Decisive fact, not taste: `typed_id!` is a private `macro_rules!` at `id.rs:50`, whose own
doc comment states why the bodies must be one implementation. Any other home means either
exporting the macro — which changes `anim_core` regardless — or hand-writing a second copy
of the strict parse and the 16-byte `Ord` in the tie-break term of the event ordering.
Spec 10.8 lists seven ids; this is recorded as a deviation, and the file's module header now
says eight and names 13.7 and 13.8 as the source.

**Ruling 4 — one parameterized easing variant, and a table rather than a Bézier.**
`Easing::Curve(EasingCurve)`, where `EasingCurve` holds normalized outputs at equally spaced
inputs, first pinned to `0/1`, last pinned to `1/1`, length in `[2, 65]`, interior samples
allowed to overshoot. Why this shape:

- It makes 13.11's `InvalidEasing` and 13.7's "invalid easing parameters" *reachable*, which
  a two-variant enum could not be, and that was the whole point of the ruling.
- The spec sanctions it twice — 32.7 "a cubic or table-based ramp", 32.5 "fixed easing
  tables" — and never once names a curve family.
- Exactness. The lookup is rational at every step: locate the segment by the table's own
  spacing rule, take the exact fraction inside it, lerp the two samples. A Bézier needs a
  root solve, which is either an iterative float method or a polynomial whose denominators
  cube the frame-position denominators inside `i128`.
- The endpoint pinning is what keeps 13.7's "At exact key time return that key" true under
  authored data. Without it a curve can move a key value, and one property would have two
  values at one instant depending on how the lookup rounded.
- `EASING_ALGORITHM_VERSION = 1` is a separate number, hashed into a snapshot's pre-image and
  **not** into the document's. ADR-0005 Decision 4's split, and 42.7's "changing summation
  technique is an algorithm version change" is the precedent. Authored data does not change
  when the reading of it changes; two snapshots that differ must not share a hash.
- Rejected: `EaseIn`/`EaseOut`/`SmoothStep` named variants. The spec never lists any, so the
  list would be invented, and it would still leave 13.7's invalid-*parameter* clause
  unproducible.

**Ruling 5 (consequence of 4), and the one that removed code.** `EasingCurve`'s constructor
validates and its `Deserialize` runs the constructor, so the evaluator's planned re-check of
the endpoints was **unreachable**. An earlier revision of `properties.rs` carried that check
plus a test that fed a bad table through JSON to reach it; the test failed because the loader
refused first, which is the correct behaviour. Both were deleted. 13.11's easing failure
remains reachable where the type genuinely cannot see it — the two halves are authored in two
different objects, a key's value type and that key's easing — so `interpolate` raises it for a
non-`Step` easing on a bool, a label or an asset, and that is tested.

**Ruling 6 (consequence of 1), the schema bump done properly.** `PROJECT_SCHEMA_VERSION` 1
to 2, one real step in `migrate::chain()` (`V1_TO_V2`, which adds an empty `events` map per
scene and is idempotent), the committed `tests/fixtures/project_v1.anim.json`, and
`schemas/project/v2.json` generated from v1 rather than retyped, with v1 kept because the
files it describes still exist. `DOCUMENT_DOMAIN` stays `anim_engine/document/v1`: appending
a section to an unchanged framing is a field addition, not a byte-layout change. No
`serde(default)` on `events`, which is what forces every version 1 load through the chain
rather than quietly reading an old file as a new one.

**Ruling 7, not asked and therefore recorded here.** 13.12 tests "camera cuts" and "audio cue
identity", and there is no authored camera or audio-cue structure — the same class of gap as
the layers, one the four questions did not cover. Resolved minimally: an `EvaluatedCamera` is
a node that a `CameraCut` event names, and an `AudioCue` is an `AudioCue` event carrying its
source node's evaluated asset. Both come from the event collection ruling 1 added, so no
third authored collection was created. WP15's rig (30.8) will extend these types.

### #34 — WP04 — G3 result summary, and what the tests caught

G3 is WP04 and it is self-certifiable under #21, so this is the record that says it passed.
`tools\ci.cmd` on 2026-11-06: **fmt PASS, clippy PASS, test PASS** — 431 tests across the
workspace, against 313 before this package. 13.12's thirteen clauses are each named by a test
in `crates/anim_scene/tests/evaluation.rs` except "identical canonical snapshots and event
logs from two fresh processes", which is
`crates/anim_test_support/tests/wp04_isolation.rs` running the `wp04_snapshot` scenario twice.

**Four defects the tests found, all of them introduced by this package, all found before
this file was written.**

1. **`f64::sin_cos()` returns `(sin, cos)`, and it was destructured as `(cos, sin)`.** Every
   node that authored no rotation got a quarter turn: the identity became
   `[[0, -1, 0], [1, 0, 0], [0, 0, 1]]`. The failure was finite and plausible, which is why
   the comment now names the order at the call site. Caught by
   `an_untouched_node_is_visible_opaque_and_at_the_origin`, which asserts that an untouched
   node's matrix *is* the identity — the only test in the crate that could see it.
2. **A child's local matrix was composed twice.** `hierarchy::visit` returns world matrices,
   and `evaluate_node` composed the authored local onto that result instead of onto the
   parent's, so x = 1 under a parent at x = 10 became 12. `evaluate_node` now takes the
   parent's matrix, and the test asserts both the number and that the two passes agree.
3. **Four hand-computed expectations were wrong while the code was right**, which is the case
   the oracle rule exists for and the reason it was kept rather than relaxed. One and a half
   frames of a `0..4` interval is `u = 3/8`, not `3/4`; a quarter of the way into a
   four-frame rise is `1/16`, not `1/4`; four frames at `30000/1001` is `1001/7500` and a
   digit was mistyped as `40004/30000`; and the pair mirrored about an interval's midpoint is
   `1.5` and `2.5`, not `1.5` and `3.5`. Each was corrected against the specification's
   arithmetic, never against the implementation's output.
4. **Five `unwrap()`s on `Option` returns and two `Arc<[T]>`-against-`T` comparisons.**
   `ProjectDocument::insert_scene` hands back what it displaced, so a fresh insert returns
   `None` and the fixture panicked in a way that read like an evaluation failure. One of them
   sat in the negative-control test, where a panic would have hidden the fact that the
   scenario could not tell two documents apart.

**Three more, caught by reading the diff rather than by a failing test** — which is worth
recording separately, because a defect this package's own suite cannot find is a defect the
next package's suite will not find either:

5. **`SceneError::InvalidEasing` and `InvalidPropertyKeys` were leaving `anim_scene` with
   blank `node` and `property` fields.** `properties.rs` is about values, and a value does
   not know whose it is, so every refusal raised there named nothing while looking perfectly
   well-formed in the type. `PropertySources::value` is the one place that knows both, and it
   now calls `properties::with_subject` on the way out. `a_key_list_refusal_names_the_node_and
   _the_property_it_broke` is the test that would have caught it had it been written first.
6. **The evaluator reported a missing *node* as `SceneError::MissingScene`.** That is not a
   smaller version of the right error, it is a wrong category: 13.8's `EvaluatedNode` names a
   `NodeId`, and 13.11 has no variant meaning "the hierarchy produced an id the scene does
   not contain" because that is not a document defect but a bug in `hierarchy::visit`. Fixed
   structurally rather than by inventing a tenth variant — `WorldEntry` now carries the index
   of the node it came from, so the pairing is total and the impossible branch does not
   exist. `entry.parent`'s world lookup keeps a report, and it is `HierarchyCycle`, which is
   what an unvisited parent actually means here; it replaced an `unwrap_or(IDENTITY)` that
   would have drawn a child under a parent the hierarchy never placed.
7. **Four public items were declared against consumers that do not exist** —
   `evaluator::ActiveCamera`, `camera::no_cameras`, `animation::unauthored_local`,
   `SceneError::is_arithmetic` — and `SceneError::EventOrderOverflow` was being described in
   this crate's own module comment as reachable when nothing can reach it. The first four are
   deleted. The claim about the fifth is corrected in `lib.rs`, in `ADR-0007` consequence 6,
   in risk 19 and in deviation 14, because *a declared error variant reads like covered error
   handling to the next session*, and that is the same class of false assurance as a stub that
   compiles.


**What the package removed.** An `ActiveCamera` alias in `evaluator.rs`, a `no_cameras()`
constant in `camera.rs`, an `unauthored_local()` re-export in `animation.rs`, and
`SceneError::is_arithmetic`. Each was written against a consumer that turned out not to
exist; deleting them is the cheaper correction than leaving a public surface with no caller.

**The residual G3 does not cover.** `SceneError::EventOrderOverflow` is declared because
13.11 lists it, and **no test reaches it**: with one authored position per definition, the
ordering path's only arithmetic is a constant zero index and a literal domain tag. Eight of
the nine variants have a producing test; this is the ninth. It becomes reachable when WP14
gives an event something to repeat. `BUILD_STATE.md` carries it as an open item so that a
later session does not read a declared variant as coverage.

**Also owed, also delivered:** `docs/adr/ADR-0007-event-phases-occurrence-identity-and-tie-ordering.md`,
which spec 24's register closes at WP04 — the occurrence-identity derivation, the
frame-ownership rule and the tie ordering, each with its alternatives and its schema impact.

## Test command

**Use the gate. `tools\ci.cmd` is the single entry point** (DECISIONS.md #19) and it
resolves cargo itself, so it works from a fresh shell:

```
tools\ci.cmd                 # fmt + clippy + test --locked
tools\ci.cmd -WithProbes     # + the eight platform probe scenarios
tools\ci.cmd -Only clippy    # one step while iterating
```

The equivalent raw commands, if you would rather run them by hand — from Git Bash,
prepend the toolchain that `setx` persisted but a live shell does not have:

```
export PATH="/d/cargo/bin:$PATH"
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Then the five probes, which are the part of G0 that touches hardware and are not run
by `cargo test` (a test suite that needs a discrete GPU cannot run in CI or on a
machine whose dGPU is power-gated):

```
cargo run -p anim_test_support --bin wp00_gpu
cargo run -p anim_test_support --bin wp00_adapter
cargo run -p anim_test_support --bin wp00_adapter -- --force-unavailable   # expect exit 1
cargo run -p anim_test_support --bin wp00_replace -- --fault before-commit
cargo run -p anim_test_support --bin wp00_replace -- --fault after-commit
cargo run -p anim_test_support --bin wp00_replace -- --fault none
cargo run -p anim_test_support --bin wp00_media
cargo run -p anim_test_support --bin wp00_export
```

Each writes `probe-out/<capability>.json` and exits nonzero on an unmet requirement
(spec 9.6). The certified set from 2026-11-05 is copied to
`docs/adr/evidence/wp00/`; `probe-out/` itself is git-ignored.

`wp00_pen` is cancelled — see DECISIONS.md #8 and `ADR-0015-native-ink-routing-CANCELLED.md`.

---

## 2026-11-06 (WP05)

### #35 — WP05 — the two-part first decision, taken before code, and what it obliged

WP05's prerequisite did not exist: spec 14.2 names `ProjectDocument.simulation.ticks_per_frame`
and spec 12.8 declares six document fields, none of them a simulation section. That is the
package's first decision, it changes the saved format, and it was put to the user as one
bundled `AskUserQuestion` with two parts **before any code was written**, in the same form as
#33's four. Both recommended options were taken.

1. **`ticks_per_frame` arrives as raw project data.** `PROJECT_SCHEMA_VERSION` 2 → 3, one real
   `migrate::chain()` step (`V2_TO_V3`), a committed version-2 fixture,
   `schemas/project/v3.json`, and `SimulationSettings` in `crates/anim_project/src/simulation.rs`
   beside `event.rs`.
   **Rejected: a runtime value supplied by the caller.** 14.12's own criteria settle it —
   "changing checkpoint cadence or warm-up chunk size cannot change the exported state" and
   "compare sequential execution with seeks to every tick" are both statements that one saved
   revision replays to one state, and a per-session rate makes that false by construction.
   `scheduled_tick = ceil(t * simulation_rate)` is derived from the same number, so an event's
   tick placement and every subframe spawn age move with it. Spec 40's "the tick rate is
   project-configured, not a global 1200 Hz constant" agrees, and the memory note that spec 30's
   `MotionLayer` is camera motion rather than scene animation was correct and is unchanged.
2. **The random-stream seed lives in the same section**, authored and persisted. 14.7 derives
   every `StreamKey` from "project seed" and #23 established there is no entropy source in the
   graph, so a seed generated at open would make a saved project's streams unreproducible from
   its own bytes and 14.11 forbids substituting one silently.
   **Rejected: a seed derived from the document digest** — no authoring needed, but every edit
   re-derives it and shifts every stream in the project, which breaks 14.12's per-stream
   stability and makes warm-up replay depend on unrelated content changes.

**What those two answers obliged, decided in the same shape (spec gap → one place to record
it), with the reasoning in `ADR-0009`:**

3. **The frame rate is not copied into the section.** It stays `ProjectDocument.frame_rate`.
   A second copy would give one timeline two rate bindings that can disagree, which is the same
   objection `event.rs` raises about a cue's asset. The clock therefore gets built by
   `anim_sim`'s own `SimSettings::new(document.frame_rate, &document.simulation)`, and 14.9's
   `initial_state(snapshot, settings)` parameter is named `SimSettings` here while
   `SimulationSettings` names the persisted section. Two types, one name each, no duplicated
   value; recorded as a deviation rather than quietly harmonised.
4. **`INITIAL_PROJECT_SEED` is all zero, and that is a value, not 14.11's substitution.**
   Substitution would be choosing something else where the data named a seed; a document that
   has never been saved names nothing, and zero is the stable answer two fresh processes agree
   on. It is visible in the file and changed by authoring.
5. **`solver_iterations` is an engine constant carried in the state**, not a persisted field.
   14.6 says "a **fixed** number of solver iterations" in the same paragraph that says the step
   duration comes from exact configuration — the two are deliberately different in kind.
   14.11's `InvalidSolverCount` keeps a producer because the count lives in `SimState`, enters
   the digest, and is re-checked when a checkpoint is restored. The argument for revisiting it
   is in `DEFAULT_SOLVER_ITERATIONS`'s doc comment.
6. **`Spring`'s fields are WP05's**, because 14.8 puts `Vec<Spring>` in `SimState`, 14.7 asks
   the checkpoint encoder to hash it in stable ID order, and the type has no field list anywhere
   in 5,438 lines — spec 29 (WP14) defines `SecondaryChainComponent`/`ChainParams`/`ChainNode`
   and never mentions `Spring`. A spring is an edge from a kinematic anchor node to a dynamic
   body, with `rest_length` measured from the snapshot at initialization. Bodies are the nodes
   that have a parent, because `EvaluatedNode.parent` is the only chain structure a snapshot
   contains — `SimulationInput` deliberately has no parent field. **Consequence: any authored
   node hierarchy is a spring chain in WP05.** It does not change what the scene evaluator
   reports, because 14.8's `SimulationFrame` is a separate extraction; WP12 and WP14 will narrow
   the set when they add the component data, and that is #31.1's minimal-by-ruling pattern
   extended, twice now.
7. **A spawn is an `EventKind::Signal` on a source node.** No section authors an emitter for
   WP05, `Signal` is "a signal for a consumer to react to" and is the one kind with no other
   consumer, and the emitter identity is then 13.7's third sort term rather than a second
   binding that can disagree with it. Everything the burst needs beyond that is drawn from the
   streams, over constants inside `SIMULATION_MODEL_VERSION`.
8. **`SimState` gained exactly one field beyond 14.8's list: `presentation`, and it is excluded
   from the canonical encoding.** Without it, 14.9's `SimulationMode` parameter would control
   nothing, and 14.9 states it controls external side effects. It is not hashed because 14.12
   requires warm-up and sequential execution to agree at every tick, and a ledger of what a
   viewer was told cannot be part of what the world is; `restore_checkpoint` clears it, because
   a restored run has announced nothing. `a_ledger_effect_is_not_part_of_the_digest` asserts
   the exclusion on bytes.
9. **`crates/anim_sim/src/context.rs` is a twelfth file**, and `crates/anim_headless` grew a
   library target next to its binary. 14.3 lists eleven files and locates neither
   `SimulationContext` nor `SimulationInputFrame`, which 14.9 defines; 14.12's fresh-process
   clause needs a process, `CARGO_BIN_EXE_` names only binaries in the same package, so the
   child has to reach the reporting code as a library. Both are departures from a listing, not
   from a rule.
10. **`seek` and `prepare_export` take `&mut SimulationContext` where 14.9 writes `&`.** The
    context owns WP04's `SnapshotCache`, which is owned-and-mutable rather than locked because
    `clippy.toml` denies `Mutex`/`RwLock` workspace-wide and #22 prefers an owned invariant to
    interior mutability. A shared borrow cannot fill that cache, so the literal signature would
    need either a lock or a cache that never fills.
11. **`anim_sim` depends on `anim_project`,** which 14.4 does not list. Its MUST NOT clause —
    no `anim_commands`, no renderer, no audio device, no editor, no wall clock — holds;
    `anim_scene` re-exports no `anim_project` or `anim_core` type at all, and 14.9's own
    `SimulationContext` "owns immutable project/evaluation inputs". Same reasoning as
    deviation row 11's direct `anim_assets` edge.
12. **`V1_TO_V2` now writes `2`, not `PROJECT_SCHEMA_VERSION`.** WP04 wrote the constant while
    version 2 was the end of the chain, so the two were indistinguishable. With three versions,
    a step that writes this build's number leaves an intermediate document claiming a version it
    has not passed through. `V0_TO_V1`'s comment already stated the right rule; the production
    step now follows it, and `TEST_CHAIN` runs three steps with two real ones.
13. **`tools/make_v3_schema.py` is versioned where WP04's generator was not.** The schema files
    are inputs — `.gitignore` says fixtures are inputs, not output — and WP04's generator lived
    in git-ignored `.tools/` and was deleted after use, so "generated from the previous version,
    not retyped" was a claim nobody could repeat. A retyped schema is how a JSON pattern that
    disagrees with the Rust decoder enters the repository.

**Addendum the spec owes:** yes, in five places, none of which changes what the engine does.
12.8's field list lacks `simulation`; 14.4's dependency list lacks `anim_project`; 14.8 names
`Spring`, `SimulationFrame`, `SimulationInputFrame`, `SimulationContext`, `SimulationMode` and
`AlgorithmVersions` without fields; 14.9 types the key as `[u64; 2]` while Philox4x32's key is
64 bits (ADR-0008 Decision 3 is this project's resolution and the register names that ADR for
exactly that question); and no section defines `SimulationSettings` at all.

### #36 — WP05 — G4 result summary, and what the tests caught

G4 is WP05 and it is self-certifiable under #21, so this is the record that says it passed.
`tools\ci.cmd` on 2026-11-06: **fmt PASS, clippy PASS, test PASS** — 584 tests across the
workspace, against 431 before this package. 14.12's eight clauses are each named by a test in
`crates/anim_sim/tests/determinism.rs` (19 tests) except the fresh-process half, which is
`crates/anim_test_support/tests/wp05_isolation.rs` (7 tests) running
`src/bin/wp05_replay.rs` twice at 10,000 frames and comparing all 40,001 tick hashes line by
line as well as byte-for-byte as text.

**The external authority is real, and it is the part a project cannot manufacture.**
`crates/anim_sim/tests/fixtures/philox4x32_10_test_vectors.json` carries 55 published
known-answer rows from D. E. Shaw Research's Random123 at commit `9545ff64…`, with the SHA-256
of each fetched file and each row's verbatim source line recorded in the fixture, and all 55
reproduce exactly. Retrieval also settled a question this package could not have answered
itself: Random123 publishes the Philox4x32 key as **two 32-bit words**, not the 128 bits 14.7
derives, so a literal implementation discards half the derived key silently. ADR-0008 Decision 3
folds the 128 bits into the 64 the primitive reads; the measured discrimination — seven rounds
reproduces 0 of 55, an eleven-round variant 0, swapped multipliers 0, the wrong key packing 14
— is why the fixture is worth its bytes.

**Six defects, all introduced by this package, all found before this file was written.**

1. **`random_u32` refused any `stream_version` that differed from
   `PHILOX_ALGORITHM_VERSION`.** The first version bump of a stream therefore did not change
   its numbers, it made the stream **unusable** — `UnsupportedAlgorithm` on every draw. That is
   a design defect found by the gate's own last-but-one clause, not by review: 14.12's
   "changing one stream version must leave unrelated stream outputs unchanged" cannot be tested
   against a stream that will not draw at all. Fixed by taking the gate out of the draw, making
   `stream_version` purely an address term, moving 14.11's "unsupported random algorithm"
   refusal to `restore_checkpoint` where a version actually arrives from persisted data, and
   **deleting** `RandomError::UnsupportedAlgorithm` rather than keeping a variant nothing
   produces — risk 19's shape, applied to my own code the same week it was written down.
2. **`springs::solve` no-oped.** It looked for each spring's anchor among `SimState.bodies`,
   and anchors are kinematic — they live in the snapshot, not in the body list — so every pass
   `continue`d and the fixed-iteration stage ran ten times doing nothing. Caught while reading
   the draft, and now pinned by `the_solver_projection_lands_on_the_rest_length_and_never_moves
   _the_anchor`, which measures the distance after a pass and asserts the anchor did not move.
   A stage that exists, is called, is ordered correctly and computes nothing is the most
   expensive kind of half-implementation, because every downstream test still passes.
3. **`accumulate_forces` located a body with `position(...).unwrap_or(len - 1)`.** On a spring
   whose body was missing, that accumulates force onto the **last body in the list** — finite,
   plausible, wrong, and silent. Replaced with one `BTreeMap<NodeId, usize>` index built per
   stage, which is also what removes an `O(n²)` scan from a chain.
4. **A newborn's subframe age was integrated as a full `dt`.** `spawn_for_event` was passing the
   step duration where 14.7 says to pass `age = time_of_tick(k) − t`. Working the identity out
   then produced the rule the module comment now states: a newborn legitimately takes **both**
   the age and the tick's `dt`, in that order, and they sum to `time_of_tick(k+1) − t` — exactly
   how long a particle born at `t` has lived at tick `k+1`. The alternative reading, exempting
   the newborn from its first `dt`, leaves anything born on a tick boundary lagging the authored
   timeline by one tick for its whole life.
5. **Nine hand-computed expectations were wrong while the code was right.** This is #33.3's
   class, and it is the reason the rule survived into a fifth package: 3.5 frames at four ticks
   per frame is tick **14**, not 84 (I multiplied by the frame rate instead of the tick rate);
   the Philox counter word for a draw index is `[…, draw, 0]` and I asserted `[0,1,2,3]`; the
   `age_ticks` field sits at byte **76** of a particle's stream, not 64; a cadence of 7 over
   ticks 0..40 with capacity 4 keeps `14,21,28,35`, not `7,21,28,35`; `prepare_export` stops on
   `range.start × N` so a tick-57 target is **56** for that route, and the mid-frame case is now
   asserted on its own rather than folded into a comparison that would have failed;
   `FramePos(−1, 1/2)` is exactly on tick −2 at four ticks per frame, so its age is **0** and the
   non-zero sub-frame case needed `1/3`; two particles sharing an id do **not** hash the same as
   one, because the count is encoded — the claim was rewritten as the weaker true one, with a
   self-consistent forged entry as the case only `validate` catches; `advance_lifetimes`
   increments ages, so a "never held the expired particle" comparison has to advance those ages
   too; and a stability assertion compared `before[0]` where the survivor's index was simply
   wherever the sorted order put it. Every correction was made against the specification's
   arithmetic, none against the implementation's output.
6. **Four lint-shaped defects that the gate refuses, and one it would not have caught.**
   `#[must_use]` on functions returning `Result` (denied since clippy 1.99, and every
   Result-returning function in `anim_core`/`anim_scene`/`anim_project` carries none, so the
   house rule is to let the type say it); `CanonicalBytes::with_domain(…).expect(…)` in three
   infallible-signature id derivations, replaced by the `new()` + `text(domain)` pair that
   `with_domain` is defined as rather than by an `unwrap` in production scope;
   `SimState::empty_for_test` sitting on the impl block where `unwrap_used` is denied, moved
   into a `#[cfg(test)] pub(crate) mod test_support`; and `clippy::large_enum_variant` firing on
   `recovery::CandidateState` because the 36-byte `simulation` section tipped the ratio over the
   default threshold — allowed with a reason on the enum rather than satisfied by boxing a
   variant of a public recovery API, which is the same judgement the workspace already makes by
   allowing `result_large_err` at the top level.

**What WP06 and later packages need from this one, in one place.**

- `BUILD_STATE.md`'s Current Work Package row is the resume point. `anim_sim` is the only
  consumer of `anim_scene`'s snapshots, `SimulationContext` is the only thing that evaluates on
  the simulation path, and nothing in `anim_sim` takes `&mut` to `anim_project` types.
- **Read DECISIONS.md #18 before designing adapter selection or telemetry.** Nothing enforces
  it, and WP06 is where that stops being theoretical.
- No command authors simulation settings yet, and none authors an event: 12.9's set is WP03's
  twelve, spec 13 and spec 14 add none. WP10 owes `PlaceEvent`- **and**
  `SetSimulationSettings`-shaped commands, must extend
  `every_command_kind_has_an_inverse_that_closes` rather than trust it forward, and must be
  handed a narrowed `Registry` (item 16).
- `SimError::TickOverflow`'s `"schedule size"` operation is unreachable at any event count a
  document can hold; it is declared because the arithmetic path is shared. Stated in
  `ADR-0009`'s outstanding list rather than given a fake call site.
- The particle set is unbounded in WP05. 29's truncation rule belongs to WP14/WP09, and
  18.8's `ExportSettings` has no field for it either — a contradiction in the spec, raised here
  rather than resolved by guessing.
- Property paths still come from WP04's registry in `animation.rs`; the simulation reads none of
  them, and a WP10 picker should offer those names rather than free text (item 21).

### #37 — WP05 follow-up — a dead-surface sweep, and two claims that survived a green gate

Asked after the commit whether anything remained, I swept the new code for public items with no
caller rather than answering from memory. Four findings, three of them real. `git log` reference:
this entry and its commit sit directly on top of `1c07bb9`.

1. **`assert_caching_differed` was dead in practice and its module header claimed otherwise.**
   The helper was `pub`, had its own unit test, and the isolation test never called it — while
   `crates/anim_test_support/src/determinism.rs`'s header said the assertion "is therefore
   called alongside every comparison rather than being assumed." Fixed by making the claim true
   rather than by deleting the sentence: `anim_headless::replay::report` now writes a
   `checkpoints_stored` line, and
   `a_checkpointed_process_matches_an_uncheckpointed_one` reads it out of each child's report and
   calls the helper. **Why the report line and not an in-process count:** the whole point of the
   precondition is that the two *processes* cached differently, and a number computed in the
   parent describes a cache the comparison never looked at. It is deliberately **not** an
   identity field — two configurations that agree on every hash must still be allowed to disagree
   here, which is the property being asserted.
2. **`in_process_hashes` had zero callers anywhere.** Written for a parent-versus-child check
   that I then implemented directly with `sequential_hashes`. Deleted.
3. **`SimulationInputFrame.position` and `.time` were never read.** Both are pure functions of
   `(clock, tick)` through `position_of_tick`/`time_of_tick`, so carrying them was a second place
   for one instant to be wrong — the same objection that kept `frame_rate` out of
   `ProjectDocument.simulation` (#35 ruling 3) and that `event.rs` raises about a cue's asset.
   Removed, and the two assertions that had read them now ask the clock. 14.8 leaves the type
   undefined, so narrowing it is still this project's call.
4. **`particle_stream_key` and `stream_subkeys` had already gone** when `particles.rs` was
   rewritten mid-package, so there was no other uncalled public surface from WP05. Worth
   recording as the null result: the repo made a whole follow-up commit for this in WP03
   (`bf33469`), and a repeat was the thing being checked for.

**Then the sweep turned up two more, both of the same kind, and neither reachable by a test.**

- `determinism.rs`'s module header linked **`assert_same_state_hashes`, a function that has never
  existed** — it was renamed to `assert_same_run` while the package was in flight and the link was
  left behind. Intra-doc links are only checked by `cargo doc`, which the gate does not run.
- `determinism.rs`'s own `sample_report` fixture had **drifted from the real writer**: once
  `report()` emitted a new line, five of the module's tests failed immediately, which is the good
  outcome. The bad outcome is that the fixture was already not what the writer produced before
  this change, and nothing noticed until a second field forced the comparison. Both are now
  documented at the fixture.

**The generalisable part, and the reason this is an entry rather than a silent amend.** A green
gate proves the compiled assertions hold. It says nothing about a doc comment's truth, about a
`///` link's target, or about a public function nobody calls — and a header sentence describing a
precondition as enforced is exactly as misleading to the next session as an unreachable error
variant is (risk 19), possibly more so, because it reads like the test suite covers it.
`stream_version`'s gate (#36.1) was caught by a test; these three were not catchable by one, which
is why the sweep is now part of how a package closes here.

**Also added, per the operator's choice:**
`a_global_lever_moves_every_stream_where_a_per_stream_lever_moves_exactly_one`. The previous
contrast was weak in isolation — recomputing a pure function and comparing it to itself. This
turns the two knobs against each other where both are observable: the global lever is the project
seed (one of the five `StreamKey` terms, and the only global one reachable at runtime —
`PHILOX_ALGORITHM_VERSION` is a compile-time constant, and its runtime carrier
`AlgorithmVersions::philox` is refused at `restore_checkpoint`, which that test already covers).
Result: one seed byte moves **all 18** (entity × channel) streams; one `stream_version` bump moves
**exactly one**, named. The two instruments are now demonstrably different, which is what G4's
last clause is actually leaning on.

Workspace test count 584 → **585**. `tools\ci.cmd` green again: fmt, clippy `-D warnings`,
`test --workspace --locked`. No behaviour of a committed package changed: nothing that a caller
could observe through `step_once`, `seek`, `prepare_export` or a state digest is different, and
the 10,000-frame two-process comparison still passes line by line.

### #38 — environment — dGPU-only mode: what it fixed, what it did not, and one correction to #18

The operator switched this machine to discrete-only graphics — **Armoury Crate → GPU Modes →
Ultimate** on the FX607VBR — on 2026-11-06. That is exactly the free input #18's residual asked
someone to check. Measured the
same day with `wp00_adapter` and `wp00_gpu` (both PASS, exit 0) plus `Win32_VideoController`,
`Get-PnpDevice -Class Display` and `vulkaninfo --summary`. Fresh output is in `probe-out/`
(git-ignored, regenerated); **no committed G0 evidence was rewritten.**

**What it fixed.**
- NVIDIA now correlates at **DXGI adapter #0**; the committed hybrid evidence says **#1**.
- Both probes resolve on **attempt 1**. Hazard 2 (`CM_PROB_PHANTOM`, power-gating) did not
  appear in either run.
- Consequence: BUILD_STATE's WP00 row claim "ordinal 0 is the iGPU here" is **mode-dependent,
  not a board fact**. It is now false in this configuration, which is the proof that it must
  never be a selection key in either configuration.

**What it did not fix — the correction to #18's residual.** `Win32_VideoController` and
`Get-PnpDevice` still list `Intel(R) Graphics` (`PCI\VEN_8086&DEV_A7AB…`, driver 32.0.101.6314,
Status **OK**), and `vulkaninfo --summary` still enumerates it as
`PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU` (api 1.3.297, driver 101.6314). Measured alongside: NVIDIA
now owns the desktop at 1920x1200 @144 Hz — the recorded mode, unchanged — while Intel reports **no
active resolution**. The iGPU has no output and is still a fully selectable Vulkan/DXGI device.
dGPU-only moves the panel and reorders enumeration; it does **not** remove the adapter from
Vulkan or DXGI. So the
wrong-GPU class survives the setting, #18 decisions 1–3 stand unchanged (PCI bus id + adapter
name + driver revalidation, the 3892 MiB `DedicatedVideoMemory` floor — re-measured, identical —
and the WP10 startup assertion), and decision 5's silent-degradation path is still live. **Read
that residual as answered negatively.**

**One asymmetry that would have mis-led WP06.** `device_pci_bus_id` is `None` on
`wp00_adapter`'s `AdapterIdentity` (from `wgpu::AdapterInfo`) and `Some("0000:01:00.0")` on
`wp00_gpu`'s (from ash via `VK_EXT_pci_bus_info`). The `None` is **pre-existing** — byte-identical
in the committed hybrid evidence — so it is a provenance difference, not a mode effect. #18's
"require non-empty and hard-fail" is correct but implementable only on the ash path: a WP06 that
reads `wgpu::AdapterInfo::device_pci_bus_id` and hard-fails on empty hard-fails on this machine
in *both* modes. **The PCI key must come from `VK_EXT_pci_bus_info`, not from `AdapterInfo`.**

> **CORRECTED 2026-11-06 by #40 — the two provenances above are swapped.** Measured from the code
> and re-confirmed by running both probes in Standard the same day: `Some("0000:01:00.0")` on
> `wp00_gpu` comes from **`wgpu::AdapterInfo::device_pci_bus_id`** (`wp00_gpu.rs:207`, populated by
> wgpu-hal from `VK_EXT_pci_bus_info`), and the `None` on `wp00_adapter` is a **hard-coded `None`**
> at `wp00_adapter.rs:342`, whose own comment says that probe never queries the extension through
> ash. `ash` appears nowhere in `wp00_gpu.rs`. The asymmetry is real, is a provenance difference and
> is not a mode effect — those claims stand, as does #18's "require non-empty and hard-fail". What
> does not stand is the conclusion: the hard-fail is implementable on the **`AdapterInfo`** path,
> which returns a value here in both GPU modes, so production takes no `ash` dependency and opens
> no second `VkInstance`. **The PCI key comes from `wgpu::AdapterInfo`.**

**Unchased, stated rather than hidden.** `vulkaninfo --summary` exited 1 while still printing both
devices. The device list is the evidence; the nonzero code was not investigated and is claimed as
neither a pass nor a failure of the mode. If a later package needs per-adapter presentation
support, that question makes this exit code live.

**Ruling on where WP06 runs** *(SUPERSEDED BY #39 THE SAME DAY — read #39; the measurement
paragraphs above still stand.)* (operator offered no preference, then confirmed Ultimate is the
mode he has on; taking the recommended branch):
develop and cut WP06's goldens in **Ultimate** — stable, no power-gating, and the same silicon,
device id and driver as G0, so spec 9.12 carries. **Owed, one run:** switch back to **Standard**
(hybrid) once and record what the adapter probes select and whether the retry path engages,
because Standard is
the only configuration in which the iGPU-fallback class can be *observed* rather than reasoned
about. That run is what separates #18 as a written constraint from #18 as a tested one, and it
belongs to WP06/WP07, not to G0. If the model also exposes **Eco**, that mode is the cheapest
honest test of the refusal path — no discrete adapter at all, so the engine must fail rather than
render — but it has never been measured here and is not assumed to exist.

No engine code changed. G0 stays valid: adapter, driver and pinned FFmpeg are the same hardware —
the mode changes enumeration order, not silicon.

### #39 — environment — the operator's standing GPU mode is **Standard**, and what that re-decides

Same day as #38. After its consequences were laid out he chose **Standard** (hybrid Optimus) over
**Ultimate**. This entry replaces #38's "Ruling on where WP06 runs" paragraph and nothing else in
#38 — its measurements stand and are the reason this choice is defensible.

**Why Standard is arguably the better call for WP06 specifically.**
- **The falsifying case becomes the everyday case.** In Standard the iGPU holds DXGI ordinal
  **#0** and NVIDIA is **#1**; in Ultimate NVIDIA is **#0**. A first-adapter bug is *accidentally
  correct* in Ultimate and wrong in Standard. Certifying `device.rs` in Standard exercises the
  guard on every run instead of in one scheduled excursion.
- **#18's premise is restored honestly.** In Standard the primary desktop adapter *is* the iGPU,
  so spec 9.7's prohibition on substituting it and #18's "must be the discrete card" requirement
  genuinely conflict — which is the situation the guard exists for. Ultimate satisfied spec 9.7 by
  accident of configuration, not by code.
- **No effect on output bytes.** Same adapter, same driver, same shaders. #38 measured that
  everything identity-bearing is identical across modes: name, `10de/28a3`, both driver
  spellings, PCI `0000:01:00.0`, 3892 MiB dedicated, both memory budgets.

**What it costs, accepted knowingly.**
- **Hazard 2 is live again.** The dGPU power-gates, enumeration can report phantom, and adapter
  selection needs its retry/wait path in ordinary operation. Expect slower probe runs and the
  occasional retry inside `-WithProbes`.
- **Long unattended exports lose a free protection.** Nothing about Standard makes an export
  wrong, but until WP10's startup assertion exists no code can notice a silent iGPU selection.
  Rule of thumb, not a spec requirement: develop and certify in Standard; if a multi-hour export
  genuinely matters before the guard has been *seen to refuse something on purpose*, flip to
  Ultimate for that run.
- **`probe-out/` currently holds Ultimate figures** (git-ignored, regenerated). **The first act of
  the WP06 session is to re-run `wp00_adapter` and `wp00_gpu` in Standard** and treat that as the
  baseline. The committed 2026-11-05 evidence is Standard-mode and one day old on the same
  driver, so it is the closer reference — but it is not today's measurement.

**Two claims retracted from my own reasoning in this session:** that Ultimate "removes the
wrong-GPU class" (#38 measured that it does not), and that cutting goldens in Ultimate was the
better default. It was the *quieter* default, and quiet is the wrong property to certify an
adapter guard in.

**Unchanged by this choice:** #18's rules still have to be implemented — neither mode does that
work. **Eco** mode is still unconfirmed on this machine. The reboot-to-switch requirement is
ASUS's documented behaviour, not something measured here.

No engine code changed. `BUILD_STATE.md` updated: GPU mode section (rewritten), hazard 2, the
probe-practice note, the Display row, risk 5, and the WP06 preamble plus its owed-work item.

### #40 — WP06 rulings, one correction to #38's PCI provenance, and what is parked

Four prerequisite rulings the operator made on 2026-11-06 before any render code was written,
plus a measurement that contradicts a required-reading paragraph, plus the honest state of the
package at the end of the session that started it.

#### 1. Correction to #38's "One asymmetry" paragraph — the provenances are swapped

#38 states that `device_pci_bus_id` is `Some("0000:01:00.0")` on `wp00_gpu`'s report because it
comes "from ash via `VK_EXT_pci_bus_info`", that the `None` on `wp00_adapter` comes from
`wgpu::AdapterInfo`, and therefore that "the PCI key must come from `VK_EXT_pci_bus_info`, not
from `AdapterInfo`" — a WP06 that hard-fails on an empty `AdapterInfo` value "hard-fails on this
machine in *both* modes".

That is inverted, and the code is unambiguous about it:

| | Where the value comes from | Measured |
| --- | --- | --- |
| `wp00_gpu.rs:207` | **`wgpu::AdapterInfo::device_pci_bus_id`** — populated by wgpu-hal from `VK_EXT_pci_bus_info` | `Some("0000:01:00.0")` today, in Standard |
| `wp00_adapter.rs:342` | hard-coded **`None`**, with the comment "ash's PCI bus info is a separate extension (`VK_EXT_pci_bus_info`) and this probe correlates by LUID, so the field stays empty" | `None`, and its own doc says the extension was never queried through ash |

`ash` does not appear anywhere in `wp00_gpu.rs`. The committed 2026-11-05 G0 evidence shows the
same split, so this is not a mode effect either — which is the one claim in #38 that survives
intact: the asymmetry is real, is a provenance difference, and does not come from the GPU mode.

**Ruling (operator, 2026-11-06):** the production PCI anchor is read from
`wgpu::AdapterInfo::device_pci_bus_id`. `anim_render` therefore takes **no `ash` dependency and
opens no second `VkInstance`**, which is the re-decision ADR-0002 consequence 6 explicitly left
to this package. #18's operative rule is unchanged and still implemented in full: PCI bus id +
adapter name + driver as the revalidation key, and a blank PCI id is a hard failure
(`RenderError::PciBusIdUnavailable`), not "unknown, proceed".

**Why #18's residual stays correct as written.** It says a blank means `VK_EXT_pci_bus_info` is
unsupported and must be refused. On this machine the extension *is* supported, wgpu-hal queries
it, and the `None` in `wp00_adapter` was an unqueried field rather than an absent extension. The
residual's warning and its remedy are both still true; only the location of the working value
moved.

**Consequence for the two spellings of the driver.** With the wgpu-only path, production compares
`AdapterInfo::driver` (`"NVIDIA"`) and `AdapterInfo::driver_info` (`"617.14"`) against the record
of those same two fields. Vulkan's `617.56.0` spelling is an ash-side value and is never compared
to a wgpu one, so #18's "comparing only one produces a false mismatch" cannot be constructed here
at all. `the_vulkan_spelling_of_the_driver_is_never_compared_to_the_wgpu_spelling` in `device.rs`
is the test that keeps it that way.

#### 2. Asset color-space and alpha metadata arrives as manifest data (schema bumps)

Spec 15.2's "Image assets carry color-space and alpha metadata" had no home: WP02's decoders are
metadata-only by ruling (#26) and `AssetManifestEntry` has no color or alpha field. Ruled: the
fact is **recorded at import** and travels in the manifest, not re-read from file headers at
render time — the reason is 15.11's "no silent transfer-function fallback". A renderer that
decided the transfer itself would be free to change its mind on a machine, driver or toolchain
move, and a golden would stop reproducing with nothing in the project data to explain it. The
importer classifies once from the PNG/JPEG header; the manifest stores the verdict; the render
graph reads that verdict and fails on a missing one.

This is a `MANIFEST_SCHEMA_VERSION` **1→2** bump, and because `ProjectDocument.assets` *embeds*
the record collection (deviation row 11) it is simultaneously a `PROJECT_SCHEMA_VERSION` **3→4**
bump with a real `migrate::V3_TO_V4`, a committed `project_v3.anim.json` fixture and a generated
`schemas/project/v4.json` — the fourth time this exact prerequisite gap has been closed this way
(#33 for events, #35 for the tick rate).

Classification rules, as implemented: PNG `sRGB` chunk → sRGB; PNG `gAMA` = 1.0 → linear; PNG
`iCCP`/`c2PC` or JPEG `APP2` ICC → `Unclassified` **plus an import diagnostic**, because an ICC
profile we do not model must be reported rather than approximated (the `PixelLayout::Unknown`
precedent in `image.rs`); JPEG and unmarked PNG → sRGB, recorded as a decision written into
hashed data at import, not an inference made at render. PNG alpha is straight by definition;
no-alpha is opaque.

`Unclassified` has exactly one producer: `V3_TO_V4`. Opening a pre-WP06 project is the honest
failure it should be — the document loads, edits and saves, and `extract_frame` refuses to render
with `UnclassifiedAssetMetadata` naming the asset and the remedy (classify it by re-importing).
An inferred default would be the silent fallback 15.11 forbids.

#### 3. `scene_output` is `Rgba8Unorm` with the transfer in `output.wgsl`

15.6 calls it "explicit export or surface-compatible output format" and 15.12 requires "exactly
one output transfer"; WP00 probed no output format and ADR-0003 left the choice here. Ruled:
`Rgba8Unorm`, **not** the `Srgb` variant, with the linear→sRGB encode performed in the shader. The
reason is testability: an `Srgb` format puts one transfer in fixed-function state where the golden
cannot attribute it, and the moment a second transfer is added somewhere in the chain the two are
indistinguishable in the output. With a linear-storage format the transfer is a named stage in the
pass log, and the "exactly one" clause becomes a claim about a stage rather than about hardware.

#### 4. The post chain is the graph plus one tone-map stage; the effects stay disabled

15.3 lists `post.wgsl` with "DOF, motion blur, bloom and tone-map entry points", 15.5 step 7 asks
for "the fixed-order HDR post chain and one tone-map stage", and 29 (WP14) and 32 (WP17) own the
effects' parameters. Ruled: implement the chain, the ping-pong, the versioned order, the
disabled-pass rule and exactly one tone map. DOF, motion blur and bloom are declared-and-disabled.
Their kernels are not this package's design decisions, and writing them now invents parameters
WP14/WP17 will replace. The disabled-pass contract itself is **tested** (a disabled stage declares
no reads and no writes and returns its input target), which is the part WP14 depends on.

#### 5. The VRAM floor runs through DXGI, matched by vendor/device, and is a warning when it cannot be attributed

#18 decision 2 calls `DedicatedVideoMemory` a required guard; the probe read it because it already
had a `GetDesc1` call. Production has no such call, so `anim_render` takes the target-gated
`windows` dependency 15.4 already lists and enumerates DXGI adapters, matching on
`(VendorId, DeviceId)` — the cross-check 9.7 already prescribes — not by LUID (that needs a second
`VkInstance`, and #18 decision 4 confines LUID work to WP07's budget attribution) and not by
ordinal (#38/#39 make an ordinal a property of the GPU mode, not the board).

Exactly one match applies the floor as a hard guard. Zero or many matches is
`VramFloor::Indeterminate`, **reported rather than silently passed** (9.11), and it is a warning
rather than a refusal because the selection itself is already anchored on the PCI bus id, which is
the field `DXGI_ADAPTER_DESC1` does not have. The residual is stated: on a machine with two
identical NVIDIA cards the floor cannot be attributed without WP07's LUID correlation.

#### 6. Device loss, features and formats

`required_features` stays `Features::empty()` — ADR-0003's own decision says a capability list
gathered from a device opened with extra features is not the list WP06's device has, and every
usage this graph needs is core. The ADR-0003 half it left open closes as an explicit requirement
list checked on the *adapter* before `request_device`: `Rgba16Float` (attachment + binding + copy),
`Depth32Float` (attachment + binding), `Rgba8Unorm` (attachment + binding + copy), `sample_count`
1, no multisampling, because 15.6's table names none and a resolve pass would have no owner.
Loss is detected through `set_device_lost_callback` recorded in a `OnceLock` — not a `Mutex`, which
`clippy.toml` bans near canonical output — and 15.7's "previous transforms update only after
successful submission" is structural: `commit_submission` is the only writer of history and
`graph.rs` calls it after `submit`.

#### 7. Two 15.8 shapes gained a field, and one error variant was dropped rather than declared

`DrawItem` gains `previous_valid: bool` (15.7's per-item "history is invalid" condition is
invisible to a shader that cannot know whether the node existed last submission). `RenderTargets`
gains `scene_output`, which 15.6's target table names and 15.8's struct omits. `AlphaMode` is
verbatim. `UnsupportedAlphaMode` was **not** added to `RenderError` even though 15.7 says
extraction validates "supported alpha modes": `AlphaMode` is a closed five-variant enum and all
five have descriptors, so no condition can produce that refusal — WP05's
`RandomError::UnsupportedAlgorithm` deletion is the precedent, and a declared-but-unreachable
variant reads as covered error handling in a later audit. The reachable half of the same sentence
is `InvalidAlphaCutoff`, which does have a producer and a test.

#### 8. What this session reached, and what is parked

WP06 is **not code complete** and must not be read as if it were. Written: `device.rs`,
`targets.rs`, `camera.rs`, `alpha.rs`, and first drafts of `graph.rs` and `draw_list.rs`. The last
two reference functions and fields that do not exist yet (a `DrawItem::opacity()` reading a field
that was never added, `claims_for()` as a `None` stub that would make every extraction fail,
pipeline signatures the pass modules do not have), and none of `resources.rs`, `readback.rs`, the
five pass modules, the five WGSL files, `tests/golden.rs`, ADR-0010 or ADR-0011 was started.

Ruled on how to land it: the manifest/project schema slice is committed on its own as a labelled
prerequisite commit — it is self-contained, its own tests decide whether it is right, and it is
the thing 15.2 asks for — and the renderer files move to `crates/anim_render/wip/`, which cargo
does not compile, so the gate returns to green and verified rather than red-and-explained.
`lib.rs` declares no modules from `wip/`, and the directory carries a README stating that the code
is unbuilt, unvalidated and partly invented. A future session treats it as notes, not as a
reference implementation.

**How to apply:** the four rulings above are the contract; the parked files are an unverified
sketch against it. Anyone resuming WP06 re-derives the pass, draw-list and uniform shapes from
spec 15.7–15.9, not from `wip/`.

#### 9. The slice was delegated, ran out of turns, and two of its consequences were not optional

The schema slice went to a subagent with a fixed contract and a 150-turn budget. It wrote 1,701
lines into the right files with the right shapes, and then hit the cap mid-implementation — its
last message was "Now I'll implement. Starting with `image.rs`:" after 175 tool calls, most of
them reads. What reached this chat was therefore ~90% done and **red**, not done, and the gate is
what said so: it found a fifth 7-argument call site in `anim_commands`, then a sixth in
`anim_test_support`, then a stale expectation in `anim_headless`.

Two lessons, both about the gate rather than the agent:

- **A `tail`-piped gate reports the pipeline's exit code, not the gate's.** My first verification
  run showed "exit 0" because `cmd //c tools\ci.cmd | tail -40` exits with `tail`'s status. The
  same command, redirected to a file and read directly, returned 1 and named two failing steps.
  Any future "the gate passed" claim must come from the gate's own summary line, and this is the
  second time in the project that a wrapper has laundered a result (#19's hosted-CI file would
  have done the same by declaring coverage that never runs).
- **Classifying at import is stricter than WP02 was, and the strictness is the point.** Now that
  the record's claims are derived from the stored header, `anim_assets::import` refuses payload
  bytes that carry no PNG or JPEG signature — `UnsupportedDecoder` — where before it accepted
  anything labelled `Image`. Three suites were importing strings like `b"an imported payload"` as
  cels. They were given real minimal PNGs rather than looser assertions, because a test that
  imports arbitrary bytes as an image would keep passing after the classification rule was
  deleted, which is the exact shape of unenforced coverage this project keeps finding.

`anim_headless`'s replay report prints `document.schema_version`, so its `schema_version 3`
expectation became 4 with this bump. The report's own `wp05-replay/1` tag did not move, and the
test comment says which of the two numbers a format change is supposed to touch.

**How to apply:** a bounded, decided slice can be delegated, but budget for the reading. Anything
with more than a handful of files to *understand* before the first edit needs either a larger turn
allowance or its own research pass first — and the finishing is only mechanical while the gate
keeps coming back green, which is the part that took three cycles rather than zero.

### #41 — WP06 pre-code rulings: the `extract_frame` claims seam, and where pixel decode lives

Two decisions taken on 2026-11-07 before any render code was written, both flagged as open by
`crates/anim_render/wip/README.md` and `BUILD_STATE.md`'s WP06 preamble. #40's four rulings are
untouched; this entry closes the two questions #40 left for the resuming session.

#### 1. `extract_frame` reaches the claims through a widened 15.9 signature

**The gap.** Spec 15.9 gives `extract_frame(scene: &SceneSnapshot, sim: &SimulationFrame,
previous: &PreviousFrameState, quality: RenderQuality)`. A `DrawItem`'s alpha mode and colour
transfer come from `ImageAssetClaims`, which #40 ruling 2 placed on `AssetManifestEntry.image`
— inside `ProjectDocument.assets`. Neither argument carries it: WP04's `EvaluatedNode` has
`asset: Option<AssetId>` and nothing else (13.8), and `SimulationFrame` has no asset vocabulary
at all. So the signature as written cannot produce a `DrawItem`.

**Ruling: append one parameter, `assets: &AssetManifest`, after `quality`.**

```text
pub fn extract_frame(scene: &SceneSnapshot, sim: &SimulationFrame,
    previous: &PreviousFrameState, quality: RenderQuality,
    assets: &AssetManifest) -> Result<RenderFrame, RenderError>;
```

The first four parameters keep 15.9's exact order and types, so the deviation is a pure suffix
— the same shape #40 ruling 2 used when it encoded `AssetManifestEntry.image` last to keep every
earlier field's byte position. `AssetManifest::get(&AssetId) -> Option<&AssetManifestEntry>` is
already the only public route into its private `records` map, and `anim_assets` is already a
15.4 dependency, so this adds no edge to the crate graph.

**Rejected: a resolver trait.** A `ClaimsResolver`/`AssetClaimsSource` trait would have exactly
one implementor, `AssetManifest`, and would exist only to let a test substitute a fake. But
`SceneSnapshot` and `AssetManifest` both have public constructors and public fields, so a test
builds the real thing — `anim_scene/tests/evaluation.rs` and `anim_sim/tests/determinism.rs`
both do exactly that and neither uses a mock. An abstraction with one implementor and no second
caller is the premature kind. `anim_assets::resolver` is free functions over
`(store, manifest, id)`, not a trait, so there is no existing seam to implement either.

**Rejected: claims in the snapshot.** This is the option that looks cheapest and is the most
expensive. `snapshot_hash` is a `ContentHash` over `anim_engine/snapshot/v1` (WP04, ADR-0007),
and adding claims to `EvaluatedNode` changes that pre-image, so **every snapshot hash in the
project moves**. That is WP04's identity, G3 is already passed against it, and WP06 has no
mandate to spend it. It would also make the same fact live in two places — the manifest record
and the snapshot — which is the second-source-of-truth shape #37 deleted two frame fields for.

**Rejected: a `None` stub.** `wip/draw_list.rs`'s `claims_for()` returns `None` unconditionally,
which makes every extraction fail with `UnclassifiedAssetMetadata`. #40 §8 names it as a defect;
it is not a seam, it is a broken function that compiles.

**The lookup rules that follow**, all in `draw_list.rs`, each with a producing test:

| Node state | Outcome |
| --- | --- |
| `visible == false` | no draw item; its transform is already composed into children by WP04 |
| `asset == None` | no draw item — a group node, not a drawable |
| `asset == Some(id)`, `get` returns `None` | `RenderError::MissingAsset` naming the id |
| entry present, `image == None` | `RenderError::NotAnImageAsset` naming the id and `kind` |
| `color_space` or `alpha` is `Unclassified` | `RenderError::UnclassifiedAssetMetadata` (#40 ruling 2's one consumer rule) |
| classified | `alpha_mode` derived from `ImageAlpha` + the material's blend intent |

Two new `RenderError` variants are needed for rows 3 and 4; 15.11's list has no row for either,
which is the same omission class as deviation rows 9 and 19. `UnsupportedAlphaMode` stays
**deleted** per #40 §7 — `AlphaMode` is a closed five-variant enum and all five have descriptors.

#### 2. Pixel decode is implemented in `anim_assets`, with two pinned decoder libraries

**The gap.** 15.12 requires golden coverage of "linear decoding", and 15.7's alpha conversion
starts from decoded authoring colour. Nothing in the workspace decodes a pixel: #26 made WP02's
decoders metadata-only, and `image.rs` parses PNG/JPEG *headers* itself. A workspace-wide search
for a decode entry point returns nothing, and no codec crate is in the dependency graph. #26
deferred this precisely because "pixel/PCM decode has no consumer until WP06/WP08" — WP06 is
that consumer, so the deferral expires here.

**Ruling (operator, 2026-11-07, asked as one bundled question with three options): add the
decoder to `anim_assets`.** The home is already named by the spec and needs no new file: 11.3
lists `crates/anim_assets/src/image.rs` as "Image metadata, color and alpha decode", and 11.4
already permits `anim_assets` to depend on "decoder libraries". 11.7's resolve-and-verify step
ends "then invoke the registered decoder", and 11.10 says "GPU textures are derived caches keyed
by exact content and decode configuration" — so the decode belongs behind `DecoderConfiguration`
and its result is cacheable, not authoritative.

Pinned pair, chosen because `image 0.25.10` in the local registry cache already pairs these two
versions and is therefore a combination that has been built rather than guessed:

| Crate | Pin | Role |
| --- | --- | --- |
| `png` | `=0.18.1` | PNG decode, already in the registry cache |
| `zune-jpeg` | `=0.5.5` | JPEG decode, pure Rust, `default-features = false` |

Both are `=`-pinned on ADR-0001's rule for anything that decides output bytes. Spec 3.2 requires
an ADR for a dependency exception, so **both rows go into ADR-0001's existing exceptions table**
beside `ash`, `pollster` and `sha2` rather than into a new ADR number — ADR-0001 *is* the
dependency-lock record, and the spec's ADR register assigns WP06 only ADR-0010 and ADR-0011.
`Cargo.lock` must be regenerated and committed in the same change, because the gate runs
`cargo test --workspace --locked` and `anim_render` currently has no `dependencies` array at all.

**Rejected: decoded bytes as a typed input to `resources.rs`.** No new dependency and it stays
inside 15.3's file list, but WP06 could then not render an imported asset end to end, and
"linear decoding" would be tested against procedural pixels the test itself invented. That is
a declared gap rather than a `None` stub, but it is still a gap on the one clause that needs a
real file.

**Rejected: a hand-rolled PNG decoder.** Inflate plus unfiltering with no new dependency avoids
the exception ADR, but puts a hand-written codec on a golden-bearing path and leaves JPEG
assets unrenderable. The risk is exactly where this project least wants it.

**What the decode must respect.** `DecoderConfiguration` already has `Rgba8Unpremultiplied` and
`Rgba8Premultiplied`, and #40 ruling 2 deliberately kept premultiplication *out* of the manifest
record because it is a decode choice. So the configuration selects it, the derived-cache key
(`content_hash`, `importer_version`, `configuration`) discriminates it, and the record still
says only `srgb`/`linear` and `opaque`/`straight`. `ImageMetadata.interlaced` is documented in
`image.rs` as the flag WP06 must consult before committing to a pass — Adam7 is decoded, not
refused, because refusing it would make a valid PNG unrenderable with no remedy.

**How to apply:** `extract_frame` takes five parameters and the fifth is the manifest; any call
site written against 15.9's four is wrong. Decode lives in `anim_assets::image`, never in
`anim_render` — a renderer that decodes is a renderer with a second source of colour truth.

### #42 — WP06 built: the render graph, and four defects only running it could find

WP06 is **code complete** and **G5 is not claimed**. The package landed as one commit after
#41's two pre-code rulings; `crates/anim_render/wip/` is gone, its four usable files un-parked
and its two invented ones (`graph.rs`, `draw_list.rs`) rewritten from spec 15.7–15.9 rather
than repaired. All eighteen files 15.3 lists exist, plus the five WGSL shaders and
`tests/golden.rs`. ADR-0010 and ADR-0011 are written and **ADR-0003 is closed**, which its own
text said G5 depended on.

#### 1. What the goldens are, and why there is no tolerance in them

15.12 asks for "pinned-stack readback goldens", and the suite opens a real device through
`GpuContext::create`, so #18's PCI/name/driver revalidation and the VRAM floor run on every
test. On any other adapter it **refuses** rather than skipping: a golden that does not run is
not a golden that passed, and spec 9.12 will not carry a pass across a different adapter.

No numeric tolerance appears anywhere, because cross-device tolerance is WP11's ADR-0016 and
approval is Gate 1 after S2. The goldens are instead three kinds, each exact:

- **Render against render.** A `Straight` and a `Premultiplied` asset holding the same pixels
  must produce byte-identical images; an equal-depth tie must composite the same way in either
  insertion order; the clean scene output must equal the export readback. Comparing two
  readbacks needs no expected colour and no transcendental to disagree about.
- **Analytic where the arithmetic is exact.** Black is 0.0 through the tone map and through
  both segments of the sRGB curve; alpha passes through both untouched. Those bytes are the
  numbers the spec says they are.
- **Structural.** The nine-element pass log, the target generation, the velocity validity and
  the f16 HDR range are claims about what ran, not about what colour it produced.

Where `pow` is involved the assertion is a *relation* — greater than, not clamped — because a
byte there would be a tolerance in disguise.

#### 2. Four defects the tests caught that reading the code would not have

Each is recorded because the shape of the mistake is the reusable part.

1. **`SceneLoad` latched forever.** `HistoryState::detect` read it from `!self.valid`, and
   `commit_submission` then set `valid = blocking.is_empty()`. The first commit reported
   `SceneLoad`, which set `valid` false, which made the second commit report `SceneLoad` again
   — so history was never usable and every velocity was zero for the life of the process. It
   is now detected from `previous_tick.is_none()`, which the commit actually fills in.
2. **The graph read the previous frame's verdict for the current frame.** Even with the latch
   fixed, `scene_uniform.history_valid` came from `self.previous.history`, which describes the
   *last* commit. The frame right after the first submission therefore had a contiguous
   history and a zero velocity. `RenderFrame` now carries its own `history_valid`, computed by
   `extract_frame`. This one needed the GPU to surface: the CPU tests passed.
3. **The uniform structs were `align(4)`, not `align(16)`.** `#[repr(C)]` over `[[f32;4];4]`
   fields gives alignment 4, while WGSL's `mat4x4<f32>` requires 16. The *offsets* were right,
   so the byte layout worked and nothing visibly broke — which is exactly why it is dangerous.
   Both are now `#[repr(C, align(16))]`, and the offset tests assert the alignment too.
4. **Two golden tests were vacuous and passed for the wrong reason.** The layer-order test
   used one texture, and over-blending a single colour is commutative even at unequal alpha, so
   swapping the order could not change the image; it also never sorted, because `render_items`
   bypasses extraction. It now registers two colours and applies `total_cmp`. The
   linear-decoding test changed only the manifest claim while the texture had been uploaded
   from a procedural `DecodedImage`, so the claim could not affect anything; it now decodes one
   real PNG twice under two records that differ only in `color_space`.

The general rule these four point at: **a test that cannot fail is worse than no test**, and
the way to find one is to ask what change would make it go red. Two of these four went red only
after that question was asked of them.

#### 3. What is deliberately not built

- **DOF, motion blur, bloom and grade** are declared and disabled (#40 ruling 4). Their entry
  points exist in `post.wgsl` as the disabled-stage identity and are unreachable, because
  `PostChainConfig::validate` refuses to enable them with `PostStageUnavailable` naming
  WP14/WP17. The disabled-pass contract is tested as data: a disabled stage produces a
  `PassRecord` with empty reads and writes, so the log shows it was considered rather than
  forgotten.
- **No WP07 allocator.** `ResourceRegistry` keeps the size, usage and ownership records 16.2
  asks for and registers nothing with a module that does not exist.
- **No golden tolerance and no baseline approval.** ADR-0016 is not written; it is not in
  `docs/adr/` and never was, and the spec register assigns it to WP11.
- **`AlphaTest`, `Premultiplied` and `AdditivePremultiplied` are not authorable.** They have
  descriptors, pipelines and goldens, but no authored field carries a cutoff or a
  premultiplication intent. `alpha::mode_from_claims` is the one place WP12 extends.
- **Particle rotation is not applied.** `Particle.rotation` is in turns, and building a
  rotation matrix needs `sin`/`cos`; BUILD_STATE risk 28 requires the first package that
  rotates a particle into a velocity to use a versioned fixed-point routine and move
  `SIMULATION_MODEL_VERSION`, not to call trig whose bits a different C runtime could change.
  WP06 draws particles axis-aligned and leaves the rotation to the package that owns the rule.

#### 4. The transparent-sort sign, written down because it is invisible otherwise

15.7 says "descending quantized_view_depth" and never says which sign depth has. The authored
convention is +z toward the viewer, so view depth is the **negated** view-space z — distance
along the forward axis, larger meaning farther — and descending is back-to-front. Reading
"depth" as the authored z sorts front-to-back, blends to a different image, and is finite and
plausible at every step. `draw_list.rs` comments the sign at the computation and
`the_comparator_is_15_7_s_tuple_in_15_7_s_order` tests it.

**How to apply:** G5 is code-complete-plus-outstanding-approval. The suite passes and the
images exist; saying "G5 passed" is not correct until the operator approves them at Gate 1
after S2. The next package is WP07, and `ResourceRegistry`'s records are what it consumes.


