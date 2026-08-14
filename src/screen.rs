use vvmux_terminal::{Cell, Terminal, TerminalColor, TerminalHyperlink, UnderlineStyle};

use crate::layout::Rect;

/// Foreground and background for drawn text.
///
/// Carrying the pair in one struct keeps call sites self-documenting and leaves room for the
/// themed colors to be `Rgb` as well as `Indexed`; `write_color` already encodes all three forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextStyle {
    pub foreground: TerminalColor,
    pub background: TerminalColor,
}

impl TextStyle {
    // Themed colors reach production through `ResolvedTheme`; this shorthand is for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn indexed(foreground: u8, background: u8) -> Self {
        Self {
            foreground: TerminalColor::Indexed(foreground),
            background: TerminalColor::Indexed(background),
        }
    }
}

/// How a cell belonging to an OSC 8 link is marked.
///
/// Only the underline is touched. Text color, background, and every other attribute belong to the
/// application that printed the cell, and a link inside an already-styled run must not lose them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkStyle {
    pub underline: UnderlineStyle,
    pub underline_color: Option<TerminalColor>,
}

impl LinkStyle {
    /// The mark for a link the pointer is not on: present but quiet.
    pub fn resting(color: TerminalColor) -> Self {
        Self {
            underline: UnderlineStyle::Dotted,
            underline_color: Some(color),
        }
    }

    /// The mark for the link under the pointer.
    pub fn hovered(color: TerminalColor) -> Self {
        Self {
            underline: UnderlineStyle::Single,
            underline_color: Some(color),
        }
    }
}

/// Border, title, and background colors for a pane frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameStyle {
    pub border: TerminalColor,
    pub title: TerminalColor,
    pub background: TerminalColor,
}

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

    pub fn draw_text(&mut self, x: u16, y: u16, text: &str, style: TextStyle) {
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
                    foreground: style.foreground,
                    background: style.background,
                    ..Cell::default()
                },
            );
        }
    }

    /// Paint an entire row with `style`, so a themed status bar spans the full width instead of
    /// only the columns its text happens to occupy.
    // Consumed by the themed status bar; only the unit tests exercise it until then.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn fill_row(&mut self, y: u16, style: TextStyle) {
        if y >= self.rows {
            return;
        }
        for x in 0..self.columns {
            self.set(
                x,
                y,
                Cell {
                    ch: ' ',
                    foreground: style.foreground,
                    background: style.background,
                    ..Cell::default()
                },
            );
        }
    }

    /// Recolor a run of existing cells without disturbing their text.
    ///
    /// Search highlighting needs to restyle a match in place: rewriting the cells would lose wide
    /// glyphs, combining marks, and hyperlinks that the pane's own output put there.
    // Consumed by scrollback search highlighting; only the unit tests exercise it until then.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn restyle(&mut self, x: u16, y: u16, width: u16, style: TextStyle) {
        if y >= self.rows {
            return;
        }
        let end = x.saturating_add(width).min(self.columns);
        for column in x..end {
            let index = usize::from(y) * usize::from(self.columns) + usize::from(column);
            let cell = &mut self.cells[index];
            cell.foreground = style.foreground;
            cell.background = style.background;
        }
    }

    /// Give the cells inside `rect` that inherit the host terminal's background an explicit one.
    ///
    /// This is what makes a pane opaque. A cell left at [`TerminalColor::Default`] is written as
    /// SGR 49, which the outer terminal renders with no background of its own — under a
    /// translucent window that means the desktop shows through. Substituting a concrete color
    /// makes just this pane a solid panel while its neighbours stay see-through.
    ///
    /// Only the default background is replaced. A cell the application colored itself already
    /// paints over the window, and overwriting it would discard the application's choice.
    /// Everything else about each cell — text, wide-cell structure, combining marks, foreground,
    /// hyperlinks — is left alone, exactly as in [`Self::restyle`].
    pub fn fill_default_background(&mut self, rect: Rect, color: TerminalColor) {
        let right = rect.x.saturating_add(rect.width).min(self.columns);
        let bottom = rect.y.saturating_add(rect.height).min(self.rows);
        for y in rect.y..bottom {
            for x in rect.x..right {
                let index = usize::from(y) * usize::from(self.columns) + usize::from(x);
                let cell = &mut self.cells[index];
                if cell.background == TerminalColor::Default {
                    cell.background = color;
                }
            }
        }
    }

    /// Toggle reverse video across an existing run without replacing any terminal cell data.
    ///
    /// A toggle, rather than forcing inverse on, keeps selection visible over applications that
    /// already use reverse video for some cells. Text, wide-cell structure, combining marks, and
    /// hyperlinks remain untouched.
    pub fn invert(&mut self, x: u16, y: u16, width: u16) {
        if y >= self.rows {
            return;
        }
        let end = x.saturating_add(width).min(self.columns);
        for column in x..end {
            let index = usize::from(y) * usize::from(self.columns) + usize::from(column);
            self.cells[index].inverse = !self.cells[index].inverse;
        }
    }

    /// Mark the OSC 8 links inside `rect`, underlining the hovered one more strongly.
    ///
    /// This runs over the composited buffer rather than the pane's `Terminal` because
    /// `draw_terminal` already copied each cell's hyperlink across, so the scroll offset has
    /// already been applied and does not need re-deriving.
    ///
    /// `hovered` is matched by link identity, not by position, so a link that wraps across rows or
    /// is interrupted by other text still highlights as one link. Pass `hovered: None` for every
    /// pane except the one under the pointer — that is what keeps one pane's hover from marking an
    /// identically-targeted link in another pane.
    ///
    /// Text, wide-cell structure, combining marks, colors, and the cells' own hyperlinks are left
    /// alone; only underline attributes change.
    pub fn style_links(
        &mut self,
        rect: Rect,
        hovered: Option<&TerminalHyperlink>,
        resting: Option<LinkStyle>,
        hover: LinkStyle,
    ) {
        let right = rect.x.saturating_add(rect.width).min(self.columns);
        let bottom = rect.y.saturating_add(rect.height).min(self.rows);
        for y in rect.y..bottom {
            for x in rect.x..right {
                let index = usize::from(y) * usize::from(self.columns) + usize::from(x);
                let cell = &mut self.cells[index];
                let Some(link) = cell.hyperlink.as_ref() else {
                    continue;
                };
                let style = if hovered.is_some_and(|target| target == link) {
                    hover
                } else {
                    // The resting mark yields to an underline the application chose itself, so a
                    // link that is already underlined keeps the style its author asked for.
                    match resting {
                        Some(resting) if cell.underline_style == UnderlineStyle::None => resting,
                        _ => continue,
                    }
                };
                cell.underline = style.underline != UnderlineStyle::None;
                cell.underline_style = style.underline;
                if let Some(color) = style.underline_color {
                    cell.underline_color = Some(color);
                }
            }
        }
    }

    pub fn draw_frame(&mut self, rect: Rect, title: &str, style: FrameStyle) {
        if rect.width < 2 || rect.height < 2 {
            return;
        }
        let styled = |ch| Cell {
            ch,
            foreground: style.border,
            background: style.background,
            ..Cell::default()
        };
        let titled = |ch| Cell {
            ch,
            foreground: style.title,
            background: style.background,
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
            self.set(rect.x + 2 + offset as u16, rect.y, titled(ch));
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

    /// Hide multiplexer-only Kitty placement cells from hosts that cannot render them.
    pub fn suppress_kitty_placeholders(&mut self) {
        for cell in &mut self.cells {
            if cell.ch == '\u{10EEEE}' {
                cell.ch = ' ';
                cell.combining.clear();
                cell.foreground = TerminalColor::Default;
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

    #[test]
    fn kitty_placeholders_are_hidden_without_changing_other_cells() {
        let mut screen = ScreenBuffer::new(2, 1);
        let placeholder = Cell {
            ch: '\u{10EEEE}',
            combining: "\u{0305}\u{030D}\u{030E}".into(),
            foreground: TerminalColor::Rgb(1, 2, 3),
            ..Cell::default()
        };
        screen.set(0, 0, placeholder);
        let text = Cell {
            ch: 'x',
            ..Cell::default()
        };
        screen.set(1, 0, text.clone());

        screen.suppress_kitty_placeholders();

        assert_eq!(screen.cells[0].ch, ' ');
        assert!(screen.cells[0].combining.is_empty());
        assert_eq!(screen.cells[0].foreground, TerminalColor::Default);
        assert_eq!(screen.cells[1], text);
    }
    use vvmux_terminal::TerminalHyperlink;

    #[test]
    fn diff_skips_unchanged_cells() {
        let mut first = ScreenBuffer::new(4, 2);
        first.draw_text(0, 0, "test", TextStyle::indexed(7, 0));
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
    fn selection_inversion_preserves_cells_and_toggles_existing_reverse_video() {
        let mut screen = ScreenBuffer::new(3, 1);
        screen.cells[0].ch = 'a';
        screen.cells[1].ch = 'b';
        screen.cells[1].inverse = true;
        let original = screen.cells.clone();

        screen.invert(0, 0, 2);
        assert!(screen.cells[0].inverse);
        assert!(!screen.cells[1].inverse);
        assert_eq!(screen.cells[0].ch, original[0].ch);
        assert_eq!(screen.cells[1].ch, original[1].ch);

        screen.invert(0, 0, 2);
        assert_eq!(screen.cells, original);
    }

    #[test]
    fn truecolor_styles_reach_the_outer_terminal() {
        let mut screen = ScreenBuffer::new(4, 1);
        screen.draw_text(
            0,
            0,
            "hi",
            TextStyle {
                foreground: TerminalColor::Rgb(10, 20, 30),
                background: TerminalColor::Rgb(40, 50, 60),
            },
        );
        let output = ansi_diff(None, &screen, true);

        assert!(
            output
                .windows(b";38;2;10;20;30".len())
                .any(|bytes| bytes == b";38;2;10;20;30"),
            "an RGB theme foreground must survive to the wire"
        );
        assert!(
            output
                .windows(b";48;2;40;50;60".len())
                .any(|bytes| bytes == b";48;2;40;50;60"),
            "an RGB theme background must survive to the wire"
        );
    }

    #[test]
    fn a_frame_can_color_its_title_apart_from_its_border() {
        let mut screen = ScreenBuffer::new(10, 3);
        screen.draw_frame(
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 3,
            },
            "ab",
            FrameStyle {
                border: TerminalColor::Indexed(1),
                title: TerminalColor::Indexed(2),
                background: TerminalColor::Indexed(3),
            },
        );

        assert_eq!(screen.cells[0].ch, '┌');
        assert_eq!(screen.cells[0].foreground, TerminalColor::Indexed(1));
        assert_eq!(screen.cells[0].background, TerminalColor::Indexed(3));
        assert_eq!(screen.cells[2].ch, 'a');
        assert_eq!(screen.cells[2].foreground, TerminalColor::Indexed(2));
        assert_eq!(screen.cells[2].background, TerminalColor::Indexed(3));
    }

    #[test]
    fn fill_row_spans_the_full_width_before_text_is_drawn() {
        let mut screen = ScreenBuffer::new(6, 2);
        let style = TextStyle::indexed(15, 4);
        screen.fill_row(1, style);
        screen.draw_text(0, 1, "ab", style);

        for column in 0..6 {
            let cell = &screen.cells[usize::from(screen.columns) + column];
            assert_eq!(
                cell.background,
                TerminalColor::Indexed(4),
                "column {column} must carry the status background"
            );
        }
        assert_eq!(screen.cells[usize::from(screen.columns)].ch, 'a');
    }

    #[test]
    fn restyle_recolors_without_disturbing_text() {
        let mut screen = ScreenBuffer::new(6, 1);
        screen.draw_text(0, 0, "needle", TextStyle::indexed(7, 0));
        screen.restyle(2, 0, 3, TextStyle::indexed(0, 11));

        assert_eq!(screen.cells[2].ch, 'e', "text must survive a restyle");
        assert_eq!(screen.cells[2].background, TerminalColor::Indexed(11));
        assert_eq!(screen.cells[4].background, TerminalColor::Indexed(11));
        assert_eq!(
            screen.cells[5].background,
            TerminalColor::Indexed(0),
            "restyle must stop at the requested width"
        );
    }

    #[test]
    fn restyle_clamps_to_the_buffer() {
        let mut screen = ScreenBuffer::new(4, 1);
        screen.draw_text(0, 0, "abcd", TextStyle::indexed(7, 0));

        screen.restyle(3, 0, 99, TextStyle::indexed(1, 2));
        screen.restyle(0, 5, 2, TextStyle::indexed(1, 2));

        assert_eq!(screen.cells[3].background, TerminalColor::Indexed(2));
        assert_eq!(screen.cells[0].background, TerminalColor::Indexed(0));
    }

    #[test]
    fn an_opaque_pane_fills_only_the_cells_that_inherit_the_terminal_background() {
        let mut screen = ScreenBuffer::new(4, 1);
        screen.draw_text(0, 0, "ab", TextStyle::indexed(7, 0));
        screen.cells[2] = Cell {
            ch: 'c',
            foreground: TerminalColor::Indexed(7),
            background: TerminalColor::Default,
            ..Cell::default()
        };

        screen.fill_default_background(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 1,
            },
            TerminalColor::Rgb(30, 30, 46),
        );

        assert_eq!(
            screen.cells[0].background,
            TerminalColor::Indexed(0),
            "a background the application chose must survive"
        );
        assert_eq!(screen.cells[2].background, TerminalColor::Rgb(30, 30, 46));
        assert_eq!(screen.cells[2].ch, 'c', "text must survive the fill");
        assert_eq!(
            screen.cells[2].foreground,
            TerminalColor::Indexed(7),
            "only the background is decided by the pane's opacity"
        );
        assert_eq!(
            screen.cells[3].background,
            TerminalColor::Rgb(30, 30, 46),
            "blank cells are what make the pane read as a solid panel"
        );
    }

    #[test]
    fn filling_a_pane_background_clamps_to_the_buffer_and_leaves_neighbours_alone() {
        let mut screen = ScreenBuffer::new(4, 2);

        screen.fill_default_background(
            Rect {
                x: 2,
                y: 0,
                width: 99,
                height: 99,
            },
            TerminalColor::Indexed(4),
        );

        assert_eq!(screen.cells[2].background, TerminalColor::Indexed(4));
        assert_eq!(screen.cells[3].background, TerminalColor::Indexed(4));
        assert_eq!(
            screen.cells[0].background,
            TerminalColor::Default,
            "a transparent pane to the left keeps showing the desktop"
        );
        assert_eq!(
            screen.cells[usize::from(screen.columns) + 2].background,
            TerminalColor::Indexed(4),
            "the fill covers every row of the rect"
        );
    }

    /// Composition fills each pane as it is drawn rather than sweeping the finished buffer, which
    /// is what keeps the projection order authoritative: a transparent pane stacked over an opaque
    /// one stays see-through where the two overlap.
    #[test]
    fn a_later_transparent_pane_is_not_made_opaque_by_the_pane_beneath_it() {
        let mut screen = ScreenBuffer::new(4, 1);

        let beneath = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 1,
        };
        screen.fill_default_background(beneath, TerminalColor::Indexed(4));

        for x in 2..4 {
            screen.set(x, 0, Cell::default());
        }

        assert_eq!(screen.cells[0].background, TerminalColor::Indexed(4));
        assert_eq!(screen.cells[1].background, TerminalColor::Indexed(4));
        assert_eq!(
            screen.cells[2].background,
            TerminalColor::Default,
            "the pane drawn later owns the cell, opacity and all"
        );
        assert_eq!(screen.cells[3].background, TerminalColor::Default);
    }

    #[test]
    fn filling_a_pane_background_preserves_wide_glyph_structure() {
        let mut screen = ScreenBuffer::new(2, 1);
        screen.set(
            0,
            0,
            Cell {
                ch: '漢',
                ..Cell::default()
            },
        );
        screen.set(
            1,
            0,
            Cell {
                wide_continuation: true,
                ..Cell::default()
            },
        );

        screen.fill_default_background(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            TerminalColor::Indexed(4),
        );

        assert_eq!(screen.cells[0].ch, '漢');
        assert!(
            screen.cells[1].wide_continuation,
            "an in-place fill must not go through the wide-cell repair in `set`"
        );
        assert_eq!(screen.cells[1].background, TerminalColor::Indexed(4));
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

    /// Build a one-row screen whose cells all belong to one link, with `bold` set from `bold_at`.
    fn linked_row(width: u16, text: &str, bold_at: Option<usize>) -> ScreenBuffer {
        let mut screen = ScreenBuffer::new(width, 1);
        for (index, ch) in text.chars().enumerate() {
            screen.cells[index] = Cell {
                ch,
                bold: bold_at == Some(index),
                hyperlink: Some(TerminalHyperlink {
                    id: Some("vvmux-7".into()),
                    uri: "https://example.test/".into(),
                }),
                ..Cell::default()
            };
        }
        screen
    }

    /// Every `id=` value the emitter opened, in order.
    fn emitted_link_ids(output: &[u8]) -> Vec<String> {
        let mut ids = Vec::new();
        let mut rest = output;
        while let Some(start) = rest
            .windows(b"\x1b]8;id=".len())
            .position(|bytes| bytes == b"\x1b]8;id=")
        {
            rest = &rest[start + b"\x1b]8;id=".len()..];
            let end = rest.iter().position(|byte| *byte == b';').unwrap_or(0);
            ids.push(String::from_utf8_lossy(&rest[..end]).into_owned());
            rest = &rest[end..];
        }
        ids
    }

    fn link(id: &str, uri: &str) -> TerminalHyperlink {
        TerminalHyperlink {
            id: Some(id.into()),
            uri: uri.into(),
        }
    }

    fn rect(x: u16, width: u16) -> Rect {
        Rect {
            x,
            y: 0,
            width,
            height: 1,
        }
    }

    #[test]
    fn styling_links_leaves_cell_content_untouched() {
        let mut screen = ScreenBuffer::new(2, 1);
        let target = link("vvmux-1-0", "https://example.test/");
        screen.cells[0] = Cell {
            ch: '界',
            combining: "\u{301}".into(),
            foreground: TerminalColor::Indexed(3),
            background: TerminalColor::Indexed(5),
            bold: true,
            tab_width: Some(4),
            hyperlink: Some(target.clone()),
            ..Cell::default()
        };
        screen.cells[1] = Cell {
            wide_continuation: true,
            hyperlink: Some(target.clone()),
            ..Cell::default()
        };
        let before = screen.cells.clone();

        screen.style_links(
            rect(0, 2),
            Some(&target),
            None,
            LinkStyle::hovered(TerminalColor::Indexed(4)),
        );

        for (index, (after, before)) in screen.cells.iter().zip(&before).enumerate() {
            assert_eq!(after.ch, before.ch, "cell {index} text changed");
            assert_eq!(after.combining, before.combining);
            assert_eq!(after.wide_continuation, before.wide_continuation);
            assert_eq!(after.tab_width, before.tab_width);
            assert_eq!(after.hyperlink, before.hyperlink);
            assert_eq!(after.foreground, before.foreground);
            assert_eq!(after.background, before.background);
            assert_eq!(after.bold, before.bold);
        }
        assert_eq!(screen.cells[0].underline_style, UnderlineStyle::Single);
        assert!(screen.cells[0].underline);
    }

    #[test]
    fn the_resting_mark_yields_to_an_underline_the_application_chose() {
        let mut screen = ScreenBuffer::new(2, 1);
        let target = link("vvmux-1-0", "https://example.test/");
        screen.cells[0] = Cell {
            ch: 'a',
            underline: true,
            underline_style: UnderlineStyle::Curl,
            hyperlink: Some(target.clone()),
            ..Cell::default()
        };
        screen.cells[1] = Cell {
            ch: 'b',
            hyperlink: Some(target.clone()),
            ..Cell::default()
        };

        screen.style_links(
            rect(0, 2),
            None,
            Some(LinkStyle::resting(TerminalColor::Indexed(4))),
            LinkStyle::hovered(TerminalColor::Indexed(4)),
        );

        assert_eq!(
            screen.cells[0].underline_style,
            UnderlineStyle::Curl,
            "an application's own underline must survive the resting mark"
        );
        assert_eq!(screen.cells[1].underline_style, UnderlineStyle::Dotted);
    }

    #[test]
    fn hovering_one_pane_leaves_another_panes_identical_link_alone() {
        // Two owners that deliberately reuse one identifier: an application may supply `id=1` in
        // both panes, so the hovered link alone cannot say which pane it belongs to. Only the
        // caller passing `hovered` for one rect keeps the mark scoped.
        let mut screen = ScreenBuffer::new(4, 1);
        let shared = link("1", "https://example.test/");
        for index in 0..4 {
            screen.cells[index] = Cell {
                ch: 'x',
                hyperlink: Some(shared.clone()),
                ..Cell::default()
            };
        }
        let left = rect(0, 2);
        let right = rect(2, 2);

        let resting = Some(LinkStyle::resting(TerminalColor::Indexed(4)));
        let hover = LinkStyle::hovered(TerminalColor::Indexed(4));
        screen.style_links(left, Some(&shared), resting, hover);
        let right_before = screen.cells[2..4].to_vec();
        screen.style_links(right, None, resting, hover);

        assert_eq!(screen.cells[0].underline_style, UnderlineStyle::Single);
        assert_eq!(screen.cells[1].underline_style, UnderlineStyle::Single);
        assert_eq!(
            screen.cells[2].underline_style,
            UnderlineStyle::Dotted,
            "the unhovered pane keeps its resting mark"
        );
        assert_eq!(screen.cells[3].underline_style, UnderlineStyle::Dotted);
        // The hovered pane's pass must not have reached across the rect boundary at all.
        assert_eq!(
            right_before,
            vec![
                Cell {
                    ch: 'x',
                    hyperlink: Some(shared.clone()),
                    ..Cell::default()
                },
                Cell {
                    ch: 'x',
                    hyperlink: Some(shared.clone()),
                    ..Cell::default()
                },
            ]
        );
    }

    #[test]
    fn a_link_is_marked_across_every_row_it_wraps_onto() {
        let mut screen = ScreenBuffer::new(2, 2);
        let target = link("vvmux-1-0", "https://example.test/");
        for index in 0..4 {
            screen.cells[index] = Cell {
                ch: 'x',
                hyperlink: Some(target.clone()),
                ..Cell::default()
            };
        }

        screen.style_links(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            Some(&target),
            None,
            LinkStyle::hovered(TerminalColor::Indexed(4)),
        );

        assert!(
            screen
                .cells
                .iter()
                .all(|cell| cell.underline_style == UnderlineStyle::Single),
            "identity matching must span rows, not stop at the first row's end"
        );
    }

    #[test]
    fn an_unlabeled_link_reaches_the_outer_terminal_with_one_identity() {
        // The whole path the bug lived on: a program prints an OSC 8 link with no `id=`, vvmux
        // parses it into its own grid, composites it, and re-serializes it for the outer terminal.
        let mut terminal = Terminal::new(2, 12, 10);
        terminal.feed(b"\x1b]8;;https://example.test/\x1b\\link\x1b]8;;\x1b\\");
        let mut screen = ScreenBuffer::new(12, 2);
        screen.draw_terminal(
            Rect {
                x: 0,
                y: 0,
                width: 12,
                height: 2,
            },
            &terminal,
            0,
        );

        let full = emitted_link_ids(&ansi_diff(None, &screen, true));
        assert_eq!(full.len(), 1, "one open for one link: {full:?}");
        assert!(full[0].starts_with("vvmux-"), "id was {}", full[0]);

        // Repaint only part of the link, the way a diff after unrelated output does. Before ids
        // were synthesized this emitted a bare `ESC ] 8 ; ; uri`, and the presenter minted a second
        // identity for the repainted cells while the untouched ones kept the first.
        let previous = screen.clone();
        screen.cells[1].ch = 'X';
        let partial = emitted_link_ids(&ansi_diff(Some(&previous), &screen, false));
        assert_eq!(
            partial, full,
            "a partial repaint must reuse the original identity"
        );
    }

    #[test]
    fn a_style_change_mid_link_reopens_the_same_link() {
        // `write_style` closes and reopens the link on every style change. A presenter that mints
        // its own identity per open would otherwise split one link in two at the bold boundary.
        let screen = linked_row(4, "abcd", Some(2));
        let ids = emitted_link_ids(&ansi_diff(None, &screen, true));

        assert!(ids.len() >= 2, "expected the link to be reopened: {ids:?}");
        assert!(
            ids.iter().all(|id| id == "vvmux-7"),
            "every reopen must carry the original id: {ids:?}"
        );
    }

    #[test]
    fn a_partial_repaint_reopens_the_same_link() {
        // `ansi_diff` resets its style tracker per call, so a diff that touches only part of a link
        // reopens it. The reopened cells must keep the identity the untouched cells still carry,
        // or the presenter sees two links where the user sees one.
        let previous = linked_row(4, "abcd", None);
        let mut current = linked_row(4, "abcd", None);
        current.cells[2].ch = 'Z';

        let output = ansi_diff(Some(&previous), &current, false);
        let ids = emitted_link_ids(&output);

        assert!(!ids.is_empty(), "partial repaint should reopen the link");
        assert!(
            ids.iter().all(|id| id == "vvmux-7"),
            "a partial repaint must not mint a new identity: {ids:?}"
        );
    }
}
