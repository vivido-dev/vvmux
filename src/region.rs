use crate::layout::Rect;

pub const FIXED_ONE: i64 = 1_i64 << 32;

/// A validated signed 32.32 rectangle. Width and height are positive and both far edges are
/// representable, so every subsequent intersection/subtraction can use checked arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FixedRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl FixedRect {
    pub fn new(x: i64, y: i64, width: i64, height: i64) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }
        x.checked_add(width)?;
        y.checked_add(height)?;
        Some(Self {
            x,
            y,
            width,
            height,
        })
    }

    fn right(self) -> i64 {
        // Guaranteed by `new` and preserved by all constructors in this module.
        self.x + self.width
    }

    fn bottom(self) -> i64 {
        self.y + self.height
    }
}

pub fn from_cells(rect: Rect) -> Option<FixedRect> {
    FixedRect::new(
        i64::from(rect.x).checked_mul(FIXED_ONE)?,
        i64::from(rect.y).checked_mul(FIXED_ONE)?,
        i64::from(rect.width).checked_mul(FIXED_ONE)?,
        i64::from(rect.height).checked_mul(FIXED_ONE)?,
    )
}

pub fn intersect(left: FixedRect, right: FixedRect) -> Option<FixedRect> {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let end_x = left.right().min(right.right());
    let end_y = left.bottom().min(right.bottom());
    FixedRect::new(x, y, end_x.checked_sub(x)?, end_y.checked_sub(y)?)
}

/// Subtract one rectangle into at most four pairwise-disjoint pieces.
pub fn subtract(base: FixedRect, hole: FixedRect) -> impl Iterator<Item = FixedRect> {
    let Some(overlap) = intersect(base, hole) else {
        return vec![base].into_iter();
    };
    let mut pieces = Vec::with_capacity(4);
    if let Some(top) = FixedRect::new(base.x, base.y, base.width, overlap.y - base.y) {
        pieces.push(top);
    }
    if let Some(bottom) = FixedRect::new(
        base.x,
        overlap.bottom(),
        base.width,
        base.bottom() - overlap.bottom(),
    ) {
        pieces.push(bottom);
    }
    if let Some(left) = FixedRect::new(base.x, overlap.y, overlap.x - base.x, overlap.height) {
        pieces.push(left);
    }
    if let Some(right) = FixedRect::new(
        overlap.right(),
        overlap.y,
        base.right() - overlap.right(),
        overlap.height,
    ) {
        pieces.push(right);
    }
    pieces.into_iter()
}

/// Apply ordered occluders, coalescing exact neighbors after every subtraction. `None` is an
/// atomic overflow result: callers must omit the whole logical node rather than an arbitrary
/// fragment subset.
pub fn subtract_all(base: FixedRect, holes: &[FixedRect], cap: usize) -> Option<Vec<FixedRect>> {
    let mut fragments = vec![base];
    for hole in holes {
        let mut next = Vec::new();
        for fragment in fragments {
            next.extend(subtract(fragment, *hole));
        }
        order_and_coalesce(&mut next);
        if next.len() > cap {
            return None;
        }
        fragments = next;
        if fragments.is_empty() {
            break;
        }
    }
    order_and_coalesce(&mut fragments);
    (fragments.len() <= cap).then_some(fragments)
}

/// Sort top-to-bottom/left-to-right and merge only edge-adjacent rectangles whose union is
/// itself a rectangle. Repeating to a fixed point makes the result independent of discovery
/// order without ever adding covered area.
pub fn order_and_coalesce(fragments: &mut Vec<FixedRect>) {
    loop {
        fragments.sort_by_key(|rect| (rect.y, rect.x, rect.height, rect.width));
        let mut merged = None;
        'outer: for left in 0..fragments.len() {
            for right in left + 1..fragments.len() {
                let a = fragments[left];
                let b = fragments[right];
                let joined = if a.y == b.y && a.height == b.height && a.right() == b.x {
                    b.right()
                        .checked_sub(a.x)
                        .and_then(|width| FixedRect::new(a.x, a.y, width, a.height))
                } else if a.x == b.x && a.width == b.width && a.bottom() == b.y {
                    b.bottom()
                        .checked_sub(a.y)
                        .and_then(|height| FixedRect::new(a.x, a.y, a.width, height))
                } else {
                    None
                };
                if let Some(joined) = joined {
                    merged = Some((left, right, joined));
                    break 'outer;
                }
            }
        }
        let Some((left, right, joined)) = merged else {
            break;
        };
        fragments[left] = joined;
        fragments.remove(right);
    }
    fragments.sort_by_key(|rect| (rect.y, rect.x, rect.height, rect.width));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i64, y: i64, width: i64, height: i64) -> FixedRect {
        FixedRect::new(x, y, width, height).unwrap()
    }

    fn area(rect: FixedRect) -> i128 {
        i128::from(rect.width) * i128::from(rect.height)
    }

    #[test]
    fn subtraction_is_disjoint_ordered_and_exact_for_subcell_offsets() {
        let base = rect(
            -(FIXED_ONE / 2),
            FIXED_ONE / 4,
            9 * FIXED_ONE,
            7 * FIXED_ONE,
        );
        let holes = [
            rect(FIXED_ONE, 2 * FIXED_ONE, 3 * FIXED_ONE, 2 * FIXED_ONE),
            rect(3 * FIXED_ONE, FIXED_ONE / 2, 2 * FIXED_ONE, 6 * FIXED_ONE),
        ];
        let fragments = subtract_all(base, &holes, 16).unwrap();
        assert!(fragments.windows(2).all(|pair| {
            (pair[0].y, pair[0].x, pair[0].height, pair[0].width)
                <= (pair[1].y, pair[1].x, pair[1].height, pair[1].width)
        }));
        for (index, fragment) in fragments.iter().enumerate() {
            assert_eq!(intersect(base, *fragment), Some(*fragment));
            for other in &fragments[index + 1..] {
                assert!(intersect(*fragment, *other).is_none());
            }
            assert!(
                holes
                    .iter()
                    .all(|hole| intersect(*fragment, *hole).is_none())
            );
        }
        let visible_area: i128 = fragments.iter().copied().map(area).sum();
        // Compute the covered union independently by subtracting the second overlap from the
        // first; this uses exact i128 area and exercises non-cell-aligned coordinates.
        let first = intersect(base, holes[0]).unwrap();
        let second = intersect(base, holes[1]).unwrap();
        let overlap = intersect(first, second).map_or(0, area);
        assert_eq!(
            visible_area,
            area(base) - area(first) - area(second) + overlap
        );
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 32) % bound
        }
    }

    #[test]
    fn seeded_cell_rectangles_match_a_boolean_grid_reference() {
        let mut random = Lcg(0x8bad_f00d);
        for _ in 0..1000 {
            let width = 1 + random.next(16) as usize;
            let height = 1 + random.next(16) as usize;
            let base = rect(0, 0, width as i64, height as i64);
            let mut visible = vec![true; width * height];
            let mut holes = Vec::new();
            for _ in 0..random.next(8) {
                let x = random.next(width as u64) as usize;
                let y = random.next(height as u64) as usize;
                let hole_width = 1 + random.next((width - x) as u64) as usize;
                let hole_height = 1 + random.next((height - y) as u64) as usize;
                holes.push(rect(
                    x as i64,
                    y as i64,
                    hole_width as i64,
                    hole_height as i64,
                ));
                for row in y..y + hole_height {
                    for column in x..x + hole_width {
                        visible[row * width + column] = false;
                    }
                }
            }
            let fragments = subtract_all(base, &holes, 256).unwrap();
            for row in 0..height {
                for column in 0..width {
                    let covered = fragments.iter().any(|fragment| {
                        intersect(*fragment, rect(column as i64, row as i64, 1, 1)).is_some()
                    });
                    assert_eq!(covered, visible[row * width + column]);
                }
            }
        }
    }

    #[test]
    fn cap_and_extreme_coordinates_fail_safely() {
        let base = rect(0, 0, 10, 10);
        let cross = [rect(4, 0, 2, 10), rect(0, 4, 10, 2)];
        assert!(subtract_all(base, &cross, 3).is_none());
        assert!(FixedRect::new(i64::MAX - 2, 0, 3, 1).is_none());
        assert!(FixedRect::new(0, i64::MIN, 1, i64::MAX).is_some());
        assert!(FixedRect::new(0, i64::MAX - 1, 1, 2).is_none());
    }
}
