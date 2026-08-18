//! Pane-oriented terminal emulation and platform PTY support for vvmux.

#![cfg_attr(not(unix), allow(dead_code))]

use std::cmp;
use std::collections::{BTreeMap, VecDeque};
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use base64::Engine;
use unicode_width::UnicodeWidthChar;
use vte::ansi::{
    Attr, ClearMode, Color, Handler, Hyperlink, KeyboardModes, KeyboardModesApplyBehavior,
    LineClearMode, NamedColor, PrivateMode, Processor, TabulationClearMode,
};

const KEYBOARD_MODE_STACK_MAX_DEPTH: usize = 4096;
const AGENT_OSC_MAX_CHARS: usize = 256;
const KITTY_PACKET_MAX_BYTES: usize = 8 * 1024;
const KITTY_PAYLOAD_MAX_BYTES: usize = 4096;
const KITTY_DECODED_TRANSFER_MAX_BYTES: u32 = 64 * 1024 * 1024;
/// Largest OSC 52 payload accepted from a pane, matching the session's copy-buffer ceiling.
const CLIPBOARD_DECODED_MAX_BYTES: usize = 1024 * 1024;
const DCS_MAX_BYTES: usize = 4096;

/// OSC 52 producers in the wild routinely omit base64 padding, so a store is decoded with either
/// form accepted rather than rejected outright.
const CLIPBOARD_BASE64: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::general_purpose::PAD_INDIFFERENT,
);

/// Source of synthetic `id=` values for OSC 8 links that arrive without one.
///
/// The counter is process-global rather than per-`Terminal` because the outer terminal composites
/// every pane into a single grid: two panes numbering their own links from zero would hand the
/// presenter colliding identities for unrelated links. `Terminal::new` also takes no pane identity
/// to seed a per-pane counter from.
static HYPERLINK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Build the id for an OSC 8 link that the application left unlabeled.
///
/// Re-emitting a link without `id=` is not idempotent: a presenter that mints its own identity per
/// OSC 8 open (as Vivido does) sees every reopen as a distinct link, and `ansi_diff` reopens a link
/// on each style change and at the start of every partial repaint. Assigning the id once, at
/// ingest, makes every later re-emission carry the same identity so the link stays one link.
///
/// One id per *open*, not per URI: two unlabeled links to the same target are separate links, which
/// matches the presenter's own semantics.
///
/// The process id scopes the counter to this daemon. Without it a restarted daemon would start
/// numbering at zero again while the outer terminal still held cells labeled by its predecessor,
/// so two unrelated links could claim one identity.
fn synthesize_hyperlink_id() -> String {
    format!(
        "vvmux-{:x}-{:x}",
        std::process::id(),
        HYPERLINK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Debug, Default)]
struct AgentOscTracker {
    state: OscState,
    body: Vec<u8>,
    title: Option<String>,
    progress: Option<String>,
}

#[derive(Debug, Default)]
struct DcsScanner {
    state: DcsState,
    body: Vec<u8>,
    raw: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
enum DcsState {
    #[default]
    Ground,
    Escape,
    Body,
    BodyEscape,
    Discarding,
    DiscardingEscape,
}

#[derive(Debug)]
enum DcsChunk {
    Bytes(Vec<u8>),
    Request(DcsRequest),
}

#[derive(Debug)]
enum DcsRequest {
    Decrqss(Vec<u8>),
    Xtgettcap(Vec<u8>),
    Xtsettcap(Vec<u8>),
}

impl DcsScanner {
    fn push(&mut self, bytes: &[u8]) -> Vec<DcsChunk> {
        let mut chunks = Vec::new();
        for &byte in bytes {
            match self.state {
                DcsState::Ground if byte == 0x1b => self.state = DcsState::Escape,
                DcsState::Ground => push_dcs_bytes(&mut chunks, &[byte]),
                DcsState::Escape if byte == b'P' => {
                    self.body.clear();
                    self.raw.clear();
                    self.raw.extend_from_slice(b"\x1bP");
                    self.state = DcsState::Body;
                }
                DcsState::Escape if byte == 0x1b => {
                    push_dcs_bytes(&mut chunks, b"\x1b");
                }
                DcsState::Escape => {
                    push_dcs_bytes(&mut chunks, &[0x1b, byte]);
                    self.state = DcsState::Ground;
                }
                DcsState::Body if byte == 0x1b => {
                    self.state = DcsState::BodyEscape;
                }
                DcsState::Body => self.push_body(byte),
                DcsState::BodyEscape if byte == b'\\' => {
                    self.raw.extend_from_slice(b"\x1b\\");
                    self.finish(&mut chunks);
                }
                DcsState::BodyEscape => {
                    self.push_body(0x1b);
                    if matches!(self.state, DcsState::Body) {
                        self.push_body(byte);
                    }
                }
                DcsState::Discarding if byte == 0x1b => {
                    self.state = DcsState::DiscardingEscape;
                }
                DcsState::Discarding => {}
                DcsState::DiscardingEscape if byte == b'\\' => {
                    self.body.clear();
                    self.raw.clear();
                    self.state = DcsState::Ground;
                }
                DcsState::DiscardingEscape if byte == 0x1b => {}
                DcsState::DiscardingEscape => self.state = DcsState::Discarding,
            }
        }
        chunks
    }

    fn push_body(&mut self, byte: u8) {
        self.body.push(byte);
        self.raw.push(byte);
        self.state = if self.body.len() > DCS_MAX_BYTES {
            self.body.clear();
            self.raw.clear();
            DcsState::Discarding
        } else {
            DcsState::Body
        };
    }

    fn finish(&mut self, chunks: &mut Vec<DcsChunk>) {
        let request = self
            .body
            .strip_prefix(b"$q")
            .map(|body| DcsRequest::Decrqss(body.to_vec()))
            .or_else(|| {
                self.body
                    .strip_prefix(b"+q")
                    .map(|body| DcsRequest::Xtgettcap(body.to_vec()))
            })
            .or_else(|| {
                self.body
                    .strip_prefix(b"+p")
                    .map(|body| DcsRequest::Xtsettcap(body.to_vec()))
            });
        chunks.push(match request {
            Some(request) => DcsChunk::Request(request),
            None => DcsChunk::Bytes(mem::take(&mut self.raw)),
        });
        self.body.clear();
        self.raw.clear();
        self.state = DcsState::Ground;
    }
}

fn push_dcs_bytes(chunks: &mut Vec<DcsChunk>, bytes: &[u8]) {
    if let Some(DcsChunk::Bytes(previous)) = chunks.last_mut() {
        previous.extend_from_slice(bytes);
    } else {
        chunks.push(DcsChunk::Bytes(bytes.to_vec()));
    }
}

#[derive(Debug, Clone, Copy, Default)]
enum OscState {
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
    const MAX_BODY_BYTES: usize = 4096;

    fn observe(&mut self, bytes: &[u8]) {
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

    fn push(&mut self, byte: u8) {
        self.body.push(byte);
        if self.body.len() > Self::MAX_BODY_BYTES {
            self.body.clear();
            self.state = OscState::Discarding;
        } else {
            self.state = OscState::Body;
        }
    }

    fn finish(&mut self) {
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
            _ => {}
        }
        self.body.clear();
        self.state = OscState::Ground;
    }
}

fn sanitize_agent_osc(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload)
        .chars()
        .filter(|character| !character.is_control())
        .take(AGENT_OSC_MAX_CHARS)
        .collect()
}

pub mod pty;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TerminalColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cell {
    pub ch: char,
    pub combining: String,
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub underline_style: UnderlineStyle,
    pub underline_color: Option<TerminalColor>,
    pub blink: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikeout: bool,
    pub wide_continuation: bool,
    pub leading_wide_spacer: bool,
    /// Number of physical cells traversed by a literal horizontal tab starting here.
    pub tab_width: Option<u16>,
    pub hyperlink: Option<TerminalHyperlink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalHyperlink {
    pub id: Option<String>,
    pub uri: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curl,
    Dotted,
    Dashed,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            combining: String::new(),
            foreground: TerminalColor::Default,
            background: TerminalColor::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            underline_style: UnderlineStyle::None,
            underline_color: None,
            blink: false,
            inverse: false,
            hidden: false,
            strikeout: false,
            wide_continuation: false,
            leading_wide_spacer: false,
            tab_width: None,
            hyperlink: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TerminalModes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_clicks: bool,
    pub mouse_motion: bool,
    pub sgr_mouse: bool,
    pub sgr_pixels: bool,
    pub focus_reporting: bool,
    pub cursor_visible: bool,
    pub application_keypad: bool,
    pub keyboard_flags: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    Damage,
    Title(Option<String>),
    Bell,
    PtyWrite(Vec<u8>),
    ModeChange(TerminalModes),
    VividMarker {
        marker: String,
        row: usize,
        column: usize,
        alternate: bool,
    },
    KittyGraphics(KittyGraphicsCommand),
    /// The grid scrolled. `lines` is signed (up positive, down negative); `top`/`bottom` bound the
    /// scrolled region. `pushed_to_history` is set only when a full-screen primary-screen scroll
    /// carried lines into scrollback — the only scroll that keeps a viewport-anchored cell on the
    /// same text, which is what selection rotation needs to know.
    GridScroll {
        lines: i32,
        top: usize,
        bottom: usize,
        pushed_to_history: bool,
        alternate: bool,
    },
    Clear {
        alternate: bool,
    },
    ScreenSwap {
        alternate: bool,
    },
    /// A pane asked to write the user's clipboard (OSC 52 store). Already decoded and bounded.
    ClipboardStore {
        selection: u8,
        text: Vec<u8>,
    },
    /// A pane asked to read the user's clipboard (OSC 52 query). The pane's own terminator is
    /// carried back so the reply is framed the way the pane framed its request.
    ClipboardLoad {
        selection: u8,
        terminator: String,
    },
}

/// A validated Kitty graphics command consumed from pane output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyGraphicsCommand {
    /// The standard direct-transmission support query. The session answers on the pane PTY.
    Query { image_id: u32 },
    /// A packet safe to forward to a directly attached Kitty-compatible host.
    Packet {
        bytes: Vec<u8>,
        starts_transfer: bool,
        more: bool,
    },
}

/// DECSC/DECRC state. Origin mode belongs to the saved cursor, so an application that saves while
/// addressing is margin-relative restores into the same coordinate space.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SavedCursor {
    row: usize,
    col: usize,
    origin_mode: bool,
}

pub struct Terminal {
    rows: usize,
    cols: usize,
    grid: Vec<Vec<Cell>>,
    alternate_grid: Vec<Vec<Cell>>,
    grid_wrapped: Vec<bool>,
    alternate_grid_wrapped: Vec<bool>,
    history: VecDeque<Vec<Cell>>,
    history_wrapped: VecDeque<bool>,
    history_limit: usize,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: SavedCursor,
    /// The DECSC slot belonging to whichever screen is not currently active. xterm keeps one save
    /// slot per screen, which is what stops a full-screen application's own DECSC from clobbering
    /// the primary-screen cursor that DECSET 1049 captured.
    inactive_saved_cursor: SavedCursor,
    scroll_top: usize,
    scroll_bottom: usize,
    origin_mode: bool,
    template: Cell,
    processor: Processor,
    events: Vec<TerminalEvent>,
    modes: TerminalModes,
    title: Option<String>,
    alternate_screen: bool,
    tab_stops: Vec<bool>,
    tab_stops_customized: bool,
    marker_scanner: VividMarkerScanner,
    kitty_scanner: KittyGraphicsScanner,
    dcs_scanner: DcsScanner,
    agent_osc: AgentOscTracker,
    keyboard_mode_stack: Vec<KeyboardModes>,
    inactive_keyboard_mode_stack: Vec<KeyboardModes>,
}

impl Terminal {
    pub fn new(rows: usize, cols: usize, history_limit: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows,
            cols,
            grid: blank_grid(rows, cols),
            alternate_grid: blank_grid(rows, cols),
            grid_wrapped: vec![false; rows],
            alternate_grid_wrapped: vec![false; rows],
            history: VecDeque::new(),
            history_wrapped: VecDeque::new(),
            history_limit,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: SavedCursor::default(),
            inactive_saved_cursor: SavedCursor::default(),
            scroll_top: 0,
            scroll_bottom: rows,
            origin_mode: false,
            template: Cell::default(),
            processor: Processor::new(),
            events: Vec::new(),
            modes: TerminalModes {
                cursor_visible: true,
                ..TerminalModes::default()
            },
            title: None,
            alternate_screen: false,
            tab_stops: default_tab_stops(cols, 8),
            tab_stops_customized: false,
            marker_scanner: VividMarkerScanner::default(),
            kitty_scanner: KittyGraphicsScanner::default(),
            dcs_scanner: DcsScanner::default(),
            agent_osc: AgentOscTracker::default(),
            keyboard_mode_stack: Vec::new(),
            inactive_keyboard_mode_stack: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TerminalEvent> {
        self.agent_osc.observe(bytes);
        let chunks = self.kitty_scanner.push(bytes);
        self.process_kitty_chunks(chunks, !bytes.is_empty(), false)
    }

    /// Latest bounded OSC 0/2 title retained for passive agent detection.
    pub fn agent_osc_title(&self) -> &str {
        self.agent_osc.title.as_deref().unwrap_or("")
    }

    /// Latest bounded OSC 9 payload retained for passive agent detection.
    pub fn agent_osc_progress(&self) -> &str {
        self.agent_osc.progress.as_deref().unwrap_or("")
    }

    /// Prevent a replacement foreground process from inheriting stale OSC evidence.
    pub fn clear_agent_osc(&mut self) {
        self.agent_osc.title = None;
        self.agent_osc.progress = None;
    }

    /// Flush bytes held only because they could have been a fragmented marker.
    pub fn finish(&mut self) -> Vec<TerminalEvent> {
        let chunks = self.kitty_scanner.finish();
        let has_bytes = chunks
            .iter()
            .any(|chunk| matches!(chunk, KittyChunk::Bytes(bytes) if !bytes.is_empty()));
        self.process_kitty_chunks(chunks, has_bytes, true)
    }

    /// When a buffered synchronized update (DECSET 2026) has to be applied even though the ESU
    /// that would close it never arrived.
    ///
    /// vte arms this deadline on BSU but never tests it: `StdSyncHandler::pending_timeout` only
    /// reports that a deadline exists. Unless the owner drives it, a pane that opens a synchronized
    /// update and then stalls keeps buffering until it hits vte's 2 MiB ceiling, so the pane
    /// appears frozen.
    pub fn sync_flush_deadline(&self) -> Option<Instant> {
        self.processor.sync_timeout().sync_timeout()
    }

    /// Apply a synchronized update that outlived its deadline.
    ///
    /// Never call this from inside a `Handler` callback: `self.processor` is a default placeholder
    /// for the duration of `advance`, so a re-entrant call would silently no-op and strand the
    /// buffered bytes.
    pub fn flush_synchronized_update(&mut self) -> Vec<TerminalEvent> {
        let mut processor = mem::take(&mut self.processor);
        processor.stop_sync(self);
        self.processor = processor;
        self.finish_events(true)
    }

    fn process_kitty_chunks(
        &mut self,
        chunks: Vec<KittyChunk>,
        damage: bool,
        finishing: bool,
    ) -> Vec<TerminalEvent> {
        for chunk in chunks {
            match chunk {
                KittyChunk::Bytes(bytes) => {
                    let dcs = self.dcs_scanner.push(&bytes);
                    self.process_dcs_chunks(dcs);
                }
                KittyChunk::Command(command) => {
                    self.events.push(TerminalEvent::KittyGraphics(command));
                }
            }
        }
        if finishing {
            let vivid = self.marker_scanner.finish();
            self.process_vivid_chunks(vivid);
        }
        self.finish_events(damage)
    }

    fn process_dcs_chunks(&mut self, chunks: Vec<DcsChunk>) {
        for chunk in chunks {
            match chunk {
                DcsChunk::Bytes(bytes) => {
                    let vivid = self.marker_scanner.push(&bytes);
                    self.process_vivid_chunks(vivid);
                }
                DcsChunk::Request(request) => self.handle_dcs_request(request),
            }
        }
    }

    fn handle_dcs_request(&mut self, request: DcsRequest) {
        match request {
            DcsRequest::Decrqss(request) => self.report_status_string(&request),
            DcsRequest::Xtgettcap(request) => self.report_termcap(&request, "vvmux"),
            DcsRequest::Xtsettcap(request) => {
                // Validate the bounded hex input, then deliberately ignore it. Terminal identity
                // and capabilities are emulator-owned state and cannot be changed by a pane.
                let _ = request
                    .split(|byte| *byte == b';')
                    .all(|name| !name.is_empty() && hex::decode(name).is_ok());
            }
        }
    }

    fn report_status_string(&mut self, request: &[u8]) {
        let status = match request {
            b"m" => sgr_status(&self.template),
            b"r" => format!("{};{}r", self.scroll_top + 1, self.scroll_bottom),
            b"\"p" => "62;1\"p".to_owned(),
            // Select Character Protection Attribute is not otherwise tracked, so characters are
            // always reported as erasable (the DEC default).
            b"\"q" => "0\"q".to_owned(),
            _ => {
                self.events
                    .push(TerminalEvent::PtyWrite(b"\x1bP0$r\x1b\\".to_vec()));
                return;
            }
        };
        self.events.push(TerminalEvent::PtyWrite(
            format!("\x1bP1$r{status}\x1b\\").into_bytes(),
        ));
    }

    fn report_termcap(&mut self, request: &[u8], terminal_name: &str) {
        let mut values = Vec::new();
        for encoded_name in request.split(|byte| *byte == b';') {
            let Ok(name) = hex::decode(encoded_name) else {
                continue;
            };
            let value = match name.as_slice() {
                b"TN" => Some(Some(terminal_name.as_bytes())),
                b"Co" => Some(Some(b"256".as_slice())),
                b"RGB" => Some(Some(b"8".as_slice())),
                b"Tc" => Some(None),
                _ => None,
            };
            let Some(value) = value else { continue };
            let mut entry = encoded_name.to_vec();
            if let Some(value) = value {
                entry.push(b'=');
                entry.extend_from_slice(hex::encode(value).as_bytes());
            }
            values.push(entry);
        }
        let reply = if values.is_empty() {
            b"\x1bP0+r\x1b\\".to_vec()
        } else {
            let mut reply = b"\x1bP1+r".to_vec();
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    reply.push(b';');
                }
                reply.extend_from_slice(value);
            }
            reply.extend_from_slice(b"\x1b\\");
            reply
        };
        self.events.push(TerminalEvent::PtyWrite(reply));
    }

    fn process_vivid_chunks(&mut self, chunks: Vec<VividChunk>) {
        let mut processor = mem::take(&mut self.processor);
        for chunk in chunks {
            match chunk {
                VividChunk::Bytes(bytes) => processor.advance(self, &bytes),
                VividChunk::Marker(marker) => {
                    // The cursor has processed exactly the bytes preceding the marker, so it
                    // sits on the cell where the marker glyphs began. Capture it now: ConPTY
                    // batches the marker with repositioning output, so by the time the event
                    // is handled the live cursor has already moved elsewhere.
                    self.events.push(TerminalEvent::VividMarker {
                        marker,
                        row: self.cursor_row,
                        column: self.cursor_col,
                        alternate: self.alternate_screen,
                    });
                }
            }
        }
        self.processor = processor;
    }

    fn finish_events(&mut self, damage: bool) -> Vec<TerminalEvent> {
        if damage
            && !self
                .events
                .iter()
                .any(|event| matches!(event, TerminalEvent::Damage))
        {
            self.events.push(TerminalEvent::Damage);
        }
        mem::take(&mut self.events)
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        // Capture the region shape against the old height, before `self.rows` moves.
        let region_was_full_screen = self.scroll_top == 0 && self.scroll_bottom == self.rows;
        resize_grid(&mut self.grid, rows, cols);
        resize_grid(&mut self.alternate_grid, rows, cols);
        self.grid_wrapped.resize(rows, false);
        self.alternate_grid_wrapped.resize(rows, false);
        if self.tab_stops_customized {
            self.tab_stops.resize(cols, false);
        } else {
            self.tab_stops = default_tab_stops(cols, 8);
        }
        self.rows = rows;
        self.cols = cols;
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
        // A region an application deliberately set outlives the resize, clamped to the new height.
        // A region that already spanned the screen was not that kind of choice, so it regrows
        // instead of being pinned to the old height. Saved cursors need no clamp here: DECRC
        // clamps on restore and nothing else reads the slots.
        if region_was_full_screen {
            self.scroll_top = 0;
            self.scroll_bottom = rows;
        } else {
            self.scroll_top = self.scroll_top.min(rows - 1);
            self.scroll_bottom = self.scroll_bottom.clamp(self.scroll_top + 1, rows);
        }
        self.events.push(TerminalEvent::Damage);
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn cells(&self) -> &[Vec<Cell>] {
        &self.grid
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    pub fn modes(&self) -> TerminalModes {
        self.modes
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    pub fn viewport_line(&self, line: isize) -> Option<&[Cell]> {
        if line < 0 {
            let index = self.history.len() as isize + line;
            (index >= 0)
                .then(|| self.history.get(index as usize).map(Vec::as_slice))
                .flatten()
        } else {
            self.grid.get(line as usize).map(Vec::as_slice)
        }
    }

    pub fn line_wrapped(&self, line: isize) -> Option<bool> {
        if line < 0 {
            let index = self.history.len() as isize + line;
            (index >= 0)
                .then(|| self.history_wrapped.get(index as usize).copied())
                .flatten()
        } else {
            self.grid_wrapped.get(line as usize).copied()
        }
    }

    /// The most recent `rows` lines of scrollback, oldest first, with each line's wrap flag.
    ///
    /// Scrollback only — never the live grid. What is on screen belongs to whatever is running in
    /// the pane, and a restored session has a different program in it.
    pub fn history_tail(&self, rows: usize) -> Vec<(&[Cell], bool)> {
        let start = self.history.len().saturating_sub(rows);
        self.history
            .iter()
            .skip(start)
            .zip(self.history_wrapped.iter().skip(start))
            .map(|(cells, wrapped)| (cells.as_slice(), *wrapped))
            .collect()
    }

    /// Seed scrollback with lines from a previous session, oldest first.
    ///
    /// Deliberately not `feed`: these bytes came from a file, and the parser turns bytes into
    /// events — clipboard writes, device replies, media anchors, graphics. Restoring what a pane
    /// looked like must not re-run what it *did*, so the cells are placed directly and nothing is
    /// interpreted. Returns no events for the same reason.
    ///
    /// Replaces rather than appends: this runs on a pane that has just been created, and a pane with
    /// scrollback of its own is not one being restored.
    pub fn restore_history(&mut self, rows: Vec<(Vec<Cell>, bool)>) {
        self.history.clear();
        self.history_wrapped.clear();
        for (cells, wrapped) in rows.into_iter().rev().take(self.history_limit).rev() {
            self.history.push_back(cells);
            self.history_wrapped.push_back(wrapped);
        }
    }

    pub fn alternate_screen(&self) -> bool {
        self.alternate_screen
    }

    pub fn visible_text(&self, display_offset: usize) -> String {
        let rows = self.rows;
        let start = -(display_offset.min(self.history.len()) as isize);
        self.extract_rows(start, rows)
    }

    /// The last `rows` rows, with soft-wrapped rows joined into one logical line.
    ///
    /// This is the log-friendly read: a command's output comes back as the lines it wrote, not as
    /// the lines the terminal happened to break them into at the current width.
    pub fn latest_text(&self, rows: usize) -> String {
        self.latest_text_with(rows, true)
    }

    /// The last `rows` rows, one line per physical terminal row.
    ///
    /// Preserves the wrap points a reader sees on screen, which is what a caller wants when
    /// reasoning about layout rather than about content.
    pub fn latest_text_physical(&self, rows: usize) -> String {
        self.latest_text_with(rows, false)
    }

    fn latest_text_with(&self, rows: usize, join_wrapped: bool) -> String {
        let available = self.history.len().saturating_add(self.rows);
        let count = rows.min(available);
        let start_absolute = available.saturating_sub(count);
        let start = start_absolute as isize - self.history.len() as isize;
        self.extract_rows_with(start, count, join_wrapped)
    }

    pub fn extract_rows(&self, start_line: isize, row_count: usize) -> String {
        self.extract_rows_with(start_line, row_count, true)
    }

    pub fn extract_rows_physical(&self, start_line: isize, row_count: usize) -> String {
        self.extract_rows_with(start_line, row_count, false)
    }

    fn extract_rows_with(&self, start_line: isize, row_count: usize, join_wrapped: bool) -> String {
        let mut text = String::new();
        let mut previous_wrapped = false;
        for index in 0..row_count {
            let line = start_line.saturating_add(index as isize);
            let Some(cells) = self.viewport_line(line) else {
                continue;
            };
            if index > 0 && !(join_wrapped && previous_wrapped) {
                text.push('\n');
            }
            append_row_text(&mut text, &cells[..cells.len().min(self.cols)]);
            previous_wrapped = self.line_wrapped(line).unwrap_or(false);
        }
        text
    }

    fn damage(&mut self) {
        if !self
            .events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Damage))
        {
            self.events.push(TerminalEvent::Damage);
        }
    }

    fn blank(&self) -> Cell {
        Cell {
            ch: ' ',
            combining: String::new(),
            wide_continuation: false,
            leading_wide_spacer: false,
            tab_width: None,
            ..self.template.clone()
        }
    }

    /// Exchange the active and inactive screens. Grids, wrap flags, keyboard-mode stacks and DECSC
    /// slots all belong to a screen, so they move together.
    fn swap_screens(&mut self) {
        mem::swap(&mut self.grid, &mut self.alternate_grid);
        mem::swap(&mut self.grid_wrapped, &mut self.alternate_grid_wrapped);
        mem::swap(
            &mut self.keyboard_mode_stack,
            &mut self.inactive_keyboard_mode_stack,
        );
        mem::swap(&mut self.saved_cursor, &mut self.inactive_saved_cursor);
        self.modes.keyboard_flags = self
            .keyboard_mode_stack
            .last()
            .copied()
            .unwrap_or(KeyboardModes::NO_MODE)
            .bits();
        self.alternate_screen = !self.alternate_screen;
        self.events.push(TerminalEvent::ScreenSwap {
            alternate: self.alternate_screen,
        });
        self.damage();
    }

    /// Blank the active screen without announcing `TerminalEvent::Clear`. A screen switch changes
    /// which anchors are visible but does not constitute an explicit clear of either anchor set.
    fn clear_active_grid(&mut self) {
        self.grid = blank_grid(self.rows, self.cols);
        self.grid_wrapped.fill(false);
        self.damage();
    }

    /// IL and DL are defined only when the cursor sits inside the scroll region. Outside it they
    /// must not touch the grid: `insert_blank_lines`/`delete_lines` widen the region to reach the
    /// cursor, so a cursor above the region would restore the full-screen shape that
    /// `scroll_region_up` treats as permission to feed scrollback.
    fn cursor_inside_scroll_region(&self) -> bool {
        self.cursor_row >= self.scroll_top && self.cursor_row < self.scroll_bottom
    }

    fn scroll_region_up(&mut self, count: usize) {
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top));
        // Only a full-screen primary-screen scroll carries lines into scrollback.
        let pushed_to_history =
            self.scroll_top == 0 && self.scroll_bottom == self.rows && !self.alternate_screen;
        for _ in 0..count {
            let removed = self.grid.remove(self.scroll_top);
            let removed_wrapped = self.grid_wrapped.remove(self.scroll_top);
            if pushed_to_history {
                self.history.push_back(removed);
                self.history_wrapped.push_back(removed_wrapped);
                while self.history.len() > self.history_limit {
                    self.history.pop_front();
                    self.history_wrapped.pop_front();
                }
            }
            self.grid
                .insert(self.scroll_bottom - 1, vec![self.blank(); self.cols]);
            self.grid_wrapped.insert(self.scroll_bottom - 1, false);
        }
        if count > 0 {
            self.events.push(TerminalEvent::GridScroll {
                lines: count as i32,
                top: self.scroll_top,
                bottom: self.scroll_bottom,
                pushed_to_history,
                alternate: self.alternate_screen,
            });
            self.damage();
        }
    }

    fn scroll_region_down(&mut self, count: usize) {
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top));
        for _ in 0..count {
            self.grid.remove(self.scroll_bottom - 1);
            self.grid_wrapped.remove(self.scroll_bottom - 1);
            self.grid
                .insert(self.scroll_top, vec![self.blank(); self.cols]);
            self.grid_wrapped.insert(self.scroll_top, false);
        }
        if count > 0 {
            self.events.push(TerminalEvent::GridScroll {
                lines: -(count as i32),
                top: self.scroll_top,
                bottom: self.scroll_bottom,
                pushed_to_history: false,
                alternate: self.alternate_screen,
            });
            self.damage();
        }
    }

    fn line_feed(&mut self) {
        if self.cursor_row + 1 >= self.scroll_bottom {
            self.scroll_region_up(1);
        } else {
            self.cursor_row = (self.cursor_row + 1).min(self.rows - 1);
        }
    }

    fn clear_row_range(&mut self, row: usize, start: usize, end: usize) {
        let start = start.min(self.cols);
        let end = end.min(self.cols);
        for column in 0..self.cols {
            if let Some(width) = self.grid[row][column].tab_width {
                let tab_end = column.saturating_add(usize::from(width));
                if column < end && tab_end > start {
                    self.grid[row][column].tab_width = None;
                }
            }
        }
        let blank = self.blank();
        for cell in &mut self.grid[row][start..end] {
            *cell = blank.clone();
        }
        self.damage();
    }

    fn invalidate_tab_at(&mut self, row: usize, column: usize) {
        for start in 0..self.cols {
            if let Some(width) = self.grid[row][start].tab_width
                && start <= column
                && start.saturating_add(usize::from(width)) > column
            {
                self.grid[row][start].tab_width = None;
            }
        }
    }
}

impl Handler for Terminal {
    fn input(&mut self, c: char) {
        let width = c.width().unwrap_or(1);
        if width == 0 {
            let col = self.cursor_col.saturating_sub(1);
            self.grid[self.cursor_row][col].combining.push(c);
            self.damage();
            return;
        }
        if self.cursor_col >= self.cols {
            self.grid_wrapped[self.cursor_row] = true;
            self.cursor_col = 0;
            self.line_feed();
        }
        if width == 2 && self.cursor_col + 1 >= self.cols {
            self.invalidate_tab_at(self.cursor_row, self.cursor_col);
            let mut spacer = self.blank();
            spacer.leading_wide_spacer = true;
            self.grid[self.cursor_row][self.cursor_col] = spacer;
            self.grid_wrapped[self.cursor_row] = true;
            self.cursor_col = 0;
            self.line_feed();
        }
        self.invalidate_tab_at(self.cursor_row, self.cursor_col);
        if width == 2 {
            self.invalidate_tab_at(self.cursor_row, self.cursor_col + 1);
        }
        let mut cell = self.template.clone();
        cell.ch = c;
        cell.combining.clear();
        cell.wide_continuation = false;
        cell.leading_wide_spacer = false;
        self.grid[self.cursor_row][self.cursor_col] = cell;
        self.cursor_col += 1;
        if width == 2 && self.cursor_col < self.cols {
            let mut spacer = self.blank();
            spacer.wide_continuation = true;
            self.grid[self.cursor_row][self.cursor_col] = spacer;
            self.cursor_col += 1;
        }
        self.damage();
    }

    fn goto(&mut self, line: i32, col: usize) {
        // Under DECOM row addressing is relative to the scroll region and confined to it. With
        // origin mode off, absolute addressing may legally leave a region that is set.
        let (origin, last_row) = if self.origin_mode {
            (self.scroll_top, self.scroll_bottom.saturating_sub(1))
        } else {
            (0, self.rows - 1)
        };
        self.cursor_row = (line.max(0) as usize).saturating_add(origin).min(last_row);
        self.cursor_col = col.min(self.cols - 1);
    }

    fn goto_line(&mut self, line: i32) {
        self.goto(line, self.cursor_col);
    }

    fn goto_col(&mut self, col: usize) {
        self.goto(self.cursor_row as i32, col);
    }

    fn move_up(&mut self, rows: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(rows).max(self.scroll_top);
    }

    fn move_down(&mut self, rows: usize) {
        self.cursor_row = (self.cursor_row + rows).min(self.scroll_bottom.saturating_sub(1));
    }

    fn move_forward(&mut self, cols: usize) {
        self.cursor_col = (self.cursor_col + cols).min(self.cols - 1);
    }

    fn move_backward(&mut self, cols: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(cols);
    }

    fn move_down_and_cr(&mut self, rows: usize) {
        self.move_down(rows);
        self.cursor_col = 0;
    }

    fn move_up_and_cr(&mut self, rows: usize) {
        self.move_up(rows);
        self.cursor_col = 0;
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    fn linefeed(&mut self) {
        self.line_feed();
    }

    fn newline(&mut self) {
        self.line_feed();
        self.cursor_col = 0;
    }

    fn backspace(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    fn put_tab(&mut self, count: u16) {
        for _ in 0..count {
            let start = self.cursor_col;
            let next = (start + 1..self.cols)
                .find(|column| self.tab_stops.get(*column).copied().unwrap_or(false))
                .unwrap_or(self.cols - 1);
            if next > start {
                self.grid[self.cursor_row][start].tab_width = u16::try_from(next - start).ok();
            }
            self.cursor_col = next;
        }
    }

    fn set_horizontal_tabstop(&mut self) {
        self.tab_stops_customized = true;
        if let Some(stop) = self.tab_stops.get_mut(self.cursor_col) {
            *stop = true;
        }
    }

    fn clear_tabs(&mut self, mode: TabulationClearMode) {
        self.tab_stops_customized = true;
        match mode {
            TabulationClearMode::Current => {
                if let Some(stop) = self.tab_stops.get_mut(self.cursor_col) {
                    *stop = false;
                }
            }
            TabulationClearMode::All => self.tab_stops.fill(false),
        }
    }

    fn set_tabs(&mut self, interval: u16) {
        self.tab_stops_customized = true;
        self.tab_stops = default_tab_stops(self.cols, usize::from(interval.max(1)));
    }

    fn insert_blank(&mut self, count: usize) {
        let count = count.min(self.cols - self.cursor_col);
        let blank = self.blank();
        let row = &mut self.grid[self.cursor_row];
        for cell in row.iter_mut() {
            cell.tab_width = None;
        }
        for column in (self.cursor_col..self.cols - count).rev() {
            row[column + count] = row[column].clone();
        }
        row[self.cursor_col..self.cursor_col + count].fill(blank);
        self.damage();
    }

    fn delete_chars(&mut self, count: usize) {
        let count = count.min(self.cols - self.cursor_col);
        let blank = self.blank();
        let row = &mut self.grid[self.cursor_row];
        for cell in row.iter_mut() {
            cell.tab_width = None;
        }
        for column in self.cursor_col..self.cols - count {
            row[column] = row[column + count].clone();
        }
        row[self.cols - count..].fill(blank);
        self.damage();
    }

    fn erase_chars(&mut self, count: usize) {
        self.clear_row_range(
            self.cursor_row,
            self.cursor_col,
            (self.cursor_col + count).min(self.cols),
        );
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        match mode {
            LineClearMode::Right => {
                self.clear_row_range(self.cursor_row, self.cursor_col, self.cols)
            }
            LineClearMode::Left => self.clear_row_range(self.cursor_row, 0, self.cursor_col + 1),
            LineClearMode::All => self.clear_row_range(self.cursor_row, 0, self.cols),
        }
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        match mode {
            ClearMode::Below => {
                self.clear_row_range(self.cursor_row, self.cursor_col, self.cols);
                for row in self.cursor_row + 1..self.rows {
                    self.clear_row_range(row, 0, self.cols);
                }
            }
            ClearMode::Above => {
                for row in 0..self.cursor_row {
                    self.clear_row_range(row, 0, self.cols);
                }
                self.clear_row_range(self.cursor_row, 0, self.cursor_col + 1);
            }
            ClearMode::All => {
                self.grid = blank_grid(self.rows, self.cols);
                self.grid_wrapped.fill(false);
                self.events.push(TerminalEvent::Clear {
                    alternate: self.alternate_screen,
                });
                self.damage();
            }
            ClearMode::Saved => {
                self.history.clear();
                self.history_wrapped.clear();
                // ConPTY renders `cls` as erase-line on every viewport row followed by ED 3,
                // never ED 2. When the scrollback purge arrives with the viewport already
                // blank, the terminal was fully cleared and media anchors must not survive.
                if self
                    .grid
                    .iter()
                    .flatten()
                    .all(|cell| cell.ch == ' ' && cell.combining.is_empty())
                {
                    self.events.push(TerminalEvent::Clear {
                        alternate: self.alternate_screen,
                    });
                }
            }
        }
    }

    fn scroll_up(&mut self, rows: usize) {
        self.scroll_region_up(rows);
    }

    fn scroll_down(&mut self, rows: usize) {
        self.scroll_region_down(rows);
    }

    fn insert_blank_lines(&mut self, rows: usize) {
        if !self.cursor_inside_scroll_region() {
            return;
        }
        let old_top = self.scroll_top;
        self.scroll_top = self.cursor_row;
        self.scroll_region_down(rows);
        self.scroll_top = old_top;
    }

    fn delete_lines(&mut self, rows: usize) {
        if !self.cursor_inside_scroll_region() {
            return;
        }
        let old_top = self.scroll_top;
        self.scroll_top = self.cursor_row;
        self.scroll_region_up(rows);
        self.scroll_top = old_top;
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        self.scroll_top = top.saturating_sub(1).min(self.rows - 1);
        self.scroll_bottom = bottom
            .unwrap_or(self.rows)
            .clamp(self.scroll_top + 1, self.rows);
        self.goto(0, 0);
    }

    fn save_cursor_position(&mut self) {
        self.saved_cursor = SavedCursor {
            row: self.cursor_row,
            col: self.cursor_col,
            origin_mode: self.origin_mode,
        };
    }

    fn restore_cursor_position(&mut self) {
        // The saved position is absolute, so it is restored without the origin offset. Origin mode
        // itself is part of the saved state and comes back with it.
        self.origin_mode = self.saved_cursor.origin_mode;
        self.cursor_row = self.saved_cursor.row.min(self.rows - 1);
        self.cursor_col = self.saved_cursor.col.min(self.cols - 1);
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == self.scroll_top {
            self.scroll_region_down(1);
        } else {
            self.cursor_row = self.cursor_row.saturating_sub(1);
        }
    }

    fn reset_state(&mut self) {
        self.grid = blank_grid(self.rows, self.cols);
        self.alternate_grid = blank_grid(self.rows, self.cols);
        self.grid_wrapped.fill(false);
        self.alternate_grid_wrapped.fill(false);
        self.history.clear();
        self.history_wrapped.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.saved_cursor = SavedCursor::default();
        self.inactive_saved_cursor = SavedCursor::default();
        // RIS returns to the primary screen. Leaving the flag set would outlive the reset and
        // permanently suppress scrollback capture, which `scroll_region_up` gates on it.
        self.alternate_screen = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows;
        self.origin_mode = false;
        self.template = Cell::default();
        self.tab_stops = default_tab_stops(self.cols, 8);
        self.tab_stops_customized = false;
        self.modes = TerminalModes {
            cursor_visible: true,
            ..TerminalModes::default()
        };
        self.keyboard_mode_stack.clear();
        self.inactive_keyboard_mode_stack.clear();
        self.events.push(TerminalEvent::Clear { alternate: false });
        self.damage();
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        match attr {
            Attr::Reset => {
                let hyperlink = self.template.hyperlink.take();
                self.template = Cell::default();
                self.template.hyperlink = hyperlink;
            }
            Attr::Bold => self.template.bold = true,
            Attr::Dim => self.template.dim = true,
            Attr::CancelBold => self.template.bold = false,
            Attr::CancelBoldDim => {
                self.template.bold = false;
                self.template.dim = false;
            }
            Attr::Italic => self.template.italic = true,
            Attr::CancelItalic => self.template.italic = false,
            Attr::Underline => self.set_underline(UnderlineStyle::Single),
            Attr::DoubleUnderline => self.set_underline(UnderlineStyle::Double),
            Attr::Undercurl => self.set_underline(UnderlineStyle::Curl),
            Attr::DottedUnderline => self.set_underline(UnderlineStyle::Dotted),
            Attr::DashedUnderline => self.set_underline(UnderlineStyle::Dashed),
            Attr::CancelUnderline => self.set_underline(UnderlineStyle::None),
            Attr::UnderlineColor(color) => {
                self.template.underline_color = color.map(|color| convert_color(color, true));
            }
            Attr::BlinkSlow | Attr::BlinkFast => self.template.blink = true,
            Attr::CancelBlink => self.template.blink = false,
            Attr::Reverse => self.template.inverse = true,
            Attr::CancelReverse => self.template.inverse = false,
            Attr::Hidden => self.template.hidden = true,
            Attr::CancelHidden => self.template.hidden = false,
            Attr::Strike => self.template.strikeout = true,
            Attr::CancelStrike => self.template.strikeout = false,
            Attr::Foreground(color) => self.template.foreground = convert_color(color, true),
            Attr::Background(color) => self.template.background = convert_color(color, false),
        }
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        self.update_private_mode(mode, true);
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        self.update_private_mode(mode, false);
    }

    fn set_title(&mut self, title: Option<String>) {
        self.title.clone_from(&title);
        self.events.push(TerminalEvent::Title(title));
    }

    fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        self.template.hyperlink = hyperlink.map(|link| TerminalHyperlink {
            id: Some(link.id.unwrap_or_else(synthesize_hyperlink_id)),
            uri: link.uri,
        });
    }

    fn set_keypad_application_mode(&mut self) {
        self.modes.application_keypad = true;
        self.events.push(TerminalEvent::ModeChange(self.modes));
    }

    fn unset_keypad_application_mode(&mut self) {
        self.modes.application_keypad = false;
        self.events.push(TerminalEvent::ModeChange(self.modes));
    }

    fn bell(&mut self) {
        self.events.push(TerminalEvent::Bell);
    }

    fn identify_terminal(&mut self, intermediate: Option<char>) {
        let reply = match intermediate {
            // Primary DA: VT220 with ANSI color. Sixel (4) is deliberately absent — DCS is
            // discarded before it reaches the parser, so advertising it only persuades
            // applications to emit bytes that vanish.
            None => b"\x1b[?62;22c".to_vec(),
            Some('>') => {
                format!("\x1b[>0;{};1c", version_number(env!("CARGO_PKG_VERSION"))).into_bytes()
            }
            Some('=') => b"\x1bP!|00000000\x1b\\".to_vec(),
            _ => return,
        };
        self.events.push(TerminalEvent::PtyWrite(reply));
    }

    fn clipboard_store(&mut self, clipboard: u8, base64: &[u8]) {
        // Whether the store is permitted, and from which pane, is the session's decision: only it
        // knows which pane holds focus and what the configured OSC 52 policy is.
        let Some(text) = decode_clipboard_payload(base64) else {
            return;
        };
        self.events.push(TerminalEvent::ClipboardStore {
            selection: clipboard,
            text,
        });
    }

    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        self.events.push(TerminalEvent::ClipboardLoad {
            selection: clipboard,
            terminator: terminator.to_owned(),
        });
    }

    fn report_private_mode(&mut self, mode: PrivateMode) {
        // DECRPM states: 1 set, 2 reset, 0 not recognized. Reporting 0 for a mode we do implement
        // would keep applications from using it at all — synchronized output is detected this way,
        // not through device attributes — so the answer set is enumerated rather than guessed.
        let enabled = match mode.raw() {
            1 => self.modes.application_cursor,
            6 => self.origin_mode,
            25 => self.modes.cursor_visible,
            47 | 1047 | 1049 => self.alternate_screen,
            1000 => self.modes.mouse_clicks,
            1002 | 1003 => self.modes.mouse_motion,
            1004 => self.modes.focus_reporting,
            1006 => self.modes.sgr_mouse,
            1016 => self.modes.sgr_pixels,
            2004 => self.modes.bracketed_paste,
            // vte buffers synchronized updates itself and the session applies the deadline, so
            // support is real, but an update is never still open once a query is answered.
            2026 => false,
            _ => {
                self.events.push(TerminalEvent::PtyWrite(
                    format!("\x1b[?{};0$y", mode.raw()).into_bytes(),
                ));
                return;
            }
        };
        let state = if enabled { 1 } else { 2 };
        self.events.push(TerminalEvent::PtyWrite(
            format!("\x1b[?{};{state}$y", mode.raw()).into_bytes(),
        ));
    }

    fn device_status(&mut self, status: usize) {
        match status {
            5 => self
                .events
                .push(TerminalEvent::PtyWrite(b"\x1b[0n".to_vec())),
            6 => self.events.push(TerminalEvent::PtyWrite(
                format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_col + 1).into_bytes(),
            )),
            _ => {}
        }
    }

    fn report_keyboard_mode(&mut self) {
        self.events.push(TerminalEvent::PtyWrite(
            format!("\x1b[?{}u", self.modes.keyboard_flags).into_bytes(),
        ));
    }

    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        if self.keyboard_mode_stack.len() >= KEYBOARD_MODE_STACK_MAX_DEPTH {
            self.keyboard_mode_stack.remove(0);
        }
        self.keyboard_mode_stack.push(mode);
        self.apply_keyboard_mode(mode, KeyboardModesApplyBehavior::Replace);
    }

    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        let new_len = self
            .keyboard_mode_stack
            .len()
            .saturating_sub(usize::from(to_pop));
        self.keyboard_mode_stack.truncate(new_len);
        let mode = self
            .keyboard_mode_stack
            .last()
            .copied()
            .unwrap_or(KeyboardModes::NO_MODE);
        self.apply_keyboard_mode(mode, KeyboardModesApplyBehavior::Replace);
    }

    fn set_keyboard_mode(&mut self, mode: KeyboardModes, behavior: KeyboardModesApplyBehavior) {
        self.apply_keyboard_mode(mode, behavior);
    }
}

impl Terminal {
    fn update_private_mode(&mut self, mode: PrivateMode, enabled: bool) {
        match mode.raw() {
            1 => self.modes.application_cursor = enabled,
            6 => {
                // Both DECSET and DECRST of DECOM home the cursor, and home is itself
                // origin-relative, so the flag has to be live before the move.
                self.origin_mode = enabled;
                self.goto(0, 0);
            }
            2004 => self.modes.bracketed_paste = enabled,
            1000 => self.modes.mouse_clicks = enabled,
            1002 | 1003 => {
                // Button-event and any-event tracking both include button press/release reports.
                // Treating these modes as motion-only makes applications which request 1003
                // (including Vrowser) unable to receive a click at all.
                self.modes.mouse_clicks = enabled;
                self.modes.mouse_motion = enabled;
            }
            1006 => self.modes.sgr_mouse = enabled,
            1016 => self.modes.sgr_pixels = enabled,
            1004 => self.modes.focus_reporting = enabled,
            25 => self.modes.cursor_visible = enabled,
            47 | 1047 if enabled != self.alternate_screen => {
                // Legacy smcup/rmcup. Neither variant saves or restores the cursor. 1047 clears
                // the alternate screen on the way out of it; 47 leaves it intact.
                if !enabled && mode.raw() == 1047 {
                    self.clear_active_grid();
                }
                self.swap_screens();
            }
            1048 => {
                // Cursor save and restore with no screen switch.
                if enabled {
                    self.save_cursor_position();
                } else {
                    self.restore_cursor_position();
                }
            }
            1049 if enabled != self.alternate_screen => {
                // Save before the swap, restore after it. Each screen owns its DECSC slot, so the
                // primary cursor has to be written while the primary screen is still active and
                // read back only once it is active again — otherwise an application's own DECSC
                // inside the alternate screen overwrites the position the shell needs back.
                if enabled {
                    self.save_cursor_position();
                    self.swap_screens();
                    self.clear_active_grid();
                } else {
                    self.swap_screens();
                    self.restore_cursor_position();
                }
            }
            _ => {}
        }
        self.events.push(TerminalEvent::ModeChange(self.modes));
    }

    fn apply_keyboard_mode(&mut self, mode: KeyboardModes, behavior: KeyboardModesApplyBehavior) {
        let active = KeyboardModes::from_bits_truncate(self.modes.keyboard_flags);
        let next = match behavior {
            KeyboardModesApplyBehavior::Replace => mode,
            KeyboardModesApplyBehavior::Union => active.union(mode),
            KeyboardModesApplyBehavior::Difference => active.difference(mode),
        };
        self.modes.keyboard_flags = next.bits();
        if let Some(top) = self.keyboard_mode_stack.last_mut() {
            *top = next;
        }
        self.events.push(TerminalEvent::ModeChange(self.modes));
    }
}

impl Terminal {
    fn set_underline(&mut self, style: UnderlineStyle) {
        self.template.underline = style != UnderlineStyle::None;
        self.template.underline_style = style;
    }
}

fn blank_grid(rows: usize, cols: usize) -> Vec<Vec<Cell>> {
    vec![vec![Cell::default(); cols]; rows]
}

fn resize_grid(grid: &mut Vec<Vec<Cell>>, rows: usize, cols: usize) {
    grid.resize_with(rows, || vec![Cell::default(); cols]);
    for row in grid {
        row.resize(cols, Cell::default());
    }
}

/// Decode an OSC 52 store payload, rejecting anything that would decode past the copy-buffer
/// ceiling before allocating for it.
fn decode_clipboard_payload(base64: &[u8]) -> Option<Vec<u8>> {
    // Every four base64 characters carry at most three bytes, so the decoded size is bounded
    // without decoding. The extra group covers an unpadded trailing partial group.
    let decoded_bound = (base64.len() / 4).checked_add(1)?.checked_mul(3)?;
    if decoded_bound > CLIPBOARD_DECODED_MAX_BYTES {
        return None;
    }
    CLIPBOARD_BASE64.decode(base64).ok()
}

/// Encode a semantic version as the single integer a secondary device-attributes reply carries.
fn version_number(mut version: &str) -> usize {
    if let Some(separator) = version.rfind('-') {
        version = &version[..separator];
    }
    let mut number = 0;
    for (index, part) in version.split('.').rev().enumerate() {
        number += usize::pow(100, index as u32) * part.parse::<usize>().unwrap_or(0);
    }
    number
}

fn default_tab_stops(cols: usize, interval: usize) -> Vec<bool> {
    (0..cols)
        .map(|column| column > 0 && column % interval == 0)
        .collect()
}

fn append_row_text(output: &mut String, cells: &[Cell]) {
    let end = cells
        .iter()
        .rposition(|cell| {
            cell.ch != ' '
                || !cell.combining.is_empty()
                || cell.tab_width.is_some()
                || cell.wide_continuation
                || cell.leading_wide_spacer
        })
        .map_or(0, |index| index + 1);
    let mut column = 0;
    while column < end {
        let cell = &cells[column];
        if let Some(width) = cell.tab_width {
            output.push('\t');
            column = column.saturating_add(usize::from(width).max(1));
            continue;
        }
        if !cell.wide_continuation && !cell.leading_wide_spacer {
            output.push(cell.ch);
            output.push_str(&cell.combining);
        }
        column += 1;
    }
}

fn sgr_status(cell: &Cell) -> String {
    let mut params = Vec::new();
    if cell.bold {
        params.push("1".to_owned());
    }
    if cell.dim {
        params.push("2".to_owned());
    }
    if cell.italic {
        params.push("3".to_owned());
    }
    match cell.underline_style {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => params.push("4".to_owned()),
        UnderlineStyle::Double => params.push("4:2".to_owned()),
        UnderlineStyle::Curl => params.push("4:3".to_owned()),
        UnderlineStyle::Dotted => params.push("4:4".to_owned()),
        UnderlineStyle::Dashed => params.push("4:5".to_owned()),
    }
    if cell.blink {
        params.push("5".to_owned());
    }
    if cell.inverse {
        params.push("7".to_owned());
    }
    if cell.hidden {
        params.push("8".to_owned());
    }
    if cell.strikeout {
        params.push("9".to_owned());
    }
    push_sgr_color(&mut params, cell.foreground, true);
    push_sgr_color(&mut params, cell.background, false);
    if params.is_empty() {
        params.push("0".to_owned());
    }
    format!("{}m", params.join(";"))
}

fn push_sgr_color(params: &mut Vec<String>, color: TerminalColor, foreground: bool) {
    let base = if foreground { 30 } else { 40 };
    match color {
        TerminalColor::Default => {}
        TerminalColor::Indexed(index @ 0..=7) => params.push((base + index).to_string()),
        TerminalColor::Indexed(index @ 8..=15) => {
            params.push((base + 60 + index - 8).to_string());
        }
        TerminalColor::Indexed(index) => {
            params.push(format!("{};5;{index}", if foreground { 38 } else { 48 }));
        }
        TerminalColor::Rgb(red, green, blue) => params.push(format!(
            "{};2;{red};{green};{blue}",
            if foreground { 38 } else { 48 }
        )),
    }
}

fn convert_color(color: Color, foreground: bool) -> TerminalColor {
    match color {
        Color::Indexed(index) => TerminalColor::Indexed(index),
        Color::Spec(rgb) => TerminalColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Named(NamedColor::Foreground) if foreground => TerminalColor::Default,
        Color::Named(NamedColor::Background) if !foreground => TerminalColor::Default,
        Color::Named(named) => {
            let index = named as usize;
            if index < 16 {
                TerminalColor::Indexed(index as u8)
            } else {
                TerminalColor::Default
            }
        }
    }
}

const MAX_MARKER_BYTES: usize = 128;

#[derive(Clone, Copy)]
struct MarkerEnvelope {
    prefix: &'static [u8],
    terminator: &'static [u8],
    payload_skip: usize,
}

const APC_ENVELOPE: MarkerEnvelope = MarkerEnvelope {
    prefix: b"\x1b_VIVID;3;A;",
    terminator: b"\x1b\\",
    payload_skip: 2,
};

enum KittyChunk {
    Bytes(Vec<u8>),
    Command(KittyGraphicsCommand),
}

#[derive(Default)]
struct KittyGraphicsScanner {
    pending: Vec<u8>,
}

impl KittyGraphicsScanner {
    fn push(&mut self, bytes: &[u8]) -> Vec<KittyChunk> {
        const PREFIX: &[u8] = b"\x1b_G";
        const TERMINATOR: &[u8] = b"\x1b\\";

        self.pending.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        let mut cursor = 0;
        loop {
            let Some(relative_start) = find_bytes(&self.pending[cursor..], PREFIX) else {
                let keep = partial_prefix_len(&self.pending[cursor..], PREFIX);
                let end = self.pending.len().saturating_sub(keep);
                push_kitty_bytes(&mut chunks, &self.pending[cursor..end]);
                cursor = end;
                break;
            };
            let start = cursor + relative_start;
            push_kitty_bytes(&mut chunks, &self.pending[cursor..start]);
            let body_start = start + PREFIX.len();
            let Some(relative_end) = find_bytes(&self.pending[body_start..], TERMINATOR) else {
                if self.pending.len().saturating_sub(start) > KITTY_PACKET_MAX_BYTES {
                    push_kitty_bytes(&mut chunks, &self.pending[start..body_start]);
                    cursor = body_start;
                    continue;
                }
                cursor = start;
                break;
            };
            let terminator = body_start + relative_end;
            let end = terminator + TERMINATOR.len();
            if end.saturating_sub(start) > KITTY_PACKET_MAX_BYTES {
                push_kitty_bytes(&mut chunks, &self.pending[start..body_start]);
                cursor = body_start;
                continue;
            }
            let raw = &self.pending[start..end];
            match validate_kitty_command(&self.pending[body_start..terminator], raw) {
                Some(command) => chunks.push(KittyChunk::Command(command)),
                None => push_kitty_bytes(&mut chunks, raw),
            }
            cursor = end;
        }
        self.pending.drain(..cursor);
        chunks
    }

    fn finish(&mut self) -> Vec<KittyChunk> {
        let pending = mem::take(&mut self.pending);
        if pending.is_empty() {
            Vec::new()
        } else {
            vec![KittyChunk::Bytes(pending)]
        }
    }
}

fn validate_kitty_command(body: &[u8], raw: &[u8]) -> Option<KittyGraphicsCommand> {
    let separator = body.iter().position(|byte| *byte == b';');
    let (control, payload) = separator.map_or((body, &[][..]), |index| {
        (&body[..index], &body[index + 1..])
    });
    let control = std::str::from_utf8(control).ok()?;
    if !control.is_ascii() || payload.len() > KITTY_PAYLOAD_MAX_BYTES {
        return None;
    }

    let mut fields = BTreeMap::new();
    for field in control.split(',') {
        let (key, value) = field.split_once('=')?;
        if key.is_empty()
            || value.is_empty()
            || !key.bytes().all(|byte| byte.is_ascii_alphabetic())
            || !value.is_ascii()
            || fields.insert(key, value).is_some()
        {
            return None;
        }
    }
    let number = |key: &str| fields.get(key).and_then(|value| value.parse::<u32>().ok());
    let more = number("m").unwrap_or(0);
    if more > 1 || number("q") != Some(2) {
        return None;
    }

    match fields.get("a").copied() {
        Some("q") => {
            if !keys_are(&fields, &["a", "i", "s", "v", "t", "f", "q"])
                || number("i") == Some(0)
                || number("i").is_none()
                || number("s") != Some(1)
                || number("v") != Some(1)
                || number("f") != Some(24)
                || fields.get("t").copied() != Some("d")
                || payload != b"AAAA"
            {
                return None;
            }
            Some(KittyGraphicsCommand::Query {
                image_id: number("i")?,
            })
        }
        Some("T") | Some("t") => {
            let action = fields.get("a").copied()?;
            let allowed = if action == "T" {
                &["a", "i", "f", "s", "v", "c", "r", "U", "C", "q", "m", "t"][..]
            } else {
                &["a", "i", "f", "s", "v", "q", "m", "t"][..]
            };
            let format = number("f")?;
            let width = number("s")?;
            let height = number("v")?;
            let bytes_per_pixel = match format {
                24 => 3,
                32 => 4,
                _ => return None,
            };
            let decoded_bytes = width.checked_mul(height)?.checked_mul(bytes_per_pixel)?;
            if !keys_are(&fields, allowed)
                || fields.get("t").is_some_and(|transport| *transport != "d")
                || width == 0
                || height == 0
                || decoded_bytes > KITTY_DECODED_TRANSFER_MAX_BYTES
                || payload.is_empty()
                || !valid_base64_payload(payload, more == 1)
            {
                return None;
            }
            if action == "T"
                && (number("i").is_none_or(|value| value == 0)
                    || number("U") != Some(1)
                    || number("c").is_none_or(|value| value == 0)
                    || number("r").is_none_or(|value| value == 0))
            {
                return None;
            }
            Some(KittyGraphicsCommand::Packet {
                bytes: raw.to_vec(),
                starts_transfer: true,
                more: more == 1,
            })
        }
        Some("p") => {
            if !keys_are(&fields, &["a", "i", "c", "r", "U", "C", "q"])
                || number("i").is_none_or(|value| value == 0)
                || number("U") != Some(1)
                || number("c").is_none_or(|value| value == 0)
                || number("r").is_none_or(|value| value == 0)
                || !payload.is_empty()
            {
                return None;
            }
            Some(KittyGraphicsCommand::Packet {
                bytes: raw.to_vec(),
                starts_transfer: true,
                more: false,
            })
        }
        Some("d") => {
            if !keys_are(&fields, &["a", "d", "i", "q"])
                || !matches!(fields.get("d").copied(), Some("i" | "I"))
                || number("i").is_none_or(|value| value == 0)
                || !payload.is_empty()
            {
                return None;
            }
            Some(KittyGraphicsCommand::Packet {
                bytes: raw.to_vec(),
                starts_transfer: true,
                more: false,
            })
        }
        None => {
            if !keys_are(&fields, &["q", "m"])
                || separator.is_none()
                || payload.is_empty()
                || !valid_base64_payload(payload, more == 1)
            {
                return None;
            }
            Some(KittyGraphicsCommand::Packet {
                bytes: raw.to_vec(),
                starts_transfer: false,
                more: more == 1,
            })
        }
        _ => None,
    }
}

fn keys_are(fields: &BTreeMap<&str, &str>, allowed: &[&str]) -> bool {
    fields.keys().all(|key| allowed.contains(key))
}

fn valid_base64_payload(payload: &[u8], more: bool) -> bool {
    payload.len() <= KITTY_PAYLOAD_MAX_BYTES
        && (!more || payload.len().is_multiple_of(4))
        && payload
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        && base64::engine::general_purpose::STANDARD
            .decode(payload)
            .is_ok()
}

fn push_kitty_bytes(chunks: &mut Vec<KittyChunk>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(KittyChunk::Bytes(previous)) = chunks.last_mut() {
        previous.extend_from_slice(bytes);
    } else {
        chunks.push(KittyChunk::Bytes(bytes.to_vec()));
    }
}

#[cfg(windows)]
const CONPTY_ENVELOPE: MarkerEnvelope = MarkerEnvelope {
    prefix: b"VIVID;3;A;",
    terminator: b";VIVID-END",
    payload_skip: 0,
};

enum VividChunk {
    Bytes(Vec<u8>),
    Marker(String),
}

#[derive(Default)]
struct VividMarkerScanner {
    pending: Vec<u8>,
}

impl VividMarkerScanner {
    fn push(&mut self, bytes: &[u8]) -> Vec<VividChunk> {
        self.pending.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        let mut cursor = 0;
        loop {
            let Some((relative_start, envelope)) = find_envelope(&self.pending[cursor..]) else {
                let keep = marker_envelopes()
                    .iter()
                    .map(|envelope| partial_prefix_len(&self.pending[cursor..], envelope.prefix))
                    .max()
                    .unwrap_or(0);
                let end = self.pending.len().saturating_sub(keep);
                push_bytes(&mut chunks, &self.pending[cursor..end]);
                cursor = end;
                break;
            };
            let start = cursor + relative_start;
            push_bytes(&mut chunks, &self.pending[cursor..start]);
            let search_start = start + envelope.prefix.len();
            let Some(relative_end) = find_bytes(&self.pending[search_start..], envelope.terminator)
            else {
                if self.pending.len() - start > MAX_MARKER_BYTES {
                    push_bytes(&mut chunks, &self.pending[start..search_start]);
                    cursor = search_start;
                    continue;
                }
                cursor = start;
                break;
            };
            let terminator = search_start + relative_end;
            let end = terminator + envelope.terminator.len();
            if end - start > MAX_MARKER_BYTES {
                push_bytes(&mut chunks, &self.pending[start..search_start]);
                cursor = search_start;
                continue;
            }
            match std::str::from_utf8(&self.pending[start + envelope.payload_skip..terminator]) {
                Ok(marker) if valid_marker_shape(marker) => {
                    chunks.push(VividChunk::Marker(marker.to_owned()));
                }
                _ => push_bytes(&mut chunks, &self.pending[start..end]),
            }
            cursor = end;
        }
        self.pending.drain(..cursor);
        chunks
    }

    fn finish(&mut self) -> Vec<VividChunk> {
        let pending = mem::take(&mut self.pending);
        if pending.is_empty() {
            Vec::new()
        } else {
            vec![VividChunk::Bytes(pending)]
        }
    }
}

fn marker_envelopes() -> &'static [MarkerEnvelope] {
    #[cfg(unix)]
    {
        std::slice::from_ref(&APC_ENVELOPE)
    }
    #[cfg(windows)]
    {
        &[CONPTY_ENVELOPE, APC_ENVELOPE]
    }
}

fn find_envelope(haystack: &[u8]) -> Option<(usize, MarkerEnvelope)> {
    marker_envelopes()
        .iter()
        .filter_map(|envelope| {
            find_bytes(haystack, envelope.prefix).map(|position| (position, *envelope))
        })
        .min_by_key(|(position, _)| *position)
}

fn valid_marker_shape(marker: &str) -> bool {
    if marker.len() > 124 || !marker.is_ascii() {
        return false;
    }
    let mut fields = marker.split(';');
    let valid = fields.next() == Some("VIVID")
        && fields.next() == Some("3")
        && fields.next() == Some("A")
        && fields
            .next()
            .is_some_and(|tag| tag.len() == 22 && tag.bytes().all(is_base64url))
        && fields.next().is_some_and(|id| {
            id.len() == 16
                && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && id.bytes().any(|byte| byte != b'0')
        })
        && fields.next().is_some_and(|id| {
            id.len() == 16
                && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && id.bytes().any(|byte| byte != b'0')
        })
        && fields
            .next()
            .is_some_and(|auth| auth.len() == 22 && auth.bytes().all(is_base64url));
    valid && fields.next().is_none()
}

fn is_base64url(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn partial_prefix_len(haystack: &[u8], prefix: &[u8]) -> usize {
    (1..=cmp::min(haystack.len(), prefix.len().saturating_sub(1)))
        .rev()
        .find(|&len| haystack.ends_with(&prefix[..len]))
        .unwrap_or(0)
}

fn push_bytes(chunks: &mut Vec<VividChunk>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(VividChunk::Bytes(previous)) = chunks.last_mut() {
        previous.extend_from_slice(bytes);
    } else {
        chunks.push(VividChunk::Bytes(bytes.to_vec()));
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    fn scrolled(lines: usize) -> Terminal {
        let mut terminal = Terminal::new(4, 20, 200);
        for line in 0..lines {
            terminal.feed(format!("line-{line}\r\n").as_bytes());
        }
        terminal
    }

    #[test]
    fn history_tail_returns_the_most_recent_lines_oldest_first() {
        let terminal = scrolled(20);
        let tail = terminal.history_tail(3);
        assert_eq!(tail.len(), 3);
        let text = |cells: &[Cell]| cells.iter().map(|cell| cell.ch).collect::<String>();
        let rendered = tail
            .iter()
            .map(|(cells, _)| text(cells).trim_end().to_owned())
            .collect::<Vec<_>>();
        assert!(
            rendered.windows(2).all(|pair| pair[0] < pair[1]),
            "not oldest first: {rendered:?}"
        );
        assert!(tail.len() <= terminal.history_len());
    }

    /// The whole point of not going through `feed`: a restore must place cells, never interpret
    /// bytes. Anything the parser would have acted on has to be inert by construction.
    #[test]
    fn restoring_history_places_cells_without_producing_events() {
        let mut terminal = Terminal::new(4, 20, 200);
        let cells = "restored"
            .chars()
            .map(|ch| Cell {
                ch,
                ..Cell::default()
            })
            .collect::<Vec<_>>();
        terminal.restore_history(vec![(cells, false)]);

        assert_eq!(terminal.history_len(), 1);
        let restored = terminal
            .viewport_line(-1)
            .unwrap()
            .iter()
            .map(|cell| cell.ch)
            .collect::<String>();
        assert_eq!(restored.trim_end(), "restored");
        assert_eq!(terminal.line_wrapped(-1), Some(false));
    }

    /// The security claim, stated as a test: a persisted line carrying sequences the parser acts on
    /// must come back as inert text. Routing `restore_history` through `feed` fails this.
    #[test]
    fn restoring_history_never_acts_on_what_it_restores() {
        let mut terminal = Terminal::new(4, 40, 200);
        // A clipboard write, a device-status query, a title change, and a bell — every one an
        // event `feed` would emit and a restore must not.
        let hostile = "\u{1b}]52;c;aGVsbG8=\u{7}\u{1b}[5n\u{1b}]0;pwned\u{7}\u{7}text";
        let cells = hostile
            .chars()
            .map(|ch| Cell {
                ch,
                ..Cell::default()
            })
            .collect::<Vec<_>>();
        terminal.restore_history(vec![(cells, false)]);

        let restored = terminal
            .viewport_line(-1)
            .unwrap()
            .iter()
            .map(|cell| cell.ch)
            .collect::<String>();
        assert!(
            restored.contains('\u{1b}') && restored.contains("text"),
            "the bytes were interpreted rather than stored: {restored:?}"
        );
        // Feeding the same bytes is what a naive implementation would do, and it is not inert.
        let mut compare = Terminal::new(4, 40, 200);
        let events = compare.feed(hostile.as_bytes());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TerminalEvent::ClipboardStore { .. })),
            "the fixture no longer exercises a side effect: {events:?}"
        );
    }

    #[test]
    fn restoring_history_replaces_rather_than_appends_and_respects_the_limit() {
        let mut terminal = Terminal::new(4, 20, 2);
        terminal.feed(b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n");
        let row = |ch: char| {
            (
                vec![Cell {
                    ch,
                    ..Cell::default()
                }],
                false,
            )
        };
        terminal.restore_history(vec![row('1'), row('2'), row('3')]);
        // Capped at the limit, and it is the *newest* that survive.
        assert_eq!(terminal.history_len(), 2);
        let text = |line: isize| terminal.viewport_line(line).unwrap()[0].ch;
        assert_eq!((text(-2), text(-1)), ('2', '3'));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_ansi_wide_and_scrollback() {
        let mut terminal = Terminal::new(2, 4, 10);
        terminal.feed(b"ab\r\ncd\r\nef");
        assert_eq!(terminal.history_len(), 1);
        assert_eq!(terminal.cells()[0][0].ch, 'c');
        terminal.feed("界".as_bytes());
        assert!(
            terminal
                .cells()
                .iter()
                .flatten()
                .any(|cell| cell.wide_continuation)
        );
    }

    #[test]
    fn vrowser_input_modes_are_retained_for_the_outer_client() {
        let mut terminal = Terminal::new(24, 80, 0);
        terminal.feed(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[>31u");
        assert_eq!(
            terminal.modes(),
            TerminalModes {
                mouse_clicks: true,
                mouse_motion: true,
                sgr_mouse: true,
                sgr_pixels: true,
                keyboard_flags: 31,
                cursor_visible: true,
                ..TerminalModes::default()
            }
        );

        terminal.feed(b"\x1b[<u\x1b[?1016l\x1b[?1006l\x1b[?1003l");
        assert!(!terminal.modes().mouse_clicks);
        assert!(!terminal.modes().mouse_motion);
        assert!(!terminal.modes().sgr_mouse);
        assert!(!terminal.modes().sgr_pixels);
        assert_eq!(terminal.modes().keyboard_flags, 0);
    }

    #[test]
    fn keyboard_mode_query_replies_to_the_child_pty() {
        let mut terminal = Terminal::new(2, 4, 0);
        terminal.feed(b"\x1b[>31u");
        let events = terminal.feed(b"\x1b[?u");
        assert!(events.contains(&TerminalEvent::PtyWrite(b"\x1b[?31u".to_vec())));
    }

    #[test]
    fn kitty_packets_are_bounded_validated_and_fragment_safe() {
        let packet = b"\x1b_Ga=T,i=16909060,f=32,s=1,v=1,c=1,r=1,U=1,q=2,m=0;AAAAAA==\x1b\\";
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(2, 4, 0);
            let mut events = terminal.feed(&packet[..split]);
            events.extend(terminal.feed(&packet[split..]));
            assert!(events.contains(&TerminalEvent::KittyGraphics(
                KittyGraphicsCommand::Packet {
                    bytes: packet.to_vec(),
                    starts_transfer: true,
                    more: false,
                }
            )));
            assert!(terminal.cells().iter().flatten().all(|cell| cell.ch == ' '));
        }

        let mut terminal = Terminal::new(2, 4, 0);
        let rejected = terminal.feed(b"\x1b_Ga=T,t=f,i=1,f=32,s=1,v=1,c=1,r=1,U=1,q=2;AAAA\x1b\\");
        assert!(
            !rejected
                .iter()
                .any(|event| matches!(event, TerminalEvent::KittyGraphics(_)))
        );

        let rejected = terminal.feed(b"\x1b_Ga=T,i=1,f=32,s=1,v=1,c=1,r=1,U=1,q=2;A===\x1b\\");
        assert!(
            !rejected
                .iter()
                .any(|event| matches!(event, TerminalEvent::KittyGraphics(_)))
        );

        let rejected =
            terminal.feed(b"\x1b_Ga=T,i=1,f=32,s=100000,v=100000,c=1,r=1,U=1,q=2;AAAA\x1b\\");
        assert!(
            !rejected
                .iter()
                .any(|event| matches!(event, TerminalEvent::KittyGraphics(_)))
        );

        let oversized = vec![b'A'; KITTY_PAYLOAD_MAX_BYTES + 1];
        let mut packet = b"\x1b_Ga=T,i=1,f=32,s=1,v=1,c=1,r=1,U=1,q=2;".to_vec();
        packet.extend_from_slice(&oversized);
        packet.extend_from_slice(b"\x1b\\");
        let rejected = terminal.feed(&packet);
        assert!(
            !rejected
                .iter()
                .any(|event| matches!(event, TerminalEvent::KittyGraphics(_)))
        );
    }

    #[test]
    fn kitty_chunks_queries_and_vivid_markers_coexist() {
        let first = b"\x1b_Ga=T,i=1,f=32,s=1,v=1,c=1,r=1,U=1,q=2,m=1;AAAA\x1b\\";
        let last = b"\x1b_Gq=2,m=0;AAAA\x1b\\";
        let marker = b"\x1b_VIVID;3;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA\x1b\\";
        let query = b"\x1b_Ga=q,t=d,f=24,s=1,v=1,i=31,q=2;AAAA\x1b\\";
        let mut input = Vec::new();
        input.extend_from_slice(first);
        input.extend_from_slice(marker);
        input.extend_from_slice(last);
        input.extend_from_slice(query);

        let mut terminal = Terminal::new(2, 4, 0);
        let events = terminal.feed(&input);
        assert!(events.contains(&TerminalEvent::KittyGraphics(
            KittyGraphicsCommand::Packet {
                bytes: first.to_vec(),
                starts_transfer: true,
                more: true,
            }
        )));
        assert!(events.contains(&TerminalEvent::KittyGraphics(
            KittyGraphicsCommand::Packet {
                bytes: last.to_vec(),
                starts_transfer: false,
                more: false,
            }
        )));
        assert!(
            events.contains(&TerminalEvent::KittyGraphics(KittyGraphicsCommand::Query {
                image_id: 31
            }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TerminalEvent::VividMarker { .. }))
        );
    }

    #[test]
    fn marker_is_consumed_across_boundaries() {
        let marker =
            b"\x1b_VIVID;3;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA\x1b\\";
        for split in 0..=marker.len() {
            let mut terminal = Terminal::new(2, 20, 0);
            let mut events = terminal.feed(&marker[..split]);
            events.extend(terminal.feed(&marker[split..]));
            assert!(
                events.contains(&TerminalEvent::VividMarker {
                    marker:
                        "VIVID;3;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA"
                            .into(),
                    row: 0,
                    column: 0,
                    alternate: false,
                })
            );
            assert!(terminal.cells().iter().flatten().all(|cell| cell.ch == ' '));
        }
    }

    #[cfg(windows)]
    #[test]
    fn conpty_marker_is_consumed_across_boundaries() {
        let marker =
            b"VIVID;3;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA;VIVID-END";
        for split in 0..=marker.len() {
            let mut terminal = Terminal::new(2, 20, 0);
            let mut events = terminal.feed(&marker[..split]);
            events.extend(terminal.feed(&marker[split..]));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, TerminalEvent::VividMarker { .. }))
                    .count(),
                1
            );
            assert!(terminal.cells().iter().flatten().all(|cell| cell.ch == ' '));
        }
    }

    #[test]
    fn malformed_and_oversized_markers_are_byte_exact_text() {
        let malformed = b"\x1b_VIVID;3;A;short;0000000000000003;0000000000000007;bad\x1b\\";
        let mut scanner = VividMarkerScanner::default();
        let bytes = scanner
            .push(malformed)
            .into_iter()
            .flat_map(|chunk| match chunk {
                VividChunk::Bytes(bytes) => bytes,
                VividChunk::Marker(_) => panic!("malformed marker was consumed"),
            })
            .collect::<Vec<_>>();
        assert_eq!(bytes, malformed);

        let mut oversized = APC_ENVELOPE.prefix.to_vec();
        oversized.extend(std::iter::repeat_n(b'x', MAX_MARKER_BYTES));
        oversized.extend_from_slice(APC_ENVELOPE.terminator);
        let bytes = scanner
            .push(&oversized)
            .into_iter()
            .flat_map(|chunk| match chunk {
                VividChunk::Bytes(bytes) => bytes,
                VividChunk::Marker(_) => panic!("oversized marker was consumed"),
            })
            .collect::<Vec<_>>();
        assert_eq!(bytes, oversized);
    }

    #[test]
    fn adjacent_markers_are_zero_width_and_surrounding_utf8_is_byte_exact() {
        let marker =
            b"\x1b_VIVID;3;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA\x1b\\";
        let mut input = "before-界".as_bytes().to_vec();
        input.extend_from_slice(marker);
        input.extend_from_slice(marker);
        input.extend_from_slice("-after-é".as_bytes());
        let mut scanner = VividMarkerScanner::default();
        let mut output = Vec::new();
        let mut markers = 0;
        for byte in input {
            for chunk in scanner.push(&[byte]) {
                match chunk {
                    VividChunk::Bytes(bytes) => output.extend(bytes),
                    VividChunk::Marker(_) => markers += 1,
                }
            }
        }
        assert_eq!(markers, 2);
        assert_eq!(output, "before-界-after-é".as_bytes());
    }

    #[test]
    fn partial_marker_candidate_is_preserved_when_disproved() {
        let candidate = b"prefix\x1b_VIVID;3;A;partial!suffix";
        let mut scanner = VividMarkerScanner::default();
        let mut chunks = Vec::new();
        for byte in candidate {
            chunks.extend(scanner.push(std::slice::from_ref(byte)));
        }
        chunks.extend(scanner.finish());
        let bytes = chunks
            .into_iter()
            .flat_map(|chunk| match chunk {
                VividChunk::Bytes(bytes) => bytes,
                VividChunk::Marker(_) => panic!("partial marker was consumed"),
            })
            .collect::<Vec<_>>();
        assert_eq!(bytes, candidate);
    }

    #[cfg(windows)]
    #[test]
    fn malformed_conpty_candidate_is_byte_exact_text() {
        let malformed = b"VIVID;3;A;short;0000000000000003;0000000000000007;bad;VIVID-END";
        let mut scanner = VividMarkerScanner::default();
        let bytes = scanner
            .push(malformed)
            .into_iter()
            .flat_map(|chunk| match chunk {
                VividChunk::Bytes(bytes) => bytes,
                VividChunk::Marker(_) => panic!("malformed ConPTY marker was consumed"),
            })
            .collect::<Vec<_>>();
        assert_eq!(bytes, malformed);
    }

    #[test]
    fn marker_event_carries_the_cell_where_the_marker_was_printed() {
        let mut terminal = Terminal::new(4, 40, 10);
        let mut input = b"\x1b[2;3H".to_vec();
        input.extend_from_slice(
            b"\x1b_VIVID;3;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA\x1b\\",
        );
        // ConPTY batches follow-up repositioning with the marker; the event must keep the
        // marker cell, not the final cursor position.
        input.extend_from_slice(b"\x1b[4;1HC:\\>");
        let events = terminal.feed(&input);
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::VividMarker {
                row: 1,
                column: 2,
                ..
            }
        )));
        assert_eq!(terminal.cursor(), (3, 4));
    }

    #[test]
    fn erase_scrollback_on_blank_viewport_clears_like_conpty_cls() {
        let mut terminal = Terminal::new(3, 10, 10);
        terminal.feed(b"one\r\ntwo\r\nthree\r\nfour");
        // ConPTY cls: home, erase-line on every row, then ED 3. No ED 2 is sent.
        let events = terminal.feed(b"\x1b[H\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[3J");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TerminalEvent::Clear { alternate: false }))
        );
        assert_eq!(terminal.history_len(), 0);

        // A bare scrollback purge with visible viewport content is not a clear.
        let mut populated = Terminal::new(3, 10, 10);
        populated.feed(b"one\r\ntwo\r\nthree\r\nfour");
        let events = populated.feed(b"\x1b[3J");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TerminalEvent::Clear { .. }))
        );
    }

    #[test]
    fn grid_scroll_reports_region_and_history_push() {
        // A full-screen primary-screen scroll carries lines into scrollback.
        let mut terminal = Terminal::new(3, 10, 10);
        terminal.feed(b"one\r\ntwo\r\nthree");
        let events = terminal.feed(b"\r\nx");
        assert!(events.contains(&TerminalEvent::GridScroll {
            lines: 1,
            top: 0,
            bottom: 3,
            pushed_to_history: true,
            alternate: false,
        }));
        assert_eq!(terminal.history_len(), 1);

        // A partial scroll region never pushes to history.
        let mut region = Terminal::new(3, 10, 10);
        region.feed(b"\x1b[2;3r\x1b[3;1H");
        let events = region.feed(b"\r\nx");
        assert!(events.contains(&TerminalEvent::GridScroll {
            lines: 1,
            top: 1,
            bottom: 3,
            pushed_to_history: false,
            alternate: false,
        }));
        assert_eq!(region.history_len(), 0);

        // Alternate-screen scrolls are dropped, not retained.
        let mut alt = Terminal::new(3, 10, 10);
        alt.feed(b"\x1b[?1049h\x1b[3;1H");
        let events = alt.feed(b"\r\n");
        assert!(events.contains(&TerminalEvent::GridScroll {
            lines: 1,
            top: 0,
            bottom: 3,
            pushed_to_history: false,
            alternate: true,
        }));
        assert_eq!(alt.history_len(), 0);

        // A downward scroll reports negative lines.
        let mut down = Terminal::new(3, 10, 10);
        let events = down.feed(b"\x1bM");
        assert!(events.contains(&TerminalEvent::GridScroll {
            lines: -1,
            top: 0,
            bottom: 3,
            pushed_to_history: false,
            alternate: false,
        }));
    }

    #[test]
    fn origin_mode_makes_cursor_addressing_margin_relative_and_survives_save_restore() {
        let mut terminal = Terminal::new(10, 8, 0);
        // Region is 1-based rows 4..8, so origin-relative row 1 is absolute row 3.
        terminal.feed(b"\x1b[4;8r\x1b[?6h");
        // DECOM homes to the top margin on set, not to the screen origin.
        assert_eq!(terminal.cursor(), (3, 0));

        terminal.feed(b"\x1b[2;3H");
        assert_eq!(terminal.cursor(), (4, 2));

        // Addressing past the bottom margin is confined to the region.
        terminal.feed(b"\x1b[9;1H");
        assert_eq!(terminal.cursor(), (7, 0));

        // DECSC captures origin mode with the position; the app then leaves origin mode and
        // addresses absolutely, and DECRC must bring both back.
        terminal.feed(b"\x1b[3;2H\x1b7");
        assert_eq!(terminal.cursor(), (5, 1));
        terminal.feed(b"\x1b[?6l\x1b[1;1H");
        assert_eq!(terminal.cursor(), (0, 0));
        terminal.feed(b"\x1b8");
        assert_eq!(terminal.cursor(), (5, 1));
        terminal.feed(b"\x1b[1;1H");
        assert_eq!(
            terminal.cursor(),
            (3, 0),
            "restore must bring origin mode back"
        );

        // RIS clears origin mode along with the region.
        terminal.feed(b"\x1bc\x1b[1;1H");
        assert_eq!(terminal.cursor(), (0, 0));
    }

    #[test]
    fn scrolling_region_change_homes_to_the_origin_in_both_modes() {
        let mut absolute = Terminal::new(10, 8, 0);
        absolute.feed(b"\x1b[5;5H\x1b[3;9r");
        assert_eq!(absolute.cursor(), (0, 0));

        let mut relative = Terminal::new(10, 8, 0);
        relative.feed(b"\x1b[?6h\x1b[5;5H\x1b[3;9r");
        assert_eq!(relative.cursor(), (2, 0));
    }

    #[test]
    fn line_insert_and_delete_outside_the_scroll_region_do_not_scroll_or_reach_scrollback() {
        let mut terminal = Terminal::new(10, 4, 10);
        for row in 0..10 {
            terminal.feed(format!("\x1b[{};1Hr{row}", row + 1).as_bytes());
        }
        // A region that reaches the last row is the dangerous case: above it, the cursor-relative
        // shim in DL restores `scroll_top == 0 && scroll_bottom == rows`, which is exactly the shape
        // scroll_region_up reads as permission to push rows into history. Those rows never scrolled
        // off, so accepting them silently corrupts scrollback.
        terminal.feed(b"\x1b[6;10r");
        let untouched = terminal.cells().to_vec();

        terminal.feed(b"\x1b[1;1H\x1b[2M");
        assert_eq!(terminal.cells(), untouched.as_slice());
        assert_eq!(terminal.history_len(), 0);

        terminal.feed(b"\x1b[1;1H\x1b[3L");
        assert_eq!(terminal.cells(), untouched.as_slice());
        assert_eq!(terminal.history_len(), 0);

        // Below the region IL and DL must be inert too.
        terminal.feed(b"\x1b[1;5r\x1b[9;1H\x1b[2M\x1b[3L");
        assert_eq!(terminal.cells(), untouched.as_slice());
        assert_eq!(terminal.history_len(), 0);
    }

    #[test]
    fn osc52_store_and_query_are_reported_with_the_selection_and_terminator() {
        let mut terminal = Terminal::new(2, 8, 0);
        let events = terminal.feed(b"\x1b]52;c;aGVsbG8=\x07");
        assert!(events.contains(&TerminalEvent::ClipboardStore {
            selection: b'c',
            text: b"hello".to_vec(),
        }));

        // Producers routinely omit padding, and the pane's own terminator is echoed so the reply
        // is framed the way the request was.
        let unpadded = terminal.feed(b"\x1b]52;p;aGVsbG8\x1b\\");
        assert!(unpadded.contains(&TerminalEvent::ClipboardStore {
            selection: b'p',
            text: b"hello".to_vec(),
        }));

        let queried = terminal.feed(b"\x1b]52;c;?\x1b\\");
        assert!(queried.contains(&TerminalEvent::ClipboardLoad {
            selection: b'c',
            terminator: "\x1b\\".to_owned(),
        }));
    }

    #[test]
    fn oversized_osc52_payload_is_rejected_before_decoding() {
        let oversized = vec![b'A'; (CLIPBOARD_DECODED_MAX_BYTES / 3 + 8) * 4];
        assert!(decode_clipboard_payload(&oversized).is_none());
        assert!(decode_clipboard_payload(b"not valid base64!!").is_none());
        assert_eq!(
            decode_clipboard_payload(b"aGVsbG8=").as_deref(),
            Some(b"hello".as_slice())
        );
    }

    #[test]
    fn device_attribute_queries_answer_primary_secondary_and_tertiary_distinctly() {
        let mut terminal = Terminal::new(2, 8, 0);
        let replies = |events: Vec<TerminalEvent>| {
            events
                .into_iter()
                .filter_map(|event| match event {
                    TerminalEvent::PtyWrite(bytes) => Some(bytes),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            replies(terminal.feed(b"\x1b[c")),
            vec![b"\x1b[?62;22c".to_vec()],
            "primary DA must not claim sixel, which vvmux discards"
        );
        assert_eq!(
            replies(terminal.feed(b"\x1b[>c")),
            vec![format!("\x1b[>0;{};1c", version_number(env!("CARGO_PKG_VERSION"))).into_bytes()]
        );
        assert_eq!(
            replies(terminal.feed(b"\x1b[=c")),
            vec![b"\x1bP!|00000000\x1b\\".to_vec()]
        );
        assert_eq!(version_number("0.4.2"), 402);
        assert_eq!(version_number("1.2.3-rc1"), 10203);
    }

    #[test]
    fn decrqm_reports_tracked_modes_and_zero_for_unknown_modes() {
        let mut terminal = Terminal::new(2, 8, 0);
        let reply = |events: Vec<TerminalEvent>| {
            events
                .into_iter()
                .find_map(|event| match event {
                    TerminalEvent::PtyWrite(bytes) => Some(bytes),
                    _ => None,
                })
                .expect("DECRQM must be answered")
        };

        assert_eq!(reply(terminal.feed(b"\x1b[?2004$p")), b"\x1b[?2004;2$y");
        assert_eq!(
            reply(terminal.feed(b"\x1b[?2004h\x1b[?2004$p")),
            b"\x1b[?2004;1$y"
        );
        // Synchronized output must not answer "not recognized": applications detect it here, and a
        // zero would keep them from ever opening an update.
        assert_eq!(reply(terminal.feed(b"\x1b[?2026$p")), b"\x1b[?2026;2$y");
        assert_eq!(reply(terminal.feed(b"\x1b[?6h\x1b[?6$p")), b"\x1b[?6;1$y");
        assert_eq!(
            reply(terminal.feed(b"\x1b[?1049h\x1b[?1049$p")),
            b"\x1b[?1049;1$y"
        );
        assert_eq!(reply(terminal.feed(b"\x1b[?12345$p")), b"\x1b[?12345;0$y");
    }

    #[test]
    fn decrqss_reports_sgr_scroll_region_and_refuses_unknown_requests() {
        let mut terminal = Terminal::new(10, 20, 0);
        terminal.feed(b"\x1b[1;31m\x1b[3;7r");
        let replies = |events: Vec<TerminalEvent>| {
            events
                .into_iter()
                .filter_map(|event| match event {
                    TerminalEvent::PtyWrite(bytes) => Some(bytes),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            replies(terminal.feed(b"\x1bP$qm\x1b\\")),
            vec![b"\x1bP1$r1;31m\x1b\\".to_vec()]
        );
        assert_eq!(
            replies(terminal.feed(b"\x1bP$qr\x1b\\")),
            vec![b"\x1bP1$r3;7r\x1b\\".to_vec()]
        );
        assert_eq!(
            replies(terminal.feed(b"\x1bP$qz\x1b\\")),
            vec![b"\x1bP0$r\x1b\\".to_vec()]
        );
    }

    #[test]
    fn xtgettcap_is_fragment_safe_bounded_and_reports_honest_capabilities() {
        let mut terminal = Terminal::new(2, 8, 0);
        assert!(
            terminal
                .feed(b"\x1bP+q544e;524")
                .iter()
                .all(|event| !matches!(event, TerminalEvent::PtyWrite(_)))
        );
        let events = terminal.feed(b"742;5463;756e6b6e6f776e\x1b\\");
        assert!(events.contains(&TerminalEvent::PtyWrite(
            b"\x1bP1+r544e=76766d7578;524742=38;5463\x1b\\".to_vec()
        )));

        assert!(
            terminal
                .feed(b"\x1bP+q756e6b6e6f776e\x1b\\")
                .contains(&TerminalEvent::PtyWrite(b"\x1bP0+r\x1b\\".to_vec()))
        );
        assert!(
            terminal
                .feed(b"\x1bP+p544e=6576696c\x1b\\")
                .iter()
                .all(|event| !matches!(event, TerminalEvent::PtyWrite(_)))
        );

        let oversized = vec![b'a'; DCS_MAX_BYTES + 1];
        let mut packet = b"\x1bP+q".to_vec();
        packet.extend_from_slice(&oversized);
        packet.extend_from_slice(b"\x1b\\");
        assert!(
            terminal
                .feed(&packet)
                .iter()
                .all(|event| !matches!(event, TerminalEvent::PtyWrite(_)))
        );
    }

    #[test]
    fn stalled_synchronized_update_is_flushed_after_the_vte_timeout() {
        let mut terminal = Terminal::new(2, 8, 0);
        terminal.feed(b"\x1b[?2026hburied");

        // vte buffers everything after BSU, so nothing has reached the grid yet.
        assert_eq!(terminal.extract_rows(0, 1), "");
        assert!(
            terminal.sync_flush_deadline().is_some(),
            "BSU has to arm a deadline the session can drive"
        );

        // The pane never sends ESU. Without an owner applying the deadline it would stay frozen
        // until vte's 2 MiB buffer ceiling forced the issue.
        terminal.flush_synchronized_update();
        assert_eq!(terminal.extract_rows(0, 1), "buried");
        assert!(terminal.sync_flush_deadline().is_none());

        // A well-behaved application closing its own update needs no flush and arms no deadline.
        let mut closed = Terminal::new(2, 8, 0);
        closed.feed(b"\x1b[?2026hshown\x1b[?2026l");
        assert_eq!(closed.extract_rows(0, 1), "shown");
        assert!(closed.sync_flush_deadline().is_none());
    }

    #[test]
    fn queries_buffered_in_a_synchronized_update_reply_only_once_it_is_flushed() {
        let mut terminal = Terminal::new(2, 8, 0);
        let buffered = terminal.feed(b"\x1b[?2026h\x1b[c");
        assert!(
            !buffered
                .iter()
                .any(|event| matches!(event, TerminalEvent::PtyWrite(_))),
            "a query inside the update is buffered with everything else"
        );

        let flushed = terminal.flush_synchronized_update();
        assert!(
            flushed
                .iter()
                .any(|event| matches!(event, TerminalEvent::PtyWrite(_))),
            "the flush has to deliver replies the pane is blocked waiting for"
        );
    }

    #[test]
    fn legacy_alternate_screen_modes_swap_without_saving_the_cursor() {
        // 47 leaves the alternate screen intact on the way out; 1047 clears it.
        for (mode, alt_survives) in [(47, true), (1047, false)] {
            let mut terminal = Terminal::new(2, 4, 10);
            terminal.feed(b"main");
            terminal.feed(format!("\x1b[?{mode}h").as_bytes());
            assert!(terminal.alternate_screen());
            terminal.feed(b"\x1b[1;1Halt");
            terminal.feed(format!("\x1b[?{mode}l").as_bytes());

            assert!(!terminal.alternate_screen());
            assert_eq!(terminal.extract_rows(0, 1), "main");
            // Neither variant restores a cursor, so the position carries across unchanged.
            assert_eq!(terminal.cursor(), (0, 3));

            terminal.feed(format!("\x1b[?{mode}h").as_bytes());
            assert_eq!(
                terminal.extract_rows(0, 1),
                if alt_survives { "alt" } else { "" }
            );
        }
    }

    #[test]
    fn cursor_save_mode_1048_does_not_switch_screens() {
        let mut terminal = Terminal::new(4, 8, 10);
        terminal.feed(b"\x1b[3;5H\x1b[?1048h\x1b[1;1H");
        assert_eq!(terminal.cursor(), (0, 0));
        assert!(!terminal.alternate_screen());

        terminal.feed(b"\x1b[?1048l");
        assert_eq!(terminal.cursor(), (2, 4));
        assert!(!terminal.alternate_screen());
    }

    #[test]
    fn alternate_screen_keeps_a_separate_saved_cursor_per_screen() {
        let mut terminal = Terminal::new(4, 12, 10);
        terminal.feed(b"\x1b[2;3H\x1b7");

        // The application's own DECSC inside the alternate screen writes the alternate screen's
        // slot, so the primary screen's slot survives untouched on both sides of the switch.
        terminal.feed(b"\x1b[?1049h\x1b[4;10H\x1b7\x1b[1;1H\x1b8");
        assert_eq!(terminal.cursor(), (3, 9));
        terminal.feed(b"\x1b[?1049l\x1b[1;1H\x1b8");
        assert_eq!(terminal.cursor(), (1, 2));
    }

    #[test]
    fn alternate_screen_entry_clears_the_alternate_grid_without_clearing_media_anchors() {
        let mut terminal = Terminal::new(2, 4, 10);
        terminal.feed(b"\x1b[?1049hold\x1b[?1049l");

        // A second smcup must not inherit the previous alternate-screen contents.
        let events = terminal.feed(b"\x1b[?1049h");
        assert_eq!(terminal.extract_rows(0, 1), "");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TerminalEvent::Clear { .. })),
            "a screen switch must not report an explicit clear"
        );
        assert!(events.contains(&TerminalEvent::ScreenSwap { alternate: true }));

        let events = terminal.feed(b"\x1b[2J");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TerminalEvent::Clear { alternate: true }))
        );
        let events = terminal.feed(b"\x1b[?1049l");
        assert!(events.contains(&TerminalEvent::ScreenSwap { alternate: false }));
    }

    #[test]
    fn reset_while_on_the_alternate_screen_returns_to_primary_and_restores_scrollback() {
        let mut terminal = Terminal::new(2, 4, 10);
        terminal.feed(b"\x1b[?1049h");
        terminal.feed(b"\x1bc");
        assert!(!terminal.alternate_screen());

        // A stuck alternate-screen flag silently suppresses scrollback capture for good.
        terminal.feed(b"a\r\nb\r\nc\r\nd");
        assert!(terminal.history_len() > 0);
    }

    #[test]
    fn resize_keeps_a_partial_scroll_region_and_regrows_a_full_screen_one() {
        let mut partial = Terminal::new(10, 8, 0);
        partial.feed(b"\x1b[3;7r\x1b[?6h");
        partial.resize(6, 8);
        // Origin-relative addressing reports the region that survived the resize.
        partial.feed(b"\x1b[1;1H");
        assert_eq!(partial.cursor(), (2, 0));
        partial.feed(b"\x1b[99;1H");
        assert_eq!(partial.cursor(), (5, 0));

        let mut full = Terminal::new(10, 8, 0);
        full.feed(b"\x1b[?6h");
        full.resize(20, 8);
        full.feed(b"\x1b[99;1H");
        assert_eq!(
            full.cursor(),
            (19, 0),
            "a full-screen region regrows with the screen"
        );
    }

    #[test]
    fn alternate_screen_restores_primary() {
        let mut terminal = Terminal::new(2, 4, 10);
        terminal.feed(b"main\x1b[?1049halt\x1b[?1049l");
        assert_eq!(
            terminal.cells()[0]
                .iter()
                .map(|cell| cell.ch)
                .collect::<String>(),
            "main"
        );
    }

    #[test]
    fn alternate_screen_restores_primary_cursor_before_shell_redraw() {
        let mut terminal = Terminal::new(4, 12, 10);
        terminal.feed(b"one\r\ntwo\r\nshell> ");
        let primary_cursor = terminal.cursor();

        // Full-screen applications can save the cursor for their own use. That must not replace
        // the primary-screen cursor captured by DECSET 1049.
        terminal.feed(b"\x1b[?1049h\x1b[4;10H\x1b7editor\x1b8\x1b[?1049l");
        assert_eq!(terminal.cursor(), primary_cursor);

        // Shell line editors commonly erase below while repainting the prompt. With the cursor
        // incorrectly restored to the top-left, this clears all of the primary-screen content.
        terminal.feed(b"\rprompt> \x1b[J");
        assert_eq!(terminal.extract_rows(0, 3), "one\ntwo\nprompt>");
    }

    #[test]
    fn bracketed_paste_mode_changes_are_reported() {
        let mut terminal = Terminal::new(2, 4, 10);
        let enabled = terminal.feed(b"\x1b[?2004h");
        assert!(terminal.modes().bracketed_paste);
        assert!(enabled.iter().any(|event| matches!(
            event,
            TerminalEvent::ModeChange(modes) if modes.bracketed_paste
        )));

        let disabled = terminal.feed(b"\x1b[?2004l");
        assert!(!terminal.modes().bracketed_paste);
        assert!(disabled.iter().any(|event| matches!(
            event,
            TerminalEvent::ModeChange(modes) if !modes.bracketed_paste
        )));
    }

    #[test]
    fn logical_text_joins_soft_wraps_but_preserves_hard_lines() {
        let mut wrapped = Terminal::new(2, 4, 10);
        wrapped.feed(b"abcde");
        assert_eq!(wrapped.line_wrapped(0), Some(true));
        assert_eq!(wrapped.visible_text(0), "abcde");

        let mut hard = Terminal::new(2, 4, 10);
        hard.feed(b"ab\r\ncd");
        assert_eq!(hard.line_wrapped(0), Some(false));
        assert_eq!(hard.visible_text(0), "ab\ncd");
    }

    #[test]
    fn physical_text_keeps_the_wrap_points_logical_text_joins() {
        let mut terminal = Terminal::new(2, 4, 10);
        terminal.feed(b"abcde");
        assert_eq!(terminal.line_wrapped(0), Some(true));

        // Same rows, two readings: one as the lines a command wrote, one as the lines the
        // terminal drew at this width.
        assert_eq!(terminal.latest_text(2), "abcde");
        assert_eq!(terminal.latest_text_physical(2), "abcd\ne");
        assert_eq!(terminal.extract_rows(0, 2), "abcde");
        assert_eq!(terminal.extract_rows_physical(0, 2), "abcd\ne");

        // A hard newline is a real line break in both readings.
        let mut hard = Terminal::new(2, 4, 10);
        hard.feed(b"ab\r\ncd");
        assert_eq!(hard.latest_text(2), "ab\ncd");
        assert_eq!(hard.latest_text_physical(2), "ab\ncd");
    }

    #[test]
    fn logical_text_preserves_blank_rows_and_literal_tabs() {
        let mut terminal = Terminal::new(3, 10, 10);
        terminal.feed(b"\r\n\r\na\tb");
        assert_eq!(terminal.visible_text(0), "\n\na\tb");
        assert_eq!(terminal.cells()[2][1].tab_width, Some(7));
    }

    #[test]
    fn custom_tab_stops_and_hyperlinks_are_retained() {
        let mut terminal = Terminal::new(2, 12, 10);
        terminal.feed(b"abc\x1bH\r\tb");
        assert_eq!(terminal.visible_text(0), "\tb\n");
        assert_eq!(terminal.cells()[0][0].tab_width, Some(3));

        terminal.feed(b"\r\n\x1b]8;id=link-1;https://example.test/\x1b\\x\x1b]8;;\x1b\\");
        assert_eq!(
            terminal.cells()[1][0]
                .hyperlink
                .as_ref()
                .map(|link| link.uri.as_str()),
            Some("https://example.test/")
        );
        assert_eq!(
            terminal.cells()[1][0]
                .hyperlink
                .as_ref()
                .and_then(|link| link.id.as_deref()),
            Some("link-1")
        );
    }

    /// Read the hyperlink id stamped onto a cell, if any.
    fn link_id(terminal: &Terminal, row: usize, column: usize) -> Option<String> {
        terminal.cells()[row][column]
            .hyperlink
            .as_ref()
            .and_then(|link| link.id.clone())
    }

    #[test]
    fn unlabeled_hyperlinks_receive_a_synthesized_id() {
        let mut terminal = Terminal::new(2, 12, 10);
        terminal.feed(b"\x1b]8;;https://example.test/\x1b\\x\x1b]8;;\x1b\\");

        let id = link_id(&terminal, 0, 0).expect("unlabeled link should be assigned an id");
        assert!(id.starts_with("vvmux-"), "unexpected synthesized id: {id}");
        // `Style::from` drops links whose id carries control characters or `;`, so a synthesized id
        // that cannot survive that filter would silently stop being re-emitted.
        assert!(!id.contains(';'));
        assert!(!id.chars().any(char::is_control));
    }

    #[test]
    fn separate_unlabeled_opens_are_distinct_links() {
        let mut terminal = Terminal::new(2, 12, 10);
        // Two opens of the same URI. They are separate links, so they must not merge.
        terminal.feed(b"\x1b]8;;https://example.test/\x1b\\a\x1b]8;;\x1b\\");
        terminal.feed(b"\x1b]8;;https://example.test/\x1b\\b\x1b]8;;\x1b\\");

        let first = link_id(&terminal, 0, 0).expect("first link id");
        let second = link_id(&terminal, 0, 1).expect("second link id");
        assert_ne!(first, second);
    }

    #[test]
    fn one_open_keeps_a_single_id_across_its_run() {
        let mut terminal = Terminal::new(2, 12, 10);
        // A style change mid-link must not start a new link.
        terminal.feed(b"\x1b]8;;https://example.test/\x1b\\a\x1b[1mb\x1b]8;;\x1b\\");

        let first = link_id(&terminal, 0, 0).expect("first cell id");
        let second = link_id(&terminal, 0, 1).expect("second cell id");
        assert_eq!(first, second);
        assert!(terminal.cells()[0][1].bold);
    }

    #[test]
    fn unlabeled_links_in_separate_panes_do_not_collide() {
        // Two panes are two `Terminal`s whose grids number rows and columns identically. The outer
        // presenter composites both into one grid, so identical URIs at identical coordinates must
        // still be distinguishable links.
        let mut left = Terminal::new(2, 12, 10);
        let mut right = Terminal::new(2, 12, 10);
        let sequence = b"\x1b]8;;https://example.test/\x1b\\x\x1b]8;;\x1b\\";
        left.feed(sequence);
        right.feed(sequence);

        let left_id = link_id(&left, 0, 0).expect("left pane link id");
        let right_id = link_id(&right, 0, 0).expect("right pane link id");
        assert_ne!(
            left_id, right_id,
            "panes must not hand the presenter colliding link identities"
        );
        // The URI is genuinely the same; only identity separates them.
        assert_eq!(
            left.cells()[0][0]
                .hyperlink
                .as_ref()
                .map(|l| l.uri.as_str()),
            right.cells()[0][0]
                .hyperlink
                .as_ref()
                .map(|l| l.uri.as_str())
        );
    }

    #[test]
    fn rich_attributes_combining_and_wide_cells_are_retained() {
        let mut terminal = Terminal::new(2, 12, 10);
        terminal.feed("\x1b[1;2;3;4;9;38;5;2m界e\u{301}".as_bytes());
        let wide = &terminal.cells()[0][0];
        assert!(wide.bold && wide.dim && wide.italic && wide.underline && wide.strikeout);
        assert_eq!(wide.foreground, TerminalColor::Indexed(2));
        assert!(terminal.cells()[0][1].wide_continuation);
        assert_eq!(terminal.cells()[0][2].combining, "\u{301}");

        let mut edge = Terminal::new(2, 4, 0);
        edge.feed("abc界".as_bytes());
        assert!(edge.cells()[0][3].leading_wide_spacer);
        assert_eq!(edge.line_wrapped(0), Some(true));
        assert_eq!(edge.visible_text(0), "abc界");
    }

    #[test]
    fn agent_osc_is_fragment_safe_bounded_and_clearable() {
        let sequence = b"\x1b]9;4;3;\x1b\\\x1b]2;\xe2\x9a\xa0 Action Required\x07";
        for split in 0..=sequence.len() {
            let mut terminal = Terminal::new(2, 20, 0);
            terminal.feed(&sequence[..split]);
            terminal.feed(&sequence[split..]);
            assert_eq!(terminal.agent_osc_progress(), "4;3;");
            assert_eq!(terminal.agent_osc_title(), "⚠ Action Required");
        }

        let mut terminal = Terminal::new(2, 20, 0);
        let oversized = format!("\x1b]9;{}\x07", "x".repeat(5000));
        terminal.feed(oversized.as_bytes());
        assert_eq!(terminal.agent_osc_progress(), "");
        terminal.feed(b"\x1b]9;4;0;\x07");
        assert_eq!(terminal.agent_osc_progress(), "4;0;");
        terminal.clear_agent_osc();
        assert_eq!(terminal.agent_osc_progress(), "");
    }
}
