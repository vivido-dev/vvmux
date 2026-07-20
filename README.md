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

Windows releases are versioned ZIP files. Run the signed `install.ps1` from the extracted archive
to install `vvmux.exe` below `%LOCALAPPDATA%\Programs\vvmux\bin` and add that exact directory to the
user PATH. `uninstall.ps1` refuses to run while a live session exists, removes the installed binary
and its PATH entry, and preserves `%APPDATA%\vvmux`, including `config.toml`.

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
vvmux --config PATH ...            use an explicit strict TOML config
```

Only one client can be attached to a session. `--replace` sends a clean detach to the old client
before the new client is admitted.

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
| `Ctrl-b f` / `Ctrl-b F` | Create a floating pane / show or hide ordinary floats |
| `Ctrl-b P` | Pin or unpin the focused floating pane |
| `Ctrl-b m` / `Ctrl-b r` | Enter floating move / resize mode |
| `Ctrl-b d` | Detach |
| `Ctrl-b [` / `Ctrl-b ]` | Copy mode / paste copy buffer |

Copy mode accepts arrows, Page Up/Down, Space to start selection, Enter to copy, and `q` or Escape
to cancel. Copies are capped at 1 MiB and the client emits OSC 52. Paste honors the focused
application's bracketed-paste mode and neutralizes embedded bracketed-paste terminators.

In floating move or resize mode, arrows step by one cell and Shift-Arrow steps by five. Enter
commits and Escape restores the rectangle captured on entry. Zoom hides every other pane, including
pinned floats, without mutating the tiled tree, floating rectangles, visibility, pins, z-order, or
focus.

Mouse clicks focus panes. Tiled border drags resize. On a floating pane, the top title frame moves
the pane, while side/bottom frames and corners resize it; drag geometry is based on the press-time
rectangle and total pointer delta. Mouse input is translated into pane-local SGR coordinates when
the application requested mouse reporting; Shift cancels or prevents pane dragging and forces
vvmux scrolling/copy behavior.

Strict floating defaults live under `[floating]`: `default_width_percent` and
`default_height_percent` accept 10–100, and `border_drag_margin` accepts 1–4. New floats are centered
at 60% by 60% by default, with a minimum 4-by-2 content area plus frame.

## Vivid behavior

Every pane receives a distinct 256-bit `VIVID_TOKEN` and the session-wide virtual presenter
endpoint. The server validates producer handshakes, single-use media tickets, request/object scope,
source and scene quotas, monotonic packet/frame sequences, images, rasters, transactions, and
authenticated anchor markers.

Static encoded images and the latest raster are retained within the configured aggregate budget.
Timed audio/video continues to receive credits while detached but payloads are discarded. On a new
projection, live video gets `NEED_KEYFRAME` with a new minimum epoch; only that fresh keyframe and
later packets are eligible for forwarding. Audio resumes with newly arriving packets. EOS video
does not acquire a reconstructed poster.

The foreground client reconciles stable source and `(producer, node, fragment)` identities into the
current Vivido session, reuses unchanged sources/media channels, resolves virtual anchors, and
applies an exact negotiated clip rectangle for every pane. Higher pane outer rectangles are opaque
media occluders, including their frames. A logical media node is split into at most eight exact
signed-32.32 fragments that share one source; a projection contains at most 256 upstream nodes and
omits lower-priority background media when that budget is exhausted. Source creation and scene
control are pipelined and correlated; uncertain partial reconciliation reconnects once from the
newest authoritative snapshot.

Each outer media connection has a bounded source-specific writer and credit ledger. A blocked video
socket cannot stop linked audio, another pane, terminal rendering, or control. The virtual
presenter's one-packet grant is returned after the outer record write succeeds, so linked audio
pre-roll reaches Vivido before `PLAY` without inheriting an outer RTT stop-and-wait. The playing
snapshot is ordered before the first post-PLAY keyframe. EOS marks the end of submission but leaves
already-buffered outer video and linked audio playing until explicit stop or source teardown.

vvmux validates and forwards the Vivid portable profiles without transcoding, including canonical
Opus, Vorbis, and FLAC initialization. Outer `NEED_KEYFRAME` and source loss are routed back to the
matching inner source without replacing unrelated topology.

Control uses inherited `VIVID_ENDPOINT`. If inherited `VIVID_ENDPOINT_BULK` is present, only the
foreground bridge uses it for non-control outer connections; the hidden server and pane processes
never receive the outer endpoint or token. A bulk connection may fall back to the primary endpoint
only before `ATTACH_CHANNEL` consumes its ticket.

If the outer terminal has no Vivid capability, or its presenter rejects `node-clip-rect-v1`,
terminal use continues without media and the client emits a single status/title warning.

## Current scope

Implemented: native Unix sockets and owner-restricted Windows named pipes; Unix PTYs and
ConPTY/Job Object panes; Unix and Windows console clients; named detachable sessions, tabs, tiled
and tab-scoped floating/pinned shell panes, zoom, scrollback/copy/paste, ordered overlap
composition, mouse focus/move/resize/forwarding, status line, strict TOML, VVMX IPC, exact
fragment-aware pane media occlusion, static rehydration, timed-media headless semantics,
full-duplex outer control, linked A/V projection, and optional bulk-media endpoint discovery.

Intentionally absent: plugins, web/remote clients, command panes, arbitrary action sockets, startup
layout scripts, stacked panes, pane-class conversion, multi-pane selection, scrollback
editing/search, source transcoding, mirrored multi-client sessions, WinPTY, MSI/service installs,
machine-wide PATH changes, and arbitrary configured shell argument lists.

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
