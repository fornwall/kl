//! Composes the panes, borders and status line into one grid, then sends the
//! outer terminal only what changed since the last frame.

use super::images::Images;
use super::layout::{Divider, Rect};
use super::session::{Session, Window};
use super::ui::{List, Ui};
use crate::ghostty::{
    self, Cell, CursorShape, DEFAULT_POINTER_SHAPE, Link, Match, Progress, Rgb, STYLE_BLINK,
    STYLE_BOLD, STYLE_FAINT, STYLE_INVERSE, STYLE_INVISIBLE, STYLE_ITALIC, STYLE_OVERLINE,
    STYLE_STRIKETHROUGH, STYLE_UNDERLINE_MASK, STYLE_UNDERLINE_SHIFT, Style, StyleColor, Text,
    Width,
};
use crate::sys;

const BLACK: u8 = 0;
const RED: u8 = 1;
const GREEN: u8 = 2;
const YELLOW: u8 = 3;
const BLUE: u8 = 4;
const MAGENTA: u8 = 5;
const CYAN: u8 = 6;

/// The digits of tmux's clock and `display-panes`, five bits for each row.
const BIG_DIGITS: [[u8; 5]; 10] = [
    [0b11111, 0b10001, 0b10001, 0b10001, 0b11111],
    [0b00001, 0b00001, 0b00001, 0b00001, 0b00001],
    [0b11111, 0b00001, 0b11111, 0b10000, 0b11111],
    [0b11111, 0b00001, 0b11111, 0b00001, 0b11111],
    [0b10001, 0b10001, 0b11111, 0b00001, 0b00001],
    [0b11111, 0b10000, 0b11111, 0b00001, 0b11111],
    [0b11111, 0b10000, 0b11111, 0b10001, 0b11111],
    [0b11111, 0b00001, 0b00001, 0b00001, 0b00001],
    [0b11111, 0b10001, 0b11111, 0b10001, 0b11111],
    [0b11111, 0b10001, 0b11111, 0b00001, 0b11111],
];

fn colored(fg: u8, bg: u8) -> Style {
    Style {
        fg: StyleColor::palette(fg),
        bg: StyleColor::palette(bg),
        ..Style::default()
    }
}

/// tmux's defaults for the status line, and for copy mode and messages.
fn status_style() -> Style {
    colored(BLACK, GREEN)
}

/// tmux's `window-status-bell-style` and `window-status-activity-style`.
fn alert_style() -> Style {
    Style {
        flags: STYLE_INVERSE,
        ..status_style()
    }
}

fn mode_style() -> Style {
    colored(BLACK, YELLOW)
}

/// tmux's `copy-mode-match-style` and `copy-mode-current-match-style`.
fn match_style(current: bool) -> Style {
    colored(BLACK, if current { MAGENTA } else { CYAN })
}

fn border_style(active: bool) -> Style {
    if active {
        Style {
            fg: StyleColor::palette(GREEN),
            ..Style::default()
        }
    } else {
        Style::default()
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Grid {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
}

impl Grid {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); usize::from(cols) * usize::from(rows)],
        }
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        (x < self.cols && y < self.rows)
            .then(|| usize::from(y) * usize::from(self.cols) + usize::from(x))
    }

    fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if let Some(slot) = self.index(x, y).and_then(|index| self.cells.get_mut(index)) {
            *slot = cell;
        }
    }

    fn clear(&mut self) {
        self.cells.fill(Cell::default());
    }

    /// Gives a cell another style, unless it is selected, which goes first.
    fn highlight(&mut self, x: u16, y: u16, style: Style) {
        if let Some(cell) = self.index(x, y).and_then(|index| self.cells.get_mut(index))
            && !cell.selected
        {
            cell.style = style;
        }
    }

    /// Blanks the left half of a double width character whose right half is at
    /// this position, since text is about to go there. Without this, the left
    /// half would be drawn as the whole character, hiding the new text.
    fn split_wide(&mut self, x: u16, y: u16) {
        let is_tail = |grid: &Self, index: usize| {
            grid.cells
                .get(index)
                .is_some_and(|cell| cell.width == Width::Tail)
        };
        if let Some(index) = self.index(x, y).filter(|index| is_tail(self, *index))
            && let Some(head) = index.checked_sub(1).and_then(|i| self.cells.get_mut(i))
        {
            *head = Cell {
                style: head.style,
                ..Cell::default()
            };
        }
    }

    /// Writes text, returning the column after it. Stops at `limit`, and leaves
    /// out a double width character that would not fit.
    fn put_str(&mut self, mut x: u16, y: u16, text: &str, style: Style, limit: u16) -> u16 {
        for c in text.chars() {
            let width = ghostty::char_width(c);
            if width == 0 {
                continue;
            }
            if x.saturating_add(width) > limit {
                break;
            }
            let mut encoded = [0; 4];
            self.set(
                x,
                y,
                Cell {
                    text: Text::new(c.encode_utf8(&mut encoded)),
                    style,
                    width: if width == 2 {
                        Width::Wide
                    } else {
                        Width::Narrow
                    },
                    ..Cell::default()
                },
            );
            if width == 2 {
                self.set(
                    x + 1,
                    y,
                    Cell {
                        style,
                        width: Width::Tail,
                        ..Cell::default()
                    },
                );
            }
            x += width;
        }
        x
    }

    fn fill_row(&mut self, y: u16, style: Style) {
        for x in 0..self.cols {
            self.set(
                x,
                y,
                Cell {
                    style,
                    ..Cell::default()
                },
            );
        }
    }
}

/// How wide some text is on the terminal.
fn text_width(text: &str) -> u16 {
    text.chars().map(ghostty::char_width).sum()
}

/// Where each window is on the status line, for clicks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    pub start: u16,
    pub end: u16,
    pub window: u32,
}

/// Where the cursor belongs and its DECSCUSR style, if it is to be seen at all.
type CursorTarget = Option<(u16, u16, u8)>;

#[derive(Default)]
pub struct Renderer {
    shown: Grid,
    next: Grid,
    /// Nothing on the terminal can be trusted, as after attaching.
    reset: bool,
    /// The cursor as last sent, if the terminal's can be trusted.
    cursor: Option<CursorTarget>,
    /// The color the terminal is showing the cursor in, if not its own.
    cursor_color: Option<Rgb>,
    title: String,
    /// The progress the terminal is showing.
    progress: Option<Progress>,
    /// The pointer shape the terminal is showing, if not the one it started with.
    pointer_shape: Option<&'static str>,
    /// Mouse reports were asked to come in pixels.
    pixel_mouse: bool,
    images: Images,
    pub status_entries: Vec<StatusEntry>,
}

impl Renderer {
    /// Forgets what the terminal shows, so the next frame draws everything.
    pub fn invalidate(&mut self) {
        self.reset = true;
        self.cursor = None;
        self.title.clear();
        // The client undid these on leaving.
        self.cursor_color = None;
        self.progress = None;
        self.pointer_shape = None;
        self.pixel_mouse = false;
        self.images = Images::default();
    }

    fn resize(&mut self, cols: u16, rows: u16) -> bool {
        if self.next.cols == cols && self.next.rows == rows {
            return false;
        }
        self.next = Grid::new(cols, rows);
        self.shown = Grid::new(cols, rows);
        self.reset = true;
        true
    }

    /// Produces the bytes that bring the terminal up to date.
    pub fn frame(&mut self, session: &mut Session, ui: &Ui) -> Vec<u8> {
        let (cols, rows) = (session.size.cols, session.size.rows);
        if self.resize(cols, rows) || self.reset {
            session.repaint = true;
        }
        self.compose(session);
        if ui.pane_numbers.is_some() {
            self.draw_pane_numbers(session);
        }
        if let Some(list) = &ui.list {
            self.draw_list(list, session.area());
        }
        self.draw_status(session, ui);

        let mut out = Vec::new();
        if self.reset {
            push_link(&mut out, None);
            // Not `2J`, which would take all images along with it.
            out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[J");
            self.shown = Grid::new(cols, rows);
            self.reset = false;
            self.images.forget_placements(&mut out);
        }
        self.emit_changes(|link| session.link_uri(link), &mut out);
        let mut transfers = Vec::new();
        let covered = ui.list.is_some() || ui.pane_numbers.is_some();
        self.images
            .update(session, covered, &mut transfers, &mut out);
        self.emit_cursor(session, ui, &mut out);
        self.emit_title(session, &mut out);
        self.emit_progress(session, &mut out);
        self.emit_cursor_color(session, &mut out);
        self.emit_pointer_shape(session, &mut out);
        self.emit_mouse_format(session, &mut out);
        let mut frame = transfers;
        if !out.is_empty() {
            frame.extend_from_slice(b"\x1b[?2026h");
            frame.append(&mut out);
            frame.extend_from_slice(b"\x1b[?2026l");
        }
        frame
    }

    fn compose(&mut self, session: &mut Session) {
        let repaint = std::mem::take(&mut session.repaint);
        if repaint {
            self.next.clear();
        }
        let visible = session.visible_panes();
        for (id, rect) in &visible {
            if let Some(pane) = session.panes.get_mut(id) {
                let all = repaint || std::mem::take(&mut pane.redraw);
                pane.terminal.refresh();
                let next = &mut self.next;
                pane.terminal
                    .draw_rows(all, |y, cells| draw_row(next, *rect, y, cells));
                if let Some(mode) = pane.copy_mode {
                    pane.redraw = true;
                    let position = pane.terminal.scrollbar();
                    let matches = pane.terminal.visible_matches();
                    let top = position.map_or(0, |bar| bar.offset);
                    let top = u32::try_from(top).unwrap_or(u32::MAX);
                    draw_matches(next, *rect, top, (mode.x, mode.y), &matches);
                    draw_position(next, *rect, position, pane.terminal.match_count());
                }
            }
        }
        if let Some(window) = session.window()
            && !window.zoomed
        {
            let active = visible.iter().find(|(id, _)| *id == window.active);
            let active = active.map(|(_, rect)| *rect).unwrap_or_default();
            let marked = visible
                .iter()
                .find(|(id, _)| session.marked_pane() == Some(*id))
                .map(|(_, rect)| *rect);
            self.draw_borders(&window.root.dividers(session.area()), active, marked);
        }
    }

    /// tmux's `display-panes`. The rows come back by repainting when it ends.
    fn draw_pane_numbers(&mut self, session: &Session) {
        let Some(window) = session.window() else {
            return;
        };
        let all = window.root.panes(session.area());
        for (id, rect) in session.visible_panes() {
            if let Some(number) = all.iter().position(|(pane, _)| *pane == id) {
                let color = if id == window.active { RED } else { BLUE };
                draw_pane_number(&mut self.next, rect, number, color);
            }
        }
    }

    /// Covers the panes, whose rows come back by repainting when the list goes.
    fn draw_list(&mut self, list: &List, area: Rect) {
        let height = usize::from(area.height);
        let top = list.top(height);
        for (y, index) in (area.y..area.y.saturating_add(area.height)).zip(top..) {
            let selected = index == list.selected;
            let style = if selected {
                mode_style()
            } else {
                Style::default()
            };
            self.next.fill_row(y, style);
            if let Some(row) = list.rows.get(index) {
                let line = match (&row.command, index) {
                    (Some(_), 0..=9) => format!("({index}) {}", row.text),
                    (Some(_), _) => format!("    {}", row.text),
                    (None, _) => row.text.clone(),
                };
                self.next.put_str(area.x, y, &line, style, area.width);
            }
        }
        if list.rows.len() > height {
            let text = format!("[{}/{}]", list.selected + 1, list.rows.len());
            let x = area.width.saturating_sub(text_width(&text));
            self.next.split_wide(x, area.y);
            self.next
                .put_str(x, area.y, &text, mode_style(), area.width);
        }
    }

    fn draw_borders(&mut self, dividers: &[Divider], active: Rect, marked: Option<Rect>) {
        let on_line = |x: Option<u16>, y: Option<u16>| match (x, y) {
            (Some(x), Some(y)) => dividers.iter().any(|d| d.line.contains(x, y)),
            _ => false,
        };
        for divider in dividers {
            let line = divider.line;
            for y in line.y..line.y.saturating_add(line.height) {
                for x in line.x..line.x.saturating_add(line.width) {
                    let c = border_char([
                        on_line(Some(x), y.checked_sub(1)),
                        on_line(Some(x), y.checked_add(1)),
                        on_line(x.checked_sub(1), Some(y)),
                        on_line(x.checked_add(1), Some(y)),
                    ]);
                    let mut style = border_style(touches(active, x, y));
                    if marked.is_some_and(|marked| touches(marked, x, y)) {
                        style.flags |= STYLE_INVERSE;
                    }
                    let mut encoded = [0; 4];
                    self.next.set(
                        x,
                        y,
                        Cell {
                            text: Text::new(c.encode_utf8(&mut encoded)),
                            style,
                            ..Cell::default()
                        },
                    );
                }
            }
        }
    }

    fn draw_status(&mut self, session: &Session, ui: &Ui) {
        let y = session.size.rows.saturating_sub(1);
        let cols = session.size.cols;
        self.status_entries.clear();

        if let Some(prompt) = &ui.prompt {
            self.next.fill_row(y, mode_style());
            let line = format!("{}{}", prompt.label, prompt.text());
            self.next.put_str(0, y, &line, mode_style(), cols);
            return;
        }
        if let Some(message) = ui.message() {
            self.next.fill_row(y, mode_style());
            self.next.put_str(0, y, message, mode_style(), cols);
            return;
        }

        let style = status_style();
        self.next.fill_row(y, style);
        let right = status_right(session);
        let right_start = cols.saturating_sub(text_width(&right));
        let mut x = self.next.put_str(0, y, "[kl] ", style, cols);
        for window in &session.windows {
            let label = window_label(session, window);
            let start = x;
            let alert = window.bell || window.has_activity();
            let label_style = if alert { alert_style() } else { style };
            x = self.next.put_str(x, y, &label, label_style, right_start);
            self.status_entries.push(StatusEntry {
                start,
                end: x,
                window: window.index,
            });
            x = self.next.put_str(x, y, " ", style, right_start);
        }
        self.next.put_str(right_start, y, &right, style, cols);
    }

    fn emit_changes<'a>(&mut self, uri: impl Fn(Link) -> Option<&'a [u8]>, out: &mut Vec<u8>) {
        let mut pen = Pen::default();
        let cols = usize::from(self.next.cols).max(1);
        let rows = self
            .next
            .cells
            .chunks(cols)
            .zip(self.shown.cells.chunks(cols));
        for (y, (next, shown)) in (0..).zip(rows) {
            let mut cells = (0..).zip(next.iter().zip(shown));
            while let Some((x, (next, shown))) = cells.next() {
                // The right half of a character is drawn along with its left.
                if next != shown && next.width != Width::Tail {
                    pen.draw(x, y, next, &uri, out);
                    if next.width == Width::Wide {
                        cells.next();
                    }
                }
            }
        }
        if pen.link.is_some() {
            push_link(out, None);
        }
        if pen.drew {
            out.extend_from_slice(b"\x1b[0m");
        }
        self.shown.cells.clone_from(&self.next.cells);
    }

    fn emit_cursor(&mut self, session: &Session, ui: &Ui, out: &mut Vec<u8>) {
        let wanted = cursor_target(session, ui);
        let known = self.cursor.replace(wanted);
        // Drawing moves the cursor, so if anything was drawn it has to go back.
        if let Some((x, y, _)) = wanted
            && (!out.is_empty() || known != Some(wanted))
        {
            push_move(out, x, y);
        }
        let shape = |target: CursorTarget| target.map(|(_, _, shape)| shape);
        if known.map(shape) == Some(shape(wanted)) {
            return;
        }
        match shape(wanted) {
            Some(shape) => out.extend_from_slice(format!("\x1b[{shape} q\x1b[?25h").as_bytes()),
            None => out.extend_from_slice(b"\x1b[?25l"),
        }
    }

    /// The outer terminal draws the cursor, so a color of the active pane's
    /// program is the one thing about colors that has to go there.
    fn emit_cursor_color(&mut self, session: &Session, out: &mut Vec<u8>) {
        let wanted = session
            .active_pane()
            .and_then(|pane| pane.terminal.cursor_color());
        if wanted == self.cursor_color {
            return;
        }
        self.cursor_color = wanted;
        match wanted {
            Some(Rgb { r, g, b }) => {
                out.extend_from_slice(
                    format!("\x1b]12;rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\").as_bytes(),
                );
            }
            None => out.extend_from_slice(b"\x1b]112\x1b\\"),
        }
    }

    fn emit_title(&mut self, session: &Session, out: &mut Vec<u8>) {
        let title = session
            .active_pane()
            .map(|pane| pane.terminal.title())
            .filter(|title| !title.is_empty())
            .or_else(|| session.window().map(|window| window.name.clone()))
            .unwrap_or_default();
        if title != self.title {
            out.extend_from_slice(b"\x1b]2;");
            out.extend(osc_text(&title));
            out.extend_from_slice(b"\x1b\\");
            self.title = title;
        }
    }

    fn emit_pointer_shape(&mut self, session: &Session, out: &mut Vec<u8>) {
        let wanted = session.pointer_shape();
        if wanted != self.pointer_shape.unwrap_or(DEFAULT_POINTER_SHAPE) {
            out.extend_from_slice(format!("\x1b]22;{wanted}\x1b\\").as_bytes());
            self.pointer_shape = Some(wanted);
        }
    }

    /// Asks for the mouse in pixels only while a program can use them, so that
    /// everything else does not depend on them being what they seem to be. The
    /// reply to asking about the mode marks where in the input the format changes,
    /// and tells whether it did. Leaving the mode leaves no format at all.
    fn emit_mouse_format(&mut self, session: &Session, out: &mut Vec<u8>) {
        let wanted = session.wants_pixel_mouse();
        if wanted != self.pixel_mouse {
            self.pixel_mouse = wanted;
            out.extend_from_slice(if wanted {
                b"\x1b[?1016h\x1b[?1016$p"
            } else {
                b"\x1b[?1016l\x1b[?1006h\x1b[?1016$p"
            });
        }
    }

    fn emit_progress(&mut self, session: &mut Session, out: &mut Vec<u8>) {
        let reported = std::mem::take(&mut session.progress_reported);
        let wanted = session.progress();
        if wanted == self.progress && !(reported && wanted.is_some()) {
            return;
        }
        self.progress = wanted;
        let sequence = match wanted {
            None => "\x1b]9;4;0\x1b\\".to_owned(),
            Some(Progress {
                state,
                percent: None,
            }) => format!("\x1b]9;4;{state}\x1b\\"),
            Some(Progress {
                state,
                percent: Some(percent),
            }) => format!("\x1b]9;4;{state};{percent}\x1b\\"),
        };
        out.extend_from_slice(sequence.as_bytes());
    }
}

/// Text that came from a program, safe to put inside an OSC.
pub fn osc_text(text: &str) -> impl Iterator<Item = u8> {
    text.bytes().filter(|byte| !byte.is_ascii_control())
}

/// Whether a cell of a border runs along this pane.
const fn touches(rect: Rect, x: u16, y: u16) -> bool {
    let left = rect.x.saturating_sub(1);
    let top = rect.y.saturating_sub(1);
    x >= left
        && y >= top
        && x <= rect.x.saturating_add(rect.width)
        && y <= rect.y.saturating_add(rect.height)
}

/// Picks a line drawing character from which of the neighbors above, below, to
/// the left and to the right are also lines.
const fn border_char(neighbors: [bool; 4]) -> char {
    match neighbors {
        [true, true, true, true] => '┼',
        [true, true, false, true] => '├',
        [true, true, true, false] => '┤',
        [false, true, true, true] => '┬',
        [true, false, true, true] => '┴',
        [true, false, false, true] => '└',
        [true, false, true, false] => '┘',
        [false, true, false, true] => '┌',
        [false, true, true, false] => '┐',
        [_, _, false, false] => '│',
        _ => '─',
    }
}

fn draw_row(grid: &mut Grid, rect: Rect, y: u16, cells: &[Cell]) {
    if y >= rect.height {
        return;
    }
    for (x, cell) in (0..rect.width).zip(cells) {
        let mut cell = cell.clone();
        if cell.selected {
            cell.style = mode_style();
        }
        grid.set(rect.x + x, rect.y + y, cell);
    }
}

/// A pane's number in its middle, in big digits if they fit, and its size.
fn draw_pane_number(grid: &mut Grid, rect: Rect, number: usize, color: u8) {
    let text = number.to_string();
    let len = text_width(&text);
    let (middle_x, middle_y) = (rect.x + rect.width / 2, rect.y + rect.height / 2);
    let foreground = Style {
        fg: StyleColor::palette(color),
        ..Style::default()
    };
    if rect.width < len * 6 || rect.height < 5 {
        let x = middle_x.saturating_sub(len / 2);
        grid.split_wide(x, middle_y);
        grid.put_str(x, middle_y, &text, foreground, rect.x + rect.width);
        return;
    }
    let block = Cell {
        style: Style {
            bg: StyleColor::palette(color),
            ..Style::default()
        },
        ..Cell::default()
    };
    let digits = text.chars().filter_map(|c| c.to_digit(10));
    let (left, top) = (middle_x - len * 3, middle_y - 2);
    for (digit, digit_x) in digits.zip((left..).step_by(6)) {
        let rows = usize::try_from(digit).ok().and_then(|d| BIG_DIGITS.get(d));
        for (bits, y) in rows.into_iter().flatten().zip(top..) {
            for column in 0..5 {
                if bits & (0b10000 >> column) != 0 {
                    grid.split_wide(digit_x + column, y);
                    grid.set(digit_x + column, y, block.clone());
                }
            }
        }
    }
    let size = format!("{}x{}", rect.width, rect.height);
    let width = text_width(&size);
    if rect.height > 6 && rect.width >= width {
        let x = rect.x + rect.width - width;
        grid.split_wide(x, rect.y);
        grid.put_str(x, rect.y, &size, foreground, rect.x + rect.width);
    }
}

/// Shows what a search found. `top` is the row of the screen at the top of the
/// pane, and `cursor` is relative to the pane.
fn draw_matches(grid: &mut Grid, rect: Rect, top: u32, cursor: (u16, u16), matches: &[Match]) {
    let cursor_row = top.saturating_add(u32::from(cursor.1));
    let last_column = rect.width.saturating_sub(1);
    for found in matches {
        let style = match_style(found.contains(cursor.0, cursor_row));
        for row in found.start.1..=found.end.1 {
            let y = row.checked_sub(top).and_then(|y| u16::try_from(y).ok());
            let Some(y) = y.filter(|y| *y < rect.height) else {
                continue;
            };
            let first = if row == found.start.1 {
                found.start.0
            } else {
                0
            };
            let last = if row == found.end.1 {
                found.end.0.min(last_column)
            } else {
                last_column
            };
            for x in first..=last {
                grid.highlight(rect.x + x, rect.y + y, style);
            }
        }
    }
}

/// tmux's `[offset/history]` in the corner of a pane in copy mode, with how much
/// a search found.
fn draw_position(
    grid: &mut Grid,
    rect: Rect,
    scrollbar: Option<ghostty::Scrollbar>,
    results: usize,
) {
    let Some(scrollbar) = scrollbar else {
        return;
    };
    let results = match results {
        0 => String::new(),
        results => format!(" ({results} results)"),
    };
    let (below, history) = (scrollbar.lines_below(), scrollbar.history());
    let text = format!("[{below}/{history}]{results}");
    let width = text_width(&text);
    let x = rect.x + rect.width.saturating_sub(width);
    grid.split_wide(x, rect.y);
    grid.put_str(x, rect.y, &text, mode_style(), rect.x + rect.width);
}

fn window_label(session: &Session, window: &Window) -> String {
    let flags = session.window_flags(window);
    format!("{}:{}{flags}", window.index, window.name)
}

fn status_right(session: &Session) -> String {
    let title: String = session
        .active_pane()
        .map(|pane| pane.terminal.title())
        .filter(|title| !title.is_empty())
        // A pane is named after the machine until its program says otherwise,
        // which is where tmux has it too.
        .unwrap_or_else(sys::hostname)
        .chars()
        .take(21)
        .collect();
    let time = sys::local_time(c"%H:%M %d-%b-%y");
    format!("\"{title}\" {time}")
}

fn cursor_target(session: &Session, ui: &Ui) -> CursorTarget {
    if let Some(prompt) = &ui.prompt {
        let x = text_width(&prompt.label) + text_width(&prompt.text_before_cursor());
        return Some((x, session.size.rows.saturating_sub(1), 2));
    }
    if ui.list.is_some() {
        return None;
    }
    let pane = session.active_pane()?;
    let rect = pane.rect;
    if let Some(mode) = pane.copy_mode {
        return Some((rect.x + mode.x, rect.y + mode.y, 2));
    }
    let cursor = pane.terminal.cursor()?;
    let (x, y) = cursor.position.filter(|_| cursor.visible)?;
    let steady = u8::from(!cursor.blinking);
    let shape = match cursor.shape {
        CursorShape::Block => 1,
        CursorShape::Underline => 3,
        CursorShape::Bar => 5,
    };
    Some((rect.x + x, rect.y + y, shape + steady))
}

pub fn push_move(out: &mut Vec<u8>, x: u16, y: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
}

/// Tracks where the terminal's cursor is and what style it would draw with, to
/// leave out sequences that change nothing.
#[derive(Default)]
struct Pen {
    position: Option<(u16, u16)>,
    style: Option<Style>,
    link: Option<Link>,
    drew: bool,
}

impl Pen {
    fn draw<'a>(
        &mut self,
        x: u16,
        y: u16,
        cell: &Cell,
        uri: impl Fn(Link) -> Option<&'a [u8]>,
        out: &mut Vec<u8>,
    ) {
        if self.position != Some((x, y)) {
            push_move(out, x, y);
        }
        if self.style != Some(cell.style) {
            push_style(out, &cell.style);
            self.style = Some(cell.style);
        }
        if self.link != cell.link {
            // Terminals refuse a URI that is too long, and it may have got too
            // long here. Text must then end up without a link, not with the last.
            if self.link.is_some() {
                push_link(out, None);
            }
            self.link = cell.link;
            if let Some(open) = cell.link.and_then(|link| Some((link, uri(link)?))) {
                push_link(out, Some(open));
            }
        }
        out.extend_from_slice(cell.text.as_str().as_bytes());
        let width = if cell.width == Width::Wide { 2 } else { 1 };
        self.position = Some((x + width, y));
        self.drew = true;
    }
}

/// Opens a link, or closes the open one. The `id` makes the outer terminal see
/// one link however many pieces it is drawn in, and the number behind it differs
/// between panes, as the same URI in two panes is two links.
fn push_link(out: &mut Vec<u8>, link: Option<(Link, &[u8])>) {
    out.extend_from_slice(b"\x1b]8;");
    if let Some((link, uri)) = link {
        out.extend_from_slice(format!("id=kl{:X};", link.number()).as_bytes());
        for byte in uri {
            // What programs get into a URI must not get out of the sequence.
            if (0x20..0x7f).contains(byte) {
                out.push(*byte);
            } else {
                out.extend_from_slice(format!("%{byte:02X}").as_bytes());
            }
        }
    } else {
        out.push(b';');
    }
    out.extend_from_slice(b"\x1b\\");
}

/// The SGR parameter for a foreground or background color, where `base` is 30
/// or 40. The basic sixteen stay symbolic, so that they follow the outer theme.
fn color_param(color: StyleColor, base: u8) -> Option<String> {
    if let Some([r, g, b]) = color.as_rgb() {
        return Some(format!("{};2;{r};{g};{b}", base + 8));
    }
    color.as_palette().map(|index| match index {
        0..=7 => (base + index).to_string(),
        8..=15 => (base + 60 + index - 8).to_string(),
        _ => format!("{};5;{index}", base + 8),
    })
}

fn underline_color_param(color: StyleColor) -> Option<String> {
    if let Some([r, g, b]) = color.as_rgb() {
        return Some(format!("58:2::{r}:{g}:{b}"));
    }
    color.as_palette().map(|index| format!("58:5:{index}"))
}

/// Writes a full SGR sequence for a style, starting from a reset.
fn push_style(out: &mut Vec<u8>, style: &Style) {
    let mut params = vec!["0".to_owned()];
    for (flag, code) in [
        (STYLE_BOLD, "1"),
        (STYLE_FAINT, "2"),
        (STYLE_ITALIC, "3"),
        (STYLE_BLINK, "5"),
        (STYLE_INVERSE, "7"),
        (STYLE_INVISIBLE, "8"),
        (STYLE_STRIKETHROUGH, "9"),
        (STYLE_OVERLINE, "53"),
    ] {
        if style.flags & flag != 0 {
            params.push(code.to_owned());
        }
    }
    let underline = (style.flags & STYLE_UNDERLINE_MASK) >> STYLE_UNDERLINE_SHIFT;
    if underline != 0 {
        params.push(format!("4:{underline}"));
    }
    params.extend(color_param(style.fg, 30));
    params.extend(color_param(style.bg, 40));
    params.extend(underline_color_param(style.underline_color));
    out.extend_from_slice(format!("\x1b[{}m", params.join(";")).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::super::ui::ListRow;
    use super::*;
    use crate::command::Command;
    use crate::ghostty::{Rgb, Terminal};

    fn sgr(style: &Style) -> String {
        let mut out = Vec::new();
        push_style(&mut out, style);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn writes_styles_as_sgr() {
        assert_eq!(sgr(&Style::default()), "\x1b[0m");
        assert_eq!(
            sgr(&Style {
                flags: STYLE_BOLD | STYLE_INVERSE,
                fg: StyleColor::palette(1),
                bg: StyleColor::palette(12),
                ..Style::default()
            }),
            "\x1b[0;1;7;31;104m"
        );
        assert_eq!(
            sgr(&Style {
                fg: StyleColor::palette(200),
                bg: StyleColor::rgb(Rgb { r: 1, g: 2, b: 3 }),
                ..Style::default()
            }),
            "\x1b[0;38;5;200;48;2;1;2;3m"
        );
    }

    /// Whatever a program can set must survive being written back out, or
    /// panes would look different from running without kl.
    #[test]
    fn styles_survive_a_round_trip_through_a_terminal() {
        for input in [
            "\x1b[1;3;4;9m",
            "\x1b[2;5;7;8;53m",
            "\x1b[4:3;58:2::10:20:30m",
            "\x1b[4:5;58:5:99m",
            "\x1b[38;2;1;2;3;48;5;200m",
            "\x1b[91;42m",
        ] {
            let mut original = Terminal::new(4, 1).unwrap();
            original.write(input.as_bytes());
            original.write(b"x");
            original.refresh();
            let mut style = Style::default();
            original.draw_rows(true, |_, cells| style = cells[0].style);
            assert_ne!(style, Style::default(), "{input:?} set no style");

            let mut copy = Terminal::new(4, 1).unwrap();
            copy.write(sgr(&style).as_bytes());
            copy.write(b"x");
            copy.refresh();
            let mut copied = Style::default();
            copy.draw_rows(true, |_, cells| copied = cells[0].style);
            assert_eq!(copied, style, "for {input:?}");
        }
    }

    #[test]
    fn picks_border_characters() {
        assert_eq!(border_char([false, true, false, false]), '│');
        assert_eq!(border_char([true, true, false, false]), '│');
        assert_eq!(border_char([false, false, true, true]), '─');
        assert_eq!(border_char([false, false, false, true]), '─');
        assert_eq!(border_char([true, true, true, false]), '┤');
        assert_eq!(border_char([true, true, true, true]), '┼');
    }

    /// Where a grid has a background color, as text.
    fn blocks(grid: &Grid) -> Vec<String> {
        let row = |cells: &[Cell]| {
            let is_block = |cell: &Cell| cell.style.bg != StyleColor::default();
            cells
                .iter()
                .map(|cell| if is_block(cell) { '#' } else { '.' })
                .collect()
        };
        grid.cells.chunks(usize::from(grid.cols)).map(row).collect()
    }

    #[test]
    fn draws_big_pane_numbers_in_the_middle() {
        let mut grid = Grid::new(11, 5);
        let rect = Rect {
            x: 0,
            y: 0,
            width: 11,
            height: 5,
        };
        draw_pane_number(&mut grid, rect, 2, BLUE);
        assert_eq!(
            blocks(&grid),
            [
                "..#####....",
                "......#....",
                "..#####....",
                "..#........",
                "..#####....",
            ]
        );
    }

    #[test]
    fn small_panes_get_their_number_as_text() {
        let mut grid = Grid::new(4, 3);
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 3,
        };
        draw_pane_number(&mut grid, rect, 12, RED);
        assert_eq!(grid.cells[5].text.as_str(), "1");
        assert_eq!(grid.cells[6].text.as_str(), "2");
    }

    #[test]
    fn tall_enough_panes_show_their_size() {
        let mut grid = Grid::new(10, 7);
        let rect = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 7,
        };
        draw_pane_number(&mut grid, rect, 1, RED);
        assert_eq!(grid.cells[6].text.as_str(), "1");
        assert_eq!(grid.cells[7].text.as_str(), "0");
        assert_eq!(grid.cells[8].text.as_str(), "x");
    }

    fn shown_rows(renderer: &Renderer, rows: u16) -> Vec<String> {
        (0..rows)
            .map(|y| {
                let cells = (0..renderer.next.cols).filter_map(|x| {
                    let index = renderer.next.index(x, y)?;
                    renderer.next.cells.get(index)
                });
                let line: String = cells.map(|cell| cell.text.as_str()).collect();
                line.trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn draws_a_list_over_the_panes() {
        let mut renderer = Renderer::default();
        renderer.resize(12, 3);
        renderer
            .next
            .put_str(0, 1, "underneath", Style::default(), 12);
        let rows = vec![
            ListRow::new("one".to_owned(), Command::LastWindow),
            ListRow::text("two".to_owned()),
        ];
        let area = Rect {
            x: 0,
            y: 0,
            width: 12,
            height: 3,
        };
        renderer.draw_list(&List::new(rows, 1), area);
        assert_eq!(shown_rows(&renderer, 3), ["(0) one", "two", ""]);
        assert_eq!(renderer.next.cells[12].style, mode_style());
        assert_eq!(renderer.next.cells[0].style, Style::default());
    }

    #[test]
    fn a_long_list_says_where_it_is() {
        let mut renderer = Renderer::default();
        renderer.resize(12, 2);
        let row = |index: u32| ListRow::text(index.to_string());
        let area = Rect {
            x: 0,
            y: 0,
            width: 12,
            height: 2,
        };
        renderer.draw_list(&List::new((0..5).map(row).collect(), 4), area);
        assert_eq!(shown_rows(&renderer, 2), ["3      [5/5]", "4"]);
    }

    #[test]
    fn put_str_leaves_out_a_wide_character_that_does_not_fit() {
        let mut grid = Grid::new(3, 1);
        let end = grid.put_str(0, 0, "a\u{4e16}\u{4e16}", Style::default(), 3);
        assert_eq!(end, 3);
        assert_eq!(grid.cells[1].width, Width::Wide);
        assert_eq!(grid.cells[2].width, Width::Tail);
    }

    #[test]
    fn the_position_in_copy_mode_does_not_leave_half_a_wide_character() {
        let mut grid = Grid::new(6, 1);
        grid.put_str(0, 0, "\u{4e16}\u{4e16}\u{4e16}", Style::default(), 6);
        let rect = Rect {
            x: 0,
            y: 0,
            width: 6,
            height: 1,
        };
        let scrollbar = ghostty::Scrollbar {
            total: 1,
            offset: 0,
            len: 1,
        };
        draw_position(&mut grid, rect, Some(scrollbar), 0);
        assert_eq!(grid.cells[0], Cell::default());
        assert_eq!(grid.cells[1].text.as_str(), "[");
        assert_eq!(grid.cells[1].width, Width::Narrow);
    }

    fn emitted(renderer: &mut Renderer) -> String {
        let mut out = Vec::new();
        renderer.emit_changes(|_| None, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn sends_only_what_changed() {
        let mut renderer = Renderer::default();
        renderer.resize(5, 2);
        renderer.next.put_str(0, 0, "ab", Style::default(), 5);
        assert_eq!(emitted(&mut renderer), "\x1b[1;1H\x1b[0mab\x1b[0m");
        assert_eq!(emitted(&mut renderer), "");
        renderer.next.put_str(1, 0, "X", Style::default(), 5);
        assert_eq!(emitted(&mut renderer), "\x1b[1;2H\x1b[0mX\x1b[0m");
    }

    #[test]
    fn a_run_of_changes_needs_one_move() {
        let mut renderer = Renderer::default();
        renderer.resize(5, 2);
        renderer.next.put_str(1, 1, "abc", Style::default(), 5);
        assert_eq!(emitted(&mut renderer), "\x1b[2;2H\x1b[0mabc\x1b[0m");
    }

    fn a_link() -> Link {
        let mut terminal = Terminal::new(1, 1).unwrap();
        terminal.write(b"\x1b]8;;https://a\x1b\\a");
        terminal.refresh();
        let mut link = None;
        terminal.draw_rows(true, |_, cells| link = cells[0].link);
        link.unwrap()
    }

    fn emitted_links(renderer: &mut Renderer) -> String {
        let mut out = Vec::new();
        renderer.emit_changes(|_| Some(b"uri"), &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_link_opens_and_closes_around_its_cells() {
        let link = a_link();
        let mut renderer = Renderer::default();
        renderer.resize(5, 1);
        renderer.next.put_str(0, 0, "abcd", Style::default(), 5);
        renderer.next.cells[1].link = Some(link);
        renderer.next.cells[2].link = Some(link);
        let id = link.number();
        assert_eq!(
            emitted_links(&mut renderer),
            format!("\x1b[1;1H\x1b[0ma\x1b]8;id=kl{id:X};uri\x1b\\bc\x1b]8;;\x1b\\d\x1b[0m"),
        );
    }

    #[test]
    fn a_link_is_closed_before_the_next_one_opens() {
        let (first, second) = (a_link(), a_link());
        let mut renderer = Renderer::default();
        renderer.resize(5, 1);
        renderer.next.put_str(0, 0, "ab", Style::default(), 5);
        renderer.next.cells[0].link = Some(first);
        renderer.next.cells[1].link = Some(second);
        let (first, second) = (first.number(), second.number());
        assert_eq!(
            emitted_links(&mut renderer),
            format!(
                "\x1b[1;1H\x1b[0m\x1b]8;id=kl{first:X};uri\x1b\\a\x1b]8;;\x1b\\\
                 \x1b]8;id=kl{second:X};uri\x1b\\b\x1b]8;;\x1b\\\x1b[0m"
            ),
        );
    }

    #[test]
    fn a_link_does_not_outlast_what_changed() {
        let link = a_link();
        let mut renderer = Renderer::default();
        renderer.resize(5, 2);
        renderer.next.put_str(4, 0, "a", Style::default(), 5);
        renderer.next.cells[4].link = Some(link);
        let id = link.number();
        assert_eq!(
            emitted_links(&mut renderer),
            format!("\x1b[1;5H\x1b[0m\x1b]8;id=kl{id:X};uri\x1b\\a\x1b]8;;\x1b\\\x1b[0m"),
        );
        assert_eq!(emitted_links(&mut renderer), "");
    }

    #[test]
    fn only_what_is_allowed_in_a_uri_gets_through() {
        let mut out = Vec::new();
        push_link(&mut out, Some((a_link(), b"a;b\x1b\\\x07\xc3\xa9\x7f")));
        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with(";a;b%1B\\%07%C3%A9%7F\x1b\\"), "{text:?}");
    }

    #[test]
    fn a_wide_character_moves_the_pen_two_cells() {
        let mut renderer = Renderer::default();
        renderer.resize(5, 1);
        renderer
            .next
            .put_str(0, 0, "\u{4e16}a", Style::default(), 5);
        assert_eq!(emitted(&mut renderer), "\x1b[1;1H\x1b[0m\u{4e16}a\x1b[0m");
    }
}
