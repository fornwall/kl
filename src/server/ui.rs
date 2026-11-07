//! State of the interface around the panes: the prompt and passing messages.

use std::time::{Duration, Instant};

use crate::command::Command;
use crate::ghostty::key;
use crate::input::KeyEvent;

/// How long a message stays on the status line.
const MESSAGE_TIME: Duration = Duration::from_secs(3);

/// What to do with the text of a finished prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum PromptAction {
    /// Run the text as a command.
    Command,
    RenameWindow,
    /// Run this command if the answer is `y`.
    Confirm(Command),
}

pub struct Prompt {
    pub label: &'static str,
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
}

impl Prompt {
    pub fn new(label: &'static str, initial: &str, action: PromptAction) -> Self {
        let text: Vec<char> = initial.chars().collect();
        Self {
            label,
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

    pub fn handle_key(&mut self, event: &KeyEvent) -> PromptOutcome {
        if let PromptAction::Confirm(_) = self.action {
            return if event.char() == Some('y') {
                PromptOutcome::Done(String::new())
            } else {
                PromptOutcome::Cancelled
            };
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

#[derive(Default)]
pub struct Ui {
    pub prompt: Option<Prompt>,
    message: Option<(String, Instant)>,
}

impl Ui {
    pub fn show_message(&mut self, text: String) {
        self.message = Some((text, Instant::now() + MESSAGE_TIME));
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_ref().map(|(text, _)| text.as_str())
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
        let mut prompt = Prompt::new(":", "", PromptAction::Command);
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
        let mut prompt = Prompt::new("name: ", "zsh", PromptAction::RenameWindow);
        typed(&mut prompt, "2");
        assert_eq!(prompt.text(), "zsh2");
    }

    #[test]
    fn clears_to_the_start_of_the_line() {
        let mut prompt = Prompt::new(":", "abcd", PromptAction::Command);
        prompt.handle_key(&key(b"\x1b[D"));
        prompt.handle_key(&key(&[0x15]));
        assert_eq!(prompt.text(), "d");
    }

    #[test]
    fn cancels() {
        let mut prompt = Prompt::new(":", "x", PromptAction::Command);
        assert_eq!(prompt.handle_key(&key(&[3])), PromptOutcome::Cancelled);
        assert_eq!(
            prompt.handle_key(&key(b"\x1b[27u")),
            PromptOutcome::Cancelled
        );
    }

    #[test]
    fn confirmation_takes_only_y() {
        let action = PromptAction::Confirm(Command::KillPane);
        let mut prompt = Prompt::new("kill? ", "", action);
        assert_eq!(prompt.handle_key(&key(b"n")), PromptOutcome::Cancelled);
        assert_eq!(
            prompt.handle_key(&key(b"y")),
            PromptOutcome::Done(String::new())
        );
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
