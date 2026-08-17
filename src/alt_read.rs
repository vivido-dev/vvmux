//! Bounded alternate-screen transcript harvesting for idle full-screen agents.
//!
//! Adapted from herdr's alternate-screen reader and history merge (Apache-2.0, commit
//! `6c6ddcd49384d6ea9f0ee2e63bf7b2643dfd5bcf`). See `agent/PROVENANCE.md`.

use std::time::{Duration, Instant};

use vvmux_terminal::pty::PtyInput;
use vvmux_terminal::{Terminal, TerminalModes};

use crate::agent_drive::{WHEEL_DOWN, WHEEL_UP, encode_sgr_mouse};

pub(crate) const MAX_ALT_SCREEN_READS: usize = 8;
pub(crate) const MAX_READ_LINES: usize = 1000;

const STEP_SETTLE: Duration = Duration::from_millis(120);
const MAX_DURATION: Duration = Duration::from_secs(15);
const MAX_RESTORE_DURATION: Duration = Duration::from_secs(5);
const MAX_UNALIGNED_CHECKS: u8 = 4;
const WHEEL_STEP_EVENTS: usize = 3;
const MIN_ALIGNMENT_RATIO_PERCENT: usize = 30;
const SIMILAR_VIEWPORT_RATIO_PERCENT: usize = 70;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScreenRow {
    text: String,
    soft_wrapped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScreenSnapshot {
    cols: usize,
    rows: Vec<ScreenRow>,
}

impl ScreenSnapshot {
    fn capture(terminal: &Terminal) -> Self {
        let rows = (0..terminal.rows())
            .map(|index| {
                let mut text = String::new();
                if let Some(cells) = terminal.viewport_line(index as isize) {
                    for cell in cells.iter().take(terminal.cols()) {
                        if cell.wide_continuation {
                            continue;
                        }
                        text.push(cell.ch);
                        text.push_str(&cell.combining);
                    }
                }
                ScreenRow {
                    text,
                    soft_wrapped: terminal.line_wrapped(index as isize).unwrap_or(false),
                }
            })
            .collect();
        Self {
            cols: terminal.cols(),
            rows,
        }
    }

    fn similar_text(&self, other: &Self) -> bool {
        if self.cols != other.cols || self.rows.len() != other.rows.len() {
            return false;
        }
        let left = row_identities(&self.rows);
        let right = row_identities(&other.rows);
        let comparable = left
            .iter()
            .zip(&right)
            .filter(|(left, right)| !left.is_empty() || !right.is_empty())
            .count();
        if comparable == 0 {
            return true;
        }
        let matches = left
            .iter()
            .zip(&right)
            .filter(|(left, right)| left == right && (!left.is_empty() || !right.is_empty()))
            .count();
        matches.saturating_mul(100) >= comparable.saturating_mul(SIMILAR_VIEWPORT_RATIO_PERCENT)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpwardMerge {
    Advanced,
    Unchanged,
    Unaligned,
}

fn merge_scrolled_up(
    history: &mut Vec<ScreenRow>,
    previous: &ScreenSnapshot,
    next: &ScreenSnapshot,
) -> UpwardMerge {
    if previous.cols != next.cols || previous.rows.len() != next.rows.len() {
        return UpwardMerge::Unaligned;
    }
    let previous_text = row_identities(&previous.rows);
    let next_text = row_identities(&next.rows);
    if previous_text == next_text {
        return UpwardMerge::Unchanged;
    }
    let Some(shift) = best_upward_shift(&previous_text, &next_text) else {
        return UpwardMerge::Unaligned;
    };
    let Some(boundary) = (0..previous_text.len().saturating_sub(shift)).find_map(|index| {
        let next_index = index + shift;
        (!previous_text[index].is_empty() && previous_text[index] == next_text[next_index])
            .then_some(next_index)
    }) else {
        return UpwardMerge::Unaligned;
    };
    let added: Vec<_> = next.rows[..boundary]
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            next_text[*index].is_empty() || previous_text.get(*index) != Some(&next_text[*index])
        })
        .map(|(_, row)| row.clone())
        .collect();
    if added.is_empty() {
        return UpwardMerge::Unaligned;
    }
    history.splice(0..0, added);
    UpwardMerge::Advanced
}

fn best_upward_shift(previous: &[String], next: &[String]) -> Option<usize> {
    let mut best = None;
    for shift in 1..previous.len() {
        let overlap = previous.len() - shift;
        let mut comparable = 0usize;
        let mut matches = 0usize;
        for index in 0..overlap {
            let before = &previous[index];
            let after = &next[index + shift];
            if before.is_empty() || after.is_empty() {
                continue;
            }
            comparable += 1;
            if before == after {
                matches += 1;
            }
        }
        if comparable == 0
            || matches.saturating_mul(100) < comparable.saturating_mul(MIN_ALIGNMENT_RATIO_PERCENT)
        {
            continue;
        }
        if best.is_none_or(|(_, best_matches, best_comparable)| {
            matches > best_matches || (matches == best_matches && comparable > best_comparable)
        }) {
            best = Some((shift, matches, comparable));
        }
    }
    best.map(|(shift, _, _)| shift)
}

fn row_identities(rows: &[ScreenRow]) -> Vec<String> {
    rows.iter()
        .map(|row| row.text.trim_end().to_owned())
        .collect()
}

fn snapshot_text(rows: &[ScreenRow], lines: usize) -> String {
    let start = rows.len().saturating_sub(lines);
    let mut logical = Vec::new();
    let mut current = String::new();
    for row in &rows[start..] {
        current.push_str(row.text.trim_end());
        if !row.soft_wrapped {
            logical.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        logical.push(current);
    }
    while logical.last().is_some_and(|line| line.trim().is_empty()) {
        logical.pop();
    }
    let text = logical.join("\n");
    if text.is_empty() {
        text
    } else {
        format!("{text}\n")
    }
}

#[derive(Debug)]
enum Phase {
    SettleInitial { checks: u8 },
    ProbeBottom,
    RestoreProbe { stable_checks: u8 },
    Harvest { unaligned_checks: u8 },
    Restore { stable_checks: u8 },
}

pub(crate) struct ReadResult {
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

pub(crate) enum PollOutcome {
    Pending(PendingAltRead),
    Success(ReadResult),
    Fallback,
}

pub(crate) struct PendingAltRead {
    lines: usize,
    initial: ScreenSnapshot,
    previous: ScreenSnapshot,
    history: Vec<ScreenRow>,
    phase: Phase,
    next_poll_at: Instant,
    started_at: Instant,
    restore_started_at: Option<Instant>,
    upward_events: usize,
    reached_top: bool,
    valid: bool,
}

impl PendingAltRead {
    pub(crate) fn start(terminal: &Terminal, lines: usize, now: Instant) -> Self {
        let initial = ScreenSnapshot::capture(terminal);
        Self {
            lines,
            previous: initial.clone(),
            history: initial.rows.clone(),
            initial,
            phase: Phase::SettleInitial { checks: 0 },
            next_poll_at: now + STEP_SETTLE,
            started_at: now,
            restore_started_at: None,
            upward_events: 0,
            reached_top: false,
            valid: true,
        }
    }

    pub(crate) fn next_deadline(&self) -> Instant {
        self.next_poll_at
    }

    pub(crate) fn poll(
        mut self,
        terminal: &Terminal,
        input: &PtyInput,
        agent_idle: bool,
        cell_size: (u16, u16),
        now: Instant,
    ) -> PollOutcome {
        if now < self.next_poll_at {
            return PollOutcome::Pending(self);
        }
        let snapshot = ScreenSnapshot::capture(terminal);
        let compatible = terminal.alternate_screen()
            && snapshot.cols == self.initial.cols
            && snapshot.rows.len() == self.initial.rows.len();
        if !compatible {
            return PollOutcome::Fallback;
        }
        if !agent_idle && self.valid {
            self.valid = false;
            return self.start_restore(terminal.modes(), input, cell_size, now);
        }
        if self
            .restore_started_at
            .is_some_and(|started| now.duration_since(started) >= MAX_RESTORE_DURATION)
        {
            return PollOutcome::Fallback;
        }
        if now.duration_since(self.started_at) >= MAX_DURATION
            && !matches!(
                self.phase,
                Phase::Restore { .. } | Phase::RestoreProbe { .. }
            )
        {
            self.valid = false;
            return self.start_restore(terminal.modes(), input, cell_size, now);
        }

        match self.phase {
            Phase::SettleInitial { checks } => {
                if snapshot.similar_text(&self.initial) {
                    if checks >= 1 {
                        if send_wheel(
                            input,
                            terminal.modes(),
                            &snapshot,
                            WHEEL_DOWN,
                            WHEEL_STEP_EVENTS,
                            cell_size,
                        )
                        .is_err()
                        {
                            return PollOutcome::Fallback;
                        }
                        self.phase = Phase::ProbeBottom;
                    } else {
                        self.phase = Phase::SettleInitial { checks: checks + 1 };
                    }
                } else {
                    self.initial = snapshot.clone();
                    self.previous = snapshot.clone();
                    self.history = snapshot.rows;
                    self.phase = Phase::SettleInitial { checks: 0 };
                }
                self.next_poll_at = now + STEP_SETTLE;
                PollOutcome::Pending(self)
            }
            Phase::ProbeBottom => {
                if snapshot.similar_text(&self.initial) {
                    self.start_harvest(terminal.modes(), input, cell_size, now)
                } else if send_wheel(
                    input,
                    terminal.modes(),
                    &snapshot,
                    WHEEL_UP,
                    WHEEL_STEP_EVENTS,
                    cell_size,
                )
                .is_ok()
                {
                    self.valid = false;
                    self.phase = Phase::RestoreProbe { stable_checks: 0 };
                    self.restore_started_at = Some(now);
                    self.next_poll_at = now + STEP_SETTLE;
                    PollOutcome::Pending(self)
                } else {
                    PollOutcome::Fallback
                }
            }
            Phase::RestoreProbe { stable_checks } => {
                if snapshot.similar_text(&self.initial) {
                    if stable_checks >= 1 {
                        PollOutcome::Fallback
                    } else {
                        self.phase = Phase::RestoreProbe {
                            stable_checks: stable_checks + 1,
                        };
                        self.next_poll_at = now + STEP_SETTLE;
                        PollOutcome::Pending(self)
                    }
                } else {
                    self.next_poll_at = now + STEP_SETTLE;
                    PollOutcome::Pending(self)
                }
            }
            Phase::Harvest { unaligned_checks } => {
                match merge_scrolled_up(&mut self.history, &self.previous, &snapshot) {
                    UpwardMerge::Advanced => {
                        self.previous = snapshot;
                        if self.history.len() >= self.lines {
                            self.start_restore(terminal.modes(), input, cell_size, now)
                        } else {
                            self.start_harvest(terminal.modes(), input, cell_size, now)
                        }
                    }
                    UpwardMerge::Unchanged => {
                        self.reached_top = true;
                        self.start_restore(terminal.modes(), input, cell_size, now)
                    }
                    UpwardMerge::Unaligned if unaligned_checks + 1 < MAX_UNALIGNED_CHECKS => {
                        self.phase = Phase::Harvest {
                            unaligned_checks: unaligned_checks + 1,
                        };
                        self.next_poll_at = now + STEP_SETTLE;
                        PollOutcome::Pending(self)
                    }
                    UpwardMerge::Unaligned => {
                        self.valid = false;
                        self.start_restore(terminal.modes(), input, cell_size, now)
                    }
                }
            }
            Phase::Restore { stable_checks } => {
                if snapshot.similar_text(&self.initial) {
                    if stable_checks >= 1 {
                        if self.valid {
                            let truncated = !self.reached_top || self.history.len() > self.lines;
                            PollOutcome::Success(read_result(&self.history, self.lines, truncated))
                        } else {
                            PollOutcome::Fallback
                        }
                    } else {
                        self.phase = Phase::Restore {
                            stable_checks: stable_checks + 1,
                        };
                        self.next_poll_at = now + STEP_SETTLE;
                        PollOutcome::Pending(self)
                    }
                } else if send_wheel(
                    input,
                    terminal.modes(),
                    &snapshot,
                    WHEEL_DOWN,
                    snapshot.rows.len().saturating_div(2).max(1),
                    cell_size,
                )
                .is_ok()
                {
                    self.phase = Phase::Restore { stable_checks: 0 };
                    self.next_poll_at = now + STEP_SETTLE;
                    PollOutcome::Pending(self)
                } else {
                    PollOutcome::Fallback
                }
            }
        }
    }

    fn start_harvest(
        mut self,
        modes: TerminalModes,
        input: &PtyInput,
        cell_size: (u16, u16),
        now: Instant,
    ) -> PollOutcome {
        if send_wheel(
            input,
            modes,
            &self.previous,
            WHEEL_UP,
            WHEEL_STEP_EVENTS,
            cell_size,
        )
        .is_err()
        {
            return PollOutcome::Fallback;
        }
        self.upward_events = self.upward_events.saturating_add(WHEEL_STEP_EVENTS);
        self.phase = Phase::Harvest {
            unaligned_checks: 0,
        };
        self.next_poll_at = now + STEP_SETTLE;
        PollOutcome::Pending(self)
    }

    fn start_restore(
        mut self,
        modes: TerminalModes,
        input: &PtyInput,
        cell_size: (u16, u16),
        now: Instant,
    ) -> PollOutcome {
        if self.upward_events == 0 {
            return PollOutcome::Fallback;
        }
        if send_wheel(
            input,
            modes,
            &self.previous,
            WHEEL_DOWN,
            self.upward_events,
            cell_size,
        )
        .is_err()
        {
            return PollOutcome::Fallback;
        }
        self.phase = Phase::Restore { stable_checks: 0 };
        self.restore_started_at = Some(now);
        self.next_poll_at = now + STEP_SETTLE;
        PollOutcome::Pending(self)
    }
}

pub(crate) fn visible_text(terminal: &Terminal, lines: usize) -> String {
    snapshot_text(&ScreenSnapshot::capture(terminal).rows, lines)
}

fn read_result(rows: &[ScreenRow], lines: usize, truncated: bool) -> ReadResult {
    ReadResult {
        text: snapshot_text(rows, lines),
        truncated,
    }
}

fn send_wheel(
    input: &PtyInput,
    modes: TerminalModes,
    snapshot: &ScreenSnapshot,
    button: u16,
    events: usize,
    (cell_width, cell_height): (u16, u16),
) -> std::io::Result<()> {
    if !(modes.mouse_clicks || modes.mouse_motion) {
        return Err(std::io::Error::other("mouse reporting is disabled"));
    }
    let column = snapshot.cols.saturating_sub(1) / 2;
    let row = snapshot.rows.len().saturating_sub(1) / 2;
    let (column, row) = if modes.sgr_pixels {
        (
            column
                .saturating_mul(usize::from(cell_width.max(1)))
                .saturating_add(usize::from(cell_width.max(1)) / 2)
                .saturating_add(1),
            row.saturating_mul(usize::from(cell_height.max(1)))
                .saturating_add(usize::from(cell_height.max(1)) / 2)
                .saturating_add(1),
        )
    } else {
        (column.saturating_add(1), row.saturating_add(1))
    };
    let event = encode_sgr_mouse(
        button,
        u32::try_from(column).unwrap_or(u32::MAX),
        u32::try_from(row).unwrap_or(u32::MAX),
        true,
    );
    let mut bytes = Vec::with_capacity(event.len().saturating_mul(events));
    for _ in 0..events {
        bytes.extend_from_slice(event.as_bytes());
    }
    input.send(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> ScreenRow {
        ScreenRow {
            text: text.to_owned(),
            soft_wrapped: false,
        }
    }

    fn snapshot(lines: &[&str]) -> ScreenSnapshot {
        ScreenSnapshot {
            cols: 20,
            rows: lines.iter().map(|line| row(line)).collect(),
        }
    }

    #[test]
    fn similarity_tolerates_a_small_dynamic_region() {
        let initial = snapshot(&["line 1", "line 2", "worked for 2s", "prompt"]);
        let changed = snapshot(&["line 1", "line 2", "worked for 3s", "prompt"]);
        let scrolled = snapshot(&["older", "line 1", "line 2", "prompt"]);
        assert!(initial.similar_text(&changed));
        assert!(!initial.similar_text(&scrolled));
    }

    #[test]
    fn upward_merge_prepends_new_rows_without_duplicating_a_header() {
        let previous = snapshot(&["HEADER", "three", "four", "prompt"]);
        let next = snapshot(&["HEADER", "one", "two", "three"]);
        let mut history = previous.rows.clone();
        assert_eq!(
            merge_scrolled_up(&mut history, &previous, &next),
            UpwardMerge::Advanced
        );
        assert_eq!(
            row_identities(&history),
            ["one", "two", "HEADER", "three", "four", "prompt"]
        );
    }

    #[test]
    fn alignment_requires_thirty_percent_of_non_empty_overlap() {
        assert_eq!(
            best_upward_shift(
                &["a".into(), "b".into(), "c".into(), "d".into()],
                &["x".into(), "a".into(), "z".into(), "q".into()]
            ),
            Some(1)
        );
        assert_eq!(
            best_upward_shift(
                &["a".into(), "b".into(), "c".into(), "d".into()],
                &["x".into(), "z".into(), "q".into(), "w".into()]
            ),
            None
        );
    }

    #[test]
    fn snapshot_joins_soft_wraps_and_adds_one_trailing_newline() {
        let rows = vec![
            ScreenRow {
                text: "wrapped ".into(),
                soft_wrapped: true,
            },
            row("line"),
            row("tail"),
            row("   "),
        ];
        assert_eq!(snapshot_text(&rows, rows.len()), "wrappedline\ntail\n");
        let limited = read_result(&rows, 2, true);
        assert_eq!(limited.text, "tail\n");
        assert!(limited.truncated);
    }
}
