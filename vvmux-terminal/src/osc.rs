//! Passive OSC observation: titles and progress for agent detection, and OSC 133 shell markers.

/// Most characters of an OSC title or progress payload retained for agent detection.
pub(crate) const AGENT_OSC_MAX_CHARS: usize = 256;

#[derive(Debug, Default)]
pub(crate) struct AgentOscTracker {
    pub(crate) state: OscState,
    pub(crate) body: Vec<u8>,
    pub(crate) title: Option<String>,
    pub(crate) progress: Option<String>,
    pub(crate) shell: ShellIntegration,
}

/// Where a pane's shell says it is, from its OSC 133 markers.
///
/// Reported rather than inferred. Prompt detection by pattern matching is guesswork that breaks on
/// every unusual prompt; a shell that emits these markers is stating the boundary, and one that
/// does not leaves this `Unknown` so a caller can tell "at a prompt" from "no idea".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShellIntegration {
    /// What the shell is doing now.
    pub phase: ShellPhase,
    /// Bumped when a command starts. Zero until one does.
    pub command_id: u64,
    /// The most recent command to have finished.
    pub completed_command_id: u64,
    /// The exit status the last finished command reported, when it reported one.
    pub exit_code: Option<i64>,
}

impl ShellIntegration {
    /// Whether this pane's shell reports command boundaries at all.
    #[must_use]
    pub fn is_active(self) -> bool {
        self.phase != ShellPhase::Unknown
    }
}

/// The shell state reported by OSC 133 markers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShellPhase {
    /// No OSC 133 marker has ever arrived, so nothing is known.
    #[default]
    Unknown,
    /// At a prompt, ready for a command.
    Prompt,
    /// Running a command.
    Running,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum OscState {
    #[default]
    Ground,
    Escape,
    Body,
    BodyEscape,
    Ignoring,
    IgnoringEscape,
    Discarding,
    DiscardingEscape,
}

impl AgentOscTracker {
    /// Longest OSC body tracked; longer sequences are discarded without being retained.
    const MAX_BODY_BYTES: usize = 4096;

    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                OscState::Ground => {
                    if byte == 0x1b {
                        self.state = OscState::Escape;
                    }
                }
                OscState::Escape => match byte {
                    b']' => {
                        self.body.clear();
                        self.state = OscState::Body;
                    }
                    b'P' | b'_' | b'^' | b'X' => self.state = OscState::Ignoring,
                    0x1b => {}
                    _ => self.state = OscState::Ground,
                },
                OscState::Body => match byte {
                    0x07 => self.finish(),
                    0x1b => self.state = OscState::BodyEscape,
                    _ => self.push(byte),
                },
                OscState::BodyEscape => match byte {
                    b'\\' => self.finish(),
                    0x1b => self.push(0x1b),
                    byte => {
                        self.push(0x1b);
                        if matches!(self.state, OscState::Body) {
                            self.push(byte);
                        }
                    }
                },
                OscState::Ignoring => {
                    if byte == 0x1b {
                        self.state = OscState::IgnoringEscape;
                    }
                }
                OscState::IgnoringEscape => {
                    self.state = match byte {
                        b'\\' => OscState::Ground,
                        0x1b => OscState::IgnoringEscape,
                        _ => OscState::Ignoring,
                    };
                }
                OscState::Discarding => match byte {
                    0x07 => self.state = OscState::Ground,
                    0x1b => self.state = OscState::DiscardingEscape,
                    _ => {}
                },
                OscState::DiscardingEscape => {
                    self.state = match byte {
                        b'\\' => OscState::Ground,
                        0x1b => OscState::DiscardingEscape,
                        _ => OscState::Discarding,
                    };
                }
            }
        }
    }

    /// Record one OSC 133 shell-integration marker.
    ///
    /// The four that matter: `A` starts a prompt, `B` ends it and starts the command line, `C`
    /// starts command output, and `D` ends the command and may carry its exit status. Only a shell
    /// configured to emit these produces any of them, so a pane without shell integration simply
    /// stays in the `Unknown` state and every caller that needs a boundary is told so rather than
    /// being given a guess.
    pub(crate) fn shell_marker(&mut self, value: &str) {
        let (kind, payload) = value
            .split_once(';')
            .map_or((value, None), |(kind, rest)| (kind, Some(rest)));
        match kind {
            "A" | "B" => self.shell.phase = ShellPhase::Prompt,
            "C" => {
                self.shell.phase = ShellPhase::Running;
                self.shell.command_id = self.shell.command_id.saturating_add(1);
            }
            "D" => {
                // A `D` without a preceding `C` is a prompt redraw rather than a finished command;
                // counting it would invent a command that never ran.
                if self.shell.phase == ShellPhase::Running {
                    self.shell.completed_command_id = self.shell.command_id;
                    self.shell.exit_code = payload
                        .and_then(|payload| payload.split(';').next())
                        .and_then(|code| code.trim().parse::<i64>().ok());
                }
                self.shell.phase = ShellPhase::Prompt;
            }
            _ => {}
        }
    }

    pub(crate) fn push(&mut self, byte: u8) {
        self.body.push(byte);
        if self.body.len() > Self::MAX_BODY_BYTES {
            self.body.clear();
            self.state = OscState::Discarding;
        } else {
            self.state = OscState::Body;
        }
    }

    pub(crate) fn finish(&mut self) {
        let Some(separator) = self.body.iter().position(|byte| *byte == b';') else {
            self.body.clear();
            self.state = OscState::Ground;
            return;
        };
        let command = &self.body[..separator];
        let payload = &self.body[separator + 1..];
        let value = sanitize_agent_osc(payload);
        match command {
            b"0" | b"2" => self.title = (!value.is_empty()).then_some(value),
            b"9" => self.progress = Some(value),
            b"133" => self.shell_marker(&value),
            _ => {}
        }
        self.body.clear();
        self.state = OscState::Ground;
    }
}

pub(crate) fn sanitize_agent_osc(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload)
        .chars()
        .filter(|character| !character.is_control())
        .take(AGENT_OSC_MAX_CHARS)
        .collect()
}
