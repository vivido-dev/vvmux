use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::ipc::{Action, Axis, Direction, FloatingEditCommand, MouseEvent, MouseKind};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParsedInput {
    Input(Vec<u8>),
    Action(Action),
    Mouse(MouseEvent, MouseCoordinates),
    /// The host terminal gained or lost focus.
    Focus(bool),
    Detach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseCoordinates {
    Cells,
    Pixels,
}

/// How long a lone ESC may be held while the parser waits to see whether it begins a longer
/// terminal sequence. Nothing may hold a bare Escape past this: an editor such as vim leaves
/// insert mode only when the byte itself arrives, so an indefinite hold reads as a lost keypress.
pub(crate) const ESCAPE_DELAY: Duration = Duration::from_millis(25);

/// Fragment-safe parser for the intentionally tiny floating-edit key language. A leading ESC is
/// held briefly so an arrow split across terminal reads is not mistaken for a bare Escape. Any
/// byte sequence outside the language is returned to the normal prefix/mouse/input parser.
#[derive(Default)]
pub(crate) struct FloatEditScanner {
    pending: Vec<u8>,
    pending_since: Option<Instant>,
}

impl FloatEditScanner {
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
        if self.pending == b"\x1b" && now.saturating_duration_since(since) >= ESCAPE_DELAY {
            self.pending.clear();
            self.pending_since = None;
            Some(FloatingEditCommand::Cancel)
        } else {
            None
        }
    }

    /// Whether a bare Escape is being held and therefore needs an expiry poll.
    pub(crate) fn holds_bare_escape(&self) -> bool {
        self.pending == b"\x1b"
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
    escape_sequence: Vec<u8>,
    escape_since: Option<Instant>,
    confirm_close: bool,
    mouse_coordinates: MouseCoordinates,
    keyboard_flags: u8,
    /// Kitty key codepoints whose press was consumed as a vvmux command, so their release and
    /// repeat reports must not reach the pane. Keyed by codepoint alone: the modifier state at
    /// release can differ from the press (releasing Ctrl before the prefix key reports the same
    /// key with no modifiers), and a key is still the same key.
    suppressed_kitty_releases: HashSet<u32>,
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
            escape_sequence: Vec::new(),
            escape_since: None,
            confirm_close: false,
            mouse_coordinates: MouseCoordinates::Cells,
            keyboard_flags: 0,
            suppressed_kitty_releases: HashSet::new(),
        }
    }

    pub(crate) fn set_mouse_coordinates(&mut self, coordinates: MouseCoordinates) {
        self.mouse_coordinates = coordinates;
    }

    pub(crate) fn set_keyboard_flags(&mut self, flags: u8) {
        self.keyboard_flags = flags;
        if flags == 0 {
            self.suppressed_kitty_releases.clear();
        }
    }

    /// Release a lone Escape once it can no longer be the start of a mouse report or focus event.
    /// The parser has to hold ESC while that is still possible, but holding it until the next
    /// keystroke arrives makes vim and other modal programs look like they need Escape twice.
    pub(crate) fn expire(&mut self, now: Instant) -> Option<Vec<u8>> {
        let since = self.escape_since?;
        if self.escape_sequence == b"\x1b" && now.saturating_duration_since(since) >= ESCAPE_DELAY {
            self.escape_since = None;
            Some(std::mem::take(&mut self.escape_sequence))
        } else {
            None
        }
    }

    /// Whether a bare Escape is being held and therefore needs an expiry poll.
    pub(crate) fn holds_bare_escape(&self) -> bool {
        self.escape_sequence == b"\x1b"
    }

    fn clear_escape(&mut self) {
        self.escape_sequence.clear();
        self.escape_since = None;
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<ParsedInput> {
        let mut output = Vec::new();
        let mut ordinary = Vec::new();
        for &byte in bytes {
            if self.confirm_close && self.keyboard_flags == 0 {
                match byte {
                    b'y' | b'Y' => {
                        self.confirm_close = false;
                        output.push(ParsedInput::Action(Action::ResolveClosePaneConfirmation(
                            true,
                        )));
                    }
                    b'n' | b'N' | 0x1b => {
                        self.confirm_close = false;
                        output.push(ParsedInput::Action(Action::ResolveClosePaneConfirmation(
                            false,
                        )));
                    }
                    _ => {}
                }
                continue;
            }
            if !self.sequence.is_empty() {
                self.sequence.push(byte);
                if let Some(focused) = focus_event(&self.sequence) {
                    // A window focus change is not a chord. It must neither be mistaken for one
                    // of the prefix arrow sequences nor consume the prefix the user just typed.
                    output.push(ParsedInput::Focus(focused));
                    self.sequence.clear();
                    continue;
                }
                if let Some(command) = prefix_sequence(&self.sequence) {
                    output.push(ParsedInput::Action(command));
                    self.sequence.clear();
                    self.prefix = false;
                } else if self.keyboard_flags != 0 && self.sequence.starts_with(b"\x1b[") {
                    let final_byte = self.sequence.len() >= 3 && (0x40..=0x7e).contains(&byte);
                    if final_byte {
                        let sequence = std::mem::take(&mut self.sequence);
                        if byte == b'u'
                            && let Some(key) = parse_kitty_key(&sequence)
                        {
                            if key.kind != KittyKeyKind::Press {
                                // A release or repeat report is never the command that follows the
                                // prefix. Consume it when its press was consumed, otherwise hand it
                                // to the pane, but never let it run a binding or cancel the prefix.
                                let consumed = if key.kind == KittyKeyKind::Release {
                                    self.suppressed_kitty_releases.remove(&key.codepoint)
                                } else {
                                    self.suppressed_kitty_releases.contains(&key.codepoint)
                                };
                                if !consumed {
                                    output.push(ParsedInput::Input(sequence));
                                }
                            } else if (57441..=57452).contains(&key.codepoint) {
                                // Modifier reports surround a Kitty-encoded Ctrl+prefix chord but
                                // are not themselves the following vvmux command. Forward them so
                                // a modifier press sent before the prefix still receives its keyup.
                                output.push(ParsedInput::Input(sequence));
                            } else if let Some(command_byte) = key.command_byte() {
                                let literal_prefix = command_byte == self.prefix_byte;
                                self.handle_prefix_byte(command_byte, &sequence, &mut output);
                                if !literal_prefix {
                                    self.suppressed_kitty_releases.insert(key.codepoint);
                                }
                            } else {
                                self.prefix = false;
                            }
                        } else {
                            self.prefix = false;
                        }
                    } else if self.sequence.len() >= 64 {
                        self.sequence.clear();
                        self.prefix = false;
                    }
                } else if self.sequence.len() >= 7 {
                    self.sequence.clear();
                    self.prefix = false;
                }
                continue;
            }
            if !self.prefix {
                if !self.escape_sequence.is_empty() {
                    self.escape_sequence.push(byte);
                    if let Some(focused) = focus_event(&self.escape_sequence) {
                        if !ordinary.is_empty() {
                            output.push(ParsedInput::Input(std::mem::take(&mut ordinary)));
                        }
                        output.push(ParsedInput::Focus(focused));
                        self.clear_escape();
                        continue;
                    }
                    let csi = self.escape_sequence.starts_with(b"\x1b[");
                    if self.escape_sequence.len() == 2 && !csi {
                        ordinary.extend_from_slice(&self.escape_sequence);
                        self.clear_escape();
                    } else if self.escape_sequence.starts_with(b"\x1b[<")
                        && matches!(byte, b'M' | b'm')
                    {
                        if !ordinary.is_empty() {
                            output.push(ParsedInput::Input(std::mem::take(&mut ordinary)));
                        }
                        if let Some(mouse) = parse_sgr_mouse(&self.escape_sequence) {
                            output.push(ParsedInput::Mouse(mouse, self.mouse_coordinates));
                        } else {
                            ordinary.extend_from_slice(&self.escape_sequence);
                        }
                        self.clear_escape();
                    } else if csi
                        && self.escape_sequence.len() >= 3
                        && (0x40..=0x7e).contains(&byte)
                    {
                        let sequence = std::mem::take(&mut self.escape_sequence);
                        self.escape_since = None;
                        if byte == b'u'
                            && self.keyboard_flags != 0
                            && let Some(key) = parse_kitty_key(&sequence)
                        {
                            if key.kind == KittyKeyKind::Release {
                                if !self.suppressed_kitty_releases.remove(&key.codepoint) {
                                    ordinary.extend_from_slice(&sequence);
                                }
                            } else if key.kind == KittyKeyKind::Repeat
                                && self.suppressed_kitty_releases.contains(&key.codepoint)
                            {
                                // A held vvmux command key remains consumed until its release.
                            } else if let Some(command_byte) = key.command_byte() {
                                if self.confirm_close
                                    || self.prefix
                                    || command_byte == self.prefix_byte
                                {
                                    if !ordinary.is_empty() {
                                        output.push(ParsedInput::Input(std::mem::take(
                                            &mut ordinary,
                                        )));
                                    }
                                    let literal_prefix =
                                        self.prefix && command_byte == self.prefix_byte;
                                    self.handle_prefix_byte(command_byte, &sequence, &mut output);
                                    if !literal_prefix {
                                        self.suppressed_kitty_releases.insert(key.codepoint);
                                    }
                                } else {
                                    ordinary.extend_from_slice(&sequence);
                                }
                            } else {
                                ordinary.extend_from_slice(&sequence);
                            }
                        } else {
                            ordinary.extend_from_slice(&sequence);
                        }
                    } else if self.escape_sequence.len() >= 64 {
                        ordinary.extend_from_slice(&self.escape_sequence);
                        self.clear_escape();
                    }
                    continue;
                }
                if byte == 0x1b {
                    self.escape_sequence.push(byte);
                    self.escape_since = Some(Instant::now());
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
            self.handle_prefix_byte(byte, &[self.prefix_byte], &mut output);
        }
        if !ordinary.is_empty() {
            output.push(ParsedInput::Input(ordinary));
        }
        output
    }

    fn handle_prefix_byte(&mut self, byte: u8, literal: &[u8], output: &mut Vec<ParsedInput>) {
        if self.confirm_close {
            match byte {
                b'y' | b'Y' => {
                    self.confirm_close = false;
                    output.push(ParsedInput::Action(Action::ResolveClosePaneConfirmation(
                        true,
                    )));
                }
                b'n' | b'N' | 0x1b => {
                    self.confirm_close = false;
                    output.push(ParsedInput::Action(Action::ResolveClosePaneConfirmation(
                        false,
                    )));
                }
                _ => {}
            }
            return;
        }
        if !self.prefix {
            debug_assert_eq!(byte, self.prefix_byte);
            self.prefix = true;
            return;
        }
        if let Some(action) = self.bindings.get(&byte).cloned() {
            if matches!(action, Action::BeginClosePaneConfirmation) {
                self.confirm_close = true;
            }
            output.push(ParsedInput::Action(action));
            self.prefix = false;
            return;
        }
        match byte {
            value if value == self.prefix_byte => output.push(ParsedInput::Input(literal.to_vec())),
            b'%' => output.push(ParsedInput::Action(Action::Split(Axis::Vertical))),
            b'"' => output.push(ParsedInput::Action(Action::Split(Axis::Horizontal))),
            b'c' => output.push(ParsedInput::Action(Action::NewTab)),
            b'n' => output.push(ParsedInput::Action(Action::NextTab)),
            b'p' => output.push(ParsedInput::Action(Action::PreviousTab)),
            b'h' => output.push(ParsedInput::Action(Action::Focus(Direction::Left))),
            b'j' => output.push(ParsedInput::Action(Action::Focus(Direction::Down))),
            b'k' => output.push(ParsedInput::Action(Action::Focus(Direction::Up))),
            b'l' => output.push(ParsedInput::Action(Action::Focus(Direction::Right))),
            b'w' => output.push(ParsedInput::Action(Action::ToggleTabNavigator)),
            b',' => output.push(ParsedInput::Action(Action::BeginRenameTab)),
            b'z' => output.push(ParsedInput::Action(Action::ToggleZoom)),
            b's' => output.push(ParsedInput::Action(Action::BeginSaveLayout)),
            b'S' => output.push(ParsedInput::Action(Action::ToggleSyncInput)),
            b'f' => output.push(ParsedInput::Action(Action::NewFloatingPane)),
            b'F' => output.push(ParsedInput::Action(Action::ToggleFloatingPanes)),
            b'P' => output.push(ParsedInput::Action(Action::TogglePanePinned)),
            b'm' => output.push(ParsedInput::Action(Action::EnterFloatingMoveMode)),
            b'r' => output.push(ParsedInput::Action(Action::EnterFloatingResizeMode)),
            b'a' => output.push(ParsedInput::Action(Action::ToggleAgentNavigator)),
            b'd' => output.push(ParsedInput::Detach),
            b'[' => output.push(ParsedInput::Action(Action::EnterCopyMode)),
            b']' => output.push(ParsedInput::Action(Action::Paste)),
            b'x' => {
                self.confirm_close = true;
                output.push(ParsedInput::Action(Action::BeginClosePaneConfirmation));
            }
            b'1'..=b'9' => output.push(ParsedInput::Action(Action::SelectTab(
                (byte - b'1') as usize,
            ))),
            0x1b => {
                self.sequence.push(byte);
                return;
            }
            _ => {}
        }
        self.prefix = false;
    }
}

pub(crate) fn parse_configured_action(action: &str) -> Option<Action> {
    if action.starts_with("plugin:") {
        return Some(Action::Plugin(action.to_owned()));
    }
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
        "tab-navigator" => Some(Action::ToggleTabNavigator),
        "rename-tab" => Some(Action::BeginRenameTab),
        "confirm-close-pane" => Some(Action::BeginClosePaneConfirmation),
        "close-pane" => Some(Action::ClosePane),
        "toggle-zoom" => Some(Action::ToggleZoom),
        "toggle-sync-input" => Some(Action::ToggleSyncInput),
        "copy-mode" => Some(Action::EnterCopyMode),
        "paste" => Some(Action::Paste),
        "new-floating-pane" => Some(Action::NewFloatingPane),
        "toggle-floating-panes" => Some(Action::ToggleFloatingPanes),
        "toggle-pane-pinned" => Some(Action::TogglePanePinned),
        "enter-floating-move-mode" => Some(Action::EnterFloatingMoveMode),
        "enter-floating-resize-mode" => Some(Action::EnterFloatingResizeMode),
        "agent-navigator" => Some(Action::ToggleAgentNavigator),
        "save-layout" => Some(Action::BeginSaveLayout),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KittyKeyKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KittyKey {
    codepoint: u32,
    shifted_codepoint: Option<u32>,
    modifiers: u8,
    kind: KittyKeyKind,
}

impl KittyKey {
    /// Map a Kitty key report to the one-byte language used by vvmux's prefix chords. The raw
    /// report is still forwarded byte-for-byte for every key which is not a vvmux command.
    fn command_byte(self) -> Option<u8> {
        let shift = self.modifiers & 1 != 0;
        let control = self.modifiers & 4 != 0;
        let mut codepoint = if shift {
            self.shifted_codepoint.unwrap_or(self.codepoint)
        } else {
            self.codepoint
        };
        if shift
            && self.shifted_codepoint.is_none()
            && let Some(character) = char::from_u32(codepoint)
        {
            codepoint = u32::from(character.to_ascii_uppercase());
        }
        let byte = u8::try_from(codepoint).ok()?;
        if control {
            match byte {
                b'@'..=b'_' | b'`'..=b'~' => Some(byte & 0x1f),
                b'?' => Some(0x7f),
                _ => None,
            }
        } else {
            Some(byte)
        }
    }
}

fn parse_kitty_key(sequence: &[u8]) -> Option<KittyKey> {
    let text = std::str::from_utf8(sequence).ok()?;
    let body = text.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let mut fields = body.split(';');
    let mut codepoints = fields.next()?.split(':');
    let codepoint = codepoints.next()?.parse().ok()?;
    let shifted_codepoint = codepoints.next().and_then(|value| value.parse().ok());
    let mut modifiers_and_kind = fields.next().unwrap_or("1").split(':');
    let encoded_modifiers = modifiers_and_kind.next()?.parse::<u16>().ok()?;
    let modifiers = u8::try_from(encoded_modifiers.saturating_sub(1)).ok()?;
    let kind = match modifiers_and_kind.next().unwrap_or("1") {
        "1" => KittyKeyKind::Press,
        "2" => KittyKeyKind::Repeat,
        "3" => KittyKeyKind::Release,
        _ => return None,
    };
    Some(KittyKey {
        codepoint,
        shifted_codepoint,
        modifiers,
        kind,
    })
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

/// A focus in/out report from the host terminal, which the client asks for with DEC private mode
/// 1004. These are terminal state, not typed input: forwarding them into a pane wrote `ESC[I` and
/// `ESC[O` into whatever program was running there.
fn focus_event(sequence: &[u8]) -> Option<bool> {
    match sequence {
        b"\x1b[I" => Some(true),
        b"\x1b[O" => Some(false),
        _ => None,
    }
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
            parser.feed(b"\x02S"),
            [ParsedInput::Action(Action::ToggleSyncInput)]
        );
        assert_eq!(
            parser.feed(b"\x02a"),
            [ParsedInput::Action(Action::ToggleAgentNavigator)]
        );

        assert_eq!(
            parser.feed(b"\x1b[<64;5;7M"),
            [ParsedInput::Mouse(
                MouseEvent {
                    button: 0,
                    x: 4,
                    y: 6,
                    kind: MouseKind::Wheel,
                    shift: false,
                },
                MouseCoordinates::Cells
            )]
        );
    }

    #[test]
    fn tmux_navigation_and_one_based_tab_keys_are_default_bindings() {
        let mut parser = PrefixParser::default();
        assert_eq!(
            parser.feed(b"\x02h\x02j\x02k\x02l"),
            [
                ParsedInput::Action(Action::Focus(Direction::Left)),
                ParsedInput::Action(Action::Focus(Direction::Down)),
                ParsedInput::Action(Action::Focus(Direction::Up)),
                ParsedInput::Action(Action::Focus(Direction::Right)),
            ]
        );
        assert_eq!(
            parser.feed(b"\x021\x029"),
            [
                ParsedInput::Action(Action::SelectTab(0)),
                ParsedInput::Action(Action::SelectTab(8)),
            ]
        );
        assert!(parser.feed(b"\x020").is_empty());
        assert_eq!(
            parser.feed(b"\x02w\x02,"),
            [
                ParsedInput::Action(Action::ToggleTabNavigator),
                ParsedInput::Action(Action::BeginRenameTab),
            ]
        );
        // Save and synchronized input differ only by case, so both must stay reachable.
        assert_eq!(
            parser.feed(b"\x02s\x02S"),
            [
                ParsedInput::Action(Action::BeginSaveLayout),
                ParsedInput::Action(Action::ToggleSyncInput),
            ]
        );
    }

    #[test]
    fn close_confirmation_is_visible_and_consumes_until_yes_or_no() {
        let mut parser = PrefixParser::default();
        assert_eq!(
            parser.feed(b"\x02x"),
            [ParsedInput::Action(Action::BeginClosePaneConfirmation)]
        );
        assert!(
            parser.feed(b"?").is_empty(),
            "unrelated input stays consumed"
        );
        assert_eq!(
            parser.feed(b"n"),
            [ParsedInput::Action(Action::ResolveClosePaneConfirmation(
                false
            ))]
        );
        assert_eq!(
            parser.feed(b"\x02xY"),
            [
                ParsedInput::Action(Action::BeginClosePaneConfirmation),
                ParsedInput::Action(Action::ResolveClosePaneConfirmation(true)),
            ]
        );
    }

    #[test]
    fn kitty_close_confirmation_suppresses_command_releases() {
        let mut parser = PrefixParser::default();
        parser.set_keyboard_flags(31);
        assert!(parser.feed(b"\x1b[98;5u").is_empty());
        assert!(parser.feed(b"\x1b[98;5:3u").is_empty());
        assert_eq!(
            parser.feed(b"\x1b[120u"),
            [ParsedInput::Action(Action::BeginClosePaneConfirmation)]
        );
        assert!(parser.feed(b"\x1b[120;1:3u").is_empty());
        assert_eq!(
            parser.feed(b"\x1b[121u"),
            [ParsedInput::Action(Action::ResolveClosePaneConfirmation(
                true
            ))]
        );
        assert!(parser.feed(b"\x1b[121;1:3u").is_empty());
    }

    #[test]
    fn focus_reports_are_parsed_instead_of_reaching_a_pane_as_input() {
        let mut parser = PrefixParser::default();
        assert_eq!(
            parser.feed(b"\x1b[O"),
            [ParsedInput::Focus(false)],
            "a focus report must never be forwarded as pane input"
        );
        assert_eq!(parser.feed(b"\x1b[I"), [ParsedInput::Focus(true)]);

        // Split across reads, and mixed with ordinary bytes on either side.
        let mut parser = PrefixParser::default();
        let held = parser.feed(b"a\x1b[");
        assert_eq!(held.len(), 1);
        assert!(matches!(&held[0], ParsedInput::Input(bytes) if bytes == b"a"));
        let completed = parser.feed(b"Ob");
        assert_eq!(completed[0], ParsedInput::Focus(false));
        assert!(matches!(&completed[1], ParsedInput::Input(bytes) if bytes == b"b"));

        // Unfocusing the window while a prefix is pending leaves the chord usable.
        let mut parser = PrefixParser::default();
        assert!(parser.feed(b"\x02").is_empty());
        assert_eq!(parser.feed(b"\x1b[O"), [ParsedInput::Focus(false)]);
        assert_eq!(
            parser.feed(b"z"),
            [ParsedInput::Action(Action::ToggleZoom)],
            "a focus report must not consume the pending prefix"
        );
    }

    #[test]
    fn pixel_mouse_reports_keep_their_coordinate_model() {
        let mut parser = PrefixParser::default();
        parser.set_mouse_coordinates(MouseCoordinates::Pixels);
        assert!(matches!(
            parser.feed(b"\x1b[<0;121;81M").as_slice(),
            [ParsedInput::Mouse(
                MouseEvent { x: 120, y: 80, .. },
                MouseCoordinates::Pixels
            )]
        ));
    }

    #[test]
    fn kitty_backspace_is_forwarded_byte_exact_across_fragments() {
        let mut parser = PrefixParser::default();
        parser.set_keyboard_flags(31);
        assert!(parser.feed(b"\x1b[127;1").is_empty());
        assert_eq!(
            parser.feed(b"u\x1b[127;1:3u"),
            [ParsedInput::Input(b"\x1b[127;1u\x1b[127;1:3u".to_vec())]
        );
    }

    #[test]
    fn kitty_control_prefix_still_runs_vvmux_commands() {
        let mut parser = PrefixParser::default();
        parser.set_keyboard_flags(31);
        assert!(parser.feed(b"\x1b[98;5u").is_empty());
        assert!(parser.feed(b"\x1b[98;5:3u").is_empty());
        assert_eq!(
            parser.feed(b"\x1b[99u"),
            [ParsedInput::Action(Action::NewTab)]
        );
        assert!(
            parser.feed(b"\x1b[99;1:3u").is_empty(),
            "the release for a consumed command must not leak into the pane"
        );
    }

    #[test]
    fn kitty_prefix_survives_a_release_reported_without_its_modifier() {
        // Neovim asks for Kitty flags 3, so the host terminal reports key releases. Lifting Ctrl
        // before the prefix key reports that release with no modifiers, which must neither run a
        // binding nor cancel the pending prefix: `prefix n` has to stay a tab switch.
        let mut parser = PrefixParser::default();
        parser.set_keyboard_flags(3);
        assert!(parser.feed(b"\x1b[98;5u").is_empty());
        assert!(
            parser.feed(b"\x1b[98;1:3u").is_empty(),
            "a release whose modifiers no longer match the press is not pane input either"
        );
        assert_eq!(
            parser.feed(b"n"),
            [ParsedInput::Action(Action::NextTab)],
            "the prefix must still be pending after its own key is released"
        );

        // The same holds for the release order that keeps Ctrl down.
        assert!(parser.feed(b"\x1b[98;5u").is_empty());
        assert!(parser.feed(b"\x1b[98;5:3u").is_empty());
        assert_eq!(parser.feed(b"c"), [ParsedInput::Action(Action::NewTab)]);

        // A release for a key the prefix did not consume still reaches the pane, in order.
        assert!(parser.feed(b"\x02").is_empty());
        assert_eq!(
            parser.feed(b"\x1b[106;1:3u"),
            [ParsedInput::Input(b"\x1b[106;1:3u".to_vec())]
        );
        assert_eq!(parser.feed(b"z"), [ParsedInput::Action(Action::ToggleZoom)]);
    }

    #[test]
    fn bare_escape_reaches_the_pane_without_a_second_press() {
        let mut parser = PrefixParser::default();
        assert!(
            parser.feed(b"\x1b").is_empty(),
            "ESC is held while it may still begin a mouse or focus sequence"
        );
        assert!(parser.holds_bare_escape());
        assert_eq!(
            parser.expire(Instant::now()),
            None,
            "the hold window must not be skipped"
        );
        assert_eq!(
            parser.expire(Instant::now() + ESCAPE_DELAY),
            Some(b"\x1b".to_vec()),
            "a lone Escape must be forwarded once no longer sequence is possible"
        );
        assert!(!parser.holds_bare_escape());
        assert_eq!(parser.expire(Instant::now() + ESCAPE_DELAY), None);

        // A sequence still in flight is not a bare Escape and keeps its full parse.
        let mut parser = PrefixParser::default();
        assert!(parser.feed(b"\x1b[").is_empty());
        assert!(!parser.holds_bare_escape());
        assert_eq!(
            parser.expire(Instant::now() + Duration::from_secs(1)),
            None,
            "a fragmented mouse or focus prefix must not become Escape"
        );
        assert_eq!(parser.feed(b"O"), [ParsedInput::Focus(false)]);

        // Escape immediately followed by another key still forwards both, in order, at once.
        let mut parser = PrefixParser::default();
        let commands = parser.feed(b"\x1b:");
        assert!(matches!(&commands[0], ParsedInput::Input(bytes) if bytes == b"\x1b:"));
        assert!(!parser.holds_bare_escape());
        assert_eq!(parser.expire(Instant::now() + ESCAPE_DELAY), None);
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
