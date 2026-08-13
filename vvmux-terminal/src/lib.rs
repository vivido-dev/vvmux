//! Pane-oriented terminal emulation and platform PTY support for vvmux.

#![cfg_attr(not(unix), allow(dead_code))]

use std::cmp;
use std::collections::VecDeque;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};

use unicode_width::UnicodeWidthChar;
use vte::ansi::{
    Attr, ClearMode, Color, Handler, Hyperlink, KeyboardModes, KeyboardModesApplyBehavior,
    LineClearMode, NamedColor, PrivateMode, Processor, TabulationClearMode,
};

const KEYBOARD_MODE_STACK_MAX_DEPTH: usize = 4096;
const AGENT_OSC_MAX_CHARS: usize = 256;

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
    },
    GridScroll(i32),
    Clear,
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
    saved_cursor: (usize, usize),
    primary_cursor_before_alternate: Option<(usize, usize)>,
    scroll_top: usize,
    scroll_bottom: usize,
    template: Cell,
    processor: Processor,
    events: Vec<TerminalEvent>,
    modes: TerminalModes,
    title: Option<String>,
    alternate_screen: bool,
    tab_stops: Vec<bool>,
    tab_stops_customized: bool,
    marker_scanner: VividMarkerScanner,
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
            saved_cursor: (0, 0),
            primary_cursor_before_alternate: None,
            scroll_top: 0,
            scroll_bottom: rows,
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
            agent_osc: AgentOscTracker::default(),
            keyboard_mode_stack: Vec::new(),
            inactive_keyboard_mode_stack: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TerminalEvent> {
        self.agent_osc.observe(bytes);
        let chunks = self.marker_scanner.push(bytes);
        self.process_chunks(chunks, !bytes.is_empty())
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
        let chunks = self.marker_scanner.finish();
        let has_bytes = chunks
            .iter()
            .any(|chunk| matches!(chunk, VividChunk::Bytes(bytes) if !bytes.is_empty()));
        self.process_chunks(chunks, has_bytes)
    }

    fn process_chunks(&mut self, chunks: Vec<VividChunk>, damage: bool) -> Vec<TerminalEvent> {
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
                    });
                }
            }
        }
        self.processor = processor;
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
        if let Some((row, column)) = &mut self.primary_cursor_before_alternate {
            *row = (*row).min(rows - 1);
            *column = (*column).min(cols - 1);
        }
        self.scroll_top = 0;
        self.scroll_bottom = rows;
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

    pub fn alternate_screen(&self) -> bool {
        self.alternate_screen
    }

    pub fn visible_text(&self, display_offset: usize) -> String {
        let rows = self.rows;
        let start = -(display_offset.min(self.history.len()) as isize);
        self.extract_rows(start, rows)
    }

    pub fn latest_text(&self, rows: usize) -> String {
        let available = self.history.len().saturating_add(self.rows);
        let count = rows.min(available);
        let start_absolute = available.saturating_sub(count);
        let start = start_absolute as isize - self.history.len() as isize;
        self.extract_rows(start, count)
    }

    pub fn extract_rows(&self, start_line: isize, row_count: usize) -> String {
        let mut text = String::new();
        let mut previous_wrapped = false;
        for index in 0..row_count {
            let line = start_line.saturating_add(index as isize);
            let Some(cells) = self.viewport_line(line) else {
                continue;
            };
            if index > 0 && !previous_wrapped {
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

    fn scroll_region_up(&mut self, count: usize) {
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top));
        for _ in 0..count {
            let removed = self.grid.remove(self.scroll_top);
            let removed_wrapped = self.grid_wrapped.remove(self.scroll_top);
            if self.scroll_top == 0 && self.scroll_bottom == self.rows && !self.alternate_screen {
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
            self.events.push(TerminalEvent::GridScroll(count as i32));
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
            self.events.push(TerminalEvent::GridScroll(-(count as i32)));
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
        self.cursor_row = line.max(0) as usize;
        self.cursor_row = self.cursor_row.min(self.rows - 1);
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
                self.events.push(TerminalEvent::Clear);
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
                    self.events.push(TerminalEvent::Clear);
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
        let old_top = self.scroll_top;
        self.scroll_top = self.cursor_row;
        self.scroll_region_down(rows);
        self.scroll_top = old_top;
    }

    fn delete_lines(&mut self, rows: usize) {
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
        self.saved_cursor = (self.cursor_row, self.cursor_col);
    }

    fn restore_cursor_position(&mut self) {
        self.cursor_row = self.saved_cursor.0.min(self.rows - 1);
        self.cursor_col = self.saved_cursor.1.min(self.cols - 1);
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
        self.primary_cursor_before_alternate = None;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows;
        self.template = Cell::default();
        self.tab_stops = default_tab_stops(self.cols, 8);
        self.tab_stops_customized = false;
        self.modes = TerminalModes {
            cursor_visible: true,
            ..TerminalModes::default()
        };
        self.keyboard_mode_stack.clear();
        self.inactive_keyboard_mode_stack.clear();
        self.events.push(TerminalEvent::Clear);
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

    fn identify_terminal(&mut self, _: Option<char>) {
        self.events
            .push(TerminalEvent::PtyWrite(b"\x1b[?62;4c".to_vec()));
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
            1049 if enabled != self.alternate_screen => {
                let next_cursor = if enabled {
                    self.primary_cursor_before_alternate = Some((self.cursor_row, self.cursor_col));
                    (0, 0)
                } else {
                    self.primary_cursor_before_alternate
                        .take()
                        .unwrap_or((0, 0))
                };
                mem::swap(&mut self.grid, &mut self.alternate_grid);
                mem::swap(&mut self.grid_wrapped, &mut self.alternate_grid_wrapped);
                mem::swap(
                    &mut self.keyboard_mode_stack,
                    &mut self.inactive_keyboard_mode_stack,
                );
                self.modes.keyboard_flags = self
                    .keyboard_mode_stack
                    .last()
                    .copied()
                    .unwrap_or(KeyboardModes::NO_MODE)
                    .bits();
                self.alternate_screen = enabled;
                self.cursor_row = next_cursor.0;
                self.cursor_col = next_cursor.1;
                self.damage();
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
        assert!(events.contains(&TerminalEvent::Clear));
        assert_eq!(terminal.history_len(), 0);

        // A bare scrollback purge with visible viewport content is not a clear.
        let mut populated = Terminal::new(3, 10, 10);
        populated.feed(b"one\r\ntwo\r\nthree\r\nfour");
        let events = populated.feed(b"\x1b[3J");
        assert!(!events.contains(&TerminalEvent::Clear));
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
