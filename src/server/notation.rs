//! Layouts as text, the way tmux has them. `#{window_layout}` gives tmux's JSON,
//! and `select-layout` takes that as well as the notation from before it, with a
//! checksum in front, which tmux still takes and shows to control clients.

use super::layout::{Node, PaneId, Rect, child_areas};
use super::session::Window;
use crate::command::SplitKind;

/// How deep cells and JSON can go. Everything that works on a layout recurses,
/// and no window has any use for this many splits inside each other.
const MAX_DEPTH: usize = 64;

/// How many cells a layout can have. Those without a pane to go in them get
/// removed one by one, which should not keep the server busy for long.
const MAX_CELLS: usize = 4096;

/// tmux's `WINDOW_MAXIMUM`.
const MAX_SIZE: i64 = 10_000;

/// What a layout says about a pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PaneCell {
    /// The number of the pane that goes here. The order of the cells decides
    /// without one.
    index: Option<i64>,
    active: bool,
    /// Its place in the order of panes last active, the latest first.
    last: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Content {
    Pane(PaneCell),
    Split(SplitKind, Vec<Cell>),
}

/// A part of a layout as written down.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cell {
    width: u32,
    height: u32,
    content: Content,
}

impl Cell {
    fn count(&self) -> usize {
        match &self.content {
            Content::Pane(_) => 1,
            Content::Split(_, children) => children.iter().map(Self::count).sum(),
        }
    }

    /// Its size in the direction of a split.
    const fn extent(&self, kind: SplitKind) -> u32 {
        match kind {
            SplitKind::Horizontal => self.width,
            SplitKind::Vertical => self.height,
        }
    }

    /// Older versions of tmux could get the size at the top wrong, so that one
    /// goes by what is in it, as tmux does.
    fn fix_top(&mut self) {
        let Content::Split(kind, children) = &self.content else {
            return;
        };
        let Some(last) = children.last() else {
            return;
        };
        let sizes = children.iter().map(|child| child.extent(*kind));
        let total = sizes.fold(0_u32, |sum, size| {
            sum.saturating_add(size).saturating_add(1)
        });
        let total = total.saturating_sub(1);
        (self.width, self.height) = match kind {
            SplitKind::Horizontal => (total, last.height),
            SplitKind::Vertical => (last.width, total),
        };
    }

    /// Whether everything fits together: the sizes in a split add up to the size
    /// of the split, and what is in it is as long as it is the other way.
    fn fits(&self) -> bool {
        let Content::Split(kind, children) = &self.content else {
            return true;
        };
        let across = match kind {
            SplitKind::Horizontal => SplitKind::Vertical,
            SplitKind::Vertical => SplitKind::Horizontal,
        };
        let total: u64 = children
            .iter()
            .map(|child| u64::from(child.extent(*kind)) + 1)
            .sum();
        total.saturating_sub(1) == u64::from(self.extent(*kind))
            && children
                .iter()
                .all(|child| child.extent(across) == self.extent(across) && child.fits())
    }

    /// The tree of this, with panes numbered in order. What each one says about
    /// itself goes in `panes`.
    fn node(&self, panes: &mut Vec<PaneCell>) -> Node {
        match &self.content {
            Content::Pane(pane) => {
                let id = PaneId::try_from(panes.len()).unwrap_or(PaneId::MAX);
                panes.push(*pane);
                Node::Leaf(id)
            }
            Content::Split(kind, children) => {
                let mut nodes: Vec<Node> = children.iter().map(|cell| cell.node(panes)).collect();
                if nodes.len() == 1
                    && let Some(only) = nodes.pop()
                {
                    return only;
                }
                Node::Split {
                    kind: *kind,
                    shares: children.iter().map(|cell| cell.extent(*kind)).collect(),
                    children: nodes,
                }
            }
        }
    }
}

/// tmux's checksum of the notation with a checksum, over what comes after it.
fn checksum(text: &str) -> u16 {
    text.bytes().fold(0, |sum: u16, byte| {
        sum.rotate_right(1).wrapping_add(u16::from(byte))
    })
}

/// A place in the notation with a checksum.
struct Classic<'a> {
    text: &'a [u8],
    at: usize,
    cells: usize,
}

impl Classic<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn eat(&mut self, wanted: u8) -> Option<()> {
        (self.peek() == Some(wanted)).then(|| self.at += 1)
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.at += 1;
        }
        let digits = self.text.get(start..self.at)?;
        std::str::from_utf8(digits).ok()?.parse().ok()
    }

    /// `WxH,X,Y`, then the id of a pane or what is in a split. What follows a
    /// comma can also be the next cell, as a cell can do without an id.
    fn cell(&mut self, depth: usize) -> Option<Cell> {
        self.cells += 1;
        if depth >= MAX_DEPTH || self.cells > MAX_CELLS {
            return None;
        }
        let width = self.number()?;
        self.eat(b'x')?;
        let height = self.number()?;
        self.eat(b',')?;
        self.number()?;
        self.eat(b',')?;
        self.number()?;
        if self.peek() == Some(b',') {
            let saved = self.at;
            self.at += 1;
            let id = self.number();
            if id.is_none() || self.peek() == Some(b'x') {
                self.at = saved;
            }
        }
        let (kind, close) = match self.peek() {
            None | Some(b',' | b'}' | b']') => {
                let content = Content::Pane(PaneCell::default());
                return Some(Cell {
                    width,
                    height,
                    content,
                });
            }
            Some(b'{') => (SplitKind::Horizontal, b'}'),
            Some(b'[') => (SplitKind::Vertical, b']'),
            Some(_) => return None,
        };
        let mut children = Vec::new();
        loop {
            self.at += 1;
            children.push(self.cell(depth + 1)?);
            if self.peek() != Some(b',') {
                break;
            }
        }
        self.eat(close)?;
        let content = Content::Split(kind, children);
        Some(Cell {
            width,
            height,
            content,
        })
    }
}

fn parse_classic(text: &str) -> Result<Cell, String> {
    let header = text
        .split_once(',')
        .filter(|(sum, _)| sum.len() == 4 && sum.bytes().all(|digit| digit.is_ascii_hexdigit()));
    let (sum, layout) = header.ok_or("malformed layout header")?;
    if u16::from_str_radix(sum, 16).ok() != Some(checksum(layout)) {
        return Err("invalid layout checksum".to_owned());
    }
    let mut reader = Classic {
        text: layout.as_bytes(),
        at: 0,
        cells: 0,
    };
    let cell = reader.cell(0).ok_or("invalid layout")?;
    if reader.peek().is_some() {
        return Err("trailing data".to_owned());
    }
    Ok(cell)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Json {
    Null,
    Bool(bool),
    Number(i64),
    Text(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

/// A place in some JSON.
struct JsonReader<'a> {
    text: &'a [u8],
    at: usize,
}

impl JsonReader<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    /// Goes past a character after any space, if that is what comes next.
    fn eat(&mut self, wanted: u8) -> bool {
        self.skip_space();
        let found = self.peek() == Some(wanted);
        self.at += usize::from(found);
        found
    }

    fn word(&mut self, word: &[u8]) -> Option<()> {
        let end = self.at.checked_add(word.len())?;
        let found = self.text.get(self.at..end)? == word;
        found.then(|| self.at = end)
    }

    /// The text of a string. Nothing here needs its escapes for more than
    /// getting past them.
    fn string(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let mut bytes = Vec::new();
        loop {
            let c = self.peek()?;
            self.at += 1;
            match c {
                b'"' => return String::from_utf8(bytes).ok(),
                b'\\' => {
                    bytes.push(self.peek()?);
                    self.at += 1;
                }
                c => bytes.push(c),
            }
        }
    }

    fn number(&mut self) -> Option<i64> {
        let start = self.at;
        self.at += usize::from(self.peek() == Some(b'-'));
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.at += 1;
        }
        let digits = self.text.get(start..self.at)?;
        std::str::from_utf8(digits).ok()?.parse().ok()
    }

    fn array(&mut self, depth: usize) -> Option<Json> {
        let mut values = Vec::new();
        if self.eat(b']') {
            return Some(Json::Array(values));
        }
        loop {
            values.push(self.value(depth + 1)?);
            if !self.eat(b',') {
                break;
            }
        }
        self.eat(b']').then_some(Json::Array(values))
    }

    fn object(&mut self, depth: usize) -> Option<Json> {
        let mut members = Vec::new();
        if self.eat(b'}') {
            return Some(Json::Object(members));
        }
        loop {
            let key = self.string()?;
            if !self.eat(b':') {
                return None;
            }
            members.push((key, self.value(depth + 1)?));
            if !self.eat(b',') {
                break;
            }
        }
        self.eat(b'}').then_some(Json::Object(members))
    }

    /// A value at some depth. Each cell takes two: its object, and the array of
    /// what is in it.
    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth >= MAX_DEPTH * 2 {
            return None;
        }
        self.skip_space();
        match self.peek()? {
            b'{' => {
                self.at += 1;
                self.object(depth)
            }
            b'[' => {
                self.at += 1;
                self.array(depth)
            }
            b'"' => self.string().map(Json::Text),
            b't' => self.word(b"true").map(|()| Json::Bool(true)),
            b'f' => self.word(b"false").map(|()| Json::Bool(false)),
            b'n' => self.word(b"null").map(|()| Json::Null),
            _ => self.number().map(Json::Number),
        }
    }
}

fn parse_json(text: &str) -> Option<Json> {
    let mut reader = JsonReader {
        text: text.as_bytes(),
        at: 0,
    };
    let value = reader.value(0)?;
    reader.skip_space();
    reader.peek().is_none().then_some(value)
}

/// The members of a JSON object, with tmux's words for what is wrong with them.
#[derive(Clone, Copy)]
struct Members<'a>(&'a [(String, Json)]);

impl<'a> Members<'a> {
    fn find(self, key: &str) -> Option<&'a Json> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    fn get(self, key: &str) -> Result<&'a Json, String> {
        self.find(key)
            .ok_or_else(|| format!("key \"{key}\" not found"))
    }

    fn number(self, key: &str) -> Result<i64, String> {
        match self.get(key)? {
            Json::Number(number) => Ok(*number),
            _ => Err(format!("key \"{key}\" expected a number")),
        }
    }

    fn size(self, key: &str, what: &str) -> Result<u32, String> {
        let size = self.number(key)?;
        u32::try_from(size)
            .ok()
            .filter(|_| (1..=MAX_SIZE).contains(&size))
            .ok_or_else(|| format!("invalid {what} {size}"))
    }

    fn pane(self) -> Result<PaneCell, String> {
        let active = match self.find("a") {
            None => false,
            Some(Json::Bool(active)) => *active,
            Some(_) => return Err("key \"a\" expected a boolean".to_owned()),
        };
        let last = self.find("l").map(|_| self.number("l")).transpose()?;
        let index = Some(self.number("i")?);
        Ok(PaneCell {
            index,
            active,
            last,
        })
    }

    fn children(self) -> Result<Vec<Cell>, String> {
        let Json::Array(children) = self.get("c")? else {
            return Err("key \"c\" expected an array".to_owned());
        };
        if children.len() < 2 {
            return Err("nodes must have more than one child".to_owned());
        }
        children.iter().map(json_cell).collect()
    }
}

fn json_cell(json: &Json) -> Result<Cell, String> {
    let Json::Object(members) = json else {
        return Err("expected an object".to_owned());
    };
    let members = Members(members);
    let Json::Text(kind) = members.get("t")? else {
        return Err("key \"t\" expected a string".to_owned());
    };
    let width = members.size("w", "width")?;
    let height = members.size("h", "height")?;
    members.number("x")?;
    members.number("y")?;
    let content = match kind.as_str() {
        "p" => Content::Pane(members.pane()?),
        "h" => Content::Split(SplitKind::Horizontal, members.children()?),
        "v" => Content::Split(SplitKind::Vertical, members.children()?),
        other => return Err(format!("unknown cell type \"{other}\"")),
    };
    Ok(Cell {
        width,
        height,
        content,
    })
}

fn parse_layout_json(text: &str) -> Result<Cell, String> {
    let Some(Json::Object(members)) = parse_json(text) else {
        return Err("invalid layout json".to_owned());
    };
    let members = Members(&members);
    members.number("V")?;
    match members.get("L")? {
        layout @ Json::Object(_) => json_cell(layout),
        _ => Err("key \"L\" expected an object".to_owned()),
    }
}

/// What is left of panes being active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Focus {
    pub active: Option<PaneId>,
    pub last: Option<PaneId>,
}

/// A layout with panes in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arranged {
    pub root: Node,
    /// What the layout says about which panes were active, if it can say that.
    pub focus: Option<Focus>,
}

/// Whether two cells say they are for the same pane.
fn has_duplicates(cells: &[PaneCell]) -> bool {
    let mut indexes: Vec<i64> = cells.iter().filter_map(|cell| cell.index).collect();
    indexes.sort_unstable();
    indexes.windows(2).any(|pair| pair.first() == pair.last())
}

/// Which pane goes in each of the cells, of which the numbers say which comes
/// first. Without them, the cells are in order already.
fn assign(cells: &[PaneCell], panes: &[PaneId]) -> Vec<PaneId> {
    let mut order: Vec<usize> = (0..cells.len()).collect();
    order.sort_by_key(|cell| cells.get(*cell).and_then(|cell| cell.index));
    let mut assigned = vec![0; cells.len()];
    for (cell, pane) in order.into_iter().zip(panes) {
        if let Some(slot) = assigned.get_mut(cell) {
            *slot = *pane;
        }
    }
    assigned
}

/// Fits panes in a layout written down. Like in tmux, a layout can have more
/// room than it takes, and the cells at the bottom right give way, but there
/// have to be enough cells for all panes.
pub fn arrange(text: &str, panes: &[PaneId]) -> Result<Arranged, String> {
    let json = text.starts_with('{');
    let mut cell = if json {
        parse_layout_json(text)?
    } else {
        parse_classic(text)?
    };
    let count = cell.count();
    if count > MAX_CELLS {
        return Err("invalid layout".to_owned());
    }
    if panes.len() > count {
        return Err(format!("have {} panes but need {count}", panes.len()));
    }
    cell.fix_top();
    if !cell.fits() {
        return Err("size mismatch after applying layout".to_owned());
    }
    let mut cells = Vec::new();
    let mut root = cell.node(&mut cells);
    if has_duplicates(&cells) {
        return Err("duplicate pane index".to_owned());
    }
    while cells.len() > panes.len() {
        cells.pop();
        let last = PaneId::try_from(cells.len()).unwrap_or(PaneId::MAX);
        let Some(rest) = root.clone().remove(last) else {
            break;
        };
        root = rest;
    }
    let assigned = assign(&cells, panes);
    root.fill(&mut assigned.iter().copied());
    let pane_of = |position: usize| assigned.get(position).copied();
    let with_cells = || cells.iter().enumerate();
    let active = with_cells().find(|(_, cell)| cell.active);
    let last = with_cells()
        .filter(|(_, cell)| cell.last.is_some())
        .min_by_key(|(_, cell)| cell.last);
    let focus = json.then(|| Focus {
        active: active.and_then(|(position, _)| pane_of(position)),
        last: last.and_then(|(position, _)| pane_of(position)),
    });
    Ok(Arranged { root, focus })
}

/// Writes a cell the way `#{window_layout}` has it.
fn write_cell(text: &mut String, kind: char, area: Rect) {
    let Rect {
        x,
        y,
        width,
        height,
    } = area;
    let cell = format!("{{\"t\":\"{kind}\",\"w\":{width},\"h\":{height},\"x\":{x},\"y\":{y}");
    text.push_str(&cell);
}

fn write_pane(text: &mut String, window: &Window, order: &[PaneId], id: PaneId, area: Rect) {
    write_cell(text, 'p', area);
    if id == window.active {
        text.push_str(",\"a\":true");
    } else if window.last_active == Some(id) {
        text.push_str(",\"l\":0");
    }
    let index = order.iter().position(|pane| *pane == id).unwrap_or(0);
    let tail = format!(",\"i\":{index},\"I\":\"%{id}\"}}");
    text.push_str(&tail);
}

fn write_node(text: &mut String, window: &Window, order: &[PaneId], node: &Node, area: Rect) {
    match node {
        Node::Leaf(id) => write_pane(text, window, order, *id, area),
        Node::Split {
            kind,
            shares,
            children,
        } => {
            let letter = match kind {
                SplitKind::Horizontal => 'h',
                SplitKind::Vertical => 'v',
            };
            write_cell(text, letter, area);
            text.push_str(",\"c\":[");
            let areas = child_areas(area, *kind, shares);
            for (position, (child, area)) in children.iter().zip(areas).enumerate() {
                if position > 0 {
                    text.push(',');
                }
                write_node(text, window, order, child, area);
            }
            text.push_str("]}");
        }
    }
}

/// The layout of a window as `#{window_layout}` has it. `order` has its panes by
/// their numbers in layouts. With `zoomed`, that is what can be seen of a window
/// that is zoomed in on a pane, as for `#{window_visible_layout}`.
pub fn dump(window: &Window, order: &[PaneId], area: Rect, zoomed: bool) -> String {
    let mut text = String::from("{\"V\":2,\"L\":");
    if zoomed {
        write_pane(&mut text, window, order, window.active, area);
    } else {
        write_node(&mut text, window, order, &window.root, area);
    }
    text.push('}');
    text
}

#[cfg(test)]
#[expect(
    clippy::literal_string_with_formatting_args,
    reason = "these are layouts, not formats"
)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 40,
    };

    /// What tmux tells a control client, with the checksums it worked out.
    const FROM_TMUX: [(&str, usize); 5] = [
        (
            "56f7,120x40,0,0[120x19,0,0{59x19,0,0,0,60x19,60,0,1},120x20,0,20,2]",
            3,
        ),
        (
            "1f9f,120x40,0,0{80x40,0,0,0,39x40,81,0[39x20,81,0,1,39x19,81,21,2]}",
            3,
        ),
        (
            "6d9e,120x40,0,0[120x13,0,0,0,120x13,0,14,1,120x12,0,28,2]",
            3,
        ),
        (
            "c90d,120x40,0,0[120x24,0,0,0,120x15,0,25{60x15,0,25,1,59x15,61,25,2}]",
            3,
        ),
        ("aafd,120x40,0,0,0", 1),
    ];

    fn panes(count: u32) -> Vec<PaneId> {
        (10..10 + count).collect()
    }

    fn rects(arranged: &Arranged) -> Vec<(PaneId, u16, u16, u16, u16)> {
        let panes = arranged.root.panes(AREA);
        panes
            .iter()
            .map(|(id, r)| (*id, r.x, r.y, r.width, r.height))
            .collect()
    }

    /// The notation of a layout, with the checksum it needs.
    fn with_checksum(layout: &str) -> String {
        format!("{:04x},{layout}", checksum(layout))
    }

    fn error_of(text: &str, count: u32) -> String {
        arrange(text, &panes(count)).unwrap_err()
    }

    #[test]
    fn works_out_tmuxs_checksums() {
        for (text, _) in FROM_TMUX {
            let (sum, layout) = text.split_once(',').unwrap();
            assert_eq!(format!("{:04x}", checksum(layout)), sum, "{text}");
        }
    }

    #[test]
    fn takes_what_tmux_shows_to_control_clients() {
        for (text, count) in FROM_TMUX {
            let count = u32::try_from(count).unwrap();
            assert!(arrange(text, &panes(count)).is_ok(), "{text}");
        }
        let arranged = arrange(FROM_TMUX[1].0, &panes(3)).unwrap();
        assert_eq!(
            rects(&arranged),
            [
                (10, 0, 0, 80, 40),
                (11, 81, 0, 39, 20),
                (12, 81, 21, 39, 19)
            ]
        );
        assert_eq!(arranged.focus, None);
    }

    #[test]
    fn checksums_take_either_case_of_hexadecimal() {
        let layout = with_checksum("120x40,0,0{50x40,0,0,5,69x40,51,0,6}");
        assert!(arrange(&layout.to_uppercase().replace('X', "x"), &panes(2)).is_ok());
    }

    #[test]
    fn says_what_is_wrong_with_the_notation() {
        for text in [
            "tiled",
            "abcd",
            "abcde",
            "abc,1x1,0,0",
            "zzzz,120x40,0,0",
            "",
        ] {
            assert_eq!(error_of(text, 1), "malformed layout header", "{text}");
        }
        assert_eq!(error_of("abcd,", 1), "invalid layout checksum");
        assert_eq!(
            error_of(
                "56f8,120x40,0,0[120x19,0,0{59x19,0,0,0,60x19,60,0,1},120x20,0,20,2]",
                3
            ),
            "invalid layout checksum"
        );
        assert_eq!(error_of(&with_checksum("nonsense"), 1), "invalid layout");
        assert_eq!(error_of(&with_checksum("1x1,0"), 1), "invalid layout");
        assert_eq!(
            error_of(&with_checksum("2x1,0,0{1x1,0,0"), 1),
            "invalid layout"
        );
        assert_eq!(
            error_of(&with_checksum("2x1,0,0{1x1,0,0]"), 1),
            "invalid layout"
        );
        assert_eq!(error_of(&with_checksum("1x1,0,0,1x"), 1), "trailing data");
    }

    #[test]
    fn a_pane_can_do_without_an_id() {
        let arranged = arrange(
            &with_checksum("120x40,0,0{30x40,0,0,89x40,31,0}"),
            &panes(2),
        );
        let widths: Vec<u16> = rects(&arranged.unwrap()).iter().map(|r| r.3).collect();
        assert_eq!(widths, [30, 89]);
    }

    #[test]
    fn a_split_of_one_is_just_what_is_in_it() {
        let text = with_checksum("120x40,0,0{120x40,0,0,5}");
        assert_eq!(arrange(&text, &panes(1)).unwrap().root, Node::Leaf(10));
    }

    fn json_of(children: &str) -> String {
        format!(r#"{{"V":2,"L":{{"t":"h","w":120,"h":40,"x":0,"y":0,"c":[{children}]}}}}"#)
    }

    fn json_pane(width: u32, x: u32, rest: &str) -> String {
        format!(r#"{{"t":"p","w":{width},"h":40,"x":{x},"y":0,{rest}}}"#)
    }

    fn two_panes(left: &str, right: &str) -> String {
        json_of(&format!(
            "{},{}",
            json_pane(30, 0, left),
            json_pane(89, 31, right)
        ))
    }

    #[test]
    fn takes_tmuxs_json() {
        let text = concat!(
            r#"{"V":2,"L":{"t":"h","w":120,"h":40,"x":0,"y":0,"c":["#,
            r#"{"t":"v","w":60,"h":40,"x":0,"y":0,"c":["#,
            r#"{"t":"p","w":60,"h":34,"x":0,"y":0,"l":0,"i":0,"I":"%0"},"#,
            r#"{"t":"p","w":60,"h":5,"x":0,"y":35,"a":true,"i":1,"I":"%3"}]},"#,
            r#"{"t":"v","w":59,"h":40,"x":61,"y":0,"c":["#,
            r#"{"t":"p","w":59,"h":20,"x":61,"y":0,"l":2,"i":2,"I":"%1"},"#,
            r#"{"t":"p","w":59,"h":19,"x":61,"y":21,"l":1,"i":3,"I":"%2"}]}]}}"#,
        );
        let arranged = arrange(text, &panes(4)).unwrap();
        assert_eq!(
            rects(&arranged),
            [
                (10, 0, 0, 60, 34),
                (11, 0, 35, 60, 5),
                (12, 61, 0, 59, 20),
                (13, 61, 21, 59, 19)
            ]
        );
        let focus = Focus {
            active: Some(11),
            last: Some(10),
        };
        assert_eq!(arranged.focus, Some(focus));
    }

    #[test]
    fn panes_go_by_their_numbers_rather_than_their_ids() {
        let text = two_panes(r#""i":1,"I":"%0""#, r#""i":0,"I":"%1""#);
        let arranged = arrange(&text, &panes(2)).unwrap();
        assert_eq!(rects(&arranged), [(11, 0, 0, 30, 40), (10, 31, 0, 89, 40)]);

        let text = two_panes(r#""i":7"#, r#""i":9"#);
        let arranged = arrange(&text, &panes(2)).unwrap();
        assert_eq!(rects(&arranged), [(10, 0, 0, 30, 40), (11, 31, 0, 89, 40)]);
    }

    #[test]
    fn json_can_leave_out_which_pane_was_active() {
        let arranged = arrange(&two_panes(r#""i":0"#, r#""i":1"#), &panes(2)).unwrap();
        let focus = Focus {
            active: None,
            last: None,
        };
        assert_eq!(arranged.focus, Some(focus));
    }

    #[test]
    fn says_what_is_wrong_with_json() {
        let bare = r#"{"V":2,"L":{"t":"p","w":120,"h":40,"x":0,"y":0}}"#;
        assert_eq!(error_of(bare, 1), r#"key "i" not found"#);
        assert_eq!(error_of("{}", 1), r#"key "V" not found"#);
        assert_eq!(error_of(r#"{"V":2}"#, 1), r#"key "L" not found"#);
        assert_eq!(
            error_of(r#"{"V":"x","L":{}}"#, 1),
            r#"key "V" expected a number"#
        );
        assert_eq!(
            error_of(r#"{"V":2,"L":5}"#, 1),
            r#"key "L" expected an object"#
        );
        for (text, error) in [
            (
                r#"{"t":5,"w":1,"h":1,"x":0,"y":0}"#,
                r#"key "t" expected a string"#,
            ),
            (
                r#"{"t":"p","w":"a","h":1,"x":0,"y":0}"#,
                r#"key "w" expected a number"#,
            ),
            (r#"{"t":"p","w":1,"h":1,"y":0}"#, r#"key "x" not found"#),
            (
                r#"{"t":"p","w":1,"h":1,"x":0,"y":0,"i":"a"}"#,
                r#"key "i" expected a number"#,
            ),
            (
                r#"{"t":"p","w":1,"h":1,"x":0,"y":0,"i":0,"a":1}"#,
                r#"key "a" expected a boolean"#,
            ),
            (
                r#"{"t":"h","w":1,"h":1,"x":0,"y":0}"#,
                r#"key "c" not found"#,
            ),
            (
                r#"{"t":"h","w":1,"h":1,"x":0,"y":0,"c":5}"#,
                r#"key "c" expected an array"#,
            ),
            (
                r#"{"t":"h","w":1,"h":1,"x":0,"y":0,"c":[]}"#,
                "nodes must have more than one child",
            ),
            (
                r#"{"t":"z","w":1,"h":1,"x":0,"y":0}"#,
                r#"unknown cell type "z""#,
            ),
            (r#"{"t":"p","w":0,"h":1,"x":0,"y":0}"#, "invalid width 0"),
            (r#"{"t":"p","w":-1,"h":1,"x":0,"y":0}"#, "invalid width -1"),
            (
                r#"{"t":"p","w":1,"h":10001,"x":0,"y":0}"#,
                "invalid height 10001",
            ),
        ] {
            let layout = format!(r#"{{"V":2,"L":{text}}}"#);
            assert_eq!(error_of(&layout, 1), error, "{layout}");
        }
    }

    #[test]
    fn broken_json_is_not_a_layout() {
        let good = r#"{"V":2,"L":{"t":"p","w":120,"h":40,"x":0,"y":0,"i":0}}"#;
        assert!(arrange(good, &panes(1)).is_ok());
        for broken in [
            "{",
            "{V:2}",
            r#"{"V"}"#,
            r#"{"V":}"#,
            r#"{"V":tru}"#,
            r#"{"V":"abc}"#,
            r#"{"V":2,}"#,
            r#"{"V":1.5}"#,
        ] {
            assert_eq!(error_of(broken, 1), "invalid layout json", "{broken}");
        }
        assert_eq!(error_of(&format!("{good} x"), 1), "invalid layout json");
        assert_eq!(
            error_of(good.trim_end_matches('}'), 1),
            "invalid layout json"
        );
    }

    #[test]
    fn json_leaves_room_for_space_and_what_it_does_not_know() {
        let text = r#" {"V" : 2, "zz":[1,{"a":null}], "L": {"t":"p", "w":120,
             "h":40,"x":0,"y":0,"i":0,"q":"\"r\""}} "#;
        assert_eq!(error_of(text, 1), "malformed layout header");
        let text = text.trim_start();
        assert_eq!(arrange(text, &panes(1)).unwrap().root, Node::Leaf(10));
    }

    #[test]
    fn nothing_goes_too_deep() {
        let opening = r#"{"t":"h","w":1,"h":1,"x":0,"y":0,"c":["#.repeat(300);
        let text = format!(r#"{{"V":2,"L":{opening}"#);
        assert_eq!(error_of(&text, 1), "invalid layout json");
        let text = format!(r#"{{"V":2,"L":{}}}"#, nested_json(70));
        assert_eq!(error_of(&text, 1), "invalid layout json");

        let opening = "1x1,0,0{".repeat(100_000);
        assert_eq!(error_of(&with_checksum(&opening), 1), "invalid layout");
    }

    /// Cells inside of each other to some depth, which is fine as JSON.
    fn nested_json(depth: u32) -> String {
        let pane = r#"{"t":"p","w":1,"h":1,"x":0,"y":0,"i":0}"#;
        (0..depth).fold(pane.to_owned(), |inner, _| {
            format!(r#"{{"t":"h","w":3,"h":1,"x":0,"y":0,"c":[{inner},{pane}]}}"#)
        })
    }

    #[test]
    fn a_layout_cannot_have_any_number_of_cells() {
        let cell = "1x1,0,0,";
        let cells = cell.repeat(MAX_CELLS + 1);
        let layout = format!("9x9,0,0{{{}}}", cells.trim_end_matches(','));
        assert_eq!(error_of(&with_checksum(&layout), 1), "invalid layout");
    }

    #[test]
    fn panes_need_enough_cells() {
        assert_eq!(error_of(FROM_TMUX[4].0, 2), "have 2 panes but need 1");
        let text = two_panes(r#""i":0"#, r#""i":1"#);
        assert_eq!(error_of(&text, 3), "have 3 panes but need 2");
    }

    #[test]
    fn cells_without_panes_give_way_from_the_bottom_right() {
        let arranged = arrange(FROM_TMUX[1].0, &panes(2)).unwrap();
        assert_eq!(rects(&arranged), [(10, 0, 0, 80, 40), (11, 81, 0, 39, 40)]);
        let arranged = arrange(FROM_TMUX[1].0, &panes(1)).unwrap();
        assert_eq!(arranged.root, Node::Leaf(10));
    }

    #[test]
    fn sizes_have_to_add_up_inside_a_split() {
        let wrong = with_checksum("120x40,0,0{30x20,0,0,5,89x40,31,0,6}");
        assert_eq!(error_of(&wrong, 2), "size mismatch after applying layout");
        let inner = with_checksum("120x40,0,0{30x40,0,0,5,89x40,31,0[89x10,31,0,6,89x10,31,11,7]}");
        assert_eq!(error_of(&inner, 3), "size mismatch after applying layout");
    }

    #[test]
    fn the_size_at_the_top_goes_by_what_is_in_it() {
        let text = with_checksum("120x40,0,0{200x40,0,0,5,89x40,31,0,6}");
        assert!(arrange(&text, &panes(2)).is_ok());
    }

    #[test]
    fn sizes_can_be_as_silly_as_they_like() {
        let huge = u32::MAX;
        let text = with_checksum(&format!("9x9,0,0{{{huge}x9,0,0,0,{huge}x9,0,0,1}}"));
        assert_eq!(error_of(&text, 2), "size mismatch after applying layout");
        assert_eq!(
            error_of(&with_checksum("99999999999x1,0,0"), 1),
            "invalid layout"
        );
    }

    #[test]
    fn duplicate_numbers_are_a_mistake() {
        let text = two_panes(r#""i":0"#, r#""i":0"#);
        assert_eq!(error_of(&text, 2), "duplicate pane index");
    }

    fn window(root: Node, active: PaneId, last_active: Option<PaneId>) -> Window {
        Window {
            id: 0,
            index: 0,
            name: String::new(),
            automatic_name: false,
            root,
            active,
            last_active,
            zoomed: false,
            bell: false,
            monitor: super::super::session::Monitor::Off,
            layout: None,
            old_layout: None,
            next_name: None,
        }
    }

    #[test]
    fn writes_what_tmux_writes() {
        let text = concat!(
            r#"{"V":2,"L":{"t":"h","w":120,"h":40,"x":0,"y":0,"c":["#,
            r#"{"t":"v","w":60,"h":40,"x":0,"y":0,"c":["#,
            r#"{"t":"p","w":60,"h":34,"x":0,"y":0,"l":0,"i":0,"I":"%0"},"#,
            r#"{"t":"p","w":60,"h":5,"x":0,"y":35,"a":true,"i":1,"I":"%3"}]},"#,
            r#"{"t":"v","w":59,"h":40,"x":61,"y":0,"c":["#,
            r#"{"t":"p","w":59,"h":20,"x":61,"y":0,"i":2,"I":"%1"},"#,
            r#"{"t":"p","w":59,"h":19,"x":61,"y":21,"i":3,"I":"%2"}]}]}}"#,
        );
        let order = [0, 3, 1, 2];
        let arranged = arrange(text, &order).unwrap();
        let window = window(arranged.root, 3, Some(0));
        assert_eq!(dump(&window, &order, AREA, false), text);
    }

    #[test]
    fn writes_what_can_be_seen_of_a_zoomed_window() {
        let node = Node::Split {
            kind: SplitKind::Horizontal,
            shares: vec![60, 59],
            children: vec![Node::Leaf(4), Node::Leaf(7)],
        };
        let window = window(node, 7, None);
        assert_eq!(
            dump(&window, &[4, 7], AREA, true),
            r#"{"V":2,"L":{"t":"p","w":120,"h":40,"x":0,"y":0,"a":true,"i":1,"I":"%7"}}"#
        );
    }

    #[test]
    fn what_it_writes_it_can_read_again() {
        let text = two_panes(r#""a":true,"i":0"#, r#""l":0,"i":1"#);
        let arranged = arrange(&text, &[4, 7]).unwrap();
        let window = window(arranged.root.clone(), 4, Some(7));
        let again = arrange(&dump(&window, &[4, 7], AREA, false), &[4, 7]).unwrap();
        assert_eq!(again, arranged);
    }
}
