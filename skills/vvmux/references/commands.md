# vvmux automation commands

Every flag here comes from the shipped CLI, but `vvmux msg capabilities` is the authority for the
release you are talking to. Run `vvmux msg <command> --help` rather than reconstructing an
unfamiliar request.

## Global options

These are declared once on `msg` rather than per subcommand, because which pane a request addresses
is a property of the request, not of the verb:

| Option | Meaning |
|---|---|
| `--target SESSION` | which session; `vvmux list --json` finds one |
| `--pane-id N` | the pane, by ID within this server run |
| `--pane-name NAME` | the pane, by a name that survives a server restart |
| `--alias NAME` | the pane whose *agent* carries this name |
| `--expect-screen N` | refuse unless the pane's screen sequence is still this |
| `--expect-session N` / `--expect-layout N` | the same for session and layout sequences |
| `--idempotency-key KEY` | apply at most once however many times it is sent; mutating methods
only |
| `--lease ID` | act under a lease another caller would otherwise block |

With no pane selector, a request targets the focused pane. `VVMUX_PANE_ID` is accepted only when it
belongs to the target session.

## Discovery

```sh
vvmux list --json
vvmux doctor --target SESSION --json
vvmux msg capabilities
vvmux msg session-inspect
vvmux msg list-tabs
vvmux msg list-panes
vvmux msg layout
vvmux msg inspect --pane-id 3
vvmux msg snapshot
```

`capabilities` returns `methods`, `method_capabilities` (each with `class` and `mutating` — only
`observe` is non-mutating), `limits`, `error_codes`, `event_kinds`, `completion_waits`, `plugins`,
and the protocol version. Sixty-odd error codes: branch on them, not on message text.

`layout` is the only call carrying the split tree. Its shape:

```
active_tab_id, area, caller, layout_sequence, session, session_instance, session_sequence
tabs[]   tab_id, tab_name, display_index, active, focused_pane_id, sync_input, panes[]
  panes[]  pane_id, pane_name, agent_alias, locator{tab_id,tab_name,pane_id,pane_name},
           split_path, geometry, content_geometry, neighbors{left,right,up,down},
           focused, visible, zoomed, layer, title, is_caller
```

`display_index` is zero-based and mutable — never address a tab by it. `neighbors` values are pane
IDs. `visible` is about projection: every pane of a detached session reports `false`.

`inspect` returns `{pane, session, session_sequence, layout_sequence, rendered_session_sequence,
limits}`. Everything about the pane is under `pane`: `screen_sequence` and `output_offset`, the
`agent` block, `pending_resume`, `process` and `process_state`, `screen` (`primary` or `alternate`),
`retained_output_from_offset`, `media`, `plugin`, and `outer_crop`. `session-inspect` carries
attachment, revisions, queues, bridge state, and the `outer` block.

## Routing to a pane

```sh
vvmux msg resolve-pane --path left
vvmux msg resolve-pane --path right,down
vvmux msg resolve-pane --tab-name build --path down
vvmux msg resolve-pane --pane-name editor
vvmux msg activate-pane --pane-id 3
vvmux msg focus --pane-id 3
```

A route starts at the caller's pane. Each direction is **one navigation step** using the same
neighbor graph `action focus` uses, so it can land on a pane not strictly on that side; repeat
directions to cross nested splits (`resolve_pane_steps` caps the route at 32). `--tab-id` or
`--tab-name` routes in another tab.

`activate-pane` reveals a pane — selecting its tab and lifting a zoom that covers it — **without**
moving focus or disturbing the attachment. Media projection keys off visibility, so revealing and
focusing are different requests. `focus` does move focus.

## Naming

```sh
vvmux msg pane-rename --pane-id 3 --name editor
vvmux msg pane-rename --pane-id 3 --clear
vvmux msg agent-rename --pane-id 3 --name reviewer
vvmux msg rename-tab --tab-id 2 --name build
vvmux msg reset-tab-title --tab-id 2
```

Names are unique per session (`pane_name_taken`, `agent_alias_taken`). A pane name is written into
the session snapshot and survives a server restart, which pane IDs do not. An agent name belongs to
the agent process and is cleared when it exits.

## Input

```sh
vvmux msg typing 'cargo test' --pane-id 3 --report
vvmux msg key Enter --pane-id 3
vvmux msg paste "$(cat notes.txt)" --pane-id 3
vvmux msg submit 'cargo test' --pane-id 3          # typing plus Enter
vvmux msg signal INT --pane-id 3
vvmux msg shell-command 'cargo test' --pane-id 3
```

`--report` on `typing`, `key`, and `paste` proves the bytes reached the PTY writer — not that the
child consumed them. `submit` is the right shape for sending a line to something already running.

`shell-command` runs in the pane's own shell, so aliases, virtual environments, and the current
directory apply, and it returns the command's **real exit status**. It requires the shell to emit
OSC 133 markers and refuses when it does not; that refusal is information, not an obstacle — fall
back to `submit` plus `wait output`.

`signal` reaches the foreground process group; the reply's `foreground_job` says whether that was a
running job or a shell at its prompt.

## Mouse

```sh
vvmux msg mouse click --cell-column 12 --cell-row 4 --pane-id 3
vvmux msg mouse path --point 10,4 --point 12,6 --point 14,8 --button left --pane-id 3
vvmux msg mouse scroll --cell-column 1 --cell-row 1 --pane-id 3
```

Actions: `move`, `click`, `double-click`, `down`, `up`, `drag`, `path`, `scroll`. Coordinates are
**pane-local cells**, so no hit testing is needed and `--route application` reaches a pane that is
hidden, on another tab, or under a zoom. `--route mux` is for vvmux's own handling — copy-mode
selection, float drag, pane focus. Pixel coordinates (`--x`/`--y`) need an attached client and are
refused without one.

Send a drag as one `mouse path` with `--point` repeated, never as separate down/move/up calls: a
failure between them leaves a button held. Up to 1,000 points.

## Reading

```sh
vvmux msg get-text --pane-id 3 --rows 200
vvmux msg get-grid --pane-id 3
vvmux msg transcript --pane-id 3 --after-offset "$offset" --max-bytes 65536
vvmux msg search 'error' --pane-id 3
vvmux msg capture --pane-id 3 --stable 500ms --no-activate
```

`capture` is reveal, settle, and read as one request rather than three with races between them:
`--no-activate` skips the reveal, `--stable` waits for a quiet screen, `--rendered` waits for the
attached client to acknowledge a composite render, and `--grid` returns structure instead of text.

**Which reader to use.** A pane overwrites its own output, so the grid holds only what replaced it:

- `transcript --after-offset` — right for a quiet program, and the only thing that recovers output
  the terminal has scrolled past. Check `dropped_before_offset`. The retained window is
  `transcript_bytes_per_pane` (256 KiB), bounded in **bytes**.
- `get-text --rows N` — reads scrollback with soft wraps joined. Right for a long answer from an
  agent that paints a status bar: a bar repainting once a second burns through the byte-bounded
  transcript in minutes, evicting the answer while keeping the ticks. Measured on a real agent pane,
  a transcript spanning the whole run held one mention of the subject and hundreds of counter
  repaints, while the scrollback held all of it.

Read `inspect`'s `pane.output_offset` **before** submitting anything whose answer you intend to
collect. It is the only way to ask for exactly the reply afterwards.

## Waiting

```sh
vvmux msg wait text 'ready' --pane-id 3
vvmux msg wait output 'test result' --pane-id 3 --after-offset "$offset"
vvmux msg wait screen-change --pane-id 3 --after-screen "$screen"
vvmux msg wait screen-stable --pane-id 3 --quiet 3s --ignore-bottom 2 --timeout 10m
vvmux msg wait agent-state --pane-id 3 --until blocked,done
vvmux msg wait exit --pane-id 3
vvmux msg wait rendered --pane-id 3
vvmux msg wait media --pane-id 3
vvmux msg wait media-track ...
```

`wait media-track` needs the complete producer/context/surface/track identity. Use `focus` or
`select-tab --wait outer` when media projection is the assertion and `--wait rendered` for the
attached terminal frame.

## Layout mutation

```sh
vvmux msg split horizontal --pane-id 3
vvmux msg run 'htop' --pane-id 3
vvmux msg new-tab
vvmux msg select-tab --tab-id 2
vvmux msg close-tab --tab-id 2
vvmux msg close-pane --pane-id 3
vvmux msg resize-pane --pane-id 3 --width 100
vvmux msg move-pane --pane-id 3 --tab-id 2
vvmux msg set-flag zoom --on --pane-id 3
vvmux msg sync-input --on --pane-id 3
vvmux msg save-layout --name work
```

`split` takes its axis positionally (`vertical` or `horizontal`) and does not change the active tab.
`set-flag` takes `zoom`, `pinned`, `transparent`, `copy-mode`, `floats-visible`, or `sync-input`,
with `--on`/`--off` — always prefer it to `action toggle-*`, which cannot be replayed. Creation
replies carry the new IDs; rediscover with `layout` rather than predicting them.

A session's shape survives its server restarting, so do not rebuild it by hand after one.
`snapshot` reports whether this session came from a snapshot and when it was last written.

## Media

```sh
vvmux msg capture-media --pane-id 3 --out page.png
vvmux msg capture-media --pane-id 3 --out page.png --scale 2 --force
vvmux msg inspect-media --pane-id 3
vvmux msg trace-media --pane-id 3 --after SEQUENCE --follow
```

`capture-media` writes what a pane presents, composed from media vvmux already holds — it works
detached, with the pane hidden, and with no Vivido. It is **not** a screenshot: terminal text
belongs to the presenter's renderer and never appears, and pixels are never returned inline.

`skipped` explains a blank capture. `undecoded_video` is an encoded source (vvland, vvcam) relayed
without decoding, so no scale or retry helps. `no_retained_pixels` is a track whose frame has not
landed, so retrying is worthwhile. `--scale N` asks viewport-sized producers (`vvrd`, `vrowser`) to
re-render denser; a producer whose raster is its own document (`vvpaint`) is already at full
resolution. Scale is refused while a client is attached unless `--force`, since it visibly resizes
the pane and changes it back.

## Agents

```sh
vvmux msg agent-start --kind codex --pane-id 3
vvmux msg agent-prompt --pane-id 3 --wait --until blocked,done 'review the diff'
vvmux msg agent-send-keys --pane-id 3 Escape
vvmux msg agent-read --pane-id 3 --lines 200
vvmux msg agent-explain --pane-id 3
vvmux msg report-agent ... | report-agent-session ... | clear-agent-report ...
```

`agent-start` verifies the pane is a shell at its prompt, quotes arguments for that shell, and
returns only once the agent is detected and settled. `agent_pane_busy` means the pane is running
something else; `agent_not_launchable` means that provider is detection-only.

`agent-prompt` separates prompt text from Enter and waits on lifecycle rather than screen text.
`agent-send-keys` carries a bounded allow-list of control and navigation keys. `agent-read` needs an
*idle* alternate-screen agent and restores the viewport afterwards; when its gates do not hold, ask
the agent to write a Markdown file and read the file.

`agent-explain` names the rule that decided a pane's state and shows every rule's evidence. Use it
before changing anything when a state looks wrong — `diagnose` covers infrastructure, not
classification.

To watch many panes, stream `subscribe --name agent.status_changed` instead of polling each one.
Treat a `gap` record as missed events, not an error; it is never filtered out.

### Driving an agent with no provider plugin

`inspect`'s `pane.agent` is `null` and the agent commands are unavailable. That is a missing
package, not a fault — offer `vvmux plugin install NAME` first, because this fallback loses
lifecycle waiting and conversation resume. To proceed anyway:

1. Read `inspect`'s `pane.output_offset` **before** submitting. Without it `transcript` cannot
   separate the reply from everything before it, and the grid holds only the last screen.
2. Send with `submit`.
3. `wait screen-stable --quiet 3s --ignore-bottom N`, sized to the agent's status bar, with a
   timeout matching the work rather than the default. **Without `--ignore-bottom` this never
   fires** for an agent painting a spinner, elapsed timer, or token counter: the screen changes
   every second for as long as the agent works, which is exactly the interval being waited on.
4. Collect according to `inspect`'s `pane.screen`, worth reading *before* the answer arrives:
   - `primary` — ordinary scrolling output. `get-text --rows N`, asking for more rows than the
     viewport holds. Prefer it to `transcript` for a status-bar agent, for the byte-bound reason
     above.
   - `alternate` — a full-screen TUI. No scrollback to walk, and `transcript` is a stream of
     redraws, so neither collector works. A reply that fits the viewport reads with
     `capture --no-activate`; for anything longer, ask the agent to write a file and read it —
     exact, and one round trip instead of scrolling and stitching.

A quiet screen is weaker evidence than a lifecycle state: an agent that pauses mid-answer looks
finished.

## Events, plans, leases, recording

```sh
vvmux msg subscribe --name pane.screen_changed --name agent.status_changed
vvmux msg run-plan --file plan.json --preflight
vvmux msg lease acquire --scope input --pane-id 3 --holder my-name
vvmux msg lease renew --lease ID
vvmux msg lease release --lease ID
vvmux msg lease list
vvmux msg record start recording.vvmux
vvmux msg record stop
vvmux replay recording.vvmux
```

Event kinds include `session.started`, `pane.opened`, `pane.exited`, `pane.closed`,
`pane.screen_changed`, `agent.status_changed`, `layout.changed`, and `focus.changed`; take the full
list from `capabilities`.

A plan runs over one connection, `bind` carries a result into a later step by JSON Pointer, and
`{"$ref":"alias"}` substitutes it, so pane IDs never have to be parsed out and passed back by hand.
References are backward-only and the whole plan is validated before any of it runs. `--preflight`
runs only the observations; `verify` on a step puts a newer screen or an acknowledged render in the
same transaction as the action.

Leases are advisory: an unleased pane is open to anyone. Every lease expires, so renew a long job
rather than asking for a long TTL, and release it when done.

`record` captures input classes and lengths but never what was typed — it does write pane output to
a file, so start one only when the user has asked for it.

## Diagnostics

```sh
vvmux msg diagnose --all-panes --trace-limit 512
vvmux doctor --target SESSION --json
vvmux debug-bundle --target SESSION
```

`diagnose` is one non-blocking correlated snapshot. Keep `debug-bundle` metadata-only unless the
user explicitly authorizes pane grid, text, or log content.
