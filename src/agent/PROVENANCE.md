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
