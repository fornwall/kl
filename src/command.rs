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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
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
    },
    DetachClient,
    DisplayMessage(String),
    /// Shows the number of each pane for a while, or until a key if zero.
    DisplayPanes(Option<Duration>),
    KillPane,
    KillServer,
    KillWindow,
    LastPane,
    LastWindow,
    /// `list-keys`, with what each key does in words for `-N`.
    ListKeys {
        notes: bool,
    },
    ListPanes,
    ListWindows,
    NewWindow {
        name: Option<String>,
        program: Vec<String>,
    },
    NextPane,
    NextWindow,
    PreviousWindow,
    RenameWindow(String),
    ResizePane {
        direction: Direction,
        cells: u16,
    },
    /// Replaces the server with this binary, or the one it is running from.
    RestartServer(Option<PathBuf>),
    SelectPane(Direction),
    SelectWindow(u32),
    SendKeys(Vec<String>),
    SendPrefix,
    /// Turns a window's `monitor-activity` option on or off, or toggles it.
    SetMonitorActivity(Option<bool>),
    SplitWindow {
        kind: SplitKind,
        program: Vec<String>,
    },
    ToggleZoom,
}

/// Parses the words of a command, with tmux's names and aliases.
pub fn parse(words: &[String]) -> Result<Command, String> {
    let Some((name, args)) = words.split_first() else {
        return Err("no command".to_owned());
    };
    match name.as_str() {
        "choose-buffer" => parse_choose_buffer(args),
        "choose-tree" => parse_choose_tree(args),
        "command-prompt" => parse_command_prompt(args),
        "confirm-before" | "confirm" => parse_confirm_before(args),
        "copy-mode" => parse_copy_mode(args),
        "detach-client" | "detach" => no_args(name, args, Command::DetachClient),
        "display-message" | "display" => Ok(Command::DisplayMessage(args.join(" "))),
        "delete-buffer" | "deleteb" => parse_buffer_name(args, BufferCommand::Delete),
        "display-panes" | "displayp" => parse_display_panes(args),
        "kill-pane" | "killp" => no_args(name, args, Command::KillPane),
        "kill-server" => no_args(name, args, Command::KillServer),
        "kill-window" | "killw" => no_args(name, args, Command::KillWindow),
        "last-pane" | "lastp" => no_args(name, args, Command::LastPane),
        "last-window" | "last" => no_args(name, args, Command::LastWindow),
        "list-buffers" | "lsb" => no_args(name, args, Command::Buffer(BufferCommand::List)),
        "list-keys" | "lsk" => parse_list_keys(args),
        "list-panes" | "lsp" => no_args(name, args, Command::ListPanes),
        "list-windows" | "lsw" => no_args(name, args, Command::ListWindows),
        "load-buffer" | "loadb" => parse_load_buffer(args),
        "new-window" | "neww" => parse_new_window(args),
        "next-window" | "next" => no_args(name, args, Command::NextWindow),
        "paste-buffer" | "pasteb" => parse_paste_buffer(args),
        "previous-window" | "prev" => no_args(name, args, Command::PreviousWindow),
        "rename-window" | "renamew" => parse_rename_window(args),
        "resize-pane" | "resizep" => parse_resize_pane(args),
        "restart-server" => parse_restart_server(args),
        "save-buffer" | "saveb" => parse_save_buffer(args),
        "select-pane" | "selectp" => parse_select_pane(args),
        "select-window" | "selectw" => parse_select_window(args),
        "send-keys" | "send" => Ok(Command::SendKeys(args.to_vec())),
        "send-prefix" => no_args(name, args, Command::SendPrefix),
        "set-buffer" | "setb" => parse_set_buffer(args),
        "set-option" | "set" | "set-window-option" | "setw" => parse_set_option(args),
        "show-buffer" | "showb" => parse_buffer_name(args, BufferCommand::Show),
        "split-window" | "splitw" => Ok(parse_split_window(args)),
        other => Err(format!("unknown command: {other}")),
    }
}

/// Parses a line of text as a command.
pub fn parse_line(line: &str) -> Result<Command, String> {
    parse(&split_words(line)?)
}

/// The flags of a command, as getopt finds them in front of its other arguments.
struct Flags<'a> {
    found: Vec<(char, Option<&'a str>)>,
    rest: &'a [String],
}

impl<'a> Flags<'a> {
    /// Takes the flags in `spec`, in which a colon follows those with a value.
    fn parse(args: &'a [String], spec: &str) -> Result<Self, String> {
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
                    return Err(format!("unknown flag -{flag}"));
                }
                if spec.next() != Some(':') {
                    found.push((flag, None));
                    continue;
                }
                let attached = chars.as_str();
                let value = if attached.is_empty() {
                    let (value, tail) = rest
                        .split_first()
                        .ok_or_else(|| format!("-{flag} expects an argument"))?;
                    rest = tail;
                    value.as_str()
                } else {
                    attached
                };
                found.push((flag, Some(value)));
                break;
            }
        }
        Ok(Self { found, rest })
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
}

fn no_args(name: &str, args: &[String], command: Command) -> Result<Command, String> {
    if args.is_empty() {
        Ok(command)
    } else {
        Err(format!("{name}: too many arguments"))
    }
}

fn parse_command_prompt(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse(args, "I:p:")?;
    let template = match flags.rest {
        [] => None,
        [template] => Some(template.clone()),
        _ => return Err("usage: command-prompt [-I initial] [-p prompt] [template]".to_owned()),
    };
    Ok(Command::Prompt {
        prompt: flags.value('p'),
        initial: flags.value('I'),
        template,
    })
}

fn parse_confirm_before(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse(args, "p:")?;
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

/// With a single session, `-s` and `-w` end up with the same list, and it
/// always covers the window as with `-Z`.
fn parse_choose_buffer(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse(args, "NrZ")?;
    if !flags.rest.is_empty() {
        return Err("usage: choose-buffer [-NrZ]".to_owned());
    }
    Ok(Command::Buffer(BufferCommand::Choose))
}

/// Commands that take the name of a buffer, and nothing else.
fn parse_buffer_name(
    args: &[String],
    command: fn(Option<String>) -> BufferCommand,
) -> Result<Command, String> {
    let flags = Flags::parse(args, "b:")?;
    if !flags.rest.is_empty() {
        return Err("usage: [-b buffer-name]".to_owned());
    }
    Ok(Command::Buffer(command(flags.value('b'))))
}

fn parse_load_buffer(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse(args, "b:w")?;
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
    let flags = Flags::parse(args, "b:dprs:")?;
    if !flags.rest.is_empty() {
        return Err("usage: paste-buffer [-dpr] [-s separator] [-b buffer-name]".to_owned());
    }
    Ok(Command::Buffer(BufferCommand::Paste(PasteOptions {
        name: flags.value('b'),
        delete: flags.has('d'),
        bracket: flags.has('p'),
        raw: flags.has('r'),
        separator: flags.value('s'),
    })))
}

fn parse_save_buffer(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse(args, "ab:")?;
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
    let flags = Flags::parse(args, "ab:n:w")?;
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

fn parse_choose_tree(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse(args, "swZ")?;
    if !flags.rest.is_empty() {
        return Err("usage: choose-tree [-swZ]".to_owned());
    }
    Ok(Command::ChooseTree)
}

fn parse_display_panes(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse(args, "d:")?;
    if !flags.rest.is_empty() {
        return Err("usage: display-panes [-d milliseconds]".to_owned());
    }
    let millis = flags.value('d').map(|text| {
        text.parse()
            .map(Duration::from_millis)
            .map_err(|error| format!("{text}: {error}"))
    });
    millis.transpose().map(Command::DisplayPanes)
}

fn parse_list_keys(args: &[String]) -> Result<Command, String> {
    let flags = Flags::parse(args, "N")?;
    if !flags.rest.is_empty() {
        return Err("usage: list-keys [-N]".to_owned());
    }
    Ok(Command::ListKeys {
        notes: flags.has('N'),
    })
}

fn parse_copy_mode(args: &[String]) -> Result<Command, String> {
    match args {
        [] => Ok(Command::CopyMode { page_up: false }),
        [flag] if flag == "-u" => Ok(Command::CopyMode { page_up: true }),
        _ => Err("usage: copy-mode [-u]".to_owned()),
    }
}

fn parse_new_window(args: &[String]) -> Result<Command, String> {
    match args {
        [flag, name, program @ ..] if flag == "-n" => Ok(Command::NewWindow {
            name: Some(name.clone()),
            program: program.to_vec(),
        }),
        [flag] if flag == "-n" => Err("usage: new-window [-n name] [command]".to_owned()),
        program => Ok(Command::NewWindow {
            name: None,
            program: program.to_vec(),
        }),
    }
}

fn parse_rename_window(args: &[String]) -> Result<Command, String> {
    if args.is_empty() {
        return Err("usage: rename-window name".to_owned());
    }
    Ok(Command::RenameWindow(args.join(" ")))
}

fn parse_direction(flag: &str) -> Option<Direction> {
    match flag {
        "-L" => Some(Direction::Left),
        "-R" => Some(Direction::Right),
        "-U" => Some(Direction::Up),
        "-D" => Some(Direction::Down),
        _ => None,
    }
}

fn parse_resize_pane(args: &[String]) -> Result<Command, String> {
    let usage = "usage: resize-pane -Z | -L|-R|-U|-D [cells]";
    match args {
        [flag] if flag == "-Z" => Ok(Command::ToggleZoom),
        [flag] => parse_direction(flag)
            .map(|direction| Command::ResizePane {
                direction,
                cells: 1,
            })
            .ok_or_else(|| usage.to_owned()),
        [flag, cells] => {
            let direction = parse_direction(flag).ok_or_else(|| usage.to_owned())?;
            let cells = cells.parse().map_err(|error| format!("{cells}: {error}"))?;
            Ok(Command::ResizePane { direction, cells })
        }
        _ => Err(usage.to_owned()),
    }
}

fn parse_restart_server(args: &[String]) -> Result<Command, String> {
    match args {
        [] => Ok(Command::RestartServer(None)),
        [binary] => Ok(Command::RestartServer(Some(PathBuf::from(binary)))),
        _ => Err("usage: restart-server [binary]".to_owned()),
    }
}

fn parse_select_pane(args: &[String]) -> Result<Command, String> {
    match args {
        [flag] if flag == "-l" => Ok(Command::LastPane),
        [flag] => parse_direction(flag)
            .map(Command::SelectPane)
            .ok_or_else(|| "usage: select-pane -L|-R|-U|-D|-l".to_owned()),
        [] => Ok(Command::NextPane),
        _ => Err("usage: select-pane -L|-R|-U|-D|-l".to_owned()),
    }
}

fn parse_select_window(args: &[String]) -> Result<Command, String> {
    match args {
        [flag, index] if flag == "-t" => index
            .parse()
            .map(Command::SelectWindow)
            .map_err(|error| format!("{index}: {error}")),
        [flag] if flag == "-l" => Ok(Command::LastWindow),
        [flag] if flag == "-n" => Ok(Command::NextWindow),
        [flag] if flag == "-p" => Ok(Command::PreviousWindow),
        _ => Err("usage: select-window -t index | -l | -n | -p".to_owned()),
    }
}

/// The only option so far is the one tmux makes it impossible to do without.
fn parse_set_option(args: &[String]) -> Result<Command, String> {
    let args = match args {
        [flag, rest @ ..] if flag == "-w" => rest,
        args => args,
    };
    match args {
        [name] if name == "monitor-activity" => Ok(Command::SetMonitorActivity(None)),
        [name, value] if name == "monitor-activity" => match value.as_str() {
            "on" | "yes" | "1" => Ok(Command::SetMonitorActivity(Some(true))),
            "off" | "no" | "0" => Ok(Command::SetMonitorActivity(Some(false))),
            _ => Err(format!("bad value: {value}")),
        },
        [name, ..] => Err(format!("invalid option: {name}")),
        [] => Err("usage: set-option [-w] option [value]".to_owned()),
    }
}

fn parse_split_window(args: &[String]) -> Command {
    let (kind, program) = match args {
        [flag, program @ ..] if flag == "-h" => (SplitKind::Horizontal, program),
        [flag, program @ ..] if flag == "-v" => (SplitKind::Vertical, program),
        program => (SplitKind::Vertical, program),
    };
    Command::SplitWindow {
        kind,
        program: program.to_vec(),
    }
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

    #[test]
    fn parses_names_and_aliases_alike() {
        assert_eq!(command("next-window"), Command::NextWindow);
        assert_eq!(command("next"), Command::NextWindow);
        assert_eq!(command("detach"), Command::DetachClient);
        assert_eq!(command("resizep -Z"), Command::ToggleZoom);
    }

    #[test]
    fn parses_split_window() {
        assert_eq!(
            command("split-window -h"),
            Command::SplitWindow {
                kind: SplitKind::Horizontal,
                program: vec![],
            }
        );
        assert_eq!(
            command("splitw top -d 1"),
            Command::SplitWindow {
                kind: SplitKind::Vertical,
                program: words("top -d 1"),
            }
        );
    }

    #[test]
    fn parses_new_window() {
        assert_eq!(
            command("new-window -n logs tail -f x"),
            Command::NewWindow {
                name: Some("logs".to_owned()),
                program: words("tail -f x"),
            }
        );
        assert!(parse(&words("new-window -n")).is_err());
    }

    #[test]
    fn parses_resize_and_selection() {
        assert_eq!(
            command("resize-pane -L 5"),
            Command::ResizePane {
                direction: Direction::Left,
                cells: 5,
            }
        );
        assert_eq!(
            command("select-pane -U"),
            Command::SelectPane(Direction::Up)
        );
        assert_eq!(command("select-window -t 3"), Command::SelectWindow(3));
        assert_eq!(
            command("rename-window my logs"),
            Command::RenameWindow("my logs".to_owned())
        );
    }

    #[test]
    fn parses_options() {
        assert_eq!(
            command("setw monitor-activity on"),
            Command::SetMonitorActivity(Some(true))
        );
        assert_eq!(
            command("set -w monitor-activity off"),
            Command::SetMonitorActivity(Some(false))
        );
        assert_eq!(
            command("set-window-option monitor-activity"),
            Command::SetMonitorActivity(None)
        );
        assert_eq!(
            parse(&words("set status off")).unwrap_err(),
            "invalid option: status"
        );
        assert!(parse(&words("setw monitor-activity maybe")).is_err());
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
                command: Box::new(Command::KillPane),
            }
        );
        assert_eq!(
            command(r#"confirm "resize-pane -Z""#),
            Command::ConfirmBefore {
                prompt: None,
                command: Box::new(Command::ToggleZoom),
            }
        );
    }

    fn flags<'a>(args: &'a [String], spec: &str) -> Result<Flags<'a>, String> {
        Flags::parse(args, spec)
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
        assert_eq!(flags(&args, "ab").err().unwrap(), "unknown flag -x");

        let args = words("-t");
        assert_eq!(flags(&args, "t:").err().unwrap(), "-t expects an argument");
    }

    #[test]
    fn explains_what_it_cannot_parse() {
        assert_eq!(
            parse(&words("frobnicate")).unwrap_err(),
            "unknown command: frobnicate"
        );
        assert!(parse(&words("resize-pane -X")).is_err());
        assert!(parse(&words("select-window -t nope")).is_err());
        assert!(parse(&words("kill-pane now")).is_err());
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
