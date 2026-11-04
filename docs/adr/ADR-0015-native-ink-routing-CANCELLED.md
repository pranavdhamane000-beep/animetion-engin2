# ADR-0015 — Native Ink routing and cancellation semantics: CANCELLED

Status: superseded by removal from scope — this ADR records a cancellation, not a decision
Date: 2026-11-05
Work package: WP00 / WP10 (spec register line "ADR-0015 · Native Ink routing and
cancellation semantics · WP00/WP10")

This is the addendum that `BUILD_STATE.md` and `DECISIONS.md` #8 said was owed. The
DOCX is authoritative and still specifies pen input in four places, so until this
file existed the only things between a fresh session and a rebuilt pen probe were a
deviations table and a decision log entry.

## Problem

Spec v3.2 routes pen input through two artifacts and one gate requirement:

| Location | What it asks for |
| --- | --- |
| 9.3 | `crates/anim_test_support/src/bin/wp00_pen.rs` — "Win32 pen pressure, tilt, history and cancellation capture" |
| 9.5 step 6 | "Capture native pen packets, coalesced history, pressure, tilt, pointer ID, capture loss and cancellation" |
| 9.12 | "Pressure changes during a physical pen test" as a G0 exit condition |
| 19 (WP10) | `anim_editor/src/ink.rs` — `GetPointerPenInfo`, pressure normalised to `[0,1]`, coalesced history, frozen view transform at pointer-down, transient preview committing exactly once as one undo entry |
| 4.2 | `anim_editor` responsibility list includes "pen strokes" |

Gate G0 cannot pass on a machine with no digitizer, and this machine has none: PnP
enumeration finds only an ASUS Precision Touchpad and a USB mouse. Spec 9.11
explicitly forbids the obvious workaround — substituting mouse-only input.

## Alternatives considered

1. **Build the probe, validate against synthetic `WM_POINTER*` packets, record G0 as
   "partial — pen unverified".** This was the decision taken on 2026-11-04 before
   checking whether anything consumes ink strokes. It keeps an unsatisfiable
   hardware requirement alive forever and lets a synthetic run be mistaken for the
   physical one that 9.12 asks for. Rejected.
2. **Strokes as timeline annotation.** Keeps the digitizer debt for convenience a
   mouse already provides. Rejected.
3. **Strokes as renderable artwork.** Needs a paint module — rasterisation, layer
   blending, alpha, colour management, a brush model — that does not exist anywhere
   in the spec. It would also make a tablet urgent rather than optional. Rejected.
4. **Remove pen input from the project.** Chosen.

## Evidence for removal

The gap analysis is the whole argument, so it is recorded here rather than left in a
log entry:

- Pen appears in exactly the five locations above. Nothing else in v3.2 mentions it.
- WP10 §19 specifies **capture only**. Nowhere in the document does a committed
  stroke receive a consumer.
- The vocabulary a stroke-consumer would need is absent outright: `corrective` and
  `brush` have zero occurrences in the full document. `cleanup` appears only in
  transaction and resource contexts; `paint` only inside a dope-sheet file path;
  `raster` only in CPU silhouette analysis.
- The artwork path the engine actually uses is **imported assets**. WP02's
  content-addressed store feeds named poses and replacement cels ("Named poses and
  replacement cels occur at exact frames"). WP14 generates smears procedurally
  ("Stylized smear. Use driver velocity magnitude for stretch, clamped to max
  aspect").
- The spec itself places hand-drawn smears and original choreography at Level 4, not
  Level 3 — and Level 4 frames arrive as cels, i.e. through WP02.

## Decision

Do not build `wp00_pen.rs`. Do not build `anim_editor/src/ink.rs`. Artwork is
authored externally in Krita or Clip Studio Paint and imported through WP02.

ADR-0015 is therefore closed by removal. No replacement ADR for ink routing is
required, and none should be opened.

## Consequences

1. **G0's exit condition shrinks by one clause.** Spec 9.12's "pressure changes
   during a physical pen test" is deleted, not waived. G0 becomes fully achievable
   on this machine — pen was the single prerequisite it could not meet.
2. **WP00 is five probes, not six**: `wp00_gpu`, `wp00_adapter`, `wp00_media`,
   `wp00_export`, `wp00_replace`.
3. **WP10 keeps everything else** — scene editing, transport, export launch, preview
   quality adaptation. Only ink is removed. `anim_editor`'s spec 4.2 responsibility
   line reads "Session, transport, panels and preview quality"; the recorded crate
   doc states why "pen strokes" is missing.
4. **Level 4 still depends on hand-drawn frames.** This changes *where* they are
   drawn, not whether they are needed.
5. **Level 3 review annotations are unaffected.** Spec 45 requires annotations as
   acceptance evidence; they are producible with mouse, text and frame markers.
6. **Reopening condition.** If marking up clips by hand ever feels slow, a pen tablet
   reopens this decision rather than reversing it. The cost of reopening is one probe
   and one editor panel, both of which the spec already describes well enough to
   build from.

## Schema/version impact

None. No input device was ever serialised into a project, command, or export format,
so no schema changes and no migration is needed.
