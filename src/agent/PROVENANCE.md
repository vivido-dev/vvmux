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

The adapted first-party rules ship as installable plugin packages under the repository's
`official-plugins/` directory, not inside this binary; user agent providers use the same public
manifest schema. Those packages also carry the lifecycle hook assets, whose provenance is recorded
in `official-plugins/PROVENANCE.md`.

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
  classification has no equivalent forced-idle window.
- **Agent names** (`AgentAlias` in `agent.rs`, `agent-rename` and target resolution in
  `session.rs`) — the grammar is ported from HerdR's `valid_agent_name` (`src/app/agents.rs`):
  one to thirty-two characters, leading lowercase letter, lowercase letters, digits, `-` and `_`.
  The lifecycle matches HerdR's too — a name belongs to the agent process rather than the pane, is
  unique among live agents, is refused for a pane with no agent or a launch in flight, and is
  released explicitly. Four deliberate divergences. The flag is `--alias` rather than HerdR's
  positional name-or-pane target, because vvmux already spells an agent *kind* `--agent` on
  `report-agent`, and because a single global flag resolves for every `msg` verb instead of each
  verb growing its own target parser. Resolution is strict: an explicit `--pane-id` wins, then an
  alias, then the focused pane, and naming both is refused rather than silently preferring one —
  HerdR resolves a single target string and reports ambiguity instead. The name is held outside the
  lifecycle snapshot, so renaming cannot advance the transition counter `agent-prompt` reads as its
  stall baseline. And the name survives the detector's first observation of an agent that already
  reported itself, which is the one path where HerdR's equivalent state is rebuilt.
- **Native agent session restore** (`AgentResumePlan` and the restore path in `session.rs`,
  `[agents.launch] resume` in `vvmux-plugin-api`) — the resume command shapes, the reserve-before-arm
  deduplication, the rule that an unsupported or duplicated reference restores as a plain shell, and
  the decision to wait for a client rather than resume at server start are adapted from HerdR's
  `src/agent_resume.rs` and `src/app/agent_resume.rs`. Four deliberate divergences. The per-agent
  commands live in each provider's manifest as an argument template rather than in a hardcoded
  sixteen-entry Rust match, so a user's own plugin can declare a resumable agent without a vvmux
  release — the same providers-as-data decision the launch executable follows. The command line is
  resolved when the resume fires rather than when it is armed, because vvmux compiles its agent
  catalog from the plugin registry after the session actor exists. vvmux needs no equivalent of
  HerdR's 750 ms host-theme wait, because its client sends geometry with the attach. And the restored
  pane's shell is identified from what this session spawned rather than by scanning the process
  table, which the session actor is not allowed to do.
- **Agent prompt and bounded keys** (`agent_drive.rs`, `agent-prompt` and `agent-send-keys` in
  `session.rs`) — the prompt/submit split and 5 s no-transition stall gate are adapted from
  HerdR's agent automation path. vvmux routes both writes through its delayed, bounded PTY input
  queue, keeps lifecycle waits pane-scoped, and exposes a fixed key allow-list instead of arbitrary
  terminal input. The 3 s-300 s request bound is shared with launch.
- **Alternate-screen transcript reads** (`alt_read.rs`, `agent-read` in `session.rs`) — the
  70%-similar viewport settling, 30%-overlap upward alignment, fixed-header-safe prepend merge,
  soft-wrap reconstruction, three-event wheel steps, and bounded harvest/restore phases are
  adapted from `src/server/alt_screen_read.rs` and `src/terminal/history_read.rs`. vvmux drives the
  phases on its existing session-actor deadlines and writes through its bounded PTY input queue;
  it additionally encodes synthetic coordinates in pixels when the application enabled DEC 1016
  and freezes each admitted snapshot rather than sharing mutable harvest state across concurrent
  reads.
