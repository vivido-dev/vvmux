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

/// The longest win32-input-mode record: `ESC [` plus six five-digit fields, five separators, and
/// the final `_`. Anything longer is not one and is passed through.
const WIN32_INPUT_MAX: usize = 2 + 6 * 5 + 5 + 1;
/// Bounds how many bytes one record's repeat count can expand to.
const WIN32_INPUT_MAX_REPEAT: u16 = 64;

const WIN32_RIGHT_ALT: u32 = 0x1;
const WIN32_LEFT_ALT: u32 = 0x2;
const WIN32_RIGHT_CTRL: u32 = 0x4;
const WIN32_LEFT_CTRL: u32 = 0x8;
const WIN32_SHIFT: u32 = 0x10;

/// Translates Windows Terminal's win32-input-mode key records into ordinary terminal input.
///
/// A console can hand this client the host's raw `ESC [ Vk;Sc;Uc;Kd;Cs;Rc _` records instead of
/// the VT bytes it asked for: every press, release, and bare modifier as its own record. Left
/// alone they reach a pane's ConPTY, which decodes them itself, so keys still type but no prefix
/// chord, prompt, or copy-mode key is ever recognized. Decoding here, before anything else reads
/// the input, gives the rest of the client the same bytes a VT console would have produced.
#[derive(Default)]
pub(crate) struct Win32InputDecoder {
    pending: Vec<u8>,
    high_surrogate: Option<u16>,
}

impl Win32InputDecoder {
    pub(crate) fn decode(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.pending.is_empty() {
                if byte == 0x1b {
                    self.pending.push(byte);
                } else {
                    output.push(byte);
                }
                continue;
            }
            self.pending.push(byte);
            let sequence = self.pending.as_slice();
            if sequence.len() == 2 {
                if byte != b'[' {
                    self.flush_into(&mut output);
                }
                continue;
            }
            if byte == b'_' {
                let sequence = std::mem::take(&mut self.pending);
                match parse_win32_record(&sequence) {
                    Some(record) => self.translate(record, &mut output),
                    None => output.extend_from_slice(&sequence),
                }
            } else if !(byte.is_ascii_digit() || byte == b';') || sequence.len() >= WIN32_INPUT_MAX
            {
                self.flush_into(&mut output);
            }
        }
        // A lone ESC or `ESC [` is left to the prefix parser, which owns the bare-Escape timing.
        // Only a tail that is already recognizably a record waits for the rest of it.
        if self.pending.len() <= 2 {
            self.flush_into(&mut output);
        }
        output
    }

    fn flush_into(&mut self, output: &mut Vec<u8>) {
        output.append(&mut self.pending);
    }

    fn translate(&mut self, record: Win32KeyRecord, output: &mut Vec<u8>) {
        if !record.key_down {
            return;
        }
        let alt = record.control_state & (WIN32_LEFT_ALT | WIN32_RIGHT_ALT) != 0;
        let ctrl = record.control_state & (WIN32_LEFT_CTRL | WIN32_RIGHT_CTRL) != 0;
        let shift = record.control_state & WIN32_SHIFT != 0;
        let mut bytes = Vec::new();
        match (record.virtual_key, record.unicode) {
            // Terminals send DEL for Backspace; Ctrl+Backspace keeps the console's BS.
            (0x08, _) => bytes.push(if ctrl { 0x08 } else { 0x7f }),
            (0x09, _) if shift => bytes.extend_from_slice(b"\x1b[Z"),
            (0x20, _) if ctrl => bytes.push(0),
            (_, 0) => {
                let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
                if let Some(sequence) = special_key_sequence(record.virtual_key, modifier) {
                    for _ in 0..record.repeat {
                        output.extend_from_slice(&sequence);
                    }
                }
                // Bare modifiers and keys without a terminal encoding produce no input.
                return;
            }
            (_, unit @ 0xd800..=0xdbff) => {
                self.high_surrogate = Some(unit);
                return;
            }
            (_, unit @ 0xdc00..=0xdfff) => {
                let Some(high) = self.high_surrogate.take() else {
                    return;
                };
                let Some(Ok(character)) = char::decode_utf16([high, unit]).next() else {
                    return;
                };
                let mut buffer = [0; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            }
            (_, unit) => {
                let Some(character) = char::from_u32(unit.into()) else {
                    return;
                };
                let mut buffer = [0; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            }
        }
        self.high_surrogate = None;
        // Alt prefixes ESC as a VT console does; Ctrl+Alt is AltGr and already chose the text.
        if alt && !ctrl {
            bytes.insert(0, 0x1b);
        }
        for _ in 0..record.repeat {
            output.extend_from_slice(&bytes);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Win32KeyRecord {
    virtual_key: u16,
    unicode: u16,
    key_down: bool,
    control_state: u32,
    repeat: u16,
}

/// Parse `ESC [ Vk;Sc;Uc;Kd;Cs;Rc _`. Windows Terminal always sends all six fields, so a shorter
/// `CSI … _` is some other sequence and is left alone. An empty field is zero. The scan code is
/// not needed to reproduce what a VT console would send.
fn parse_win32_record(sequence: &[u8]) -> Option<Win32KeyRecord> {
    let body = sequence.strip_prefix(b"\x1b[")?.strip_suffix(b"_")?;
    let mut fields = [0_u32; 6];
    let mut count = 0;
    for field in body.split(|byte| *byte == b';') {
        let slot = fields.get_mut(count)?;
        count += 1;
        if field.is_empty() {
            continue;
        }
        *slot = std::str::from_utf8(field).ok()?.parse().ok()?;
    }
    if count != fields.len() {
        return None;
    }
    let [
        virtual_key,
        _scan_code,
        unicode,
        key_down,
        control_state,
        repeat,
    ] = fields;
    Some(Win32KeyRecord {
        virtual_key: u16::try_from(virtual_key).ok()?,
        unicode: u16::try_from(unicode).ok()?,
        key_down: key_down != 0,
        control_state,
        repeat: u16::try_from(repeat)
            .unwrap_or(u16::MAX)
            .clamp(1, WIN32_INPUT_MAX_REPEAT),
    })
}

/// The xterm encoding of a key that has no character, with `modifier` as xterm numbers it.
fn special_key_sequence(virtual_key: u16, modifier: u8) -> Option<Vec<u8>> {
    let letter = |final_byte: u8| {
        if modifier == 1 {
            vec![0x1b, b'[', final_byte]
        } else {
            format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes()
        }
    };
    let function = |final_byte: u8| {
        if modifier == 1 {
            vec![0x1b, b'O', final_byte]
        } else {
            format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes()
        }
    };
    let tilde = |number: u8| {
        if modifier == 1 {
            format!("\x1b[{number}~").into_bytes()
        } else {
            format!("\x1b[{number};{modifier}~").into_bytes()
        }
    };
    Some(match virtual_key {
        0x26 => letter(b'A'),
        0x28 => letter(b'B'),
        0x27 => letter(b'C'),
        0x25 => letter(b'D'),
        0x24 => letter(b'H'),
        0x23 => letter(b'F'),
        0x2d => tilde(2),
        0x2e => tilde(3),
        0x21 => tilde(5),
        0x22 => tilde(6),
        0x70 => function(b'P'),
        0x71 => function(b'Q'),
        0x72 => function(b'R'),
        0x73 => function(b'S'),
        0x74 => tilde(15),
        0x75 => tilde(17),
        0x76 => tilde(18),
        0x77 => tilde(19),
        0x78 => tilde(20),
        0x79 => tilde(21),
        0x7a => tilde(23),
        0x7b => tilde(24),
        _ => return None,
    })
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

/// Kitty keyboard flag for reporting key release and repeat events, not only presses.
const KITTY_REPORT_EVENT_TYPES: u8 = 2;

/// ConPTY preserves the legacy F12 press but drops the Kitty release that follows it.
const LEGACY_F12_PRESS: &[u8] = b"\x1b[24~";
const KITTY_F12_RELEASE: &[u8] = b"\x1b[24;1:3~";

/// The Kitty codepoint a plain command byte reports when it is released.
///
/// A release report carries the unshifted key, so `prefix S` presses as `S` and releases as `s`.
/// Layout-specific shifted symbols such as `%` have no base key that can be recovered from the
/// byte alone, and their release reaches the pane as an ordinary report.
fn release_codepoint(byte: u8) -> u32 {
    u32::from(byte.to_ascii_lowercase())
}

pub(crate) struct PrefixParser {
    prefix_byte: u8,
    bindings: HashMap<u8, Action>,
    plugin_bindings: HashMap<u8, Action>,
    prefix: bool,
    direct: bool,
    sequence: Vec<u8>,
    escape_sequence: Vec<u8>,
    escape_since: Option<Instant>,
    confirm_close: bool,
    mouse_coordinates: MouseCoordinates,
    keyboard_flags: u8,
    conpty_input_transport: bool,
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
        Self::new_with_mode(prefix_byte, configured, false)
    }

    pub(crate) fn new_with_mode(
        prefix_byte: u8,
        configured: &BTreeMap<String, String>,
        direct: bool,
    ) -> Self {
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
            plugin_bindings: HashMap::new(),
            prefix: false,
            direct,
            sequence: Vec::new(),
            escape_sequence: Vec::new(),
            escape_since: None,
            confirm_close: false,
            mouse_coordinates: MouseCoordinates::Cells,
            keyboard_flags: 0,
            conpty_input_transport: false,
            suppressed_kitty_releases: HashSet::new(),
        }
    }

    /// Whether the host terminal reports key releases and repeats, not only presses.
    fn reports_key_events(&self) -> bool {
        self.keyboard_flags & KITTY_REPORT_EVENT_TYPES != 0
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

    /// Record whether input reaches this foreground client through ConPTY.
    ///
    /// Pane environments cannot carry this attachment-local fact: the hidden server strips the
    /// outer Vivid namespace, and pane producers need their own anchor transport. The foreground
    /// client is therefore the boundary that repairs ConPTY's missing F12 release.
    pub(crate) fn set_conpty_input_transport(&mut self, conpty: bool) {
        self.conpty_input_transport = conpty;
    }

    pub(crate) fn set_plugin_bindings(&mut self, bindings: Vec<crate::ipc::PluginKeybinding>) {
        self.plugin_bindings = bindings
            .into_iter()
            .filter(|binding| {
                !self.bindings.contains_key(&binding.chord) && !is_core_chord(binding.chord)
            })
            .map(|binding| {
                (
                    binding.chord,
                    Action::Plugin(format!("plugin:{}", binding.action)),
                )
            })
            .collect();
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
                        if self.conpty_input_transport
                            && self.reports_key_events()
                            && sequence == LEGACY_F12_PRESS
                        {
                            ordinary.extend_from_slice(&sequence);
                            ordinary.extend_from_slice(KITTY_F12_RELEASE);
                        } else if byte == b'u'
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
            // A command taken from a plain byte still has a Kitty release report on its way when
            // the host reports key events: an unambiguous key such as `w` presses as text and only
            // its release is escaped. The pane never saw the press, so it must not see the release
            // either.
            let consumed = byte != self.prefix_byte && self.reports_key_events();
            self.handle_prefix_byte(byte, &[self.prefix_byte], &mut output);
            if consumed {
                self.suppressed_kitty_releases
                    .insert(release_codepoint(byte));
            }
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
        if self.direct {
            match byte {
                b'q' => output.push(ParsedInput::Detach),
                value if value == self.prefix_byte => {
                    output.push(ParsedInput::Input(literal.to_vec()));
                }
                _ => {}
            }
            self.prefix = false;
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
            b'%' => output.push(ParsedInput::Action(Action::Split(Axis::Horizontal))),
            b'"' => output.push(ParsedInput::Action(Action::Split(Axis::Vertical))),
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
            b't' => output.push(ParsedInput::Action(Action::TogglePaneTransparency)),
            b'm' => output.push(ParsedInput::Action(Action::EnterFloatingMoveMode)),
            b'r' => output.push(ParsedInput::Action(Action::EnterFloatingResizeMode)),
            b'a' => output.push(ParsedInput::Action(Action::ToggleAgentNavigator)),
            b'T' => output.push(ParsedInput::Action(Action::CycleTabView)),
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
            _ => {
                if let Some(action) = self.plugin_bindings.get(&byte).cloned() {
                    output.push(ParsedInput::Action(action));
                }
            }
        }
        self.prefix = false;
    }
}

fn is_core_chord(byte: u8) -> bool {
    matches!(
        byte,
        b'%' | b'"'
            | b'c'
            | b'n'
            | b'p'
            | b'h'
            | b'j'
            | b'k'
            | b'l'
            | b'w'
            | b','
            | b'z'
            | b's'
            | b'S'
            | b'f'
            | b'F'
            | b'P'
            | b't'
            | b'm'
            | b'r'
            | b'a'
            | b'T'
            | b'd'
            | b'['
            | b']'
            | b'x'
            | b'1'..=b'9' | 0x1b
    )
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
        "toggle-pane-transparency" => Some(Action::TogglePaneTransparency),
        "enter-floating-move-mode" => Some(Action::EnterFloatingMoveMode),
        "enter-floating-resize-mode" => Some(Action::EnterFloatingResizeMode),
        "agent-navigator" => Some(Action::ToggleAgentNavigator),
        "save-layout" => Some(Action::BeginSaveLayout),
        "cycle-tab-view" => Some(Action::CycleTabView),
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

/// Preserve terminal bytes alongside decoded enhanced-key reports. The actor gives a focused
/// overlay first refusal; a declined report reaches the PTY byte-for-byte.
pub(crate) fn key_input_message(bytes: Vec<u8>) -> crate::ipc::ClientMessage {
    use crate::ipc::{ClientMessage, OverlayKeyInput};
    use vivid_protocol::overlay::modifiers as mods;
    let mut keys = Vec::new();
    let mut start = 0;
    while start + 2 < bytes.len() {
        if bytes[start..].starts_with(b"\x1b[")
            && let Some(length) = bytes[start + 2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
        {
            let end = start + 3 + length;
            if let Some(key) = parse_kitty_key(&bytes[start..end]) {
                let modifiers = if key.modifiers & 1 != 0 {
                    mods::SHIFT
                } else {
                    0
                } | if key.modifiers & 2 != 0 { mods::ALT } else { 0 }
                    | if key.modifiers & 4 != 0 {
                        mods::CONTROL
                    } else {
                        0
                    }
                    | if key.modifiers & 8 != 0 {
                        mods::SUPER
                    } else {
                        0
                    }
                    | if key.modifiers & 64 != 0 {
                        mods::CAPS_LOCK
                    } else {
                        0
                    }
                    | if key.modifiers & 128 != 0 {
                        mods::NUM_LOCK
                    } else {
                        0
                    };
                let down = key.kind != KittyKeyKind::Release;
                let sequence = std::str::from_utf8(&bytes[start + 2..end - 1]).unwrap_or("");
                let associated = sequence.split(';').nth(2).and_then(|value| {
                    value
                        .split(':')
                        .map(|value| value.parse::<u32>().ok().and_then(char::from_u32))
                        .collect::<Option<String>>()
                });
                let text = if down && modifiers & (mods::CONTROL | mods::ALT | mods::SUPER) == 0 {
                    associated.unwrap_or_else(|| {
                        char::from_u32(
                            key.shifted_codepoint
                                .filter(|_| key.modifiers & 1 != 0)
                                .unwrap_or(key.codepoint),
                        )
                        .filter(|character| {
                            !character.is_control()
                                && !(0xe000..=0xf8ff).contains(&(*character as u32))
                        })
                        .map_or_else(String::new, |character| character.to_string())
                    })
                } else {
                    String::new()
                };
                let base = sequence
                    .split(';')
                    .next()
                    .and_then(|value| value.split(':').nth(2))
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(key.codepoint);
                keys.push(OverlayKeyInput {
                    start,
                    end,
                    physical: hid_usage(base),
                    down,
                    repeat: key.kind == KittyKeyKind::Repeat,
                    modifiers,
                    text,
                });
                start = end;
                continue;
            }
        }
        start += 1;
    }
    if keys.is_empty() {
        ClientMessage::Input(bytes)
    } else {
        ClientMessage::KeyInput { bytes, keys }
    }
}

fn hid_usage(key: u32) -> u32 {
    match key {
        97..=122 => key - 97 + 4,
        65..=90 => key - 65 + 4,
        49..=57 => key - 49 + 30,
        48 => 39,
        13 => 40,
        27 => 41,
        127 | 8 => 42,
        9 => 43,
        32 => 44,
        45 => 45,
        61 => 46,
        91 => 47,
        93 => 48,
        92 => 49,
        59 => 51,
        39 => 52,
        96 => 53,
        44 => 54,
        46 => 55,
        47 => 56,
        57364..=57375 => key - 57364 + 58,
        57348 => 73,
        57349 => 76,
        57350 => 80,
        57351 => 79,
        57352 => 82,
        57353 => 81,
        57354 => 75,
        57355 => 78,
        57356 => 74,
        57357 => 77,
        _ => 0,
    }
}

#[cfg(test)]
mod overlay_key_tests {
    use super::*;
    #[test]
    fn kitty_keys_keep_bytes_and_carry_physical_text_repeat_and_release() {
        let bytes = b"before\x1b[97:65:113;2;65u\x1b[9;1:2u\x1b[9;1:3uafter".to_vec();
        let crate::ipc::ClientMessage::KeyInput {
            bytes: original,
            keys,
        } = key_input_message(bytes.clone())
        else {
            panic!("keys missing");
        };
        assert_eq!(original, bytes);
        assert_eq!(keys.len(), 3);
        assert_eq!((keys[0].physical, keys[0].text.as_str()), (20, "A"));
        assert_eq!(keys[0].modifiers, vivid_protocol::overlay::modifiers::SHIFT);
        assert_eq!(&original[keys[1].start..keys[1].end], b"\x1b[9;1:2u");
        assert!(keys[1].repeat && keys[1].down);
        assert!(!keys[2].down);
        assert!(keys[2].text.is_empty());
        assert!(matches!(
            key_input_message(b"ordinary\x1b[broken".to_vec()),
            crate::ipc::ClientMessage::Input(_)
        ));
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
        alt: raw & 8 != 0,
        ctrl: raw & 16 != 0,
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

    /// Ctrl+B then `h`, exactly as Windows Terminal delivered them in win32-input-mode: bare Ctrl
    /// presses, the chord's press and release, Ctrl's release, then `h` down and up.
    const WIN32_PREFIX_H: &[u8] = b"\x1b[17;29;0;1;8;1_\x1b[66;48;2;1;8;1_\x1b[66;48;2;0;8;1_\
\x1b[17;29;0;0;0;1_\x1b[72;35;104;1;0;1_\x1b[72;35;104;0;0;1_";

    #[test]
    fn win32_input_records_become_the_bytes_a_vt_console_sends() {
        let mut decoder = Win32InputDecoder::default();
        assert_eq!(decoder.decode(WIN32_PREFIX_H), b"\x02h");
    }

    #[test]
    fn win32_input_prefix_chords_reach_the_prefix_parser() {
        let mut decoder = Win32InputDecoder::default();
        let mut parser = PrefixParser::new(0x02, &BTreeMap::new());
        assert_eq!(
            parser.feed(&decoder.decode(WIN32_PREFIX_H)),
            vec![ParsedInput::Action(Action::Focus(Direction::Left))]
        );
    }

    #[test]
    fn win32_input_record_split_across_reads_is_reassembled() {
        let mut decoder = Win32InputDecoder::default();
        let (first, second) = WIN32_PREFIX_H.split_at(25);
        let mut decoded = decoder.decode(first);
        decoded.extend(decoder.decode(second));
        assert_eq!(decoded, b"\x02h");
    }

    #[test]
    fn win32_input_special_keys_use_xterm_encodings() {
        let mut decoder = Win32InputDecoder::default();
        // Up, Shift+Right, F1, Delete, Backspace, Shift+Tab, Enter, Escape.
        let input = b"\x1b[38;72;0;1;0;1_\x1b[39;77;0;1;16;1_\x1b[112;59;0;1;0;1_\
\x1b[46;83;0;1;256;1_\x1b[8;14;8;1;0;1_\x1b[9;15;9;1;16;1_\x1b[13;28;13;1;0;1_\
\x1b[27;1;27;1;0;1_";
        assert_eq!(
            decoder.decode(input),
            b"\x1b[A\x1b[1;2C\x1bOP\x1b[3~\x7f\x1b[Z\r\x1b"
        );
    }

    #[test]
    fn win32_input_alt_prefixes_escape_but_altgr_does_not() {
        let mut decoder = Win32InputDecoder::default();
        // Alt+x, then AltGr+Q producing `@` (reported as Right Alt with Left Ctrl).
        let input = b"\x1b[88;45;120;1;2;1_\x1b[81;16;64;1;9;1_";
        assert_eq!(decoder.decode(input), b"\x1bx@");
    }

    #[test]
    fn win32_input_decodes_surrogate_pairs_and_bounds_repeats() {
        let mut decoder = Win32InputDecoder::default();
        let emoji = b"\x1b[0;0;55357;1;0;1_\x1b[0;0;56832;1;0;1_";
        assert_eq!(decoder.decode(emoji), "\u{1f600}".as_bytes());
        let repeated = decoder.decode(b"\x1b[65;30;97;1;0;65535_");
        assert_eq!(repeated, vec![b'a'; usize::from(WIN32_INPUT_MAX_REPEAT)]);
    }

    #[test]
    fn win32_input_leaves_other_input_untouched() {
        let mut decoder = Win32InputDecoder::default();
        let input = b"plain\x1b[A\x1b[<0;10;5M\x1b[1;2_x\x1b";
        assert_eq!(decoder.decode(input), input);
        // A lone ESC is never held here; the prefix parser owns bare-Escape timing.
        assert_eq!(decoder.decode(b"\x1b"), b"\x1b");
    }

    #[test]
    fn parser_preserves_literal_prefix_actions_and_mouse() {
        let mut parser = PrefixParser::default();
        let commands = parser.feed(b"a\x02\x02\x02%z");
        assert!(matches!(&commands[0], ParsedInput::Input(bytes) if bytes == b"a"));
        assert!(matches!(&commands[1], ParsedInput::Input(bytes) if bytes == b"\x02"));
        assert_eq!(
            commands[2],
            ParsedInput::Action(Action::Split(Axis::Horizontal))
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
                    alt: false,
                    ctrl: false,
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
        // `t` rather than `b`: the prefix key's own letter is confusing to press twice, and `b`
        // reads as "back" to the vi-style navigation the other bindings follow.
        assert_eq!(
            parser.feed(b"\x02t\x02P"),
            [
                ParsedInput::Action(Action::TogglePaneTransparency),
                ParsedInput::Action(Action::TogglePanePinned),
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
    fn conpty_repairs_f12_release_only_when_the_pane_requested_key_events() {
        let mut parser = PrefixParser::default();
        parser.set_conpty_input_transport(true);
        parser.set_keyboard_flags(31);
        assert!(parser.feed(b"\x1b[24").is_empty());
        assert_eq!(
            parser.feed(b"~"),
            [ParsedInput::Input(b"\x1b[24~\x1b[24;1:3~".to_vec())],
            "the pane receives the preserved press followed by the missing release"
        );

        let mut byte_transparent = PrefixParser::default();
        byte_transparent.set_keyboard_flags(31);
        assert_eq!(
            byte_transparent.feed(LEGACY_F12_PRESS),
            [ParsedInput::Input(LEGACY_F12_PRESS.to_vec())],
            "a byte-transparent attachment must not synthesize input"
        );

        let mut press_only = PrefixParser::default();
        press_only.set_conpty_input_transport(true);
        press_only.set_keyboard_flags(1);
        assert_eq!(
            press_only.feed(LEGACY_F12_PRESS),
            [ParsedInput::Input(LEGACY_F12_PRESS.to_vec())],
            "a pane that did not request release events keeps press-only input"
        );
    }

    #[test]
    fn kitty_enter_events_are_forwarded_byte_exact() {
        let mut parser = PrefixParser::default();
        parser.set_keyboard_flags(31);
        let events = b"\x1b[13;1u\x1b[13;1:2u\x1b[13;1:3u";
        assert_eq!(
            parser.feed(events),
            [ParsedInput::Input(events.to_vec())],
            "nested applications must receive Enter press, repeat, and release unchanged"
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
    fn a_command_key_pressed_as_text_has_its_release_report_consumed() {
        // With Kitty flags 3 the host escapes only what is ambiguous, so `prefix w` arrives as a
        // plain `w` press followed by an escaped release. The pane never saw that press, and the
        // tab navigator the press opened reads a leading ESC as a cancel, so the release must not
        // be forwarded.
        let mut parser = PrefixParser::default();
        parser.set_keyboard_flags(3);
        assert!(parser.feed(b"\x1b[98;5u").is_empty());
        assert!(parser.feed(b"\x1b[98;1:3u").is_empty());
        assert_eq!(
            parser.feed(b"w"),
            [ParsedInput::Action(Action::ToggleTabNavigator)]
        );
        assert_eq!(parser.feed(b"\x1b[119;1:3u"), []);

        // A shifted command key releases as its unshifted codepoint.
        assert!(parser.feed(b"\x1b[98;5u").is_empty());
        assert_eq!(
            parser.feed(b"S"),
            [ParsedInput::Action(Action::ToggleSyncInput)]
        );
        assert_eq!(parser.feed(b"\x1b[115;2:3u"), []);

        // A release for a key vvmux never consumed still reaches the pane.
        assert_eq!(
            parser.feed(b"\x1b[106;1:3u"),
            [ParsedInput::Input(b"\x1b[106;1:3u".to_vec())]
        );

        // Without event reporting there is no release to wait for, so nothing is recorded.
        parser.set_keyboard_flags(1);
        assert!(parser.feed(b"\x02").is_empty());
        assert_eq!(parser.feed(b"z"), [ParsedInput::Action(Action::ToggleZoom)]);
        assert_eq!(
            parser.feed(b"\x1b[122;1:3u"),
            [ParsedInput::Input(b"\x1b[122;1:3u".to_vec())]
        );
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

    #[test]
    fn direct_attachment_keeps_only_literal_prefix_and_detach() {
        let mut parser = PrefixParser::new_with_mode(0x02, &BTreeMap::new(), true);
        assert_eq!(
            parser.feed(b"ordinary"),
            [ParsedInput::Input(b"ordinary".to_vec())]
        );
        assert!(parser.feed(b"\x02z").is_empty());
        assert_eq!(parser.feed(b"\x02\x02"), [ParsedInput::Input(vec![0x02])]);
        assert_eq!(parser.feed(b"\x02q"), [ParsedInput::Detach]);
    }

    #[test]
    fn plugin_bindings_fill_only_unclaimed_prefix_chords() {
        let configured = BTreeMap::from([("u".into(), "new-tab".into())]);
        let mut parser = PrefixParser::new(0x02, &configured);
        parser.set_plugin_bindings(vec![
            crate::ipc::PluginKeybinding {
                chord: b'u',
                action: "dev.example/open".into(),
            },
            crate::ipc::PluginKeybinding {
                chord: b'z',
                action: "dev.example/open".into(),
            },
            crate::ipc::PluginKeybinding {
                chord: b'v',
                action: "dev.example/open".into(),
            },
        ]);
        assert_eq!(parser.feed(b"\x02u"), [ParsedInput::Action(Action::NewTab)]);
        assert_eq!(
            parser.feed(b"\x02z"),
            [ParsedInput::Action(Action::ToggleZoom)]
        );
        assert_eq!(
            parser.feed(b"\x02v"),
            [ParsedInput::Action(Action::Plugin(
                "plugin:dev.example/open".into()
            ))]
        );
    }
}
