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
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "\
usage: kl [command [flags]]

With no command, attaches to the session, starting it if needed.

Session:
  attach-session (attach, new-session, new)   attach to the session
  detach-client (detach)                      detach the attached client
  kill-server                                 end the session

Windows:
  new-window (neww) [-n name] [command]       open a window
  select-window (selectw) -t index | -l|-n|-p  switch windows
  next-window (next), previous-window (prev), last-window (last)
  rename-window (renamew) name                name the window
  kill-window (killw)                         close the window
  list-windows (lsw)                          list windows

Panes:
  split-window (splitw) [-h|-v] [command]     split the active pane
  select-pane (selectp) -L|-R|-U|-D|-l        move to another pane
  last-pane (lastp)                           go back to the previous pane
  resize-pane (resizep) -L|-R|-U|-D [cells]   move a border
  resize-pane (resizep) -Z                    toggle zoom
  kill-pane (killp)                           close the active pane
  list-panes (lsp)                            list panes in the window

Other:
  send-keys (send) key...                     type into the active pane
  copy-mode [-u], paste-buffer (pasteb), display-message (display) text

Keys follow tmux, with C-b as the prefix: c n p l 0-9 , & for windows,
% \" o ; z x arrows for panes, [ ] for copying, : for commands and d to detach.
The mouse selects panes and windows, drags borders, scrolls and copies text.";

fn attach() -> Result<ExitCode, Box<dyn Error>> {
    let message = client::attach(&client::socket_path()?)?;
    println!("{message}");
    Ok(ExitCode::SUCCESS)
}

fn run_command(args: &[String]) -> Result<ExitCode, Box<dyn Error>> {
    let (success, text) = client::run_command(&client::socket_path()?, args)?;
    if success {
        print!("{text}");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("kl: {text}");
        Ok(ExitCode::FAILURE)
    }
}

fn run(args: &[String]) -> Result<ExitCode, Box<dyn Error>> {
    match args.first().map(String::as_str) {
        None | Some("attach-session" | "attach" | "a" | "new-session" | "new") => attach(),
        Some("--server") => {
            let socket = args.get(1).ok_or("--server needs a socket path")?;
            server::run(Path::new(socket))?;
            Ok(ExitCode::SUCCESS)
        }
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Some("-V" | "--version") => {
            println!("kl {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        Some(_) => run_command(args),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    run(&args).unwrap_or_else(|error| {
        eprintln!("kl: {error}");
        ExitCode::FAILURE
    })
}
