//! Composes the panes, borders and status line into one grid, then sends the
//! outer terminal only what changed since the last frame.

use super::layout::{Divider, Rect};
use super::session::{Session, Window};
use super::ui::Ui;
use crate::ghostty::{
    self, Cell, CursorShape, STYLE_BLINK, STYLE_BOLD, STYLE_FAINT, STYLE_INVERSE, STYLE_INVISIBLE,
    STYLE_ITALIC, STYLE_OVERLINE, STYLE_STRIKETHROUGH, STYLE_UNDERLINE_MASK, STYLE_UNDERLINE_SHIFT,
    Style, StyleColor, Text, Width,
};
use crate::sys;

const BLACK: u8 = 0;
const GREEN: u8 = 2;
const YELLOW: u8 = 3;

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

fn mode_style() -> Style {
    colored(BLACK, YELLOW)
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
                    selected: false,
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

/// What the terminal's cursor should look like, as last sent.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CursorState {
    visible: bool,
    shape: u8,
}

#[derive(Default)]
pub struct Renderer {
    shown: Grid,
    next: Grid,
    /// Nothing on the terminal can be trusted, as after attaching.
    reset: bool,
    cursor: Option<CursorState>,
    title: String,
    pub status_entries: Vec<StatusEntry>,
}

impl Renderer {
    /// Forgets what the terminal shows, so the next frame draws everything.
    pub fn invalidate(&mut self) {
        self.reset = true;
        self.cursor = None;
        self.title.clear();
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
        self.draw_status(session, ui);

        let mut out = Vec::new();
        if self.reset {
            out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
            self.shown = Grid::new(cols, rows);
            self.reset = false;
        }
        self.emit_changes(&mut out);
        self.emit_cursor(session, ui, &mut out);
        self.emit_title(session, &mut out);
        if out.is_empty() {
            return out;
        }
        let mut frame = b"\x1b[?2026h".to_vec();
        frame.append(&mut out);
        frame.extend_from_slice(b"\x1b[?2026l");
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
                if pane.copy_mode.is_some() {
                    pane.redraw = true;
                    let position = pane.terminal.scrollbar();
                    draw_position(next, *rect, position);
                }
            }
        }
        if let Some(window) = session.window()
            && !window.zoomed
        {
            let active = visible.iter().find(|(id, _)| *id == window.active);
            let active = active.map(|(_, rect)| *rect).unwrap_or_default();
            self.draw_borders(&window.root.dividers(session.area()), active);
        }
    }

    fn draw_borders(&mut self, dividers: &[Divider], active: Rect) {
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
                    let mut encoded = [0; 4];
                    self.next.set(
                        x,
                        y,
                        Cell {
                            text: Text::new(c.encode_utf8(&mut encoded)),
                            style: border_style(touches(active, x, y)),
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
            x = self.next.put_str(x, y, &label, style, right_start);
            self.status_entries.push(StatusEntry {
                start,
                end: x,
                window: window.index,
            });
            x = self.next.put_str(x, y, " ", style, right_start);
        }
        self.next.put_str(right_start, y, &right, style, cols);
    }

    fn emit_changes(&mut self, out: &mut Vec<u8>) {
        let mut pen = Pen::default();
        for y in 0..self.next.rows {
            let mut x = 0;
            while x < self.next.cols {
                let index = usize::from(y) * usize::from(self.next.cols) + usize::from(x);
                let (Some(next), Some(shown)) =
                    (self.next.cells.get(index), self.shown.cells.get(index))
                else {
                    break;
                };
                // The right half of a character is drawn along with its left.
                if next == shown || next.width == Width::Tail {
                    x += 1;
                    continue;
                }
                pen.draw(x, y, next, out);
                x += if next.width == Width::Wide { 2 } else { 1 };
            }
        }
        if pen.drew {
            out.extend_from_slice(b"\x1b[0m");
        }
        self.shown.cells.clone_from(&self.next.cells);
    }

    fn emit_cursor(&mut self, session: &Session, ui: &Ui, out: &mut Vec<u8>) {
        let wanted = cursor_target(session, ui);
        let state = CursorState {
            visible: wanted.is_some(),
            shape: wanted.map_or(0, |(_, _, shape)| shape),
        };
        if let Some((x, y, _)) = wanted {
            // Always positioned, since drawing the frame moved the cursor.
            push_move(out, x, y);
        }
        if self.cursor == Some(state) {
            return;
        }
        if state.visible {
            out.extend_from_slice(format!("\x1b[{} q\x1b[?25h", state.shape).as_bytes());
        } else {
            out.extend_from_slice(b"\x1b[?25l");
        }
        self.cursor = Some(state);
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
            out.extend(title.bytes().filter(|byte| !byte.is_ascii_control()));
            out.extend_from_slice(b"\x1b\\");
            self.title = title;
        }
    }
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

/// tmux's `[offset/history]` in the corner of a pane in copy mode.
fn draw_position(grid: &mut Grid, rect: Rect, scrollbar: Option<ghostty::Scrollbar>) {
    let Some(scrollbar) = scrollbar else {
        return;
    };
    let text = format!("[{}/{}]", scrollbar.lines_below(), scrollbar.history());
    let width = text_width(&text);
    let x = rect.x + rect.width.saturating_sub(width);
    grid.put_str(x, rect.y, &text, mode_style(), rect.x + rect.width);
}

fn window_label(session: &Session, window: &Window) -> String {
    let mut flags = String::new();
    if window.index == session.current {
        flags.push('*');
    } else if session.last == Some(window.index) {
        flags.push('-');
    }
    if window.bell {
        flags.push('!');
    }
    if window.zoomed {
        flags.push('Z');
    }
    format!("{}:{}{flags}", window.index, window.name)
}

fn status_right(session: &Session) -> String {
    let title: String = session
        .active_pane()
        .map(|pane| pane.terminal.title())
        .unwrap_or_default()
        .chars()
        .take(21)
        .collect();
    let time = sys::local_time(c"%H:%M %d-%b-%y");
    format!("\"{title}\" {time}")
}

/// Where the cursor belongs, and the DECSCUSR style to give it.
fn cursor_target(session: &Session, ui: &Ui) -> Option<(u16, u16, u8)> {
    if let Some(prompt) = &ui.prompt {
        let x = text_width(&prompt.label) + text_width(&prompt.text_before_cursor());
        return Some((x, session.size.rows.saturating_sub(1), 2));
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

fn push_move(out: &mut Vec<u8>, x: u16, y: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
}

/// Tracks where the terminal's cursor is and what style it would draw with, to
/// leave out sequences that change nothing.
#[derive(Default)]
struct Pen {
    position: Option<(u16, u16)>,
    style: Option<Style>,
    drew: bool,
}

impl Pen {
    fn draw(&mut self, x: u16, y: u16, cell: &Cell, out: &mut Vec<u8>) {
        if self.position != Some((x, y)) {
            push_move(out, x, y);
        }
        if self.style != Some(cell.style) {
            push_style(out, &cell.style);
            self.style = Some(cell.style);
        }
        out.extend_from_slice(cell.text.as_str().as_bytes());
        let width = if cell.width == Width::Wide { 2 } else { 1 };
        self.position = Some((x + width, y));
        self.drew = true;
    }
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
    use super::*;
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

    #[test]
    fn put_str_leaves_out_a_wide_character_that_does_not_fit() {
        let mut grid = Grid::new(3, 1);
        let end = grid.put_str(0, 0, "a\u{4e16}\u{4e16}", Style::default(), 3);
        assert_eq!(end, 3);
        assert_eq!(grid.cells[1].width, Width::Wide);
        assert_eq!(grid.cells[2].width, Width::Tail);
    }

    fn emitted(renderer: &mut Renderer) -> String {
        let mut out = Vec::new();
        renderer.emit_changes(&mut out);
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
