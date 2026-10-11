# Kitty graphics on every outer terminal — plan and notes

Written 2026-10-11. Implemented in vvmux on 2026-10-11. Planning baselines: `vvmux` `771ab37`, `vivido`
`5ea8058` (first Vivido with Kitty graphics), `vvte` `c4b3f66`.

Part 1 is the plan. Part 2 is what was learned while preparing it: how the pieces behave today,
the facts the plan rests on, and what was considered and left out.

## Part 1 — Plan

### Problem

vvmux already forwards a deliberately narrow Kitty subset from panes: direct upload, `q=2`,
Unicode virtual placements (`U=1`), delete by id, and its own support query. That is exactly what
`kitim` sends once it sees `VVMUX_PANE_ID`. But the client advertises the capability only when the
outer `TERM` is exactly `xterm-kitty` or `xterm-ghostty`. Inside Vivido it is never set, so kitim
reports "Kitty graphics are unavailable in this vvmux attachment" even though Vivido can now draw
the images.

`TERM` cannot settle this. Vivido builds with and without Kitty graphics both report `TERM=vivido`
(or `xterm-256color` when its terminfo entry is missing). The outer terminal has to be asked.

### Scope decided

- Panes keep the subset they have today. No new commands are accepted from panes.
- Vivido is reached by the same passthrough kitty and ghostty use, not through Vivid media.
- Images are still dropped when the presenting client leaves or the role moves. New: vvmux stops
  leaving them behind in the host terminal.

### Result by outer terminal

| Outer terminal | How it is decided | Result |
|---|---|---|
| kitty, ghostty | exact `TERM`, unchanged, nothing asked | graphics, as today |
| Vivido with Kitty graphics | asked; answers `OK` | graphics |
| Older Vivido | asked; answers only the sentinel | none: query → `ENOTSUP`, placeholders blanked |
| xterm and everything else | not a candidate; **nothing is written to it** | none, as today |
| Windows console, browser/VVWS gateway | not probed | none, as today |

### Design

1. **Ask before attaching.** `ClientMessage::Attach` already carries `kitty_graphics`, so deciding
   it before `request_attachment` needs no VVMX change and no version bump. A bump would force
   every running session to restart. The probe runs where `current_display_metrics` already
   inspects the host, before the alternate screen, so a refused attach still leaves the caller's
   terminal untouched.
2. **Only candidates are asked.** A candidate is `TERM=vivido`, or the presence of outer Vivid
   credentials (Vivido without its terminfo entry, or reached over `vvssh`). The probe is the
   evidence; candidacy only decides whether any bytes are written. xterm never receives an APC.
3. **The probe is the standard query plus a sentinel.**
   `ESC _ G i=<id>,s=1,v=1,a=q,t=d,f=24;AAAA ESC \` followed by `ESC [ c`. Every terminal answers
   the device-attributes request, so the wait ends at that reply: milliseconds locally, and just
   as fast on an older Vivido. The cap is one second. Supported means the graphics reply for our
   id says `OK`.
4. **Nothing typed is lost, nothing stray is typed.** Bytes read during the probe that are not its
   replies are handed to the input loop as its first input. If the cap expires first, the replies
   may still arrive later; a one-shot filter drops exactly those two sequences so they never reach
   a pane as keystrokes.
5. **Uploads go ahead of the synchronized repaint.** Frames are written as `BSU + bytes + ESU`.
   Vivido's update buffer is 2 MiB, so an upload inside the bracket ends the update early and the
   repaint tears. The client peels the leading `ESC _ G … ESC \` run off the frame and writes
   `uploads, BSU, repaint, ESU` in the same locked write. A virtual placement shows nothing by
   itself, so order and atomicity are unchanged.
6. **Leave the host clean.** Images with virtual placements survive the host's own
   alternate-screen clearing. On teardown a capable client writes
   `ESC _ G a=d,d=R,x=1,y=4294967295,q=2 ESC \` before leaving the alternate screen. The host keeps
   one image store per screen, so nothing the user displayed outside vvmux is touched. A client
   that loses the presenter role while staying attached gets the same command with its next frame.

### Changes

All in `vvmux/`.

| Where | What |
|---|---|
| `src/host_probe.rs` (new) | Platform-neutral and unit-testable: host classification (`Known` / `Ask` / `No`), the query bytes, an incremental reply parser returning supported / unsupported / undecided plus the non-reply bytes, and the late-reply filter. |
| `src/platform/unix.rs` | `probe_kitty_graphics(deadline)` beside `current_display_metrics`: raw mode through a restoring guard, `poll` + read as `ClientTerminal::read_input` does, no mode left changed. `ClientTerminal` learns whether to send the cleanup command in `Drop`. |
| `src/platform/windows.rs` | Same signature; returns unsupported without writing. |
| `src/client.rs` | Replace `host_supports_kitty_graphics` with classification + probe in `attach`. Seed `PrefixParser` with the typed-ahead bytes. Run input through the late-reply filter. Split uploads from the repaint in `run_terminal_output`. Log the outcome in the existing `client.attach.outer` event. |
| `src/session/clients.rs`, `render.rs`, `mod.rs` | `AttachedClient` gets a one-shot "reset host graphics" flag, set in `set_presenter` for a demoted Kitty-capable client and consumed in `render_client`. |
| `README.md`, `ARCHITECTURE.md` | "Kitty graphics compatibility", "Current scope", and the VVMX 20 paragraph say Vivido attachments never advertise Kitty graphics. Rewrite them to describe candidates, the probe, and the cleanup. |

Not changing:

- The pane-facing subset and its validator (`vvmux-terminal/src/kitty.rs`).
- VVMX and its version.
- The gateway adapter, which keeps `kitty_graphics: false`.
- Capability use in the session: `handle_kitty_graphics`, `KittyTransferBuffer`,
  `suppress_kitty_placeholders`.
- Vivido and kitim.

### Verification

From `vvmux/`:

```sh
cargo fmt --all --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Tests to add:

- Classification table: kitty and ghostty known; `vivido` and Vivid-credentialed hosts asked;
  xterm, `vvmux`, near-miss names, and no `TERM` not asked.
- Reply parser at every split: `OK`; an error reply; sentinel only; replies in either order;
  another image id ignored; typed-ahead before, between, and after the replies preserved in order;
  a reply cut off at the deadline is not forwarded as input.
- Late-reply filter removes each expected reply once and passes everything else, including a
  genuine later escape sequence.
- Probe against a real PTY pair standing in for the terminal: supported, sentinel-only, silent
  (cap honored), and termios restored on every path including a write error.
- A non-candidate attach writes nothing before the alternate-screen sequence.
- Output thread: uploads precede the bracket, the repaint stays inside it, brackets stay balanced,
  and a frame with no uploads is byte-identical to today's.
- Session: demoting a Kitty presenter sends the reset once and only to that client; teardown sends
  it only when the capability was advertised. Existing transfer and query tests keep passing.

End to end, in headless Vivido sessions driven with `vivido msg` (recipe in the notes):

1. New Vivido → `vvmux` → `kitim <png>` in a pane. The screenshot shows the image, and `vvmux`
   inspect reports the client with `kitty_graphics: true`.
2. Split the pane, switch tabs, zoom. The image is clipped to its pane and hides with it.
3. `kitim <video>` in a pane. It plays, and repeated screenshots show no torn repaint.
4. Detach and reattach. The image is gone until redrawn, by design, and nothing stale appears.
   Printing placeholder cells for the old id on a fresh alternate screen shows nothing.
5. Older Vivido → the same steps. kitim reports graphics unavailable, there are no stray
   characters, and attach is not perceptibly slower.
6. `TERM=xterm-256color` with no Vivid environment under a PTY harness. No probe bytes are
   written, and kitim reports unavailable.

Cannot be verified on the planning machine: real kitty and ghostty (their path is unchanged and
covered by unit tests), and Linux and Windows builds.

## Part 2 — Notes

### How it works today

- **Pane side.** `vvmux-terminal/src/kitty.rs` scans pane output for `ESC _ G … ESC \` ahead of the
  parser, with packets capped at 8 KiB and payloads at 4096 bytes. `validate_kitty_command` accepts
  only: the support query in one exact form; `a=t` / `a=T` with `f=24|32`, `t=d`, and for `a=T`
  `i≠0, U=1, c≠0, r≠0`; `a=p` with `U=1`; `a=d` with `d=i|I`; and keyless continuation chunks.
  Every accepted command must carry `q=2`. Anything else falls through to the parser as ordinary
  bytes, which ignores it.
- **Session side.** `handle_kitty_graphics` (`src/session/mod.rs`) answers the query on the pane
  PTY from the presenter's capability, and pushes packets into `KittyTransferBuffer`: per-pane
  assembly, 64 MiB across the session, completed transfers queued in order.
- **Render.** `render_client` (`src/session/render.rs`) drains completed transfers only for the
  presenter, prepends them to that client's frame, and forces a full repaint. Other clients get
  `suppress_kitty_placeholders`. Placeholder cells are ordinary grid cells, so pane clipping,
  floats, tabs, zoom, and scrollback need nothing special.
- **Client side.** `attach` (`src/client.rs`) computes `kitty_graphics` from `TERM` and sends it
  in `ClientMessage::Attach`. The server stores it on `AttachedClient`; `presents` is
  `(vivid || kitty_graphics)` and the media request.
- **Lifetime.** `release_presentation` clears the buffer on any presenter change. Nothing is
  deleted in the host.

### Facts the plan rests on

- **Vivido answers the standard query.** `ESC _ G i=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC \` returns
  `ESC _ G i=31;OK ESC \`, checked live. With `q=2` Vivido stays silent, as the protocol says, so
  the probe must not carry `q=2`.
- **vvmux's own query form requires `q=2`** and vvmux answers it anyway. That contract is between
  vvmux and multiplexer-aware programs. It is why a probe aimed at Vivido and a probe aimed at an
  outer vvmux pane cannot be the same bytes, and why nested vvmux is left out.
- **Why panes must not get the standard query answered.** A program that is not
  multiplexer-aware (chafa, timg, icat, yazi) probes with the standard form. If vvmux said `OK`,
  it would send cursor-positioned placements, which vvmux drops, and show nothing instead of
  falling back to text.
- **An older Vivido is safe to probe.** It consumes an APC silently and answers `ESC [ c`. The
  installed `/usr/local/bin/vivido` is 0.5.3, which predates Kitty graphics and serves as the
  negative case.
- **Vivido's synchronized-update buffer is 2 MiB** (`SYNC_BUFFER_SIZE` in `vvte/src/ansi.rs`). At
  the limit the update is applied early and the rest is processed unsynchronized. A 640x480 RGBA
  frame is 1.6 MB of base64; anything larger overflows it.
- **Virtual placements outlive screen clears.** Kitty's `grman_clear` skips them, and Vivido's
  `ImageStore::clear` does the same. An image with a virtual placement is therefore never freed by
  entering or leaving the alternate screen, only by quota, explicit delete, or reset.
- **`d=R` deletes by id range, virtual placements included, and frees the images.** Verified in
  Vivido's store tests and in Kitty's `handle_delete_command`. Kitty older than 0.33 rejects the
  flag and ignores the command, which is harmless.
- **Image stores are per screen** in Kitty (`main_grman` / `alt_grman`) and Vivido. The vvmux
  client lives on the alternate screen, so a range delete there cannot reach primary-screen images.
- **What kitim sends under vvmux.** Query:
  `ESC _ G a=q,t=d,f=24,s=1,v=1,i=<id>,q=2;AAAA ESC \` then `ESC [ c`, with a 750 ms wait.
  Upload: `a=T,i=<id>,f=32,s=,v=,c=,r=,U=1[,C=1],q=2,m=` in 4096-character chunks. Then
  placeholder rows with a 24-bit foreground color and three diacritics (row, column, id high
  byte). For video it alternates two ids and deletes an id before reusing it, so the upload of the
  next frame never touches the one on screen.
- **Vivido keeps Kitty commands out of its automation transcript** and does not wake its UI for
  payload that has not completed an image. Image traffic from vvmux costs Vivido little.

### Measured in Vivido, for expectations

Taken while adding Kitty graphics to Vivido, on an M-series Mac, headless:

- Chunked RGBA through the whole input pipeline: about 3 GiB/s.
- kitim 640x480 at 30 fps, direct, no vvmux: about 26% of one core, mostly the kernel's PTY read.
- Under vvmux every completed upload forces a full repaint of that client, which is existing
  behavior. Expect video in a pane to cost more than it does directly.

### Risks and open points

- **Ghostty and `d=R`.** Believed supported, not checked. If it is not, the cleanup is a no-op
  there and behavior is what it is today.
- **Links slower than the cap.** A reply after one second leaves the attachment without graphics
  until the next attach. The late-reply filter keeps the input clean. Raising the cap trades
  attach latency on a dead link for coverage.
- **Image ids are forwarded verbatim.** Two panes that pick the same id collide in the host. This
  is existing behavior; kitim draws ids from a random 32-bit seed, so it is rare. Namespacing would
  mean rewriting ids in packets and in placeholder cell colors.
- **A pane that closes leaves its uploads in the host** until the client detaches. Existing
  behavior, bounded by the host's own quota. Tracking ids per pane would fix it.
- **Windows.** No probe and no capability. ConPTY does not carry APC reliably, so this is not a
  regression.
- **Cross-platform builds** could not be run while planning.

### Considered and left out

- **Terminating the protocol in vvmux.** A per-pane image store, cursor-positioned placements,
  PNG and zlib, every delete selector, replies, and redrawing on the host with vvmux's own ids.
  This is what would make unmodified programs work in a pane. It is a new subsystem in
  `vvmux-terminal` and the session actor. The module written for Vivido
  (`vivido/src/terminal/graphics/`) is nearly terminal-agnostic and could be shared rather than
  written twice.
- **Converting pane images to Vivid media.** Pixel-exact, shared with every Vivid client, and
  restored on reattach. It needs an in-process injection API in `vivid_sdk`'s `VirtualVivid`,
  which today only accepts producers over its socket, and it would be a second delivery path
  beside the Kitty one that kitty and ghostty still need.
- **Retaining uploads and replaying them on attach.** More state in the session actor and a burst
  of upload at attach. The current documentation states the opposite on purpose.
- **Probing after entering the client terminal.** The reply would arrive through the normal input
  stream and the capability would be learned after `Attach`, which needs a new VVMX message and a
  version bump.
- **Probing kitty and ghostty too.** It would make `TERM` a hint everywhere, but a timeout on a
  slow link would take away graphics that work today.

### Recipe: headless Vivido for the end-to-end checks

```sh
V=vivido/target/release/vivido          # new build; /usr/local/bin/vivido for the old one
$V --headless --session kittytest --headless-size 1280x720px
$V msg -t kittytest typing 'vvmux' --window-id 1
$V msg -t kittytest key Enter --window-id 1
$V msg -t kittytest wait screen-stable --window-id 1 --quiet 400ms --timeout 30s
$V msg -t kittytest screenshot --window-id 1 --json    # prints the PNG path
$V msg -t kittytest inspect --window-id 1              # render_optimization counters
$V msg -t kittytest quit
```

- kitim: build with `cargo build --release --locked` in `kitim/`. Test media is in `medias/`
  (`lenna.png`, `581-200x200.jpg`, `test_pattern.mp4`).
- A short GIF: `ffmpeg -t 3 -i medias/test_pattern.mp4 -vf fps=10,scale=320:-1 test.gif`.
- For tmux comparisons use a private server: `tmux -L kittytest -f /dev/null new-session '…'`,
  and `tmux -L kittytest kill-server` afterwards.
- To read a terminal's replies from inside it, put `/dev/tty` in raw mode, write the queries, and
  read until `select` times out. A plain `read` in the shell echoes the reply instead.

## Implementation verification — 2026-10-11

The workspace format check, all-target tests, and Clippy check pass on macOS. Regression coverage
includes every reply split, delayed replies, typed-ahead input, real PTY mode restoration and
timeouts, upload/repaint ordering, output-worker shutdown, and client-scoped one-shot cleanup
through actual attachments. The suite was rerun outside the sandbox so local-socket tests ran.

Headless Vivido 0.5.4 advertised `kitty_graphics: true` and displayed the PNG and looping test
video through kitim. Screenshots confirmed pane clipping, tab hiding, zoom, advancing video,
and absent images after reattach. A fixed image ID was uploaded, then its placeholder was printed
on a fresh alternate screen after detach; it stayed blank, confirming host-store cleanup.
Installed Vivido 0.5.3 advertised `kitty_graphics: false`, and kitim reported the expected unavailable
message without stray probe characters. Real kitty/Ghostty and Linux/Windows builds were not run.
