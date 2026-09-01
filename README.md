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
vvmux attach [-d|--replace] [-t NAME] [--pane-id ID|--alias NAME]
vvmux list [--json]                list live owner sessions
vvmux doctor -t NAME --json        check registry, IPC, bridge, and queue health
vvmux debug-bundle -t NAME ...     write an atomic diagnostic ZIP
vvmux kill-session -t NAME         terminate a session and its process groups
vvmux msg [-t NAME] COMMAND        automate or inspect one pane directly
vvmux plugin COMMAND               install, inspect, and invoke typed plugins
vvmux api schema --json            emit the automation JSON Schema
vvmux channel set stable|preview   select the Unix update stream
vvmux update [--check]             verify/install a signed Unix release
vvmux token create [--rotate]      create/rotate the VVWS bearer token
vvmux token create-scoped --scope automation-read|automation-input
                                   create an automation-only token that cannot attach a terminal
vvmux replay FILE [--pane-id ID]   reconstruct state from a recording, running nothing
vvmux serve [OPTIONS]              run the loopback VVWS/1 session gateway
vvmux --config PATH ...            use an explicit strict TOML config
```

Only one client can be attached to a session. `-d` (or `--replace`) sends a clean detach to the old
client before the new client is admitted. With no `-t`, `vvmux attach -d` replaces the client on
the `default` session. `--pane-id` and `--alias` attach that pane directly over the whole terminal:
input and resize affect only it, the session chrome and ordinary prefix actions are absent, and
`Ctrl+b q` returns to the shell.

## Pane automation

`vvmux msg` connects directly to the owner-only hidden session server. It does not type vvmux
prefix keys and does not replace the foreground client, so an attached Vivido window keeps
rendering normally while another shell controls or observes individual panes.

The session target is resolved from `--target`, then `VVMUX_SESSION`, then `default`. A pane target
is resolved from `--pane-id`, then `--alias`, then `--pane-name`, then a same-session
`VVMUX_PANE_ID`, then the focused pane in the active tab. `close-pane` deliberately has no focused
fallback. Pane shells inherit the exact `VVMUX_SESSION`, `VVMUX_TAB_ID`, and `VVMUX_PANE_ID` values
for their owner.

**Pane IDs are stable only within one run of a server.** Restoring a session from its snapshot
rebuilds it and reassigns them, so anything saved or re-run should name its panes instead:

```sh
vvmux msg pane-rename --pane-id 2 --name editor
vvmux msg typing --pane-name editor 'cargo test'     # still correct after a restart
```

A pane name is unique per session (`pane_name_taken`), is written into the snapshot, and comes back
attached to the same pane. It is not an agent alias: an alias names the agent *process* and is
cleared when that process exits, while a pane name outlives whatever is running in the pane, which
is what makes it usable for a plain shell.

`layout` is the discovery call. It returns every tab and pane with a one-based `split_path` from the
tab root, cell rectangles, visibility, zoom, a `locator`, and directional `neighbors` — plus a
`caller` block locating the pane the request came from. `resolve-pane` walks that graph without
touching focus:

```sh
vvmux msg resolve-pane --path left
vvmux msg resolve-pane --pane-id 1 --path right,down
vvmux msg resolve-pane --tab-name Logs --path down
```

`neighbors` and `resolve-pane` are the *navigation* graph, computed by the same rule `action focus`
uses, so resolving a pane and focusing it can never disagree. That also means a direction is one
navigation step rather than a global edge selector: from a full-height left pane, `up` lands on the
upper of the two panes to its right, because that is where focus would go. Read `geometry` when the
question is really about position on screen, and inspect `split_path` and the rectangles before
translating a phrase like "the bottom pane" into a target.

Every `msg` command also takes `--expect-screen`, `--expect-session`, and `--expect-layout`, which
refuse the request unless the session is still where the caller last read it. That closes the race
every `inspect`-then-act pair has: the screen you reasoned about can change before you act on it,
and typing into a dialog that already closed is worse than being told the state moved. A refused
request never reaches the PTY.

`--idempotency-key` applies a request at most once however many times it is sent, and hands a retry
the original reply. A caller that retries after a lost answer cannot otherwise tell "never arrived"
from "the reply did not come back", and pressing Enter twice is not pressing it once. Mutating
methods only — replaying a cached read would be a lie about the present — and a failed request
releases its key so a corrected retry can reuse it.

## Sharing a session between agents

A vvmux session routinely has several agents working in different panes, and nothing stopped two of
them typing into the same one. A lease says a pane is yours for a while:

```sh
lease=$(vvmux msg lease acquire --scope input --pane-id 2 --holder reviewer | jq -r .lease_id)
vvmux msg --lease "$lease" typing --pane-id 2 'cargo test'
vvmux msg lease release "$lease"
```

Leases are **advisory in one direction**: a caller holding no lease is never blocked unless somebody
else holds an exclusive one on that pane and scope, so nothing that worked before starts failing and
an interactive user is never locked out of their own terminal. `observe` is shared — watching a pane
changes nothing — while `input`, `layout`, and `process` admit one holder each. Every lease expires,
because a TTL is the only release that does not need the holder to still be alive.

## Recording a session

```sh
vvmux msg record start /tmp/session.ndjson
vvmux msg record stop
vvmux replay /tmp/session.ndjson
```

A recording holds pane output, layout changes, exits, and agent transitions. It records **input
classes, never input content**: that a pane was written to and how much, not which bytes. Knowing 14
bytes were typed reproduces the shape of a session; knowing which 14 bytes is a credential leak with
an excuse. Starting one is always explicit for the same reason `[session] pane_history` is opt-in —
it writes what scrolled past to a file.

Both the buffer and the file are bounded and report what they dropped, so a partial recording says
so before it is read. `replay` reconstructs terminal and layout state and **runs nothing**: a
recording is evidence about a session that already happened, and re-executing its commands would be
a different session with different, possibly destructive, side effects.

## Plans

`run-plan` runs a bounded list of steps over **one** connection, with results flowing between them:

```sh
vvmux msg run-plan --file work.json          # execute
vvmux msg run-plan --file work.json --preflight   # observations only, mutations skipped
vvmux msg run-plan --file work.json --dry-run     # report what would run, connect for nothing else
```

```json
{
  "version": 1,
  "steps": [
    {"id": "split", "method": "split", "params": {"axis": "Vertical"}, "pane_id": 1,
     "bind": {"right": "/new_pane_id"}},
    {"id": "name", "method": "pane_rename", "pane_id": {"$ref": "right"},
     "params": {"name": "worker"}},
    {"id": "run", "method": "submit_line", "pane_name": "worker",
     "params": {"text": "cargo test", "report": true},
     "verify": {"screen_changed": true, "capture": true, "timeout_ms": 5000}}
  ]
}
```

A step names a wire method (as `capabilities` advertises it) and its params. `bind` reads a JSON
Pointer out of the step's result under an alias; `{"$ref": "alias"}` substitutes it anywhere in a
later step. References are **backward-only** and the whole plan is validated before any of it runs,
so a typo on the last step does not first perform the mutations in the steps before it. `when`
runs a step only when a bound alias equals a value, and `on_error: "continue"` keeps going past a
failure instead of aborting. Steps also take `expect` and `idempotency_key`, exactly as the global
flags express them.

`verify` belongs to the step rather than following it, because the "before" sequence has to be read
before the action. vvmux has no GPU frame, so a step verifies against the pane's screen sequence
(`screen_changed`) or the attached client's render acknowledgement (`rendered`) — neither of which
is GPU presentation. Pair `rendered` with Vivido's own `wait frame` when that is what matters.

Output is NDJSON: one `plan_started`, one line per step, one `plan_completed`. Exit is nonzero if
any step failed. There are no loops, no conditionals beyond one equality test, and no arithmetic —
which is what makes a plan safe to accept and possible to preflight without running it.

`capture` is the read composite: activate the pane, wait for it to settle, and read it in one
request rather than three the caller has to sequence. `--no-activate` reads it where it is.

`shell-command` runs one command in the pane's **existing** interactive shell and returns its real
exit status, so aliases, functions, virtual environments and the current directory all still apply.
It needs the shell to emit OSC 133 command markers; without them the boundary between a command and
its output can only be guessed from prompt text, so a pane whose shell reports nothing is refused
rather than guessed at. Most shell-integration setups already emit them.

`mouse` takes **pane-local** coordinates, so a caller never has to know where a pane sits.
`--route application` encodes through that pane's own live terminal modes and writes to its PTY,
which reaches a pane whether or not it is visible, in another tab, or under a zoom — an explicitly
targeted event is not hit-tested. `--route mux` gives the event to vvmux instead, which is what
handles copy-mode selection, float drag and resize, and pane focus. Cells are the primary
coordinate; `--x/--y` pixels need an attached client's cell metrics and are refused without one, and
pass through exactly under SGR pixel mouse mode. `mouse path` sends one bounded press/move/release
gesture over 2–1000 points, so a gesture cannot leave a button held down because a separate release
call failed.

`signal` reaches the pane's **foreground process group**, which typing `Ctrl+C` cannot promise: a
shell running `cargo test` hands that job the terminal, and a signal aimed at the child would reach
the shell instead. The reply says which group it went to, so `foreground_job` distinguishes
interrupting the job from interrupting a shell sitting at its prompt. Windows has no equivalent and
refuses rather than approximating one.

`transcript` and `wait output` observe the **output stream**, not the screen. Output that a pane
overwrites — a progress line rewritten by carriage returns, anything that scrolls past between two
polls — is gone from the grid before `get-text` can see it. Each pane keeps a bounded in-memory
rolling window with a monotonic byte offset; a request for output that has already scrolled out of
it reports `dropped_before_offset` rather than silently returning less. This is separate from the
opt-in on-disk `[session] pane_history` and writes nothing anywhere.

`resize-pane` sets an exact size, unlike `action resize <direction>`, which nudges one step. A
tiled pane is sized by reweighting the split that decides its span — the deepest ancestor on that
axis — so a neighbour absorbs the difference and the rest of the layout stays put.

`set-flag` replaces the toggles for automation. A toggle cannot be replayed: running it twice puts
the flag back, which makes every retry loop wrong. Each setter states the state it wants and reports
`changed`, so a repeat is a no-op rather than a reversal. The toggles remain as keybindings.

`move-pane` relocates a pane to another tab, swaps it with a neighbour, or moves it between the
tiled tree and the floating layer. The pane keeps its ID, its name, its agent, and its Vivid media
ownership; only where it sits changes.

`activate-pane` reveals a pane — selecting its tab and lifting a zoom that hides it — **without**
moving focus. Visibility is what drives media projection, so "let me see this" and "type here now"
are separate requests.

A pane does **not** inherit an outer `VIVIDO_SOCKET`, `VIVIDO_WINDOW_ID`, or `VIVIDO_SESSION`. The
session server outlives the Vivido window that started it, so those name a window that may be gone,
may now be a different one after a reattach, or — over `vvssh` — may live on another machine
entirely. They are stripped along with the whole `VIVID_*` namespace and an outer `TMUX`/`STY`
identity, so a pane agent cannot silently drive somebody else's terminal.

The live answer comes from `session-inspect` instead, under `outer`: which Vivido window is
presenting the session right now, its cell metrics, and `vivido_automation_reachable`. It is `null`
when nothing is attached, and `remote` is true when the client reached vvmux over `vvssh` — in which
case the Vivido automation socket is on another machine and `vivido msg` is not a route that exists.
`inspect` adds `outer_crop`, the pane's rectangle in that window's physical pixels, which is the
crop to apply to a `vivido msg screenshot`. **None of it carries the outer Vivid endpoint or root
secret**: those stay in the foreground client, which is what lets the daemon answer "which window"
without ever holding "how to reach it".

`capabilities` is authoritative for a release and describes the surface rather than just naming it:

```sh
# Every method, with what it does and whether it changes anything.
vvmux msg capabilities | jq '.method_capabilities[] | select(.mutating | not) | .name'
```

Each entry carries a `name`, a `class` (`observe`, `input`, `pane`, `layout`, `config`, `agent`,
`plugin`), and `mutating`, which is `false` only for `observe`. A read-only pass runs the
non-mutating set and skips the rest. `capabilities` also lists every `error_codes` value a reply can
carry and every `event_kinds` name `subscribe --name` accepts, so a caller can tell a failure it
handles from one this release added, and a real event name from a typo.

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
run-plan [--file PATH] [--dry-run|--preflight]
get-config
reload-config
reload-plugins
action ACTION [--pane-id ID]
list-panes
session-inspect
list-tabs
layout
resolve-pane [--path left,down] [--pane-id ID] [--tab-id ID|--tab-name NAME]
pane-rename [--pane-id ID] (--name NAME | --clear)
activate-pane [--pane-id ID]
new-tab [--name NAME]
rename-tab --name NAME [--tab-id ID|--tab-name NAME]
reset-tab-title [--tab-id ID|--tab-name NAME]
close-tab [--tab-id ID|--tab-name NAME]
select-tab [--tab-id ID|--tab-name NAME] [--wait outer|rendered] [--timeout DURATION]
diagnose [--pane-id ID|--all-panes] [--trace-limit N]
report-agent --agent AGENT --state idle|working|blocked --source ID --sequence N [--message TEXT] [--agent-session-id ID] [--agent-session-path PATH] [--pane-id ID]
report-agent-session --agent AGENT --source ID --sequence N [--agent-session-id ID] [--agent-session-path PATH] [--pane-id ID]
report-metadata --source ID --sequence N [--token NAME=VALUE]... [--ttl-ms MS] [--display-agent TEXT] [--state-label STATUS=TEXT]... [--title TEXT] [--pane-id ID]
clear-agent-report --source ID --sequence N [--pane-id ID]
snapshot
agent-explain [--pane-id ID]
agent-rename [--pane-id ID] --name NAME | --clear
agent-start --kind AGENT --pane-id ID [--timeout DURATION] [-- ARGS...]
agent-prompt [--pane-id ID] [--wait] [--until idle|working|blocked|done[,...]] [--timeout DURATION] TEXT
agent-send-keys [--pane-id ID] --key KEY [--key KEY]...
agent-read [--pane-id ID] [--lines N]
inspect [--pane-id ID]
inspect-media [--pane-id ID]
trace-media [--after SEQ] [--limit N] [--follow] [--producer-id ID --context-id ID --surface-id ID --track-id ID] [--category CATEGORY] [--recovery-only] [--timeout DURATION] [--pane-id ID]
save-layout [--path NAME|PATH]
split vertical|horizontal [--pane-id ID]
run COMMAND [--placement split|float|tab] [--axis vertical|horizontal] [--cwd DIR] [--hold] [--no-focus] [--pane-id ID]
focus [--pane-id ID] [--wait outer|rendered] [--timeout DURATION]
close-pane --pane-id ID
mouse move|click|double-click|down|up|drag|scroll|path (--cell-column N --cell-row N | --x PX --y PX | --relative-x F --relative-y F) [--point COL,ROW]... [--button left|middle|right] [--route application|mux] [--mods Shift,Alt,Ctrl] [--scroll N] [--pane-id ID]
signal INT|TERM|HUP|QUIT|TSTP|CONT|WINCH|KILL|STOP [--pane-id ID]
resize-pane [--columns N] [--rows N] [--pane-id ID]
move-pane (--to-tab ID|--to-tab-name NAME | --swap left|right|up|down | --to-layer tiled|floating) [--pane-id ID]
set-flag zoom|pinned|transparent|copy-mode|floats-visible|sync-input (--on|--off) [--offset N] [--pane-id ID]
transcript [--after-offset N] [--max-bytes N] [--base64] [--pane-id ID]
capture [--no-activate] [--after-screen SEQ] [--stable DURATION] [--rendered] [--grid] [--timeout DURATION] [--pane-id ID]
shell-command COMMAND [--timeout DURATION] [--pane-id ID]
lease acquire --scope observe|input|layout|process [--ttl DURATION] [--holder NAME] [--pane-id ID]
lease renew LEASE_ID [--ttl DURATION] | lease release LEASE_ID | lease list
record start PATH | record stop | record status
typing TEXT [--pane-id ID] [--report]
key KEY [--mods Shift,Alt,Ctrl,Super] [--repeat N] [--pane-id ID] [--report]
paste TEXT [--pane-id ID] [--report]
submit TEXT [--pane-id ID] [--report]
get-text [--rows N] [--source visible|recent|recent-unwrapped|detection] [--pane-id ID]
get-grid [--start-line LINE --row-count N | --since-screen SEQ] [--pane-id ID]
search --pattern TEXT [--regex] [--direction forward|backward] [--start-line LINE --start-column COLUMN] [--limit N] [--pane-id ID]
sync-input (--on|--off) [--pane-id ID]
wait text TEXT [--regex] [--after-screen SEQ] [--timeout DURATION] [--pane-id ID]
wait output PATTERN [--regex] [--after-offset N] [--timeout DURATION] [--pane-id ID]
wait screen-change [--after-screen SEQ] [--timeout DURATION] [--pane-id ID]
wait screen-stable [--quiet DURATION] [--after-screen SEQ] [--timeout DURATION] [--pane-id ID]
wait rendered --after-session SEQ [--timeout DURATION]
subscribe [--after SEQ] [--name EVENT]... [--pane-id ID]
wait agent-state --until idle|working|blocked|done[,...] [--timeout DURATION] [--pane-id ID]
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

`submit` sends one line to a pane that is already running something, putting the text and its
Enter into a single PTY write. `typing` followed by `key Enter` is two calls, and a failure
between them strands a half-typed command at the prompt — which is why retry loops should prefer
`submit`. It honors bracketed-paste mode like `paste`, and refuses embedded newlines so "submit
one line" is never ambiguous about how many commands ran. To open a *new* pane for a command, use
`run` instead. (This is herdr's `pane run`; `run` was already taken here.)

```sh
vvmux msg submit 'cargo test --workspace' --pane-id 2
```

`get-text --source` chooses which text comes back:

| Source | Returns |
|---|---|
| `visible` | the current viewport, honoring copy-mode scroll |
| `recent-unwrapped` | the last `--rows N` rows with soft wraps joined, so output reads as the lines a command wrote |
| `recent` | the same rows, one line per physical terminal row |
| `detection` | the exact bottom-buffer snapshot and OSC fields agent classification runs against |

Without `--source`, `--rows N` means `recent-unwrapped` and its absence means `visible` — the
long-standing behavior. `--rows` applies only to the two `recent` sources and is refused elsewhere
rather than ignored. `detection` prints JSON (`text`, `osc_title`, `osc_progress`, `rows`) because
the snapshot is only meaningful beside the OSC fields the classifier also reads; pair it with
`agent-explain` when a rule is not matching what you expect.

```sh
vvmux msg get-text --pane-id 2 --source recent-unwrapped --rows 200 | grep -i error
vvmux msg get-text --pane-id 2 --source detection | jq -r .text
```

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

Agent detection is plugin-provided, and vvmux ships no providers of its own: a fresh install
detects nothing until you install the package for the agent you use. The first-party packages are
one bare name each, resolved to `github.com/vivido-dev/<name>`:

```sh
vvmux plugin install claude          # or codex, opencode, hermes
vvmux plugin list
vvmux plugin disable dev.vivido.agent.claude
vvmux plugin inspect dev.vivido.agent.codex
```

`vvmux plugin install` takes four forms: a bare name (first-party), `owner/name` (anyone else's
GitHub repository), a full `https://` URL, and a local path — which must be absolute or start with
`./` or `../`, so a name is never ambiguous with a directory. A package's `[[dependencies]]` are
cloned and installed with it. Plugin IDs beginning `dev.vivido.` may only be installed from
`https://github.com/vivido-dev/`, or linked from a local working copy with `vvmux plugin link`.
The discovery index at [vivido.dev/vvmux/plugins](https://vivido.dev/vvmux/plugins) lists indexed
packages, while installation still fetches source directly and shows the local trust preview.

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

Manifest v2 may also register up to 16 unused prefix chords and 16 Ctrl-click OSC 8 URL routes.
The referenced action must be declared by the same package. User-configured chords win over core
chords, core chords win over plugin chords, and a chord claimed by two plugins is disabled.

```toml
[[keybindings]]
chord = "v"
action = "open-issue"

[[link_handlers]]
pattern = '^https://github\.com/[^/]+/[^/]+/issues/[0-9]+$'
action = "open-issue"

[[events]]
on = "session.started"
command = ["./restore-state"]
```

`session.started` is published once after snapshot/layout restoration. Like every executable event
hook, failure is isolated to the plugin worker and does not stop the session.

When a background agent blocks or finishes, the attached client raises a desktop notification, so
you do not have to watch every pane. `[notifications]` controls it:

```toml
[notifications]
enabled = true
on = ["blocked", "done"]
min_interval_ms = 2000
# sound_command = ["afplay", "/System/Library/Sounds/Glass.aiff"]
```

The session decides *whether* to notify; the foreground client decides *how*, because only it knows
your real terminal — the hidden server never learns anything about it. Ghostty, iTerm2, WezTerm,
and Vivido get OSC 9; kitty gets OSC 99; inside tmux the escape is wrapped for passthrough. A
terminal vvmux does not recognize gets **no** escape rather than a stray one printed into your
session, and a browser attach gets no notification at all. `done` is only ever derived when the
pane was not visible, so neither default fires for an agent you are already watching, and
`min_interval_ms` keeps a flapping agent from spamming the desktop. `sound_command` is run by the
client, detached, with no stdio; because the client owns it, a live reload reports
`notifications.sound_command` as deferred until you reattach.

`subscribe` streams session events as NDJSON, so automation can react instead of poll. It does not
require plugins to be enabled — these events describe the session, not the plugin system.

```sh
vvmux msg subscribe --name agent.status_changed | while read -r event; do
  echo "$event" | jq -r '.payload | "\(.pane_id) \(.previous_status) -> \(.status)"'
done
```

`--name` (repeatable, up to 16) and `--pane-id` narrow the stream; `--after SEQ` replays retained
events first. Each record is an `event` with a monotonic `sequence`, or a `gap` naming a range that
was dropped. **A gap is never filtered out**, so a narrowed stream still tells you when it missed
something; filtered streams also show jumps in `sequence`, which is the filter working rather than
loss. A subscriber that stops reading is disconnected rather than allowed to stall the session.

`agent.status_changed` carries `pane_id`, `status`, `previous_status`, `state`, and the agent's
`kind`/`label`/`provider`/`source`, plus a blocked `message`. A `null` status means the agent left
the pane. Display-only metadata is deliberately absent: it can change on every tool call, and a
subscriber reacting to lifecycle should not be woken by a progress counter.

`wait agent-state` blocks until a pane's agent reaches one of the states you name, so a script can
submit work and then wait for the agent to need it or finish, instead of polling:

```sh
vvmux msg submit 'refactor the parser' --pane-id 2
vvmux msg wait agent-state --pane-id 2 --until blocked,done --timeout 30m
```

It resolves immediately when the state already holds, and answers with the reached `status`, the
`initial_status` the pane had when the wait was registered, and the agent. `done` means the agent
finished while you were not looking; focusing its pane in the navigator acknowledges that back to
`idle`. A wait is scoped to its pane — another pane reaching the state does not satisfy it. A pane with no
agent yet is simply not matching, so "launch an agent, then wait for it" works; if the wait times
out without one ever appearing, the error says so.

### Session state

A session's shape is written to disk as it changes, and restored when its server starts again — no
`save-layout`, no `startup.toml`, no ritual:

```sh
vvmux msg snapshot     # where it lives, how big it is, whether this session came from one
```

What is recorded is what a layout file records — tab names, split axes and weights, floating size,
position and pin state, the focused pane, each pane's spawn `cwd` — plus what a layout file
deliberately cannot: which tab was active, which pane was zoomed, and synchronized input. Commands
are still not recorded, so a restored pane is a plain shell in its saved directory rather than a
re-run of whatever was in it. Plugin panes are skipped.

**Pane IDs are not preserved.** They are assigned in layout-tree order when a session is built, so a
restored pane can come back with a different ID than it had. Use an agent's name (below) as a durable
target; a pane ID is only stable within one run of a server.

Precedence when a session starts: an explicit `--layout` wins, then a snapshot, then `startup.toml`,
then `[general].default_layout`. A snapshot that is missing, unreadable, from a newer vvmux, or not a
usable layout is reported and skipped — a lost restore is a disappointment, but a multiplexer that
will not start is broken.

The snapshot lives in an owner-only file under `$XDG_STATE_HOME/vvmux` (`~/.local/state/vvmux` by
default), separate from the config directory because it holds working directories and, once agents
report it, resumable session identity. It is never added to a `debug-bundle`.

```toml
[session]
auto_snapshot = true    # default
pane_history = false    # default
resume_agents = true    # default
```

Turning either off also discards what is already on disk, so opting out leaves nothing behind. Both
are live-reloadable.

#### Pane history

`pane_history` additionally persists what was on each pane's screen and restores it as scrollback.
It is **off by default, and that is a privacy decision**: pane output is whatever scrolled past —
tokens, keys, command output — and enabling it writes that to a file. The file is owner-only and
never enters a `debug-bundle`, but it exists, and it holds what your terminal held.

What is stored is text and style only. Hyperlinks are dropped, because a restored OSC 8 target would
make a URL from a previous session clickable in this one. Graphics placements, media anchors, cursor
position, and terminal modes are dropped too. Restored lines go into scrollback, never onto the
screen: the viewport belongs to the shell that just started.

Persisted history is **never fed back through the terminal parser**. Cells are placed directly, so a
line containing a clipboard-write sequence, a device query, or a media anchor comes back as inert
text rather than acting a second time.

Each pane contributes at most 2000 lines or 256 KiB, whichever comes first, and a whole session at
most 4 MiB, oldest lines dropped first. Only lines that actually scrolled off a pane are in
scrollback, so what is still on screen at shutdown is not part of it.

Writes are debounced by five seconds and happen on a worker thread, so a burst of splits is one
write and the session actor never blocks on the filesystem. A clean shutdown writes inline, so
stopping a session does not lose the last few seconds of shape.

#### Resuming agents

An agent pane restores as a plain shell, and then reopens the conversation it had. If the agent's
integration reported a session identity (`report-agent --agent-session-id`, which the first-party
provider packages' integrations do), vvmux types that agent's own resume command at the restored
pane's shell:

| Agent | Command |
|---|---|
| claude | `claude --resume <id>` |
| codex | `codex resume <id>` |
| opencode | `opencode --session <id>` |
| hermes | `hermes --resume <id>` |

These come from the provider's manifest, not from a table inside vvmux, so a plugin can make its own
agent resumable:

```toml
launch = { executable = "myagent", resume = ["--continue", "{session_id}"] }
```

Exactly one argument carries `{session_id}` or `{session_path}`, either as the whole argument or
after a `--flag=`. An agent with no `resume` restores as a plain shell.

**A resume fires when a client attaches, never at server start.** A full-screen agent reads its
terminal size as it starts, and one launched against the placeholder geometry would lay itself out
for a window nobody is looking at — so a session left detached starts no agent processes at all.
`inspect` reports `pending_resume` while one is armed.

Three things make a resume not happen, each leaving a working shell: the reporting source does not
own that agent kind (a session identity becomes a command line on your machine, so only the
integration that owns the agent may supply one); two panes named the same conversation, in which case
the first wins and the second stays a shell; or the pane is no longer sitting at its own shell.

A pane that is about to resume gets no scrollback replay even with `pane_history` on — the agent
repaints its own transcript, and replaying underneath would show it twice.

The agent's name comes back with it, once the agent is actually detected again, so
`--alias reviewer` keeps working across a restart. Set `resume_agents = false` to restore agent panes
as plain shells.

### Naming an agent

A pane ID is a stable handle to a *pane*. When you are driving one agent repeatedly, name the agent
instead:

```sh
vvmux msg agent-rename --pane-id 2 --name reviewer
vvmux msg --alias reviewer agent-prompt --wait --until blocked,done 'review the diff'
vvmux msg --alias reviewer agent-read --lines 200
vvmux msg agent-rename --pane-id 2 --clear
```

`--alias NAME` works on every `msg` command in place of `--pane-id`, and keeps resolving after
splits, closes, and renumbering, because the name belongs to the agent process rather than to the
pane or the layout. It is spelled `--alias` rather than `--agent` because `report-agent --agent`
already names an agent *kind*.

A name is one to thirty-two characters, starts with a lowercase letter, and holds lowercase letters,
digits, `-`, and `_`. That is narrower than it needs to be on purpose: a name is typed next to
numeric pane IDs, so anything that could read as a number, a flag, or a differently-cased spelling
of another name is refused.

Names are unique within a session — a second pane asking for a name in use is refused with
`agent_alias_taken` rather than quietly stealing the target. A name requires a detected agent
(`agent_not_detected`), and cannot be attached to a pane whose launch is still in flight
(`agent_launch_pending`), since the agent it would name may never arrive.

A name is cleared when its agent exits or is replaced, so `agent_alias_not_found` means the agent is
gone rather than that you mistyped. Withdrawing a lifecycle report is not the agent leaving: the
agent is still detected in that pane, and keeps its name. Renaming is display state, not lifecycle
state — it never advances the change counter `agent-prompt` uses to detect a stalled prompt.

`agent-start` is the one command a name cannot target: it needs a pane with *no* agent in it, and a
name only ever refers to an agent already running. Its `--pane-id` stays required.

`agent-start` launches a recognized agent in a pane that is sitting at a shell prompt, and returns
only once that same pane is detected running it:

```sh
vvmux msg agent-start --kind claude --pane-id 2
vvmux msg agent-start --kind codex --pane-id 2 --timeout 60s -- --model gpt-5.4
```

The pane must be an *available shell*: its own shell, at its prompt, with nothing else in the
foreground. A pane running a command, holding an editor, or already hosting an agent is refused
with `agent_pane_busy` — the check is a fresh look at the process table, not a cached one, because
typing into a pane that is busy would feed whatever is running there instead. Arguments after `--`
are passed through verbatim and quoted for the pane's shell, so an argument containing spaces stays
one argument.

The reply means the agent is running, not that the command was typed: it resolves on `idle` or
`blocked`, never on `working`, since an agent painting its first screen can look busy before it can
accept anything. Failures are named rather than left to a timeout — `invalid_agent_kind` for an
agent that is not enabled, `agent_not_launchable` for a detection-only provider that declares no
launch command, `agent_kind_mismatch` when a different agent appears, and `agent_start_failed` when
the pane exits or nothing starts before `--timeout` (3s–300s, default 30s).

Providers declare their launch command in the manifest:

```toml
[[agents]]
id = "claude"
name = "Claude Code"
process = { executables = ["claude", "claude-code"] }
launch = { executable = "claude" }
```

The executable is a bare command name resolved through the pane's `PATH`, never a path — the point
is to run whatever the user's environment means by `claude`. An agent with no `launch` block is
detection-only: it is still recognized when a user starts it by hand, but `agent-start` refuses it
rather than guessing a command from the detection matchers, which describe a *running* agent
(wrapper scripts, package paths) and not the way to start one.

`agent-prompt` is the agent-aware input path. It writes the prompt first, waits at least 200 ms,
then writes Enter separately so full-screen agents do not receive one pasted submission. With
`--wait`, it returns when one of the comma-separated `--until` states is reached. A pane that does
not transition within 5 seconds after submission returns `agent_prompt_stalled`; the request's
3s-300s timeout still wins when it is shorter. `agent-send-keys` sends up to 32 allow-listed
navigation/control keys in one ordered write. Both commands require a detected agent that is ready
for input and refuse an ordinary shell pane.

```sh
vvmux msg agent-prompt --pane-id 2 --wait --until blocked,done --timeout 5m \
  'Implement the parser change and report any ambiguity.'
vvmux msg agent-send-keys --pane-id 2 --key esc --key up
```

`agent-read` retrieves prior context owned by an idle alternate-screen application. It temporarily
scrolls upward, merges stable snapshots, restores the original viewport, and returns at most
`--lines` 1000 lines. At most eight reads may run per session. It refuses non-agent, non-idle,
primary-screen, mouse-less, and copy-mode panes rather than injecting speculative input. When a
full-screen application does not satisfy these gates, ask the agent to write a Markdown file and
read that file directly instead of scraping the terminal.

```sh
vvmux msg agent-read --pane-id 2 --lines 200
vvmux msg agent-explain --pane-id 2
```

Agent launch accepts at most 32 arguments of at most 4096 bytes each. POSIX shells and PowerShell
are supported; Windows `cmd` is deliberately excluded because vvmux does not implement its quoting
rules.

`agent-explain` answers why a pane shows the state it shows. It replays the live detection
snapshot through the active manifest and reports the rule that decided, every rule that was
evaluated with its region and evidence, and the reason when none matched — `rule_matched`,
`state_preserved`, `no_rule_matched`, `startup_grace`, or `reported`. It is read-only and is the
tool to reach for when writing rules for a new provider.

```sh
vvmux msg agent-explain --pane-id 2 | jq '.explain | {decision, matched_rule}'
vvmux msg agent-explain --pane-id 2 | jq '.explain.rules[] | select(.matched)'
```

Rules are evaluated even while a report holds authority, so a hook that disagrees with the screen
is visible rather than hidden; `decision` says which one won. A pane with no detected or reported
agent returns `agent_not_detected` rather than an empty result.

Rules run by descending priority and support `idle`, `working`, `blocked`, and state-preserving
`unknown`, the screen/OSC regions used by the first-party providers, and nested `contains`, `regex`,
`line_regex`, `all`, `any`, and `not` gates. Agent IDs are globally unique among enabled plugins;
disable the current provider before enabling a replacement with the same ID. Manifests are bounded
to 16 agents and 64 rules per agent, and the live catalog is bounded to 64 agents.

### Lifecycle integrations

Screen rules classify what a pane looks like. An agent's own lifecycle hook is what reports native
session identity — the thing a resume is rebuilt from — and, for OpenCode, its full lifecycle
state. Those hooks are declared by the provider package and installed with it:

```sh
vvmux plugin install opencode        # installs the package and its integration
vvmux plugin integrate dev.vivido.agent.opencode   # re-run or repair
vvmux plugin list                    # shows each integration and whether it is current
vvmux plugin uninstall dev.vivido.agent.opencode   # removes hooks, then the package
```

An integration is manifest data, not vvmux code: `[[integrations]]` names the agent's config
directory, the files to place in it, and the registrations that make the agent run them. A package
declaring one must hold the `integration.write` permission, which appears in the install preview
and forces fresh confirmation if an update adds it. See `official-plugins/README.md` for the
authoring contract.

What the engine guarantees:

- Every managed file carries `VVMUX_INTEGRATION_ID` and `VVMUX_INTEGRATION_VERSION` in its first
  lines, and a file without the matching id is never replaced or removed — so a hook you wrote
  yourself is safe, and is reported rather than clobbered.
- A missing agent config directory is a skip with a hint, not a failure; create it (or set the
  agent's own override) and run `vvmux plugin integrate`. Claude uses `CLAUDE_CONFIG_DIR` or
  `~/.claude`, Codex `CODEX_HOME` or `~/.codex`, Hermes `HERMES_HOME` or `~/.hermes`, and OpenCode
  `~/.config/opencode`.
- JSON edits preserve every foreign hook; an unparsable config file refuses the whole install
  unchanged. Claude's `settings.json` is reparsed and rewritten as JSON, so comments are not
  retained.
- Codex keeps `[features] hooks = true` after uninstall, matching Codex's global feature semantics.
- Hermes plugin files install safely, but its YAML is not edited: the install prints the exact
  `plugins.enabled` stanza to add by hand.
- Claude and Codex shell hooks require `python3`.
- An integration failure never fails the package install, and a hand-edited file never blocks an
  uninstall.

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
  x_percent = 5
  y_percent = 10
  pinned = true
```

Splits may nest and contain 2–16 children. `sizes` are relative integer weights from 1 through
1000; omitting them gives every child equal weight. A layout may contain up to 16 tabs and 64
panes total. Each pane label is tab-local and unique, and `focus` names one of those labels.
Floating-only tabs are valid. `command` is one shell command line passed to `shell -c`, `cwd`
accepts `~/`, and `hold = true` preserves command output after exit. If one pane cannot spawn, its
leaf is removed and its siblings keep their exact owner-scoped layout; a layout where every pane
fails falls back to one shell tab.

A floating pane's `width_percent`/`height_percent` (10–100) size it against the tab content area.
The optional `x_percent`/`y_percent` (0–100) fix its top-left edge instead of centering it; a
float without them is centered with a small cascade offset from the one before it. A session
starts detached against a placeholder host, so these percentages are re-proportioned onto the
real host when a client attaches, and again on every host resize — until the float is moved or
resized by hand, which fixes its geometry.

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

The gateway lists, creates, and exclusively attaches to sessions, and runs automation on them. It
serves no HTML or JavaScript and does not expose session kill operations on the loopback listener.

Possession of the bearer token is equivalent to shell access, which is too much authority to hand an
agent that only needs to drive automation. A **scoped token** carries less:

```sh
vvmux token create-scoped --scope automation-read      # observations only
vvmux token create-scoped --scope automation-input     # observations and mutations
```

A scoped token cannot attach a terminal at all, and uses `select_session` instead — which opens a
session for automation without evicting whoever is sitting at it. Its scope is enforced against the
same per-method class `capabilities` advertises, so a method added without a class does not compile
rather than silently widening every scoped credential. The bearer token is unchanged and keeps full
authority; scoped tokens live beside it in the same owner-only record. Plain xterm.js clients can
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
When a remote vvmux attachment arrives through Vivido's ConPTY carrier, the foreground client also
repairs ConPTY's missing F12 key-release report for a focused pane that requested Kitty key-event
types. This keeps release-gated emergency shortcuts working without changing input for ordinary
PTY attachments or applications that requested press events only.

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
at 60% by 60% by default, with a minimum 4-by-2 content area plus frame, and keep re-proportioning
to those percentages as the host resizes until moved or resized by hand.

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

Intentionally absent: an in-terminal marketplace, a bundled web UI, direct non-loopback/TLS serving,
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

## Updates and API schema

Linux and macOS builds can select `stable` (the default) or `preview`, check for a release, and
replace the current user-owned executable. The updater accepts only HTTPS, bounds every download,
verifies the release manifest with the public key compiled into the binary, then checks the signed
size and SHA-256 before an atomic same-directory replacement. It never sends credentials and does
not restart live session daemons. Windows remains managed by the Vivido Suite installer.

Official release automation requires the repository variable `VVMUX_UPDATE_PUBLIC_KEY_HEX` and
the matching secret `VVMUX_UPDATE_SIGNING_KEY_PEM`; local builds intentionally carry a fail-closed
development key. `vvmux api schema --json` emits a deterministic Draft 2020-12 document tagged with
the exact VVMX version for client/tooling discovery.

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
