use vvmux_terminal::{Cell, Terminal, TerminalColor, TerminalHyperlink, UnderlineStyle};

use crate::layout::Rect;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenBuffer {
    pub rows: u16,
    pub columns: u16,
    pub cells: Vec<Cell>,
    pub cursor: Option<(u16, u16)>,
}

impl ScreenBuffer {
    pub fn new(columns: u16, rows: u16) -> Self {
        Self {
            rows,
            columns,
            cells: vec![Cell::default(); usize::from(columns) * usize::from(rows)],
            cursor: None,
        }
    }

    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if x < self.columns && y < self.rows {
            let index = usize::from(y) * usize::from(self.columns) + usize::from(x);
            // Overwriting either half of an existing wide glyph must erase the other half.
            // Otherwise an upper floating frame can leave a lower pane's double-width leading
            // cell pointing through the overlap boundary (or leave an orphan continuation).
            if self.cells[index].wide_continuation && x > 0 {
                self.cells[index - 1] = Cell::default();
            }
            if x + 1 < self.columns && self.cells[index + 1].wide_continuation {
                self.cells[index + 1] = Cell::default();
            }
            self.cells[index] = cell;
        }
    }

    pub fn draw_text(&mut self, x: u16, y: u16, text: &str, foreground: u8, background: u8) {
        for (offset, ch) in text.chars().enumerate() {
            let x = x.saturating_add(offset as u16);
            if x >= self.columns {
                break;
            }
            self.set(
                x,
                y,
                Cell {
                    ch,
                    foreground: TerminalColor::Indexed(foreground),
                    background: TerminalColor::Indexed(background),
                    ..Cell::default()
                },
            );
        }
    }

    pub fn draw_frame(&mut self, rect: Rect, title: &str, color: u8) {
        if rect.width < 2 || rect.height < 2 {
            return;
        }
        let styled = |ch| Cell {
            ch,
            foreground: TerminalColor::Indexed(color),
            ..Cell::default()
        };
        let right = rect.x + rect.width - 1;
        let bottom = rect.y + rect.height - 1;
        for x in rect.x + 1..right {
            self.set(x, rect.y, styled('─'));
            self.set(x, bottom, styled('─'));
        }
        for y in rect.y + 1..bottom {
            self.set(rect.x, y, styled('│'));
            self.set(right, y, styled('│'));
        }
        self.set(rect.x, rect.y, styled('┌'));
        self.set(right, rect.y, styled('┐'));
        self.set(rect.x, bottom, styled('└'));
        self.set(right, bottom, styled('┘'));
        for (offset, ch) in title
            .chars()
            .take(rect.width.saturating_sub(4) as usize)
            .enumerate()
        {
            self.set(rect.x + 2 + offset as u16, rect.y, styled(ch));
        }
    }

    pub fn draw_terminal(&mut self, rect: Rect, terminal: &Terminal, scroll_offset: usize) {
        for row in 0..usize::from(rect.height) {
            let logical = row as isize - scroll_offset as isize;
            let Some(line) = terminal.viewport_line(logical) else {
                continue;
            };
            for column in 0..usize::from(rect.width) {
                if let Some(cell) = line.get(column) {
                    self.set(rect.x + column as u16, rect.y + row as u16, cell.clone());
                }
            }
        }
    }
}

pub fn ansi_diff(
    previous: Option<&ScreenBuffer>,
    current: &ScreenBuffer,
    force_full: bool,
) -> Vec<u8> {
    let full = force_full
        || previous.is_none()
        || previous.is_some_and(|old| old.rows != current.rows || old.columns != current.columns);
    let mut output = Vec::with_capacity(current.cells.len().saturating_mul(2));
    if full {
        // CSI 2 J is observable by a Vivid-aware outer terminal: it clears every scene node,
        // including the unanchored nodes owned by OuterBridge. A full ScreenBuffer already
        // repaints every cell (blank cells included), so clearing is both redundant and
        // destructive to media state that the bridge still considers live.
        output.extend_from_slice(b"\x1b[0m\x1b[?25l\x1b[H");
    }
    let mut style: Option<Style> = None;
    for row in 0..current.rows {
        let mut column = 0;
        while column < current.columns {
            let index = usize::from(row) * usize::from(current.columns) + usize::from(column);
            let changed =
                full || previous.is_none_or(|old| old.cells[index] != current.cells[index]);
            if !changed {
                column += 1;
                continue;
            }
            output.extend_from_slice(format!("\x1b[{};{}H", row + 1, column + 1).as_bytes());
            while column < current.columns {
                let index = usize::from(row) * usize::from(current.columns) + usize::from(column);
                if !full && previous.is_some_and(|old| old.cells[index] == current.cells[index]) {
                    break;
                }
                let cell = &current.cells[index];
                let next_style = Style::from(cell);
                if style.as_ref() != Some(&next_style) {
                    write_style(&mut output, &next_style);
                    style = Some(next_style);
                }
                if !cell.wide_continuation {
                    let mut encoded = [0; 4];
                    output.extend_from_slice(cell.ch.encode_utf8(&mut encoded).as_bytes());
                    output.extend_from_slice(cell.combining.as_bytes());
                }
                column += 1;
            }
        }
    }
    output.extend_from_slice(b"\x1b]8;;\x1b\\\x1b[0m");
    if let Some((x, y)) = current.cursor {
        output.extend_from_slice(format!("\x1b[{};{}H\x1b[?25h", y + 1, x + 1).as_bytes());
    } else {
        output.extend_from_slice(b"\x1b[?25l");
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Style {
    foreground: TerminalColor,
    background: TerminalColor,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: UnderlineStyle,
    underline_color: Option<TerminalColor>,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strikeout: bool,
    hyperlink: Option<TerminalHyperlink>,
}

impl From<&Cell> for Style {
    fn from(cell: &Cell) -> Self {
        Self {
            foreground: cell.foreground,
            background: cell.background,
            bold: cell.bold,
            dim: cell.dim,
            italic: cell.italic,
            underline: cell.underline_style,
            underline_color: cell.underline_color,
            blink: cell.blink,
            inverse: cell.inverse,
            hidden: cell.hidden,
            strikeout: cell.strikeout,
            hyperlink: cell
                .hyperlink
                .as_ref()
                .filter(|link| {
                    !link.uri.chars().any(char::is_control)
                        && link
                            .id
                            .as_ref()
                            .is_none_or(|id| !id.chars().any(char::is_control) && !id.contains(';'))
                })
                .cloned(),
        }
    }
}

fn write_style(output: &mut Vec<u8>, style: &Style) {
    output.extend_from_slice(b"\x1b]8;;\x1b\\");
    output.extend_from_slice(b"\x1b[0");
    if style.bold {
        output.extend_from_slice(b";1");
    }
    if style.dim {
        output.extend_from_slice(b";2");
    }
    if style.italic {
        output.extend_from_slice(b";3");
    }
    match style.underline {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => output.extend_from_slice(b";4"),
        UnderlineStyle::Double => output.extend_from_slice(b";4:2"),
        UnderlineStyle::Curl => output.extend_from_slice(b";4:3"),
        UnderlineStyle::Dotted => output.extend_from_slice(b";4:4"),
        UnderlineStyle::Dashed => output.extend_from_slice(b";4:5"),
    }
    if style.blink {
        output.extend_from_slice(b";5");
    }
    if style.inverse {
        output.extend_from_slice(b";7");
    }
    if style.hidden {
        output.extend_from_slice(b";8");
    }
    if style.strikeout {
        output.extend_from_slice(b";9");
    }
    write_color(output, style.foreground, true);
    write_color(output, style.background, false);
    write_underline_color(output, style.underline_color);
    output.push(b'm');
    if let Some(uri) = &style.hyperlink {
        output.extend_from_slice(b"\x1b]8;");
        if let Some(id) = &uri.id {
            output.extend_from_slice(b"id=");
            output.extend_from_slice(id.as_bytes());
        }
        output.push(b';');
        output.extend_from_slice(uri.uri.as_bytes());
        output.extend_from_slice(b"\x1b\\");
    }
}

fn write_underline_color(output: &mut Vec<u8>, color: Option<TerminalColor>) {
    match color {
        None | Some(TerminalColor::Default) => output.extend_from_slice(b";59"),
        Some(TerminalColor::Indexed(index)) => {
            output.extend_from_slice(format!(";58;5;{index}").as_bytes());
        }
        Some(TerminalColor::Rgb(red, green, blue)) => {
            output.extend_from_slice(format!(";58;2;{red};{green};{blue}").as_bytes());
        }
    }
}

fn write_color(output: &mut Vec<u8>, color: TerminalColor, foreground: bool) {
    match color {
        TerminalColor::Default => {
            output.extend_from_slice(if foreground { b";39" } else { b";49" })
        }
        TerminalColor::Indexed(index) if index < 8 => {
            output.extend_from_slice(
                format!(";{}", if foreground { 30 + index } else { 40 + index }).as_bytes(),
            );
        }
        TerminalColor::Indexed(index) if index < 16 => {
            output.extend_from_slice(
                format!(
                    ";{}",
                    if foreground {
                        90 + index - 8
                    } else {
                        100 + index - 8
                    }
                )
                .as_bytes(),
            );
        }
        TerminalColor::Indexed(index) => {
            output.extend_from_slice(
                format!(";{};5;{index}", if foreground { 38 } else { 48 }).as_bytes(),
            );
        }
        TerminalColor::Rgb(red, green, blue) => {
            output.extend_from_slice(
                format!(
                    ";{};2;{red};{green};{blue}",
                    if foreground { 38 } else { 48 }
                )
                .as_bytes(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vvmux_terminal::TerminalHyperlink;

    #[test]
    fn diff_skips_unchanged_cells() {
        let mut first = ScreenBuffer::new(4, 2);
        first.draw_text(0, 0, "test", 7, 0);
        let full = ansi_diff(None, &first, true);
        let unchanged = ansi_diff(Some(&first), &first, false);
        assert!(full.len() > unchanged.len());
        let mut second = first.clone();
        second.cells[0].ch = 'b';
        assert!(String::from_utf8_lossy(&ansi_diff(Some(&first), &second, false)).contains('b'));
    }

    #[test]
    fn full_redraw_repaints_without_clearing_outer_media() {
        let screen = ScreenBuffer::new(4, 2);
        let full = ansi_diff(None, &screen, true);

        assert!(
            !full
                .windows(b"\x1b[2J".len())
                .any(|window| window == b"\x1b[2J"),
            "a terminal clear destroys every Vivid node in the outer presenter"
        );
        assert_eq!(
            full.iter().filter(|byte| **byte == b' ').count(),
            screen.cells.len(),
            "a clear-free full redraw must explicitly repaint blank cells"
        );
    }

    #[test]
    fn overwriting_either_half_of_a_wide_cell_clears_the_other_half() {
        let wide = Cell {
            ch: '界',
            ..Cell::default()
        };
        let continuation = Cell {
            wide_continuation: true,
            ..Cell::default()
        };

        let mut leading_overlap = ScreenBuffer::new(4, 1);
        leading_overlap.set(1, 0, wide.clone());
        leading_overlap.set(2, 0, continuation.clone());
        leading_overlap.set(1, 0, Cell::default());
        assert_eq!(leading_overlap.cells[2], Cell::default());

        let mut continuation_overlap = ScreenBuffer::new(4, 1);
        continuation_overlap.set(1, 0, wide);
        continuation_overlap.set(2, 0, continuation);
        continuation_overlap.set(2, 0, Cell::default());
        assert_eq!(continuation_overlap.cells[1], Cell::default());
    }

    #[test]
    fn rich_styles_and_hyperlinks_are_forwarded_to_the_outer_terminal() {
        let mut screen = ScreenBuffer::new(2, 1);
        screen.cells[0] = Cell {
            ch: 'x',
            dim: true,
            underline: true,
            underline_style: UnderlineStyle::Curl,
            underline_color: Some(TerminalColor::Rgb(1, 2, 3)),
            strikeout: true,
            hyperlink: Some(TerminalHyperlink {
                id: Some("link-1".into()),
                uri: "https://example.test/".into(),
            }),
            ..Cell::default()
        };
        let output = ansi_diff(None, &screen, true);
        assert!(
            output
                .windows(b";2;4:3;9".len())
                .any(|bytes| bytes == b";2;4:3;9")
        );
        assert!(
            output
                .windows(b";58;2;1;2;3".len())
                .any(|bytes| bytes == b";58;2;1;2;3")
        );
        assert!(
            output
                .windows(b"\x1b]8;id=link-1;https://example.test/\x1b\\".len())
                .any(|bytes| { bytes == b"\x1b]8;id=link-1;https://example.test/\x1b\\" })
        );
    }
}
