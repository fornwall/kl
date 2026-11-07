//! How a window divides its area between panes: a tree of splits, each with any
//! number of children side by side or stacked, as in tmux.
//!
//! The size of a child is kept as a share, which is its size in cells as of when
//! it was last set. That makes what a layout or a drag sets exact, and has the
//! proportions survive the window changing size.

use crate::command::{Direction, Place, SplitKind};

/// Identifies a pane for the life of the server.
pub type PaneId = u32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
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

    /// The part it has in common with another one, if any.
    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let width = self.right().min(other.right()).checked_sub(x)?;
        let height = self.bottom().min(other.bottom()).checked_sub(y)?;
        let rect = Self {
            x,
            y,
            width,
            height,
        };
        (!rect.is_empty()).then_some(rect)
    }

    /// Its size in the direction of a split.
    pub const fn extent(self, kind: SplitKind) -> u16 {
        match kind {
            SplitKind::Horizontal => self.width,
            SplitKind::Vertical => self.height,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Leaf(PaneId),
    Split {
        kind: SplitKind,
        /// How the space is divided, one for each child. See the module docs.
        shares: Vec<u32>,
        children: Vec<Self>,
    },
}

/// tmux's preset layouts, in the order `next-layout` goes through them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    EvenHorizontal,
    EvenVertical,
    MainHorizontal,
    MainHorizontalMirrored,
    MainVertical,
    MainVerticalMirrored,
    Tiled,
}

/// What the main pane gets, which are tmux's defaults for `main-pane-width` and
/// `main-pane-height`.
const MAIN_PANE_WIDTH: u16 = 80;
const MAIN_PANE_HEIGHT: u16 = 24;

impl Layout {
    pub const ALL: [Self; 7] = [
        Self::EvenHorizontal,
        Self::EvenVertical,
        Self::MainHorizontal,
        Self::MainHorizontalMirrored,
        Self::MainVertical,
        Self::MainVerticalMirrored,
        Self::Tiled,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::EvenHorizontal => "even-horizontal",
            Self::EvenVertical => "even-vertical",
            Self::MainHorizontal => "main-horizontal",
            Self::MainHorizontalMirrored => "main-horizontal-mirrored",
            Self::MainVertical => "main-vertical",
            Self::MainVerticalMirrored => "main-vertical-mirrored",
            Self::Tiled => "tiled",
        }
    }

    /// The layout of a name, or of the start of only one name.
    pub fn find(name: &str) -> Option<Self> {
        let exact = Self::ALL.into_iter().find(|layout| layout.name() == name);
        let mut started = Self::ALL
            .iter()
            .filter(|layout| layout.name().starts_with(name));
        exact.or_else(|| {
            let first = started.next().copied();
            first.filter(|_| started.next().is_none())
        })
    }

    /// The layout after this one, or before it, which wraps around.
    pub fn step(self, forward: bool) -> Self {
        let count = Self::ALL.len();
        let position = Self::ALL.iter().position(|layout| *layout == self);
        let step = if forward { 1 } else { count - 1 };
        let position = position.map_or(0, |position| (position + step) % count);
        Self::ALL.get(position).copied().unwrap_or(self)
    }

    /// Whether the pane that tmux keeps first ends up last here.
    pub const fn is_mirrored(self) -> bool {
        matches!(
            self,
            Self::MainHorizontalMirrored | Self::MainVerticalMirrored
        )
    }

    /// Arranges panes in this layout. There has to be at least one.
    pub fn arrange(self, panes: &[PaneId], area: Rect) -> Option<Node> {
        let (horizontal, vertical) = (SplitKind::Horizontal, SplitKind::Vertical);
        match self {
            Self::EvenHorizontal => line_of_panes(panes, horizontal, area),
            Self::EvenVertical => line_of_panes(panes, vertical, area),
            Self::MainHorizontal => main_and_others(panes, vertical, area, false),
            Self::MainHorizontalMirrored => main_and_others(panes, vertical, area, true),
            Self::MainVertical => main_and_others(panes, horizontal, area, false),
            Self::MainVerticalMirrored => main_and_others(panes, horizontal, area, true),
            Self::Tiled => tiled(panes, area),
        }
    }
}

/// The sizes of children of the same size, where the first ones get what does
/// not divide evenly.
fn even_shares(length: u16, count: usize) -> Vec<u32> {
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    let space = u32::from(length).saturating_sub(count.saturating_sub(1));
    let each = space.checked_div(count).unwrap_or(0);
    let extra = space.checked_rem(count).unwrap_or(0);
    (0..count)
        .map(|index| (each + u32::from(index < extra)).max(1))
        .collect()
}

/// Panes one after the other, of even sizes.
fn line_of_panes(panes: &[PaneId], kind: SplitKind, area: Rect) -> Option<Node> {
    match panes {
        [] => None,
        [only] => Some(Node::Leaf(*only)),
        panes => Some(Node::Split {
            kind,
            shares: even_shares(area.extent(kind), panes.len()),
            children: panes.iter().copied().map(Node::Leaf).collect(),
        }),
    }
}

/// The first pane with the others in a line next to it, which comes first when
/// mirrored. The main pane gets what it wants, leaving a cell for the others.
fn main_and_others(panes: &[PaneId], kind: SplitKind, area: Rect, mirrored: bool) -> Option<Node> {
    let (main, others) = panes.split_first()?;
    let (wanted, others_kind) = match kind {
        SplitKind::Horizontal => (MAIN_PANE_WIDTH, SplitKind::Vertical),
        SplitKind::Vertical => (MAIN_PANE_HEIGHT, SplitKind::Horizontal),
    };
    let Some(others) = line_of_panes(others, others_kind, area) else {
        return Some(Node::Leaf(*main));
    };
    let length = area.extent(kind);
    let main_size = wanted.min(length.saturating_sub(2));
    let others_size = length.saturating_sub(main_size).saturating_sub(1);
    let (main_size, others_size) = (u32::from(main_size), u32::from(others_size));
    let main = Node::Leaf(*main);
    Some(if mirrored {
        Node::Split {
            kind,
            shares: vec![others_size, main_size],
            children: vec![others, main],
        }
    } else {
        Node::Split {
            kind,
            shares: vec![main_size, others_size],
            children: vec![main, others],
        }
    })
}

/// How long the cells of a grid are along a line of `length` cells, for `count`
/// of them. What does not divide evenly goes to the last one.
fn grid_shares(length: u16, count: usize, cells: usize) -> Vec<u32> {
    let columns = u32::try_from(cells).unwrap_or(u32::MAX).max(1);
    let length = u32::from(length);
    let size = (length.saturating_sub(columns - 1) / columns).max(1);
    let mut shares = vec![size; count];
    let used = u32::try_from(count).unwrap_or(0) * (size + 1) - 1;
    if let Some(last) = shares.last_mut() {
        *last += length.saturating_sub(used);
    }
    shares
}

/// A grid, filled up row by row. A single pane in a row gets all of it.
fn tiled(panes: &[PaneId], area: Rect) -> Option<Node> {
    if panes.len() < 2 {
        return panes.first().copied().map(Node::Leaf);
    }
    let (mut rows, mut columns) = (1, 1);
    while rows * columns < panes.len() {
        rows += 1;
        if rows * columns < panes.len() {
            columns += 1;
        }
    }
    let row = |panes: &[PaneId]| match panes {
        [only] => Node::Leaf(*only),
        panes => Node::Split {
            kind: SplitKind::Horizontal,
            shares: grid_shares(area.width, panes.len(), columns),
            children: panes.iter().copied().map(Node::Leaf).collect(),
        },
    };
    let mut children: Vec<Node> = panes.chunks(columns).map(row).collect();
    if let [only] = children.as_mut_slice() {
        return Some(std::mem::replace(only, Node::Leaf(0)));
    }
    Some(Node::Split {
        kind: SplitKind::Vertical,
        shares: grid_shares(area.height, children.len(), rows),
        children,
    })
}

/// A line between two children of a split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divider {
    /// Finds the split again, as the child to go to at each level.
    pub path: Vec<usize>,
    /// The child before the line.
    pub index: usize,
    pub kind: SplitKind,
    /// The one cell wide line itself.
    pub line: Rect,
    /// The area the split divides, which a drag moves the line within.
    pub area: Rect,
}

/// Divides `length` cells between children by their shares, leaving a cell for
/// the line between each two of them.
fn divide(length: u16, shares: &[u32]) -> Vec<u16> {
    let lines = u64::try_from(shares.len().saturating_sub(1)).unwrap_or(0);
    let space = u64::from(length).saturating_sub(lines);
    let total: u64 = shares.iter().copied().map(u64::from).sum();
    let mut sum = 0;
    let mut used = 0;
    let mut sizes: Vec<u16> = shares
        .iter()
        .map(|share| {
            sum += u64::from(*share);
            let end = space * sum / total.max(1);
            let size = end - used;
            used = end;
            u16::try_from(size).unwrap_or(u16::MAX)
        })
        .collect();
    // Everything keeps a cell for as long as there is room for it.
    while let Some(empty) = sizes.iter().position(|size| *size == 0) {
        let largest = sizes
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|(_, size)| *size);
        let Some((largest, 2..)) = largest else {
            break;
        };
        for (index, size) in sizes.iter_mut().enumerate() {
            *size = match index {
                _ if index == largest => *size - 1,
                _ if index == empty => 1,
                _ => *size,
            };
        }
    }
    sizes
}

/// The areas of the children of a split, leaving out the lines between them.
pub fn child_areas(area: Rect, kind: SplitKind, shares: &[u32]) -> Vec<Rect> {
    let (mut x, mut y) = (area.x, area.y);
    divide(area.extent(kind), shares)
        .into_iter()
        .map(|size| match kind {
            SplitKind::Horizontal => {
                let rect = Rect {
                    x,
                    width: size,
                    ..area
                };
                x = x.saturating_add(size).saturating_add(1);
                rect
            }
            SplitKind::Vertical => {
                let rect = Rect {
                    y,
                    height: size,
                    ..area
                };
                y = y.saturating_add(size).saturating_add(1);
                rect
            }
        })
        .collect()
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

/// How `length` cells divide between what was there and a new pane. Returns
/// their sizes in that order, with a cell left for the line between them.
fn sizes_with_new(length: u16, place: Place) -> (u32, u32) {
    let last = match place.size.map(|size| size.cells(length)) {
        None => (length.saturating_add(1) / 2).saturating_sub(1),
        Some(size) if place.before => length.saturating_sub(size).saturating_sub(1),
        Some(size) => size,
    };
    let last = last.clamp(1, length.saturating_sub(2).max(1));
    let first = length.saturating_sub(1).saturating_sub(last);
    let (first, last) = (u32::from(first), u32::from(last));
    if place.before {
        (last, first)
    } else {
        (first, last)
    }
}

/// Moves up to `cells` to the child `receiver`, from each of the `donors` in
/// turn for as much as it can spare.
fn transfer(
    sizes: &mut [u16],
    minimums: &[u16],
    donors: impl Iterator<Item = usize>,
    receiver: usize,
    cells: u16,
) {
    let mut moved = 0;
    for donor in donors {
        if let (Some(size), Some(minimum)) = (sizes.get_mut(donor), minimums.get(donor)) {
            let spare = size.saturating_sub(*minimum).min(cells - moved);
            *size -= spare;
            moved += spare;
        }
    }
    if let Some(size) = sizes.get_mut(receiver) {
        *size += moved;
    }
}

impl Node {
    /// Each pane with its area, in order from top left.
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
                shares,
                children,
            } => {
                let areas = child_areas(area, *kind, shares);
                for (child, area) in children.iter().zip(areas) {
                    child.collect_panes(area, panes);
                }
            }
        }
    }

    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        let mut dividers = Vec::new();
        self.collect_dividers(area, &mut Vec::new(), &mut dividers);
        dividers
    }

    fn collect_dividers(&self, area: Rect, path: &mut Vec<usize>, dividers: &mut Vec<Divider>) {
        let Self::Split {
            kind,
            shares,
            children,
        } = self
        else {
            return;
        };
        let areas = child_areas(area, *kind, shares);
        for (index, first) in areas
            .iter()
            .enumerate()
            .take(children.len().saturating_sub(1))
        {
            dividers.push(Divider {
                path: path.clone(),
                index,
                kind: *kind,
                line: divider_line(*first, area, *kind),
                area,
            });
        }
        for (index, (child, area)) in children.iter().zip(areas).enumerate() {
            path.push(index);
            child.collect_dividers(area, path, dividers);
            path.pop();
        }
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            Self::Leaf(id) => *id == pane,
            Self::Split { children, .. } => children.iter().any(|child| child.contains(pane)),
        }
    }

    /// The pane at the top left.
    pub fn first_pane(&self) -> PaneId {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { children, .. } => children.first().map_or(0, Self::first_pane),
        }
    }

    /// Puts `new` where `place` says relative to `target`.
    pub fn place(&mut self, target: PaneId, new: PaneId, place: Place, area: Rect) -> bool {
        if place.full {
            self.add_next_to_everything(new, place, area);
            true
        } else {
            self.split(target, new, place, area)
        }
    }

    /// Puts `new` at an end of this node, making the rest of it smaller.
    fn add_next_to_everything(&mut self, new: PaneId, place: Place, area: Rect) {
        let (own, added) = sizes_with_new(area.extent(place.kind), place);
        match self {
            Self::Split {
                kind,
                shares,
                children,
            } if *kind == place.kind => {
                let own = u16::try_from(own).unwrap_or(u16::MAX);
                *shares = divide(own, shares).into_iter().map(u32::from).collect();
                let index = if place.before { 0 } else { children.len() };
                shares.insert(index, added);
                children.insert(index, Self::Leaf(new));
            }
            _ => {
                let own_node = std::mem::replace(self, Self::Leaf(new));
                *self = if place.before {
                    Self::Split {
                        kind: place.kind,
                        shares: vec![added, own],
                        children: vec![Self::Leaf(new), own_node],
                    }
                } else {
                    Self::Split {
                        kind: place.kind,
                        shares: vec![own, added],
                        children: vec![own_node, Self::Leaf(new)],
                    }
                };
            }
        }
    }

    /// Puts `new` next to `target`, in the split it is in if that runs the right
    /// way, and in one of its own otherwise. Returns whether it was found.
    fn split(&mut self, target: PaneId, new: PaneId, place: Place, area: Rect) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                self.add_next_to_everything(new, place, area);
                true
            }
            Self::Leaf(_) => false,
            Self::Split {
                kind,
                shares,
                children,
            } => {
                let position = children
                    .iter()
                    .position(|child| *child == Self::Leaf(target));
                if let Some(index) = position.filter(|_| *kind == place.kind) {
                    let sizes = divide(area.extent(*kind), shares);
                    *shares = sizes.iter().copied().map(u32::from).collect();
                    let size = sizes.get(index).copied().unwrap_or(0);
                    let (own, added) = sizes_with_new(size, place);
                    if let Some(share) = shares.get_mut(index) {
                        *share = own;
                    }
                    let index = index + usize::from(!place.before);
                    shares.insert(index, added);
                    children.insert(index, Self::Leaf(new));
                    return true;
                }
                let areas = child_areas(area, *kind, shares);
                children
                    .iter_mut()
                    .zip(areas)
                    .any(|(child, area)| child.split(target, new, place, area))
            }
        }
    }

    /// Evens out the split a pane is in, or the nearest one above it that this
    /// makes a difference for. Returns whether it made one anywhere.
    pub fn spread(&mut self, pane: PaneId, area: Rect) -> bool {
        let Self::Split {
            kind,
            shares,
            children,
        } = self
        else {
            return false;
        };
        let Some(position) = children.iter().position(|child| child.contains(pane)) else {
            return false;
        };
        let areas = child_areas(area, *kind, shares);
        let child_area = areas.get(position).copied().unwrap_or(area);
        if children
            .get_mut(position)
            .is_some_and(|child| child.spread(pane, child_area))
        {
            return true;
        }
        let length = area.extent(*kind);
        let even = even_shares(length, children.len());
        let sizes = divide(length, shares);
        let changed = sizes
            .iter()
            .copied()
            .map(u32::from)
            .ne(even.iter().copied());
        *shares = even;
        changed
    }

    /// Puts panes in the place of what is here, in order.
    pub fn fill(&mut self, panes: &mut impl Iterator<Item = PaneId>) {
        match self {
            Self::Leaf(id) => *id = panes.next().unwrap_or(*id),
            Self::Split { children, .. } => {
                for child in children {
                    child.fill(panes);
                }
            }
        }
    }

    /// Puts two panes in each other's place. One of them can be somewhere else,
    /// which makes this replace the other one.
    pub fn swap(&mut self, a: PaneId, b: PaneId) {
        match self {
            Self::Leaf(id) if *id == a => *id = b,
            Self::Leaf(id) if *id == b => *id = a,
            Self::Leaf(_) => {}
            Self::Split { children, .. } => {
                for child in children {
                    child.swap(a, b);
                }
            }
        }
    }

    /// Removes a pane, giving its space to the child before it, or after it for
    /// the first. Returns `None` if this node was the pane itself, so nothing is
    /// left of it.
    pub fn remove(self, pane: PaneId) -> Option<Self> {
        let Self::Split {
            kind,
            mut shares,
            mut children,
        } = self
        else {
            return (self != Self::Leaf(pane)).then_some(self);
        };
        let split = |shares, children| Self::Split {
            kind,
            shares,
            children,
        };
        let Some(index) = children.iter().position(|child| child.contains(pane)) else {
            return Some(split(shares, children));
        };
        let child = children.remove(index);
        if let Some(child) = child.remove(pane) {
            children.insert(index, child);
            return Some(split(shares, children));
        }
        let share = shares.remove(index);
        if let Some(neighbor) = shares.get_mut(index.saturating_sub(1)) {
            *neighbor = neighbor.saturating_add(share).saturating_add(1);
        }
        match children.len() {
            0 => None,
            1 => children.pop(),
            _ => Some(split(shares, children)),
        }
    }

    fn at_path_mut(&mut self, path: &[usize]) -> Option<&mut Self> {
        match (path.split_first(), self) {
            (None, node) => Some(node),
            (Some((index, rest)), Self::Split { children, .. }) => {
                children.get_mut(*index)?.at_path_mut(rest)
            }
            (Some(_), Self::Leaf(_)) => None,
        }
    }

    /// The fewest cells this node can do with in the direction of a split.
    fn minimum(&self, direction: SplitKind) -> u16 {
        let Self::Split { kind, children, .. } = self else {
            return 1;
        };
        let minimums = children.iter().map(|child| child.minimum(direction));
        if *kind == direction {
            minimums
                .fold(0_u16, |sum, size| {
                    sum.saturating_add(size).saturating_add(1)
                })
                .saturating_sub(1)
        } else {
            minimums.max().unwrap_or(1)
        }
    }

    /// Runs `change` on the sizes of the children of a split, with what each of
    /// them needs at least, and keeps the outcome.
    fn resize_children(&mut self, length: u16, change: impl FnOnce(&mut [u16], &[u16])) {
        if let Self::Split {
            kind,
            shares,
            children,
        } = self
        {
            let mut sizes = divide(length, shares);
            let minimums: Vec<u16> = children.iter().map(|child| child.minimum(*kind)).collect();
            change(&mut sizes, &minimums);
            *shares = sizes.into_iter().map(u32::from).collect();
        }
    }

    /// Moves a divider so that it sits at the given position, for dragging it
    /// with the mouse. A line in the way gets pushed along, as in tmux.
    pub fn move_divider(&mut self, divider: &Divider, x: u16, y: u16) {
        let Some(split) = self.at_path_mut(&divider.path) else {
            return;
        };
        let Self::Split {
            kind,
            shares,
            children,
        } = split
        else {
            return;
        };
        let index = divider.index;
        let count = children.len();
        let length = divider.area.extent(*kind);
        let before = child_areas(divider.area, *kind, shares);
        let before = before.get(index).copied().unwrap_or_default();
        let (line, wanted) = match kind {
            SplitKind::Horizontal => (before.right(), x),
            SplitKind::Vertical => (before.bottom(), y),
        };
        split.resize_children(length, |sizes, minimums| {
            if wanted > line {
                transfer(sizes, minimums, index + 1..count, index, wanted - line);
            } else {
                let donors = (0..=index).rev();
                transfer(sizes, minimums, donors, index + 1, line - wanted);
            }
        });
    }

    /// How far what holds a pane in the nearest split of this kind reaches, and
    /// whether that comes last in the split.
    fn extent_in_split(&self, pane: PaneId, area: Rect, wanted: SplitKind) -> Option<(u16, bool)> {
        let Self::Split {
            kind,
            shares,
            children,
        } = self
        else {
            return None;
        };
        let position = children.iter().position(|child| child.contains(pane))?;
        let child_area = child_areas(area, *kind, shares).get(position).copied()?;
        let nearer = children
            .get(position)
            .and_then(|child| child.extent_in_split(pane, child_area, wanted));
        let last = position + 1 == children.len();
        nearer.or_else(|| (*kind == wanted).then_some((child_area.extent(wanted), last)))
    }

    /// Gives a pane a size, like `resize-pane -x` and `-y`, which is a matter of
    /// moving the same border as [`Self::resize`] does. A pane in a stack of them
    /// goes by the size of the stack.
    pub fn resize_to(&mut self, pane: PaneId, area: Rect, kind: SplitKind, size: u16) {
        let Some((current, last)) = self.extent_in_split(pane, area, kind) else {
            return;
        };
        // The border is before the last child, so that one grows the other way.
        let forward = (size > current) != last;
        let direction = match kind {
            SplitKind::Horizontal if forward => Direction::Right,
            SplitKind::Horizontal => Direction::Left,
            SplitKind::Vertical if forward => Direction::Down,
            SplitKind::Vertical => Direction::Up,
        };
        self.resize(pane, area, direction, size.abs_diff(current));
    }

    /// Moves the border of a pane by some cells, like `resize-pane`: the one
    /// after it, or before it for the last pane of a split. A pane out of room
    /// on one side grows the other way.
    pub fn resize(&mut self, pane: PaneId, area: Rect, direction: Direction, cells: u16) -> bool {
        let wanted = match direction {
            Direction::Left | Direction::Right => SplitKind::Horizontal,
            Direction::Up | Direction::Down => SplitKind::Vertical,
        };
        let Self::Split {
            kind,
            shares,
            children,
        } = self
        else {
            return false;
        };
        let kind = *kind;
        let count = children.len();
        let Some(position) = children.iter().position(|child| child.contains(pane)) else {
            return false;
        };
        let areas = child_areas(area, kind, shares);
        let child_area = areas.get(position).copied().unwrap_or(area);
        let resized = children
            .get_mut(position)
            .is_some_and(|child| child.resize(pane, child_area, direction, cells));
        if resized || kind != wanted {
            return resized;
        }
        let index = position.min(count.saturating_sub(2));
        let grow = matches!(direction, Direction::Right | Direction::Down);
        self.resize_children(area.extent(kind), |sizes, minimums| {
            if grow {
                let donors = (index + 1..count).chain((0..index).rev());
                transfer(sizes, minimums, donors, index, cells);
            } else {
                transfer(sizes, minimums, (0..=index).rev(), index + 1, cells);
            }
        });
        true
    }
}

/// How much two ranges overlap.
fn overlap(a_start: u16, a_end: u16, b_start: u16, b_end: u16) -> u16 {
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

/// The pane next to `from` in a direction, wrapping around the window edge like
/// tmux. Among several candidates, picks the one sharing the longest edge.
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
    use crate::command::Size;

    const fn beside() -> Place {
        Place {
            kind: SplitKind::Horizontal,
            before: false,
            full: false,
            size: None,
        }
    }

    const fn below() -> Place {
        Place {
            kind: SplitKind::Vertical,
            ..beside()
        }
    }

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
        assert!(node.split(1, 2, beside(), AREA));
        assert!(node.split(2, 3, below(), AREA));
        node
    }

    /// Three panes in a row, of 20, 30 and 29 cells.
    fn row() -> Node {
        Node::Split {
            kind: SplitKind::Horizontal,
            shares: vec![20, 30, 29],
            children: vec![Node::Leaf(1), Node::Leaf(2), Node::Leaf(3)],
        }
    }

    fn widths(node: &Node) -> Vec<u16> {
        let panes = node.panes(AREA);
        panes.iter().map(|(_, rect)| rect.width).collect()
    }

    #[test]
    fn a_single_pane_gets_everything() {
        assert_eq!(Node::Leaf(1).panes(AREA), [(1, AREA)]);
        assert_eq!(Node::Leaf(1).dividers(AREA), []);
    }

    #[test]
    fn splits_in_half_leaving_room_for_the_line() {
        let mut node = Node::Leaf(1);
        node.split(1, 2, beside(), AREA);
        assert_eq!(
            node.panes(AREA),
            [(1, rect(0, 0, 40, 25)), (2, rect(41, 0, 40, 25))]
        );
        let dividers = node.dividers(AREA);
        assert_eq!(dividers[0].line, rect(40, 0, 1, 25));
    }

    #[test]
    fn splits_with_the_rounding_of_tmux() {
        let area = rect(0, 0, 80, 24);
        let mut node = Node::Leaf(1);
        node.split(1, 2, beside(), area);
        assert_eq!(node.panes(area)[0].1.width, 40);
        assert_eq!(node.panes(area)[1].1.width, 39);
        node.split(2, 3, beside(), area);
        node.split(3, 4, below(), area);
        node.split(1, 5, beside(), area);
        // What tmux does with the same splits in the same order.
        assert_eq!(
            node.panes(area),
            [
                (1, rect(0, 0, 20, 24)),
                (5, rect(21, 0, 19, 24)),
                (2, rect(41, 0, 19, 24)),
                (3, rect(61, 0, 19, 12)),
                (4, rect(61, 13, 19, 11)),
            ]
        );
    }

    /// What tmux makes of panes joined with these flags.
    #[test]
    fn places_panes_by_flags() {
        let area = rect(0, 0, 80, 24);
        let mut node = Node::Leaf(1);
        let place = Place {
            before: true,
            size: Some(Size::Cells(10)),
            ..below()
        };
        node.place(1, 2, place, area);
        assert_eq!(
            node.panes(area),
            [(2, rect(0, 0, 80, 10)), (1, rect(0, 11, 80, 13))]
        );
        let place = Place {
            size: Some(Size::Percent(30)),
            ..below()
        };
        node.place(2, 3, place, area);
        assert_eq!(
            node.panes(area),
            [
                (2, rect(0, 0, 80, 6)),
                (3, rect(0, 7, 80, 3)),
                (1, rect(0, 11, 80, 13)),
            ]
        );
    }

    #[test]
    fn a_full_pane_goes_next_to_everything() {
        let area = rect(0, 0, 80, 24);
        let mut node = Node::Leaf(1);
        node.place(1, 2, below(), area);
        let place = Place {
            full: true,
            ..beside()
        };
        node.place(2, 3, place, area);
        assert_eq!(
            node.panes(area),
            [
                (1, rect(0, 0, 40, 12)),
                (2, rect(0, 13, 40, 11)),
                (3, rect(41, 0, 39, 24)),
            ]
        );
    }

    #[test]
    fn a_full_pane_joins_a_split_in_the_same_direction() {
        let area = rect(0, 0, 80, 24);
        let mut node = Node::Leaf(1);
        node.place(1, 2, beside(), area);
        let place = Place {
            full: true,
            before: true,
            ..beside()
        };
        node.place(2, 3, place, area);
        assert_eq!(node.dividers(area).len(), 2);
        assert_eq!(node.panes(area)[0], (3, rect(0, 0, 40, 24)));
    }

    const FIVE: [PaneId; 5] = [1, 2, 3, 4, 5];

    fn arranged(layout: Layout, area: Rect) -> Vec<Rect> {
        let node = layout.arrange(&FIVE, area).unwrap();
        let panes = node.panes(area);
        // The main pane is last in a mirrored layout, and first in tmux's list.
        let ids: Vec<PaneId> = panes.iter().map(|(id, _)| *id).collect();
        let main_first = if layout.is_mirrored() {
            [2, 3, 4, 5, 1]
        } else {
            FIVE
        };
        assert_eq!(ids, main_first);
        panes.iter().map(|(_, rect)| *rect).collect()
    }

    /// These are what tmux makes of five panes in windows of these sizes.
    #[test]
    fn layouts_match_tmux_in_a_small_window() {
        let area = rect(0, 0, 80, 24);
        assert_eq!(
            arranged(Layout::EvenHorizontal, area),
            [
                rect(0, 0, 16, 24),
                rect(17, 0, 15, 24),
                rect(33, 0, 15, 24),
                rect(49, 0, 15, 24),
                rect(65, 0, 15, 24),
            ]
        );
        assert_eq!(
            arranged(Layout::EvenVertical, area),
            [
                rect(0, 0, 80, 4),
                rect(0, 5, 80, 4),
                rect(0, 10, 80, 4),
                rect(0, 15, 80, 4),
                rect(0, 20, 80, 4),
            ]
        );
        assert_eq!(
            arranged(Layout::MainHorizontal, area),
            [
                rect(0, 0, 80, 22),
                rect(0, 23, 20, 1),
                rect(21, 23, 19, 1),
                rect(41, 23, 19, 1),
                rect(61, 23, 19, 1),
            ]
        );
        assert_eq!(
            arranged(Layout::MainVertical, area),
            [
                rect(0, 0, 78, 24),
                rect(79, 0, 1, 6),
                rect(79, 7, 1, 5),
                rect(79, 13, 1, 5),
                rect(79, 19, 1, 5),
            ]
        );
        assert_eq!(
            arranged(Layout::Tiled, area),
            [
                rect(0, 0, 39, 7),
                rect(40, 0, 40, 7),
                rect(0, 8, 39, 7),
                rect(40, 8, 40, 7),
                rect(0, 16, 80, 8),
            ]
        );
    }

    #[test]
    fn layouts_match_tmux_in_a_large_window() {
        let area = rect(0, 0, 200, 50);
        assert_eq!(
            arranged(Layout::EvenVertical, area),
            [
                rect(0, 0, 200, 10),
                rect(0, 11, 200, 9),
                rect(0, 21, 200, 9),
                rect(0, 31, 200, 9),
                rect(0, 41, 200, 9),
            ]
        );
        assert_eq!(
            arranged(Layout::MainHorizontal, area),
            [
                rect(0, 0, 200, 24),
                rect(0, 25, 50, 25),
                rect(51, 25, 49, 25),
                rect(101, 25, 49, 25),
                rect(151, 25, 49, 25),
            ]
        );
        assert_eq!(
            arranged(Layout::MainVertical, area),
            [
                rect(0, 0, 80, 50),
                rect(81, 0, 119, 12),
                rect(81, 13, 119, 12),
                rect(81, 26, 119, 12),
                rect(81, 39, 119, 11),
            ]
        );
        assert_eq!(
            arranged(Layout::Tiled, area),
            [
                rect(0, 0, 99, 16),
                rect(100, 0, 100, 16),
                rect(0, 17, 99, 16),
                rect(100, 17, 100, 16),
                rect(0, 34, 200, 16),
            ]
        );
    }

    #[test]
    fn mirrored_layouts_have_the_main_pane_on_the_other_side() {
        let area = rect(0, 0, 200, 50);
        assert_eq!(
            arranged(Layout::MainHorizontalMirrored, area),
            [
                rect(0, 0, 50, 25),
                rect(51, 0, 49, 25),
                rect(101, 0, 49, 25),
                rect(151, 0, 49, 25),
                rect(0, 26, 200, 24),
            ]
        );
        assert_eq!(
            arranged(Layout::MainVerticalMirrored, area),
            [
                rect(0, 0, 119, 12),
                rect(0, 13, 119, 12),
                rect(0, 26, 119, 12),
                rect(0, 39, 119, 11),
                rect(120, 0, 80, 50),
            ]
        );
    }

    #[test]
    fn tiles_a_full_grid() {
        let area = rect(0, 0, 80, 24);
        let node = Layout::Tiled.arrange(&[1, 2, 3, 4], area).unwrap();
        assert_eq!(
            node.panes(area),
            [
                (1, rect(0, 0, 39, 11)),
                (2, rect(40, 0, 40, 11)),
                (3, rect(0, 12, 39, 12)),
                (4, rect(40, 12, 40, 12)),
            ]
        );
    }

    #[test]
    fn every_layout_works_for_any_number_of_panes_in_any_window() {
        let panes: Vec<PaneId> = (0..12).collect();
        for layout in Layout::ALL {
            assert_eq!(layout.arrange(&[], AREA), None);
            assert_eq!(layout.arrange(&[7], AREA), Some(Node::Leaf(7)));
            for count in 2..panes.len() {
                for area in [AREA, rect(0, 0, 3, 3), rect(0, 0, 0, 0), rect(0, 0, 1, 40)] {
                    let node = layout.arrange(&panes[..count], area).unwrap();
                    assert_eq!(node.panes(area).len(), count, "{layout:?}");
                }
                let node = layout.arrange(&panes[..count], AREA).unwrap();
                // Every cell belongs to exactly one pane or divider.
                let rects = node.panes(AREA);
                let dividers = node.dividers(AREA);
                for y in 0..AREA.height {
                    for x in 0..AREA.width {
                        let owners = rects.iter().filter(|(_, r)| r.contains(x, y)).count()
                            + dividers.iter().filter(|d| d.line.contains(x, y)).count();
                        assert!(owners >= 1, "({x},{y}) of {layout:?} with {count} panes");
                    }
                }
            }
        }
    }

    #[test]
    fn finds_layouts_by_the_start_of_their_names() {
        assert_eq!(Layout::find("tiled"), Some(Layout::Tiled));
        assert_eq!(Layout::find("even-v"), Some(Layout::EvenVertical));
        assert_eq!(Layout::find("main-vertical"), Some(Layout::MainVertical));
        assert_eq!(Layout::find("main-v"), None);
        assert_eq!(Layout::find("main"), None);
        assert_eq!(Layout::find("foo"), None);
    }

    #[test]
    fn steps_through_layouts_in_the_order_of_tmux() {
        assert_eq!(Layout::EvenHorizontal.step(true), Layout::EvenVertical);
        assert_eq!(Layout::Tiled.step(true), Layout::EvenHorizontal);
        assert_eq!(Layout::EvenHorizontal.step(false), Layout::Tiled);
    }

    #[test]
    fn spreading_evens_out_the_split_of_a_pane() {
        let mut node = row();
        assert!(node.spread(2, AREA));
        assert_eq!(widths(&node), [27, 26, 26]);
        assert!(!node.spread(2, AREA));
    }

    #[test]
    fn spreading_goes_up_when_there_is_nothing_to_do_where_it_starts() {
        let area = rect(0, 0, 80, 24);
        let mut node = Node::Split {
            kind: SplitKind::Horizontal,
            shares: vec![10, 69],
            children: vec![
                Node::Split {
                    kind: SplitKind::Vertical,
                    shares: vec![12, 11],
                    children: vec![Node::Leaf(1), Node::Leaf(2)],
                },
                Node::Leaf(3),
            ],
        };
        assert!(node.spread(1, area));
        assert_eq!(node.panes(area)[2].1.width, 39);
    }

    #[test]
    fn fills_the_places_in_a_layout() {
        let mut node = three_panes();
        let before = node.panes(AREA);
        node.fill(&mut [3, 1, 2].into_iter());
        let after = node.panes(AREA);
        assert_eq!(after[0], (3, before[0].1));
        assert_eq!(after[1], (1, before[1].1));
        assert_eq!(after[2], (2, before[2].1));
    }

    #[test]
    fn swapping_leaves_the_sizes_where_they_are() {
        let mut node = three_panes();
        let before = node.panes(AREA);
        node.swap(1, 3);
        let after = node.panes(AREA);
        assert_eq!(after[0], (3, before[0].1));
        assert_eq!(after[1], before[1]);
        assert_eq!(after[2], (1, before[2].1));
    }

    #[test]
    fn splitting_along_a_split_adds_to_it() {
        let mut node = row();
        node.split(2, 4, beside(), AREA);
        assert_eq!(widths(&node), [20, 15, 14, 29]);
        assert_eq!(node.dividers(AREA).len(), 3);
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
        node.split(1, 2, beside(), AREA);
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
    fn a_removed_pane_gives_its_space_to_the_one_before_it() {
        assert_eq!(widths(&row().remove(2).unwrap()), [51, 29]);
        assert_eq!(widths(&row().remove(3).unwrap()), [20, 60]);
        assert_eq!(widths(&row().remove(1).unwrap()), [51, 29]);
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
        assert_eq!(divider.path, [1]);
        node.move_divider(&divider, 50, 5);
        let dividers = node.dividers(AREA);
        assert_eq!(dividers[1].line.y, 5);
        assert_eq!(dividers[0].line.x, 40);
    }

    #[test]
    fn dragging_a_divider_leaves_the_other_children_alone() {
        let mut node = row();
        let divider = node.dividers(AREA).remove(0);
        node.move_divider(&divider, 30, 0);
        assert_eq!(widths(&node), [30, 20, 29]);
    }

    #[test]
    fn dragging_a_divider_pushes_the_next_one_along() {
        let mut node = row();
        let divider = node.dividers(AREA).remove(0);
        node.move_divider(&divider, 70, 0);
        assert_eq!(widths(&node), [70, 1, 8]);
        // A drag goes on from where the line went.
        node.move_divider(&divider, 60, 0);
        assert_eq!(widths(&node), [60, 11, 8]);
    }

    #[test]
    fn a_drag_stops_at_what_nested_panes_need() {
        let mut node = Node::Split {
            kind: SplitKind::Horizontal,
            shares: vec![40, 40],
            children: vec![Node::Leaf(1), row()],
        };
        let divider = node.dividers(AREA).remove(0);
        node.move_divider(&divider, 80, 0);
        // Three panes and the lines between them need five cells.
        assert_eq!(node.dividers(AREA)[0].line.x, 75);
    }

    #[test]
    fn resize_moves_the_border_after_a_pane() {
        let mut node = row();
        node.resize(2, AREA, Direction::Right, 5);
        assert_eq!(widths(&node), [20, 35, 24]);
        node.resize(2, AREA, Direction::Left, 10);
        assert_eq!(widths(&node), [20, 25, 34]);
    }

    #[test]
    fn resize_moves_the_border_before_the_last_pane() {
        let mut node = row();
        node.resize(3, AREA, Direction::Right, 5);
        assert_eq!(widths(&node), [20, 35, 24]);
    }

    #[test]
    fn resize_goes_on_past_panes_with_no_room_left() {
        let mut node = row();
        node.resize(1, AREA, Direction::Right, 40);
        assert_eq!(widths(&node), [60, 1, 18]);
        // With nothing to take from on the right, it comes from the left.
        node.resize(2, AREA, Direction::Right, 30);
        assert_eq!(widths(&node), [47, 31, 1]);
        node.resize(2, AREA, Direction::Left, 60);
        assert_eq!(widths(&node), [17, 1, 61]);
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
        node.split(1, 2, beside(), AREA);
        let before = node.clone();
        node.resize(1, AREA, Direction::Up, 3);
        assert_eq!(node, before);
    }

    /// The sizes of the panes after giving one of them a size. The numbers in
    /// the tests come from tmux, with the same panes in a window of 81x25.
    fn sizes_after(pane: PaneId, kind: SplitKind, size: u16) -> Vec<(u16, u16)> {
        let mut node = three_panes();
        node.resize_to(pane, AREA, kind, size);
        let panes = node.panes(AREA);
        panes.iter().map(|(_, r)| (r.width, r.height)).collect()
    }

    #[test]
    fn resizes_to_a_size_like_tmux() {
        let (across, down) = (SplitKind::Horizontal, SplitKind::Vertical);
        assert_eq!(sizes_after(1, across, 30), [(30, 25), (50, 12), (50, 12)]);
        // What is in the last place of the split gets there from the other side.
        assert_eq!(sizes_after(2, across, 30), [(50, 25), (30, 12), (30, 12)]);
        assert_eq!(sizes_after(3, across, 30), [(50, 25), (30, 12), (30, 12)]);
        assert_eq!(sizes_after(2, down, 5), [(40, 25), (40, 5), (40, 19)]);
        assert_eq!(sizes_after(3, down, 5), [(40, 25), (40, 19), (40, 5)]);
    }

    #[test]
    fn resizing_to_a_size_stops_where_there_is_no_room() {
        let across = SplitKind::Horizontal;
        assert_eq!(sizes_after(1, across, 5000), [(79, 25), (1, 12), (1, 12)]);
        assert_eq!(sizes_after(1, across, 0), [(1, 25), (79, 12), (79, 12)]);
    }

    #[test]
    fn resizing_to_a_size_needs_a_split_that_way() {
        let down = SplitKind::Vertical;
        assert_eq!(sizes_after(1, down, 5), [(40, 25), (40, 12), (40, 12)]);
        let mut node = Node::Leaf(1);
        node.resize_to(1, AREA, SplitKind::Horizontal, 10);
        assert_eq!(node, Node::Leaf(1));
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
