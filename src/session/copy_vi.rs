//! Vi motions and operator state for a pane's retained copy-mode grid.

use super::{
    CopyState, MouseSelection, MouseSelectionMode, Terminal, TerminalEvent,
    normalize_mouse_selection_cell, pane_mouse_selection_after_output,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;

type Point = (isize, usize);
/// Four decimal digits bound repeated commands on the session actor.
const MAX_COUNT: usize = 9999;
/// Limit a single key's synchronous scan across a large retained grid.
const MAX_SCAN: usize = 65_536;
/// A paste in normal mode must not run thousands of synchronous grid scans on the actor.
const MAX_BATCH_KEYS: usize = 16;

pub(super) fn command_batch(bytes: &[u8]) -> Result<Option<&str>, &'static str> {
    if bytes.len() <= 1 || bytes.first() == Some(&0x1b) {
        return Ok(None);
    }
    let error = "copy mode accepts at most 16 command keys per input batch";
    if bytes.len() > MAX_BATCH_KEYS * 4 {
        return Err(error);
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(None);
    };
    let count = text.chars().count();
    if count > MAX_BATCH_KEYS {
        Err(error)
    } else {
        Ok((count > 1).then_some(text))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ViState {
    count: usize,
    yank_count: Option<usize>,
    awaiting: Option<char>,
    last_find: Option<(char, char)>,
    marks: BTreeMap<char, Point>,
    pub(super) linewise: bool,
}

impl ViState {
    fn reset_command(&mut self) {
        self.count = 0;
        self.yank_count = None;
        self.awaiting = None;
    }

    pub(super) fn adjust_marks(
        &mut self,
        events: &[TerminalEvent],
        switched: bool,
        history: usize,
    ) {
        if events.is_empty() && !switched {
            return;
        }
        self.marks.retain(|_, point| {
            let anchor = MouseSelection {
                start: *point,
                end: *point,
                mode: MouseSelectionMode::Character,
            };
            if let Some(next) =
                pane_mouse_selection_after_output(Some(anchor), events, switched, history)
            {
                *point = next.start;
                true
            } else {
                false
            }
        });
    }

    pub(super) fn label(&self) -> String {
        let mut s = String::from(" [copy");
        if self.count != 0 || self.yank_count.is_some() || self.awaiting.is_some() {
            s.push(' ');
            if let Some(n) = self.yank_count {
                if n != 1 {
                    let _ = write!(s, "{n}");
                }
                s.push('y');
            }
            if self.count != 0 {
                let _ = write!(s, "{}", self.count);
            }
            if let Some(ch) = self.awaiting {
                s.push(ch);
            }
        }
        s.push(']');
        s
    }
}

pub(super) enum ViInput {
    Handled,
    Unhandled,
    Yank(Point, Point),
}

fn step(term: &Terminal, mut p: Point, backward: bool) -> Option<Point> {
    loop {
        p = if backward {
            if p.1 == 0 {
                (p.0 - 1, term.viewport_line(p.0 - 1)?.len().checked_sub(1)?)
            } else {
                (p.0, p.1 - 1)
            }
        } else if p.1 + 1 < term.viewport_line(p.0)?.len() {
            (p.0, p.1 + 1)
        } else {
            (p.0 + 1, 0)
        };
        let cell = term.viewport_line(p.0)?.get(p.1)?;
        if !cell.wide_continuation && !cell.leading_wide_spacer {
            return Some(p);
        }
    }
}

fn class(term: &Terminal, p: Point, big: bool) -> Option<u8> {
    let ch = term.viewport_line(p.0)?.get(p.1)?.ch;
    Some(if ch.is_whitespace() {
        0
    } else if big || ch.is_alphanumeric() || ch == '_' {
        1
    } else {
        2
    })
}

/// Cross a hard newline as a word boundary, but keep soft-wrapped words together.
fn word(term: &Terminal, mut p: Point, key: char, budget: &mut usize) -> Option<Point> {
    let backwards = matches!(key, 'b' | 'B');
    let end = matches!(key, 'e' | 'E');
    let big = key.is_ascii_uppercase();
    loop {
        *budget = budget.checked_sub(1)?;
        let Some(next) = step(term, p, backwards) else {
            return Some(p);
        };
        let current = class(term, p, big)?;
        let next_class = class(term, next, big)?;
        let boundary = p.0 != next.0 && !term.line_wrapped(p.0.min(next.0)).unwrap_or(false);
        if !backwards && !end && next_class != 0 && (current != next_class || boundary) {
            return Some(next);
        }
        p = next;
        if (end || backwards) && next_class != 0 {
            let Some(after) = step(term, p, backwards) else {
                return Some(p);
            };
            if class(term, after, big)? != next_class
                || (p.0 != after.0 && !term.line_wrapped(p.0.min(after.0)).unwrap_or(false))
            {
                return Some(p);
            }
        }
    }
}

fn find_char(
    term: &Terminal,
    p: Point,
    key: char,
    ch: char,
    count: usize,
    repeat: bool,
) -> Option<Point> {
    let backwards = key.is_ascii_uppercase();
    let till = key.eq_ignore_ascii_case(&'t');
    let row = term.viewport_line(p.0)?;
    let start = if repeat && till {
        step(term, p, backwards)?
    } else {
        p
    };
    if start.0 != p.0 {
        return None;
    }
    let matches = |col: &usize| !row[*col].wide_continuation && row[*col].ch == ch;
    let col = if backwards {
        (0..start.1).rev().filter(matches).nth(count - 1)?
    } else {
        (start.1 + 1..row.len()).filter(matches).nth(count - 1)?
    };
    let found = (p.0, col);
    if till {
        step(term, found, !backwards)
    } else {
        Some(found)
    }
}

fn motion(term: &Terminal, p: Point, key: char, n: usize, explicit: bool) -> Option<Point> {
    let first = -(term.history_len() as isize);
    let last = term.rows().saturating_sub(1) as isize;
    let line = term.viewport_line(p.0)?;
    let mut dest = p;
    match key {
        '0' => dest.1 = 0,
        '^' => dest.1 = line.iter().position(|c| !c.ch.is_whitespace()).unwrap_or(0),
        '$' => {
            dest.1 = line
                .iter()
                .rposition(|c| !c.ch.is_whitespace() && !c.wide_continuation)
                .unwrap_or(0);
        }
        'g' | 'G' => {
            dest.0 = if explicit {
                (first + n as isize - 1).min(last)
            } else if key == 'g' {
                first
            } else {
                last
            }
        }
        'j' => dest.0 = (p.0 + n as isize).min(last),
        'k' => dest.0 = (p.0 - n as isize).max(first),
        'h' | 'l' => {
            for _ in 0..n {
                let Some(next) = step(term, dest, key == 'h').filter(|q| q.0 == p.0) else {
                    break;
                };
                dest = next;
            }
        }
        'w' | 'W' | 'e' | 'E' | 'b' | 'B' => {
            let mut budget = MAX_SCAN;
            for _ in 0..n {
                let next = word(term, dest, key, &mut budget)?;
                if next == dest {
                    break;
                }
                dest = next;
            }
        }
        '{' | '}' => {
            let delta = if key == '{' { -1 } else { 1 };
            let mut budget = MAX_SCAN;
            for _ in 0..n {
                let current = term.viewport_line(dest.0)?;
                budget = budget.checked_sub(current.len())?;
                let mut crossed_text = current.iter().any(|cell| !cell.ch.is_whitespace());
                loop {
                    let next = (dest.0 + delta).clamp(first, last);
                    if next == dest.0 {
                        break;
                    }
                    dest.0 = next;
                    let row = term.viewport_line(next)?;
                    budget = budget.checked_sub(row.len())?;
                    let blank = row.iter().all(|c| c.ch.is_whitespace());
                    if blank && crossed_text {
                        break;
                    }
                    crossed_text |= !blank;
                }
            }
            dest.1 = 0;
        }
        _ => return None,
    }
    Some(normalize_mouse_selection_cell(term, dest))
}

impl CopyState {
    pub(super) fn selection(&self, term: &Terminal) -> (Point, Point) {
        let end = (self.row as isize - self.offset as isize, self.column);
        let start = self.selection_start.unwrap_or((-(self.offset as isize), 0));
        if self.vi.linewise {
            (
                (start.0.min(end.0), 0),
                (start.0.max(end.0), term.cols().saturating_sub(1)),
            )
        } else {
            (start, end)
        }
    }

    fn move_to(&mut self, term: &Terminal, p: Point) {
        let last = term.rows().saturating_sub(1) as isize;
        let line = p.0.clamp(-(term.history_len() as isize), last);
        let screen = line + self.offset as isize;
        if screen < 0 {
            self.offset = (-line) as usize;
        }
        if screen > last {
            self.offset = (last - line) as usize;
        }
        self.row = (line + self.offset as isize) as usize;
        self.column =
            normalize_mouse_selection_cell(term, (line, p.1.min(term.cols().saturating_sub(1)))).1;
    }

    pub(super) fn vi_key(&mut self, term: &Terminal, key: char) -> ViInput {
        let from = normalize_mouse_selection_cell(
            term,
            (
                (self.row as isize - self.offset as isize).clamp(
                    -(term.history_len() as isize),
                    term.rows().saturating_sub(1) as isize,
                ),
                self.column.min(term.cols().saturating_sub(1)),
            ),
        );
        let awaiting = self.vi.awaiting.take();
        let pending = awaiting.is_some() || self.vi.count != 0 || self.vi.yank_count.is_some();
        if key == '\u{1b}' && pending {
            self.vi.reset_command();
            return ViInput::Handled;
        }
        if awaiting.is_none() && key.is_ascii_digit() && (key != '0' || self.vi.count != 0) {
            self.vi.count = (self.vi.count * 10 + usize::from(key as u8 - b'0')).min(MAX_COUNT);
            return ViInput::Handled;
        }
        let explicit = self.vi.count != 0;
        let n = (self.vi.count.max(1) * self.vi.yank_count.unwrap_or(1)).min(MAX_COUNT);
        let op = self.vi.yank_count.is_some();
        let mut exclusive = false;
        let mut linewise = false;
        let target = match (awaiting, key) {
            (Some('m'), mark) => {
                if mark.is_ascii_lowercase() {
                    self.vi.marks.insert(mark, from);
                }
                None
            }
            (Some(prefix @ ('\'' | '`')), mark) => self
                .vi
                .marks
                .get(&mark)
                .copied()
                .map(|(line, col)| (line, if prefix == '\'' { 0 } else { col })),
            (Some(prefix @ ('f' | 'F' | 't' | 'T')), ch) => {
                self.vi.last_find = Some((prefix, ch));
                find_char(term, from, prefix, ch, n, false)
            }
            (Some('g'), 'g') => {
                linewise = op;
                motion(term, from, 'g', n, explicit)
            }
            (Some(_), _) => None,
            (None, 'y') if !op && self.selection_start.is_some() => {
                let (a, b) = self.selection(term);
                return ViInput::Yank(a, b);
            }
            (None, 'y') if op => {
                return ViInput::Yank(
                    (from.0, 0),
                    (
                        (from.0 + n as isize - 1).min(term.rows() as isize - 1),
                        term.cols() - 1,
                    ),
                );
            }
            (None, 'y') => {
                self.vi.yank_count = Some(n);
                self.vi.count = 0;
                return ViInput::Handled;
            }
            (None, ch @ ('m' | '\'' | '`' | 'g' | 'f' | 'F' | 't' | 'T')) => {
                self.vi.awaiting = Some(ch);
                return ViInput::Handled;
            }
            (None, 'v' | 'V') => {
                self.selection_start = Some(from);
                self.vi.linewise = key == 'V';
                None
            }
            (None, ';' | ',') => self.vi.last_find.and_then(|(kind, ch)| {
                let direction = if key != ',' {
                    kind
                } else if kind.is_ascii_uppercase() {
                    kind.to_ascii_lowercase()
                } else {
                    kind.to_ascii_uppercase()
                };
                find_char(term, from, direction, ch, n, true)
            }),
            (
                None,
                ch @ ('w' | 'W' | 'e' | 'E' | 'b' | 'B' | '0' | '^' | '$' | 'h' | 'j' | 'k' | 'l'
                | 'G' | '{' | '}'),
            ) => {
                exclusive = matches!(
                    ch,
                    'w' | 'W' | 'b' | 'B' | '{' | '}' | 'h' | 'l' | '0' | '^'
                );
                linewise = matches!(ch, 'j' | 'k' | 'G');
                motion(term, from, ch, n, explicit)
            }
            _ => {
                self.vi.reset_command();
                return ViInput::Unhandled;
            }
        };
        self.vi.reset_command();
        if let Some(to) = target {
            if op {
                let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
                if linewise {
                    return ViInput::Yank((lo.0, 0), (hi.0, term.cols() - 1));
                }
                if exclusive && hi == lo {
                    return ViInput::Handled;
                }
                let hi = if exclusive {
                    step(term, hi, true).unwrap_or(lo)
                } else {
                    hi
                };
                return ViInput::Yank(lo, hi);
            }
            self.move_to(term, to);
        }
        ViInput::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::extract_selection;

    fn keys(term: &Terminal, copy: &mut CopyState, keys: &str) -> Option<Vec<u8>> {
        for key in keys.chars() {
            match copy.vi_key(term, key) {
                ViInput::Yank(first, last) => return Some(extract_selection(term, first, last)),
                ViInput::Handled => {}
                ViInput::Unhandled => panic!("unexpected key {key:?}"),
            }
        }
        None
    }

    #[test]
    fn motions_and_operators_yank_terminal_cells() {
        let mut term = Terminal::new(4, 16, 10);
        term.feed("abc 猫! end\r\nsecond\r\nthird\r\nfourth".as_bytes());
        for (input, expected) in [
            ("yy", "abc 猫! end"),
            ("3yy", "abc 猫! end\nsecond\nthird"),
            ("yw", "abc"),
            ("wyf!", "猫!"),
            ("yf猫", "abc 猫"),
            ("wlyb", "猫"),
            ("2jVky", "second\nthird"),
            ("jyG", "second\nthird\nfourth"),
            ("gg3lmaG`ay$", " 猫! end"),
            ("Ggg2lmaG'ayw", "abc"),
        ] {
            assert_eq!(
                keys(&term, &mut CopyState::default(), input).as_deref(),
                Some(expected.as_bytes()),
                "{input:?}"
            );
        }
    }

    #[test]
    fn coalesced_commands_are_bounded_before_dispatch() {
        assert_eq!(command_batch(b"3yy").unwrap(), Some("3yy"));
        assert_eq!(command_batch("f猫".as_bytes()).unwrap(), Some("f猫"));
        assert_eq!(command_batch(b"\x1b[A").unwrap(), None);
        assert_eq!(
            command_batch(&[b'w'; MAX_BATCH_KEYS])
                .unwrap()
                .unwrap()
                .len(),
            MAX_BATCH_KEYS
        );
        let refusal = command_batch(&[b'w'; MAX_BATCH_KEYS + 1]).unwrap_err();
        assert_eq!(command_batch(&vec![b'w'; 65_536]).unwrap_err(), refusal);
    }

    #[test]
    fn pending_commands_cancel_and_saturate() {
        let term = Terminal::new(4, 16, 10);
        for (input, label) in [
            ("3y", " [copy 3y]"),
            ("yf", " [copy yf]"),
            ("g", " [copy g]"),
            ("9999999999", " [copy 9999]"),
        ] {
            let mut copy = CopyState::default();
            keys(&term, &mut copy, input);
            assert_eq!(copy.vi.label(), label);
            keys(&term, &mut copy, "\x1b");
            assert_eq!(copy.vi.label(), " [copy]");
            assert!(matches!(copy.vi_key(&term, '\x1b'), ViInput::Unhandled));
        }
    }

    #[test]
    fn find_repeats_paragraphs_and_scrollback_marks() {
        let mut term = Terminal::new(3, 12, 10);
        term.feed(b"a.x.x.x\r\n\r\nthree\r\nfour");
        assert_eq!(term.history_len(), 1);
        let mut copy = CopyState::default();
        keys(&term, &mut copy, "ggtx;");
        assert_eq!((copy.offset, copy.column), (1, 3));
        keys(&term, &mut copy, ";,");
        assert_eq!(copy.column, 3); // Reverse till passes the adjacent x to move again.
        keys(&term, &mut copy, "0fxma}");
        assert_eq!(copy.row as isize - copy.offset as isize, 0);
        keys(&term, &mut copy, "G`a");
        assert_eq!(
            (copy.row as isize - copy.offset as isize, copy.column),
            (-1, 2)
        );
    }

    #[test]
    fn marks_follow_new_output_into_history_and_expire_on_eviction() {
        let mut term = Terminal::new(2, 12, 1);
        term.feed(b"marked\r\nsecond");
        let mut copy = CopyState::default();
        keys(&term, &mut copy, "2lma");
        let output = term.feed(b"\r\nthird");
        copy.vi.adjust_marks(&output, false, term.history_len());
        keys(&term, &mut copy, "G`a");
        assert_eq!(
            (copy.row as isize - copy.offset as isize, copy.column),
            (-1, 2)
        );
        let output = term.feed(b"\r\nfourth");
        copy.vi.adjust_marks(&output, false, term.history_len());
        assert!(copy.vi.marks.is_empty());
    }

    #[test]
    fn word_motions_span_soft_wraps_and_preserve_wide_cells() {
        let mut term = Terminal::new(3, 6, 0);
        term.feed("abcd猫ef gh".as_bytes());
        let mut copy = CopyState::default();
        assert_eq!(keys(&term, &mut copy, "ye").unwrap(), "abcd猫ef".as_bytes());
        assert!(term.line_wrapped(0).unwrap());
    }
}
