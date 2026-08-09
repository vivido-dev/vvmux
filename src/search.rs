//! Bounded, column-preserving search over a pane's physical terminal rows.

use regex::{Regex, RegexBuilder};
use vvmux_terminal::Terminal;

pub const MAX_SEARCH_SCAN_LINES: usize = 200_000;
pub const MAX_PATTERN_BYTES: usize = 8 * 1024;
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct SearchPattern {
    regex: Regex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchDirection {
    Forward,
    Backward,
}

impl SearchDirection {
    pub fn opposite(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SearchMatch {
    pub line: isize,
    pub start_column: usize,
    pub end_column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAction {
    Editing,
    Submit(String),
    Cancel,
}

/// Apply one input chunk to the search prompt without depending on session actor state.
pub fn apply_prompt_key(prompt: &mut String, bytes: &[u8]) -> PromptAction {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return PromptAction::Editing;
    };
    for ch in text.chars() {
        match ch {
            '\u{1b}' => return PromptAction::Cancel,
            '\r' | '\n' => return PromptAction::Submit(prompt.clone()),
            '\u{8}' | '\u{7f}' => {
                prompt.pop();
            }
            ch if !ch.is_control()
                && prompt.len().saturating_add(ch.len_utf8()) <= MAX_PATTERN_BYTES =>
            {
                prompt.push(ch);
            }
            _ => {}
        }
    }
    PromptAction::Editing
}

/// Compile a literal or regular expression with vim-like smart-case behavior.
pub fn compile(pattern: &str, regex: bool, smart_case: bool) -> Result<SearchPattern, String> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err("search pattern exceeds 8 KiB".into());
    }
    let expression = if regex {
        pattern.to_owned()
    } else {
        regex::escape(pattern)
    };
    let insensitive = smart_case && !pattern.chars().any(char::is_uppercase);
    RegexBuilder::new(&expression)
        .case_insensitive(insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map(|regex| SearchPattern { regex })
        .map_err(|error| error.to_string())
}

/// Return one physical row's searchable text and the source cell for every emitted character.
pub fn row_text_with_columns(terminal: &Terminal, line: isize) -> Option<(String, Vec<usize>)> {
    let cells = terminal.viewport_line(line)?;
    let mut text = String::new();
    let mut columns = Vec::new();
    let mut column = 0;
    let end = cells.len().min(terminal.cols());
    while column < end {
        let cell = &cells[column];
        if cell.wide_continuation || cell.leading_wide_spacer {
            column += 1;
            continue;
        }
        if let Some(width) = cell.tab_width {
            text.push(' ');
            columns.push(column);
            column = column.saturating_add(usize::from(width).max(1));
            continue;
        }
        text.push(cell.ch);
        columns.push(column);
        for combining in cell.combining.chars() {
            text.push(combining);
            columns.push(column);
        }
        column += 1;
    }
    Some((text, columns))
}

fn match_from_bytes(
    line: isize,
    text: &str,
    columns: &[usize],
    start: usize,
    end: usize,
    terminal_columns: usize,
) -> SearchMatch {
    let start_index = text[..start].chars().count();
    let end_index = text[..end].chars().count();
    SearchMatch {
        line,
        start_column: columns
            .get(start_index)
            .copied()
            .unwrap_or(terminal_columns),
        end_column: columns.get(end_index).copied().unwrap_or(terminal_columns),
    }
}

pub fn find_on_line(terminal: &Terminal, pattern: &SearchPattern, line: isize) -> Vec<SearchMatch> {
    let Some((text, columns)) = row_text_with_columns(terminal, line) else {
        return Vec::new();
    };
    pattern
        .regex
        .find_iter(&text)
        .map(|found| {
            match_from_bytes(
                line,
                &text,
                &columns,
                found.start(),
                found.end(),
                terminal.cols(),
            )
        })
        .collect()
}

/// Find one match from a cell position, visiting at most [`MAX_SEARCH_SCAN_LINES`] rows.
pub fn find_next(
    terminal: &Terminal,
    pattern: &SearchPattern,
    from: (isize, usize),
    direction: SearchDirection,
    wrap: bool,
) -> Option<SearchMatch> {
    let first = -(terminal.history_len() as isize);
    let last = terminal.rows() as isize - 1;
    let start_line = from.0.clamp(first, last);
    let mut visited = 0usize;

    let mut inspect = |line: isize, starting_line: bool, wrapped: bool| {
        if visited >= MAX_SEARCH_SCAN_LINES {
            return None;
        }
        visited += 1;
        let matches = find_on_line(terminal, pattern, line);
        match direction {
            SearchDirection::Forward => matches
                .into_iter()
                .find(|found| !starting_line || wrapped || found.start_column >= from.1),
            SearchDirection::Backward => matches
                .into_iter()
                .rev()
                .find(|found| !starting_line || wrapped || found.start_column <= from.1),
        }
    };

    match direction {
        SearchDirection::Forward => {
            for line in start_line..=last {
                if let Some(found) = inspect(line, line == start_line, false) {
                    return Some(found);
                }
            }
            if wrap {
                for line in first..=start_line {
                    if let Some(found) = inspect(line, line == start_line, true) {
                        return Some(found);
                    }
                }
            }
        }
        SearchDirection::Backward => {
            for line in (first..=start_line).rev() {
                if let Some(found) = inspect(line, line == start_line, false) {
                    return Some(found);
                }
            }
            if wrap {
                for line in (start_line..=last).rev() {
                    if let Some(found) = inspect(line, line == start_line, true) {
                        return Some(found);
                    }
                }
            }
        }
    }
    None
}

/// Collect matches in physical-row order. The boolean reports either result or scan truncation.
pub fn find_all(
    terminal: &Terminal,
    pattern: &SearchPattern,
    limit: usize,
) -> (Vec<SearchMatch>, bool) {
    let first = -(terminal.history_len() as isize);
    let last = terminal.rows() as isize - 1;
    let mut matches = Vec::new();
    for (scanned, line) in (first..=last).enumerate() {
        if scanned >= MAX_SEARCH_SCAN_LINES {
            return (matches, true);
        }
        for found in find_on_line(terminal, pattern, line) {
            if matches.len() == limit {
                return (matches, true);
            }
            matches.push(found);
        }
    }
    (matches, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_terminal(text: &[u8]) -> Terminal {
        let mut terminal = Terminal::new(4, 40, 100);
        terminal.feed(text);
        terminal
    }

    #[test]
    fn literal_regex_smart_case_and_directions_work() {
        let terminal = test_terminal(b"Alpha alpha\r\nbeta 123\r\nalpha");
        let insensitive = compile("alpha", false, true).unwrap();
        let exact = compile("Alpha", false, true).unwrap();
        let digits = compile(r"\d+", true, true).unwrap();

        assert_eq!(find_on_line(&terminal, &insensitive, 0).len(), 2);
        assert_eq!(find_on_line(&terminal, &exact, 0).len(), 1);
        assert_eq!(find_on_line(&terminal, &digits, 1)[0].start_column, 5);
        assert_eq!(
            find_next(
                &terminal,
                &insensitive,
                (0, 1),
                SearchDirection::Forward,
                false,
            )
            .unwrap()
            .start_column,
            6
        );
        assert_eq!(
            find_next(
                &terminal,
                &insensitive,
                (2, 39),
                SearchDirection::Backward,
                false,
            )
            .unwrap()
            .line,
            2
        );
        assert!(
            find_next(
                &terminal,
                &compile("missing", false, true).unwrap(),
                (0, 0),
                SearchDirection::Forward,
                true,
            )
            .is_none()
        );

        let unicode = test_terminal("Ärger ärger".as_bytes());
        assert_eq!(
            find_on_line(&unicode, &compile("ärger", false, true).unwrap(), 0).len(),
            2,
            "smart case must include Unicode case folding"
        );
    }

    #[test]
    fn wrap_reaches_the_other_end() {
        let terminal = test_terminal(b"first\r\nsecond");
        let pattern = compile("first", false, true).unwrap();
        assert_eq!(
            find_next(&terminal, &pattern, (3, 39), SearchDirection::Forward, true,)
                .unwrap()
                .line,
            0
        );
    }

    #[test]
    fn cjk_combining_and_tabs_map_to_physical_columns() {
        let terminal = test_terminal("x\t日本語 e\u{301}".as_bytes());
        let japanese = find_on_line(&terminal, &compile("日本語", false, true).unwrap(), 0);
        assert_eq!(
            japanese,
            [SearchMatch {
                line: 0,
                start_column: 8,
                end_column: 14,
            }]
        );
        let tab_as_space = find_on_line(&terminal, &compile("x ", false, true).unwrap(), 0);
        assert_eq!(tab_as_space[0].end_column, 8);
        let combined = find_on_line(&terminal, &compile("e\u{301}", false, true).unwrap(), 0);
        assert_eq!(combined[0].end_column - combined[0].start_column, 1);
    }

    #[test]
    fn scrollback_uses_negative_lines_and_limits_results() {
        let mut terminal = Terminal::new(2, 20, 100);
        terminal.feed(b"line 001\r\nline 002\r\nline 003\r\nline 004");
        let pattern = compile("line", false, true).unwrap();
        assert!(find_on_line(&terminal, &pattern, -1).len() == 1);
        let (matches, truncated) = find_all(&terminal, &pattern, 2);
        assert_eq!(matches.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn compile_enforces_pattern_and_regex_program_limits() {
        assert!(compile(&"x".repeat(MAX_PATTERN_BYTES + 1), false, true).is_err());
        let large_program = format!(
            "(?:{}){{1000}}",
            (0..200)
                .map(|n| format!("x{n}"))
                .collect::<Vec<_>>()
                .join("|")
        );
        assert!(compile(&large_program, true, false).is_err());
    }

    #[test]
    fn next_match_stops_at_the_scan_budget() {
        let mut terminal = Terminal::new(1, 1, MAX_SEARCH_SCAN_LINES + 10);
        terminal.feed(&b"\r\n".repeat(MAX_SEARCH_SCAN_LINES + 1));
        terminal.feed(b"x");
        let first = -(terminal.history_len() as isize);
        assert!(
            find_next(
                &terminal,
                &compile("x", false, true).unwrap(),
                (first, 0),
                SearchDirection::Forward,
                false,
            )
            .is_none(),
            "a match beyond the line budget must not be reached"
        );
    }

    #[test]
    fn prompt_state_machine_types_erases_submits_and_cancels() {
        let mut prompt = String::new();
        assert_eq!(
            apply_prompt_key(&mut prompt, "né".as_bytes()),
            PromptAction::Editing
        );
        assert_eq!(prompt, "né");
        assert_eq!(
            apply_prompt_key(&mut prompt, b"\x7f"),
            PromptAction::Editing
        );
        assert_eq!(prompt, "n");
        assert_eq!(
            apply_prompt_key(&mut prompt, b"\x08"),
            PromptAction::Editing
        );
        assert_eq!(
            apply_prompt_key(&mut prompt, b"\x08"),
            PromptAction::Editing
        );
        assert!(prompt.is_empty());
        prompt.push_str("needle");
        assert_eq!(
            apply_prompt_key(&mut prompt, b"\r"),
            PromptAction::Submit("needle".into())
        );
        assert_eq!(apply_prompt_key(&mut prompt, b"\x1b"), PromptAction::Cancel);
        prompt.clear();
        assert_eq!(
            apply_prompt_key(&mut prompt, b"batched query\r"),
            PromptAction::Submit("batched query".into())
        );
    }
}
