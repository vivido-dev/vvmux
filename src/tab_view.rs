//! Where the tab list is drawn: a one-row bar at the bottom or top, a sidebar on either side, or
//! nowhere.
//!
//! The view is session state, so every attached client sees the same one. This module owns the
//! geometry each view reserves and the sidebar's tree of tabs and panes; the session actor owns the
//! state and does the drawing.

use serde::{Deserialize, Serialize};

use crate::layout::{PaneId, Rect};

/// The widest a sidebar grows, separator included. It never takes more than a third of the host.
const SIDEBAR_MAX_WIDTH: u16 = 24;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabView {
    #[default]
    Bottom,
    Top,
    Left,
    Right,
    Hidden,
}

impl TabView {
    /// The next view in the `cycle-tab-view` order.
    pub fn next(self) -> Self {
        match self {
            Self::Bottom => Self::Top,
            Self::Top => Self::Left,
            Self::Left => Self::Right,
            Self::Right => Self::Hidden,
            Self::Hidden => Self::Bottom,
        }
    }

    /// Host rows the view takes away from the pane area.
    pub fn bar_rows(self) -> u16 {
        u16::from(matches!(self, Self::Bottom | Self::Top))
    }

    /// The host row of a one-row bar.
    pub fn bar_row(self, rows: u16) -> Option<u16> {
        match self {
            Self::Bottom => Some(rows.saturating_sub(1)),
            Self::Top => Some(0),
            _ => None,
        }
    }

    fn sidebar_width(self, columns: u16) -> u16 {
        match self {
            Self::Left | Self::Right => (columns / 3).min(SIDEBAR_MAX_WIDTH),
            _ => 0,
        }
    }

    /// The sidebar's cells, its separator column included.
    pub fn sidebar_rect(self, columns: u16, rows: u16) -> Option<Rect> {
        let width = self.sidebar_width(columns);
        if width == 0 || rows == 0 {
            return None;
        }
        let x = match self {
            Self::Left => 0,
            _ => columns - width,
        };
        Some(Rect {
            x,
            y: 0,
            width,
            height: rows,
        })
    }

    /// The sidebar's text cells: its rect without the separator column.
    pub fn sidebar_text_rect(self, columns: u16, rows: u16) -> Option<Rect> {
        let rect = self.sidebar_rect(columns, rows)?;
        Some(Rect {
            x: if self == Self::Left {
                rect.x
            } else {
                rect.x + 1
            },
            width: rect.width - 1,
            ..rect
        })
    }

    /// The column that divides a sidebar from the panes.
    pub fn separator_column(self, columns: u16, rows: u16) -> Option<u16> {
        let rect = self.sidebar_rect(columns, rows)?;
        Some(if self == Self::Left {
            rect.x + rect.width - 1
        } else {
            rect.x
        })
    }

    /// What is left for panes on a `columns` x `rows` host.
    pub fn content_area(self, columns: u16, rows: u16) -> Rect {
        let columns = columns.max(1);
        let sidebar = self.sidebar_width(columns);
        Rect {
            x: if self == Self::Left { sidebar } else { 0 },
            y: if self == Self::Top { 1 } else { 0 },
            width: (columns - sidebar).max(1),
            height: rows.saturating_sub(self.bar_rows()).max(1),
        }
    }
}

/// One tab as the sidebar lists it. Labels arrive already made single-line.
pub struct SidebarTab {
    pub id: u64,
    pub label: String,
    pub active: bool,
    pub expanded: bool,
    pub panes: Vec<SidebarPane>,
}

pub struct SidebarPane {
    pub id: PaneId,
    pub label: String,
    pub focused: bool,
}

/// What a click on a sidebar line does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTarget {
    Tab(u64),
    Pane(PaneId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarLine {
    pub text: String,
    pub target: SidebarTarget,
    /// The tab this line's `+`/`-` marker expands or collapses, when it has one.
    pub toggle: Option<u64>,
    /// The active tab's line, drawn in reverse video.
    pub highlight: bool,
}

/// The sidebar as a rootless tree: every tab is a top-level line, and a tab with more than one pane
/// has a `+` marker that expands it into one line per pane (`-` while expanded).
pub fn sidebar_lines(tabs: &[SidebarTab]) -> Vec<SidebarLine> {
    let mut lines = Vec::new();
    for (index, tab) in tabs.iter().enumerate() {
        let expandable = tab.panes.len() > 1;
        let marker = match (expandable, tab.expanded) {
            (false, _) => ' ',
            (true, false) => '+',
            (true, true) => '-',
        };
        let number = index + 1;
        let text = if tab.label.trim().is_empty() {
            format!("{marker}{number}")
        } else {
            format!("{marker}{number} {}", tab.label)
        };
        lines.push(SidebarLine {
            text,
            target: SidebarTarget::Tab(tab.id),
            toggle: expandable.then_some(tab.id),
            highlight: tab.active,
        });
        if !expandable || !tab.expanded {
            continue;
        }
        let last = tab.panes.len() - 1;
        for (position, pane) in tab.panes.iter().enumerate() {
            let branch = if position == last { '└' } else { '├' };
            let focus = if pane.focused { '*' } else { ' ' };
            lines.push(SidebarLine {
                text: format!("  {branch}{focus}{}", pane.label),
                target: SidebarTarget::Pane(pane.id),
                toggle: None,
                highlight: false,
            });
        }
    }
    lines
}

/// The first line to draw so that `lines` lines fit in `height` rows, clamping a stale scroll.
pub fn clamp_scroll(scroll: usize, lines: usize, height: usize) -> usize {
    scroll.min(lines.saturating_sub(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: u64, label: &str, active: bool, expanded: bool, panes: &[u64]) -> SidebarTab {
        SidebarTab {
            id,
            label: label.into(),
            active,
            expanded,
            panes: panes
                .iter()
                .map(|pane| SidebarPane {
                    id: *pane,
                    label: format!("p{pane}"),
                    focused: *pane == panes[0],
                })
                .collect(),
        }
    }

    #[test]
    fn views_cycle_through_all_five_and_reserve_their_own_cells() {
        let mut view = TabView::Bottom;
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(view);
            view = view.next();
        }
        assert_eq!(view, TabView::Bottom);
        assert_eq!(
            seen,
            [
                TabView::Bottom,
                TabView::Top,
                TabView::Left,
                TabView::Right,
                TabView::Hidden
            ]
        );

        let area = |view: TabView| view.content_area(90, 30);
        assert_eq!(
            area(TabView::Bottom),
            Rect {
                x: 0,
                y: 0,
                width: 90,
                height: 29
            }
        );
        assert_eq!(
            area(TabView::Top),
            Rect {
                x: 0,
                y: 1,
                width: 90,
                height: 29
            }
        );
        assert_eq!(
            area(TabView::Left),
            Rect {
                x: 24,
                y: 0,
                width: 66,
                height: 30
            }
        );
        assert_eq!(
            area(TabView::Right),
            Rect {
                x: 0,
                y: 0,
                width: 66,
                height: 30
            }
        );
        assert_eq!(
            area(TabView::Hidden),
            Rect {
                x: 0,
                y: 0,
                width: 90,
                height: 30
            }
        );
        assert_eq!(TabView::Left.separator_column(90, 30), Some(23));
        assert_eq!(TabView::Right.separator_column(90, 30), Some(66));
        assert_eq!(TabView::Right.sidebar_text_rect(90, 30).unwrap().x, 67);
        // A narrow host keeps two thirds of its columns for panes.
        assert_eq!(TabView::Left.content_area(12, 5).width, 8);
    }

    #[test]
    fn sidebar_is_a_rootless_tree_that_expands_only_multi_pane_tabs() {
        let lines = sidebar_lines(&[
            tab(7, "editor", false, false, &[1, 2]),
            tab(8, "", true, true, &[3, 4, 5]),
            tab(9, "logs", false, true, &[6]),
        ]);
        let texts: Vec<_> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            texts,
            ["+1 editor", "-2", "  ├*p3", "  ├ p4", "  └ p5", " 3 logs"]
        );
        assert_eq!(lines[0].toggle, Some(7));
        assert_eq!(
            lines[5].toggle, None,
            "a single-pane tab has nothing to expand"
        );
        assert_eq!(lines[3].target, SidebarTarget::Pane(4));
        assert!(lines[1].highlight && !lines[0].highlight);
    }

    #[test]
    fn scroll_is_clamped_to_the_last_full_page() {
        assert_eq!(clamp_scroll(9, 12, 5), 7);
        assert_eq!(clamp_scroll(3, 4, 10), 0);
    }
}
