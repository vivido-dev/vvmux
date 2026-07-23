use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::ipc::{Action, Axis, Direction, FloatingEditCommand, MouseEvent, MouseKind};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParsedInput {
    Input(Vec<u8>),
    Action(Action),
    Mouse(MouseEvent),
    Detach,
}

/// Fragment-safe parser for the intentionally tiny floating-edit key language. A leading ESC is
/// held briefly so an arrow split across terminal reads is not mistaken for a bare Escape. Any
/// byte sequence outside the language is returned to the normal prefix/mouse/input parser.
#[derive(Default)]
pub(crate) struct FloatEditScanner {
    pending: Vec<u8>,
    pending_since: Option<Instant>,
}

impl FloatEditScanner {
    pub(crate) const ESCAPE_DELAY: Duration = Duration::from_millis(25);

    pub(crate) fn scan(&mut self, bytes: &[u8]) -> (Vec<FloatingEditCommand>, Vec<u8>) {
        let mut commands = Vec::new();
        let mut forward = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if self.pending.is_empty() {
                match byte {
                    b'\r' | b'\n' => {
                        commands.push(FloatingEditCommand::Commit);
                        forward.extend_from_slice(&bytes[index + 1..]);
                        break;
                    }
                    0x1b => {
                        self.pending.push(byte);
                        self.pending_since = Some(Instant::now());
                    }
                    _ => forward.push(byte),
                }
                index += 1;
                continue;
            }

            self.pending.push(byte);
            index += 1;
            if let Some(command) = float_edit_sequence(&self.pending) {
                let terminal = matches!(
                    command,
                    FloatingEditCommand::Commit | FloatingEditCommand::Cancel
                );
                commands.push(command);
                self.pending.clear();
                self.pending_since = None;
                if terminal {
                    forward.extend_from_slice(&bytes[index..]);
                    break;
                }
            } else if !float_edit_sequence_prefix(&self.pending) {
                forward.append(&mut self.pending);
                self.pending_since = None;
            }
        }
        (commands, forward)
    }

    pub(crate) fn expire(&mut self, now: Instant) -> Option<FloatingEditCommand> {
        let since = self.pending_since?;
        if self.pending == b"\x1b" && now.saturating_duration_since(since) >= Self::ESCAPE_DELAY {
            self.pending.clear();
            self.pending_since = None;
            Some(FloatingEditCommand::Cancel)
        } else {
            None
        }
    }

    /// Clear a no-longer-current mode and return any incomplete bytes for ordinary input.
    pub(crate) fn reset(&mut self) -> Vec<u8> {
        self.pending_since = None;
        std::mem::take(&mut self.pending)
    }
}

fn float_edit_sequence(sequence: &[u8]) -> Option<FloatingEditCommand> {
    let (direction, cells) = match sequence {
        b"\x1b[A" => (Direction::Up, 1),
        b"\x1b[B" => (Direction::Down, 1),
        b"\x1b[C" => (Direction::Right, 1),
        b"\x1b[D" => (Direction::Left, 1),
        b"\x1b[1;2A" => (Direction::Up, 5),
        b"\x1b[1;2B" => (Direction::Down, 5),
        b"\x1b[1;2C" => (Direction::Right, 5),
        b"\x1b[1;2D" => (Direction::Left, 5),
        _ => return None,
    };
    Some(FloatingEditCommand::Step { direction, cells })
}

fn float_edit_sequence_prefix(sequence: &[u8]) -> bool {
    const SEQUENCES: [&[u8]; 8] = [
        b"\x1b[A",
        b"\x1b[B",
        b"\x1b[C",
        b"\x1b[D",
        b"\x1b[1;2A",
        b"\x1b[1;2B",
        b"\x1b[1;2C",
        b"\x1b[1;2D",
    ];
    SEQUENCES
        .iter()
        .any(|candidate| candidate.starts_with(sequence))
}

pub(crate) struct PrefixParser {
    prefix_byte: u8,
    bindings: HashMap<u8, Action>,
    prefix: bool,
    sequence: Vec<u8>,
    mouse_sequence: Vec<u8>,
    confirm_close: bool,
}

impl Default for PrefixParser {
    fn default() -> Self {
        Self::new(0x02, &BTreeMap::new())
    }
}

impl PrefixParser {
    pub(crate) fn new(prefix_byte: u8, configured: &BTreeMap<String, String>) -> Self {
        let bindings = configured
            .iter()
            .filter_map(|(chord, action)| {
                let bytes = chord.as_bytes();
                (bytes.len() == 1)
                    .then_some(bytes[0])
                    .zip(parse_configured_action(action))
            })
            .collect();
        Self {
            prefix_byte,
            bindings,
            prefix: false,
            sequence: Vec::new(),
            mouse_sequence: Vec::new(),
            confirm_close: false,
        }
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<ParsedInput> {
        let mut output = Vec::new();
        let mut ordinary = Vec::new();
        for &byte in bytes {
            if self.confirm_close {
                self.confirm_close = false;
                if matches!(byte, b'y' | b'Y') {
                    output.push(ParsedInput::Action(Action::ClosePane));
                }
                continue;
            }
            if !self.sequence.is_empty() {
                self.sequence.push(byte);
                if let Some(command) = prefix_sequence(&self.sequence) {
                    output.push(ParsedInput::Action(command));
                    self.sequence.clear();
                    self.prefix = false;
                } else if self.sequence.len() >= 7 {
                    self.sequence.clear();
                    self.prefix = false;
                }
                continue;
            }
            if !self.prefix {
                if !self.mouse_sequence.is_empty() {
                    self.mouse_sequence.push(byte);
                    let valid_prefix = match self.mouse_sequence.len() {
                        2 => self.mouse_sequence == b"\x1b[",
                        3 => self.mouse_sequence == b"\x1b[<",
                        _ => true,
                    };
                    if !valid_prefix {
                        ordinary.extend_from_slice(&self.mouse_sequence);
                        self.mouse_sequence.clear();
                    } else if matches!(byte, b'M' | b'm') {
                        if !ordinary.is_empty() {
                            output.push(ParsedInput::Input(std::mem::take(&mut ordinary)));
                        }
                        if let Some(mouse) = parse_sgr_mouse(&self.mouse_sequence) {
                            output.push(ParsedInput::Mouse(mouse));
                        } else {
                            ordinary.extend_from_slice(&self.mouse_sequence);
                        }
                        self.mouse_sequence.clear();
                    } else if self.mouse_sequence.len() >= 64 {
                        ordinary.extend_from_slice(&self.mouse_sequence);
                        self.mouse_sequence.clear();
                    }
                    continue;
                }
                if byte == 0x1b {
                    self.mouse_sequence.push(byte);
                    continue;
                }
                if byte == self.prefix_byte {
                    if !ordinary.is_empty() {
                        output.push(ParsedInput::Input(std::mem::take(&mut ordinary)));
                    }
                    self.prefix = true;
                } else {
                    ordinary.push(byte);
                }
                continue;
            }
            if let Some(action) = self.bindings.get(&byte).cloned() {
                output.push(ParsedInput::Action(action));
                self.prefix = false;
                continue;
            }
            match byte {
                value if value == self.prefix_byte => {
                    output.push(ParsedInput::Input(vec![self.prefix_byte]))
                }
                b'%' => output.push(ParsedInput::Action(Action::Split(Axis::Vertical))),
                b'"' => output.push(ParsedInput::Action(Action::Split(Axis::Horizontal))),
                b'c' => output.push(ParsedInput::Action(Action::NewTab)),
                b'n' => output.push(ParsedInput::Action(Action::NextTab)),
                b'p' => output.push(ParsedInput::Action(Action::PreviousTab)),
                b'z' => output.push(ParsedInput::Action(Action::ToggleZoom)),
                b'f' => output.push(ParsedInput::Action(Action::NewFloatingPane)),
                b'F' => output.push(ParsedInput::Action(Action::ToggleFloatingPanes)),
                b'P' => output.push(ParsedInput::Action(Action::TogglePanePinned)),
                b'm' => output.push(ParsedInput::Action(Action::EnterFloatingMoveMode)),
                b'r' => output.push(ParsedInput::Action(Action::EnterFloatingResizeMode)),
                b'd' => output.push(ParsedInput::Detach),
                b'[' => output.push(ParsedInput::Action(Action::EnterCopyMode)),
                b']' => output.push(ParsedInput::Action(Action::Paste)),
                b'x' => self.confirm_close = true,
                b'0'..=b'9' => output.push(ParsedInput::Action(Action::SelectTab(
                    (byte - b'0') as usize,
                ))),
                0x1b => {
                    self.sequence.push(byte);
                    continue;
                }
                _ => {}
            }
            self.prefix = false;
        }
        if !ordinary.is_empty() {
            output.push(ParsedInput::Input(ordinary));
        }
        output
    }
}

pub(crate) fn parse_configured_action(action: &str) -> Option<Action> {
    match action {
        "split-horizontal" => Some(Action::Split(Axis::Horizontal)),
        "split-vertical" => Some(Action::Split(Axis::Vertical)),
        "focus-left" => Some(Action::Focus(Direction::Left)),
        "focus-right" => Some(Action::Focus(Direction::Right)),
        "focus-up" => Some(Action::Focus(Direction::Up)),
        "focus-down" => Some(Action::Focus(Direction::Down)),
        "resize-left" => Some(Action::Resize(Direction::Left)),
        "resize-right" => Some(Action::Resize(Direction::Right)),
        "resize-up" => Some(Action::Resize(Direction::Up)),
        "resize-down" => Some(Action::Resize(Direction::Down)),
        "new-tab" => Some(Action::NewTab),
        "next-tab" => Some(Action::NextTab),
        "previous-tab" => Some(Action::PreviousTab),
        "close-pane" => Some(Action::ClosePane),
        "toggle-zoom" => Some(Action::ToggleZoom),
        "copy-mode" => Some(Action::EnterCopyMode),
        "paste" => Some(Action::Paste),
        "new-floating-pane" => Some(Action::NewFloatingPane),
        "toggle-floating-panes" => Some(Action::ToggleFloatingPanes),
        "toggle-pane-pinned" => Some(Action::TogglePanePinned),
        "enter-floating-move-mode" => Some(Action::EnterFloatingMoveMode),
        "enter-floating-resize-mode" => Some(Action::EnterFloatingResizeMode),
        _ => None,
    }
}

fn parse_sgr_mouse(sequence: &[u8]) -> Option<MouseEvent> {
    let text = std::str::from_utf8(sequence).ok()?;
    let release = text.ends_with('m');
    let fields = text.strip_prefix("\x1b[<")?.strip_suffix(['M', 'm'])?;
    let mut fields = fields.split(';');
    let raw = fields.next()?.parse::<u16>().ok()?;
    let x = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let y = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    if fields.next().is_some() {
        return None;
    }
    let button = (raw & 0b11) as u8;
    let kind = if raw & 64 != 0 {
        MouseKind::Wheel
    } else if release {
        MouseKind::Release
    } else if raw & 32 != 0 {
        MouseKind::Move
    } else {
        MouseKind::Press
    };
    Some(MouseEvent {
        button,
        x,
        y,
        kind,
        shift: raw & 4 != 0,
    })
}

fn prefix_sequence(sequence: &[u8]) -> Option<Action> {
    let direction = match sequence {
        b"\x1b[A" | b"\x1b[1;5A" => Direction::Up,
        b"\x1b[B" | b"\x1b[1;5B" => Direction::Down,
        b"\x1b[C" | b"\x1b[1;5C" => Direction::Right,
        b"\x1b[D" | b"\x1b[1;5D" => Direction::Left,
        _ => return None,
    };
    if sequence.len() > 3 {
        Some(Action::Resize(direction))
    } else {
        Some(Action::Focus(direction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_preserves_literal_prefix_actions_and_mouse() {
        let mut parser = PrefixParser::default();
        let commands = parser.feed(b"a\x02\x02\x02%z");
        assert!(matches!(&commands[0], ParsedInput::Input(bytes) if bytes == b"a"));
        assert!(matches!(&commands[1], ParsedInput::Input(bytes) if bytes == b"\x02"));
        assert_eq!(
            commands[2],
            ParsedInput::Action(Action::Split(Axis::Vertical))
        );
        assert!(matches!(&commands[3], ParsedInput::Input(bytes) if bytes == b"z"));

        assert_eq!(
            parser.feed(b"\x1b[<64;5;7M"),
            [ParsedInput::Mouse(MouseEvent {
                button: 0,
                x: 4,
                y: 6,
                kind: MouseKind::Wheel,
                shift: false,
            })]
        );
    }

    #[test]
    fn floating_scanner_is_fragment_safe() {
        let mut scanner = FloatEditScanner::default();
        let (commands, forward) = scanner.scan(b"\x1b[");
        assert!(commands.is_empty());
        assert!(forward.is_empty());
        let (commands, forward) = scanner.scan(b"A");
        assert_eq!(
            commands,
            [FloatingEditCommand::Step {
                direction: Direction::Up,
                cells: 1,
            }]
        );
        assert!(forward.is_empty());
    }
}
