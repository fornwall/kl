//! The command language, which follows tmux. The same commands are reached from
//! key bindings, the `:` prompt and the `kl` command line.

use std::path::PathBuf;

use std::time::Duration;

/// A direction to move focus or a pane border in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// How a new pane is placed next to the current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitKind {
    /// Side by side, as `split-window -h`.
    Horizontal,
    /// Stacked, as `split-window -v`.
    Vertical,
}

/// How `paste-buffer` goes about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PasteOptions {
    pub target: Target,
    /// The buffer to paste, if not the top one.
    pub name: Option<String>,
    /// Delete the buffer afterwards.
    pub delete: bool,
    /// Tell programs that ask for it that this is a paste.
    pub bracket: bool,
    /// Leave line breaks alone.
    pub raw: bool,
    /// What goes in place of line breaks.
    pub separator: Option<String>,
}

/// What `set-buffer` was asked to do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetBuffer {
    pub name: Option<String>,
    pub new_name: Option<String>,
    pub append: bool,
    /// Put the text on the clipboard too.
    pub clipboard: bool,
    pub data: Option<String>,
}

/// The commands that work on paste buffers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufferCommand {
    Choose,
    Delete(Option<String>),
    List,
    Load {
        name: Option<String>,
        path: PathBuf,
        clipboard: bool,
    },
    Paste(PasteOptions),
    Save {
        name: Option<String>,
        path: PathBuf,
        append: bool,
    },
    Set(SetBuffer),
    Show(Option<String>),
}

/// A size as given on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Cells(u16),
    /// A part of what there is to divide, out of a hundred.
    Percent(u16),
}

impl Size {
    fn parse(text: &str) -> Result<Self, String> {
        let number = |digits: &str| {
            digits
                .parse()
                .map_err(|error| format!("size {text}: {error}"))
        };
        text.strip_suffix('%').map_or_else(
            || number(text).map(Self::Cells),
            |percent| number(percent).map(Self::Percent),
        )
    }

    /// How many cells this comes to out of `whole` of them.
    pub fn cells(self, whole: u16) -> u16 {
        match self {
            Self::Cells(cells) => cells,
            Self::Percent(percent) => {
                let cells = u32::from(whole) * u32::from(percent) / 100;
                u16::try_from(cells).unwrap_or(whole)
            }
        }
    }
}

/// What `resize-pane` was asked to do, in this order. Asking for nothing is fine,
/// as a pane that gets resized comes out of a zoom either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resize {
    /// A size to give the pane, as `-x` and `-y` say.
    pub width: Option<Size>,
    pub height: Option<Size>,
    /// A way to move a border, as `-L`, `-R`, `-U` and `-D` say.
    pub direction: Option<Direction>,
    pub cells: u16,
}

/// Where a new pane goes, as the flags of `split-window` say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub kind: SplitKind,
    /// To the left or above rather than to the right or below.
    pub before: bool,
    /// Next to the whole window rather than next to a pane.
    pub full: bool,
    /// The size of the new pane, otherwise half of what it splits.
    pub size: Option<Size>,
}

/// A window or pane as `-t` and `-s` name it, for the server to find. Without
/// one, a command is about the current one.
pub type Target = Option<String>;

/// Which panes `list-panes` shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneScope {
    Window,
    /// `-s`, all panes in the session.
    Session,
    /// `-a`, the same here, but marked with the session as in tmux.
    All,
}

/// Which side of a window another one goes on, as `-a` and `-b` say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beside {
    After,
    Before,
}

/// What `find-window` looks at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Searched {
    /// What panes show.
    pub content: bool,
    /// The names of windows.
    pub name: bool,
    /// The titles of panes.
    pub title: bool,
}

/// What `find-window` looks for, and where.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindWindow {
    /// A shell pattern for some part of the text.
    pub pattern: String,
    pub ignore_case: bool,
    pub searched: Searched,
}

/// What `move-window` was asked to do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MoveWindow {
    pub source: Target,
    pub target: Target,
    pub next_to: Option<Beside>,
    /// Stay in the window at hand.
    pub detached: bool,
    /// Replace a window that has the index already.
    pub kill: bool,
    /// Number all windows in order, rather than move one of them.
    pub renumber: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Moves a pane to a window of its own.
    BreakPane {
        source: Target,
        target: Target,
        name: Option<String>,
        /// Stay in the window at hand.
        detached: bool,
        /// A format for telling about the pane.
        print: Option<String>,
    },
    Buffer(BufferCommand),
    /// Lets the user pick a window from a list.
    ChooseTree,
    /// Asks for a line, and runs it or fills it in to a template at `%%`.
    Prompt {
        prompt: Option<String>,
        initial: Option<String>,
        template: Option<String>,
    },
    ConfirmBefore {
        prompt: Option<String>,
        command: Box<Self>,
    },
    CopyMode {
        page_up: bool,
        target: Target,
    },
    DetachClient,
    DisplayMessage {
        target: Target,
        /// To the output rather than the status line.
        print: bool,
        format: Option<String>,
    },
    /// Shows the number of each pane for a while, or until a key if zero.
    DisplayPanes(Option<Duration>),
    /// Lets the user pick from the panes that show something.
    FindWindow(FindWindow),
    /// Shows a pane, whichever window it is in. This is what rows of the window
    /// list run, and there is no name for it: tmux has `switch-client` there.
    GoToPane {
        target: Target,
    },
    /// Moves a pane next to another one.
    JoinPane {
        source: Target,
        target: Target,
        place: Place,
        /// Leave the active pane and window as they are.
        detached: bool,
    },
    KillPane {
        target: Target,
        /// Everything in its window but this pane.
        others: bool,
    },
    KillServer,
    KillWindow {
        target: Target,
        /// Every window but this one.
        others: bool,
    },
    LastPane {
        target: Target,
    },
    LastWindow,
    /// `list-keys`, with what each key does in words for `-N`.
    ListKeys {
        notes: bool,
    },
    ListPanes {
        scope: PaneScope,
        target: Target,
        format: Option<String>,
        filter: Option<String>,
    },
    ListWindows {
        all: bool,
        format: Option<String>,
        filter: Option<String>,
    },
    /// Toggles the mark on a pane, or clears the mark from wherever it is.
    MarkPane {
        target: Target,
        clear: bool,
    },
    NewWindow {
        target: Target,
        /// Right after or before the target, moving others up to make room.
        next_to: Option<Beside>,
        name: Option<String>,
        directory: Option<String>,
        /// Stay in the window at hand.
        detached: bool,
        /// A format for telling about the new pane.
        print: Option<String>,
        program: Vec<String>,
    },
    /// Moves a window to another index, or all of them to close the gaps.
    MoveWindow(MoveWindow),
    /// Goes to the next or previous preset layout.
    NextLayout {
        target: Target,
        forward: bool,
    },
    NextWindow {
        /// The next one with a bell or activity.
        alert: bool,
    },
    PreviousWindow {
        alert: bool,
    },
    RenameWindow {
        target: Target,
        name: String,
    },
    ResizePane {
        target: Target,
        resize: Resize,
    },
    /// Moves every pane to the place of the next one, or the previous with `down`.
    RotateWindow {
        target: Target,
        down: bool,
    },
    /// Replaces the server with this binary, or the one it is running from.
    RestartServer(Option<PathBuf>),
    /// Applies a layout, or again the one applied last.
    SelectLayout {
        target: Target,
        name: Option<String>,
    },
    SelectPane {
        target: Target,
        /// Go from the target this way, rather than to the target itself.
        direction: Option<Direction>,
    },
    SelectWindow {
        target: Target,
    },
    SendKeys {
        target: Target,
        /// As they are, rather than looking for names of keys in them.
        literal: bool,
        keys: Vec<String>,
    },
    SendPrefix {
        target: Target,
    },
    /// Turns a window's `monitor-activity` option on or off, or toggles it.
    SetMonitorActivity {
        target: Target,
        value: Option<bool>,
    },
    SplitWindow {
        target: Target,
        place: Place,
        directory: Option<String>,
        /// Stay in the pane at hand.
        detached: bool,
        /// A format for telling about the new pane.
        print: Option<String>,
        program: Vec<String>,
    },
    /// Evens out the split a pane is in.
    SpreadLayout {
        target: Target,
    },
    /// Puts two panes in each other's place. With a direction, that is the
    /// target and the pane before or after it.
    SwapPane {
        source: Target,
        target: Target,
        direction: Option<Direction>,
        /// Leave the active pane where it is, rather than go with the target.
        detached: bool,
    },
    /// Puts two windows at each other's index.
    SwapWindow {
        source: Target,
        target: Target,
        /// Go along with the source window, rather than stay at the same index.
        detached: bool,
    },
    ToggleZoom {
        target: Target,
    },
}

/// Parses the words of a command, with tmux's names and aliases.
pub fn parse(words: &[String]) -> Result<Command, String> {
    let Some((name, args)) = words.split_first() else {
        return Err("no command".to_owned());
    };
    match name.as_str() {
        "break-pane" | "breakp" => parse_break_pane(args),
        "choose-buffer" => parse_choose_buffer(args),
        "choose-tree" => parse_choose_tree(args),
        "command-prompt" => parse_command_prompt(args),
        "confirm-before" | "confirm" => parse_confirm_before(args),
        "copy-mode" => parse_copy_mode(args),
        "detach-client" | "detach" => no_args("detach-client", args, Command::DetachClient),
        "display-message" | "display" => parse_display_message(args),
        "delete-buffer" | "deleteb" => {
            parse_buffer_name("delete-buffer", args, BufferCommand::Delete)
        }
        "display-panes" | "displayp" => parse_display_panes(args),
        "find-window" | "findw" => parse_find_window(args),
        "join-pane" | "joinp" | "move-pane" | "movep" => parse_join_pane(args),
        "kill-pane" | "killp" => parse_kill_pane(args),
        "kill-server" => no_args("kill-server", args, Command::KillServer),
        "kill-window" | "killw" => parse_kill_window(args),
        "last-pane" | "lastp" => parse_last_pane(args),
        "last-window" | "last" => no_args("last-window", args, Command::LastWindow),
        "list-buffers" | "lsb" => {
            no_args("list-buffers", args, Command::Buffer(BufferCommand::List))
        }
        "list-keys" | "lsk" => parse_list_keys(args),
        "list-panes" | "lsp" => parse_list_panes(args),
        "list-windows" | "lsw" => parse_list_windows(args),
        "load-buffer" | "loadb" => parse_load_buffer(args),
        "move-window" | "movew" => parse_move_window(args),
        "new-window" | "neww" => parse_new_window(args),
        "next-layout" | "nextl" => parse_step_layout("next-layout", args, true),
        "next-window" | "next" => parse_next_window(args),
        "paste-buffer" | "pasteb" => parse_paste_buffer(args),
        "previous-layout" | "prevl" => parse_step_layout("previous-layout", args, false),
        "previous-window" | "prev" => parse_previous_window(args),
        "rename-window" | "renamew" => parse_rename_window(args),
        "resize-pane" | "resizep" => parse_resize_pane(args),
        "restart-server" => parse_restart_server(args),
        "rotate-window" | "rotatew" => parse_rotate_window(args),
        "save-buffer" | "saveb" => parse_save_buffer(args),
        "select-layout" | "selectl" => parse_select_layout(args),
        "select-pane" | "selectp" => parse_select_pane(args),
        "select-window" | "selectw" => parse_select_window(args),
        "send-keys" | "send" => parse_send_keys(args),
        "send-prefix" => parse_send_prefix(args),
        "set-buffer" | "setb" => parse_set_buffer(args),
        "set-option" | "set" | "set-window-option" | "setw" => parse_set_option(args),
        "show-buffer" | "showb" => parse_buffer_name("show-buffer", args, BufferCommand::Show),
        "split-window" | "splitw" => parse_split_window(args),
        "swap-pane" | "swapp" => parse_swap_pane(args),
        "swap-window" | "swapw" => parse_swap_window(args),
        other => Err(format!("unknown command: {other}")),
    }
}

/// Parses a line of text as a command.
pub fn parse_line(line: &str) -> Result<Command, String> {
    parse(&split_words(line)?)
}

/// The flags of a command, as getopt finds them in front of its other arguments.
struct Flags<'a> {
    command: &'static str,
    found: Vec<(char, Option<&'a str>)>,
    rest: &'a [String],
}

impl<'a> Flags<'a> {
    /// Takes the flags in `spec`, in which a colon follows those with a value.
    fn parse(command: &'static str, args: &'a [String], spec: &str) -> Result<Self, String> {
        let mut found = Vec::new();
        let mut rest = args;
        while let Some((word, tail)) = rest.split_first() {
            let Some(letters) = word.strip_prefix('-').filter(|letters| !letters.is_empty()) else {
                break;
            };
            rest = tail;
            if letters == "-" {
                break;
            }
            let mut chars = letters.chars();
            while let Some(flag) = chars.next() {
                let mut spec = spec.chars().skip_while(|c| *c != flag || flag == ':');
                if spec.next().is_none() {
                    return Err(format!("command {command}: unknown flag -{flag}"));
                }
                if spec.next() != Some(':') {
                    found.push((flag, None));
                    continue;
                }
                let attached = chars.as_str();
                let value = if attached.is_empty() {
                    let (value, tail) = rest
                        .split_first()
                        .ok_or_else(|| format!("command {command}: -{flag} expects an argument"))?;
                    rest = tail;
                    value.as_str()
                } else {
                    attached
                };
                found.push((flag, Some(value)));
                break;
            }
        }
        Ok(Self {
            command,
            found,
            rest,
        })
    }

    fn has(&self, flag: char) -> bool {
        self.found.iter().any(|(found, _)| *found == flag)
    }

    /// The value of a flag, the last one if given more than once.
    fn value(&self, flag: char) -> Option<String> {
        let values = self.found.iter().rev();
        values
            .filter(|(found, _)| *found == flag)
            .find_map(|(_, value)| value.map(str::to_owned))
    }

    /// Fails if there are more than this many arguments after the flags.
    fn at_most(&self, count: usize) -> Result<(), String> {
        if self.rest.len() > count {
            let command = self.command;
            return Err(format!(
                "command {command}: too many arguments (need at most {count})"
            ));
        }
        Ok(())
    }

    /// The format for telling about something new: `-P`, with `-F` to replace
    /// the default.
    fn print_format(&self) -> Option<String> {
        self.has('P').then(|| {
            self.value('F')
                .unwrap_or_else(|| "#{session_name}:#{window_index}.#{pane_index}".to_owned())
        })
    }

    /// Where `-b`, `-f`, `-h`, `-l`, `-p` and `-v` put a pane.
    fn place(&self) -> Result<Place, String> {
        let percent = self.value('p').map(|percent| format!("{percent}%"));
        let size = self.value('l').or(percent);
        let kind = if self.has('h') {
            SplitKind::Horizontal
        } else {
            SplitKind::Vertical
        };
        Ok(Place {
            kind,
            before: self.has('b'),
            full: self.has('f'),
            size: size.as_deref().map(Size::parse).transpose()?,
        })
    }

    /// The side of `-a` or `-b`, of which `-b` wins as in tmux.
    fn beside(&self) -> Option<Beside> {
        if self.has('b') {
            Some(Beside::Before)
        } else {
            self.has('a').then_some(Beside::After)
        }
    }

    /// The direction of `-L`, `-R`, `-U` or `-D`.
    fn direction(&self) -> Option<Direction> {
        [
            ('L', Direction::Left),
            ('R', Direction::Right),
            ('U', Direction::Up),
            ('D', Direction::Down),
        ]
        .into_iter()
        .find_map(|(flag, direction)| self.has(flag).then_some(direction))
    }
}

fn no_args(name: &'static str, args: &[String], command: Command) -> Result<Command, String> {
    Flags::parse(name, args, "")?.at_most(0).map(|()| command)
}

fn parse_command_prompt(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("command-prompt", args, "I:p:")?;
    flags.at_most(1)?;
    Ok(Command::Prompt {
        prompt: flags.value('p'),
        initial: flags.value('I'),
        template: flags.rest.first().cloned(),
    })
}

fn parse_confirm_before(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("confirm-before", args, "p:")?;
    let command = match flags.rest {
        [] => return Err("usage: confirm-before [-p prompt] command".to_owned()),
        [line] => parse_line(line)?,
        words => parse(words)?,
    };
    Ok(Command::ConfirmBefore {
        prompt: flags.value('p'),
        command: Box::new(command),
    })
}

fn parse_choose_buffer(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("choose-buffer", args, "NrZ")?;
    flags.at_most(0)?;
    Ok(Command::Buffer(BufferCommand::Choose))
}

/// Commands that take the name of a buffer, and nothing else.
fn parse_buffer_name(
    name: &'static str,
    args: &[String],
    command: fn(Option<String>) -> BufferCommand,
) -> Result<Command, String> {
    let flags = Flags::parse(name, args, "b:")?;
    flags.at_most(0)?;
    Ok(Command::Buffer(command(flags.value('b'))))
}

fn parse_load_buffer(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("load-buffer", args, "b:w")?;
    let [path] = flags.rest else {
        return Err("usage: load-buffer [-w] [-b buffer-name] path".to_owned());
    };
    Ok(Command::Buffer(BufferCommand::Load {
        name: flags.value('b'),
        path: PathBuf::from(path),
        clipboard: flags.has('w'),
    }))
}

fn parse_paste_buffer(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("paste-buffer", args, "b:dprs:t:")?;
    flags.at_most(0)?;
    Ok(Command::Buffer(BufferCommand::Paste(PasteOptions {
        target: flags.value('t'),
        name: flags.value('b'),
        delete: flags.has('d'),
        bracket: flags.has('p'),
        raw: flags.has('r'),
        separator: flags.value('s'),
    })))
}

fn parse_save_buffer(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("save-buffer", args, "ab:")?;
    let [path] = flags.rest else {
        return Err("usage: save-buffer [-a] [-b buffer-name] path".to_owned());
    };
    Ok(Command::Buffer(BufferCommand::Save {
        name: flags.value('b'),
        path: PathBuf::from(path),
        append: flags.has('a'),
    }))
}

fn parse_set_buffer(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("set-buffer", args, "ab:n:w")?;
    if flags.rest.len() > 1 {
        return Err("usage: set-buffer [-aw] [-b buffer-name] [-n new-name] [data]".to_owned());
    }
    Ok(Command::Buffer(BufferCommand::Set(SetBuffer {
        name: flags.value('b'),
        new_name: flags.value('n'),
        append: flags.has('a'),
        clipboard: flags.has('w'),
        data: flags.rest.first().cloned(),
    })))
}

/// With a single session, `-s` and `-w` end up with the same list, and it
/// always covers the window as with `-Z`.
fn parse_choose_tree(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("choose-tree", args, "swZ")?;
    flags.at_most(0).map(|()| Command::ChooseTree)
}

/// There is no `-r` for lack of regular expressions, and lists always cover the
/// window as with `-Z`.
fn parse_find_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("find-window", args, "CiNTZ")?;
    flags.at_most(1)?;
    let [pattern] = flags.rest else {
        return Err("command find-window: too few arguments (need at least 1)".to_owned());
    };
    let everywhere = !(flags.has('C') || flags.has('N') || flags.has('T'));
    Ok(Command::FindWindow(FindWindow {
        pattern: pattern.clone(),
        ignore_case: flags.has('i'),
        searched: Searched {
            content: everywhere || flags.has('C'),
            name: everywhere || flags.has('N'),
            title: everywhere || flags.has('T'),
        },
    }))
}

fn parse_display_message(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("display-message", args, "F:pt:")?;
    flags.at_most(1)?;
    Ok(Command::DisplayMessage {
        target: flags.value('t'),
        print: flags.has('p'),
        format: flags.value('F').or_else(|| flags.rest.first().cloned()),
    })
}

fn parse_display_panes(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("display-panes", args, "d:")?;
    flags.at_most(0)?;
    let millis = flags.value('d').map(|text| {
        text.parse()
            .map(Duration::from_millis)
            .map_err(|error| format!("{text}: {error}"))
    });
    millis.transpose().map(Command::DisplayPanes)
}

fn parse_break_pane(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("break-pane", args, "dF:n:Ps:t:")?;
    flags.at_most(0)?;
    Ok(Command::BreakPane {
        source: flags.value('s'),
        target: flags.value('t'),
        name: flags.value('n'),
        detached: flags.has('d'),
        print: flags.print_format(),
    })
}

fn parse_join_pane(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("join-pane", args, "bdfhl:p:s:t:v")?;
    flags.at_most(0)?;
    Ok(Command::JoinPane {
        source: flags.value('s'),
        target: flags.value('t'),
        place: flags.place()?,
        detached: flags.has('d'),
    })
}

fn parse_swap_pane(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("swap-pane", args, "dDs:t:UZ")?;
    flags.at_most(0)?;
    Ok(Command::SwapPane {
        source: flags.value('s'),
        target: flags.value('t'),
        direction: flags.direction(),
        detached: flags.has('d'),
    })
}

fn parse_swap_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("swap-window", args, "ds:t:")?;
    flags.at_most(0)?;
    Ok(Command::SwapWindow {
        source: flags.value('s'),
        target: flags.value('t'),
        detached: flags.has('d'),
    })
}

fn parse_kill_pane(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("kill-pane", args, "at:")?;
    flags.at_most(0)?;
    Ok(Command::KillPane {
        target: flags.value('t'),
        others: flags.has('a'),
    })
}

fn parse_kill_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("kill-window", args, "at:")?;
    flags.at_most(0)?;
    Ok(Command::KillWindow {
        target: flags.value('t'),
        others: flags.has('a'),
    })
}

fn parse_last_pane(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("last-pane", args, "t:")?;
    flags.at_most(0)?;
    Ok(Command::LastPane {
        target: flags.value('t'),
    })
}

fn parse_list_keys(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("list-keys", args, "N")?;
    flags.at_most(0)?;
    Ok(Command::ListKeys {
        notes: flags.has('N'),
    })
}

fn parse_list_panes(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("list-panes", args, "asF:f:t:")?;
    flags.at_most(0)?;
    let scope = if flags.has('a') {
        PaneScope::All
    } else if flags.has('s') {
        PaneScope::Session
    } else {
        PaneScope::Window
    };
    Ok(Command::ListPanes {
        scope,
        target: flags.value('t'),
        format: flags.value('F'),
        filter: flags.value('f'),
    })
}

fn parse_list_windows(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("list-windows", args, "aF:f:")?;
    flags.at_most(0)?;
    Ok(Command::ListWindows {
        all: flags.has('a'),
        format: flags.value('F'),
        filter: flags.value('f'),
    })
}

fn parse_copy_mode(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("copy-mode", args, "t:u")?;
    flags.at_most(0)?;
    Ok(Command::CopyMode {
        page_up: flags.has('u'),
        target: flags.value('t'),
    })
}

fn parse_move_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("move-window", args, "abdkrs:t:")?;
    flags.at_most(0)?;
    Ok(Command::MoveWindow(MoveWindow {
        source: flags.value('s'),
        target: flags.value('t'),
        next_to: flags.beside(),
        detached: flags.has('d'),
        kill: flags.has('k'),
        renumber: flags.has('r'),
    }))
}

fn parse_new_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("new-window", args, "abc:dF:n:Pt:")?;
    Ok(Command::NewWindow {
        target: flags.value('t'),
        next_to: flags.beside(),
        name: flags.value('n'),
        directory: flags.value('c'),
        detached: flags.has('d'),
        print: flags.print_format(),
        program: flags.rest.to_vec(),
    })
}

fn parse_step_layout(
    name: &'static str,
    args: &[String],
    forward: bool,
) -> Result<Command, String> {
    let flags = Flags::parse(name, args, "t:")?;
    flags.at_most(0)?;
    Ok(Command::NextLayout {
        target: flags.value('t'),
        forward,
    })
}

fn parse_rotate_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("rotate-window", args, "Dt:UZ")?;
    flags.at_most(0)?;
    Ok(Command::RotateWindow {
        target: flags.value('t'),
        down: flags.has('D'),
    })
}

fn parse_select_layout(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("select-layout", args, "Enpt:")?;
    flags.at_most(1)?;
    let target = flags.value('t');
    Ok(if flags.has('E') {
        Command::SpreadLayout { target }
    } else if flags.has('n') || flags.has('p') {
        Command::NextLayout {
            target,
            forward: flags.has('n'),
        }
    } else {
        Command::SelectLayout {
            target,
            name: flags.rest.first().cloned(),
        }
    })
}

fn parse_next_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("next-window", args, "a")?;
    flags.at_most(0)?;
    Ok(Command::NextWindow {
        alert: flags.has('a'),
    })
}

fn parse_previous_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("previous-window", args, "a")?;
    flags.at_most(0)?;
    Ok(Command::PreviousWindow {
        alert: flags.has('a'),
    })
}

fn parse_rename_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("rename-window", args, "t:")?;
    if flags.rest.is_empty() {
        return Err("usage: rename-window [-t target] name".to_owned());
    }
    Ok(Command::RenameWindow {
        target: flags.value('t'),
        name: flags.rest.join(" "),
    })
}

/// The value of `-x` or `-y`, with tmux's words for what is wrong with it.
fn parse_pane_size(what: &str, text: Option<String>) -> Result<Option<Size>, String> {
    let parse = |text: String| {
        if text.starts_with('-') {
            return Err(format!("{what} too small"));
        }
        let size = Size::parse(&text).ok();
        size.ok_or_else(|| format!("{what} invalid"))
    };
    text.map(parse).transpose()
}

fn parse_resize_pane(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("resize-pane", args, "DLRUt:x:y:Z")?;
    flags.at_most(1)?;
    let target = flags.value('t');
    if flags.has('Z') {
        return Ok(Command::ToggleZoom { target });
    }
    let cells = flags.rest.first().map_or(Ok(1), |cells| {
        cells.parse().map_err(|error| format!("{cells}: {error}"))
    })?;
    Ok(Command::ResizePane {
        target,
        resize: Resize {
            width: parse_pane_size("width", flags.value('x'))?,
            height: parse_pane_size("height", flags.value('y'))?,
            direction: flags.direction(),
            cells,
        },
    })
}

fn parse_restart_server(args: &[String]) -> Result<Command, String> {
    match args {
        [] => Ok(Command::RestartServer(None)),
        [binary] => Ok(Command::RestartServer(Some(PathBuf::from(binary)))),
        _ => Err("usage: restart-server [binary]".to_owned()),
    }
}

fn parse_select_pane(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("select-pane", args, "DLlMmRt:UZ")?;
    flags.at_most(0)?;
    let target = flags.value('t');
    if flags.has('l') {
        return Ok(Command::LastPane { target });
    }
    if flags.has('m') || flags.has('M') {
        return Ok(Command::MarkPane {
            target,
            clear: flags.has('M'),
        });
    }
    Ok(Command::SelectPane {
        target,
        direction: flags.direction(),
    })
}

fn parse_select_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("select-window", args, "lnpt:")?;
    flags.at_most(0)?;
    Ok(if flags.has('l') {
        Command::LastWindow
    } else if flags.has('n') {
        Command::NextWindow { alert: false }
    } else if flags.has('p') {
        Command::PreviousWindow { alert: false }
    } else {
        Command::SelectWindow {
            target: flags.value('t'),
        }
    })
}

fn parse_send_keys(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("send-keys", args, "lt:")?;
    Ok(Command::SendKeys {
        target: flags.value('t'),
        literal: flags.has('l'),
        keys: flags.rest.to_vec(),
    })
}

fn parse_send_prefix(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("send-prefix", args, "t:")?;
    flags.at_most(0)?;
    Ok(Command::SendPrefix {
        target: flags.value('t'),
    })
}

/// The only option so far is the one tmux makes it impossible to do without.
fn parse_set_option(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("set-option", args, "t:w")?;
    let target = flags.value('t');
    let value = match flags.rest {
        [name, ..] if name != "monitor-activity" => return Err(format!("invalid option: {name}")),
        [] => return Err("usage: set-option [-w] [-t target] option [value]".to_owned()),
        [_] => None,
        [_, value] => match value.as_str() {
            "on" | "yes" | "1" => Some(true),
            "off" | "no" | "0" => Some(false),
            _ => return Err(format!("bad value: {value}")),
        },
        _ => return flags.at_most(2).map(|()| Command::DetachClient),
    };
    Ok(Command::SetMonitorActivity { target, value })
}

fn parse_split_window(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse("split-window", args, "bc:dF:fhl:p:Pt:vZ")?;
    Ok(Command::SplitWindow {
        target: flags.value('t'),
        place: flags.place()?,
        directory: flags.value('c'),
        detached: flags.has('d'),
        print: flags.print_format(),
        program: flags.rest.to_vec(),
    })
}

/// Splits a line typed at the prompt into words, with shell-like quoting.
pub fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some('"') | None, '\\') => {
                if let Some(next) = chars.next() {
                    word.get_or_insert_default().push(next);
                }
            }
            (None, '"' | '\'') => {
                word.get_or_insert_default();
                quote = Some(c);
            }
            (None, c) if c.is_whitespace() => words.extend(word.take()),
            (_, c) => word.get_or_insert_default().push(c),
        }
    }
    if quote.is_some() {
        return Err("unterminated quote".to_owned());
    }
    words.extend(word);
    Ok(words)
}

/// The bytes a `send-keys` argument stands for.
pub fn key_bytes(name: &str) -> Vec<u8> {
    let named: Option<&[u8]> = match name {
        "Enter" => Some(b"\r"),
        "Tab" => Some(b"\t"),
        "Space" => Some(b" "),
        "Escape" => Some(b"\x1b"),
        "BSpace" => Some(b"\x7f"),
        "Up" => Some(b"\x1b[A"),
        "Down" => Some(b"\x1b[B"),
        "Right" => Some(b"\x1b[C"),
        "Left" => Some(b"\x1b[D"),
        _ => None,
    };
    if let Some(bytes) = named {
        return bytes.to_vec();
    }
    if let Some(&[letter]) = name.strip_prefix("C-").map(str::as_bytes)
        && letter.is_ascii_alphabetic()
    {
        return vec![letter.to_ascii_lowercase() & 0x1f];
    }
    name.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        split_words(line).unwrap()
    }

    fn command(line: &str) -> Command {
        parse(&words(line)).unwrap()
    }

    /// Where a `split-window` line puts a pane, and what it runs there.
    fn split_of(line: &str) -> (Place, Vec<String>) {
        match command(line) {
            Command::SplitWindow { place, program, .. } => (place, program),
            other => {
                assert_eq!(format!("{other:?}"), "a split-window");
                (
                    Place {
                        kind: SplitKind::Vertical,
                        before: false,
                        full: false,
                        size: None,
                    },
                    vec![],
                )
            }
        }
    }

    #[test]
    fn parses_names_and_aliases_alike() {
        assert_eq!(command("next-window"), command("next"));
        assert_eq!(command("detach"), Command::DetachClient);
        assert_eq!(command("resizep -Z"), Command::ToggleZoom { target: None });
    }

    #[test]
    fn parses_split_window() {
        let (place, program) = split_of("split-window -h");
        assert_eq!(place.kind, SplitKind::Horizontal);
        assert_eq!(program, Vec::<String>::new());

        let (place, program) = split_of("splitw top -d 1");
        assert_eq!(place.kind, SplitKind::Vertical);
        assert_eq!(program, words("top -d 1"));
    }

    #[test]
    fn parses_where_a_pane_goes() {
        let line = "split-window -bfv -l 30% -t 1.2 -c /tmp -d -PF #D";
        assert_eq!(
            command(line),
            Command::SplitWindow {
                target: Some("1.2".to_owned()),
                place: Place {
                    kind: SplitKind::Vertical,
                    before: true,
                    full: true,
                    size: Some(Size::Percent(30)),
                },
                directory: Some("/tmp".to_owned()),
                detached: true,
                print: Some("#D".to_owned()),
                program: vec![],
            }
        );
        assert_eq!(split_of("splitw -l 10").0.size, Some(Size::Cells(10)));
        assert_eq!(split_of("splitw -p 25").0.size, Some(Size::Percent(25)));
        assert!(parse(&words("splitw -l big")).is_err());
    }

    #[test]
    fn works_out_sizes() {
        assert_eq!(Size::Cells(7).cells(100), 7);
        assert_eq!(Size::Percent(30).cells(10), 3);
        assert_eq!(Size::Percent(100).cells(80), 80);
        assert_eq!(Size::Percent(u16::MAX).cells(u16::MAX), u16::MAX);
    }

    #[test]
    fn parses_layouts() {
        assert_eq!(
            command("select-layout -t 1 tiled"),
            Command::SelectLayout {
                target: Some("1".to_owned()),
                name: Some("tiled".to_owned()),
            }
        );
        assert_eq!(
            command("selectl"),
            Command::SelectLayout {
                target: None,
                name: None,
            }
        );
        assert_eq!(
            command("select-layout -E"),
            Command::SpreadLayout { target: None }
        );
        assert_eq!(command("select-layout -n"), command("next-layout"));
        assert_eq!(command("selectl -p"), command("previous-layout"));
        assert_eq!(command("nextl"), command("next-layout"));
        assert_eq!(command("prevl"), command("previous-layout"));
        assert_eq!(
            command("rotate-window -D -t 2"),
            Command::RotateWindow {
                target: Some("2".to_owned()),
                down: true,
            }
        );
    }

    #[test]
    fn parses_moving_panes() {
        assert_eq!(
            command("swap-pane -d -U -t 1"),
            Command::SwapPane {
                source: None,
                target: Some("1".to_owned()),
                direction: Some(Direction::Up),
                detached: true,
            }
        );
        assert_eq!(
            command("swapp -s 0 -t 2"),
            Command::SwapPane {
                source: Some("0".to_owned()),
                target: Some("2".to_owned()),
                direction: None,
                detached: false,
            }
        );
        assert_eq!(
            command("break-pane -d -n logs -s 1.2 -t 7"),
            Command::BreakPane {
                source: Some("1.2".to_owned()),
                target: Some("7".to_owned()),
                name: Some("logs".to_owned()),
                detached: true,
                print: None,
            }
        );
        assert_eq!(
            command("join-pane -h -b -l 10 -s 1 -t 2"),
            Command::JoinPane {
                source: Some("1".to_owned()),
                target: Some("2".to_owned()),
                place: Place {
                    kind: SplitKind::Horizontal,
                    before: true,
                    full: false,
                    size: Some(Size::Cells(10)),
                },
                detached: false,
            }
        );
        assert_eq!(command("movep -s 1"), command("joinp -s 1"));
        assert_eq!(
            command("select-pane -m"),
            Command::MarkPane {
                target: None,
                clear: false,
            }
        );
        assert_eq!(
            command("selectp -M"),
            Command::MarkPane {
                target: None,
                clear: true,
            }
        );
    }

    #[test]
    fn parses_new_window() {
        assert_eq!(
            command("new-window -n logs tail -f x"),
            Command::NewWindow {
                target: None,
                next_to: None,
                name: Some("logs".to_owned()),
                directory: None,
                detached: false,
                print: None,
                program: words("tail -f x"),
            }
        );
        assert!(parse(&words("new-window -n")).is_err());
        assert!(matches!(
            command("neww -P"),
            Command::NewWindow { print: Some(format), .. } if format.starts_with("#{session_name}")
        ));
    }

    #[test]
    fn parses_moving_windows() {
        assert_eq!(
            command("move-window -d -s 1 -t 3"),
            Command::MoveWindow(MoveWindow {
                source: Some("1".to_owned()),
                target: Some("3".to_owned()),
                detached: true,
                ..MoveWindow::default()
            })
        );
        assert_eq!(
            command("movew -abkr"),
            Command::MoveWindow(MoveWindow {
                next_to: Some(Beside::Before),
                kill: true,
                renumber: true,
                ..MoveWindow::default()
            })
        );
        assert_eq!(
            command("swapw -d -s 1 -t 2"),
            Command::SwapWindow {
                source: Some("1".to_owned()),
                target: Some("2".to_owned()),
                detached: true,
            }
        );
        assert!(matches!(
            command("neww -a"),
            Command::NewWindow {
                next_to: Some(Beside::After),
                ..
            }
        ));
        assert!(matches!(
            command("neww -b"),
            Command::NewWindow {
                next_to: Some(Beside::Before),
                ..
            }
        ));
    }

    #[test]
    fn parses_finding_windows() {
        assert_eq!(
            command("find-window -Z logs"),
            Command::FindWindow(FindWindow {
                pattern: "logs".to_owned(),
                ignore_case: false,
                searched: Searched {
                    content: true,
                    name: true,
                    title: true,
                },
            })
        );
        assert_eq!(
            command("findw -iN logs"),
            Command::FindWindow(FindWindow {
                pattern: "logs".to_owned(),
                ignore_case: true,
                searched: Searched {
                    name: true,
                    ..Searched::default()
                },
            })
        );
        assert_eq!(
            parse(&words("find-window")).unwrap_err(),
            "command find-window: too few arguments (need at least 1)"
        );
        assert!(parse(&words("find-window a b")).is_err());
    }

    #[test]
    fn parses_resize_and_selection() {
        assert_eq!(
            command("resize-pane -L 5"),
            Command::ResizePane {
                target: None,
                resize: Resize {
                    width: None,
                    height: None,
                    direction: Some(Direction::Left),
                    cells: 5,
                },
            }
        );
        assert_eq!(
            command("resizep -x 30 -y 50% -t 1"),
            Command::ResizePane {
                target: Some("1".to_owned()),
                resize: Resize {
                    width: Some(Size::Cells(30)),
                    height: Some(Size::Percent(50)),
                    direction: None,
                    cells: 1,
                },
            }
        );
        assert_eq!(
            parse(&words("resize-pane -x abc")).unwrap_err(),
            "width invalid"
        );
        assert_eq!(
            parse(&words("resize-pane -y -5")).unwrap_err(),
            "height too small"
        );
        assert_eq!(
            command("select-pane -U"),
            Command::SelectPane {
                target: None,
                direction: Some(Direction::Up),
            }
        );
        assert_eq!(
            command("select-pane -t :.+"),
            Command::SelectPane {
                target: Some(":.+".to_owned()),
                direction: None,
            }
        );
        assert_eq!(
            command("select-window -t 3"),
            Command::SelectWindow {
                target: Some("3".to_owned()),
            }
        );
        assert_eq!(
            command("rename-window my logs"),
            Command::RenameWindow {
                target: None,
                name: "my logs".to_owned(),
            }
        );
    }

    #[test]
    fn parses_what_commands_print() {
        assert_eq!(
            command("display -p -t 1.2 #W"),
            Command::DisplayMessage {
                target: Some("1.2".to_owned()),
                print: true,
                format: Some("#W".to_owned()),
            }
        );
        assert_eq!(
            command("lsw -F #W"),
            Command::ListWindows {
                all: false,
                format: Some("#W".to_owned()),
                filter: None,
            }
        );
        assert_eq!(
            command("list-panes -s -f #{pane_active}"),
            Command::ListPanes {
                scope: PaneScope::Session,
                target: None,
                format: None,
                filter: Some("#{pane_active}".to_owned()),
            }
        );
    }

    #[test]
    fn parses_options() {
        assert_eq!(
            command("setw monitor-activity on"),
            Command::SetMonitorActivity {
                target: None,
                value: Some(true),
            }
        );
        assert_eq!(
            command("set -w -t 2 monitor-activity off"),
            Command::SetMonitorActivity {
                target: Some("2".to_owned()),
                value: Some(false),
            }
        );
        assert_eq!(
            command("set-window-option monitor-activity"),
            Command::SetMonitorActivity {
                target: None,
                value: None,
            }
        );
        assert_eq!(
            parse(&words("set status off")).unwrap_err(),
            "invalid option: status"
        );
        assert!(parse(&words("setw monitor-activity maybe")).is_err());
        assert!(parse(&words("setw monitor-activity on now")).is_err());
    }

    #[test]
    fn parses_display_panes() {
        assert_eq!(command("display-panes"), Command::DisplayPanes(None));
        assert_eq!(
            command("displayp -d 250"),
            Command::DisplayPanes(Some(Duration::from_millis(250)))
        );
        assert!(parse(&words("display-panes -d soon")).is_err());
    }

    #[test]
    fn parses_prompts() {
        assert_eq!(
            command("command-prompt"),
            Command::Prompt {
                prompt: None,
                initial: None,
                template: None,
            }
        );
        assert_eq!(
            command(r##"command-prompt -I "#W" -p name "rename-window '%%'""##),
            Command::Prompt {
                prompt: Some("name".to_owned()),
                initial: Some("#W".to_owned()),
                template: Some("rename-window '%%'".to_owned()),
            }
        );
        assert_eq!(
            command(r#"confirm-before -p "sure?" kill-pane"#),
            Command::ConfirmBefore {
                prompt: Some("sure?".to_owned()),
                command: Box::new(command("kill-pane")),
            }
        );
        assert_eq!(
            command(r#"confirm "resize-pane -Z""#),
            Command::ConfirmBefore {
                prompt: None,
                command: Box::new(Command::ToggleZoom { target: None }),
            }
        );
    }

    fn flags<'a>(args: &'a [String], spec: &str) -> Result<Flags<'a>, String> {
        Flags::parse("test", args, spec)
    }

    #[test]
    fn reads_flags_as_getopt_does() {
        let args = words("-ab -t one -ctwo -- -d rest");
        let parsed = flags(&args, "abt:c:d").unwrap();
        assert_eq!(parsed.value('t').as_deref(), Some("one"));
        assert_eq!(parsed.value('c').as_deref(), Some("two"));
        assert_eq!(parsed.rest, ["-d", "rest"]);
        assert!(parsed.has('a'));
        assert!(!parsed.has('d'));

        let args = words("-a word -b");
        assert_eq!(flags(&args, "ab").unwrap().rest, ["word", "-b"]);

        let args = words("- word");
        assert_eq!(flags(&args, "ab").unwrap().rest, ["-", "word"]);

        let args = words("-x");
        assert_eq!(
            flags(&args, "ab").err().unwrap(),
            "command test: unknown flag -x"
        );

        let args = words("-t");
        assert_eq!(
            flags(&args, "t:").err().unwrap(),
            "command test: -t expects an argument"
        );
    }

    #[test]
    fn explains_what_it_cannot_parse() {
        assert_eq!(
            parse(&words("frobnicate")).unwrap_err(),
            "unknown command: frobnicate"
        );
        assert!(parse(&words("resize-pane -X")).is_err());
        assert_eq!(
            parse(&words("kill-pane now")).unwrap_err(),
            "command kill-pane: too many arguments (need at most 0)"
        );
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn splits_words_with_quotes() {
        assert_eq!(words("a  b"), ["a", "b"]);
        assert_eq!(
            words(r#"say "hello there" 'a b'"#),
            ["say", "hello there", "a b"]
        );
        assert_eq!(words(r"a\ b"), ["a b"]);
        assert_eq!(words(r#"'' """#), ["", ""]);
        assert!(split_words("\"open").is_err());
    }

    #[test]
    fn converts_key_names() {
        assert_eq!(key_bytes("Enter"), b"\r");
        assert_eq!(key_bytes("C-c"), [3]);
        assert_eq!(key_bytes("C-C"), [3]);
        assert_eq!(key_bytes("hello"), b"hello");
        assert_eq!(key_bytes("C-"), b"C-");
    }
}
