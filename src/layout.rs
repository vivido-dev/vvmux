use std::collections::BTreeMap;

use crate::ipc::{Axis, Direction};

pub type PaneId = u64;

pub const MIN_CONTENT_COLUMNS: u16 = 4;
pub const MIN_CONTENT_ROWS: u16 = 2;
const FRAME_COLUMNS: u16 = 2;
const FRAME_ROWS: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn content(self) -> Self {
        Self {
            x: self.x.saturating_add(1),
            y: self.y.saturating_add(1),
            width: self.width.saturating_sub(FRAME_COLUMNS),
            height: self.height.saturating_sub(FRAME_ROWS),
        }
    }

    pub fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn center(self) -> (i32, i32) {
        (
            i32::from(self.x) * 2 + i32::from(self.width),
            i32::from(self.y) * 2 + i32::from(self.height),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneLayer {
    Tiled,
    Floating,
    Pinned,
}

/// One visible pane in bottom-to-top paint order. The same ordered list drives composition,
/// reverse-order hit testing, PTY sizing, focus, and media projection, so those consumers
/// cannot disagree about visibility or stacking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneProjection {
    pub pane_id: PaneId,
    pub outer: Rect,
    pub content: Rect,
    pub layer: PaneLayer,
    pub focused: bool,
}

/// Directional focus across visible floats and tiled panes together: candidates whose center
/// lies strictly in the requested half-plane, ranked by forward edge distance, then descending
/// perpendicular overlap, then ascending pane ID.
pub fn directional_focus(
    projections: &[PaneProjection],
    focused: PaneId,
    direction: Direction,
) -> Option<PaneId> {
    let current = projections
        .iter()
        .find(|projection| projection.pane_id == focused)?
        .outer;
    let (cx, cy) = current.center();
    projections
        .iter()
        .filter(|projection| projection.pane_id != focused)
        .filter_map(|projection| {
            let rect = projection.outer;
            let (x, y) = rect.center();
            let in_half_plane = match direction {
                Direction::Left => x < cx,
                Direction::Right => x > cx,
                Direction::Up => y < cy,
                Direction::Down => y > cy,
            };
            if !in_half_plane {
                return None;
            }
            let edge_distance = match direction {
                Direction::Left => i32::from(current.x) - i32::from(rect.x + rect.width),
                Direction::Right => i32::from(rect.x) - i32::from(current.x + current.width),
                Direction::Up => i32::from(current.y) - i32::from(rect.y + rect.height),
                Direction::Down => i32::from(rect.y) - i32::from(current.y + current.height),
            };
            let overlap = match direction {
                Direction::Left | Direction::Right => {
                    span_overlap(current.y, current.height, rect.y, rect.height)
                }
                Direction::Up | Direction::Down => {
                    span_overlap(current.x, current.width, rect.x, rect.width)
                }
            };
            Some((
                edge_distance,
                std::cmp::Reverse(overlap),
                projection.pane_id,
            ))
        })
        .min()
        .map(|(_, _, pane_id)| pane_id)
}

fn span_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let start = a_start.max(b_start);
    let end = (a_start + a_len).min(b_start + b_len);
    end.saturating_sub(start)
}

pub const MIN_FLOAT_WIDTH: u16 = MIN_CONTENT_COLUMNS + FRAME_COLUMNS;
pub const MIN_FLOAT_HEIGHT: u16 = MIN_CONTENT_ROWS + FRAME_ROWS;

/// Frame edges selected by a resize operation; corners set two bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EdgeMask {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingPane {
    pub pane_id: PaneId,
    /// Frame-inclusive rectangle in composed grid cells.
    pub rect: Rect,
    pub pinned: bool,
}

/// Tab-local floating layer. `panes` is ordered bottom to top, with every ordinary float
/// strictly before every pinned float, so paint order, hit-test order (reversed), and the
/// pinned-above-ordinary rule are all one invariant.
#[derive(Debug)]
pub struct FloatingLayer {
    panes: Vec<FloatingPane>,
    pub ordinary_visible: bool,
}

impl Default for FloatingLayer {
    fn default() -> Self {
        Self {
            panes: Vec::new(),
            ordinary_visible: true,
        }
    }
}

impl FloatingLayer {
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.panes.iter().map(|pane| pane.pane_id).collect()
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.panes.iter().any(|float| float.pane_id == pane)
    }

    pub fn get(&self, pane: PaneId) -> Option<&FloatingPane> {
        self.panes.iter().find(|float| float.pane_id == pane)
    }

    /// All floats in bottom-to-top order, regardless of visibility.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn panes(&self) -> &[FloatingPane] {
        &self.panes
    }

    /// Visible floats in bottom-to-top paint order: the ordinary block only while
    /// `ordinary_visible`, the pinned block always.
    pub fn visible(&self) -> impl Iterator<Item = &FloatingPane> {
        self.panes
            .iter()
            .filter(|pane| pane.pinned || self.ordinary_visible)
    }

    /// The focus fallback within the layer: because pinned floats are always visible and
    /// always above ordinary floats, the topmost visible float is the topmost pinned float
    /// when one exists and the topmost visible ordinary float otherwise.
    pub fn focus_candidate(&self) -> Option<PaneId> {
        self.visible().last().map(|pane| pane.pane_id)
    }

    /// Add a new ordinary float at the top of the ordinary class, centered per the configured
    /// percentages. Creating a float also shows the ordinary layer so the new pane can be
    /// focused.
    pub fn insert(&mut self, pane_id: PaneId, area: Rect, width_percent: u16, height_percent: u16) {
        let rect = default_rect(area, width_percent, height_percent);
        let top_of_ordinary = self.first_pinned_index();
        self.panes.insert(
            top_of_ordinary,
            FloatingPane {
                pane_id,
                rect,
                pinned: false,
            },
        );
        self.ordinary_visible = true;
    }

    pub fn remove(&mut self, pane: PaneId) -> bool {
        let before = self.panes.len();
        self.panes.retain(|float| float.pane_id != pane);
        self.panes.len() != before
    }

    /// Raise a float to the top of its own class only.
    pub fn raise(&mut self, pane: PaneId) -> bool {
        let Some(index) = self.panes.iter().position(|float| float.pane_id == pane) else {
            return false;
        };
        let float = self.panes.remove(index);
        let position = if float.pinned {
            self.panes.len()
        } else {
            self.first_pinned_index()
        };
        self.panes.insert(position, float);
        true
    }

    /// Pinning moves the pane to the top of the pinned class; unpinning to the top of the
    /// ordinary class.
    pub fn set_pinned(&mut self, pane: PaneId, pinned: bool) -> bool {
        let Some(index) = self.panes.iter().position(|float| float.pane_id == pane) else {
            return false;
        };
        let mut float = self.panes.remove(index);
        float.pinned = pinned;
        let position = if pinned {
            self.panes.len()
        } else {
            self.first_pinned_index()
        };
        self.panes.insert(position, float);
        true
    }

    pub fn move_by(&mut self, pane: PaneId, dx: i32, dy: i32, area: Rect) -> bool {
        self.update_rect(pane, area, |rect| Rect {
            x: offset_coordinate(rect.x, dx),
            y: offset_coordinate(rect.y, dy),
            ..rect
        })
    }

    /// Apply a pointer move from its captured press rectangle. Recomputing from the original
    /// rectangle prevents lost/duplicated motion when intermediate mouse events are coalesced.
    pub fn move_from(
        &mut self,
        pane: PaneId,
        original: Rect,
        dx: i32,
        dy: i32,
        area: Rect,
    ) -> bool {
        self.update_rect(pane, area, |_| Rect {
            x: offset_coordinate(original.x, dx),
            y: offset_coordinate(original.y, dy),
            ..original
        })
    }

    pub fn resize_by(
        &mut self,
        pane: PaneId,
        edges: EdgeMask,
        dx: i32,
        dy: i32,
        area: Rect,
    ) -> bool {
        self.update_rect(pane, area, |rect| resize_edges(rect, edges, dx, dy, area))
    }

    /// Apply a pointer edge resize from its captured press rectangle, not the last drag event.
    pub fn resize_from(
        &mut self,
        pane: PaneId,
        original: Rect,
        edges: EdgeMask,
        dx: i32,
        dy: i32,
        area: Rect,
    ) -> bool {
        self.update_rect(pane, area, |_| resize_edges(original, edges, dx, dy, area))
    }

    pub fn set_rect(&mut self, pane: PaneId, rect: Rect, area: Rect) -> bool {
        self.update_rect(pane, area, |_| rect)
    }

    /// Deterministic host-resize behavior: every float keeps its size and position where they
    /// still fit, clamping width and height before x and y so shrinking the host cannot strand
    /// a float outside the visible area.
    pub fn clamp_all(&mut self, area: Rect) {
        for float in &mut self.panes {
            float.rect = clamp_rect(float.rect, area);
        }
    }

    fn update_rect(&mut self, pane: PaneId, area: Rect, apply: impl FnOnce(Rect) -> Rect) -> bool {
        let Some(float) = self.panes.iter_mut().find(|float| float.pane_id == pane) else {
            return false;
        };
        let next = clamp_rect(apply(float.rect), area);
        let changed = next != float.rect;
        float.rect = next;
        changed
    }

    fn first_pinned_index(&self) -> usize {
        self.panes
            .iter()
            .position(|float| float.pinned)
            .unwrap_or(self.panes.len())
    }
}

/// Centered default placement: rounded integer percentages of the tab content area, clamped to
/// the minimum and the area, with any odd leftover cell assigned to the right/bottom.
fn default_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let width = ((u32::from(area.width) * u32::from(width_percent) + 50) / 100) as u16;
    let height = ((u32::from(area.height) * u32::from(height_percent) + 50) / 100) as u16;
    clamp_rect(
        Rect {
            x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
            y: area
                .y
                .saturating_add(area.height.saturating_sub(height) / 2),
            width,
            height,
        },
        area,
    )
}

fn offset_coordinate(value: u16, delta: i32) -> u16 {
    i32::from(value)
        .saturating_add(delta)
        .clamp(0, i32::from(u16::MAX)) as u16
}

/// Clamp width and height first, then x and y. A host smaller than the minimum float size
/// yields a float pinned to the area origin at the minimum size; the display normalization in
/// the session keeps interactive hosts at least one minimal float large.
fn clamp_rect(rect: Rect, area: Rect) -> Rect {
    let min_width = MIN_FLOAT_WIDTH.min(area.width).max(1);
    let min_height = MIN_FLOAT_HEIGHT.min(area.height).max(1);
    let width = rect.width.clamp(min_width, area.width.max(min_width));
    let height = rect.height.clamp(min_height, area.height.max(min_height));
    let max_x = (area.x + area.width).saturating_sub(width).max(area.x);
    let max_y = (area.y + area.height).saturating_sub(height).max(area.y);
    Rect {
        x: rect.x.clamp(area.x, max_x),
        y: rect.y.clamp(area.y, max_y),
        width,
        height,
    }
}

/// Apply a delta to the selected frame edges, keeping every unselected edge fixed and
/// enforcing the minimum size against the dragged edge.
fn resize_edges(rect: Rect, edges: EdgeMask, dx: i32, dy: i32, area: Rect) -> Rect {
    let min_width = i32::from(MIN_FLOAT_WIDTH.min(area.width).max(1));
    let min_height = i32::from(MIN_FLOAT_HEIGHT.min(area.height).max(1));
    let area_left = i32::from(area.x);
    let area_top = i32::from(area.y);
    let area_right = area_left + i32::from(area.width);
    let area_bottom = area_top + i32::from(area.height);
    let mut left = i32::from(rect.x);
    let mut top = i32::from(rect.y);
    let mut right = left + i32::from(rect.width);
    let mut bottom = top + i32::from(rect.height);
    if edges.left {
        left = (left + dx).max(area_left).min(right - min_width);
    }
    if edges.right {
        right = (right + dx).min(area_right).max(left + min_width);
    }
    if edges.top {
        top = (top + dy).max(area_top).min(bottom - min_height);
    }
    if edges.bottom {
        bottom = (bottom + dy).min(area_bottom).max(top + min_height);
    }
    Rect {
        x: left.max(0) as u16,
        y: top.max(0) as u16,
        width: (right - left).max(1) as u16,
        height: (bottom - top).max(1) as u16,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TiledNode {
    Leaf(PaneId),
    Split {
        axis: Axis,
        first: Box<TiledNode>,
        second: Box<TiledNode>,
        first_weight: u32,
        second_weight: u32,
    },
}

impl TiledNode {
    pub fn leaf(pane: PaneId) -> Self {
        Self::Leaf(pane)
    }

    /// Build a weighted branch without applying the interactive minimum-size check.
    ///
    /// Startup layouts are created before the real display is known, so validating them against
    /// the placeholder 80x23 grid would reject layouts that fit as soon as a client attaches.
    pub fn branch(
        axis: Axis,
        first: TiledNode,
        second: TiledNode,
        first_weight: u32,
        second_weight: u32,
    ) -> Self {
        Self::Split {
            axis,
            first: Box::new(first),
            second: Box::new(second),
            first_weight: first_weight.clamp(1, 100_000),
            second_weight: second_weight.clamp(1, 100_000),
        }
    }

    /// Lower an n-ary weighted split to the binary tree used by the live layout engine.
    pub fn from_children(axis: Axis, children: Vec<TiledNode>, sizes: &[u32]) -> TiledNode {
        assert!(
            !children.is_empty(),
            "a tiled split needs at least one child"
        );
        let weights = if sizes.len() == children.len() {
            sizes.to_vec()
        } else {
            vec![1; children.len()]
        };
        let mut children = children.into_iter().zip(weights).rev();
        let (mut tree, mut accumulated_weight) = children.next().unwrap();
        for (child, weight) in children {
            tree = Self::branch(axis, child, tree, weight, accumulated_weight);
            accumulated_weight = accumulated_weight.saturating_add(weight);
        }
        tree
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        self.collect(&mut panes);
        panes
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            Self::Leaf(id) => *id == pane,
            Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    pub fn geometry(&self, area: Rect) -> BTreeMap<PaneId, Rect> {
        let mut geometry = BTreeMap::new();
        self.place(area, &mut geometry);
        geometry
    }

    pub fn split(
        &mut self,
        focused: PaneId,
        new_pane: PaneId,
        axis: Axis,
        area: Rect,
    ) -> Result<(), &'static str> {
        let mut candidate = self.clone();
        if !candidate.replace_leaf(focused, |pane| Self::Split {
            axis,
            first: Box::new(Self::Leaf(pane)),
            second: Box::new(Self::Leaf(new_pane)),
            first_weight: 1,
            second_weight: 1,
        }) {
            return Err("focused pane is not in layout");
        }
        if candidate.geometry(area).values().any(|rect| {
            let content = rect.content();
            content.width < MIN_CONTENT_COLUMNS || content.height < MIN_CONTENT_ROWS
        }) {
            return Err("split would make a pane smaller than the minimum content size");
        }
        *self = candidate;
        Ok(())
    }

    pub fn close(self, pane: PaneId) -> Option<Self> {
        match self {
            Self::Leaf(id) => (id != pane).then_some(Self::Leaf(id)),
            Self::Split {
                axis,
                first,
                second,
                first_weight,
                second_weight,
            } => {
                let first = first.close(pane);
                let second = second.close(pane);
                match (first, second) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        axis,
                        first: Box::new(first),
                        second: Box::new(second),
                        first_weight,
                        second_weight,
                    }),
                    (Some(survivor), None) | (None, Some(survivor)) => Some(survivor),
                    (None, None) => None,
                }
            }
        }
    }

    pub fn resize(&mut self, focused: PaneId, direction: Direction, area: Rect) -> bool {
        let mut changed = false;
        self.resize_inner(focused, direction, area, &mut changed);
        changed
    }

    fn collect(&self, output: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(pane) => output.push(*pane),
            Self::Split { first, second, .. } => {
                first.collect(output);
                second.collect(output);
            }
        }
    }

    fn place(&self, area: Rect, output: &mut BTreeMap<PaneId, Rect>) {
        match self {
            Self::Leaf(pane) => {
                output.insert(*pane, area);
            }
            Self::Split {
                axis,
                first,
                second,
                first_weight,
                second_weight,
            } => match axis {
                Axis::Vertical => {
                    let first_width = divide_span(area.width, *first_weight, *second_weight);
                    first.place(
                        Rect {
                            width: first_width,
                            ..area
                        },
                        output,
                    );
                    second.place(
                        Rect {
                            x: area.x.saturating_add(first_width),
                            width: area.width.saturating_sub(first_width),
                            ..area
                        },
                        output,
                    );
                }
                Axis::Horizontal => {
                    let first_height = divide_span(area.height, *first_weight, *second_weight);
                    first.place(
                        Rect {
                            height: first_height,
                            ..area
                        },
                        output,
                    );
                    second.place(
                        Rect {
                            y: area.y.saturating_add(first_height),
                            height: area.height.saturating_sub(first_height),
                            ..area
                        },
                        output,
                    );
                }
            },
        }
    }

    fn replace_leaf(&mut self, target: PaneId, replacement: impl FnOnce(PaneId) -> Self) -> bool {
        match self {
            Self::Leaf(pane) if *pane == target => {
                *self = replacement(*pane);
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                if first.contains(target) {
                    first.replace_leaf(target, replacement)
                } else {
                    second.replace_leaf(target, replacement)
                }
            }
        }
    }

    fn resize_inner(
        &mut self,
        focused: PaneId,
        direction: Direction,
        area: Rect,
        changed: &mut bool,
    ) -> bool {
        let Self::Split {
            axis,
            first,
            second,
            first_weight,
            second_weight,
        } = self
        else {
            return self.contains(focused);
        };
        let in_first = first.contains(focused);
        let (first_area, second_area, total) =
            split_areas(area, *axis, *first_weight, *second_weight);
        let contains = if in_first {
            first.resize_inner(focused, direction, first_area, changed)
        } else {
            second.resize_inner(focused, direction, second_area, changed)
        };
        if contains && !*changed {
            let compatible = matches!(
                (*axis, direction),
                (Axis::Vertical, Direction::Left | Direction::Right)
                    | (Axis::Horizontal, Direction::Up | Direction::Down)
            );
            if compatible {
                let first_size = match *axis {
                    Axis::Vertical => first_area.width,
                    Axis::Horizontal => first_area.height,
                };
                let minimum = match *axis {
                    Axis::Vertical => MIN_CONTENT_COLUMNS + FRAME_COLUMNS,
                    Axis::Horizontal => MIN_CONTENT_ROWS + FRAME_ROWS,
                };
                let delta = match (*axis, in_first, direction) {
                    (Axis::Vertical, true, Direction::Right)
                    | (Axis::Horizontal, true, Direction::Down) => 1,
                    (Axis::Vertical, false, Direction::Left)
                    | (Axis::Horizontal, false, Direction::Up) => -1,
                    _ => 0,
                };
                let next = i32::from(first_size) + delta;
                if delta != 0
                    && next >= i32::from(minimum)
                    && next <= i32::from(total.saturating_sub(minimum))
                {
                    *first_weight = next as u32;
                    *second_weight = u32::from(total) - next as u32;
                    *changed = true;
                }
            }
        }
        contains
    }
}

/// Weighted division of one split axis that stays defined for every area a window can shrink to.
///
/// Both children normally keep at least one cell. An area with a single cell cannot honor that,
/// so the first child takes it and the second is placed empty: the pane survives the squeeze and
/// reappears when the window grows, which a division that refused to divide could not do.
fn divide_span(total: u16, first_weight: u32, second_weight: u32) -> u16 {
    if total <= 1 {
        return total;
    }
    let total_weight = first_weight.saturating_add(second_weight).max(1);
    ((u32::from(total) * first_weight) / total_weight).clamp(1, u32::from(total - 1)) as u16
}

fn split_areas(area: Rect, axis: Axis, first_weight: u32, second_weight: u32) -> (Rect, Rect, u16) {
    match axis {
        Axis::Vertical => {
            let first_size = divide_span(area.width, first_weight, second_weight);
            (
                Rect {
                    width: first_size,
                    ..area
                },
                Rect {
                    x: area.x.saturating_add(first_size),
                    width: area.width.saturating_sub(first_size),
                    ..area
                },
                area.width,
            )
        }
        Axis::Horizontal => {
            let first_size = divide_span(area.height, first_weight, second_weight);
            (
                Rect {
                    height: first_size,
                    ..area
                },
                Rect {
                    y: area.y.saturating_add(first_size),
                    height: area.height.saturating_sub(first_size),
                    ..area
                },
                area.height,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 23,
        }
    }

    fn tiled_projections(tree: &TiledNode, focused: PaneId, area: Rect) -> Vec<PaneProjection> {
        tree.geometry(area)
            .into_iter()
            .map(|(pane_id, outer)| PaneProjection {
                pane_id,
                outer,
                content: outer.content(),
                layer: PaneLayer::Tiled,
                focused: pane_id == focused,
            })
            .collect()
    }

    #[test]
    fn split_close_and_zoom_source_tree_are_stable() {
        let mut tree = TiledNode::leaf(1);
        tree.split(1, 2, Axis::Vertical, area()).unwrap();
        tree.split(2, 3, Axis::Horizontal, area()).unwrap();
        assert_eq!(tree.pane_ids(), [1, 2, 3]);
        let before = tree.clone();
        assert!(matches!(
            directional_focus(&tiled_projections(&tree, 1, area()), 1, Direction::Right),
            Some(2 | 3)
        ));
        tree = tree.close(2).unwrap();
        assert_eq!(tree.pane_ids(), [1, 3]);
        assert_eq!(before.pane_ids(), [1, 2, 3]);
    }

    #[test]
    fn weighted_children_preserve_requested_proportions() {
        let tree = TiledNode::from_children(
            Axis::Vertical,
            vec![TiledNode::leaf(1), TiledNode::leaf(2)],
            &[30, 70],
        );
        let geometry = tree.geometry(Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 30,
        });
        assert_eq!(geometry[&1].width, 30);
        assert_eq!(geometry[&2].width, 70);
    }

    #[test]
    fn four_way_stack_builds_at_startup_display() {
        let tree = TiledNode::from_children(
            Axis::Horizontal,
            (1..=4).map(TiledNode::leaf).collect(),
            &[1, 1, 1, 1],
        );
        let geometry = tree.geometry(area());
        assert_eq!(geometry.len(), 4);
        assert_eq!(geometry.values().map(|rect| rect.height).sum::<u16>(), 23);
        assert!(geometry.values().all(|rect| rect.height >= 1));
    }

    #[test]
    fn randomized_weighted_layouts_partition_every_available_cell() {
        let mut state = 0x9e37_79b9_u32;
        for iteration in 0..500_u64 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let count = 2 + (state as usize % 15);
            let mut sizes = Vec::with_capacity(count);
            for _ in 0..count {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                sizes.push(1 + state % 1000);
            }
            let axis = if state & 1 == 0 {
                Axis::Vertical
            } else {
                Axis::Horizontal
            };
            let area = Rect {
                x: (state >> 24) as u16,
                y: (state >> 16) as u16,
                width: 1 + (state % 200) as u16,
                height: 1 + ((state >> 8) % 80) as u16,
            };
            let tree = TiledNode::from_children(
                axis,
                (0..count)
                    .map(|index| TiledNode::leaf(iteration * 32 + index as u64))
                    .collect(),
                &sizes,
            );
            let geometry = tree.geometry(area);
            assert_eq!(geometry.len(), count);
            let covered = geometry
                .values()
                .map(|rect| u32::from(rect.width) * u32::from(rect.height))
                .sum::<u32>();
            assert_eq!(covered, u32::from(area.width) * u32::from(area.height));
            assert!(geometry.values().all(|rect| {
                rect.x >= area.x
                    && rect.y >= area.y
                    && rect.x + rect.width <= area.x + area.width
                    && rect.y + rect.height <= area.y + area.height
            }));
        }
    }

    #[test]
    fn directional_focus_ranks_edge_distance_overlap_then_pane_id() {
        let projection = |pane_id, x, y, width, height, layer| PaneProjection {
            pane_id,
            outer: Rect {
                x,
                y,
                width,
                height,
            },
            content: Rect {
                x,
                y,
                width,
                height,
            }
            .content(),
            layer,
            focused: false,
        };
        let projections = [
            projection(1, 0, 0, 20, 10, PaneLayer::Tiled),
            // Two candidates to the right: 2 is nearer by edge distance.
            projection(2, 24, 0, 20, 10, PaneLayer::Tiled),
            projection(3, 50, 0, 20, 10, PaneLayer::Tiled),
            // A float at the same edge distance as 2 but with less vertical overlap.
            projection(4, 24, 8, 20, 10, PaneLayer::Floating),
        ];
        assert_eq!(
            directional_focus(&projections, 1, Direction::Right),
            Some(2)
        );
        assert_eq!(directional_focus(&projections, 2, Direction::Left), Some(1));
        assert_eq!(
            directional_focus(&projections, 2, Direction::Right),
            Some(3)
        );
        assert_eq!(directional_focus(&projections, 1, Direction::Up), None);
        // An overlapping float wins by overlap when edge distances tie.
        let tie = [
            projection(1, 0, 0, 20, 10, PaneLayer::Tiled),
            projection(2, 24, 6, 20, 10, PaneLayer::Floating),
            projection(3, 24, 0, 20, 10, PaneLayer::Floating),
        ];
        assert_eq!(directional_focus(&tie, 1, Direction::Right), Some(3));
        // Full tie falls back to the lower pane ID.
        let full_tie = [
            projection(5, 0, 0, 20, 10, PaneLayer::Tiled),
            projection(9, 24, 0, 20, 10, PaneLayer::Floating),
            projection(7, 24, 0, 20, 10, PaneLayer::Floating),
        ];
        assert_eq!(directional_focus(&full_tie, 5, Direction::Right), Some(7));
    }

    #[test]
    fn minimum_sizes_are_enforced() {
        let mut tree = TiledNode::leaf(1);
        assert!(
            tree.split(
                1,
                2,
                Axis::Vertical,
                Rect {
                    x: 0,
                    y: 0,
                    width: 11,
                    height: 10
                }
            )
            .is_err()
        );
    }

    /// A window can shrink below any layout's minimum, and the panes have to still be there when
    /// it grows back. Placement therefore stays defined all the way down to an empty area: a
    /// split with a single cell to give hands it to the first child and places the second empty.
    #[test]
    fn every_pane_keeps_a_placement_in_an_area_too_small_to_divide() {
        let mut tree = TiledNode::leaf(1);
        tree.split(1, 2, Axis::Vertical, area()).unwrap();
        tree.split(2, 3, Axis::Horizontal, area()).unwrap();
        for (width, height) in [(10, 4), (4, 2), (3, 1), (1, 1), (1, 0), (0, 0)] {
            let host = Rect {
                x: 0,
                y: 0,
                width,
                height,
            };
            let geometry = tree.geometry(host);
            assert_eq!(
                geometry.keys().copied().collect::<Vec<_>>(),
                vec![1, 2, 3],
                "a {width}x{height} area dropped a pane from the layout"
            );
            for (pane, rect) in geometry {
                assert!(
                    rect.x.saturating_add(rect.width) <= width
                        && rect.y.saturating_add(rect.height) <= height,
                    "pane {pane} was placed at {rect:?}, outside a {width}x{height} area"
                );
            }
        }
    }

    #[test]
    fn resize_moves_one_cell_and_preserves_area() {
        let mut tree = TiledNode::leaf(1);
        tree.split(1, 2, Axis::Vertical, area()).unwrap();
        let before = tree.geometry(area())[&1].width;
        assert!(tree.resize(1, Direction::Right, area()));
        let geometry = tree.geometry(area());
        assert_eq!(geometry[&1].width, before + 1);
        assert_eq!(geometry[&1].width + geometry[&2].width, 80);
    }

    fn assert_floating_invariants(layer: &FloatingLayer, area: Rect) {
        let ids = layer.pane_ids();
        let unique = ids.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), ids.len(), "duplicate floating pane ID");
        let mut seen_pinned = false;
        for float in layer.panes() {
            if float.pinned {
                seen_pinned = true;
            } else {
                assert!(!seen_pinned, "ordinary float layered above a pinned float");
            }
            let rect = float.rect;
            assert!(rect.width >= MIN_FLOAT_WIDTH.min(area.width));
            assert!(rect.height >= MIN_FLOAT_HEIGHT.min(area.height));
            assert!(rect.x >= area.x && rect.y >= area.y);
            assert!(
                rect.x + rect.width <= area.x + area.width,
                "float escapes right edge"
            );
            assert!(
                rect.y + rect.height <= area.y + area.height,
                "float escapes bottom edge"
            );
        }
        let visible = layer.visible().collect::<Vec<_>>();
        for float in &visible {
            assert!(float.pinned || layer.ordinary_visible);
        }
        if !layer.ordinary_visible {
            assert!(visible.iter().all(|float| float.pinned));
        }
    }

    #[test]
    fn default_float_is_centered_and_clamped() {
        let mut layer = FloatingLayer::default();
        layer.insert(1, area(), 60, 60);
        let rect = layer.get(1).unwrap().rect;
        assert_eq!((rect.width, rect.height), (48, 14));
        assert_eq!((rect.x, rect.y), (16, 4));

        let tiny = Rect {
            x: 2,
            y: 1,
            width: 7,
            height: 4,
        };
        layer.insert(2, tiny, 60, 60);
        let rect = layer.get(2).unwrap().rect;
        assert_eq!(
            (rect.width, rect.height),
            (MIN_FLOAT_WIDTH, MIN_FLOAT_HEIGHT)
        );
        assert!(rect.x >= tiny.x && rect.x + rect.width <= tiny.x + tiny.width);
    }

    #[test]
    fn raise_and_pin_preserve_class_ordering() {
        let mut layer = FloatingLayer::default();
        for pane in 1..=4 {
            layer.insert(pane, area(), 40, 40);
        }
        assert_eq!(layer.pane_ids(), [1, 2, 3, 4]);
        assert!(layer.set_pinned(2, true));
        assert!(layer.set_pinned(1, true));
        // Ordinary block [3, 4], pinned block [2, 1] (1 pinned last, so on top).
        assert_eq!(layer.pane_ids(), [3, 4, 2, 1]);
        assert_eq!(layer.focus_candidate(), Some(1));

        assert!(layer.raise(3));
        assert_eq!(
            layer.pane_ids(),
            [4, 3, 2, 1],
            "raise stays within the ordinary class"
        );
        assert!(layer.raise(2));
        assert_eq!(
            layer.pane_ids(),
            [4, 3, 1, 2],
            "raise stays within the pinned class"
        );

        assert!(layer.set_pinned(1, false));
        assert_eq!(
            layer.pane_ids(),
            [4, 3, 1, 2],
            "unpinning moves to the top of the ordinary class"
        );
        assert!(!layer.get(1).unwrap().pinned);
        assert_floating_invariants(&layer, area());
    }

    #[test]
    fn hidden_ordinary_floats_keep_state_and_pinned_stay_visible() {
        let mut layer = FloatingLayer::default();
        layer.insert(1, area(), 40, 40);
        layer.insert(2, area(), 40, 40);
        layer.set_pinned(2, true);
        let rects = (layer.get(1).unwrap().rect, layer.get(2).unwrap().rect);

        layer.ordinary_visible = false;
        assert_eq!(
            layer.visible().map(|f| f.pane_id).collect::<Vec<_>>(),
            [2],
            "pinned floats stay visible while ordinary floats are hidden"
        );
        assert_eq!(layer.focus_candidate(), Some(2));
        assert_eq!(
            (layer.get(1).unwrap().rect, layer.get(2).unwrap().rect),
            rects
        );
        assert_eq!(layer.pane_ids(), [1, 2], "hiding mutates no z-order");

        layer.insert(3, area(), 40, 40);
        assert!(
            layer.ordinary_visible,
            "creating a float shows the ordinary layer"
        );
        assert_eq!(
            layer.focus_candidate(),
            Some(2),
            "pinned floats stay above new ordinary floats"
        );
    }

    #[test]
    fn host_resize_clamps_size_before_position() {
        let mut layer = FloatingLayer::default();
        layer.insert(1, area(), 40, 40);
        layer
            .set_rect(
                1,
                Rect {
                    x: 60,
                    y: 15,
                    width: 20,
                    height: 8,
                },
                area(),
            )
            .then_some(())
            .unwrap();

        // Shrink: the size still fits, so only the position moves.
        let smaller = Rect {
            x: 0,
            y: 0,
            width: 64,
            height: 18,
        };
        layer.clamp_all(smaller);
        assert_eq!(
            layer.get(1).unwrap().rect,
            Rect {
                x: 44,
                y: 10,
                width: 20,
                height: 8
            }
        );

        // Shrink below the float's size: width and height clamp first, then position.
        let tiny = Rect {
            x: 3,
            y: 2,
            width: 10,
            height: 5,
        };
        layer.clamp_all(tiny);
        assert_eq!(
            layer.get(1).unwrap().rect,
            Rect {
                x: 3,
                y: 2,
                width: 10,
                height: 5
            }
        );
        assert_floating_invariants(&layer, tiny);
    }

    #[test]
    fn edge_resize_keeps_unselected_edges_fixed() {
        let mut layer = FloatingLayer::default();
        layer.insert(1, area(), 40, 40);
        let start = Rect {
            x: 20,
            y: 8,
            width: 12,
            height: 6,
        };
        assert!(layer.set_rect(1, start, area()));

        let left = EdgeMask {
            left: true,
            ..EdgeMask::default()
        };
        assert!(layer.resize_by(1, left, -4, 0, area()));
        let rect = layer.get(1).unwrap().rect;
        assert_eq!((rect.x, rect.width), (16, 16), "left edge grew leftward");
        assert_eq!(
            rect.x + rect.width,
            start.x + start.width,
            "right edge fixed"
        );

        // Shrinking from the left past the minimum stops at the minimum, keeping the right
        // edge fixed.
        assert!(layer.resize_by(1, left, 40, 0, area()));
        let rect = layer.get(1).unwrap().rect;
        assert_eq!(rect.width, MIN_FLOAT_WIDTH);
        assert_eq!(rect.x + rect.width, start.x + start.width);

        let corner = EdgeMask {
            right: true,
            bottom: true,
            ..EdgeMask::default()
        };
        assert!(layer.resize_by(1, corner, 5, 3, area()));
        let rect = layer.get(1).unwrap().rect;
        assert_eq!(rect.x + rect.width, start.x + start.width + 5);
        assert_eq!(rect.height, start.height + 3);
        assert_eq!(
            rect.y, start.y,
            "top edge fixed while dragging the bottom-right corner"
        );
        assert_floating_invariants(&layer, area());
    }

    #[test]
    fn pointer_geometry_is_always_recomputed_from_the_press_rectangle() {
        let mut layer = FloatingLayer::default();
        layer.insert(1, area(), 40, 40);
        let original = Rect {
            x: 20,
            y: 8,
            width: 20,
            height: 8,
        };
        assert!(layer.set_rect(1, original, area()));

        assert!(layer.move_from(1, original, 12, 4, area()));
        assert!(layer.move_from(1, original, 3, -2, area()));
        assert_eq!(
            layer.get(1).unwrap().rect,
            Rect {
                x: 23,
                y: 6,
                ..original
            },
            "the second event is original + total delta, not last + delta"
        );

        let right = EdgeMask {
            right: true,
            ..EdgeMask::default()
        };
        assert!(layer.resize_from(1, original, right, 10, 0, area()));
        assert!(layer.resize_from(1, original, right, 2, 0, area()));
        assert_eq!(layer.get(1).unwrap().rect.width, original.width + 2);
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn delta(&mut self, magnitude: i32) -> i32 {
            (self.below(2 * magnitude as u64 + 1)) as i32 - magnitude
        }
    }

    #[test]
    fn random_operations_preserve_floating_invariants() {
        let areas = [
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 23,
            },
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 12,
            },
            Rect {
                x: 0,
                y: 0,
                width: 12,
                height: 5,
            },
            Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 40,
            },
        ];
        let mut random = Lcg(0x5eed_cafe);
        let mut layer = FloatingLayer::default();
        let mut area = areas[0];
        let mut next_pane: PaneId = 1;
        for _ in 0..4000 {
            let ids = layer.pane_ids();
            let pick =
                |random: &mut Lcg, ids: &[PaneId]| ids[random.below(ids.len() as u64) as usize];
            match random.below(9) {
                0 if ids.len() < 12 => {
                    layer.insert(
                        next_pane,
                        area,
                        20 + random.below(80) as u16,
                        20 + random.below(80) as u16,
                    );
                    next_pane += 1;
                }
                1 if !ids.is_empty() => {
                    assert!(layer.remove(pick(&mut random, &ids)));
                }
                2 if !ids.is_empty() => {
                    assert!(layer.raise(pick(&mut random, &ids)));
                }
                3 if !ids.is_empty() => {
                    let pane = pick(&mut random, &ids);
                    let pinned = layer.get(pane).unwrap().pinned;
                    assert!(layer.set_pinned(pane, !pinned));
                }
                4 => {
                    layer.ordinary_visible = !layer.ordinary_visible;
                }
                5 if !ids.is_empty() => {
                    let pane = pick(&mut random, &ids);
                    layer.move_by(pane, random.delta(9), random.delta(9), area);
                }
                6 if !ids.is_empty() => {
                    let pane = pick(&mut random, &ids);
                    let edges = EdgeMask {
                        left: random.below(2) == 0,
                        right: random.below(2) == 0,
                        top: random.below(2) == 0,
                        bottom: random.below(2) == 0,
                    };
                    layer.resize_by(pane, edges, random.delta(9), random.delta(9), area);
                }
                7 if !ids.is_empty() => {
                    let pane = pick(&mut random, &ids);
                    let rect = Rect {
                        x: random.below(130) as u16,
                        y: random.below(45) as u16,
                        width: random.below(130) as u16,
                        height: random.below(45) as u16,
                    };
                    layer.set_rect(pane, rect, area);
                }
                8 => {
                    area = areas[random.below(areas.len() as u64) as usize];
                    layer.clamp_all(area);
                }
                _ => {}
            }
            assert_floating_invariants(&layer, area);
        }
        assert!(next_pane > 100, "operation mix exercised inserts");
    }
}
