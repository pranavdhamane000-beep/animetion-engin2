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
