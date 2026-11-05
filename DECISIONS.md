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
---

## Test command

Defined as of WP00, and this is what a resuming session must run (spec 9.12). From
Git Bash, prepend the toolchain that `setx` persisted but a live shell does not have:

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
