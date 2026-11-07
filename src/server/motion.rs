//! Finding where copy mode's motions lead, the way tmux's `grid-reader.c` does.
//!
//! Positions count rows of the whole screen, with history first. Everything goes
//! through [`Lines`], which keeps the terminal out of it.

use crate::ghostty::Line;

/// tmux's default `word-separators`.
pub const SEPARATORS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

/// Nothing separates words but white space, as for `W`, `B` and `E`.
pub const NO_SEPARATORS: &str = "";

const BRACKETS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// The text to move around in.
pub trait Lines {
    /// How many rows there are, history included.
    fn rows(&self) -> u32;
    /// How many columns each row has.
    fn columns(&self) -> u16;
    /// One row, which is empty if there is no such row.
    fn line(&self, y: u32) -> Line;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// Row first, for positions to sort in reading order.
    pub y: u32,
    pub x: u16,
}

impl Position {
    pub const fn new(x: u16, y: u32) -> Self {
        Self { y, x }
    }
}

/// A place in the text, keeping the row it is on at hand.
struct Reader<'a, L> {
    lines: &'a L,
    x: u16,
    y: u32,
    line: Line,
}

impl<'a, L: Lines> Reader<'a, L> {
    fn new(lines: &'a L, at: Position) -> Self {
        Self {
            lines,
            x: at.x,
            y: at.y,
            line: lines.line(at.y),
        }
    }

    const fn position(&self) -> Position {
        Position::new(self.x, self.y)
    }

    fn go_to_row(&mut self, y: u32) {
        if y != self.y {
            self.y = y;
            self.line = self.lines.line(y);
        }
    }

    fn last_row(&self) -> u32 {
        self.lines.rows().saturating_sub(1)
    }

    fn wrapped_from_above(&self) -> bool {
        self.y
            .checked_sub(1)
            .is_some_and(|above| self.lines.line(above).wrapped)
    }

    fn in_whitespace(&self) -> bool {
        self.line.cell(self.x).is_whitespace()
    }

    fn in_set(&self, set: &str) -> bool {
        set.contains(self.line.cell(self.x))
    }

    /// Moves off the right half of a double width character.
    fn settle(&mut self) {
        while self.x > 0 && self.line.is_padding(self.x) {
            self.x -= 1;
        }
    }

    fn down(&mut self) {
        if self.y < self.last_row() {
            self.go_to_row(self.y + 1);
        }
        self.settle();
    }

    fn up(&mut self) {
        self.go_to_row(self.y.saturating_sub(1));
        self.settle();
    }

    fn start_of_line(&mut self, wrap: bool) {
        if wrap {
            while self.wrapped_from_above() {
                self.go_to_row(self.y - 1);
            }
        }
        self.x = 0;
    }

    /// With `all`, goes past the text to the edge of the screen.
    fn end_of_line(&mut self, wrap: bool, all: bool) {
        if wrap {
            while self.y < self.last_row() && self.line.wrapped {
                self.go_to_row(self.y + 1);
            }
        }
        self.x = if all {
            self.lines.columns()
        } else {
            self.line.length()
        };
    }

    fn right(&mut self, wrap: bool, all: bool) {
        let limit = if all {
            self.lines.columns()
        } else {
            self.line.limit()
        };
        if wrap && self.x >= limit && self.y < self.last_row() {
            self.start_of_line(false);
            self.down();
        } else if self.x < limit {
            self.x += 1;
            while self.x < limit && self.line.is_padding(self.x) {
                self.x += 1;
            }
        }
    }

    fn left(&mut self, wrap: bool) {
        self.settle();
        if self.x == 0 && self.y > 0 && (wrap || self.wrapped_from_above()) {
            self.up();
            self.end_of_line(false, false);
        } else if self.x > 0 {
            self.x -= 1;
        }
    }

    /// The last column a word on this row can reach. A word goes on past the
    /// edge if the row wraps.
    fn word_limit(&self) -> u16 {
        if self.line.wrapped {
            self.lines.columns().saturating_sub(1)
        } else {
            self.line.length()
        }
    }

    /// Moves on to the next rows until the position is within `limit`. Returns
    /// false if that would go past the last row.
    fn handle_wrap(&mut self, limit: &mut u16) -> bool {
        while self.x > *limit {
            if self.y >= self.last_row() {
                return false;
            }
            self.start_of_line(false);
            self.down();
            *limit = self.word_limit();
        }
        true
    }

    /// Skips the rest of the word the position is in. What counts as the word
    /// is a run of separators if it starts on one, and otherwise a run of
    /// anything but separators.
    fn skip_word(&mut self, limit: &mut u16, separators: &str) {
        let in_separators = self.in_set(separators);
        loop {
            self.x += 1;
            let goes_on = self.handle_wrap(limit)
                && !self.in_whitespace()
                && self.in_set(separators) == in_separators;
            if !goes_on {
                break;
            }
        }
    }

    fn next_word(&mut self, separators: &str) {
        let mut limit = self.word_limit();
        if !self.handle_wrap(&mut limit) {
            return;
        }
        if !self.in_whitespace() {
            self.skip_word(&mut limit, separators);
        }
        while self.handle_wrap(&mut limit) && self.in_whitespace() {
            self.x += 1;
        }
    }

    /// Goes to just after the end of the next word.
    fn past_next_word(&mut self, separators: &str) {
        let mut limit = self.word_limit();
        while self.handle_wrap(&mut limit) {
            if !self.in_whitespace() {
                self.skip_word(&mut limit, separators);
                return;
            }
            self.x += 1;
        }
    }

    fn next_word_end(&mut self, separators: &str) {
        if !self.in_whitespace() {
            self.right(false, false);
        }
        self.past_next_word(separators);
        self.left(true);
    }

    fn previous_word(&mut self, separators: &str) {
        loop {
            if self.x > 0 {
                self.x -= 1;
                if !self.in_whitespace() {
                    break;
                }
            } else {
                if self.y == 0 {
                    return;
                }
                self.up();
                self.end_of_line(false, false);
            }
        }
        let in_separators = self.in_set(separators);
        loop {
            let (x, y) = (self.x, self.y);
            if self.x == 0 {
                if !self.wrapped_from_above() {
                    break;
                }
                self.up();
                self.end_of_line(false, true);
            }
            self.x = self.x.saturating_sub(1);
            if self.in_whitespace() || self.in_set(separators) != in_separators {
                self.x = x;
                self.go_to_row(y);
                break;
            }
        }
    }

    /// Finds a character from here on, through wrapped rows only.
    fn jump(&mut self, c: char) -> bool {
        let mut x = self.x;
        let mut y = self.y;
        loop {
            let line = self.lines.line(y);
            if let Some(found) = (x..line.length()).find(|x| line.cell(*x) == c) {
                self.x = found;
                self.go_to_row(y);
                return true;
            }
            if y >= self.last_row() || !line.wrapped {
                return false;
            }
            x = 0;
            y += 1;
        }
    }

    /// Finds a character from here back, through wrapped rows only.
    fn jump_back(&mut self, c: char) -> bool {
        let mut end = self.x.saturating_add(1);
        let mut y = self.y;
        loop {
            let line = self.lines.line(y);
            if let Some(found) = (0..end).rev().find(|x| line.cell(*x) == c) {
                self.x = found;
                self.go_to_row(y);
                return true;
            }
            let Some(above) = y.checked_sub(1) else {
                return false;
            };
            let line = self.lines.line(above);
            if !line.wrapped {
                return false;
            }
            end = line.length();
            y = above;
        }
    }

    fn back_to_indentation(&mut self) {
        let start = self.position();
        self.start_of_line(true);
        loop {
            let length = self.line.length();
            let indent = (0..length).find(|x| {
                let c = self.line.cell(*x);
                !c.is_whitespace() && !self.line.is_padding(*x)
            });
            if let Some(x) = indent {
                self.x = x;
                return;
            }
            if !self.line.wrapped || self.y >= self.last_row() {
                break;
            }
            self.go_to_row(self.y + 1);
        }
        self.x = start.x;
        self.go_to_row(start.y);
    }

    /// Steps one character back through the text, skipping empty rows. Returns
    /// false at the start of the text.
    fn step_back(&mut self) -> bool {
        while self.x == 0 {
            if self.y == 0 {
                return false;
            }
            self.go_to_row(self.y - 1);
            self.x = self.line.length();
        }
        self.x -= 1;
        true
    }

    /// Steps one character on through the text. Returns false at its end.
    fn step_forward(&mut self) -> bool {
        if self.x < self.line.length() {
            self.x += 1;
            return true;
        }
        if self.y >= self.last_row() {
            return false;
        }
        self.x = 0;
        self.go_to_row(self.y + 1);
        true
    }

    /// Walks to the bracket closing or opening the one here. `step` walks in the
    /// direction of `end`.
    fn matching(&mut self, end: char, step: fn(&mut Self) -> bool) -> bool {
        let start = self.line.cell(self.x);
        let mut depth = 1_u32;
        while step(self) {
            let c = self.line.cell(self.x);
            if c == start {
                depth += 1;
            } else if c == end {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
        }
        false
    }
}

/// A motion that needs nothing but the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    /// With `all`, the cursor can go on past the text.
    Right {
        all: bool,
    },
    StartOfLine,
    EndOfLine {
        all: bool,
    },
    BackToIndentation,
    NextWord(&'static str),
    NextWordEnd(&'static str),
    PreviousWord(&'static str),
}

/// Where a motion from `at` leads.
pub fn motion(lines: &impl Lines, at: Position, motion: Motion) -> Position {
    let mut reader = Reader::new(lines, at);
    match motion {
        Motion::Left => reader.left(true),
        Motion::Right { all } => reader.right(true, all),
        Motion::StartOfLine => reader.start_of_line(true),
        Motion::EndOfLine { all } => reader.end_of_line(true, all),
        Motion::BackToIndentation => reader.back_to_indentation(),
        Motion::NextWord(separators) => reader.next_word(separators),
        Motion::NextWordEnd(separators) => reader.next_word_end(separators),
        Motion::PreviousWord(separators) => reader.previous_word(separators),
    }
    reader.position()
}

/// How to look for a character on the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Jump {
    /// `f`, onto the character.
    Forward,
    /// `F`.
    Backward,
    /// `t`, up to just before the character.
    ToForward,
    /// `T`.
    ToBackward,
}

impl Jump {
    /// The same jump in the other direction, for `,`.
    pub const fn reverse(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
            Self::ToForward => Self::ToBackward,
            Self::ToBackward => Self::ToForward,
        }
    }
}

/// Where a jump to `c` from `at` leads, if `c` is there.
pub fn jump(lines: &impl Lines, at: Position, kind: Jump, c: char) -> Option<Position> {
    let mut reader = Reader::new(lines, at);
    let found = match kind {
        Jump::Forward => {
            reader.x = at.x.saturating_add(1);
            reader.jump(c)
        }
        Jump::Backward => {
            reader.left(false);
            reader.jump_back(c)
        }
        // Starting one further makes repeating `t` get past the character it
        // stopped in front of.
        Jump::ToForward => {
            reader.x = at.x.saturating_add(2);
            let found = reader.jump(c);
            if found {
                reader.left(true);
            }
            found
        }
        Jump::ToBackward => {
            reader.left(false);
            reader.left(false);
            let found = reader.jump_back(c);
            if found {
                reader.right(true, false);
            }
            found
        }
    };
    found.then(|| reader.position())
}

/// The blank row before the paragraph at row `y`, or the first row.
pub fn previous_paragraph(lines: &impl Lines, mut y: u32) -> Position {
    while y > 0 && lines.line(y).length() == 0 {
        y -= 1;
    }
    while y > 0 && lines.line(y).length() > 0 {
        y -= 1;
    }
    Position::new(0, y)
}

/// The blank row after the paragraph at row `y`, or the end of the last row.
pub fn next_paragraph(lines: &impl Lines, mut y: u32) -> Position {
    let last = lines.rows().saturating_sub(1);
    while y < last && lines.line(y).length() == 0 {
        y += 1;
    }
    while y < last && lines.line(y).length() > 0 {
        y += 1;
    }
    Position::new(lines.line(y).length(), y)
}

/// Where the bracket that goes with the first one at or after `at` is. Only the
/// rest of the line is looked at for that first one.
pub fn matching_bracket(lines: &impl Lines, at: Position) -> Option<Position> {
    let mut reader = Reader::new(lines, at);
    loop {
        let c = reader.line.cell(reader.x);
        if let Some((_, close)) = BRACKETS.iter().find(|(open, _)| *open == c) {
            let found = reader.matching(*close, Reader::step_forward);
            return found.then(|| reader.position());
        }
        if let Some((open, _)) = BRACKETS.iter().find(|(_, close)| *close == c) {
            let found = reader.matching(*open, Reader::step_back);
            return found.then(|| reader.position());
        }
        if reader.x >= reader.line.length() {
            if !reader.line.wrapped || reader.y >= reader.last_row() {
                return None;
            }
            reader.x = 0;
            reader.go_to_row(reader.y + 1);
        } else {
            reader.x += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Text(Vec<Line>);

    impl Lines for Text {
        fn rows(&self) -> u32 {
            u32::try_from(self.0.len()).unwrap()
        }

        fn columns(&self) -> u16 {
            20
        }

        fn line(&self, y: u32) -> Line {
            self.0.get(y as usize).cloned().unwrap_or_default()
        }
    }

    /// Rows of text, where a row ending in `\` wraps onto the next one and is
    /// as wide as the screen.
    fn text(rows: &[&str]) -> Text {
        let line = |row: &&str| {
            let (row, wrapped) = row
                .strip_suffix('\\')
                .map_or((*row, false), |row| (row, true));
            let cells = row.chars().collect();
            Line {
                cells,
                wrapped,
                prompt: false,
            }
        };
        Text(rows.iter().map(line).collect())
    }

    fn walk(text: &Text, step: Motion, stops: &[(u16, u32)]) {
        let mut at = Position::new(stops[0].0, stops[0].1);
        for &(x, y) in &stops[1..] {
            at = motion(text, at, step);
            assert_eq!(at, Position::new(x, y));
        }
    }

    #[test]
    fn next_word_stops_at_words_and_runs_of_separators() {
        let text = text(&["foo bar--baz  qux", "", "   next", "end"]);
        let stops = [(0, 0), (4, 0), (7, 0), (9, 0), (14, 0), (3, 2), (0, 3)];
        walk(&text, Motion::NextWord(SEPARATORS), &stops);
    }

    #[test]
    fn next_space_only_stops_after_white_space() {
        let text = text(&["foo bar--baz  qux"]);
        let stops = [(0, 0), (4, 0), (14, 0)];
        walk(&text, Motion::NextWord(NO_SEPARATORS), &stops);
    }

    #[test]
    fn next_word_end_stops_on_the_last_character() {
        let text = text(&["foo bar--baz  qux", "x"]);
        let stops = [(0, 0), (2, 0), (6, 0), (8, 0), (11, 0), (16, 0)];
        walk(&text, Motion::NextWordEnd(SEPARATORS), &stops);
        // As in tmux, the next line is only reached from before the last word.
        walk(&text, Motion::NextWordEnd(SEPARATORS), &[(16, 0), (16, 0)]);
    }

    #[test]
    fn previous_word_goes_back_through_lines() {
        let text = text(&["foo bar--baz", "", "  qux"]);
        let stops = [(2, 2), (9, 0), (7, 0), (4, 0), (0, 0), (0, 0)];
        walk(&text, Motion::PreviousWord(SEPARATORS), &stops);
    }

    #[test]
    fn previous_space_treats_separators_as_part_of_the_word() {
        let text = text(&["foo bar--baz"]);
        let stops = [(11, 0), (4, 0), (0, 0)];
        walk(&text, Motion::PreviousWord(NO_SEPARATORS), &stops);
    }

    #[test]
    fn a_word_goes_on_across_a_wrapped_row() {
        let text = text(&["one twenty-long-word\\", "more rest"]);
        walk(
            &text,
            Motion::PreviousWord(NO_SEPARATORS),
            &[(2, 1), (4, 0)],
        );
        walk(&text, Motion::NextWord(NO_SEPARATORS), &[(4, 0), (5, 1)]);
    }

    #[test]
    fn jumps_on_the_line_only() {
        let text = text(&["a,b,c", "d,e"]);
        let at = Position::new(0, 0);
        assert_eq!(
            jump(&text, at, Jump::Forward, ','),
            Some(Position::new(1, 0))
        );
        assert_eq!(
            jump(&text, Position::new(1, 0), Jump::Forward, ','),
            Some(Position::new(3, 0))
        );
        assert_eq!(jump(&text, Position::new(3, 0), Jump::Forward, ','), None);
        assert_eq!(jump(&text, at, Jump::Forward, 'e'), None);
        assert_eq!(
            jump(&text, Position::new(4, 0), Jump::Backward, ','),
            Some(Position::new(3, 0))
        );
        assert_eq!(
            jump(&text, Position::new(1, 1), Jump::Backward, 'a'),
            None,
            "the previous row does not wrap"
        );
    }

    #[test]
    fn jumps_stop_short_and_repeat_past_the_character() {
        let text = text(&["a,b,c"]);
        let to = |x, kind| jump(&text, Position::new(x, 0), kind, ',').map(|at| at.x);
        assert_eq!(to(0, Jump::ToForward), Some(2));
        assert_eq!(to(1, Jump::ToForward), Some(2));
        assert_eq!(to(2, Jump::ToForward), None);
        assert_eq!(to(4, Jump::ToBackward), Some(2));
        assert_eq!(to(3, Jump::ToBackward), Some(2));
        assert_eq!(to(2, Jump::ToBackward), None);
    }

    #[test]
    fn jumps_follow_wrapped_rows() {
        let text = text(&["ab\\", "cd"]);
        assert_eq!(
            jump(&text, Position::new(0, 0), Jump::Forward, 'd'),
            Some(Position::new(1, 1))
        );
        assert_eq!(
            jump(&text, Position::new(1, 1), Jump::Backward, 'a'),
            Some(Position::new(0, 0))
        );
    }

    #[test]
    fn finds_the_indentation_and_the_ends_of_lines() {
        let text = text(&["   abc  ", "", "x"]);
        let at = Position::new(6, 0);
        assert_eq!(motion(&text, at, Motion::BackToIndentation).x, 3);
        assert_eq!(motion(&text, at, Motion::StartOfLine).x, 0);
        assert_eq!(
            motion(&text, at, Motion::EndOfLine { all: false }).x,
            6,
            "one past the text, which the cursor limit then corrects"
        );
        let blank = Position::new(0, 1);
        assert_eq!(motion(&text, blank, Motion::BackToIndentation), blank);
    }

    #[test]
    fn moving_sideways_goes_on_to_the_neighboring_lines() {
        let text = text(&["ab", "cd"]);
        let right = Motion::Right { all: false };
        walk(&text, right, &[(0, 0), (1, 0), (0, 1), (1, 1), (1, 1)]);
        walk(&text, Motion::Left, &[(1, 1), (0, 1), (2, 0), (1, 0)]);
    }

    #[test]
    fn steps_over_double_width_characters() {
        let line = Line {
            cells: vec!['a', '世', '\0', 'b'],
            wrapped: false,
            prompt: false,
        };
        let text = Text(vec![line]);
        let right = Motion::Right { all: false };
        walk(&text, right, &[(0, 0), (1, 0), (3, 0)]);
        assert_eq!(motion(&text, Position::new(3, 0), Motion::Left).x, 2);
    }

    #[test]
    fn paragraphs_end_at_blank_rows() {
        let text = text(&["a", "b", "", "", "c", "d"]);
        assert_eq!(previous_paragraph(&text, 5), Position::new(0, 3));
        assert_eq!(previous_paragraph(&text, 3), Position::new(0, 0));
        assert_eq!(next_paragraph(&text, 0), Position::new(0, 2));
        assert_eq!(next_paragraph(&text, 2), Position::new(1, 5));
    }

    #[test]
    fn matches_brackets_across_lines() {
        let text = text(&["fn f(a[0]) {", "  (x)", "}"]);
        let open = Position::new(4, 0);
        assert_eq!(matching_bracket(&text, open), Some(Position::new(9, 0)));
        assert_eq!(
            matching_bracket(&text, Position::new(9, 0)),
            Some(open),
            "going back from the closing one"
        );
        assert_eq!(
            matching_bracket(&text, Position::new(11, 0)),
            Some(Position::new(0, 2))
        );
        assert_eq!(
            matching_bracket(&text, Position::new(0, 0)),
            Some(Position::new(9, 0)),
            "the first bracket on the rest of the line"
        );
        assert_eq!(
            matching_bracket(&text, Position::new(0, 2)).map(|at| at.y),
            Some(0)
        );
    }

    #[test]
    fn unbalanced_brackets_match_nothing() {
        let text = text(&["(a", "no brackets"]);
        assert_eq!(matching_bracket(&text, Position::new(0, 0)), None);
        assert_eq!(matching_bracket(&text, Position::new(0, 1)), None);
    }

    #[test]
    fn positions_sort_in_reading_order() {
        assert!(Position::new(9, 0) < Position::new(0, 1));
        assert!(Position::new(1, 1) < Position::new(2, 1));
    }
}
