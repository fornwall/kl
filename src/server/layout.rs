//! How a window divides its area between panes: a binary tree of splits, where
//! each split keeps its proportions as the window is resized.

use crate::command::{Direction, SplitKind};

/// Identifies a pane for the life of the server.
pub type PaneId = u32;

/// The fixed-point scale of a split's ratio.
const SCALE: u32 = 1 << 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    #[must_use]
    pub const fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x && y >= self.y && (x - self.x) < self.width && (y - self.y) < self.height
    }

    const fn right(self) -> u16 {
        self.x.saturating_add(self.width)
    }

    const fn bottom(self) -> u16 {
        self.y.saturating_add(self.height)
    }

    const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Leaf(PaneId),
    Split {
        kind: SplitKind,
        /// The share of the space that goes to `first`, out of [`SCALE`].
        ratio: u32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

/// The line between the two sides of a split. `path` finds the split again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divider {
    pub path: Vec<bool>,
    pub kind: SplitKind,
    /// The one cell wide line itself.
    pub line: Rect,
    /// The area the split divides, which a drag moves the line within.
    pub area: Rect,
}

/// Divides `length` cells into the two sides of a split and the line between.
fn divide(length: u16, ratio: u32) -> (u16, u16) {
    let space = length.saturating_sub(1);
    let first = u32::from(space) * ratio / SCALE;
    let first = u16::try_from(first).unwrap_or(space);
    // Both sides keep a cell for as long as there is room for it.
    let first = first.clamp(
        u16::from(space > 0),
        space.saturating_sub(1).max(u16::from(space > 0)),
    );
    (first, space.saturating_sub(first))
}

/// The areas of the two sides of a split, leaving out the line between them.
fn split_rect(area: Rect, kind: SplitKind, ratio: u32) -> (Rect, Rect) {
    match kind {
        SplitKind::Horizontal => {
            let (first, second) = divide(area.width, ratio);
            (
                Rect {
                    width: first,
                    ..area
                },
                Rect {
                    x: area.x.saturating_add(first).saturating_add(1),
                    width: second,
                    ..area
                },
            )
        }
        SplitKind::Vertical => {
            let (first, second) = divide(area.height, ratio);
            (
                Rect {
                    height: first,
                    ..area
                },
                Rect {
                    y: area.y.saturating_add(first).saturating_add(1),
                    height: second,
                    ..area
                },
            )
        }
    }
}

const fn divider_line(first: Rect, area: Rect, kind: SplitKind) -> Rect {
    match kind {
        SplitKind::Horizontal => Rect {
            x: first.right(),
            width: 1,
            ..area
        },
        SplitKind::Vertical => Rect {
            y: first.bottom(),
            height: 1,
            ..area
        },
    }
}

impl Node {
    /// Each pane with its area, in order from top left.
    #[must_use]
    pub fn panes(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut panes = Vec::new();
        self.collect_panes(area, &mut panes);
        panes
    }

    fn collect_panes(&self, area: Rect, panes: &mut Vec<(PaneId, Rect)>) {
        match self {
            Self::Leaf(id) => panes.push((*id, area)),
            Self::Split {
                kind,
                ratio,
                first,
                second,
            } => {
                let (a, b) = split_rect(area, *kind, *ratio);
                first.collect_panes(a, panes);
                second.collect_panes(b, panes);
            }
        }
    }

    #[must_use]
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        let mut dividers = Vec::new();
        self.collect_dividers(area, &mut Vec::new(), &mut dividers);
        dividers
    }

    fn collect_dividers(&self, area: Rect, path: &mut Vec<bool>, dividers: &mut Vec<Divider>) {
        if let Self::Split {
            kind,
            ratio,
            first,
            second,
        } = self
        {
            let (a, b) = split_rect(area, *kind, *ratio);
            dividers.push(Divider {
                path: path.clone(),
                kind: *kind,
                line: divider_line(a, area, *kind),
                area,
            });
            path.push(false);
            first.collect_dividers(a, path, dividers);
            path.pop();
            path.push(true);
            second.collect_dividers(b, path, dividers);
            path.pop();
        }
    }

    #[must_use]
    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            Self::Leaf(id) => *id == pane,
            Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    /// Replaces `target` with a split of it and `new`. Returns whether it was found.
    pub fn split(&mut self, target: PaneId, new: PaneId, kind: SplitKind) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    kind,
                    ratio: SCALE / 2,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new, kind) || second.split(target, new, kind)
            }
        }
    }

    /// Removes a pane, giving its space to its sibling. Returns `None` if this
    /// node was the pane itself, so nothing is left of it.
    #[must_use]
    pub fn remove(self, pane: PaneId) -> Option<Self> {
        match self {
            Self::Leaf(id) if id == pane => None,
            Self::Leaf(_) => Some(self),
            Self::Split {
                kind,
                ratio,
                first,
                second,
            } => match (first.remove(pane), second.remove(pane)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    kind,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    fn at_path_mut(&mut self, path: &[bool]) -> Option<&mut Self> {
        match (path.split_first(), self) {
            (None, node) => Some(node),
            (Some((side, rest)), Self::Split { first, second, .. }) => {
                if *side { second } else { first }.at_path_mut(rest)
            }
            (Some(_), Self::Leaf(_)) => None,
        }
    }

    /// Moves a divider so that it sits at the given position, for dragging it
    /// with the mouse.
    pub fn move_divider(&mut self, divider: &Divider, x: u16, y: u16) {
        let (start, position, length) = match divider.kind {
            SplitKind::Horizontal => (divider.area.x, x, divider.area.width),
            SplitKind::Vertical => (divider.area.y, y, divider.area.height),
        };
        let space = u32::from(length.saturating_sub(1)).max(1);
        // A half cell of rounding, so that the line lands where it was asked to.
        let offset = u32::from(position.saturating_sub(start));
        let new_ratio = ((offset * SCALE) + SCALE / 2) / space;
        if let Some(Self::Split { ratio, .. }) = self.at_path_mut(&divider.path) {
            *ratio = new_ratio.min(SCALE);
        }
    }

    /// The nearest split above `pane` that runs in the direction of `kind`,
    /// and whether the pane is on its far side.
    fn enclosing_split(&self, pane: PaneId, kind: SplitKind) -> Option<(Vec<bool>, bool)> {
        fn search(
            node: &Node,
            pane: PaneId,
            kind: SplitKind,
            path: &mut Vec<bool>,
        ) -> Option<(Vec<bool>, bool)> {
            let Node::Split {
                kind: node_kind,
                first,
                second,
                ..
            } = node
            else {
                return None;
            };
            for (side, child) in [(false, first), (true, second)] {
                if !child.contains(pane) {
                    continue;
                }
                path.push(side);
                let deeper = search(child, pane, kind, path);
                path.pop();
                return deeper.or_else(|| (*node_kind == kind).then(|| (path.clone(), side)));
            }
            None
        }
        search(self, pane, kind, &mut Vec::new())
    }

    /// Moves the border of a pane by some cells, like `resize-pane`. The border
    /// moved is the one between the pane and the rest of its enclosing split.
    pub fn resize(&mut self, pane: PaneId, area: Rect, direction: Direction, cells: u16) {
        let kind = match direction {
            Direction::Left | Direction::Right => SplitKind::Horizontal,
            Direction::Up | Direction::Down => SplitKind::Vertical,
        };
        let Some((path, _)) = self.enclosing_split(pane, kind) else {
            return;
        };
        let Some(divider) = self.dividers(area).into_iter().find(|d| d.path == path) else {
            return;
        };
        let (x, y) = (divider.line.x, divider.line.y);
        let (x, y) = match direction {
            Direction::Left => (x.saturating_sub(cells), y),
            Direction::Right => (x.saturating_add(cells), y),
            Direction::Up => (x, y.saturating_sub(cells)),
            Direction::Down => (x, y.saturating_add(cells)),
        };
        self.move_divider(&divider, x, y);
    }
}

/// How much two ranges overlap.
fn overlap(a_start: u16, a_end: u16, b_start: u16, b_end: u16) -> u16 {
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

/// The pane next to `from` in a direction, wrapping around the window edge like
/// tmux. Among several candidates, picks the one sharing the longest edge.
#[must_use]
pub fn neighbor(
    panes: &[(PaneId, Rect)],
    from: PaneId,
    direction: Direction,
    area: Rect,
) -> Option<PaneId> {
    let (_, current) = *panes.iter().find(|(id, _)| *id == from)?;
    let shared_edge = |rect: &Rect| match direction {
        Direction::Left | Direction::Right => {
            overlap(current.y, current.bottom(), rect.y, rect.bottom())
        }
        Direction::Up | Direction::Down => {
            overlap(current.x, current.right(), rect.x, rect.right())
        }
    };
    let touches = |rect: &Rect| match direction {
        Direction::Left => rect.right().saturating_add(1) == current.x,
        Direction::Right => current.right().saturating_add(1) == rect.x,
        Direction::Up => rect.bottom().saturating_add(1) == current.y,
        Direction::Down => current.bottom().saturating_add(1) == rect.y,
    };
    let at_far_edge = |rect: &Rect| match direction {
        Direction::Left => rect.right() == area.right(),
        Direction::Right => rect.x == area.x,
        Direction::Up => rect.bottom() == area.bottom(),
        Direction::Down => rect.y == area.y,
    };
    let best = |matches: &dyn Fn(&Rect) -> bool| {
        panes
            .iter()
            .filter(|(id, rect)| *id != from && !rect.is_empty() && matches(rect))
            .filter(|(_, rect)| shared_edge(rect) > 0)
            .max_by_key(|(_, rect)| shared_edge(rect))
            .map(|(id, _)| *id)
    };
    best(&touches).or_else(|| best(&at_far_edge))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 81,
        height: 25,
    };

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// Two panes side by side, with the right one split in two.
    fn three_panes() -> Node {
        let mut node = Node::Leaf(1);
        assert!(node.split(1, 2, SplitKind::Horizontal));
        assert!(node.split(2, 3, SplitKind::Vertical));
        node
    }

    #[test]
    fn a_single_pane_gets_everything() {
        assert_eq!(Node::Leaf(1).panes(AREA), [(1, AREA)]);
        assert_eq!(Node::Leaf(1).dividers(AREA), []);
    }

    #[test]
    fn splits_in_half_leaving_room_for_the_line() {
        let mut node = Node::Leaf(1);
        node.split(1, 2, SplitKind::Horizontal);
        assert_eq!(
            node.panes(AREA),
            [(1, rect(0, 0, 40, 25)), (2, rect(41, 0, 40, 25))]
        );
        let dividers = node.dividers(AREA);
        assert_eq!(dividers[0].line, rect(40, 0, 1, 25));
    }

    #[test]
    fn nested_splits_tile_the_area_exactly() {
        let node = three_panes();
        let panes = node.panes(AREA);
        assert_eq!(panes[1], (2, rect(41, 0, 40, 12)));
        assert_eq!(panes[2], (3, rect(41, 13, 40, 12)));

        // Every cell belongs to exactly one pane or divider.
        let dividers = node.dividers(AREA);
        for y in 0..AREA.height {
            for x in 0..AREA.width {
                let owners = panes.iter().filter(|(_, r)| r.contains(x, y)).count()
                    + dividers.iter().filter(|d| d.line.contains(x, y)).count();
                assert!(owners >= 1, "({x},{y}) belongs to nothing");
                assert!(owners <= 2, "({x},{y}) belongs to {owners} things");
            }
        }
    }

    #[test]
    fn keeps_proportions_across_a_resize() {
        let mut node = Node::Leaf(1);
        node.split(1, 2, SplitKind::Horizontal);
        let divider = node.dividers(AREA).remove(0);
        node.move_divider(&divider, 20, 0);
        assert_eq!(node.panes(AREA)[0].1.width, 20);

        let wide = Rect { width: 161, ..AREA };
        assert_eq!(node.panes(wide)[0].1.width, 40);
    }

    #[test]
    fn panes_survive_very_small_windows() {
        let node = three_panes();
        for width in 0..6 {
            for height in 0..6 {
                let area = rect(0, 0, width, height);
                for (_, pane) in node.panes(area) {
                    assert!(pane.right() <= width.max(1) + 1, "{pane:?} in {area:?}");
                }
            }
        }
    }

    #[test]
    fn removing_a_pane_gives_its_space_to_its_sibling() {
        let node = three_panes().remove(3).unwrap();
        assert_eq!(
            node.panes(AREA),
            [(1, rect(0, 0, 40, 25)), (2, rect(41, 0, 40, 25))]
        );
        let node = node.remove(1).unwrap();
        assert_eq!(node.panes(AREA), [(2, AREA)]);
        assert_eq!(node.remove(2), None);
    }

    #[test]
    fn dragging_a_divider_moves_it_to_the_mouse() {
        let mut node = three_panes();
        let divider = node.dividers(AREA).remove(0);
        for x in [1, 10, 33, 60, 79] {
            node.move_divider(&divider, x, 5);
            assert_eq!(node.dividers(AREA)[0].line.x, x);
        }
    }

    #[test]
    fn dragging_a_nested_divider_leaves_the_others_alone() {
        let mut node = three_panes();
        let divider = node.dividers(AREA).remove(1);
        assert_eq!(divider.path, [true]);
        node.move_divider(&divider, 50, 5);
        let dividers = node.dividers(AREA);
        assert_eq!(dividers[1].line.y, 5);
        assert_eq!(dividers[0].line.x, 40);
    }

    #[test]
    fn resize_moves_the_nearest_matching_border() {
        let mut node = three_panes();
        node.resize(1, AREA, Direction::Right, 5);
        assert_eq!(node.dividers(AREA)[0].line.x, 45);
        node.resize(3, AREA, Direction::Left, 10);
        assert_eq!(node.dividers(AREA)[0].line.x, 35);
        node.resize(2, AREA, Direction::Down, 2);
        assert_eq!(node.dividers(AREA)[1].line.y, 14);
    }

    #[test]
    fn resize_without_a_matching_split_does_nothing() {
        let mut node = Node::Leaf(1);
        node.split(1, 2, SplitKind::Horizontal);
        let before = node.clone();
        node.resize(1, AREA, Direction::Up, 3);
        assert_eq!(node, before);
    }

    #[test]
    fn finds_neighbors_and_wraps_around() {
        let node = three_panes();
        let panes = node.panes(AREA);
        assert_eq!(neighbor(&panes, 2, Direction::Down, AREA), Some(3));
        assert_eq!(neighbor(&panes, 3, Direction::Up, AREA), Some(2));
        assert_eq!(neighbor(&panes, 3, Direction::Left, AREA), Some(1));
        // Wrapping: past the bottom of the right column comes its top.
        assert_eq!(neighbor(&panes, 3, Direction::Down, AREA), Some(2));
        // Past the right edge comes the left edge.
        assert_eq!(neighbor(&panes, 2, Direction::Right, AREA), Some(1));
        // No other pane shares a row or column with an unsplit window.
        assert_eq!(neighbor(&panes, 1, Direction::Up, AREA), None);
    }
}
