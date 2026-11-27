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

Commands that allow focus fallback can target the focused pane. Pane recovery and close commands
require an explicit target or a same-session `VVMUX_PANE_ID`.

## Output contract

Structured reads and waits print one JSON value and a newline. Input commands such as `typing`,
`key`, `paste` and `submit` are silent on success unless given `--report`. `get-text` writes the
rendered UTF-8 pane text exactly, without adding a newline. `transcript --base64` returns a JSON
object containing the retained PTY bytes in `base64` plus offsets. `transcript --raw` writes those
bytes directly, including terminal control characters. Pipe raw output into a file or byte parser.
Subscriptions and plans emit one complete JSON object per line. On failure the CLI exits nonzero.
Server errors include the stable code and a human-readable message on stderr. Plan step errors
also appear in their NDJSON step records. Read the server's codes from `msg capabilities`.

## Local protocol

`vvmux msg` opens the named session's private VVMX control endpoint. On Unix the runtime directory
is owner-only, the socket has mode 0600, and both ends check the connected peer's UID. Windows
uses a per-user named pipe. For the methods and row-scoped waits below, the native client and
server use private VVMX version 28. Restart an older session server after upgrading. The 12-byte
preface carries the wire version, channel kind and maximum
record size. Each record has a 16-byte big-endian header with a sequence, type, flags and body
length. Control bodies are capped at 1 MiB and bulk bodies at 64 MiB. Automation replies can span
several control records up to 16 MiB in total. A server admits at most 32 concurrent client
connections. These are transport limits from `src/ipc.rs` and `src/server.rs`; the live
`capabilities.limits` lists method-specific bounds.

## Discovery

```sh
vvmux list --json
vvmux doctor --target SESSION --json
vvmux msg capabilities
vvmux msg session-inspect
vvmux msg list-clients
vvmux msg detach-client --client-id 4
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
the attached clients and which one presents media, revisions, queues, bridge state, and the `outer`
block. `list-clients` reports just the clients, the media presenter, and the shared layout size;
`detach-client` detaches one by its `client_id` and leaves the others attached.

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
vvmux msg mouse scroll --cell-column 1 --cell-row 1 --scroll -1 --pane-id 3
vvmux msg mouse click --relative-x 0.5 --relative-y 0.5 --pane-id 3
vvmux msg mouse path --point 12,20px --point 16,22px --duration 120ms --wait-rendered --timeout 2s --pane-id 3
```

Use zero-based cells for a grid application. `--relative-x` and `--relative-y` take fractions
from 0 to 1 and resolve to cells within the current pane. `--x` and `--y` take pane-local
physical pixels. Applications requesting SGR 1016 receive sub-cell positions from pixel inputs.
Path points accept `COLUMN,ROW`, `X,Ypx` or `X,Yrel`. A path takes 2 through 1,000 points and
delivers press, motion and release. `--duration` paces the sequence, and `--timeout` bounds it.
Use `--route application` to target the application even on another tab, or `--route mux` for mux
selection, focus or floating-pane interactions. See `vvmux msg mouse --help`.

| Operation | Attached | Detached |
|---|---|---|
| Cell and relative mouse input | Uses pane cell geometry | Uses pane cell geometry |
| Physical pixel mouse input | Requires nonzero cell metrics | Refused |
| `layout` visibility | Reports projected panes | Every pane reports `visible: false` |
| `wait rendered`, `capture --rendered`, `verify.rendered`, `mouse --wait-rendered` | Waits for a mux client render acknowledgement | No attached client can acknowledge |
| `get-text`, `get-grid`, `transcript`, `capture-media` | Read available mux state | Read available mux state |
| `capture-media --scale N` above 1 | Requires `--force` to resize visible content | Allowed with a suitable producer |
| Local Vivido pane screenshot | Requires a reachable local outer window and current crop | No outer window |

## Reading

```sh
vvmux msg get-text --pane-id 3 --rows 200
vvmux msg get-grid --pane-id 3
vvmux msg transcript --pane-id 3 --after-offset "$offset" --max-bytes 65536
vvmux msg search --pattern 'error' --pane-id 3
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

`get-selection --pane-id ID` returns JSON with the selected text and a truncation flag. With no
active copy-mode or mouse selection, `text` is null. The pane selection and the session's paste
buffer have separate lifetimes. See `vvmux msg get-selection --help`.

## Reset or respawn a pane

`vvmux msg reset-pane --pane-id 3` restores a blank primary screen and default input modes.
The running process, PTY, pane ID, name, slot and scrollback survive. The presenter retires that
pane's Vivid sessions. Its running child can authenticate again using the existing pane root.

`vvmux msg respawn-pane --pane-id 3` starts the retained command in its original working
directory, then replaces the pane in the same slot. The replacement inherits the pane name,
focus, zoom and transparency and starts with fresh history. Use the returned `pane_id` for the
new process. The response also includes `previous_pane_id`. A spawn failure leaves the original
pane and its output available, including a held pane whose process has exited. See
`vvmux msg reset-pane --help` and `vvmux msg respawn-pane --help`.

## Waiting

```sh
vvmux msg wait text 'ready' --pane-id 3 --lines-from-bottom 4
vvmux msg wait output 'AAE=' --base64 --pane-id 3 --after-offset "$offset"
vvmux msg wait output 'test result' --pane-id 3 --after-offset "$offset"
vvmux msg wait screen-change --pane-id 3 --after-screen "$screen"
vvmux msg wait screen-stable --pane-id 3 --quiet 3s --ignore-bottom 2 --timeout 10m
vvmux msg wait agent-state --pane-id 3 --until blocked,done
vvmux msg wait exit --pane-id 3
vvmux msg wait rendered --after-session "$session_sequence"
vvmux msg wait media --pane-id 3
vvmux msg wait media-track ...
```

`wait output --base64` decodes a literal byte pattern; use `--regex` for a UTF-8 regular
expression. To follow only new bytes, obtain `pane.output_offset` from `inspect` first. A wait
whose requested offset has been evicted fails with `sequence_gap`. Read `transcript` for its
`dropped_before_offset` and resume from a retained offset. See `vvmux msg wait output --help`.

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
vvmux msg resize-pane --pane-id 3 --columns 100
vvmux msg move-pane --pane-id 3 --to-tab 2
vvmux msg set-flag zoom --on --pane-id 3
vvmux msg sync-input --on --pane-id 3
vvmux msg save-layout --path work
```

`split` takes its axis positionally (`horizontal` puts the new pane beside, `vertical` below, as tmux
`-h`/`-v` do) and does not change the active tab.
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

To capture the local Vivido window's terminal text and producer pixels together, run the shipped
[attached-pane example](../../../examples/capture_vivido_pane.py) with `vvmux`, `vivido` and
ImageMagick's `magick` on PATH:

```sh
python3 examples/capture_vivido_pane.py MUX_SESSION PANE_ID NEW.png --vivido-session OUTER_SESSION
```

Use `--after-frame N` to require a later Vivido frame. The helper adds window padding to
`pane.outer_crop`, checks the frame metrics and rechecks the pane and attachment before writing.
From a separate client, supply the known outer session when the mux omits its name. Detached
sessions have no outer window. An outer client reached over vvssh must capture on the host
running Vivido.

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
vvmux msg lease renew ID
vvmux msg lease release ID
vvmux msg lease list
vvmux msg record start recording.vvmux
vvmux msg record stop
vvmux replay recording.vvmux
```

Get the complete event list with `vvmux msg capabilities | jq -r '.event_kinds[]'`. A filtered
subscription still reports gaps when journal entries have been evicted. See `msg subscribe --help`.

| Event | Payload fields |
|---|---|
| `pane.output` | `pane_id`, `from_offset`, exclusive `output_offset`, and `base64` for up to 64 KiB of raw PTY output |
| `pane.title_changed` | `pane_id` and the new `title` or null |
| `pane.directory_changed` | `pane_id` and the `uri` from a shell OSC 7 directory report |
| `pane.bell` | `pane_id` and the number of bells in `count` |

`inspect` still provides process cwd where available for shells that emit no OSC 7 directory reports.
Output offsets are per pane. Journal sequence numbers from `subscribe --after` are per session.
See the running server's `capabilities.event_kinds` for availability.

A plan holds 1 through 256 steps, occupies at most 1 MiB and runs over one connection. `bind` saves an action result by JSON Pointer and
`{"$ref":"alias"}` uses the value in a later step. `--preflight` skips mutations. A step's
`verify` can wait for a newer screen or an acknowledged render. An `assert` checks the action
reply, the pane viewport, or both. For example:

```json
{"version":1,"steps":[
  {"id":"pane","method":"inspect","pane_id":1,
   "assert":{"result_pointer":"/pane/pane_id","result_equals":1}},
  {"id":"ready","method":"inspect","pane_id":1,
   "assert":{"text_contains":"ready","lines_from_bottom":4,"timeout_ms":5000}}
]}
```

Text checks default to the step's pane. `assert.pane_id` can select another pane by number or
backward reference. Omitting `lines_from_bottom` searches the full displayed viewport. A window
of 1 through 1,000 physical rows follows changes until the timeout (5 s by default, 24 h maximum).
Result comparisons use exact JSON equality, including null, and an empty pointer addresses the
whole action reply. A failed assertion obeys the step's `on_error` policy. A continuing plan
still exits nonzero if any step failed.

For a CI job with a fresh detached server, run the following in one shell. Use a plan whose checks
match the pane's actual state. See `vvmux msg run-plan --help`.

```sh
session="plan-$$"
vvmux new --detached --session "$session" || exit 1
trap 'vvmux kill-session --target "$session" >/dev/null' EXIT
vvmux msg --target "$session" run-plan --file plan.json --report junit --output steps.xml >steps.ndjson
```

`--report sarif --output steps.sarif` produces SARIF 2.1.0. Both formats include visited steps,
including skips and the step that triggered an abort. The streaming NDJSON stays on stdout.

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
