# HerdR agent-state provenance

The foreground-process aliases, state-transition model, bounded OSC capture, and the initial
Claude Code, Codex CLI, OpenCode, and Hermes terminal signals were adapted from HerdR commit
`6c6ddcd49384d6ea9f0ee2e63bf7b2643dfd5bcf`.

Source areas consulted:

- `src/pane/agent_detection.rs`, `src/pane/osc.rs`, and `src/terminal/state.rs`
- `src/detect/manifest.rs` and the four manifests under `src/detect/manifests/`
- `src/platform/linux.rs`, `src/platform/macos.rs`, and `src/platform/windows.rs`
- `src/integration/assets/opencode/herdr-agent-state.js`

HerdR is licensed under Apache License 2.0. The adapted implementation uses vvmux's own PTY,
session actor, terminal emulator, IPC, and rendering abstractions rather than retaining HerdR's
UI or server architecture.

The adapted first-party rules are now shipped as the four data-only plugin manifests under
`builtin-plugins/`; user agent providers use the same public manifest schema.

## Per-feature adaptations

Later features adapted from the same HerdR commit, recorded as they land:

- **Display-only metadata tokens** (`AgentMetadata` in `agent.rs`) — the patch/TTL/expiry model
  is adapted from `src/metadata_tokens.rs`. vvmux binds tokens to the agent runtime rather than to
  a separate pane record, keeps them out of `AgentSnapshot` so lifecycle waiters and events do not
  observe display churn, and shares the existing per-source report sequence table instead of
  HerdR's separate one.
- **Terminal notifications** (`notify.rs`) — backend detection and the OSC 9 / OSC 99 / tmux
  passthrough encodings are adapted from `src/terminal_notify.rs`. vvmux emits them from the
  foreground client on an explicit `ServerMessage::Notify` rather than from the process that
  detects the transition, because the hidden server must not learn anything about the outer
  terminal. HerdR's embedded mp3 playback is not ported; an optional user-configured
  `sound_command` replaces it, so vvmux ships no audio assets and no per-OS player table.
- **Agent launch** (`agent_drive.rs`, `agent-start` in `session.rs`) — the pane-shell allow-list,
  the "foreground group is the pane's own single shell" availability rule, and the POSIX and
  PowerShell argument quoting are adapted from `src/platform/mod.rs` and `src/platform/macos.rs`;
  the settle-then-detect readiness model follows `src/app/agents.rs` and
  `reconcile_managed_agent_at` in `src/terminal/state.rs`, including the 3 s settle and the
  3 s–300 s timeout bounds. Three deliberate divergences: `cmd` is excluded from the shell
  allow-list, because this module produces neither quoting style it parses; the launch command
  lives in the provider's plugin manifest (`[agents.launch]`) rather than in a hardcoded Rust
  table, so users can add or override providers as data; and readiness additionally waits out
  vvmux's own detection startup grace, which HerdR does not have to consider because its
  classification has no equivalent forced-idle window. HerdR's agent names/aliases
  (`agent start <name>`, `agent rename`) are not ported here; vvmux targets panes by ID.
- **Alternate-screen transcript reads** (`alt_read.rs`, `agent-read` in `session.rs`) — the
  70%-similar viewport settling, 30%-overlap upward alignment, fixed-header-safe prepend merge,
  soft-wrap reconstruction, three-event wheel steps, and bounded harvest/restore phases are
  adapted from `src/server/alt_screen_read.rs` and `src/terminal/history_read.rs`. vvmux drives the
  phases on its existing session-actor deadlines and writes through its bounded PTY input queue;
  it additionally encodes synthetic coordinates in pixels when the application enabled DEC 1016.
