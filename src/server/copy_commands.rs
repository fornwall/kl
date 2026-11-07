//! The commands of copy mode by tmux's names, for `send-keys -X`. As in tmux,
//! what makes no sense as a command of copy mode does nothing, without a word.

use super::Server;
use super::copy_mode::{self, Action, After, CopyMode, Outcome, Scroll, Source};
use super::layout::PaneId;
use super::motion::{Jump, Motion, NO_SEPARATORS, SEPARATORS};
use crate::command::CopyModeCommand;

/// What a command comes down to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CopyCommand {
    Run(Action),
    Search {
        text: String,
        forward: bool,
    },
    Copy {
        source: Source,
        after: After,
        prefix: Option<String>,
    },
    Append(After),
}

/// The commands that take nothing, and do what the keys do.
fn plain_action(name: &str) -> Option<Action> {
    let go = Action::Move;
    Some(match name {
        "back-to-indentation" => go(Motion::BackToIndentation),
        "begin-selection" => Action::BeginSelection,
        "bottom-line" => Action::BottomLine,
        "cancel" => Action::Cancel,
        "clear-selection" => Action::ClearSelection,
        "cursor-down" => Action::CursorDown,
        "cursor-down-and-cancel" => Action::CursorDownAndCancel,
        "cursor-left" => go(Motion::Left),
        "cursor-right" => go(Motion::Right { all: false }),
        "cursor-up" => Action::CursorUp,
        "end-of-line" => go(Motion::EndOfLine { all: false }),
        "halfpage-down" => Action::Scroll(Scroll::HalfpageDown),
        "halfpage-down-and-cancel" => Action::ScrollAndCancel(Scroll::HalfpageDown),
        "halfpage-up" => Action::Scroll(Scroll::HalfpageUp),
        "history-bottom" => Action::HistoryBottom,
        "history-top" => Action::HistoryTop,
        "jump-again" => Action::JumpAgain,
        "jump-reverse" => Action::JumpReverse,
        "jump-to-mark" => Action::JumpToMark,
        "middle-line" => Action::MiddleLine,
        "next-matching-bracket" => Action::NextMatchingBracket,
        "next-paragraph" => Action::NextParagraph,
        "next-space" => go(Motion::NextWord(NO_SEPARATORS)),
        "next-space-end" => go(Motion::NextWordEnd(NO_SEPARATORS)),
        "next-word" => go(Motion::NextWord(SEPARATORS)),
        "next-word-end" => go(Motion::NextWordEnd(SEPARATORS)),
        "other-end" => Action::OtherEnd,
        "page-down" => Action::Scroll(Scroll::PageDown),
        "page-down-and-cancel" => Action::ScrollAndCancel(Scroll::PageDown),
        "page-up" => Action::Scroll(Scroll::PageUp),
        "previous-paragraph" => Action::PreviousParagraph,
        "previous-space" => go(Motion::PreviousWord(NO_SEPARATORS)),
        "previous-word" => go(Motion::PreviousWord(SEPARATORS)),
        "rectangle-off" => Action::Rectangle(false),
        "rectangle-on" => Action::Rectangle(true),
        "rectangle-toggle" => Action::RectangleToggle,
        "scroll-bottom" => Action::ScrollBottom,
        "scroll-down" => Action::Scroll(Scroll::Down),
        "scroll-down-and-cancel" => Action::ScrollAndCancel(Scroll::Down),
        "scroll-middle" => Action::ScrollMiddle,
        "scroll-top" => Action::ScrollTop,
        "scroll-up" => Action::Scroll(Scroll::Up),
        "search-again" => Action::SearchAgain,
        "search-reverse" => Action::SearchReverse,
        "select-line" => Action::SelectLine,
        "set-mark" => Action::SetMark,
        "start-of-line" => go(Motion::StartOfLine),
        "stop-selection" => Action::StopSelection,
        "top-line" => Action::TopLine,
        _ => return None,
    })
}

/// The commands that jump to a character.
fn jump_kind(name: &str) -> Option<Jump> {
    match name {
        "jump-backward" => Some(Jump::Backward),
        "jump-forward" => Some(Jump::Forward),
        "jump-to-backward" => Some(Jump::ToBackward),
        "jump-to-forward" => Some(Jump::ToForward),
        _ => None,
    }
}

/// The commands that copy text, and whether they take a command to pipe it to.
fn copy_kind(name: &str) -> Option<(Source, After, bool)> {
    Some(match name {
        "copy-end-of-line" => (Source::EndOfLine, After::Clear, false),
        "copy-end-of-line-and-cancel" => (Source::EndOfLine, After::Leave, false),
        "copy-line" => (Source::Line, After::Clear, false),
        "copy-line-and-cancel" => (Source::Line, After::Leave, false),
        "copy-pipe" => (Source::Selection, After::Clear, true),
        "copy-pipe-and-cancel" => (Source::Selection, After::Leave, true),
        "copy-pipe-end-of-line" => (Source::EndOfLine, After::Clear, true),
        "copy-pipe-end-of-line-and-cancel" => (Source::EndOfLine, After::Leave, true),
        "copy-pipe-line" => (Source::Line, After::Clear, true),
        "copy-pipe-line-and-cancel" => (Source::Line, After::Leave, true),
        "copy-pipe-no-clear" => (Source::Selection, After::Keep, true),
        "copy-selection" => (Source::Selection, After::Clear, false),
        "copy-selection-and-cancel" => (Source::Selection, After::Leave, false),
        "copy-selection-no-clear" => (Source::Selection, After::Keep, false),
        _ => return None,
    })
}

/// A command that copies, with its arguments. There is nothing to pipe to here,
/// which is worth saying: the text would go missing without a word.
fn parse_copy(
    command: &CopyModeCommand,
    (source, after, pipe): (Source, After, bool),
) -> Result<Option<CopyCommand>, String> {
    let (program, prefix) = match command.args.as_slice() {
        [] => (None, None),
        [program] if pipe => (Some(program), None),
        [program, prefix] if pipe => (Some(program), Some(prefix)),
        [prefix] => (None, Some(prefix)),
        _ => return Ok(None),
    };
    if program.is_some_and(|program| !program.is_empty()) {
        return Err(format!("{}: piping is not supported", command.name));
    }
    Ok(Some(CopyCommand::Copy {
        source,
        after,
        prefix: prefix.cloned(),
    }))
}

fn parse_search(text: &str, forward: bool) -> Option<CopyCommand> {
    (!text.is_empty()).then(|| CopyCommand::Search {
        text: text.to_owned(),
        forward,
    })
}

impl CopyCommand {
    fn parse(command: &CopyModeCommand) -> Result<Option<Self>, String> {
        let name = command.name.as_str();
        let (copy_flags, output) = (command.no_buffer || command.no_clipboard, command.output);
        if let Some(kind) = copy_kind(name).filter(|_| !output) {
            return parse_copy(command, kind);
        }
        if matches!(name, "pipe" | "pipe-and-cancel" | "pipe-no-clear") {
            return Err(format!("{name}: piping is not supported"));
        }
        if copy_flags {
            return Ok(None);
        }
        Ok(match (name, command.args.as_slice()) {
            ("next-prompt", []) if output => Some(Self::Run(Action::NextOutput)),
            ("previous-prompt", []) if output => Some(Self::Run(Action::PreviousOutput)),
            _ if output => None,
            ("next-prompt", []) => Some(Self::Run(Action::NextPrompt)),
            ("previous-prompt", []) => Some(Self::Run(Action::PreviousPrompt)),
            ("append-selection", []) => Some(Self::Append(After::Clear)),
            ("append-selection-and-cancel", []) => Some(Self::Append(After::Leave)),
            ("goto-line", [line]) => line
                .parse()
                .ok()
                .map(|line| Self::Run(Action::GotoLine(line))),
            ("search-backward" | "search-backward-text", [text]) => parse_search(text, false),
            ("search-forward" | "search-forward-text", [text]) => parse_search(text, true),
            (name, [target]) => jump_kind(name)
                .zip(target.chars().next())
                .map(|(kind, c)| Self::Run(Action::Jump(kind, c))),
            (name, []) => plain_action(name).map(Self::Run),
            _ => None,
        })
    }
}

impl Server {
    /// Runs an action of copy mode in a pane, and keeps what it copied.
    pub fn copy_mode_action_in(&mut self, id: PaneId, action: Action) {
        let Some(pane) = self.session.panes.get_mut(&id) else {
            return;
        };
        if pane.copy_mode.is_none() {
            return;
        }
        let outcome = copy_mode::run(pane, action);
        let copied = matches!(outcome, Outcome::Copy | Outcome::Append)
            .then(|| copy_mode::selected_text(pane))
            .flatten();
        if outcome != Outcome::Stay {
            CopyMode::exit(pane);
        }
        match copied {
            Some(text) if outcome == Outcome::Append => self.append_buffer(&text),
            Some(text) => self.store_buffer(text),
            None => {}
        }
    }

    /// Runs what `send-keys -X` asks for in a pane.
    pub fn run_copy_command(
        &mut self,
        id: PaneId,
        repeat: u32,
        command: Option<&CopyModeCommand>,
    ) -> Result<(), String> {
        let pane = self.session.panes.get(&id);
        if pane.is_none_or(|pane| pane.copy_mode.is_none()) {
            return Err("not in a mode".to_owned());
        }
        let Some(command) = command else {
            return Ok(());
        };
        let Some(parsed) = CopyCommand::parse(command)? else {
            return Ok(());
        };
        match parsed {
            CopyCommand::Run(action) => {
                self.set_copy_repeat(id, repeat);
                self.copy_mode_action_in(id, action);
            }
            CopyCommand::Search { text, forward } => {
                if let Some(pane) = self.session.panes.get_mut(&id) {
                    copy_mode::search(pane, &text, forward);
                }
                if repeat > 1 {
                    self.set_copy_repeat(id, repeat - 1);
                    self.copy_mode_action_in(id, Action::SearchAgain);
                }
            }
            CopyCommand::Copy {
                source,
                after,
                prefix,
            } => {
                let pane = self.session.panes.get_mut(&id);
                let text = pane.and_then(|pane| copy_mode::take_text(pane, source, repeat, after));
                if let Some(text) = text.filter(|text| !text.is_empty()) {
                    if !command.no_clipboard {
                        self.send_clipboard(text.as_bytes());
                    }
                    if !command.no_buffer {
                        self.buffers
                            .add_with_prefix(prefix.as_deref().unwrap_or("buffer"), text);
                    }
                }
            }
            CopyCommand::Append(after) => {
                let pane = self.session.panes.get_mut(&id);
                let lines = 1;
                let text = pane
                    .and_then(|pane| copy_mode::take_text(pane, Source::Selection, lines, after));
                if let Some(text) = text {
                    self.append_buffer(&text);
                }
            }
        }
        Ok(())
    }

    fn set_copy_repeat(&mut self, id: PaneId, repeat: u32) {
        let pane = self.session.panes.get_mut(&id);
        if let Some(mode) = pane.and_then(|pane| pane.copy_mode.as_mut()) {
            mode.set_repeat(repeat);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(name: &str, args: &[&str]) -> Option<CopyCommand> {
        let command = CopyModeCommand {
            name: name.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            ..CopyModeCommand::default()
        };
        CopyCommand::parse(&command).unwrap()
    }

    fn with_flags(name: &str, flags: impl Fn(&mut CopyModeCommand)) -> CopyModeCommand {
        let mut command = CopyModeCommand {
            name: name.to_owned(),
            ..CopyModeCommand::default()
        };
        flags(&mut command);
        command
    }

    #[test]
    fn knows_commands_by_tmuxs_names() {
        assert_eq!(
            parsed("cursor-up", &[]),
            Some(CopyCommand::Run(Action::CursorUp))
        );
        assert_eq!(
            parsed("goto-line", &["7"]),
            Some(CopyCommand::Run(Action::GotoLine(7)))
        );
        assert_eq!(
            parsed("jump-to-forward", &["x"]),
            Some(CopyCommand::Run(Action::Jump(Jump::ToForward, 'x')))
        );
        assert_eq!(
            parsed("search-backward-text", &["a b"]),
            Some(CopyCommand::Search {
                text: "a b".to_owned(),
                forward: false,
            })
        );
    }

    #[test]
    fn what_makes_no_sense_is_nothing() {
        assert_eq!(parsed("no-such-command", &[]), None);
        assert_eq!(parsed("cursor-up", &["extra"]), None);
        assert_eq!(parsed("goto-line", &[]), None);
        assert_eq!(parsed("goto-line", &["soon"]), None);
        assert_eq!(parsed("jump-forward", &[]), None);
        assert_eq!(parsed("search-forward", &[""]), None);
        assert_eq!(parsed("copy-selection", &["a", "b"]), None);
        let wrong_flag = with_flags("cursor-up", |command| command.no_buffer = true);
        assert_eq!(CopyCommand::parse(&wrong_flag), Ok(None));
        let wrong_flag = with_flags("cursor-up", |command| command.output = true);
        assert_eq!(CopyCommand::parse(&wrong_flag), Ok(None));
        let wrong_flag = with_flags("copy-line", |command| command.output = true);
        assert_eq!(CopyCommand::parse(&wrong_flag), Ok(None));
    }

    #[test]
    fn the_flag_for_output_goes_with_prompts() {
        let output = |name| with_flags(name, |command| command.output = true);
        assert_eq!(
            CopyCommand::parse(&output("next-prompt")),
            Ok(Some(CopyCommand::Run(Action::NextOutput)))
        );
        assert_eq!(
            CopyCommand::parse(&output("previous-prompt")),
            Ok(Some(CopyCommand::Run(Action::PreviousOutput)))
        );
    }

    #[test]
    fn a_copy_can_say_how_its_buffer_is_named() {
        assert_eq!(
            parsed("copy-selection", &["clip"]),
            Some(CopyCommand::Copy {
                source: Source::Selection,
                after: After::Clear,
                prefix: Some("clip".to_owned()),
            })
        );
        assert_eq!(
            parsed("copy-pipe-and-cancel", &["", "clip"]),
            Some(CopyCommand::Copy {
                source: Source::Selection,
                after: After::Leave,
                prefix: Some("clip".to_owned()),
            })
        );
    }

    #[test]
    fn piping_says_that_it_is_not_there() {
        let pipe = |name: &str| CopyModeCommand {
            name: name.to_owned(),
            args: vec!["xclip".to_owned()],
            ..CopyModeCommand::default()
        };
        assert_eq!(
            CopyCommand::parse(&pipe("copy-pipe")),
            Err("copy-pipe: piping is not supported".to_owned())
        );
        assert_eq!(
            CopyCommand::parse(&pipe("pipe-and-cancel")),
            Err("pipe-and-cancel: piping is not supported".to_owned())
        );
    }
}
