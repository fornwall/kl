//! Klethron Terminal Multiplexer.

mod base64;
mod client;
mod command;
mod ghostty;
mod input;
mod proto;
mod server;
mod sys;

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "\
usage: kl [command [flags]]

With no command, attaches to the session, starting it if needed.

Session:
  attach-session (attach, new-session, new)   attach to the session
  detach-client (detach)                      detach the attached client
  kill-server                                 end the session
  restart-server [binary]                     replace the server, for example
                                              after an upgrade, with programs
                                              still running

Windows:
  new-window (neww) [-n name] [command]       open a window
  select-window (selectw) -t index | -l|-n|-p  switch windows
  next-window (next), previous-window (prev), last-window (last)
  rename-window (renamew) name                name the window
  kill-window (killw)                         close the window
  list-windows (lsw)                          list windows
  choose-tree                                 pick a window from a list
  set-window-option (setw) monitor-activity [on|off]  flag output in the window

Panes:
  split-window (splitw) [-h|-v] [command]     split the active pane
  select-pane (selectp) -L|-R|-U|-D|-l        move to another pane
  last-pane (lastp)                           go back to the previous pane
  resize-pane (resizep) -L|-R|-U|-D [cells]   move a border
  resize-pane (resizep) -Z                    toggle zoom
  kill-pane (killp)                           close the active pane
  list-panes (lsp)                            list panes in the window
  display-panes (displayp) [-d ms]            show the number of each pane

Other:
  list-keys (lsk) [-N]                        list the keys after the prefix
  send-keys (send) key...                     type into the active pane
  send-prefix                                 type the prefix key into the pane
  command-prompt [-I text] [-p prompt] [template]  ask for a command to run
  confirm-before [-p prompt] command          run a command after asking
  copy-mode [-u], display-message (display) text

Paste buffers, the top one unless -b names another:
  paste-buffer (pasteb) [-dpr] [-s sep] [-b name]  paste into the active pane
  set-buffer (setb) [-aw] [-b name] [-n new-name] [data]  set or rename
  show-buffer (showb) [-b name], delete-buffer (deleteb) [-b name]
  save-buffer (saveb) [-a] [-b name] path, load-buffer (loadb) [-w] [-b name] path
  list-buffers (lsb), choose-buffer           list them, or pick one to paste

Keys follow tmux, with C-b as the prefix: c n p l w 0-9 , & for windows,
% \" o ; q z x arrows for panes, [ ] # = - for copying and paste buffers, : for
commands, ? to list the keys and d to detach.
Arrows with C- or M- resize a pane. Arrows repeat without the prefix for half a
second, so that C-b C-Up C-Up resizes twice.
Copy mode has the keys of tmux's copy-mode-vi table, with y to copy as well, and
[ ] to jump between shell prompts, for shells that mark them with OSC 133.
The mouse selects panes and windows, drags borders, scrolls and copies text.
";

/// Unlike `print!`, this does not panic when nobody is reading any more.
fn print(text: &str) {
    drop(io::stdout().write_all(text.as_bytes()));
}

fn attach() -> Result<ExitCode, Box<dyn Error>> {
    let message = client::attach(&client::socket_path()?)?;
    print(&format!("{message}\n"));
    Ok(ExitCode::SUCCESS)
}

fn run_command(args: &[String]) -> Result<ExitCode, Box<dyn Error>> {
    let (success, text) = client::run_command(&client::socket_path()?, args)?;
    if success {
        print(&text);
        Ok(ExitCode::SUCCESS)
    } else {
        sys::log(format_args!("kl: {text}"));
        Ok(ExitCode::FAILURE)
    }
}

fn run(args: &[OsString]) -> Result<ExitCode, Box<dyn Error>> {
    // `env::args` would panic on an argument that is not text. Commands work
    // with text, but the socket path of the server has to stay what it is.
    let words: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    match words.first().map(String::as_str) {
        None | Some("attach-session" | "attach" | "a" | "new-session" | "new") => attach(),
        Some("--server") => {
            let socket = args.get(1).ok_or("--server needs a socket path")?;
            let state = match args.get(2..).unwrap_or(&[]) {
                [] => None,
                [flag, state] if flag == "--restore" => Some(Path::new(state)),
                _ => return Err("--server takes a socket path and --restore".into()),
            };
            server::run(Path::new(socket), state)?;
            Ok(ExitCode::SUCCESS)
        }
        Some("--state-version") => {
            print(&format!("{}\n", server::STATE_VERSION));
            Ok(ExitCode::SUCCESS)
        }
        // The running server may be an older binary, or one that is gone, and
        // what the user just ran is what it should turn into.
        Some("restart-server") if args.len() == 1 => {
            let binary = env::current_exe()?.display().to_string();
            run_command(&["restart-server".to_owned(), binary])
        }
        Some("-h" | "--help") => {
            print(USAGE);
            Ok(ExitCode::SUCCESS)
        }
        Some("-V" | "--version") => {
            print(concat!("kl ", env!("CARGO_PKG_VERSION"), "\n"));
            Ok(ExitCode::SUCCESS)
        }
        Some(_) => run_command(&words),
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    run(&args).unwrap_or_else(|error| {
        sys::log(format_args!("kl: {error}"));
        ExitCode::FAILURE
    })
}
