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
