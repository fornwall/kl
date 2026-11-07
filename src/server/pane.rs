//! A pane: one program running on a pty, and the terminal emulating it.

use std::env;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::copy_mode::CopyMode;
use super::layout::{PaneId, Rect};
use crate::ghostty::{Notification, OuterColors, Terminal};
use crate::sys::{self, WinSize};

/// How much to read from a pty at a time.
const READ_SIZE: usize = 64 * 1024;

/// How far a program can get behind on what is for it before replies are dropped.
/// Any more than the largest paste, which is not dropped, as a program that takes
/// its time with one must still hear back. One that never reads would otherwise
/// use up all memory, which ends the server and every pane with it.
const MAX_PENDING: usize = 96 * 1024 * 1024;

/// Ghostty's terminfo entry, compiled by `tic`. Programs in panes talk to
/// libghostty-vt whatever the outer terminal is, so this is the accurate entry
/// for them, but few hosts have it, least of all those reached over ssh.
const TERMINFO_ENTRY: &[u8] = include_bytes!("../../terminfo/xterm-ghostty");

/// What to run in a new pane, and where.
pub struct Spawn<'a> {
    /// Empty for the user's shell. A single word is a shell command, and more
    /// words are a program with arguments, as in tmux.
    pub program: &'a [String],
    pub cwd: Option<&'a Path>,
    /// The socket path, for programs to find their way back to the server.
    pub socket: &'a str,
    /// See [`crate::proto::ClientMessage::Attach`].
    pub environment: &'a [String],
}

/// Things a program did in the last read, for the caller to act on.
#[derive(Default)]
pub struct Activity {
    /// How much the program wrote.
    pub output: usize,
    pub bell: bool,
    pub title_changed: bool,
    pub clipboard: Vec<Vec<u8>>,
    pub notifications: Vec<Notification>,
    pub progress_reported: bool,
}

pub struct Pane {
    pub id: PaneId,
    pub terminal: Terminal,
    /// The area of the window this pane covers.
    pub rect: Rect,
    pub copy_mode: Option<CopyMode>,
    /// What was last searched for, which copy mode goes on with later.
    pub search: Option<String>,
    /// Set when every row must be drawn, not just the changed ones.
    pub redraw: bool,
    /// The program, which the server reaps along with any other child.
    pub pid: u32,
    /// The line this pane was started with, if a shell had to run it.
    pub start_command: Option<String>,
    /// The program ended or closed the pty. A background job can keep the pty
    /// open past the end of the program, so the end of output alone does not tell.
    pub exited: bool,
    pty: File,
    /// Bytes on their way to the program.
    pending: Vec<u8>,
    cell_size: (u16, u16),
}

fn user_shell() -> String {
    env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_owned())
}

/// What a window should be called before there is a foreground program to name
/// it after, by the same rules as tmux.
pub fn default_name(program: &[String]) -> String {
    let line = program.join(" ");
    name_from_line(if line.is_empty() { user_shell() } else { line }.as_str())
}

/// The name of the program a command line starts with.
fn name_from_line(line: &str) -> String {
    let line = line.trim_start_matches('"');
    let line = line.split('"').next().unwrap_or(line);
    let line = line.strip_prefix("exec ").unwrap_or(line);
    let word = line.trim_start_matches([' ', '-']);
    let word = word.split(' ').next().unwrap_or(word);
    let name = word.rsplit('/').next().unwrap_or(word);
    name.to_owned()
}

/// Starts the shell as a login shell, with the customary dash in `argv[0]`.
fn shell_command() -> Command {
    let shell = user_shell();
    let name = Path::new(&shell)
        .file_name()
        .map_or_else(|| "sh".into(), |name| name.to_string_lossy());
    let mut command = Command::new(&shell);
    command.arg0(format!("-{name}"));
    command
}

/// Puts the terminfo entry in a directory of its own next to the socket, and
/// returns the directory. Done for each pane, as something may clean up `/tmp`.
fn install_terminfo(socket: &Path) -> io::Result<PathBuf> {
    let root = socket.with_file_name("terminfo");
    // Linux files entries by their first letter, and macOS by its hex code.
    for directory in ["x", "78"] {
        let directory = root.join(directory);
        let entry = directory.join(crate::ghostty::TERM);
        if !entry.exists() {
            fs::create_dir_all(&directory)?;
            fs::write(entry, TERMINFO_ENTRY)?;
        }
    }
    Ok(root)
}

fn build_command(spawn: &Spawn<'_>, id: PaneId) -> Command {
    let mut command = match spawn.program {
        [] => shell_command(),
        [line] => {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", line]);
            command
        }
        [program, args @ ..] => {
            let mut command = Command::new(program);
            command.args(args);
            command
        }
    };
    if let Some(cwd) = spawn.cwd {
        command.current_dir(cwd);
    }
    match install_terminfo(Path::new(spawn.socket)) {
        Ok(terminfo) => command
            .env("TERM", crate::ghostty::TERM)
            .env("TERMINFO", terminfo),
        Err(error) => {
            sys::log(format_args!("installing the terminfo entry: {error}"));
            command.env("TERM", "xterm-256color")
        }
    };
    command
        .env("COLORTERM", "truecolor")
        .env("TERM_PROGRAM", "kl")
        .env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"))
        .env("KL", format!("{},{}", spawn.socket, std::process::id()))
        .env("KL_PANE", format!("%{id}"))
        .env_remove("LINES")
        .env_remove("COLUMNS");
    for entry in spawn.environment {
        match entry.split_once('=') {
            Some((name, value)) => command.env(name, value),
            None => command.env_remove(entry),
        };
    }
    command
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        spawn: &Spawn<'_>,
        size: WinSize,
        colors: &OuterColors,
    ) -> io::Result<Self> {
        let cols = size.cols.max(1);
        let rows = size.rows.max(1);
        let mut terminal = Terminal::new(cols, rows).map_err(io::Error::other)?;
        terminal.set_outer_colors(colors);
        let (pty, child) = sys::spawn_on_pty(build_command(spawn, id), size)?;
        if !sys::selectable(pty.as_raw_fd()) {
            return Err(io::Error::other("too many panes"));
        }
        let start_command = match spawn.program {
            [line] => Some(line.clone()),
            _ => None,
        };
        Ok(Self::adopt(id, child.id(), start_command, pty, terminal))
    }

    /// A pane around a program that is already running on `pty`.
    pub fn adopt(
        id: PaneId,
        pid: u32,
        start_command: Option<String>,
        pty: File,
        terminal: Terminal,
    ) -> Self {
        Self {
            id,
            terminal,
            rect: Rect::default(),
            copy_mode: None,
            search: None,
            redraw: true,
            pid,
            start_command,
            exited: false,
            pty,
            pending: Vec::new(),
            cell_size: (0, 0),
        }
    }

    pub fn fd(&self) -> RawFd {
        self.pty.as_raw_fd()
    }

    pub fn pty(&self) -> BorrowedFd<'_> {
        self.pty.as_fd()
    }

    pub const fn wants_write(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Queues bytes for the program, and writes as much as it will take now.
    pub fn send(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        self.flush();
    }

    pub fn flush(&mut self) {
        if sys::flush_some(&self.pty, &mut self.pending).is_err() {
            self.pending.clear();
        }
    }

    /// Feeds the program's output into the terminal.
    pub fn read(&mut self) -> Activity {
        let mut buf = vec![0; READ_SIZE];
        let mut read = 0;
        match (&self.pty).read(&mut buf) {
            // On Linux, the master reports EIO once the slave side is closed.
            Ok(0) => self.exited = true,
            Ok(len) => {
                read = len;
                self.terminal.write(buf.get(..len).unwrap_or(&[]));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => self.exited = true,
        }
        Activity {
            output: read,
            ..self.take_activity()
        }
    }

    fn take_activity(&mut self) -> Activity {
        let effects = self.terminal.effects();
        let replies = std::mem::take(&mut effects.replies);
        let activity = Activity {
            output: 0,
            bell: std::mem::take(&mut effects.bell),
            title_changed: std::mem::take(&mut effects.title_changed),
            clipboard: std::mem::take(&mut effects.clipboard),
            notifications: std::mem::take(&mut effects.notifications),
            progress_reported: std::mem::take(&mut effects.progress_reported),
        };
        if !replies.is_empty() && self.pending.len() < MAX_PENDING {
            self.send(&replies);
        }
        activity
    }

    /// Moves the pane, telling the program if that changed its size.
    pub fn place(&mut self, rect: Rect, cell_size: (u16, u16)) {
        let unchanged_size = rect.width == self.rect.width
            && rect.height == self.rect.height
            && cell_size == self.cell_size;
        self.rect = rect;
        self.redraw = true;
        if unchanged_size {
            return;
        }
        self.cell_size = cell_size;
        let (cols, rows) = (rect.width.max(1), rect.height.max(1));
        let (cell_width, cell_height) = cell_size;
        let result =
            self.terminal
                .resize(cols, rows, u32::from(cell_width), u32::from(cell_height));
        if let Err(error) = result {
            sys::log(format_args!("resizing pane {}: {error}", self.id));
        }
        let size = WinSize {
            cols,
            rows,
            width_px: cols.saturating_mul(cell_width),
            height_px: rows.saturating_mul(cell_height),
        };
        if let Err(error) = sys::set_winsize(self.pty.as_fd(), size) {
            sys::log(format_args!(
                "resizing the pty of pane {}: {error}",
                self.id
            ));
        }
        // A resize can move the cursor out from under copy mode.
        if let Some(copy_mode) = &mut self.copy_mode {
            copy_mode.x = copy_mode.x.min(cols - 1);
            copy_mode.y = copy_mode.y.min(rows - 1);
        }
        self.take_activity();
    }

    /// Takes over the outer terminal's colors, telling the program if it asked
    /// to hear about a change of color scheme.
    pub fn set_outer_colors(&mut self, colors: &OuterColors) {
        let report = self.terminal.set_outer_colors(colors);
        self.send(&report);
    }

    /// Tells the program, if it asked, that it gained or lost the keyboard.
    pub fn report_focus(&mut self, focused: bool) {
        let report = self.terminal.encode_focus(focused);
        self.send(&report);
    }

    /// The name of what is running in the foreground.
    pub fn foreground_name(&self) -> Option<String> {
        let pty = self.pty.as_fd();
        match &self.start_command {
            // The shell running a line stays around while the line runs, and
            // does not say what that is. A shell that the line itself started
            // takes the foreground for itself, which is then worth showing.
            Some(line) if sys::foreground_group(pty)?.cast_unsigned() == self.pid => {
                Some(name_from_line(line))
            }
            _ => sys::foreground_name(pty),
        }
    }

    /// The directory for a new pane opened from this one to start in. What the
    /// shell reported is preferred, since it survives programs in the foreground
    /// and works on every platform. The directory may be gone by now.
    pub fn working_directory(&self) -> Option<PathBuf> {
        let reported = self.terminal.pwd();
        path_from_pwd(&reported)
            .or_else(|| sys::foreground_cwd(self.pty.as_fd()).filter(|path| path.is_dir()))
    }
}

/// Shells report their directory with OSC 7, as either a path or a file URL.
fn path_from_pwd(pwd: &str) -> Option<PathBuf> {
    let path = match pwd.strip_prefix("file://") {
        Some(rest) => rest.find('/').and_then(|start| rest.get(start..))?,
        None => pwd,
    };
    let path = percent_decode(path);
    let path = PathBuf::from(path);
    path.is_dir().then_some(path)
}

fn percent_decode(text: &str) -> String {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        let escaped = (byte == b'%')
            .then(|| tail.get(..2))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(escaped) = escaped {
            bytes.push(escaped);
            rest = tail.get(2..).unwrap_or(&[]);
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_to_a_program_that_does_not_read_them_stop_at_a_limit() {
        let mut command = Command::new("sleep");
        command.arg("100");
        let size = WinSize {
            cols: 10,
            rows: 2,
            ..WinSize::default()
        };
        let (pty, mut child) = sys::spawn_on_pty(command, size).unwrap();
        let terminal = Terminal::new(10, 2).unwrap();
        let mut pane = Pane::adopt(0, child.id(), None, pty, terminal);
        let reply = vec![b'x'; 16 * 1024 * 1024];
        for _ in 0..12 {
            pane.terminal.effects().replies.clone_from(&reply);
            pane.take_activity();
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(pane.pending.len() >= MAX_PENDING - reply.len());
        assert!(pane.pending.len() < MAX_PENDING + reply.len());
    }

    fn name_of(program: &[&str]) -> String {
        let words: Vec<String> = program.iter().map(|word| (*word).to_owned()).collect();
        default_name(&words)
    }

    #[test]
    fn names_a_window_after_its_program() {
        assert_eq!(name_of(&["top"]), "top");
        assert_eq!(name_of(&["vim -p a b"]), "vim");
        assert_eq!(name_of(&["/usr/bin/tail", "-f", "x"]), "tail");
        assert_eq!(name_of(&["exec nvim"]), "nvim");
        assert_eq!(name_of(&["\"/opt/tool\" -x"]), "tool");
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("/a%20b"), "/a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn reads_pwd_as_a_path_or_a_url() {
        assert_eq!(path_from_pwd("/tmp"), Some(PathBuf::from("/tmp")));
        assert_eq!(
            path_from_pwd("file://host/tmp"),
            Some(PathBuf::from("/tmp"))
        );
        assert_eq!(path_from_pwd("file:///tmp"), Some(PathBuf::from("/tmp")));
        assert_eq!(path_from_pwd("/no/such/directory"), None);
        assert_eq!(path_from_pwd(""), None);
    }
}
