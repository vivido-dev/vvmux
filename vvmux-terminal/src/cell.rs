//! Grid cells and their colors, attributes, and hyperlinks.

/// A cell color as the application set it, before any theme is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TerminalColor {
    /// The terminal's default foreground or background.
    #[default]
    Default,
    /// An entry in the 256-color palette; 0 through 15 are the named ANSI colors.
    Indexed(u8),
    /// A 24-bit color.
    Rgb(u8, u8, u8),
}

/// One grid cell: its character and the attributes it was written with.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cell {
    /// The base character; a space for blank cells.
    pub ch: char,
    /// Zero-width combining characters that follow `ch`.
    pub combining: String,
    /// Foreground color.
    pub foreground: TerminalColor,
    /// Background color.
    pub background: TerminalColor,
    /// SGR 1.
    pub bold: bool,
    /// SGR 2.
    pub dim: bool,
    /// SGR 3.
    pub italic: bool,
    /// Whether any underline is set; `underline_style` says which.
    pub underline: bool,
    /// The underline shape, from SGR 4 and its colon sub-parameters.
    pub underline_style: UnderlineStyle,
    /// SGR 58 underline color, when set.
    pub underline_color: Option<TerminalColor>,
    /// SGR 5.
    pub blink: bool,
    /// SGR 7.
    pub inverse: bool,
    /// SGR 8.
    pub hidden: bool,
    /// SGR 9.
    pub strikeout: bool,
    /// The right half of the double-width character in the previous cell; draw nothing here.
    pub wide_continuation: bool,
    /// Padding at the end of a row where a double-width character did not fit and wrapped.
    pub leading_wide_spacer: bool,
    /// Number of physical cells traversed by a literal horizontal tab starting here.
    pub tab_width: Option<u16>,
    /// The OSC 8 hyperlink this cell belongs to.
    pub hyperlink: Option<TerminalHyperlink>,
}

/// An OSC 8 hyperlink.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalHyperlink {
    /// The link identity; unlabeled links are given a unique synthetic ID when they arrive.
    pub id: Option<String>,
    /// The link target.
    pub uri: String,
}

/// The underline shape of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UnderlineStyle {
    /// No underline.
    #[default]
    None,
    /// A single straight line.
    Single,
    /// Two straight lines.
    Double,
    /// A wavy line.
    Curl,
    /// A dotted line.
    Dotted,
    /// A dashed line.
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
