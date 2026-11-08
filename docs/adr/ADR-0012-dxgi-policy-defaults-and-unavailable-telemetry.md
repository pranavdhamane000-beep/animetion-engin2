# ADR-0012 — DXGI policy defaults and unavailable-telemetry behavior

Status: **accepted.** Written as WP07's deliverable on 2026-11-08. The choice to build
telemetry at all was ruled by the operator in DECISIONS.md #43.1 (closing BUILD_STATE
risk 24); this ADR records the policy defaults and degradation behavior spec 16 leaves
as configuration, plus the integration semantics the tests forced into the open.

Date: 2026-11-08
Work package: WP07 (spec register: "ADR-0012 · DXGI policy defaults and unavailable-telemetry behavior · WP07")
Evidence: `crates/anim_render/src/memory/` (ten files), `crates/anim_render/src/graph.rs`,
`crates/anim_render/src/targets.rs`, `crates/anim_render/src/resources.rs`,
`crates/anim_render/tests/memory.rs`, `crates/anim_test_support/src/gpu_memory.rs`,
`crates/anim_app/src/settings.rs`, `crates/anim_headless/src/budget_report.rs`
Spec 16.3–16.12, 5.2. Continues ADR-0002 (adapter correlation, budget-vs-VRAM) and
ADR-0003 (the format matrix the allocator sizes against).

## Problem

1. **16.6 gives one number and five blanks.** The initial target fraction is 0.80; the
   safety reserve, release band, sample count, query interval and upload cap are
   "configured" with no defaults. A default chosen in code is a decision that needs a
   home, and 16.6 is explicit that these are configuration defaults, not hardware
   promises.

2. **The unavailable path needs an estimate, and the obvious one is wrong.**
   ADR-0002 consequence 4 measured the local budget at 3308 MiB against 3892 MiB
   dedicated. Estimating the budget as the dedicated figure would reserve against
   memory the OS never granted.

3. **16.7's "hard trigger" and 16.6's over-budget sampling are two thresholds, and the
   spec does not say how they relate.** Conflating them makes eviction start at the
   soft target on every allocation, or never start until the budget is already crossed.

4. **wgpu 30's creation calls return handles, not results.** `create_texture` and
   `create_buffer_init` have no `Result`; a GPU allocation refusal arrives as device
   loss. 16.7's "attempt wgpu creation, on failure roll back" needs an interpretation
   that does not invent an error channel the API does not have.

5. **`create_renderer`'s certified signature receives no adapter facts.** 15.9 gives it
   `device, queue, config` — no VRAM figure, no vendor/device ids — so the manager it
   constructs cannot know a budget, and WP06's goldens must keep passing unchanged.

6. **`query_interval` is a duration in a workspace that bans `Instant`.** The ban keeps
   canonical paths clock-free; telemetry cadence is not canonical, but the exception
   has to be scoped and recorded rather than spread.

## Decisions

1. **The default register (DECISIONS.md #44 ruling 7):** `target_fraction` 0.80 (16.6's
   own number), `safety_reserve_bytes` 256 MiB, `release_band_bytes` 128 MiB,
   `over_budget_samples` 3, `query_interval` 100 ms, `max_upload_bytes_per_frame` 64 MiB.
   The reserve is an absolute floor that binds before the fraction on small budgets;
   the band is one hysteresis step wide so a pass does not leave the next sample
   immediately over; three samples at a 100 ms interval debounce a transient spike
   without delaying a real one by more than a third of a second; 64 MiB per frame is
   roughly two 4K RGBA16F uploads, enough to keep a scrub responsive without letting a
   reload storm starve the frame. `MemoryPolicy::validate` refuses a fraction outside
   `(0, 1]`, a zero sample count, a zero interval or a zero cap at construction, and
   `anim_app::settings::MemorySettings` is the validating door the app composition will
   use. **Rejected:** deriving defaults from the adapter at runtime — a default that
   moves with the hardware is not a default a report can reproduce.

2. **The unavailable estimate is three quarters of dedicated VRAM**, with the same
   target formula applied. The G0 ratio was 3308/3892 ≈ 0.85; 0.75 sits below the one
   measured ratio, so the estimate is conservative against the evidence that exists.
   Correlation failure, query failure and absent `IDXGIAdapter3` all degrade to
   `BudgetSource::Unavailable` with the reason recorded; the manager keeps internal
   accounting and never queries an unrelated adapter (16.6). `telemetry_status()` and
   the budget report carry the reason, so a degraded run is visible rather than silent.

3. **Two thresholds, two jobs.** The *soft target* is
   `min(budget × fraction, budget − reserve)` with the product floored and the
   subtraction saturating; the *release watermark* is the soft target minus the band.
   The hysteresis streak (consecutive samples over the soft target) gates the periodic
   pressure pass. The *hard trigger* for allocation preflight is the budget itself:
   projected usage above it plans eviction immediately, bypassing the streak, because
   16.6 says actual allocation failure does not wait for sampling.

4. **Preflight is the primary guard; device loss is the recovery boundary.** Because
   wgpu's creation calls are infallible handles, a GPU OOM surfaces through the loss
   callback, and `Renderer::notify_device_loss` invalidates history, registry,
   residency and serials (16.7). Permit rollback covers every fallible path that does
   exist — the registry's input validation, a refused preflight, an early `?` — and
   `recovery::plan_recovery`'s one bounded retry is exercised by the synthetic suite
   where a creation failure can be injected. A failed frame also aborts its serial and
   releases its registered buffers (`abort_frame`), so neither the accounting nor the
   fence order carries a ghost; this was a defect the GPU suite caught, not a design
   predicted.

5. **Readback staging is accounted; its internal submission takes no serial.** The
   staging buffer is reserved before the copy, registered `mapped` after it, and
   released once the synchronous copy returns (it polls its own submission and unmaps
   before returning). Nothing it owns survives the call to be evicted later, so a
   fence serial would protect nothing — a recorded departure from 16.7's "every queue
   submission", whose purpose is protecting resources that outlive the call. Scene
   submissions, whose buffers do outlive it, all take serials and complete through the
   `on_submitted_work_done` channel.

6. **The unconfigured manager accounts but does not preflight-refuse.**
   `create_renderer` constructs the manager with an unknown budget (`u64::MAX`
   sentinel), so pressure checks are inert until `configure_memory` attaches a policy,
   a conservative estimate and live telemetry. This keeps 15.9's signature and every
   WP06 golden unchanged while 16.4's "actual allocation errors remain authoritative"
   covers the unconfigured window. `configure_memory` refuses to run after images are
   uploaded, because a fresh table cannot adopt resident textures it has no records
   for.

7. **One scoped clock exception.** `QueryCache` uses `std::time::Instant` under an
   allow-with-reason naming 16.4 and #44 ruling 5: telemetry is advisory, the interval
   is a duration, and nothing in the cache reaches canonical bytes. The ban stays
   everywhere else.

8. **Execution ownership and the required set.** The manager plans; the renderer drops
   handles and confirms. Targets are registered `locked`, so "never free live targets"
   is a property of the exclusion list rather than a remembered rule. An explicit
   `evict_image` drains known completions and releases the last frame's required claim
   on the asset — both were defects the reload golden caught: a completion nobody
   drained left the texture looking in-flight, and a stale required claim barred an
   eviction the caller had explicitly asked for. The refusal diagnostic now names
   every exclusion term, including the required set and the completed watermark,
   because the first version hid the term that actually fired.

## Consequences

1. **WP09** pins active-frame resources with `set_locked`, budgets readback through the
   same preflight, and calls `release_completed_work` at frame end. Export mode never
   defers: a pinned requirement that cannot fit is an actionable `OutOfMemory` carrying
   requested bytes, budget, usage and context (16.11).
2. **WP10** composes `MemorySettings` in the preview regime, where optional uploads may
   be deferred (`UploadDeferred`, a warning) and optional targets reduced. The editor's
   narrowed registry (BUILD_STATE item 16) and this policy are independent controls.
3. **Changing a default is a policy decision**, recorded here or in DECISIONS.md — not
   a silent edit, because a golden's eviction behavior under pressure moves with it.
4. **Residual, stated:** competing-process pressure is validated synthetically
   (observed usage dominates `effective_usage` and is not evictable) plus a live query
   proving external usage reaches the snapshot — the 2026-11-08 budget run showed
   ~194 MiB of desktop usage against a 3308 MiB budget. A scripted co-running GPU load
   is not part of the gate. Node index 0 remains ADR-0002 consequence 5's assumption.
5. **G6 is self-certifiable** by the operator's ruling (#43.2): the 16.12 suite —
   synthetic convergence, rollback, ordering, hysteresis, exclusions, shrinkage,
   unavailable telemetry, adapter mismatch, the 10 000-frame stress, and the GPU
   reload/resize/scrub/loss goldens — is the certification. G5 remains owed and batched.