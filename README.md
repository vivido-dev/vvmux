# vvmux

`vvmux` is a detachable terminal multiplexer for Vivido. A persistent per-session server owns
shell PTYs, terminal grids, scrollback, layout, and virtual Vivid producer state. The foreground
client owns the current terminal and the outer Vivido capability, so detaching never gives a
background daemon access to a window token.

The implementation requires Rust 1.88 or newer and supports macOS, Linux, Windows 11, and Windows
10 version 1809/build 17763 or newer on `x86_64-pc-windows-msvc`. It does not import or depend on
the untracked Zellij or Alacritty reference trees. Windows ARM64 is compile-checked but remains a
follow-up runtime target.

## Build and install

```sh
cd vvmux
cargo build --release
tic -x -o "$HOME/.terminfo" terminfo/vvmux.info
install target/release/vvmux "$HOME/.local/bin/vvmux"
```

The primary Windows distribution is the signed Vivido Suite EXE/MSI. It installs `vvmux.exe`
beside Vivido, Vivi, and vvssh below `%LOCALAPPDATA%\Programs\Vivido`, adds that directory to the
user PATH, refuses upgrade or uninstall while a live session exists, and preserves
`%APPDATA%\vvmux`, including `config.toml`. The older signed ZIP scripts remain available only for
transition/testing and are not the primary public release path.

When the terminfo entry cannot be found, pane shells use `TERM=xterm-256color`. They always receive
`TERM_PROGRAM=vvmux` and `COLORTERM=truecolor`.

On Windows, the configured `[general].shell` is used first, then `%COMSPEC%`, then the system
`cmd.exe`. The default config is `%APPDATA%\vvmux\config.toml`; owner-only runtime registries live
below `%LOCALAPPDATA%\vvmux\runtime`. Pane shells receive an exact `127.0.0.1` virtual Vivid endpoint
and `VIVID_ANCHOR_TRANSPORT=conpty`. Remote Unix applications may still need the supplied
`terminfo/vvmux.info` installed on the remote host.

## Commands

```text
vvmux                              attach/create `default`
vvmux new [-s NAME] [-d]           create a session
vvmux attach [-t NAME] [--replace] attach exactly by name
vvmux list                         list live owner sessions
vvmux kill-session -t NAME         terminate a session and its process groups
vvmux msg [-t NAME] COMMAND        automate or inspect one pane directly
vvmux plugin COMMAND               install, inspect, and invoke typed plugins
vvmux token create [--rotate]      create/rotate the VVWS bearer token
vvmux serve [OPTIONS]              run the loopback VVWS/1 session gateway
vvmux --config PATH ...            use an explicit strict TOML config
```

Only one client can be attached to a session. `--replace` sends a clean detach to the old client
before the new client is admitted.

## Pane automation

`vvmux msg` connects directly to the owner-only hidden session server. It does not type vvmux
prefix keys and does not replace the foreground client, so an attached Vivido window keeps
rendering normally while another shell controls or observes individual panes.

The session target is resolved from `--target`, then `VVMUX_SESSION`, then `default`. A pane target
is resolved from `--pane-id`, then a same-session `VVMUX_PANE_ID`, then the focused pane in the
active tab. `close-pane` deliberately has no focused fallback. Pane shells inherit the exact
`VVMUX_SESSION`, `VVMUX_TAB_ID`, and `VVMUX_PANE_ID` values for their owner.

```sh
export VVMUX_SESSION=agent

right=$(vvmux msg split vertical --pane-id 1 | jq -r .new_pane_id)
bottom_left=$(vvmux msg split horizontal --pane-id 1 | jq -r .new_pane_id)
bottom_right=$(vvmux msg split horizontal --pane-id "$right" | jq -r .new_pane_id)

vvmux msg typing --pane-id "$right" 'echo hello from top-right'
vvmux msg key --pane-id "$right" Enter
vvmux msg wait text --pane-id "$right" 'hello from top-right'
vvmux msg get-text --pane-id "$right"
```

Available commands are:

```text
capabilities
reload-config
list-panes
report-agent --agent claude|codex|opencode|hermes --state idle|working|blocked --source ID --sequence N [--pane-id ID]
clear-agent-report --source ID --sequence N [--pane-id ID]
inspect [--pane-id ID]
inspect-media [--pane-id ID]
split vertical|horizontal [--pane-id ID]
run COMMAND [--placement split|float|tab] [--axis vertical|horizontal] [--cwd DIR] [--hold] [--no-focus] [--pane-id ID]
focus [--pane-id ID]
close-pane --pane-id ID
typing TEXT [--pane-id ID]
key KEY [--mods Shift,Alt,Ctrl,Super] [--repeat N] [--pane-id ID]
paste TEXT [--pane-id ID]
get-text [--rows N] [--pane-id ID]
get-grid [--start-line LINE --row-count N | --since-screen SEQ] [--pane-id ID]
search --pattern TEXT [--regex] [--direction forward|backward] [--start-line LINE --start-column COLUMN] [--limit N] [--pane-id ID]
sync-input (--on|--off) [--pane-id ID]
wait text TEXT [--regex] [--after-screen SEQ] [--timeout DURATION] [--pane-id ID]
wait screen-change [--after-screen SEQ] [--timeout DURATION] [--pane-id ID]
wait screen-stable [--quiet DURATION] [--after-screen SEQ] [--timeout DURATION] [--pane-id ID]
wait rendered --after-session SEQ [--timeout DURATION]
wait exit [--timeout DURATION] [--pane-id ID]
wait media [--after-virtual REV] [--after-outer REV] [--timeout DURATION] [--pane-id ID]
```

`typing`, `key`, and `paste` bypass prefix and copy-mode handling and acknowledge only after the
pane PTY writer flushes all generated bytes. `paste` honors bracketed-paste mode and prevents an
embedded bracketed-paste terminator. Keys include Unicode scalars, navigation keys, F1 through
F35, and keypad keys; cursor and keypad application modes are honored.

`run` opens a pane for one command. The command is handed to the configured shell with `-c`, so
pipes, redirection, and shell syntax work; quote it as a single argument. It is a command line,
not an argument vector. `--cwd` is resolved against the caller's working directory before it is
sent, since the session daemon does not share it.

```sh
vvmux msg run 'cargo watch -x test' --placement float
vvmux msg run 'make 2>&1 | tail -40' --hold
```

By default the pane closes when the command exits. `--hold` keeps it open with a `[exited N]`
note in the pane and an `[exited]` marker in its frame, so short-lived output stays readable;
`wait exit` resolves either way. Typing into an exited held pane closes it, because its PTY input
queue is no longer available.

Agent reports are authoritative for their pane until cleared or the foreground process group
changes. They require an explicit pane ID or a same-session `VVMUX_PANE_ID`; they never guess the
focused pane. Sequence numbers are monotonic per pane and source, and `done` is derived by vvmux
rather than accepted from reporters. `list-panes` and `inspect` expose the effective agent state.

OpenCode can report lifecycle events directly through the managed optional plugin:

```sh
vvmux integration install opencode
vvmux integration status opencode
vvmux integration uninstall opencode
```

The installer owns only `~/.config/opencode/plugins/vvmux-agent-state.js` and refuses to replace
or remove a file without the vvmux ownership marker. Claude Code, Codex CLI, and Hermes need no
installation; vvmux identifies their foreground processes and terminal UI signals passively.

## Startup layouts

`vvmux new --layout NAME` loads `~/.config/vvmux/layouts/NAME.toml`; an existing path can be
passed instead. `[general].default_layout` supplies the name or path when `--layout` is omitted.
An explicitly requested missing or invalid layout fails before the daemon forks. A missing default
layout prints a warning and starts the usual one-shell tab.

```toml
[[tabs]]
name = "dev"
focus = "shell"

  [tabs.layout]
  split = "vertical"
  sizes = [30, 70]

    [[tabs.layout.children]]
    pane = "editor"
    command = "nvim ."
    cwd = "~/src/project"

    [[tabs.layout.children]]
    pane = "shell"

  [[tabs.floating]]
  pane = "notes"
  width_percent = 50
  height_percent = 60
  pinned = true
```

Splits may nest and contain 2–16 children. `sizes` are relative integer weights from 1 through
1000; omitting them gives every child equal weight. A layout may contain up to 16 tabs and 64
panes total. Each pane label is tab-local and unique, and `focus` names one of those labels.
Floating-only tabs are valid. `command` is one shell command line passed to `shell -c`, `cwd`
accepts `~/`, and `hold = true` preserves command output after exit. If one pane cannot spawn, its
leaf is removed and its siblings keep their exact owner-scoped layout; a layout where every pane
fails falls back to one shell tab.

`get-text` writes exact Unicode without adding a newline. Its default view is the pane as vvmux is
currently displaying it, including copy-mode scroll position. `--rows N` instead returns the newest
N physical rows ending at the live bottom. Trailing unused cells are removed, soft-wrapped rows are
joined, hard row boundaries remain newlines, and literal tabs, combining text, and wide characters
are preserved.

`get-grid` prints JSON with pane/screen/session sequences, signed retained-grid line numbers,
viewport rows, cursor and copy selection, active screen and modes, a deduplicated symbolic style
table, and every physical cell. Colors are `default`, indexed, or RGB because the hidden server
does not own the outer Vivido palette. Cells retain tabs, styled blanks, wide continuations,
combining characters, hyperlinks, and wrap state. `--since-screen` returns current replacement
rows when retained history is sufficient; `full` plus `gap` identifies an evicted or invalidated
delta. Vivid media, pane frames, status lines, and other panes are intentionally excluded.

`search --pattern TEXT` reports bounded structured matches across the pane's physical live and
scrollback rows; `--regex` enables regular expressions, `--direction` and `--start-line` choose the
scan origin, and `--limit` reports truncation rather than growing an unbounded reply. Wrapped rows
remain separate in this version, matching interactive copy-mode search.

Structured observations and waits print JSON; `split` prints the new pane ID and committed session
sequence. Input, focus, and close are silent on success. Waits default to 30 seconds and accept
durations from 1 ms through 24 hours. `wait rendered` means the attached client wrote and
acknowledged a composite terminal frame covering the requested session sequence; it does not claim
that Vivido presented a GPU frame. Use `vivido msg wait frame` when GPU presentation matters.

`inspect-media` reports only pane-scoped, sanitized metadata: virtual scene/projection revisions,
the independently acknowledged outer projection revision, separate `surfaces` and `tracks`,
complete inner identity, independent inner/outer channel generations, milestones, bounded
queue/flow utilization, and node geometry. It never includes root secrets, channel keys,
authenticators, payload bytes, or hashes. `wait media` waits for either requested revision domain
to advance; when both
`--after-virtual` and `--after-outer` are supplied, both predicates must become true.

Requests and input are limited to 1 MiB, decoded replies to 16 MiB, row requests and key repeats to
1,000, and regular expressions to 8 KiB. The server bounds connections, in-flight requests,
waiters, response work, screen-delta history, and recent process-exit tombstones. VVMX 14 is a hard
private-protocol cutover, so sessions created by older binaries must be restarted after upgrading.

## Network session gateway

The optional default-enabled `server-capability` builds `vvmux serve`, a foreground loopback-only
WebSocket gateway for xterm.js-style clients. It is one gateway for all owner sessions; individual
hidden session servers remain private and continue to use same-user VVMX IPC.

Create a token, configure at least one exact browser origin, and start the gateway:

```sh
vvmux token create
vvmux serve --allow-origin http://127.0.0.1:3000
```

The default address is `127.0.0.1:7880`. Non-loopback binds, missing/`null`/wildcard origins, and
unauthenticated discovery are rejected. Use SSH port forwarding or a trusted TLS reverse proxy for
remote access. Possession of the bearer token is equivalent to shell access to every vvmux session
owned by that OS user. The raw token is printed once; only its hash is retained in an owner-only
record.

The gateway lists, creates, and exclusively attaches to sessions. It serves no HTML or JavaScript
and does not expose session kill operations on the loopback listener. Plain xterm.js clients can
attach text-only; the byte-transparent Vivid route accepts only Vivid 1.5 Control and Track
connections, uses the route's ephemeral 32-byte secret as its Vivid root secret, and advertises
`wire_version: "1.5"`. See [VVWS-1.md](VVWS-1.md) for the normative wire contract and client
integration shape.

### Connect mode: serve from anywhere, without opening a port

`vvmux serve --connect https://host --acknowledge-content-visible-gateway` opens no listener at
all. The gateway authenticates to a vvmux_server deployment with an enrolled Ed25519 identity and
holds one outbound VVTUN/1 tunnel; the deployment dials one additional leg per browser socket, and
the same VVWS/1 and Vivid loops run on the legs. This is how a machine behind NAT becomes reachable
from a browser anywhere.

```sh
vvmux cloud enroll --server https://vvmux.example
vvmux serve --connect https://vvmux.example --acknowledge-content-visible-gateway \
  --allow-account 'https://accounts.google.com#1234567890'
```

`vvmux cloud enroll` reads the one-time code from a no-echo prompt, generates the identity, submits
only the public key, and stores the private key in an owner-only file. For automation,
`--code-file PATH` reads a bounded one-line code from a file and `--code-file -` reads it from stdin.
The code and private key never appear in argv, an environment variable, or a log. The identity path
is reserved before the code is read or consumed.

The tunnel gateway uses the tunnel-asserted VVWS authentication of
[VVWS-1.md](VVWS-1.md) — the browser sends `{"type":"hello","protocol":1,"auth":"tunnel"}` and no
bearer token is involved. It advertises `tunnel-attached-v1`, and `session-kill-v1` only when
started with `--allow-kill`, which additionally enables the `kill_session` VVWS control.

`--allow-account` is repeatable and bounds which authenticated accounts the deployment may present
when opening legs; without it, any account is accepted. The visibility acknowledgement is required
for a non-loopback deployment because the relay necessarily sees terminal and media bytes. An
`https://` deployment base is canonical; an exact `wss://.../t/v1/control` URL forces the WebSocket
mapping. Plain `http://` or `ws://` is accepted for loopback development only. The tunnel reconnects
with full-jittered exponential backoff and survives a deployment restart; sessions are untouched by
tunnel loss, because the hidden session daemon is fully detached from the gateway. See
[VVTUN-1.md](VVTUN-1.md) for the tunnel protocol.

## Default keys

The prefix is `Ctrl-b`.

| Key | Action |
|---|---|
| `Ctrl-b Ctrl-b` | Send a literal `Ctrl-b` |
| `Ctrl-b %` / `Ctrl-b "` | Split left/right or top/bottom |
| `Ctrl-b Arrow` | Focus direction |
| `Ctrl-b Ctrl-Arrow` | Resize by one cell |
| `Ctrl-b c`, `n`, `p`, `0`–`9` | Create/cycle/select tabs |
| `Ctrl-b x`, then `y` | Close the focused pane |
| `Ctrl-b z` | Toggle zoom |
| `Ctrl-b S` | Toggle synchronized input for the active tab |
| `Ctrl-b a` | Open or close the AI-agent navigator |
| `Ctrl-b f` / `Ctrl-b F` | Create a floating pane / show or hide ordinary floats |
| `Ctrl-b P` | Pin or unpin the focused floating pane |
| `Ctrl-b m` / `Ctrl-b r` | Enter floating move / resize mode |
| `Ctrl-b d` | Detach |
| `Ctrl-b [` / `Ctrl-b ]` | Copy mode / paste copy buffer |

The agent navigator includes detected agents from every tab and orders them blocked, done,
working, then idle. Arrows or `j`/`k` select, Home/End and Page Up/Down scroll, Enter jumps to the
pane, and `q` or Escape closes it. Mouse wheel and row clicks are supported. The popup is a
transient compositor overlay, not a shell pane, so it never changes layout or media ownership.

Copy mode accepts arrows, Page Up/Down, Space to start selection, Enter to copy, and `q` or Escape
to cancel. `/` and `?` open forward and backward smart-case regular-expression search; `n` repeats
in the original direction and `N` repeats in the opposite direction. Search operates on bounded
physical terminal rows, including scrollback; wrapped rows are not joined in this version. Copies
are capped at 1 MiB and the client emits OSC 52. Paste honors the focused
application's bracketed-paste mode and neutralizes embedded bracketed-paste terminators.
On Windows, the outer terminal's bracketed-paste mode follows the focused pane, so `Ctrl+V` from
Windows Terminal or Vivido is delivered with bracket markers only when that pane requested them.

In floating move or resize mode, arrows step by one cell and Shift-Arrow steps by five. Enter
commits and Escape restores the rectangle captured on entry. Zoom hides every other pane, including
pinned floats, without mutating the tiled tree, floating rectangles, visibility, pins, z-order, or
focus.

Synchronized input fans ordinary typing and paste out to every live tiled and floating pane in the
active tab, including hidden floats and panes hidden by zoom. Copy-mode panes are excluded, and a
focused copy-mode pane suppresses fan-out entirely. The setting belongs to one tab and does not
follow tab switches. Every frame title and the status row show `sync` while it is enabled.

Mouse clicks focus panes. Tiled border drags resize. On a floating pane, the top title frame moves
the pane, while side/bottom frames and corners resize it; drag geometry is based on the press-time
rectangle and total pointer delta. Mouse input is translated into pane-local SGR coordinates when
the application requested mouse reporting. Otherwise, a left-button drag selects text inside the
pressed pane and copies it through OSC 52 on release; triple-click selects one displayed row, and
triple-click-drag extends by displayed rows. Selection is clipped to the pressed pane even if the
pointer crosses another tiled or floating pane, and the highlight remains until input, output, a
layout change, or the next click invalidates it. Copy mode (`Ctrl-b [`) owns the same gestures even
when the program in the pane requested mouse input.

A Shift-modified left gesture also forces pane-local selection when the outer terminal forwards
the standard SGR mouse report. Many terminals instead reserve Shift for their own native selection
while application mouse tracking is active; those gestures never reach vvmux and can still select
across pane boundaries. Use the unmodified gesture (or configure the outer terminal to forward
Shift) when pane-bounded selection is required. Shift prevents pane-frame dragging when it is
forwarded.

Strict floating defaults live under `[floating]`: `default_width_percent` and
`default_height_percent` accept 10–100, and `border_drag_margin` accepts 1–4. New floats are centered
at 60% by 60% by default, with a minimum 4-by-2 content area plus frame.

## Configuration reload

A running session picks up config edits without restarting. Three triggers reach the same
parse-validate-apply path:

```sh
# Edit the file: the session notices within a couple of seconds.
vvmux msg -t NAME reload-config      # or reload now, and report what happened
kill -USR1 $(pgrep -f "__server --session NAME")
```

The watcher polls the config path about once a second and waits for the file to stop changing
before acting, so an editor that truncates and rewrites is never read half-written. A config file
that is deleted is not treated as a change: the last good config stays in force.

A file that fails to parse or validate is rejected outright and the running session keeps its
current config, so a config saved mid-edit cannot degrade a live session. `msg reload-config`
reports the failure as `invalid_config`; the watcher and SIGUSR1 report it on the status line.

Not everything can change under a live session, and `msg reload-config` names what it did:

| Section | On reload |
|---|---|
| `[theme]`, `[appearance]` | Applied immediately, with a full repaint |
| `general.status_visible` | Applied; every pane is resized around the status row |
| `general.render_interval_ms` | Applied on the next loop iteration |
| `[floating]`, `[keys.copy]` | Applied the next time they are used |
| `general.shell`, `default_cwd`, `scrollback_lines` | Reported as `deferred`: they apply to panes spawned afterwards |
| `general.default_layout` | Reported as `deferred`: it applies to the next session created |
| `general.prefix`, `[keys.prefix]` | Reported as `deferred`: the attached client owns the prefix parser until it reattaches |
| `[media]` | Reported as `ignored`: the running virtual presenter owns it, and swapping it would strand live media |
| `[server]` | Reported as `ignored`: only `vvmux serve` reads it, in its own process |

## Theming

Frame and status colors live under `[theme]`. Every color is a string in one of four forms:

| Form | Example | Meaning |
|---|---|---|
| `"default"` | `"default"` | The terminal's own default color |
| Index | `"12"` | A palette index, 0 through 255 |
| ANSI name | `"bright-blue"` | One of the 16 names, `black` … `bright-white` |
| Truecolor | `"#ff8800"`, `"#f80"` | 24-bit RGB |

`preset` supplies every color at once; individual keys override it. The presets are `default`,
`mono`, `nord`, `solarized-dark`, and `gruvbox-dark`.

```toml
[theme]
preset = "nord"
active_frame = "#ff8800"
```

The keys are `preset`, `active_frame`, `inactive_frame`, `active_title`, `inactive_title`,
`frame_background`, `status_foreground`, `status_background`, `status_fill`,
`search_match_foreground`, `search_match_background`, `search_current_foreground`, and
`search_current_background`, plus the status-row `sync_indicator`. Titles default to their frame
color. An unknown preset name or
malformed color is rejected at startup with a message listing the accepted values.

The status bar paints its background across the full width. Set `status_fill = false` for the
earlier behavior, where the background stopped where the text did.

`[appearance]` is deprecated in favor of `[theme]`. It still works, still takes palette indexes
only, and still overrides a `preset`, so existing configs render exactly as before.

## Vivid behavior

Every pane receives a distinct zeroizing 256-bit `VIVID_ROOT_SECRET` and
`VIVID_ENDPOINT_CONTROL`. Realtime and bulk Track discovery fall back to that endpoint. The shared
listener accepts Vivid 1.5 Control and Track connections, rejects Lane connections, authenticates
each root proof with exactly one pane secret, and derives independent session, channel, and
marker-v3 keys. No Vivid 1.1 parser, alias, or downgrade path is retained.

Static encoded images and the latest raster are retained within the configured aggregate budget.
Stable surfaces own immutable tracks and authenticated channel generations. Media is admitted only
after `CHANNEL_ACCEPTED`; flow uses cumulative `MAX_CHANNEL_DATA`, with one priming record initially
and a higher maximum only after the corresponding outer delivery becomes reusable. Bridge
replacement resends images and complete raster content, requests a fresh video key unit, and
resumes audio from new packets. Ordered `CHANNEL_EOS` never becomes an implicit pause.

The foreground client reconciles complete inner `(session, context, surface, track)` identities
into fresh outer Vivid identities. The hops never share requests, revisions, generations, epochs,
media IDs, sequences, flow maxima, credentials, or recovery authority. Stable outer surfaces and
nodes survive affected-track replacement. Virtual marker-v3 anchors are consumed inside vvmux;
their marker and authenticator are never forwarded.

Bridge queues are bounded per track and scheduled round-robin, so a blocked video track cannot
consume audio or another track's queue. Linked audio/video is pre-rolled, activated atomically, and
uses active audio as clock when present. Exact play timing and policy are preserved. Outer EOS uses
its own sequence and epoch, and accepted/buffered-end progress is returned to inner waits.

vvmux validates and forwards the Vivid portable profiles without transcoding, including canonical
Opus, Vorbis, and FLAC initialization. Outer `NEED_KEYFRAME`, `NEED_FULL_FRAME`, and track loss are
routed to the matching inner track without replacing unrelated surfaces, nodes, or sibling tracks.

The outer bridge reads `VIVID_ENDPOINT_CONTROL`, optional `VIVID_ENDPOINT_REALTIME` and
`VIVID_ENDPOINT_BULK`, and `VIVID_ROOT_SECRET` from the foreground client environment. Panes receive
only their inner endpoint and root secret; outer credentials never enter the session server or PTY.

If the outer terminal has no Vivid capability, or its presenter rejects `node-clip-rect-v1`,
terminal use continues without media and the client emits a single status/title warning.

## Current scope

Implemented: native Unix sockets and owner-restricted Windows named pipes; Unix PTYs and
ConPTY/Job Object panes; Unix and Windows console clients; named detachable sessions, tabs, tiled
and tab-scoped floating/pinned shell panes, zoom, scrollback/copy/paste, ordered overlap
composition, mouse focus/move/resize/forwarding, status line, truecolor theming, strict TOML with
live reload, command panes with hold-on-exit, bounded TOML startup layouts, VVMX IPC, exact
fragment-aware pane media occlusion, static rehydration, timed-media headless semantics,
full-duplex outer control, linked A/V projection, bounded scrollback search, tab-local synchronized
input, and optional bulk-media endpoint discovery.

Intentionally absent: a plugin marketplace, a bundled web UI, direct non-loopback/TLS serving,
arbitrary action sockets, stacked panes, pane-class conversion, multi-pane selection, scrollback
editing, source transcoding, mirrored multi-client sessions, WinPTY, MSI/service installs,
and machine-wide PATH changes. `run` and a layout `command` take one shell command line, passed to
the shell with `-c`; plugin panes and runtimes use exact argument vectors without a shell.

Plugin packages use a strict `vvmux-plugin.toml` and JSON Schema Draft 2020-12 action contracts.
Discover agent-visible actions with `vvmux plugin catalog --json`; `vvmux --skill` prints the
release-matched automation guidance. WebAssembly Components are the sandboxed tier. Native process,
one-shot, and PTY-pane plugins are trusted user code and run with the user's full OS authority.
Native service SDKs expose scoped host calls for session inspection, bounded pane text, and pane
input. These calls enforce the manifest's `session.read`, `pane.read`, and `pane.input` declarations;
their short-lived broker tokens provide attribution and revocation, not an OS security boundary.
`vvmux plugin invoke ID/ACTION --target SESSION --detach` returns a session-bound job ID. Use
`vvmux plugin job status JOB`, `cancel JOB`, and `logs JOB` to inspect or stop it. Sessions retain
the newest 200 detached completions and at most 256 KiB each of result/error log text; detached work
survives the invoking CLI client, while synchronous work remains client-scoped.

## Windows troubleshooting

| Host terminal | Supported profile |
|---|---|
| Windows Terminal (current stable) | VT input/output, Unicode, alternate screen, mouse, focus, OSC 52, title, paste, restoration |
| Visual Studio integrated terminal (current stable) | Same profile; manual release certification required |
| Windows 10/11 conhost | Same profile; pixel cell metrics may be zero |

- “ConPTY is required” means the OS is older than build 17763 or the ConPTY API is unavailable.
- A named-pipe admission failure usually means the client and server run as different Windows
  users or one process is elevated under a different token. VVMX is deliberately same-user only.
- Attachment requires real stdin/stdout console handles; `new -d`, `list`, and `kill-session`
  remain usable with redirected handles.
- The RAII client restores console modes, code pages, title, cursor, alternate screen, mouse,
  focus, and bracketed-paste state on normal errors and unwinds.
