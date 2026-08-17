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

On Windows, the configured `[general].shell` is used first, then a native executable advertised by
`%SHELL%`, then `%COMSPEC%`, then the system `cmd.exe`. Vivido sets `%SHELL%` to the program selected
with `-e`, so a vvmux session opened from `vivido -e pwsh.exe` creates PowerShell panes. The default
config is `%APPDATA%\vvmux\config.toml`; owner-only runtime registries live
below `%LOCALAPPDATA%\vvmux\runtime`. Pane shells receive an exact `127.0.0.1` virtual Vivid endpoint
and `VIVID_ANCHOR_TRANSPORT=conpty`. Remote Unix applications may still need the supplied
`terminfo/vvmux.info` installed on the remote host.

## Commands

```text
vvmux                              attach/create `default`
vvmux new [-s NAME] [-d]           create a session
vvmux attach [-t NAME] [--replace] attach exactly by name
vvmux list [--json]                list live owner sessions
vvmux doctor -t NAME --json        check registry, IPC, bridge, and queue health
vvmux debug-bundle -t NAME ...     write an atomic diagnostic ZIP
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
session-inspect
list-tabs
select-tab --tab-id ID [--wait outer|rendered] [--timeout DURATION]
diagnose [--pane-id ID|--all-panes] [--trace-limit N]
report-agent --agent claude|codex|opencode|hermes --state idle|working|blocked --source ID --sequence N [--message TEXT] [--agent-session-id ID] [--agent-session-path PATH] [--pane-id ID]
report-metadata --source ID --sequence N [--token NAME=VALUE]... [--ttl-ms MS] [--display-agent TEXT] [--state-label STATUS=TEXT]... [--title TEXT] [--pane-id ID]
clear-agent-report --source ID --sequence N [--pane-id ID]
inspect [--pane-id ID]
inspect-media [--pane-id ID]
split vertical|horizontal [--pane-id ID]
run COMMAND [--placement split|float|tab] [--axis vertical|horizontal] [--cwd DIR] [--hold] [--no-focus] [--pane-id ID]
focus [--pane-id ID] [--wait outer|rendered] [--timeout DURATION]
close-pane --pane-id ID
typing TEXT [--pane-id ID] [--report]
key KEY [--mods Shift,Alt,Ctrl,Super] [--repeat N] [--pane-id ID] [--report]
paste TEXT [--pane-id ID] [--report]
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
wait media-track CONDITION --producer-id ID --context-id ID --surface-id ID --track-id ID [--timeout DURATION] [--pane-id ID]
```

`typing`, `key`, and `paste` bypass prefix and copy-mode handling and acknowledge only after the
pane PTY writer flushes all generated bytes. `paste` honors bracketed-paste mode and prevents an
embedded bracketed-paste terminator. Keys include Unicode scalars, navigation keys, F1 through
F35, and keypad keys; cursor and keypad application modes are honored.
With `--report`, input prints the pane, encoded byte count, request sequence, and PTY-write
completion, without claiming the child consumed it. `outer` waits for the foreground Vivid bridge
to apply the resulting projection; `rendered` waits for the attached client's terminal-frame ACK.
Vivido's separate `wait frame` is the GPU-presentation assertion.

For automation discovery and support collection use `vvmux list --json`, `vvmux doctor --target
NAME --json`, and `vvmux debug-bundle --target NAME --output FILE`. Bundles are atomic and
metadata-only unless pane grid/text or bounded logs are explicitly requested.

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
rather than accepted from reporters. The agent ID must come from an enabled provider. One pane
retains sequence state for at most 32 sources; a full table refuses new reporters rather than
growing, and sources already in it keep working.
`list-panes` and `inspect` expose the effective agent state, display label, and provider plugin.

`--message` carries why the agent is blocked (up to 256 bytes) and appears beside it in the agent
navigator. `--agent-session-id` and `--agent-session-path` record the agent's own session so it can
be resumed later; report them once, since a later state-only report from the same source keeps the
stored reference. That reference names a resumable conversation on your agent account, so it is
returned only by `inspect` for a single pane. `list-panes`, the plugin host's `session.inspect`,
`diagnose`, and the debug bundles built from them report `session_present` alone, and it is never
written to a log.

`report-metadata` attaches display-only annotations without claiming lifecycle authority: use it
for progress and custom status text, and `report-agent` only for real state. Tokens render beside
the agent in the navigator as `$name value`, `--display-agent` and `--state-label` rename the agent
and one status, and `--title` replaces the pane title shown there. Each option distinguishes "not
given, leave alone" from "given empty, clear", so one call can update a single token without
restating the rest. `--ttl-ms` expires only the tokens in that call; untimed tokens persist.

```sh
vvmux msg report-metadata --pane-id 2 --source my-tool --sequence 7 \
  --token 'files=42 indexed' --state-label 'working=indexing' --ttl-ms 5000
```

Metadata shares the report sequence table with `report-agent`, so one integration's state and
annotations cannot be applied out of order against each other, and it is dropped whenever report
authority is dropped. A pane holds at most 16 tokens (names 32 bytes, values 128) and 4 state
labels; an over-limit patch is refused whole rather than applied in part. Because these are
display-only, they deliberately do not appear in the agent state that `wait` and event subscribers
observe — a progress counter must not read as a stream of lifecycle transitions.

Agent detection is plugin-provided. vvmux bundles four immutable data-only providers: Claude Code
and Codex are enabled by default, while OpenCode and Hermes are disabled by default. They use the
ordinary plugin controls and appear in `vvmux plugin list`:

```sh
vvmux plugin disable dev.vivido.agent.claude
vvmux plugin enable dev.vivido.agent.opencode
vvmux plugin inspect dev.vivido.agent.codex
```

Third-party plugins use `manifest_version = 2` and may declare one or more passive agents without
an executable runtime or permissions. Executable names are normalized and matched exactly;
`argv_contains` covers packaged script paths. Exact executable matches win over argv markers, and
an equal-strength ambiguity is left undetected rather than guessed.

```toml
manifest_version = 2

[plugin]
id = "com.example.openclaw"
name = "OpenClaw Agent Support"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Passive OpenClaw state detection"
platforms = ["linux", "macos", "windows"]
permissions = []

[[agents]]
id = "openclaw"
name = "OpenClaw"
process = { executables = ["openclaw", "openclaw-cli"], argv_contains = ["@openclaw/cli"] }

[[agents.rules]]
id = "approval"
state = "blocked"
priority = 900
region = "bottom_non_empty_lines(12)"
contains = ["approval required"]

[[agents.rules]]
id = "working"
state = "working"
priority = 500
region = "whole_recent"
contains = ["esc to interrupt"]
```

Rules run by descending priority and support `idle`, `working`, `blocked`, and state-preserving
`unknown`, the screen/OSC regions used by the bundled providers, and nested `contains`, `regex`,
`line_regex`, `all`, `any`, and `not` gates. Agent IDs are globally unique among enabled plugins;
disable the current provider before enabling a replacement with the same ID. Manifests are bounded
to 16 agents and 64 rules per agent, and the live catalog is bounded to 64 agents.

OpenCode can report lifecycle events directly through the managed optional plugin:

```sh
vvmux integration install opencode
vvmux integration status opencode
vvmux integration uninstall opencode
```

The installer owns only `~/.config/opencode/plugins/vvmux-agent-state.js`, refuses to replace or
remove a file without the vvmux ownership marker, and enables the bundled OpenCode provider.
Uninstalling the adapter leaves the provider's chosen enable state unchanged. Claude Code and
Codex need no installation; Hermes can be enabled for passive detection with the plugin command
above.

## Startup layouts

A session created without `--layout` starts from `~/.config/vvmux/startup.toml` when that file
exists, so `vvmux` alone comes up in the layout you saved. The full order is `--layout NAME`, then
`startup.toml`, then `[general].default_layout`, then the usual one-shell tab.

`vvmux new --layout NAME` loads `~/.config/vvmux/layouts/NAME.toml`; an existing path can be
passed instead. `[general].default_layout` supplies the name or path when `--layout` is omitted.
An explicitly requested missing or invalid layout fails before the daemon forks. The two implicit
sources never do: a missing default layout, or a `startup.toml` that fails to parse, prints a
warning and falls through to the next candidate.

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

`Ctrl-b s` writes the live layout back out in this format. The status row prompts for a target,
prefilled with `startup.toml`; Enter accepts it, Escape cancels, and a bare name becomes
`~/.config/vvmux/<name>.toml` while a path is used as written. Replacing an existing file asks
first. `vvmux msg save-layout [--path X]` does the same without a terminal and always replaces.

A save records tab names, split axes, exact split weights, floating size and pin state, the focused
pane, and each pane's `cwd`. That `cwd` is where the pane's process was started, not wherever its
shell has since `cd`-ed. Commands are deliberately not recorded, so a saved layout reopens plain
shells rather than re-running whatever was in the pane. Plugin panes are skipped, and zoom and
synchronized input are session state rather than layout.

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
waiters, response work, screen-delta history, and recent process-exit tombstones. VVMX 18 is a hard
private-protocol cutover; it reports recreated retained tracks so tab restoration can rehydrate
their image/raster bodies. Raster deltas are composed into the retained latest framebuffer before
that replay, preserving interactive drawings across tab switches. Sessions created by older
binaries must be restarted after upgrading.

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
| `Ctrl-b h` / `j` / `k` / `l` | Focus left/down/up/right |
| `Ctrl-b Ctrl-Arrow` | Resize by one cell |
| `Ctrl-b c`, `n`, `p` | Create/cycle tabs |
| `Ctrl-b 1`–`9` | Select tabs 1–9 |
| `Ctrl-b w` | Open or close the tab navigator |
| `Ctrl-b ,` | Rename the active tab |
| `Ctrl-b x`, then `y` / `n` | Confirm or cancel closing the focused pane |
| `Ctrl-b z` | Toggle zoom |
| `Ctrl-b s` | Save the current layout, prefilled with `startup.toml` |
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

The tab navigator is the corresponding tab-scoped popup. Arrows or `j`/`k` select, Enter jumps to
the tab, and `q` or Escape closes it. Mouse wheel and row clicks are supported. Tab rename uses the
status row, starts with the current name, commits with Enter, cancels with Escape, and clears the
custom name when submitted empty.

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
follow tab switches. Every frame title shows `sync` while it is enabled.

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

### Hyperlinks

Programs can mark text as a link with OSC 8 (`printf '\e]8;;https://example.com\e\\text\e]8;;\e\\'`).
vvmux underlines every link so they are visible without hunting, and underlines the one under the
pointer more strongly while showing its target in the status row. Matching is by link identity, not
position, so a link that wraps across rows highlights as one link. Hover yields to the pane's own
program: a full-screen application that requested motion reports keeps its mouse events.

`[hyperlinks]` controls this. `enabled` turns the whole feature off, `persistent_style` drops the
resting underline so only the hovered link is marked, and `open` decides who activates a click:

- `"delegate"` (default) — vvmux opens nothing. The outer terminal activates the link, so it opens
  on the machine you are sitting at. In Vivido that gesture is Shift+click. This is the default
  because the vvmux server is frequently the far end of an ssh session.
- `"local"` — a plain click opens the link on whichever host runs the vvmux *server*. Only `http`,
  `https`, `mailto`, and `irc` links are handed to the system opener; anything else is refused,
  because an OSC 8 target is written by whatever program holds the pane and nothing else vets it.

The link's underline color is `[theme].hyperlink`. Only the underline is colored — the text color
belongs to the program that printed it.

### Clipboard access from panes

Programs can read and write the clipboard with OSC 52 (`printf '\e]52;c;<base64>\e\\'` to store,
`printf '\e]52;c;?\e\\'` to query). A store lands in the same copy buffer as a mouse selection and is
mirrored to the outer terminal; a query is answered on the requesting pane's own PTY. Both directions
are honored only for the focused pane of an attached session, so a background pane cannot overwrite
or read the buffer behind your back, and the later of a pane store and a mouse selection wins.

`[clipboard].osc52` decides which directions are honored:

- `"only_copy"` (default) — a pane may write the clipboard but not read it. This is the default
  because the vvmux server is frequently the far end of an ssh session, where answering a read would
  let any pane copy your clipboard off the machine you are sitting at.
- `"disabled"` — OSC 52 is ignored in both directions.
- `"only_paste"` — a pane may read the clipboard but not write it.
- `"copy_paste"` — both directions. Enable only where every pane is as trusted as you are.

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
| `plugins.enabled` | Applied immediately; disabling stops plugin acceptance, runtimes, and registry watching |
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
`search_current_background`. The legacy `sync_indicator` key remains accepted, although the
tabs-only status row no longer renders a separate sync segment. Titles default to their frame
color. An unknown preset name or
malformed color is rejected at startup with a message listing the accepted values.

The status bar paints its background across the full width. Set `status_fill = false` for the
earlier behavior, where the background stopped where the text did.

The ordinary status text is the complete numbered tab list: unnamed tabs appear as `1`, named tabs
as `1:name`, and the active tab as `[1:name]`. On a narrow display the list windows around the active
tab and uses `<` / `>` overflow markers. Search, rename, and close-confirmation prompts temporarily
replace that list.

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

### Kitty graphics compatibility

Vvmux has one deliberately narrow exception to its normal rule that media bytes do not cross a
pane PTY. A pane may emit bounded Kitty graphics APC packets using direct transmission (`t=d`),
quiet mode (`q=2`), and Unicode virtual placements (`U=1`). Vvmux validates and assembles the
complete transfer, then writes it atomically before the full terminal repaint containing the
`U+10EEEE` placeholder cells. File and shared-memory transports, cursor-positioned placements,
animation commands, malformed packets, and more than 64 MiB of live transfer data are rejected.

The native client advertises this exception only when the attaching terminal's exact `TERM` is
`xterm-kitty` or `xterm-ghostty`; `TERM_PROGRAM` is never used as evidence. Hosted attachments,
including Vivido, advertise no Kitty capability, and placeholder glyphs are suppressed there.
Graphics bytes belong only to the current physical attachment and are discarded on detach rather
than retained or replayed. Unix pane PTYs receive cell and pixel dimensions so applications can
choose the correct image size. This behavior is private to vvmux and does not alter Vivid media or
the public Vivid protocol.

## Current scope

Implemented: native Unix sockets and owner-restricted Windows named pipes; Unix PTYs and
ConPTY/Job Object panes; Unix and Windows console clients; named detachable sessions, tabs, tiled
and tab-scoped floating/pinned shell panes, zoom, scrollback/copy/paste, ordered overlap
composition, mouse focus/move/resize/forwarding, status line, truecolor theming, strict TOML with
live reload, command panes with hold-on-exit, bounded TOML startup layouts, VVMX IPC, exact
fragment-aware pane media occlusion, static rehydration, timed-media headless semantics,
full-duplex outer control, linked A/V projection, bounded scrollback search, tab-local synchronized
input, optional bulk-media endpoint discovery, and validated Kitty/Ghostty graphics passthrough for
Unicode virtual placements.

Intentionally absent: a plugin marketplace, a bundled web UI, direct non-loopback/TLS serving,
arbitrary action sockets, stacked panes, pane-class conversion, multi-pane selection, scrollback
editing, source transcoding, mirrored multi-client sessions, WinPTY, MSI/service installs,
and machine-wide PATH changes. `run` and a layout `command` take one shell command line, passed to
the shell with `-c`; plugin panes and runtimes use exact argument vectors without a shell.

Plugin packages use a strict `vvmux-plugin.toml` and JSON Schema Draft 2020-12 action contracts.
Manifest version 1 remains supported; version 2 adds declarative agent providers.
Discover agent-visible actions with `vvmux plugin catalog --target SESSION --json`; `vvmux --skill`
prints the release-matched automation guidance. WebAssembly Components are the sandboxed tier.
Native process, one-shot, and PTY-pane plugins are trusted user code and run with the user's full OS
authority. Native service SDKs expose scoped host calls for session inspection, bounded pane text,
pane input, and capability-checked pane close. These calls enforce the manifest's `session.read`,
`pane.read`, `pane.input`, `pane.manage_own`, and `pane.manage_any` declarations; service and
Component broker tokens provide attribution and revocation, not an OS security boundary. One-shot
actions receive no broker token.

Rust Component authors implement `vvmux_plugin_sdk::component::Guest` and export it with the SDK's
generated `component::export!` macro. Build those crates for `wasm32-wasip2`; the SDK owns the
release-matched WIT bindings and JSON/host-call/storage/log helpers. The full vvmux test gate builds
the real Rust conformance guest, so development environments need
`rustup target add wasm32-wasip2` once.

`vvmux plugin invoke ID/ACTION --target SESSION --detach` returns a session-bound job ID. Use
`vvmux plugin job status JOB`, `cancel JOB`, and `logs JOB` to inspect or stop it. Sessions retain
the newest 200 detached completions and at most 256 KiB each of result/error log text; detached work
survives the invoking CLI client, while synchronous work remains client-scoped.

`vvmux plugin pane open ID/PANE --target SESSION` resolves the pane from that session's applied
registry generation and starts its exact argv in a real PTY. Manifest placement selects a vertical
split, float, or named tab. Plugin panes default to held exit and exclusion from synchronized
keyboard and paste fan-out; `accept_sync_input = true` opts in. `pane.create` authorizes opening,
while `media.produce` controls whether the process receives a fresh pane-scoped Vivid capability.
Held crashes show a core-authored plugin/entrypoint/exit diagnostic. Close, plugin disable/removal,
artifact change, global kill switch, and session shutdown all use the same owner-scoped pane and
Vivid teardown path.

The global plugin registry carries a monotonic generation and is watched by every plugin-enabled
live session.
Install, update, enable, disable, and uninstall publish an atomic generation, wait for every live
session to accept it, and publish a newer rollback generation if any session rejects it. Changed
artifacts use immutable content-addressed package directories: existing calls drain on their pinned
artifact, disabled or removed plugins cancel their jobs, and old runtimes fully stop before package
cleanup or update acknowledgement. `vvmux msg --target SESSION reload-plugins` forces validation
and reports the generation plus applied, deferred, and failed plugin IDs.

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
