//! Pane-oriented terminal emulation and platform PTY support for vvmux.

#![cfg_attr(not(unix), allow(dead_code))]

use std::cmp;
use std::collections::VecDeque;
use std::mem;

use unicode_width::UnicodeWidthChar;
use vte::ansi::{
    Attr, ClearMode, Color, Handler, LineClearMode, NamedColor, PrivateMode, Processor,
};

pub mod pty;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub combining: String,
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub wide_continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            combining: String::new(),
            foreground: TerminalColor::Default,
            background: TerminalColor::Default,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            wide_continuation: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalModes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_clicks: bool,
    pub mouse_motion: bool,
    pub sgr_mouse: bool,
    pub focus_reporting: bool,
    pub cursor_visible: bool,
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
    history: VecDeque<Vec<Cell>>,
    history_limit: usize,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: (usize, usize),
    scroll_top: usize,
    scroll_bottom: usize,
    template: Cell,
    processor: Processor,
    events: Vec<TerminalEvent>,
    modes: TerminalModes,
    title: Option<String>,
    alternate_screen: bool,
    marker_scanner: VividMarkerScanner,
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
            history: VecDeque::new(),
            history_limit,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: (0, 0),
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
            marker_scanner: VividMarkerScanner::default(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TerminalEvent> {
        let chunks = self.marker_scanner.push(bytes);
        self.process_chunks(chunks, !bytes.is_empty())
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
        self.rows = rows;
        self.cols = cols;
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
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
            ..self.template.clone()
        }
    }

    fn scroll_region_up(&mut self, count: usize) {
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top));
        for _ in 0..count {
            let removed = self.grid.remove(self.scroll_top);
            if self.scroll_top == 0 && self.scroll_bottom == self.rows && !self.alternate_screen {
                self.history.push_back(removed);
                while self.history.len() > self.history_limit {
                    self.history.pop_front();
                }
            }
            self.grid
                .insert(self.scroll_bottom - 1, vec![self.blank(); self.cols]);
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
            self.grid
                .insert(self.scroll_top, vec![self.blank(); self.cols]);
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
        let blank = self.blank();
        for cell in &mut self.grid[row][start.min(self.cols)..end.min(self.cols)] {
            *cell = blank.clone();
        }
        self.damage();
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
            self.cursor_col = 0;
            self.line_feed();
        }
        if width == 2 && self.cursor_col + 1 >= self.cols {
            self.cursor_col = 0;
            self.line_feed();
        }
        let mut cell = self.template.clone();
        cell.ch = c;
        cell.combining.clear();
        cell.wide_continuation = false;
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
            self.cursor_col = ((self.cursor_col / 8 + 1) * 8).min(self.cols - 1);
        }
    }

    fn insert_blank(&mut self, count: usize) {
        let count = count.min(self.cols - self.cursor_col);
        let blank = self.blank();
        let row = &mut self.grid[self.cursor_row];
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
                self.events.push(TerminalEvent::Clear);
                self.damage();
            }
            ClearMode::Saved => {
                self.history.clear();
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
        self.history.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows;
        self.template = Cell::default();
        self.modes = TerminalModes {
            cursor_visible: true,
            ..TerminalModes::default()
        };
        self.events.push(TerminalEvent::Clear);
        self.damage();
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        match attr {
            Attr::Reset => self.template = Cell::default(),
            Attr::Bold => self.template.bold = true,
            Attr::CancelBold | Attr::CancelBoldDim => self.template.bold = false,
            Attr::Italic => self.template.italic = true,
            Attr::CancelItalic => self.template.italic = false,
            Attr::Underline
            | Attr::DoubleUnderline
            | Attr::Undercurl
            | Attr::DottedUnderline
            | Attr::DashedUnderline => self.template.underline = true,
            Attr::CancelUnderline => self.template.underline = false,
            Attr::Reverse => self.template.inverse = true,
            Attr::CancelReverse => self.template.inverse = false,
            Attr::Foreground(color) => self.template.foreground = convert_color(color, true),
            Attr::Background(color) => self.template.background = convert_color(color, false),
            _ => {}
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
}

impl Terminal {
    fn update_private_mode(&mut self, mode: PrivateMode, enabled: bool) {
        match mode.raw() {
            1 => self.modes.application_cursor = enabled,
            2004 => self.modes.bracketed_paste = enabled,
            1000 => self.modes.mouse_clicks = enabled,
            1002 | 1003 => {
                self.modes.mouse_motion = enabled;
            }
            1006 => self.modes.sgr_mouse = enabled,
            1004 => self.modes.focus_reporting = enabled,
            25 => self.modes.cursor_visible = enabled,
            1049 if enabled != self.alternate_screen => {
                mem::swap(&mut self.grid, &mut self.alternate_grid);
                self.alternate_screen = enabled;
                self.cursor_row = 0;
                self.cursor_col = 0;
                self.damage();
            }
            _ => {}
        }
        self.events.push(TerminalEvent::ModeChange(self.modes));
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
    prefix: b"\x1b_VIVID;2;A;",
    terminator: b"\x1b\\",
    payload_skip: 2,
};

#[cfg(windows)]
const CONPTY_ENVELOPE: MarkerEnvelope = MarkerEnvelope {
    prefix: b"VIVID;2;A;",
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
        && fields.next() == Some("2")
        && fields.next() == Some("A")
        && fields
            .next()
            .is_some_and(|tag| tag.len() == 22 && tag.bytes().all(is_base64url))
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
    fn marker_is_consumed_across_boundaries() {
        let marker =
            b"\x1b_VIVID;2;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA\x1b\\";
        for split in 0..=marker.len() {
            let mut terminal = Terminal::new(2, 20, 0);
            let mut events = terminal.feed(&marker[..split]);
            events.extend(terminal.feed(&marker[split..]));
            assert!(
                events.contains(&TerminalEvent::VividMarker {
                    marker:
                        "VIVID;2;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA"
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
            b"VIVID;2;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA;VIVID-END";
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
        let malformed = b"\x1b_VIVID;2;A;short;0000000000000007;bad\x1b\\";
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
            b"\x1b_VIVID;2;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA\x1b\\";
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
        let candidate = b"prefix\x1b_VIVID;2;A;partial!suffix";
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
        let malformed = b"VIVID;2;A;short;0000000000000007;bad;VIVID-END";
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
            b"\x1b_VIVID;2;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA\x1b\\",
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
}
