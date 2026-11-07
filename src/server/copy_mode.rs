//! Scrolling through a pane's history and selecting text in it with the
//! keyboard, with the keys of tmux's `copy-mode-vi` table and the meaning of its
//! `send-keys -X` commands. The viewport and selection live in the terminal.

use super::motion::{self, Jump, Lines, Motion, NO_SEPARATORS, Position, SEPARATORS};
use super::pane::Pane;
use crate::ghostty::key::{self, MOD_CTRL, MOD_SHIFT};
use crate::ghostty::{Line, Scrollbar, Terminal};
use crate::input::KeyEvent;

impl Lines for Terminal {
    fn rows(&self) -> u32 {
        let total = self.scrollbar().map_or(0, |scrollbar| scrollbar.total);
        u32::try_from(total).unwrap_or(u32::MAX)
    }

    fn columns(&self) -> u16 {
        self.width()
    }

    fn line(&self, y: u32) -> Line {
        self.screen_line(y)
    }
}

/// How the selection follows the cursor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Selecting {
    #[default]
    No,
    Cells,
    Lines(LineSelection),
}

/// A selection of whole lines, which stay selected whichever way it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineSelection {
    /// Where the line it started on begins, and where it ends.
    start: Position,
    end: Position,
    /// The row it started on, which can be one of several the line wraps over.
    row: u32,
    /// The cursor is at the end of the selection, rather than at its start.
    forward: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct CopyMode {
    /// The cursor, relative to the viewport.
    pub x: u16,
    pub y: u16,
    /// Entered by scrolling the wheel, so scrolling back down ends it.
    pub exit_at_bottom: bool,
    /// The column to go back to when moving on from a shorter line.
    column: u16,
    /// How long the line that column was on is.
    column_length: u16,
    selecting: Selecting,
    /// Selections are rectangles, which the cursor can draw past the text.
    rectangle: bool,
    /// What `;` and `,` repeat.
    last_jump: Option<(Jump, char)>,
    /// How many times to run the next command.
    repeat: u32,
}

/// What should happen after a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Cancel,
    /// Copy the selection, then leave.
    Copy,
    /// Add the selection to the latest buffer, then leave.
    Append,
}

/// The commands of copy mode, with the names of tmux's less their dashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Move(Motion),
    CursorUp,
    CursorDown,
    TopLine,
    MiddleLine,
    BottomLine,
    ScrollUp,
    ScrollDown,
    ScrollMiddle,
    PageUp,
    PageDown,
    HalfpageUp,
    HalfpageDown,
    HistoryTop,
    HistoryBottom,
    PreviousParagraph,
    NextParagraph,
    NextMatchingBracket,
    Jump(Jump, char),
    JumpAgain,
    JumpReverse,
    GotoLine(u32),
    BeginSelection,
    ClearSelection,
    RectangleToggle,
    SelectLine,
    OtherEnd,
    Cancel,
    CopySelection,
    AppendSelection,
    CopyEndOfLine,
}

impl Action {
    /// Whether a count before the key means doing it that many times.
    const fn repeats(self) -> bool {
        !matches!(
            self,
            Self::BeginSelection
                | Self::ClearSelection
                | Self::RectangleToggle
                | Self::SelectLine
                | Self::GotoLine(_)
                | Self::Cancel
                | Self::CopySelection
                | Self::AppendSelection
                | Self::CopyEndOfLine
        )
    }
}

/// What copy mode asks of the status line, as `command-prompt` does in tmux.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyPrompt {
    GotoLine,
    /// Takes one key, the character to jump to.
    Jump(Jump),
    /// Takes digits, for how often to run the next command.
    Repeat,
}

impl CopyPrompt {
    pub const fn label(self) -> &'static str {
        match self {
            Self::GotoLine => "(goto line) ",
            Self::Jump(Jump::Forward) => "(jump forward) ",
            Self::Jump(Jump::Backward) => "(jump backward) ",
            Self::Jump(Jump::ToForward) => "(jump to forward) ",
            Self::Jump(Jump::ToBackward) => "(jump to backward) ",
            Self::Repeat => "(repeat) ",
        }
    }
}

/// What a key does in copy mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    Run(Action),
    Prompt(CopyPrompt, Option<char>),
}

fn control_binding(event: &KeyEvent) -> Option<Action> {
    match event.char()? {
        'c' => Some(Action::Cancel),
        'd' => Some(Action::HalfpageDown),
        'e' => Some(Action::ScrollDown),
        'f' => Some(Action::PageDown),
        'h' => Some(Action::Move(Motion::Left)),
        'j' => Some(Action::CopySelection),
        'u' => Some(Action::HalfpageUp),
        'v' => Some(Action::RectangleToggle),
        'y' => Some(Action::ScrollUp),
        _ => None,
    }
}

const fn named_binding(event: &KeyEvent) -> Option<Action> {
    let mods = event.binding_mods();
    let action = match (event.key, mods) {
        (key::ESCAPE, 0) => Action::ClearSelection,
        (key::ENTER, 0) => Action::CopySelection,
        (key::SPACE, 0) => Action::BeginSelection,
        (key::BACKSPACE | key::ARROW_LEFT, 0) => Action::Move(Motion::Left),
        (key::ARROW_RIGHT, 0) => Action::Move(Motion::Right { all: false }),
        (key::ARROW_UP, 0) => Action::CursorUp,
        (key::ARROW_DOWN, 0) => Action::CursorDown,
        (key::ARROW_UP, MOD_CTRL) => Action::ScrollUp,
        (key::ARROW_DOWN, MOD_CTRL) => Action::ScrollDown,
        (key::PAGE_UP, 0) => Action::PageUp,
        (key::PAGE_DOWN, 0) => Action::PageDown,
        (key::HOME, 0) => Action::Move(Motion::StartOfLine),
        (key::END, 0) => Action::Move(Motion::EndOfLine { all: false }),
        _ => return None,
    };
    Some(action)
}

fn char_binding(c: char) -> Option<Binding> {
    let run = |action| Some(Binding::Run(action));
    let go = |motion| Some(Binding::Run(Action::Move(motion)));
    let jump = |kind| Some(Binding::Prompt(CopyPrompt::Jump(kind), None));
    match c {
        '$' => go(Motion::EndOfLine { all: false }),
        '%' => run(Action::NextMatchingBracket),
        ',' => run(Action::JumpReverse),
        '0' => go(Motion::StartOfLine),
        '1'..='9' => Some(Binding::Prompt(CopyPrompt::Repeat, Some(c))),
        ':' => Some(Binding::Prompt(CopyPrompt::GotoLine, None)),
        ';' => run(Action::JumpAgain),
        'A' => run(Action::AppendSelection),
        'B' => go(Motion::PreviousWord(NO_SEPARATORS)),
        'D' => run(Action::CopyEndOfLine),
        'E' => go(Motion::NextWordEnd(NO_SEPARATORS)),
        'F' => jump(Jump::Backward),
        'G' => run(Action::HistoryBottom),
        'H' => run(Action::TopLine),
        'J' => run(Action::ScrollDown),
        'K' => run(Action::ScrollUp),
        'L' => run(Action::BottomLine),
        'M' => run(Action::MiddleLine),
        'T' => jump(Jump::ToBackward),
        'V' => run(Action::SelectLine),
        'W' => go(Motion::NextWord(NO_SEPARATORS)),
        '^' => go(Motion::BackToIndentation),
        'b' => go(Motion::PreviousWord(SEPARATORS)),
        'e' => go(Motion::NextWordEnd(SEPARATORS)),
        'f' => jump(Jump::Forward),
        'g' => run(Action::HistoryTop),
        'h' => go(Motion::Left),
        'j' => run(Action::CursorDown),
        'k' => run(Action::CursorUp),
        'l' => go(Motion::Right { all: false }),
        'o' => run(Action::OtherEnd),
        'q' => run(Action::Cancel),
        't' => jump(Jump::ToForward),
        'v' => run(Action::RectangleToggle),
        'w' => go(Motion::NextWord(SEPARATORS)),
        // tmux leaves this free, and it is what most people bind it to.
        'y' => run(Action::CopySelection),
        'z' => run(Action::ScrollMiddle),
        '{' => run(Action::PreviousParagraph),
        '}' => run(Action::NextParagraph),
        _ => None,
    }
}

/// tmux's `copy-mode-vi` table, or the part of it that exists here.
pub fn binding(event: &KeyEvent) -> Option<Binding> {
    if let Some(action) = named_binding(event) {
        return Some(Binding::Run(action));
    }
    match event.binding_mods() {
        MOD_CTRL => control_binding(event).map(Binding::Run),
        0 | MOD_SHIFT => char_binding(event.char()?),
        _ => None,
    }
}

impl CopyMode {
    /// Starts with the cursor where the pane has its own.
    pub fn enter(pane: &mut Pane, exit_at_bottom: bool) {
        if pane.copy_mode.is_some() {
            return;
        }
        pane.terminal.refresh();
        let position = pane.terminal.cursor().and_then(|cursor| cursor.position);
        let (x, y) = position.unwrap_or_else(|| (0, pane.rect.height.saturating_sub(1)));
        pane.copy_mode = Some(Self {
            x,
            y,
            exit_at_bottom,
            column: x,
            column_length: 0,
            selecting: Selecting::No,
            rectangle: false,
            last_jump: None,
            repeat: 1,
        });
        pane.redraw = true;
    }

    pub fn exit(pane: &mut Pane) {
        pane.copy_mode = None;
        pane.terminal.clear_selection();
        pane.terminal.scroll_to_bottom();
        pane.redraw = true;
    }

    /// Runs the next command this many times.
    pub const fn set_repeat(&mut self, repeat: u32) {
        self.repeat = repeat;
    }

    const fn rectangle_selection(&self) -> bool {
        self.rectangle && matches!(self.selecting, Selecting::Cells)
    }
}

const fn height(pane: &Pane) -> u16 {
    if pane.rect.height == 0 {
        1
    } else {
        pane.rect.height
    }
}

/// The row of the screen at the top of the viewport.
fn top(pane: &Pane) -> u32 {
    let offset = pane.terminal.scrollbar().map_or(0, |bar| bar.offset);
    u32::try_from(offset).unwrap_or(u32::MAX)
}

/// How many rows the viewport can still go down by.
fn lines_below(pane: &Pane) -> u32 {
    let below = pane.terminal.scrollbar().map_or(0, Scrollbar::lines_below);
    u32::try_from(below).unwrap_or(u32::MAX)
}

fn cursor(pane: &Pane, mode: &CopyMode) -> Position {
    Position::new(mode.x, top(pane).saturating_add(u32::from(mode.y)))
}

/// A column the cursor can be in on a line. A rectangle can reach past the text.
fn limit_column(pane: &Pane, mode: &CopyMode, x: u16, y: u32) -> u16 {
    let limit = if mode.rectangle {
        pane.terminal.columns().saturating_sub(1)
    } else {
        pane.terminal.line(y).limit()
    };
    x.min(limit)
}

fn scroll(pane: &mut Pane, rows: i64) {
    let rows = rows.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
    pane.terminal.scroll_by(i32::try_from(rows).unwrap_or(0));
}

/// Moves the cursor, scrolling no further than it takes to see it.
fn go_to(pane: &mut Pane, mode: &mut CopyMode, at: Position) {
    let (first, last) = (top(pane), top(pane) + u32::from(height(pane)) - 1);
    let edge = at.y.clamp(first, last);
    if edge != at.y {
        remember_column(pane, mode);
        scroll(pane, i64::from(at.y) - i64::from(edge));
    }
    let y = at.y.saturating_sub(top(pane));
    mode.y = u16::try_from(y).unwrap_or(u16::MAX).min(height(pane) - 1);
    mode.x = limit_column(pane, mode, at.x, at.y);
}

/// Moves the cursor to what may be far away. If that means scrolling, a quarter
/// of the view is left for what comes after it, as tmux does.
fn go_far(pane: &mut Pane, mode: &mut CopyMode, at: Position) {
    let (first, rows) = (top(pane), u32::from(height(pane)));
    if at.y < first || at.y >= first + rows {
        let gap = rows / 4;
        let history = pane.terminal.rows().saturating_sub(rows);
        let row = if at.y < rows {
            0
        } else if at.y > history + rows - gap {
            history
        } else {
            at.y + gap - rows
        };
        pane.terminal.scroll_to_row(row);
    }
    go_to(pane, mode, at);
    mode.x = at.x.min(pane.terminal.width().saturating_sub(1));
}

/// Notes the column to come back to, before the cursor leaves the line.
fn remember_column(pane: &Pane, mode: &mut CopyMode) {
    let length = pane.terminal.line(cursor(pane, mode).y).length();
    if !mode.rectangle_selection() && mode.x != length {
        mode.column = mode.x;
        mode.column_length = length;
    }
}

/// Goes back to the remembered column, as far as a scrolling motion does. Like
/// in tmux, that can leave the cursor just past the end of the text.
fn restore_column(pane: &Pane, mode: &mut CopyMode) {
    if !mode.rectangle_selection() {
        mode.x = mode.column;
        follow_line_end(pane, mode);
    }
}

/// Keeps a cursor that was at the end of a line at the end of the next one.
fn follow_line_end(pane: &Pane, mode: &mut CopyMode) {
    if mode.rectangle_selection() {
        return;
    }
    let y = cursor(pane, mode).y;
    let length = pane.terminal.line(y).length();
    if mode.x != length && (mode.x >= mode.column_length || mode.x > length) {
        mode.x = limit_column(pane, mode, length, y);
    }
}

/// Moves the cursor up or down a row, scrolling when it is at the edge.
fn move_row(pane: &mut Pane, mode: &mut CopyMode, up: bool) {
    remember_column(pane, mode);
    line_selection_row(pane, mode, up);
    let target = if up {
        mode.y.checked_sub(1)
    } else {
        mode.y.checked_add(1).filter(|y| *y < height(pane))
    };
    if let Some(y) = target {
        mode.y = y;
        if !mode.rectangle_selection() {
            let row = cursor(pane, mode).y;
            mode.x = limit_column(pane, mode, mode.column, row);
            follow_line_end(pane, mode);
        }
    } else {
        pane.terminal.scroll_by(if up { -1 } else { 1 });
        restore_column(pane, mode);
    }
    line_selection_column(pane, mode);
}

/// Scrolls the view, with the cursor staying on its line while that is in view.
pub fn scroll_view(pane: &mut Pane, rows: i32) {
    let Some(mut mode) = pane.copy_mode.take() else {
        return;
    };
    scroll_lines(pane, &mut mode, i64::from(rows));
    update_selection(pane, &mode);
    pane.copy_mode = Some(mode);
}

fn scroll_lines(pane: &mut Pane, mode: &mut CopyMode, rows: i64) {
    let room = if rows < 0 {
        top(pane)
    } else {
        lines_below(pane)
    };
    if room == 0 {
        return;
    }
    remember_column(pane, mode);
    let row = cursor(pane, mode).y;
    scroll(pane, rows);
    let y = row.saturating_sub(top(pane));
    mode.y = u16::try_from(y).unwrap_or(u16::MAX).min(height(pane) - 1);
    restore_column(pane, mode);
}

/// How far `PageUp` and the like go.
const fn page_rows(pane: &Pane, half: bool) -> u32 {
    let rows = height(pane) as u32;
    match (rows > 2, half) {
        (false, _) => 1,
        (true, true) => rows / 2,
        (true, false) => rows - 2,
    }
}

/// Goes a page up, or as far as there is history and then the cursor goes on.
pub fn page_up(pane: &mut Pane, half: bool) {
    let Some(mut mode) = pane.copy_mode.take() else {
        return;
    };
    page(pane, &mut mode, true, half);
    update_selection(pane, &mode);
    pane.copy_mode = Some(mode);
}

fn page(pane: &mut Pane, mode: &mut CopyMode, up: bool, half: bool) {
    remember_column(pane, mode);
    let rows = page_rows(pane, half);
    let room = if up { top(pane) } else { lines_below(pane) };
    if room < rows {
        let moved = u16::try_from(rows).unwrap_or(u16::MAX);
        mode.y = if up {
            pane.terminal.scroll_to_top();
            mode.y.saturating_sub(moved)
        } else {
            pane.terminal.scroll_to_bottom();
            mode.y.saturating_add(moved).min(height(pane) - 1)
        };
    } else {
        let rows = i64::from(rows);
        scroll(pane, if up { -rows } else { rows });
    }
    restore_column(pane, mode);
}

/// Scrolls so that the cursor ends up on row `y` of the view, if there is enough
/// history to do that.
fn scroll_cursor_to(pane: &mut Pane, mode: &mut CopyMode, y: u16) {
    let delta = i64::from(mode.y) - i64::from(y);
    let room = i64::from(if delta > 0 {
        lines_below(pane)
    } else {
        top(pane)
    });
    if delta.abs() <= room {
        scroll(pane, delta);
        mode.y = y;
    }
}

fn to_row_of_view(pane: &Pane, mode: &mut CopyMode, y: u16) {
    mode.x = 0;
    mode.y = y.min(height(pane) - 1);
}

fn history_bottom(pane: &mut Pane, mode: &mut CopyMode) {
    pane.terminal.scroll_to_bottom();
    mode.y = height(pane) - 1;
    let y = cursor(pane, mode).y;
    mode.x = limit_column(pane, mode, u16::MAX, y);
}

fn jump(pane: &mut Pane, mode: &mut CopyMode, kind: Jump, c: char) {
    let at = cursor(pane, mode);
    if let Some(found) = motion::jump(&pane.terminal, at, kind, c) {
        go_to(pane, mode, found);
    }
}

fn select_line(pane: &mut Pane, mode: &mut CopyMode) {
    mode.rectangle = false;
    let at = cursor(pane, mode);
    let start = motion::motion(&pane.terminal, at, Motion::StartOfLine);
    let end = motion::motion(&pane.terminal, at, Motion::EndOfLine { all: false });
    mode.selecting = Selecting::Lines(LineSelection {
        start,
        end,
        row: at.y,
        forward: true,
    });
    go_to(pane, mode, end);
}

fn begin_selection(pane: &mut Pane, mode: &mut CopyMode) {
    mode.selecting = Selecting::Cells;
    pane.terminal.select_start(mode.x, mode.y);
    pane.terminal.set_selection_rectangle(mode.rectangle);
}

fn clear_selection(pane: &mut Pane, mode: &mut CopyMode) {
    mode.selecting = Selecting::No;
    pane.terminal.clear_selection();
}

/// Puts the cursor on the other end of the selection, which it moves from then
/// on. A selection of lines becomes one of cells at that point, as in tmux.
fn other_end(pane: &mut Pane, mode: &mut CopyMode) {
    let moved_end = match mode.selecting {
        Selecting::No => return,
        // The cursor is at the end the terminal has as the start, and the one
        // to go to is already the one that moves.
        Selecting::Lines(lines) if !lines.forward => {
            pane.terminal.selection_bounds().map(|(_, end)| end)
        }
        Selecting::Lines(_) | Selecting::Cells => pane.terminal.swap_selection_ends(),
    };
    mode.selecting = Selecting::Cells;
    if let Some((x, y)) = moved_end {
        go_to(pane, mode, Position::new(x, y));
    }
}

/// A cursor heading out of a selection of lines goes to that side of it, for the
/// selection to grow that way.
fn line_selection_row(pane: &mut Pane, mode: &mut CopyMode, up: bool) {
    let Selecting::Lines(mut lines) = mode.selecting else {
        return;
    };
    let row = cursor(pane, mode).y;
    let Some((start, end)) = pane.terminal.selection_bounds() else {
        return;
    };
    let edge = if up { start } else { end };
    if lines.forward == up && row == edge.1 {
        go_to(pane, mode, Position::new(edge.0, edge.1));
        lines.forward = !lines.forward;
        mode.selecting = Selecting::Lines(lines);
    }
}

fn line_selection_column(pane: &Pane, mode: &mut CopyMode) {
    if let Selecting::Lines(lines) = mode.selecting {
        let row = cursor(pane, mode).y;
        mode.x = if lines.forward {
            limit_column(pane, mode, u16::MAX, row)
        } else {
            0
        };
    }
}

/// Selects the text from here to the end of the line, for copying it.
fn select_to_end_of_line(pane: &mut Pane, mode: &mut CopyMode, lines: u32) {
    begin_selection(pane, mode);
    for _ in 1..lines {
        move_row(pane, mode, false);
    }
    let at = cursor(pane, mode);
    let end = motion::motion(&pane.terminal, at, Motion::EndOfLine { all: false });
    go_to(pane, mode, end);
}

/// Selects from the line the selection started on to the line of the cursor.
fn select_lines(pane: &mut Pane, mode: &CopyMode, lines: LineSelection) {
    let row = cursor(pane, mode).y;
    let (start, end) = if lines.row > row {
        (Position::new(0, row), lines.end)
    } else {
        let y = row.max(lines.end.y);
        (
            lines.start,
            Position::new(pane.terminal.line(y).length(), y),
        )
    };
    let last = pane.terminal.width().saturating_sub(1);
    pane.terminal
        .select_range((start.x, start.y), (end.x.min(last), end.y));
}

/// Makes the selection reach the cursor.
fn update_selection(pane: &mut Pane, mode: &CopyMode) {
    match mode.selecting {
        Selecting::No => {}
        Selecting::Cells => pane.terminal.select_extend(mode.x, mode.y),
        Selecting::Lines(lines) => select_lines(pane, mode, lines),
    }
}

fn run_once(pane: &mut Pane, mode: &mut CopyMode, action: Action, lines: u32) -> Outcome {
    match action {
        Action::Move(step) => {
            let at = cursor(pane, mode);
            let step = match step {
                Motion::Right { .. } => Motion::Right {
                    all: mode.rectangle_selection(),
                },
                Motion::EndOfLine { .. } => Motion::EndOfLine {
                    all: mode.rectangle_selection(),
                },
                step => step,
            };
            let at = motion::motion(&pane.terminal, at, step);
            go_to(pane, mode, at);
        }
        Action::CursorUp => move_row(pane, mode, true),
        Action::CursorDown => move_row(pane, mode, false),
        Action::TopLine => to_row_of_view(pane, mode, 0),
        Action::MiddleLine => to_row_of_view(pane, mode, (height(pane) - 1) / 2),
        Action::BottomLine => to_row_of_view(pane, mode, height(pane) - 1),
        Action::ScrollUp => scroll_lines(pane, mode, -1),
        Action::ScrollDown => {
            scroll_lines(pane, mode, 1);
            return leave_at_bottom(pane, mode);
        }
        Action::ScrollMiddle => scroll_cursor_to(pane, mode, (height(pane) - 1) / 2),
        Action::PageUp => page(pane, mode, true, false),
        Action::PageDown => {
            page(pane, mode, false, false);
            return leave_at_bottom(pane, mode);
        }
        Action::HalfpageUp => page(pane, mode, true, true),
        Action::HalfpageDown => {
            page(pane, mode, false, true);
            return leave_at_bottom(pane, mode);
        }
        Action::HistoryTop => {
            pane.terminal.scroll_to_top();
            to_row_of_view(pane, mode, 0);
        }
        Action::HistoryBottom => history_bottom(pane, mode),
        Action::PreviousParagraph => {
            let at = motion::previous_paragraph(&pane.terminal, cursor(pane, mode).y);
            go_far(pane, mode, at);
        }
        Action::NextParagraph => {
            let at = motion::next_paragraph(&pane.terminal, cursor(pane, mode).y);
            go_far(pane, mode, at);
        }
        Action::NextMatchingBracket => {
            if let Some(at) = motion::matching_bracket(&pane.terminal, cursor(pane, mode)) {
                go_far(pane, mode, at);
            }
        }
        Action::Jump(kind, c) => {
            mode.last_jump = Some((kind, c));
            jump(pane, mode, kind, c);
        }
        Action::JumpAgain => {
            if let Some((kind, c)) = mode.last_jump {
                jump(pane, mode, kind, c);
            }
        }
        Action::JumpReverse => {
            if let Some((kind, c)) = mode.last_jump {
                jump(pane, mode, kind.reverse(), c);
            }
        }
        Action::GotoLine(line) => {
            let history = pane.terminal.rows().saturating_sub(u32::from(height(pane)));
            pane.terminal.scroll_to_row(history.saturating_sub(line));
        }
        Action::BeginSelection => begin_selection(pane, mode),
        Action::ClearSelection => clear_selection(pane, mode),
        Action::RectangleToggle => {
            mode.rectangle = !mode.rectangle;
            if matches!(mode.selecting, Selecting::Lines(_)) {
                mode.selecting = Selecting::Cells;
            }
            if mode.selecting == Selecting::Cells {
                pane.terminal.set_selection_rectangle(mode.rectangle);
            }
            let y = cursor(pane, mode).y;
            mode.x = limit_column(pane, mode, mode.x, y);
        }
        Action::SelectLine => select_line(pane, mode),
        Action::OtherEnd => other_end(pane, mode),
        Action::Cancel => return Outcome::Cancel,
        Action::CopySelection => return Outcome::Copy,
        Action::AppendSelection => return Outcome::Append,
        Action::CopyEndOfLine => {
            select_to_end_of_line(pane, mode, lines);
            return Outcome::Copy;
        }
    }
    Outcome::Stay
}

/// Scrolling down to the end leaves copy mode that the wheel started.
fn leave_at_bottom(pane: &Pane, mode: &CopyMode) -> Outcome {
    if mode.exit_at_bottom && lines_below(pane) == 0 && mode.selecting == Selecting::No {
        Outcome::Cancel
    } else {
        Outcome::Stay
    }
}

/// The text to copy from the selection. As in tmux, a selection that goes past
/// the end of its last line takes the line break and any blank lines with it.
pub fn selected_text(pane: &Pane) -> Option<String> {
    let mut text = pane.terminal.selected_text()?;
    let (start, end) = pane.terminal.selection_bounds()?;
    let line = pane.terminal.line(end.1);
    let selecting = pane.copy_mode.map(|mode| mode.selecting);
    let lines_selected = matches!(selecting, Some(Selecting::Lines(_)));
    if end.0 < line.length() && !lines_selected {
        // The library trims these, but here they are in the middle of a line.
        // It leaves them alone in a rectangle.
        if !pane.copy_mode.is_some_and(|mode| mode.rectangle) {
            let spaces = (0..=end.0).rev().take_while(|x| line.cell(*x) == ' ');
            text.push_str(&" ".repeat(spaces.count()));
        }
    } else if !line.wrapped {
        let blank = (start.1..end.1)
            .rev()
            .map(|y| pane.terminal.line(y + 1))
            .take_while(|line| line.length() == 0)
            .count();
        text.push_str(&"\n".repeat(blank + 1));
    }
    Some(text)
}

/// Runs a command in a pane in copy mode.
pub fn run(pane: &mut Pane, action: Action) -> Outcome {
    let Some(mut mode) = pane.copy_mode.take() else {
        return Outcome::Cancel;
    };
    let repeat = std::mem::replace(&mut mode.repeat, 1);
    let mut outcome = Outcome::Stay;
    let times = if action.repeats() { repeat } else { 1 };
    for _ in 0..times {
        outcome = run_once(pane, &mut mode, action, repeat);
        if outcome != Outcome::Stay {
            break;
        }
    }
    update_selection(pane, &mode);
    pane.copy_mode = Some(mode);
    pane.redraw = true;
    outcome
}
