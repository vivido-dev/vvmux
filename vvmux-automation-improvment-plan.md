# vvmux automation improvement plan

Goal: bring `vvmux msg` automation to parity with vivido/vivida where the
multiplexer model allows it, and document where it deliberately differs.

Method: compared `skills/vvmux/` against `vivido/skills/vivido/` (SKILL.md +
`references/commands.md`) and `vivida/skills/vivida/`, then verified every
candidate gap against the implementation (`vvmux/src/automation.rs`,
`vvmux/src/ipc.rs`, `vvmux/src/session.rs`, `vvmux/src/plan.rs`,
`vvmux/src/capture_media.rs`, `vvmux/vvmux-plugin-api/src/manifest.rs`) and,
for behavior claims, against `vivido/src`. Anything below marked "verified"
was read in source, not inferred from docs.

## Where vvmux is already ahead (do not regress)

No action; listed so the plan is not one-sided and so parity work does not
flatten these back to the vivido shape.

- Advisory leases, agent lifecycle + provider plugins, `agent-explain`,
  `agent-read`, no-provider fallback procedure.
- `submit` (atomic text+Enter), `shell-command` with real exit status gated on
  OSC 133, `--expect-screen/--expect-session/--expect-layout`,
  `--idempotency-key`.
- `search`, `record`/`replay`, `snapshot`/`save-layout`, `move-pane`,
  `sync-input`, copy-mode/float/pin/transparent flags, `capture`
  reveal-settle-read, `plugin` catalog/invoke.
- `wait agent-state`, `wait rendered`, `wait media`/`wait media-track`,
  `capture --rendered`, `select-tab`/`focus --wait`.
- `inspect` depth: cursor, mode names, copy-mode state, process block with
  foreground job detection, cwd/spawn cwd, exit status, `outer_crop`.
- `get-grid` with `--since-screen`/`--start-line`/`--row-count` and gap
  metadata; `subscribe --after` replay with unfiltered `gap` records.
- `run-plan` with `bind`/`$ref`, `when`, `on_error`, `verify`
  (screen_changed/rendered/capture), `--dry-run`/`--preflight`.
- `vvmux --skill`, machine-readable `api` contracts, `update`/`channel`.

## Gap 1 — No true screenshot (structural; needs a decision)

`capture-media` composes the producer's own media surfaces only. Terminal
text is rendered by the presenter's GPU and vvmux holds a cell grid with no
font rasterizer, so text never appears (`src/capture_media.rs:8-9`). There is
consequently no `frame_sequence`, no `wait frame`, and no `verify.screenshot`.
The attached-client substitutes (`wait rendered`, `capture --rendered`,
`verify.rendered`, `select-tab --wait outer`, `inspect`'s `outer_crop` +
`session-inspect`'s `outer` block) all require an attached client and prove
mux-observed state, never GPU presentation (`src/plan.rs:99-103` says this
explicitly).

Options:

1. **Accept + harden interop (recommended default).** The Vivido interop path
   exists but is multi-hop and prose-only. Add a single skill recipe (and
   consider a `msg` helper or script) that goes `session-inspect` outer block
   → `outer_crop` → attached-Vivido `screenshot`/`capture`, with the
   detached and `remote: true` (vvssh) failure modes spelled out.
2. **Server-side grid rasterizer.** Render the cell grid + styles to PNG in
   `capture-media` (new font stack, new dependency, new correctness surface
   for wide/combining/underline/hyperlink rendering). Heavy; only if
   detached pixel-truth is worth it.
3. **Structured frame artifact.** Compose grid text + media bounding boxes
   into one non-pixel artifact (e.g. text plus per-surface rects) for
   layout assertions without fonts. Cheaper than (2), weaker than (1).

Decision required before scheduling. Options (2) and (3) also need the
owner-scoping treatment from AGENTS.md (two producers reusing numeric IDs).

## Gap 2 — Mouse: pacing, verification, and precision are wire-shaped but unusable

Verified in `src/automation.rs:1241-1400` and `src/session.rs:4195-4196,5941-6080`:

- The wire carries `duration_ms` ("pace a path") and `wait_rendered`
  ("resolve after the attached client acknowledges a newer render"),
  but the server ignores both (`duration_ms: _, wait_rendered: _`) and the
  CLI hardcodes `None`/`false` plus a fixed 30 s timeout. vivido has
  `--duration`/`--wait-frame` with release guarantees. vvmux paths are
  synchronous (points resolve up front, so resolution failures are atomic),
  which narrows but does not close the gap: no paced delivery, no
  completion-after-render.
- `mouse path --point` parses `COLUMN,ROW` cells only; the wire's
  `MousePosition::{Pixel, Relative}` forms are unreachable for gestures, so
  even attached callers cannot drive sub-cell strokes (vvpaint-style apps
  under SGR pixel mode). Single actions already encode SGR 1016 exactly
  (`application_mouse_coordinates(..., modes.sgr_pixels)`).
- Pixel coordinates need an attached client (`resolve_mouse_position`,
  `src/session.rs:5855-5860` — deliberately refuses to guess metrics).
  Detached pixel automation is therefore impossible; that is the safe
  default, but it should be a documented matrix entry (see Gap 7), not
  tribal knowledge. Do not "fix" by guessing metrics.
- No horizontal scroll; integer notches only (vivido: `--vertical`/
  `--horizontal` floats). Minor.

Work items:

1. Implement server-side paced gesture delivery with deadline-bounded
   failure, and `wait_rendered` completion; expose `--duration`,
   `--wait-rendered`, `--timeout` on `mouse`.
2. Accept pixel (`X,Ypx` or explicit flags) and relative point forms for
   `path`; keep cells the default and document which form each app class
   wants.
3. Add horizontal scroll if any in-tree consumer needs it; otherwise defer.
4. Regression: paced gesture against a failing/closing pane still releases;
   render-verified mouse resolves only after a newer acknowledged render;
   pixel path to an SGR-1016 app lands sub-cell-exact (assert encoded SGR
   bytes via transcript, not just the reply).

## Gap 3 — No terminal reset / in-place pane respawn

vivido has `reset-terminal` (discard parser state, primary screen, clear
client modes/Vivid scene, resume quarantined PTY; ID survives) and
`restart-terminal` (transactional PTY replacement from retained launch
options; ID and embedding survive). vvmux automation has neither
(`Reset*`/`Restart*` in `automation.rs`/`ipc.rs` is only `ResetTabTitle`).
Recovery today is `close-pane` + `run`, which loses pane ID, position,
history, and name attachment.

Work items:

1. Add `reset-pane`: parser/mode/scene reset for a wedged pane, keeping ID,
   position, and history. Define exactly what survives (mirror vivido's
   list, adapted: scrollback and pane ID survive).
2. Add `respawn-pane` (name TBD): replace the PTY from retained launch
   options in place; failed replacement leaves the old pane inspectable.
3. Skill + references updates; regressions for ID/position/name survival and
   for the failed-respawn-keeps-corpse path.

## Gap 4 — Plans: no assertions or CI reports

vivido v2 plans add per-step `assert` (`text_contains` with window/timeout/
line window, or `result_pointer` + `result_equals`) and
`--report junit|sarif` (`vivido/src/polling/ipc.rs`, `IpcPlanReport`), plus
`vivido test` for ephemeral headless runs. vvmux `plan.rs` has
`when`/`on_error`/`verify`/`dry-run`/`preflight` but no `assert` and no
report formats (verified: no `assert`/`junit`/`sarif` in `src/plan.rs`).

Work items:

1. Add per-step `assert` with the two vivido forms, pane-addressed.
2. Add `--report junit|sarif --output PATH` keeping NDJSON on stdout.
3. Consider a `vvmux test` wrapper over `new --detached` + `run-plan` +
   artifact capture + `kill-session` (all primitives exist; the wrapper is
   what is missing). Defer if scripting suffices.

## Gap 5 — Events: no output/title/directory/bell streams

`EVENT_KINDS` (`vvmux-plugin-api/src/manifest.rs:76-89`, 13 kinds) has no
`output` byte-stream event (vivido streams ≤64 KiB chunks with offsets), no
title/directory change events, and no bell. Automation must poll
`transcript`/`get-text` for bytes and cannot subscribe to cwd/title changes
that vivido pushes. `gap` records already cover the overflow story.

Work items:

1. Add a bounded `pane.output` event (chunked, with start/end offsets, same
   backpressure rules as existing streams).
2. Add `pane.title_changed`, `pane.directory_changed` (needs shell OSC 7/OSC
   integration the same way vivido's does; `inspect` polling stays the
   no-integration fallback), and `pane.bell`.
3. Keep `event_kinds` in `capabilities` authoritative; update the skill's
   event list, which currently names only a subset.

## Gap 6 — Small read/write parity gaps

1. `wait output` has no `--base64` raw-byte matching (vivido has it;
   `transcript --base64` exists but waits do not). Add the flag.
2. `transcript` has no `--raw` exact-bytes form (`--base64` covers the need;
   add `--raw` for CLI parity or document the decode one-liner and close).
3. Copy-mode selection text is not readable: `inspect` shows copy offset/
   row/column but the buffer is only reachable via the `Paste` action into a
   pane (no `copy_buffer` surface in `automation.rs`/`ipc.rs`). Add a
   bounded selection/buffer read.
4. `mouse` hardcodes a 30 s server timeout with no `--timeout` flag. Add it
   (fold into Gap 2 item 1).
5. `wait output` matches on the retained window; document the eviction/
   `sequence_gap` behavior to vivido's level (partially done via
   `dropped_before_offset`; check the wait path reports it the same way).

Explicitly not gaps: `drop-file` (local panes; `vvagent send --attach`
covers agent handoff), `ping` (a `capabilities` round-trip covers it; add
only if a health-check consumer asks), pixel `resize` (cells are the mux
unit; `resize-pane --columns/--rows` with committed-geometry reporting is
the right shape).

## Gap 7 — Skill/reference documentation gaps

1. **Scroll example is a live bug**: `references/commands.md` shows
   `mouse scroll --cell-column 1 --cell-row 1` with no `--scroll`, and the
   server rejects a zero notch count with `invalid_params`
   (`src/session.rs:6002-6007`). Fix the example.
2. **Relative mouse coordinates are supported but undocumented**: the CLI
   accepts `--relative-x/--relative-y` (per-mille on the wire,
   `src/automation.rs:1272-1285`) and single actions accept pixels; the
   reference shows cells only. Document all three forms and their
   attached/detached availability.
3. **No Output contract section**: vivido documents silent-on-success,
   `get-text` exact bytes, error shape, and stable codes. vvmux behavior
   mostly matches (`GetText`: "print pane text exactly, without a trailing
   newline") but it is tribal knowledge. Add the section, including the
   NDJSON-per-line rule and where `get-text`/`transcript --base64` differ.
4. **No attached-vs-detached capability matrix**: pixels, `wait rendered`,
   `capture --rendered`, `verify.rendered`, and unforced `--scale` all need
   an attached client; `visible: false` for every pane of a detached session
   (`references/commands.md:56`) surprises callers. Add one matrix.
5. **No protocol-notes equivalent**: framing, request/reply caps, connection
   limits, and endpoint authentication are not stated automation-side the
   way vivido's Protocol notes state them. Add a short section sourced from
   `src/ipc.rs` limits (or point at the authoritative doc if one exists).
6. The skill's event list names 7 kinds; `capabilities`/`EVENT_KINDS` carry
   13. Either list all or defer fully to `capabilities` (see Gap 5 item 3).

## Suggested order

- **Quick wins (docs + tiny flags):** Gap 7 items 1–4; Gap 6 items 1, 4.
- **P1 (feature parity):** Gap 2 items 1–2; Gap 3; Gap 5 items 1–2.
- **P2:** Gap 4; Gap 6 item 3; Gap 7 item 5; Gap 2 item 3 if needed.
- **Strategic (decide first):** Gap 1; Gap 5 item 2's shell-integration
  dependency is already satisfied by existing OSC handling — confirm, do not
  rebuild.

## Verification

Per `vvmux/AGENTS.md`, from `vvmux/` for every change:

```sh
cargo fmt --all --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

- New waits/mouse/plan features need real-socket regressions proving
  ordering and liveness, not state snapshots alone.
- Anything touching media projection/capture (`verify.rendered` delivery,
  Gap 1 options 2–3) needs the two-concurrent-producers-reusing-IDs
  treatment: fail one, prove the other intact via the virtual-presenter
  socket.
- Skill/reference edits need no test run, but every new flag or event in
  prose must name the `--help`/`capabilities` source it came from.
