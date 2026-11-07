//! State of the interface around the panes: the prompt, lists and passing
//! messages.

use std::time::{Duration, Instant};

use super::copy_mode::CopyPrompt;
use crate::command::Command;
use crate::ghostty::key;
use crate::input::KeyEvent;

/// How long a message stays on the status line.
const MESSAGE_TIME: Duration = Duration::from_secs(3);

/// How long pane numbers stay on display, which is tmux's `display-panes-time`.
const PANE_NUMBERS_TIME: Duration = Duration::from_secs(1);

/// What to do with the text of a finished prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum PromptAction {
    /// Run the text as a command, or as part of the template if there is one.
    Command(Option<String>),
    /// Run this command if the answer is `y`.
    Confirm(Box<Command>),
    CopyMode(CopyPrompt),
}

pub struct Prompt {
    pub label: String,
    pub action: PromptAction,
    text: Vec<char>,
    cursor: usize,
}

/// What came of a key press at the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptOutcome {
    Editing,
    Cancelled,
    Done(String),
    /// Done, and the key that ended it is still to be acted on.
    DoneBefore(String),
}

impl Prompt {
    pub fn new(label: &str, initial: &str, action: PromptAction) -> Self {
        let text: Vec<char> = initial.chars().collect();
        Self {
            label: label.to_owned(),
            action,
            cursor: text.len(),
            text,
        }
    }

    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    pub fn text_before_cursor(&self) -> String {
        self.text.iter().take(self.cursor).collect()
    }

    pub fn insert(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    fn delete_before_cursor(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }

    /// Takes the key itself as the answer, if it stands for a character.
    fn one_key(event: &KeyEvent) -> PromptOutcome {
        if event.key == key::ESCAPE || event.is_ctrl('c') || event.text.is_empty() {
            PromptOutcome::Cancelled
        } else {
            PromptOutcome::Done(event.text.clone())
        }
    }

    /// Takes digits, and is done at the first key that is not one.
    fn digit_key(&mut self, event: &KeyEvent) -> PromptOutcome {
        if event.key == key::ESCAPE || event.is_ctrl('c') {
            return PromptOutcome::Cancelled;
        }
        match event.char() {
            Some(c) if c.is_ascii_digit() => {
                self.insert(&event.text);
                PromptOutcome::Editing
            }
            _ if event.key == key::ENTER => PromptOutcome::Done(self.text()),
            _ => PromptOutcome::DoneBefore(self.text()),
        }
    }

    pub fn handle_key(&mut self, event: &KeyEvent) -> PromptOutcome {
        match self.action {
            PromptAction::Confirm(_) => {
                return if event.char() == Some('y') {
                    PromptOutcome::Done(String::new())
                } else {
                    PromptOutcome::Cancelled
                };
            }
            PromptAction::CopyMode(CopyPrompt::Jump(_)) => return Self::one_key(event),
            PromptAction::CopyMode(CopyPrompt::Repeat) => return self.digit_key(event),
            _ => {}
        }
        match event.key {
            key::ENTER => return PromptOutcome::Done(self.text()),
            key::ESCAPE => return PromptOutcome::Cancelled,
            key::BACKSPACE => self.delete_before_cursor(),
            key::ARROW_LEFT => self.cursor = self.cursor.saturating_sub(1),
            key::ARROW_RIGHT => self.cursor = (self.cursor + 1).min(self.text.len()),
            key::HOME => self.cursor = 0,
            key::END => self.cursor = self.text.len(),
            _ if event.is_ctrl('c') || event.is_ctrl('g') => return PromptOutcome::Cancelled,
            _ if event.is_ctrl('a') => self.cursor = 0,
            _ if event.is_ctrl('e') => self.cursor = self.text.len(),
            _ if event.is_ctrl('u') => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            _ => self.insert(&event.text),
        }
        PromptOutcome::Editing
    }
}

/// A row of a [`List`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListRow {
    pub text: String,
    /// What choosing this row does. Rows without one are only there to read.
    pub command: Option<Command>,
    /// What `d` does on this row.
    delete: Option<Command>,
    /// The rows that go below this one once it unfolds.
    folded: Vec<Self>,
    /// The rows below this one are on display.
    unfolded: bool,
    /// It is one of those rows.
    nested: bool,
}

impl ListRow {
    pub fn new(text: String, command: Command) -> Self {
        Self {
            command: Some(command),
            ..Self::text(text)
        }
    }

    pub const fn text(text: String) -> Self {
        Self {
            text,
            command: None,
            delete: None,
            folded: Vec::new(),
            unfolded: false,
            nested: false,
        }
    }

    pub fn with_delete(self, command: Command) -> Self {
        Self {
            delete: Some(command),
            ..self
        }
    }

    /// This row with some rows below it, for `Right` to show.
    pub fn with_folded(self, rows: Vec<Self>) -> Self {
        Self {
            folded: rows,
            ..self
        }
    }

    /// This row as one of the rows below another one.
    pub fn nested(self) -> Self {
        Self {
            nested: true,
            ..self
        }
    }

    /// This row as one that has its rows on display below it already.
    pub fn unfolded(self) -> Self {
        Self {
            unfolded: true,
            ..self
        }
    }

    /// What goes in front of the row, which says that it can fold, as in tmux.
    pub const fn marker(&self) -> &'static str {
        if self.unfolded {
            "- "
        } else if self.folded.is_empty() {
            ""
        } else {
            "+ "
        }
    }
}

/// A list over the panes, to read or to choose from. It covers what tmux does
/// with its view mode and the lists of its choose commands.
pub struct List {
    pub rows: Vec<ListRow>,
    pub selected: usize,
    top: usize,
}

/// What came of a key press in a list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListOutcome {
    Stay,
    Close,
    Run(Command),
    /// Run this, and show the list again on this row.
    Delete(Command, usize),
}

impl List {
    pub const fn new(rows: Vec<ListRow>, selected: usize) -> Self {
        Self {
            rows,
            selected,
            top: 0,
        }
    }

    /// The first row on display in a view this high, which keeps the selected
    /// row in sight.
    pub fn top(&self, height: usize) -> usize {
        let last_top = self.rows.len().saturating_sub(height);
        let lowest = (self.selected + 1).saturating_sub(height);
        // Not `clamp`, which panics when there is no room for even one row.
        self.top.max(lowest).min(self.selected).min(last_top)
    }

    /// Moves the selection. Going past either end comes back at the other one.
    pub fn step(&mut self, down: bool) {
        let count = self.rows.len().max(1);
        self.selected = (self.selected + if down { 1 } else { count - 1 }) % count;
    }

    /// Selects a row given where it is in a view this high.
    pub fn select_at(&mut self, y: usize, height: usize) {
        let row = self.top(height) + y;
        if row < self.rows.len() {
            self.selected = row;
            self.top = self.top(height);
        }
    }

    fn move_by(&mut self, rows: isize) {
        let last = self.rows.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(rows).min(last);
    }

    /// Shows the rows below the selected row. With nothing to show, this goes
    /// down a row, as tmux does.
    fn unfold(&mut self) {
        let below = self.selected + 1;
        let row = self.rows.get_mut(self.selected);
        match row.filter(|row| !row.folded.is_empty()) {
            Some(row) => {
                let rows = std::mem::take(&mut row.folded);
                row.unfolded = true;
                self.rows.splice(below..below, rows);
            }
            None => self.move_by(1),
        }
    }

    /// Hides the rows below the selected row, or the rows it is one of, going to
    /// the row they are below. With nothing to hide, this goes up a row.
    fn fold(&mut self) {
        let nested = |row: &ListRow| row.nested;
        while self.selected > 0 && self.rows.get(self.selected).is_some_and(nested) {
            self.selected -= 1;
        }
        let below = self.selected + 1;
        let rest = self.rows.get(below..).unwrap_or_default();
        let count = rest.iter().take_while(|row| nested(row)).count();
        if count == 0 {
            self.move_by(-1);
            return;
        }
        let rows = self.rows.drain(below..below + count).collect();
        if let Some(row) = self.rows.get_mut(self.selected) {
            row.folded = rows;
            row.unfolded = false;
        }
    }

    /// What choosing the selected row does.
    pub fn choose(&self) -> ListOutcome {
        let row = self.rows.get(self.selected);
        row.and_then(|row| row.command.clone())
            .map_or(ListOutcome::Close, ListOutcome::Run)
    }

    /// Handles a key press in a view this high.
    pub fn handle_key(&mut self, event: &KeyEvent, height: usize) -> ListOutcome {
        let page = isize::try_from(height.max(1)).unwrap_or(1);
        match (event.key, event.char()) {
            _ if event.is_ctrl('c') || event.is_ctrl('g') => return ListOutcome::Close,
            _ if event.is_ctrl('p') => self.step(false),
            _ if event.is_ctrl('n') => self.step(true),
            _ if event.is_ctrl('f') => self.move_by(page),
            (key::ESCAPE, _) | (_, Some('q')) => return ListOutcome::Close,
            (key::ENTER, _) => return self.choose(),
            (key::ARROW_UP, _) | (_, Some('k')) => self.step(false),
            (key::ARROW_DOWN, _) | (_, Some('j')) => self.step(true),
            (key::ARROW_RIGHT, _) | (_, Some('l' | '+')) => self.unfold(),
            (key::ARROW_LEFT, _) | (_, Some('h' | '-')) => self.fold(),
            (key::PAGE_UP, _) => self.move_by(-page),
            (key::PAGE_DOWN, _) => self.move_by(page),
            (key::HOME, _) | (_, Some('g')) => self.selected = 0,
            (key::END, _) | (_, Some('G')) => self.move_by(isize::MAX),
            (_, Some('d')) => {
                let row = self.rows.get(self.selected);
                if let Some(command) = row.and_then(|row| row.delete.clone()) {
                    return ListOutcome::Delete(command, self.selected);
                }
            }
            (_, Some(digit @ '0'..='9')) => {
                let index = digit.to_digit(10).and_then(|digit| digit.try_into().ok());
                let row = index.and_then(|index: usize| self.rows.get(index));
                if let Some(command) = row.and_then(|row| row.command.clone()) {
                    return ListOutcome::Run(command);
                }
            }
            _ => {}
        }
        self.top = self.top(height);
        ListOutcome::Stay
    }
}

/// The numbers of `display-panes` on top of the panes.
pub struct PaneNumbers {
    /// None if they stay on until a key is pressed.
    until: Option<Instant>,
}

#[derive(Default)]
pub struct Ui {
    pub prompt: Option<Prompt>,
    pub list: Option<List>,
    pub pane_numbers: Option<PaneNumbers>,
    message: Option<(String, Instant)>,
}

impl Ui {
    pub fn show_message(&mut self, text: String) {
        self.message = Some((text, Instant::now() + MESSAGE_TIME));
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_ref().map(|(text, _)| text.as_str())
    }

    pub fn show_pane_numbers(&mut self, time: Option<Duration>) {
        let time = time.unwrap_or(PANE_NUMBERS_TIME);
        let until = (!time.is_zero()).then(|| Instant::now() + time);
        self.pane_numbers = Some(PaneNumbers { until });
    }

    /// When the pane numbers on display go away.
    pub fn pane_numbers_deadline(&self) -> Option<Instant> {
        self.pane_numbers.as_ref()?.until
    }

    /// Forgets pane numbers past their time. Returns whether there were any.
    pub fn expire_pane_numbers(&mut self) -> bool {
        let expired = self
            .pane_numbers_deadline()
            .is_some_and(|until| Instant::now() >= until);
        if expired {
            self.pane_numbers = None;
        }
        expired
    }

    /// When the message on display goes away.
    pub fn message_deadline(&self) -> Option<Instant> {
        self.message.as_ref().map(|(_, expires)| *expires)
    }

    /// Forgets a message past its time. Returns whether there was one.
    pub fn expire_message(&mut self) -> bool {
        let expired = self
            .message_deadline()
            .is_some_and(|expires| Instant::now() >= expires);
        if expired {
            self.message = None;
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_copes_without_room_for_a_single_row() {
        let rows = vec![ListRow::new("a".to_owned(), Command::KillServer)];
        assert_eq!(List::new(rows, 0).top(0), 0);
    }
    use crate::input::{Event, Parser};

    fn key(bytes: &[u8]) -> KeyEvent {
        let key = Parser::default()
            .feed(bytes)
            .into_iter()
            .find_map(|event| match event {
                Event::Key(event) => Some(event),
                _ => None,
            });
        key.expect("a key event")
    }

    fn typed(prompt: &mut Prompt, text: &str) {
        for c in text.chars() {
            prompt.handle_key(&key(c.to_string().as_bytes()));
        }
    }

    #[test]
    fn edits_text() {
        let mut prompt = Prompt::new(":", "", PromptAction::Command(None));
        typed(&mut prompt, "helo");
        prompt.handle_key(&key(b"\x1b[D"));
        typed(&mut prompt, "l");
        assert_eq!(prompt.text(), "hello");
        assert_eq!(prompt.text_before_cursor(), "hell");
        prompt.handle_key(&key(&[0x7f]));
        assert_eq!(prompt.text(), "helo");
        assert_eq!(
            prompt.handle_key(&key(b"\r")),
            PromptOutcome::Done("helo".to_owned())
        );
    }

    #[test]
    fn starts_with_initial_text_ready_to_edit() {
        let mut prompt = Prompt::new("name: ", "zsh", PromptAction::Command(None));
        typed(&mut prompt, "2");
        assert_eq!(prompt.text(), "zsh2");
    }

    #[test]
    fn clears_to_the_start_of_the_line() {
        let mut prompt = Prompt::new(":", "abcd", PromptAction::Command(None));
        prompt.handle_key(&key(b"\x1b[D"));
        prompt.handle_key(&key(&[0x15]));
        assert_eq!(prompt.text(), "d");
    }

    #[test]
    fn cancels() {
        let mut prompt = Prompt::new(":", "x", PromptAction::Command(None));
        assert_eq!(prompt.handle_key(&key(&[3])), PromptOutcome::Cancelled);
        assert_eq!(
            prompt.handle_key(&key(b"\x1b[27u")),
            PromptOutcome::Cancelled
        );
    }

    #[test]
    fn confirmation_takes_only_y() {
        let action = PromptAction::Confirm(Box::new(Command::LastWindow));
        let mut prompt = Prompt::new("kill? ", "", action);
        assert_eq!(prompt.handle_key(&key(b"n")), PromptOutcome::Cancelled);
        assert_eq!(
            prompt.handle_key(&key(b"y")),
            PromptOutcome::Done(String::new())
        );
    }

    fn list(rows: usize) -> List {
        let row = |index: usize| ListRow::new(index.to_string(), Command::LastWindow);
        List::new((0..rows).map(row).collect(), 0)
    }

    #[test]
    fn moves_through_a_list_and_wraps_around() {
        let mut list = list(3);
        assert_eq!(list.handle_key(&key(b"j"), 10), ListOutcome::Stay);
        assert_eq!(list.selected, 1);
        list.handle_key(&key(b"\x1b[B"), 10);
        assert_eq!(list.selected, 2);
        list.handle_key(&key(b"j"), 10);
        assert_eq!(list.selected, 0);
        list.handle_key(&key(b"k"), 10);
        assert_eq!(list.selected, 2);
        list.handle_key(&key(b"g"), 10);
        assert_eq!(list.selected, 0);
        list.handle_key(&key(b"G"), 10);
        assert_eq!(list.selected, 2);
    }

    #[test]
    fn scrolls_a_list_to_keep_the_selection_in_view() {
        let mut list = list(20);
        assert_eq!(list.top(5), 0);
        for _ in 0..7 {
            list.handle_key(&key(b"j"), 5);
        }
        assert_eq!(list.selected, 7);
        assert_eq!(list.top(5), 3);
        list.handle_key(&key(b"k"), 5);
        assert_eq!(list.top(5), 3);
        list.handle_key(&key(b"\x1b[6~"), 5);
        assert_eq!(list.selected, 11);
        assert_eq!(list.top(5), 7);
        list.handle_key(&key(b"G"), 5);
        assert_eq!(list.top(5), 15);
        assert_eq!(list.top(100), 0);
    }

    #[test]
    fn a_list_survives_having_no_rows() {
        let mut list = list(0);
        assert_eq!(list.handle_key(&key(b"j"), 5), ListOutcome::Stay);
        assert_eq!(list.handle_key(&key(b"G"), 5), ListOutcome::Stay);
        assert_eq!(list.handle_key(&key(b"\r"), 5), ListOutcome::Close);
        assert_eq!(list.top(5), 0);
    }

    #[test]
    fn chooses_from_a_list() {
        let mut rows = list(3);
        rows.handle_key(&key(b"j"), 5);
        assert_eq!(
            rows.handle_key(&key(b"\r"), 5),
            ListOutcome::Run(Command::LastWindow)
        );
        assert_eq!(
            rows.handle_key(&key(b"2"), 5),
            ListOutcome::Run(Command::LastWindow)
        );
        assert_eq!(rows.handle_key(&key(b"7"), 5), ListOutcome::Stay);
        assert_eq!(rows.handle_key(&key(b"q"), 5), ListOutcome::Close);
    }

    #[test]
    fn rows_without_a_command_are_only_read() {
        let mut list = List::new(vec![ListRow::text("a".to_owned())], 0);
        assert_eq!(list.handle_key(&key(b"0"), 5), ListOutcome::Stay);
        assert_eq!(list.handle_key(&key(b"\r"), 5), ListOutcome::Close);
    }

    /// A row with two rows below it, between two rows.
    fn tree() -> List {
        let row = |text: &str| ListRow::new(text.to_owned(), Command::LastWindow);
        let nested = |text: &str| row(text).nested();
        let rows = vec![
            row("a"),
            row("b").with_folded(vec![nested("b1"), nested("b2")]),
            row("c"),
        ];
        List::new(rows, 1)
    }

    fn texts(list: &List) -> Vec<&str> {
        list.rows.iter().map(|row| row.text.as_str()).collect()
    }

    #[test]
    fn rows_unfold_and_fold_again() {
        let mut list = tree();
        assert_eq!(list.rows[1].marker(), "+ ");
        list.handle_key(&key(b"\x1b[C"), 10);
        assert_eq!(texts(&list), ["a", "b", "b1", "b2", "c"]);
        assert_eq!(list.selected, 1);
        assert_eq!(list.rows[1].marker(), "- ");
        list.handle_key(&key(b"\x1b[D"), 10);
        assert_eq!(texts(&list), ["a", "b", "c"]);
        assert_eq!(list.selected, 1);
        assert_eq!(list.rows[1].marker(), "+ ");
    }

    #[test]
    fn letters_and_signs_fold_too() {
        for (open, close) in [(b"l", b"h"), (b"+", b"-")] {
            let mut list = tree();
            list.handle_key(&key(open), 10);
            assert_eq!(list.rows.len(), 5);
            list.handle_key(&key(close), 10);
            assert_eq!(list.rows.len(), 3);
        }
    }

    #[test]
    fn a_row_that_has_unfolded_goes_down_a_row() {
        let mut list = tree();
        list.handle_key(&key(b"l"), 10);
        list.handle_key(&key(b"l"), 10);
        assert_eq!(list.selected, 2);
        list.handle_key(&key(b"l"), 10);
        assert_eq!(list.selected, 3);
        assert_eq!(list.rows.len(), 5);
    }

    #[test]
    fn folding_goes_from_a_nested_row_to_the_row_above_it() {
        let mut list = tree();
        list.handle_key(&key(b"l"), 10);
        list.handle_key(&key(b"j"), 10);
        list.handle_key(&key(b"j"), 10);
        assert_eq!(list.selected, 3);
        list.handle_key(&key(b"h"), 10);
        assert_eq!(texts(&list), ["a", "b", "c"]);
        assert_eq!(list.selected, 1);
    }

    #[test]
    fn nothing_to_fold_goes_up_a_row() {
        let mut list = tree();
        list.handle_key(&key(b"h"), 10);
        assert_eq!(list.selected, 0);
        list.handle_key(&key(b"h"), 10);
        assert_eq!(list.selected, 0);
    }

    #[test]
    fn a_list_without_rows_has_nothing_to_fold() {
        let mut list = list(0);
        assert_eq!(list.handle_key(&key(b"l"), 5), ListOutcome::Stay);
        assert_eq!(list.handle_key(&key(b"h"), 5), ListOutcome::Stay);
    }

    #[test]
    fn rows_can_start_out_unfolded() {
        let row = |text: &str| ListRow::new(text.to_owned(), Command::LastWindow);
        let rows = vec![row("a").unfolded(), row("a1").nested()];
        let mut list = List::new(rows, 1);
        assert_eq!(list.rows[0].marker(), "- ");
        list.handle_key(&key(b"h"), 5);
        assert_eq!(texts(&list), ["a"]);
        list.handle_key(&key(b"l"), 5);
        assert_eq!(texts(&list), ["a", "a1"]);
    }

    #[test]
    fn pane_numbers_expire_unless_they_wait_for_a_key() {
        let mut ui = Ui::default();
        ui.show_pane_numbers(Some(Duration::ZERO));
        assert_eq!(ui.pane_numbers_deadline(), None);
        assert!(!ui.expire_pane_numbers());
        assert!(ui.pane_numbers.is_some());

        ui.show_pane_numbers(None);
        assert!(!ui.expire_pane_numbers());

        ui.pane_numbers = Some(PaneNumbers {
            until: Some(Instant::now()),
        });
        assert!(ui.expire_pane_numbers());
        assert!(ui.pane_numbers.is_none());
    }

    #[test]
    fn messages_expire() {
        let mut ui = Ui::default();
        assert_eq!(ui.message(), None);
        ui.show_message("hi".to_owned());
        assert_eq!(ui.message(), Some("hi"));
        assert!(!ui.expire_message());

        ui.message = Some(("hi".to_owned(), Instant::now()));
        assert!(ui.expire_message());
        assert_eq!(ui.message(), None);
    }
}
