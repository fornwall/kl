//! End-to-end tests: the real `kl` binary on a pty, with libghostty-vt as the
//! outer terminal. Assertions see what a user of Ghostty would see, and the keys
//! sent are what Ghostty would send given the modes kl asked for.

#![expect(
    clippy::tests_outside_test_module,
    reason = "this is an integration test"
)]
#![expect(clippy::unwrap_used, reason = "helpers are test code too")]

#[expect(dead_code, reason = "shared with the binary, which uses more of it")]
#[path = "../src/ghostty/mod.rs"]
mod ghostty;
#[expect(
    dead_code,
    unused_imports,
    reason = "shared with the binary, which uses more of it"
)]
#[path = "../src/sys.rs"]
mod sys;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ghostty::key::{self, MOD_ALT, MOD_CTRL, MOD_SHIFT};
use ghostty::{
    KeyAction, KeyInput, Notification, OuterColors, Progress, Rgb, Style, Terminal, Width,
};
use sys::WinSize;

const KL: &str = env!("CARGO_BIN_EXE_kl");
const TIMEOUT: Duration = Duration::from_secs(10);

const OUTER_FOREGROUND: Rgb = Rgb {
    r: 0xee,
    g: 0xdd,
    b: 0xcc,
};
const OUTER_BACKGROUND: Rgb = Rgb {
    r: 0x11,
    g: 0x22,
    b: 0x33,
};
const OUTER_RED: Rgb = Rgb {
    r: 0xaa,
    g: 0xbb,
    b: 0xcc,
};

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// A scratch directory for one test's server, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kl-e2e-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Ends the server of a test that failed halfway through.
        let _ignored = kl_command(&self.0).arg("kill-server").output();
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn kl_command(dir: &Path) -> Command {
    let mut command = Command::new(KL);
    command
        .env("KL_TMPDIR", dir)
        // A login shell runs in panes, which must not read the real dotfiles.
        .env("HOME", dir)
        .env("SHELL", "/bin/sh")
        .env("PS1", "$ ")
        .env("ENV", "/dev/null")
        .env("TERM", "xterm-ghostty")
        .env_remove("KL");
    command
}

/// A terminal window with `kl` running in it.
struct Screen {
    terminal: Terminal,
    pty: File,
    child: Child,
    dir: Rc<Scratch>,
    /// Everything kl wrote, for checking sequences that leave no trace on screen.
    raw: Vec<u8>,
}

impl Screen {
    fn new(cols: u16, rows: u16) -> Self {
        Self::attach(Rc::new(Scratch::new()), cols, rows)
    }

    fn attach(dir: Rc<Scratch>, cols: u16, rows: u16) -> Self {
        let command = kl_command(&dir.0);
        Self::attach_with(command, dir, cols, rows)
    }

    fn attach_with(command: Command, dir: Rc<Scratch>, cols: u16, rows: u16) -> Self {
        let size = WinSize {
            cols,
            rows,
            width_px: cols * 10,
            height_px: rows * 20,
        };
        let (pty, child) = sys::spawn_on_pty(command, size).unwrap();
        let mut terminal = Terminal::new(cols, rows).unwrap();
        terminal.resize(cols, rows, 10, 20).unwrap();
        terminal.set_outer_colors(&OuterColors {
            foreground: Some(OUTER_FOREGROUND),
            background: Some(OUTER_BACKGROUND),
            palette: BTreeMap::from([(1, OUTER_RED)]),
            dark: Some(true),
        });
        let mut screen = Self {
            terminal,
            pty,
            child,
            dir,
            raw: Vec::new(),
        };
        screen.wait_for("the status line", |lines| {
            lines.last().is_some_and(|line| line.starts_with("[kl] "))
        });
        screen.wait_for("a prompt", |lines| lines[0].starts_with('$'));
        screen
    }

    /// Feeds what kl wrote to the terminal, and sends its replies back.
    fn pump(&mut self) {
        let mut buf = [0; 16384];
        loop {
            match self.pty.read(&mut buf) {
                Ok(0) => break,
                Ok(len) => {
                    self.raw.extend_from_slice(&buf[..len]);
                    self.terminal.write(&buf[..len]);
                    let replies = std::mem::take(&mut self.terminal.effects().replies);
                    self.pty.write_all(&replies).unwrap();
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                // The other side of the pty is gone: kl exited.
                Err(_) => break,
            }
        }
    }

    fn lines(&mut self) -> Vec<String> {
        self.pump();
        self.terminal.refresh();
        let mut lines = Vec::new();
        self.terminal.draw_rows(true, |_, cells| {
            let line: String = cells
                .iter()
                .filter(|cell| cell.width != Width::Tail)
                .map(|cell| cell.text.as_str())
                .collect();
            lines.push(line.trim_end().to_owned());
        });
        lines
    }

    /// Each run of linked cells, as its text and URI.
    fn links(&mut self) -> Vec<(String, String)> {
        self.pump();
        self.terminal.refresh();
        let mut rows = Vec::new();
        self.terminal
            .draw_rows(true, |_, cells| rows.push(cells.to_vec()));
        let mut links = Vec::new();
        for row in rows {
            let mut run: Option<(ghostty::Link, String)> = None;
            for cell in row.iter().filter(|cell| cell.width != Width::Tail) {
                if run.as_ref().map(|(link, _)| *link) != cell.link {
                    links.extend(run.take().map(|run| self.link_of(run)));
                    run = cell.link.map(|link| (link, String::new()));
                }
                if let Some((_, text)) = &mut run {
                    text.push_str(cell.text.as_str());
                }
            }
            links.extend(run.map(|run| self.link_of(run)));
        }
        links
    }

    fn link_of(&self, (link, text): (ghostty::Link, String)) -> (String, String) {
        let uri = self.terminal.link_uri(link).unwrap();
        (text, String::from_utf8_lossy(uri).into_owned())
    }

    fn wait_for_links(&mut self, wanted: &[(&str, &str)]) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let links = self.links();
            if links
                .iter()
                .map(|(text, uri)| (&**text, &**uri))
                .eq(wanted.iter().copied())
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {wanted:?}, with {links:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The `id` kl sent the outer terminal each time it opened a link to `uri`.
    fn link_ids(&self, uri: &str) -> Vec<String> {
        let text = self.raw_text();
        let opens = text.split("\x1b]8;id=").skip(1);
        let opens = opens.filter_map(|open| open.split_once(';'));
        opens
            .filter(|(_, rest)| {
                rest.strip_prefix(uri)
                    .is_some_and(|end| end.starts_with('\x1b'))
            })
            .map(|(id, _)| id.to_owned())
            .collect()
    }

    fn wait_for(&mut self, what: &str, done: impl Fn(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let lines = self.lines();
            if done(&lines) {
                return lines;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}. The screen shows:\n{}",
                lines.join("\n")
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_text(&mut self, text: &str) -> Vec<String> {
        self.wait_for(text, |lines| lines.iter().any(|line| line.contains(text)))
    }

    /// Waits for a line that is exactly this text, which unlike
    /// [`Self::wait_for_text`] will not match the command that prints it.
    /// Returns its row.
    fn wait_for_line(&mut self, text: &str) -> u16 {
        // In a split, the line of interest is what follows the last border.
        let matches = |line: &String| line.rsplit('│').next().map(str::trim) == Some(text);
        let lines = self.wait_for(text, |lines| lines.iter().any(matches));
        let row = lines.iter().position(matches).unwrap();
        u16::try_from(row).unwrap()
    }

    /// Waits for a clipboard write to reach the outer terminal.
    fn wait_for_clipboard(&mut self) -> Vec<Vec<u8>> {
        let deadline = Instant::now() + TIMEOUT;
        while self.terminal.effects().clipboard.is_empty() {
            self.pump();
            assert!(Instant::now() < deadline, "no clipboard write");
        }
        std::mem::take(&mut self.terminal.effects().clipboard)
    }

    /// Waits for a desktop notification to reach the outer terminal.
    fn wait_for_notification(&mut self) -> Notification {
        let deadline = Instant::now() + TIMEOUT;
        while self.terminal.effects().notifications.is_empty() {
            self.pump();
            assert!(Instant::now() < deadline, "no notification");
        }
        self.terminal.effects().notifications.remove(0)
    }

    /// Waits for the outer terminal to show this progress.
    fn wait_for_progress(&mut self, progress: Option<Progress>) {
        let deadline = Instant::now() + TIMEOUT;
        while self.terminal.progress() != progress {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "the outer terminal shows {:?}",
                self.terminal.progress()
            );
        }
    }

    /// A mouse report in pixels, as the outer terminal sends them when asked.
    fn mouse_pixels(&mut self, x: i32, y: i32) {
        self.send_bytes(format!("\x1b[<0;{x};{y}M").as_bytes());
        self.send_bytes(format!("\x1b[<0;{x};{y}m").as_bytes());
    }

    /// Waits for a mode of the outer terminal. Its reply, if it is asked about
    /// the mode, gets to kl before anything sent after this.
    fn wait_for_outer_mode(&mut self, mode: u16, value: bool) {
        let deadline = Instant::now() + TIMEOUT;
        while self.terminal.mode(mode) != value {
            self.pump();
            assert!(Instant::now() < deadline, "mode {mode} is not {value}");
        }
    }

    /// Waits for the outer terminal to show this pointer shape.
    fn wait_for_pointer_shape(&mut self, shape: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while self.terminal.pointer_shape() != shape {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "the pointer is a {}",
                self.terminal.pointer_shape()
            );
        }
    }

    /// The style of the first character of `text`, once it is on the screen.
    fn style_of(&mut self, text: &str) -> Style {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            self.terminal.refresh();
            let mut found = None;
            self.terminal.draw_rows(true, |_, cells| {
                let line: String = cells.iter().map(|cell| cell.text.as_str()).collect();
                if let Some(at) = line.find(text) {
                    let x = line.get(..at).unwrap().chars().count();
                    found = Some(cells[x].style);
                }
            });
            if let Some(style) = found {
                return style;
            }
            assert!(Instant::now() < deadline, "no {text} on the screen");
        }
    }

    /// Waits for `text` to get this style, which can lag behind the text.
    fn wait_for_style(&mut self, text: &str, done: impl Fn(Style) -> bool) -> Style {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let style = self.style_of(text);
            if done(style) {
                return style;
            }
            assert!(
                Instant::now() < deadline,
                "{text} has the style {style:?}. The screen shows:\n{}",
                self.lines().join("\n")
            );
        }
    }

    fn wait_for_no_text(&mut self, text: &str) -> Vec<String> {
        self.wait_for(&format!("{text} to go away"), |lines| {
            !lines.iter().any(|line| line.contains(text))
        })
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        self.pty.write_all(bytes).unwrap();
    }

    /// Presses and releases a key, as the outer terminal would report it. Like
    /// the Ghostty app, this has the key's text whatever the modifiers and
    /// the action, and leaves it to the encoder what to do with it.
    fn key(&mut self, key: i32, mods: u16, text: &str, unshifted: u32) {
        let bytes = self.key_bytes(key, mods, text, unshifted);
        self.send_bytes(&bytes);
    }

    fn key_bytes(&mut self, key: i32, mods: u16, text: &str, unshifted: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let shifted = text
            .chars()
            .next()
            .is_some_and(|c| u32::from(c) != unshifted);
        for action in [KeyAction::Press, KeyAction::Release] {
            let input = KeyInput {
                action,
                key,
                mods,
                consumed_mods: if shifted { mods & MOD_SHIFT } else { 0 },
                text,
                unshifted,
            };
            bytes.extend(self.terminal.encode_key(&input));
        }
        bytes
    }

    /// Types text on a US keyboard.
    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            let shifted = "~!@#$%^&*()_+{}|:\"<>?";
            let unshifted = r"`1234567890-=[]\;',./";
            let (base, mods) = match shifted.find(c) {
                Some(index) => (unshifted.as_bytes()[index] as char, MOD_SHIFT),
                None if c.is_ascii_uppercase() => (c.to_ascii_lowercase(), MOD_SHIFT),
                None => (c, 0),
            };
            let code = u32::from(base);
            let mut encoded = [0; 4];
            self.key(ghostty_key(base), mods, c.encode_utf8(&mut encoded), code);
        }
    }

    fn enter(&mut self) {
        self.key(key::ENTER, 0, "", 0);
    }

    fn run(&mut self, line: &str) {
        self.type_text(line);
        self.enter();
    }

    /// Runs `setup`, then `cat -v` to show what a program receives. Returns once
    /// `setup` took effect, since input encodes by the modes at that moment.
    fn run_setup_then_cat(&mut self, setup: &str) {
        self.run(&format!("{setup}; printf 'rea''dy\\n'; cat -v"));
        self.wait_for_line("ready");
    }

    fn ctrl(&mut self, c: char) {
        self.key(ghostty_key(c), MOD_CTRL, &c.to_string(), u32::from(c));
    }

    /// What the prefix key sends.
    fn ctrl_b_bytes(&mut self) -> Vec<u8> {
        self.key_bytes(ghostty_key('b'), MOD_CTRL, "", u32::from('b'))
    }

    /// The prefix followed by a key.
    fn prefix(&mut self, text: &str) {
        self.ctrl('b');
        self.type_text(text);
    }

    fn command(&self, args: &[&str]) -> Output {
        kl_command(&self.dir.0).args(args).output().unwrap()
    }

    /// Replaces the server, returning once this client is attached to the new
    /// one, which draws everything. Keys sent before that would be lost.
    fn restart_server(&mut self) {
        self.pump();
        let start = self.raw.len();
        let output = self.command(&["restart-server"]);
        assert!(output.status.success(), "{output:?}");
        let cleared = |raw: &[u8]| raw.windows(4).any(|bytes| bytes == b"\x1b[2J");
        let deadline = Instant::now() + TIMEOUT;
        while !cleared(&self.raw[start..]) {
            self.pump();
            assert!(Instant::now() < deadline, "no new server");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn mouse(&mut self, code: u16, x: u16, y: u16, press: bool) {
        let end = if press { 'M' } else { 'm' };
        let sequence = format!("\x1b[<{code};{};{}{end}", x + 1, y + 1);
        self.send_bytes(sequence.as_bytes());
    }

    fn click(&mut self, x: u16, y: u16) {
        self.mouse(0, x, y, true);
        self.mouse(0, x, y, false);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let size = WinSize {
            cols,
            rows,
            width_px: cols * 10,
            height_px: rows * 20,
        };
        self.terminal.resize(cols, rows, 10, 20).unwrap();
        sys::set_winsize(self.pty.as_fd(), size).unwrap();
    }

    /// What a change of theme in the outer terminal comes down to.
    fn change_outer_colors(&mut self, colors: &OuterColors) {
        let report = self.terminal.set_outer_colors(colors);
        assert!(!report.is_empty(), "kl did not ask to hear about this");
        self.send_bytes(&report);
    }

    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            self.pump();
            assert!(Instant::now() < deadline, "kl did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        self.pump();
    }

    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }
}

/// The shell command to print a link.
fn print_link(uri: &str, text: &str) -> String {
    format!(r"printf '\033]8;;{uri}\033\\{text}\033]8;;\033\\'")
}

fn ghostty_key(c: char) -> i32 {
    ghostty::key::from_codepoint(u32::from(c))
}

#[test]
fn starts_a_shell_with_a_status_line() {
    let mut screen = Screen::new(80, 24);
    screen.run("echo hello-$((40 + 2))");
    let lines = screen.wait_for_text("hello-42");
    let status = lines.last().unwrap();
    assert!(status.starts_with("[kl] 0:"), "{status}");
    assert!(status.contains('*'), "{status}");
}

#[test]
fn panes_see_a_ghostty_terminal_of_the_right_size() {
    let mut screen = Screen::new(100, 30);
    screen.run("echo $TERM $COLORTERM $KL_PANE; stty size");
    let lines = screen.wait_for_text("29 100");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("xterm-ghostty truecolor %0")),
        "{lines:?}"
    );
}

#[test]
fn panes_find_the_terminfo_entry_whatever_the_outer_terminal_is() {
    let dir = Rc::new(Scratch::new());
    let mut command = kl_command(&dir.0);
    command.env("TERM", "xterm-256color");
    let mut screen = Screen::attach_with(command, dir, 80, 24);
    screen.run("echo \"$TERM is $(tput longname)\"");
    screen.wait_for_text("xterm-ghostty is Ghostty");
}

#[test]
fn splits_side_by_side_and_keeps_typing_in_the_new_pane() {
    let mut screen = Screen::new(80, 24);
    screen.run("echo first");
    screen.wait_for_text("first");
    screen.prefix("%");
    let lines = screen.wait_for("a border", |lines| lines[5].contains('│'));
    assert_eq!(
        lines[5].chars().position(|c| c == '│'),
        Some(40),
        "{lines:?}"
    );

    screen.run("echo second");
    let lines = screen.wait_for_text("second");
    let row = lines
        .iter()
        .find(|line| line.contains("echo second"))
        .unwrap();
    assert!(row.find("echo second").unwrap() > 40, "{row}");

    let panes = screen.command(&["list-panes"]);
    let panes = String::from_utf8_lossy(&panes.stdout);
    assert_eq!(panes.lines().count(), 2, "{panes}");
    assert!(panes.contains("[39x23]"), "{panes}");
    assert!(panes.contains("[40x23]"), "{panes}");
}

#[test]
fn splits_stacked() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("\"");
    let lines = screen.wait_for("a border", |lines| lines[11].contains('─'));
    assert!(lines[11].chars().all(|c| c == '─'), "{lines:?}");
    screen.run("stty size");
    screen.wait_for_text("11 80");
}

#[test]
fn moves_between_panes_with_the_arrow_keys() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.ctrl('b');
    screen.key(key::ARROW_LEFT, 0, "", 0);
    screen.run("echo left");
    let lines = screen.wait_for_text("echo left");
    let row = lines
        .iter()
        .find(|line| line.contains("echo left"))
        .unwrap();
    assert!(row.find("echo left").unwrap() < 39, "{row}");
}

#[test]
fn closing_a_pane_gives_its_space_back() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.run("exit");
    screen.wait_for("the border to go", |lines| !lines[5].contains('│'));
    screen.run("stty size");
    screen.wait_for_text("23 80");
}

#[test]
fn kill_pane_asks_first() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.prefix("x");
    screen.wait_for_text("kill-pane 1? (y/n)");
    screen.type_text("n");
    screen.wait_for_no_text("kill-pane 1?");
    assert!(screen.lines()[5].contains('│'));
    screen.prefix("x");
    screen.wait_for_text("kill-pane 1? (y/n)");
    screen.type_text("y");
    screen.wait_for("the border to go", |lines| !lines[5].contains('│'));
}

#[test]
fn zoom_gives_a_pane_the_whole_window() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.prefix("z");
    let lines = screen.wait_for("the zoom flag", |lines| {
        lines.last().is_some_and(|status| status.contains("*Z"))
    });
    assert!(!lines[5].contains('│'), "{lines:?}");
    screen.run("stty size");
    screen.wait_for_text("23 80");
    screen.prefix("z");
    screen.wait_for("the border to return", |lines| lines[5].contains('│'));
}

#[test]
fn switches_between_windows() {
    let mut screen = Screen::new(80, 24);
    screen.run("echo one");
    screen.wait_for_text("one");
    screen.prefix("c");
    let lines = screen.wait_for("a second window", |lines| {
        lines.last().is_some_and(|status| status.contains(" 1:"))
    });
    assert!(
        !lines.iter().any(|line| line.contains("echo one")),
        "{lines:?}"
    );
    screen.run("echo two");
    screen.wait_for_text("two");

    screen.prefix("0");
    screen.wait_for_text("echo one");
    screen.wait_for_no_text("echo two");
    screen.prefix("n");
    screen.wait_for_text("echo two");
    screen.prefix("p");
    screen.wait_for_text("echo one");
    screen.prefix("l");
    screen.wait_for_text("echo two");
}

/// Splits the window in two, one above the other. Returns the border's row.
fn split_stacked(screen: &mut Screen) -> usize {
    screen.prefix("\"");
    let is_border = |line: &String| line.starts_with('─');
    let lines = screen.wait_for("a border", |lines| lines.iter().any(is_border));
    lines.iter().position(is_border).unwrap()
}

#[test]
fn resize_keys_repeat_without_the_prefix() {
    let mut screen = Screen::new(80, 25);
    let border = split_stacked(&mut screen);

    // All at once, since the time between them is what is at stake.
    let up = screen.key_bytes(key::ARROW_UP, MOD_CTRL, "", 0);
    let mut keys = screen.ctrl_b_bytes();
    keys.extend_from_slice(&up);
    keys.extend_from_slice(&up);
    screen.send_bytes(&keys);
    screen.wait_for("the border to move twice", |lines| {
        lines[border - 2].contains('─')
    });
}

#[test]
fn modifiers_do_not_end_a_repeat() {
    let mut screen = Screen::new(80, 25);
    let border = split_stacked(&mut screen);

    let up = screen.key_bytes(key::ARROW_UP, MOD_CTRL, "", 0);
    let control = screen.key_bytes(key::CONTROL_LEFT, 0, "", 0);
    let mut keys = screen.ctrl_b_bytes();
    keys.extend_from_slice(&up);
    keys.extend_from_slice(&control);
    keys.extend_from_slice(&up);
    screen.send_bytes(&keys);
    screen.wait_for("the border to move twice", |lines| {
        lines[border - 2].contains('─')
    });
}

#[test]
fn other_keys_end_a_repeat_and_reach_the_program() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    let left = screen.key_bytes(key::ARROW_LEFT, 0, "", 0);
    let c = screen.key_bytes(ghostty_key('c'), 0, "c", u32::from('c'));
    let mut keys = screen.ctrl_b_bytes();
    keys.extend_from_slice(&left);
    keys.extend_from_slice(&c);
    screen.send_bytes(&keys);
    screen.wait_for("the key in the left pane", |lines| {
        lines[0].starts_with("$ c")
    });
    let output = screen.command(&["list-windows"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
}

#[test]
fn a_repeat_runs_out() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.ctrl('b');
    screen.key(key::ARROW_LEFT, 0, "", 0);
    std::thread::sleep(Duration::from_millis(600));
    // With the repeat over, this moves the cursor in the shell instead.
    screen.key(key::ARROW_RIGHT, 0, "", 0);
    screen.type_text("typed");
    screen.wait_for("the key in the left pane", |lines| {
        lines[0].contains("typed")
    });
}

#[test]
fn lists_the_keys() {
    let screen = Screen::new(80, 24);
    let output = screen.command(&["list-keys", "-N"]);
    let keys = String::from_utf8_lossy(&output.stdout);
    assert!(keys.contains("C-b %"), "{keys}");
    assert!(keys.contains("Split window horizontally"), "{keys}");
}

#[test]
fn lists_the_keys_on_screen_and_goes_back_to_the_panes() {
    let mut screen = Screen::new(80, 24);
    screen.run("echo still-here");
    screen.wait_for_line("still-here");
    screen.prefix("?");
    screen.wait_for_text("C-b ?");
    screen.wait_for_no_text("still-here");

    // Keys go to the list and not the shell, until it is left.
    screen.type_text("G");
    screen.wait_for_text("Resize the pane right");
    screen.type_text("q");
    screen.wait_for_line("still-here");
    screen.wait_for_no_text("C-b ?");
}

#[test]
fn shows_what_a_command_prints_in_a_list() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("c");
    screen.wait_for_text(" 1:");
    screen.prefix(":");
    screen.run("list-windows");
    screen.wait_for_text("0: ");
    screen.wait_for_text("1: ");
    screen.key(key::ESCAPE, 0, "", 0);
    screen.wait_for_no_text("1 panes");
}

/// A session of three windows named a, b and c, showing the last one.
fn three_windows() -> Screen {
    let mut screen = Screen::new(80, 24);
    assert!(screen.command(&["rename-window", "a"]).status.success());
    assert!(screen.command(&["new-window", "-n", "b"]).status.success());
    assert!(screen.command(&["new-window", "-n", "c"]).status.success());
    screen.wait_for_text("2:c*");
    screen
}

#[test]
fn chooses_a_window_from_a_list() {
    let mut screen = three_windows();
    screen.prefix("w");
    let lines = screen.wait_for_text("(0) 0: a");
    assert!(lines[1].contains("(1) 1: b-"), "{lines:?}");
    assert!(lines[2].contains("(2) 2: c*"), "{lines:?}");

    // It starts on the current window, and goes up from there.
    screen.type_text("kk");
    screen.enter();
    screen.wait_for_text("0:a*");
    screen.wait_for_no_text("(0) 0: a");
}

#[test]
fn chooses_a_window_by_its_number_in_the_list() {
    let mut screen = three_windows();
    screen.prefix("w");
    screen.wait_for_text("(0) 0: a");
    screen.type_text("1");
    screen.wait_for_text("1:b*");
}

#[test]
fn leaving_the_window_list_chooses_nothing() {
    let mut screen = three_windows();
    screen.prefix("w");
    screen.wait_for_text("(0) 0: a");
    screen.type_text("k");
    screen.key(key::ESCAPE, 0, "", 0);
    screen.wait_for_no_text("(0) 0: a");
    assert!(screen.lines().last().unwrap().contains("2:c*"));
}

#[test]
fn the_mouse_works_in_the_window_list() {
    let mut screen = three_windows();
    screen.prefix("w");
    screen.wait_for_text("(0) 0: a");
    screen.click(5, 1);
    screen.click(5, 1);
    screen.wait_for_text("1:b*");
    screen.wait_for_no_text("(0) 0: a");
}

/// Two panes side by side, numbered 0 and 1, with the second one active.
fn numbered_panes() -> Screen {
    let mut screen = Screen::new(81, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.wait_for("the second prompt", |lines| lines[0].ends_with('$'));
    screen
}

#[test]
fn shows_pane_numbers_and_picks_a_pane_by_one() {
    let mut screen = numbered_panes();
    screen.command(&["display-panes", "-d", "0"]);
    screen.wait_for_text("40x23");
    screen.type_text("0");
    screen.wait_for_no_text("40x23");
    screen.type_text("left");
    screen.wait_for("the key in the left pane", |lines| {
        lines[0].starts_with("$ left")
    });
}

#[test]
fn other_keys_end_the_pane_numbers_and_do_what_they_usually_do() {
    let mut screen = numbered_panes();
    screen.command(&["display-panes", "-d", "0"]);
    screen.wait_for_text("40x23");
    screen.type_text("z");
    screen.wait_for_no_text("40x23");
    screen.wait_for("the key in the right pane", |lines| {
        lines[0].ends_with("$ z")
    });
}

#[test]
fn pane_numbers_go_away_on_their_own() {
    let mut screen = numbered_panes();
    screen.prefix("q");
    let deadline = Instant::now() + TIMEOUT;
    while !screen.raw_text().contains("40x23") {
        screen.pump();
        assert!(Instant::now() < deadline, "no pane numbers");
    }
    screen.wait_for_no_text("40x23");
}

#[test]
fn picking_a_hidden_pane_by_number_ends_the_zoom() {
    let mut screen = numbered_panes();
    screen.prefix("z");
    screen.wait_for_text("*Z");
    screen.command(&["display-panes", "-d", "0"]);
    screen.wait_for_text("81x23");
    screen.type_text("0");
    screen.wait_for("the zoom to end", |lines| lines[5].contains('│'));
    screen.type_text("left");
    screen.wait_for("the key in the left pane", |lines| {
        lines[0].starts_with("$ left")
    });
}

#[test]
fn renames_a_window() {
    let mut screen = Screen::new(80, 24);
    screen.prefix(",");
    screen.wait_for_text("(rename-window)");
    // Clear the suggested name first.
    screen.ctrl('u');
    screen.type_text("logs");
    screen.enter();
    screen.wait_for_text("0:logs*");
}

#[test]
fn a_new_window_has_a_name_from_the_start() {
    let screen = Screen::new(80, 24);
    assert!(
        screen
            .command(&["new-window", "sleep 100"])
            .status
            .success()
    );
    let output = screen.command(&["list-windows"]);
    let windows = String::from_utf8_lossy(&output.stdout);
    assert!(windows.contains("1: sleep "), "{windows}");
}

#[test]
fn a_window_started_from_a_line_is_not_named_after_the_shell_running_it() {
    let screen = Screen::new(80, 24);
    assert!(
        screen
            .command(&["new-window", "sleep 100"])
            .status
            .success()
    );
    // Past the time that names are looked at again.
    std::thread::sleep(Duration::from_millis(1100));
    let windows = screen.command(&["list-windows"]);
    let windows = String::from_utf8_lossy(&windows.stdout);
    assert!(windows.contains("1: sleep "), "{windows}");

    assert!(screen.command(&["restart-server"]).status.success());
    std::thread::sleep(Duration::from_millis(1100));
    let windows = screen.command(&["list-windows"]);
    let windows = String::from_utf8_lossy(&windows.stdout);
    assert!(windows.contains("1: sleep "), "{windows}");
}

#[test]
fn a_shell_started_from_a_line_shows_what_it_runs() {
    let mut screen = Screen::new(80, 24);
    assert!(screen.command(&["new-window", "sh -i"]).status.success());
    screen.wait_for("the prompt of the new shell", |lines| {
        lines[0].starts_with('$')
    });
    screen.run("sleep 100");
    screen.wait_for("the name of the program", |lines| {
        lines
            .last()
            .is_some_and(|status| status.contains("1:sleep*"))
    });
}

#[test]
fn window_names_follow_what_runs_in_the_foreground() {
    let mut screen = Screen::new(80, 24);
    screen.run("sleep 100");
    screen.wait_for("the name of the program", |lines| {
        lines
            .last()
            .is_some_and(|status| status.contains("0:sleep*"))
    });
    screen.ctrl('c');
    screen.wait_for("the name of the shell", |lines| {
        lines.last().is_some_and(|status| status.contains("0:sh*"))
    });
}

#[test]
fn runs_commands_from_the_prompt() {
    let mut screen = Screen::new(80, 24);
    screen.prefix(":");
    screen.wait_for("the prompt", |lines| lines.last().unwrap().starts_with(':'));
    screen.type_text("new-window -n build");
    screen.enter();
    screen.wait_for_text("1:build*");

    screen.prefix(":");
    screen.wait_for("the prompt", |lines| lines.last().unwrap().starts_with(':'));
    screen.type_text("frobnicate");
    screen.enter();
    screen.wait_for_text("unknown command: frobnicate");
}

#[test]
fn escape_leaves_the_prompt() {
    let mut screen = Screen::new(80, 24);
    screen.prefix(":");
    screen.wait_for("the prompt", |lines| lines.last().unwrap().starts_with(':'));
    screen.key(key::ESCAPE, 0, "", 0);
    screen.wait_for("the status line", |lines| {
        lines.last().unwrap().starts_with("[kl]")
    });
}

#[test]
fn runs_commands_from_the_command_line() {
    let mut screen = Screen::new(80, 24);
    let output = screen.command(&["new-window", "-n", "cli"]);
    assert!(output.status.success());
    screen.wait_for_text("1:cli*");

    let output = screen.command(&["list-windows"]);
    let windows = String::from_utf8_lossy(&output.stdout);
    assert!(
        windows.contains("1: cli (1 panes) [80x23] (active)"),
        "{windows}"
    );

    screen.wait_for("a prompt", |lines| lines[0].starts_with('$'));
    screen.command(&["send-keys", "echo sent", "Enter"]);
    screen.wait_for_text("sent");

    let output = screen.command(&["no-such-command"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("unknown command: no-such-command"),
        "{error}"
    );
}

#[test]
fn commands_explain_when_no_server_runs() {
    let dir = Scratch::new();
    let output = kl_command(&dir.0).arg("list-windows").output().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("no server running"), "{error}");
}

#[test]
fn survives_arguments_that_are_not_text() {
    let scratch = Scratch::new();
    let output = kl_command(&scratch.0)
        .args(["send-keys".as_ref(), OsStr::from_bytes(b"\xff")])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("kl: no server running"), "{stderr}");
}

#[test]
fn reports_an_unusable_socket_path_right_away() {
    let dir = Scratch::new();
    let long = dir.0.join("x".repeat(120));
    fs::create_dir_all(&long).unwrap();
    let size = WinSize {
        cols: 80,
        rows: 24,
        ..WinSize::default()
    };
    let mut command = kl_command(&dir.0);
    command.env("KL_TMPDIR", &long);
    let (mut pty, mut child) = sys::spawn_on_pty(command, size).unwrap();
    let start = Instant::now();
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    let mut output = Vec::new();
    let mut buf = [0; 1024];
    while let Ok(len @ 1..) = pty.read(&mut buf) {
        output.extend_from_slice(&buf[..len]);
    }
    let output = String::from_utf8_lossy(&output);
    assert!(output.contains("SUN_LEN"), "{output}");
}

#[test]
fn survives_detaching_and_shows_new_output_on_return() {
    let mut screen = Screen::new(80, 24);
    screen.run("sleep 1; echo finished-while-away");
    screen.prefix("d");
    screen.wait_for_exit();
    assert!(screen.raw_text().contains("[detached]"));

    let Screen { dir, .. } = screen;
    let mut screen = Screen::attach(dir, 80, 24);
    screen.wait_for_text("finished-while-away");
}

#[test]
fn typing_through_a_detach_does_not_lose_the_goodbye() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("d");
    while screen.child.try_wait().unwrap().is_none() {
        // Keeps the client busy sending, past the server closing the connection.
        drop(screen.pty.write(b"x"));
        screen.pump();
    }
    screen.pump();
    let text = screen.raw_text();
    assert!(text.contains("[detached]"), "{text}");
}

#[test]
fn reattaching_at_another_size_resizes_the_panes() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("d");
    screen.wait_for_exit();
    let Screen { dir, .. } = screen;
    let mut screen = Screen::attach(dir, 100, 30);
    screen.run("stty size");
    screen.wait_for_text("29 100");
}

#[test]
fn new_panes_get_the_environment_of_the_latest_client() {
    let dir = Rc::new(Scratch::new());
    let mut command = kl_command(&dir.0);
    command.env("SSH_AUTH_SOCK", "/old").env("DISPLAY", ":1");
    let mut screen = Screen::attach_with(command, dir, 80, 24);
    screen.prefix("d");
    screen.wait_for_exit();

    let Screen { dir, .. } = screen;
    let mut command = kl_command(&dir.0);
    command.env("SSH_AUTH_SOCK", "/new").env_remove("DISPLAY");
    let mut screen = Screen::attach_with(command, dir, 80, 24);
    screen.prefix("c");
    screen.wait_for_text("1:");
    screen.run("echo \"[$SSH_AUTH_SOCK ${DISPLAY-unset}]\"");
    screen.wait_for_text("[/new unset]");
}

#[test]
fn a_new_client_takes_over_from_the_old_one() {
    let mut first = Screen::new(80, 24);
    let mut second = Screen::attach(Rc::clone(&first.dir), 80, 24);
    first.wait_for_exit();
    assert!(
        first
            .raw_text()
            .contains("[detached (from another client)]")
    );
    second.run("echo still-here");
    second.wait_for_text("still-here");
}

#[test]
fn programs_keep_running_across_a_server_restart() {
    let mut screen = Screen::new(80, 24);
    screen.run("echo from-the-first-server");
    screen.wait_for_line("from-the-first-server");
    screen.prefix("%");
    screen.run_setup_then_cat("echo in-the-split");
    screen.type_text("typed-before");
    screen.enter();
    screen.wait_for_text("typed-before");

    screen.restart_server();
    screen.wait_for_text("from-the-first-server");

    // The same program is still there, with the same client attached.
    screen.type_text("typed-after");
    screen.enter();
    let lines = screen.wait_for("the program to echo what was typed", |lines| {
        let echoed = lines.iter().filter(|line| line.contains("typed-after"));
        echoed.count() == 2
    });
    assert!(lines.iter().any(|line| line.contains("in-the-split")));
    assert!(!screen.raw_text().contains("[detached"));

    // And the session is what it was, a window with two panes.
    let output = screen.command(&["list-panes"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 2);
}

#[test]
fn a_restart_keeps_history_and_copied_text() {
    let mut screen = Screen::new(80, 24);
    screen.run("for i in $(seq 1 100); do echo history-$i; done");
    screen.wait_for_line("history-100");
    // The line above the prompt, which only a paste can put after a prompt.
    screen.prefix("[");
    screen.type_text("k0 $");
    screen.enter();
    screen.wait_for_clipboard();

    screen.restart_server();
    screen.prefix("[");
    screen.type_text("g");
    screen.wait_for_text("for i in");
    screen.type_text("q");
    screen.wait_for_no_text("for i in");
    screen.prefix("]");
    screen.wait_for_text("$ history-100");
}

#[test]
fn a_restart_leaves_nothing_showing_that_nobody_remembers() {
    let mut screen = Screen::new(80, 24);
    screen.run(r"printf '\033]9;4;1;50\033\\'");
    screen.wait_for_progress(Some(Progress {
        state: 1,
        percent: Some(50),
    }));
    screen.restart_server();
    screen.wait_for_progress(None);
}

#[test]
fn a_failed_restart_leaves_the_server_running() {
    let mut screen = Screen::new(80, 24);
    let output = screen.command(&["restart-server", "/bin/false"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot take over"));
    screen.run("echo still-$((1 + 1))");
    screen.wait_for_text("still-2");
}

#[test]
fn ends_when_the_last_shell_exits() {
    let mut screen = Screen::new(80, 24);
    screen.run("exit");
    screen.wait_for_exit();
    assert!(screen.raw_text().contains("[exited]"));
    let output = screen.command(&["list-windows"]);
    assert!(!output.status.success());
}

#[test]
fn kill_server_ends_the_session() {
    let mut screen = Screen::new(80, 24);
    assert!(screen.command(&["kill-server"]).status.success());
    screen.wait_for_exit();
    assert!(screen.raw_text().contains("[server exited]"));
}

#[test]
fn leaves_the_terminal_as_it_found_it() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("d");
    screen.wait_for_exit();
    let raw = screen.raw_text();
    for sequence in [
        "\x1b[<u",
        "\x1b[?1049l",
        "\x1b[?1003l",
        "\x1b[?1006l",
        "\x1b[>0s",
        "\x1b[?2004l",
        "\x1b[?25h",
    ] {
        assert!(raw.contains(sequence), "{sequence:?} missing");
    }
    // The title stack for terminals that have it, and for Ghostty, no title.
    assert!(raw.contains("\x1b]2;\x1b\\\x1b[23;2t"));
    // Ghostty has grapheme clustering on by default, and others have it off.
    for setting in ["\x1b[?2027h", "\x1b[?2027l"] {
        let mut outer = Terminal::new(80, 24).unwrap();
        outer.write(setting.as_bytes());
        let before = outer.mode(2027);
        outer.write(&screen.raw);
        assert_eq!(outer.mode(2027), before, "after {setting:?}");
    }
}

#[test]
fn survives_the_terminal_going_away() {
    // Keeping the directory keeps the server, which would otherwise end first.
    let Screen {
        pty,
        mut child,
        dir: _dir,
        ..
    } = Screen::new(80, 24);
    drop(pty);
    let status = child.wait().unwrap();
    assert!(matches!(status.code(), Some(0 | 1)), "{status}");
}

#[test]
fn does_not_mind_nobody_reading_its_output() {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut command = Command::new(KL);
    let status = command.arg("--help").stdout(writer).status().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn reaps_the_panes_that_end() {
    let mut screen = Screen::new(80, 24);
    assert!(screen.command(&["split-window", "true"]).status.success());
    assert!(screen.command(&["split-window"]).status.success());
    assert!(screen.command(&["kill-pane"]).status.success());
    screen.run(
        r#"while ps -A -o ppid=,stat= | grep -q "^ *${KL##*,} *Z"; do sleep 0.01; done; echo re''aped"#,
    );
    screen.wait_for_line("reaped");
}

#[test]
fn opens_panes_after_the_directory_is_gone() {
    let mut screen = Screen::new(80, 24);
    screen.run("cd; mkdir gone; cd gone; rmdir ../gone; echo re''ady");
    screen.wait_for_line("ready");
    let output = screen.command(&["split-window"]);
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn refuses_to_nest() {
    let mut screen = Screen::new(80, 24);
    screen.run(&format!("{KL}; echo status-$?"));
    let lines = screen.wait_for_text("status-1");
    assert!(
        lines.iter().any(|line| line.contains("nested with care")),
        "{lines:?}"
    );
}

#[test]
fn moving_the_mouse_over_nothing_draws_nothing() {
    let mut screen = Screen::new(80, 24);
    screen.pump();
    let start = screen.raw.len();
    for x in 0..20 {
        screen.mouse(35, x, 5, true);
        std::thread::sleep(Duration::from_millis(10));
    }
    screen.pump();
    let frames = screen.raw[start..]
        .windows(8)
        .filter(|window| *window == b"\x1b[?2026h")
        .count();
    // One is allowed for, in case the clock on the status line moved on.
    assert!(frames <= 1, "{frames} frames");
}

#[test]
fn follows_the_outer_terminal_size() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.resize(120, 40);
    screen.wait_for("the border to move", |lines| {
        lines[5].chars().nth(60) == Some('│')
    });
    screen.run("stty size");
    screen.wait_for_text("39 59");
    let status = screen.lines();
    assert!(status[39].starts_with("[kl]"), "{status:?}");
}

#[test]
fn bindings_work_on_a_cyrillic_layout() {
    let mut screen = Screen::new(80, 24);
    let (c_key, es) = (ghostty_key('c'), '\u{441}');
    // What the outer terminal sends for the keys labeled B and C on that layout.
    screen.key(ghostty_key('b'), MOD_CTRL, "", u32::from('\u{438}'));
    screen.key(c_key, 0, &es.to_string(), u32::from(es));
    screen.wait_for_text("1:");
}

#[test]
fn control_keys_reach_programs() {
    let mut screen = Screen::new(80, 24);
    screen.run("sleep 100");
    screen.wait_for_text("0:sleep*");
    screen.ctrl('c');
    screen.wait_for_text("0:sh*");
    screen.run("echo status-$?");
    screen.wait_for_line("status-130");
}

#[test]
fn the_prefix_can_be_sent_to_programs() {
    let mut screen = Screen::new(80, 24);
    screen.run("cat -v");
    screen.ctrl('b');
    screen.ctrl('b');
    screen.enter();
    screen.wait_for_text("^B");
}

#[test]
fn alt_keys_reach_programs() {
    let mut screen = Screen::new(80, 24);
    screen.run_setup_then_cat(":");
    screen.key(ghostty_key('x'), MOD_ALT, "x", u32::from('x'));
    screen.enter();
    screen.wait_for_text("^[x");
}

#[test]
fn shifted_keys_reach_programs() {
    let mut screen = Screen::new(80, 24);
    screen.run("echo 'Hello, World! 100%'");
    screen.wait_for_text("Hello, World! 100%");
}

#[test]
fn shift_with_another_modifier_reaches_programs() {
    let mut screen = Screen::new(80, 24);
    screen.run_setup_then_cat("true");
    let x = u32::from('x');
    screen.key(ghostty_key('x'), MOD_ALT | MOD_SHIFT, "X", x);
    screen.key(ghostty_key('x'), MOD_CTRL | MOD_SHIFT, "X", x);
    screen.enter();
    screen.wait_for_text("^[X^[[120;6u");
}

#[test]
fn programs_get_their_cursor_key_mode() {
    let mut screen = Screen::new(80, 24);
    screen.run_setup_then_cat(r"printf '\033[?1h'");
    screen.key(key::ARROW_UP, 0, "", 0);
    screen.enter();
    screen.wait_for_text("^[OA");
}

#[test]
fn programs_can_use_the_kitty_keyboard_protocol() {
    let mut screen = Screen::new(80, 24);
    screen.run_setup_then_cat(r"printf '\033[>1u'");
    screen.key(key::ESCAPE, 0, "", 0);
    screen.enter();
    screen.wait_for_text("^[[27u");
}

#[test]
fn keeps_colors_and_styles() {
    let mut screen = Screen::new(80, 24);
    screen.run(r"printf '\033[1;31mred\033[0m \033[38;2;1;2;3mrgb\033[0m\n'");
    screen.wait_for_text("red rgb");

    screen.terminal.refresh();
    let mut found = Vec::new();
    screen.terminal.draw_rows(true, |_, cells| {
        let text: String = cells.iter().map(|cell| cell.text.as_str()).collect();
        if text.starts_with("red rgb") {
            found = cells.to_vec();
        }
    });
    assert_eq!(found[0].style.fg.as_palette(), Some(1));
    assert_ne!(found[0].style.flags & ghostty::STYLE_BOLD, 0);
    assert_eq!(found[4].style.fg.as_rgb(), Some([1, 2, 3]));
    assert_eq!(found[3].style, ghostty::Style::default());
}

#[test]
fn wide_characters_do_not_disturb_their_neighbors() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.ctrl('b');
    screen.key(key::ARROW_LEFT, 0, "", 0);
    screen.run(r"printf '\344\270\226\347\225\214 x\n'");
    let lines = screen.wait_for_text("\u{4e16}\u{754c} x");
    let row = lines
        .iter()
        .find(|line| line.starts_with('\u{4e16}'))
        .unwrap();
    assert!(row.contains('│'), "{row}");
}

#[test]
fn shows_the_title_of_the_active_pane() {
    let mut screen = Screen::new(80, 24);
    screen.run(r"printf '\033]2;my-title\007'");
    screen.wait_for_text("\"my-title\"");
    assert_eq!(screen.terminal.title(), "my-title");
}

#[test]
fn a_bell_in_another_window_is_flagged() {
    let mut screen = Screen::new(80, 24);
    screen.run(r"sleep 1; printf '\007'");
    screen.prefix("c");
    screen.wait_for("a second window", |lines| {
        lines.last().is_some_and(|status| status.contains(" 1:"))
    });
    screen.wait_for("the bell flag", |lines| {
        lines.last().is_some_and(|status| status.contains('!'))
    });
    screen.prefix("0");
    screen.wait_for("the bell flag to clear", |lines| {
        lines.last().is_some_and(|status| !status.contains('!'))
    });
}

#[test]
fn notifications_get_out_of_windows_out_of_sight() {
    let mut screen = Screen::new(80, 24);
    screen.run(r"sleep 0.5; printf '\033]9;all done\033\\\033]777;notify;make;failed\033\\'");
    screen.prefix("c");
    let notification = screen.wait_for_notification();
    assert_eq!(notification.title, "");
    assert_eq!(notification.body, "all done");
    let notification = screen.wait_for_notification();
    assert_eq!(notification.title, "make");
    assert_eq!(notification.body, "failed");
}

#[test]
fn progress_follows_what_can_be_seen() {
    let half = Some(Progress {
        state: 1,
        percent: Some(50),
    });
    let mut screen = Screen::new(80, 24);
    screen.run(r"printf '\033]9;4;1;50\033\\'");
    screen.wait_for_progress(half);

    // Next to the pane it is still in view, in another window it is not.
    screen.prefix("%");
    screen.wait_for("a second pane", |lines| lines[0].contains('│'));
    screen.prefix("c");
    screen.wait_for_progress(None);
    screen.prefix("0");
    screen.wait_for_progress(half);

    // The active pane goes first, and the other one is next in line.
    let unknown = Some(Progress {
        state: 3,
        percent: None,
    });
    screen.run(r"printf '\033]9;4;3\033\\'");
    screen.wait_for_progress(unknown);
    screen.run(r"printf '\033]9;4;0\033\\'");
    screen.wait_for_progress(half);

    // The outer terminal drops a report that goes quiet, so repeats get through.
    let sent = screen.raw_text().matches("9;4;1;50").count();
    screen.prefix("o");
    screen.run(r"printf '\033]9;4;1;50\033\\'");
    let deadline = Instant::now() + TIMEOUT;
    while screen.raw_text().matches("9;4;1;50").count() == sent {
        screen.pump();
        assert!(Instant::now() < deadline, "the repeated report got lost");
    }

    screen.prefix("d");
    screen.wait_for_exit();
    assert_eq!(screen.terminal.progress(), None);
}

#[test]
fn output_in_a_monitored_window_is_flagged() {
    let mut screen = Screen::new(80, 24);
    assert!(
        screen
            .command(&["set-window-option", "monitor-activity", "on"])
            .status
            .success()
    );
    screen.run("sleep 1; echo done");
    screen.prefix("c");
    screen.wait_for("the activity flag", |lines| {
        lines
            .last()
            .is_some_and(|status| status.contains("0:sleep#-"))
    });
    screen.prefix("0");
    screen.wait_for("the activity flag to clear", |lines| {
        lines.last().is_some_and(|status| !status.contains('#'))
    });
}

#[test]
fn the_window_taking_over_from_a_closed_one_loses_its_bell_flag() {
    let mut screen = Screen::new(80, 24);
    screen.run(r"sleep 1; printf '\007'");
    screen.prefix("c");
    screen.wait_for("the bell flag", |lines| {
        lines.last().is_some_and(|status| status.contains('!'))
    });
    assert!(screen.command(&["kill-window"]).status.success());
    screen.wait_for("the bell flag to clear", |lines| {
        lines.last().is_some_and(|status| !status.contains('!'))
    });
}

#[test]
fn programs_hear_about_focus_moving_between_windows() {
    let mut screen = Screen::new(80, 24);
    screen.run_setup_then_cat(r"printf '\033[?1004h'");
    screen.prefix("c");
    screen.wait_for("a second window", |lines| {
        lines.last().is_some_and(|status| status.contains(" 1:"))
    });
    screen.prefix("p");
    screen.wait_for_text("^[[O^[[I");
}

#[test]
fn programs_hear_about_focus_moving_when_a_pane_closes() {
    let mut screen = Screen::new(80, 24);
    screen.run_setup_then_cat(r"printf '\033[?1004h'");
    assert!(screen.command(&["split-window"]).status.success());
    assert!(screen.command(&["kill-pane"]).status.success());
    screen.wait_for_text("^[[O^[[I");
}

#[test]
fn forwards_clipboard_writes_to_the_outer_terminal() {
    let mut screen = Screen::new(80, 24);
    screen.run(r"printf '\033]52;c;aGVsbG8=\007'");
    assert_eq!(screen.wait_for_clipboard(), [b"hello".to_vec()]);
}

#[test]
fn programs_can_query_the_outer_colors() {
    let mut screen = Screen::new(80, 24);
    // The tty echoes the reply, which is all this needs to see.
    screen.run(r"printf '\033]11;?\033\\'");
    screen.wait_for_text("rgb:1111/2222/3333");
}

#[test]
fn programs_hear_when_the_color_scheme_changes() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a prompt in the new pane", |lines| lines[0].ends_with("│$"));
    // Listening in a pane out of sight, which still has to adapt.
    screen.run_setup_then_cat(r"printf '\033[?2031h'");
    screen.prefix("c");
    screen.wait_for("a second window", |lines| {
        lines.last().is_some_and(|status| status.contains(" 1:"))
    });

    screen.change_outer_colors(&OuterColors {
        foreground: Some(OUTER_BACKGROUND),
        background: Some(OUTER_FOREGROUND),
        dark: Some(false),
        ..OuterColors::default()
    });
    screen.prefix("0");
    screen.wait_for_text("^[[?997;2n");

    // The query is answered from the same knowledge, and the new colors are in.
    screen.prefix("o");
    screen.run(r"printf '\033[?996n\033]11;?\033\\'");
    let lines = screen.wait_for_text("rgb:eeee/dddd/cccc");
    let reply = lines.iter().find(|line| line.contains("rgb:eeee"));
    assert!(reply.unwrap().contains("^[[?997;2n"), "{reply:?}");
}

#[test]
fn programs_can_query_the_outer_palette() {
    let mut screen = Screen::new(80, 24);
    screen.run(r"printf '\033]4;1;?\033\\'");
    screen.wait_for_text("rgb:aaaa/bbbb/cccc");
}

/// Splits the window and returns with the left pane active.
fn two_panes() -> Screen {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a prompt in the new pane", |lines| lines[0].ends_with("│$"));
    screen.prefix("o");
    screen
}

#[test]
fn the_pointer_shape_goes_with_the_pane_under_it() {
    let mut screen = two_panes();
    screen.run(r"printf '\033]22;pointer\033\\'");
    // Before the pointer has been seen, the active pane is the best guess.
    screen.wait_for_pointer_shape("pointer");

    screen.mouse(35, 60, 5, true);
    screen.wait_for_pointer_shape("text");
    screen.mouse(35, 10, 5, true);
    screen.wait_for_pointer_shape("pointer");
    // Neither a border nor the status line is part of a pane.
    screen.mouse(35, 40, 5, true);
    screen.wait_for_pointer_shape("text");
    screen.mouse(35, 10, 5, true);
    screen.wait_for_pointer_shape("pointer");
    screen.mouse(35, 10, 23, true);
    screen.wait_for_pointer_shape("text");
    screen.mouse(35, 10, 5, true);
    screen.wait_for_pointer_shape("pointer");

    // A pane out of sight has no say, and comes back with its window.
    screen.prefix("c");
    screen.wait_for_pointer_shape("text");
    screen.prefix("0");
    screen.wait_for_pointer_shape("pointer");

    screen.prefix("d");
    screen.wait_for_exit();
    assert_eq!(screen.terminal.pointer_shape(), "text");
}

#[test]
fn programs_get_real_pixels_when_they_ask() {
    let mut screen = two_panes();
    // Both programs track the mouse, and only the right one wants pixels.
    screen.run(r"printf '\033[?1000h\033[?1006hrea''dy\n'; cat -v");
    screen.wait_for_text("ready");
    screen.prefix("o");
    screen.run_setup_then_cat(r"printf '\033[?1000h\033[?1016h'");
    screen.wait_for_outer_mode(1016, true);

    // The right pane starts after the border, with cells of 10 by 20 pixels.
    let border = screen.lines()[0].chars().position(|c| c == '│').unwrap();
    let right_pane = i32::try_from(border + 1).unwrap() * 10;
    screen.mouse_pixels(right_pane + 37, 125);
    screen.wait_for_text("^[[<0;37;125M");
    screen.mouse_pixels(123, 45);
    screen.wait_for_text("^[[<0;13;3M");
    // The padding around the text counts from the same place, so it is negative.
    screen.mouse_pixels(-3, 125);
    screen.wait_for_text("^[[<0;1;7M");

    // It is only on while such a program is in view.
    screen.prefix("c");
    screen.wait_for_outer_mode(1016, false);
    assert!(screen.terminal.mode(1006));
    screen.prefix("0");
    screen.wait_for_outer_mode(1016, true);

    // kl's own parts work too, also from the padding below the last row.
    let status = screen.lines().pop().unwrap();
    let label = status.find(" 1:").unwrap() + 1;
    screen.mouse_pixels(i32::try_from(label).unwrap() * 10 + 5, 500);
    screen.wait_for("the click on the second window", |lines| {
        let status = lines.last().unwrap();
        status
            .split(' ')
            .any(|word| word.starts_with("1:") && word.ends_with('*'))
    });
    screen.wait_for_outer_mode(1016, false);

    screen.prefix("d");
    screen.wait_for_exit();
    assert!(!screen.terminal.mode(1006));
}

#[test]
fn a_redefined_color_stays_in_its_pane() {
    let mut screen = two_panes();
    screen.run(r"printf '\033]4;1;rgb:12/34/56\033\\\033[31mle''fty\033[0m\n'");
    let style = screen.style_of("lefty");
    assert_eq!(style.fg.as_rgb(), Some([0x12, 0x34, 0x56]));

    screen.prefix("o");
    screen.run(r"printf '\033[31mrigh''ty\033[0m\n'");
    assert_eq!(screen.style_of("righty").fg.as_palette(), Some(1));
    assert!(!screen.raw_text().contains("\x1b]4;1;rgb"));

    screen.prefix("o");
    screen.run(r"printf '\033]104;1\033\\'");
    screen.wait_for_style("lefty", |style| style.fg.as_palette() == Some(1));
}

#[test]
fn a_changed_background_stays_in_its_pane() {
    let mut screen = two_panes();
    screen.run(r"printf '\033]11;rgb:12/34/56\033\\le''fty\n'");
    let style = screen.style_of("lefty");
    assert_eq!(style.bg.as_rgb(), Some([0x12, 0x34, 0x56]));
    screen.prefix("o");
    screen.run("echo righ''ty");
    assert_eq!(screen.style_of("righty"), Style::default());
    assert!(!screen.raw_text().contains("\x1b]11;rgb"));
}

#[test]
fn reverse_video_stays_in_its_pane() {
    let mut screen = two_panes();
    // Written first, so that only the change of mode can recolor it.
    screen.run("echo le''fty");
    assert_eq!(screen.style_of("lefty"), Style::default());
    screen.run(r"printf '\033[?5h'");
    let style = screen.wait_for_style("lefty", |style| style != Style::default());
    assert_eq!(style.fg.as_rgb(), Some([0x11, 0x22, 0x33]));
    assert_eq!(style.bg.as_rgb(), Some([0xee, 0xdd, 0xcc]));

    screen.prefix("o");
    screen.run("echo righ''ty");
    assert_eq!(screen.style_of("righty"), Style::default());

    screen.prefix("o");
    screen.run(r"printf '\033[?5l'");
    screen.wait_for_style("lefty", |style| style == Style::default());
}

#[test]
fn full_screen_programs_get_their_own_screen() {
    let mut screen = Screen::new(80, 24);
    screen.run("echo before");
    screen.wait_for_text("before");
    screen.run(r"printf '\033[?1049h\033[Halt''-screen'; read x; printf '\033[?1049l'");
    screen.wait_for_text("alt-screen");
    screen.enter();
    screen.wait_for_no_text("alt-screen");
    screen.wait_for_line("before");
}

#[test]
fn a_program_cannot_freeze_its_pane_forever() {
    let mut screen = Screen::new(80, 24);
    screen.run(r"printf '\033[?2026h'; echo held-text; sleep 100");
    screen.wait_for_text("held-text");
}

#[test]
fn copy_mode_scrolls_through_history() {
    let mut screen = Screen::new(80, 24);
    screen.run("seq 1 100");
    screen.wait_for_line("100");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.key(key::PAGE_UP, 0, "", 0);
    let lines = screen.wait_for("the view to scroll", |lines| lines[0].contains("[21/"));
    assert!(!lines.iter().any(|line| line.trim() == "100"), "{lines:?}");
    screen.type_text("q");
    screen.wait_for("the end of history", |lines| {
        lines.iter().any(|line| line.trim() == "100") && !lines[0].contains('/')
    });
}

#[test]
fn the_prefix_works_in_copy_mode() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("c");
    screen.wait_for("a second window", |lines| {
        lines.last().is_some_and(|status| status.contains(" 1:"))
    });
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.prefix("p");
    screen.wait_for("the first window", |lines| {
        let status = lines.last().unwrap();
        status
            .split(' ')
            .any(|window| window.starts_with("0:") && window.ends_with('*'))
    });
}

#[test]
fn copy_mode_goes_to_the_top_and_keys_do_not_reach_the_shell() {
    let mut screen = Screen::new(80, 24);
    screen.run("seq 1 100");
    screen.wait_for_line("100");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("g");
    screen.wait_for_text("seq 1 100");
    screen.type_text("q");
    screen.wait_for("the prompt", |lines| {
        lines.iter().any(|line| line.trim() == "$")
    });
}

#[test]
fn copies_with_the_keyboard_and_pastes() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo copy-this");
    screen.wait_for_line("copy-this");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    // From the prompt below the output: up to its line, then to its start.
    screen.type_text("0");
    screen.type_text("k");
    screen.type_text(" ");
    for _ in 0.."copy-this".len() - 1 {
        screen.type_text("l");
    }
    screen.enter();
    screen.wait_for_no_text("[0/");
    assert_eq!(screen.wait_for_clipboard(), [b"copy-this".to_vec()]);

    screen.run("cat");
    screen.prefix("]");
    screen.enter();
    screen.wait_for("the pasted line and its echo", |lines| {
        lines
            .iter()
            .filter(|line| line.trim() == "copy-this")
            .count()
            >= 3
    });
}

/// Goes into copy mode, onto the start of the line above the prompt.
fn copy_last_output(screen: &mut Screen) {
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("k0");
}

#[test]
fn copy_mode_moves_by_word() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo alpha beta.gamma delta");
    screen.wait_for_line("alpha beta.gamma delta");
    copy_last_output(&mut screen);
    // Three words, as the dot is one of its own.
    screen.type_text("w ");
    screen.type_text("eee");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"beta.gamma".to_vec()]);

    copy_last_output(&mut screen);
    screen.type_text("$bb");
    screen.type_text(" ");
    screen.type_text("W");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"gamma d".to_vec()]);

    copy_last_output(&mut screen);
    screen.type_text("W ");
    screen.type_text("E");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"beta.gamma".to_vec()]);
}

#[test]
fn copy_mode_jumps_to_characters() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo key=value,more,rest");
    screen.wait_for_line("key=value,more,rest");
    copy_last_output(&mut screen);
    screen.type_text("f=l ");
    screen.wait_for("the prompt to end", |lines| {
        !lines.iter().any(|line| line.contains("(jump"))
    });
    screen.type_text("t,");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"value".to_vec()]);

    copy_last_output(&mut screen);
    screen.type_text("f, ");
    screen.type_text(";");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b",more,".to_vec()]);

    copy_last_output(&mut screen);
    screen.type_text("$F,;");
    screen.type_text(" ");
    screen.type_text(",");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b",more,".to_vec()]);
}

#[test]
fn copy_mode_selects_rectangles() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; printf 'abcd\\nefgh\\nijkl\\n'");
    screen.wait_for_line("ijkl");
    copy_last_output(&mut screen);
    screen.type_text("kkl");
    screen.type_text("v ");
    screen.type_text("jjl");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"bc\nfg\njk".to_vec()]);
}

#[test]
fn copy_mode_selects_whole_lines() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; printf 'first\\nsecond\\nthird\\n'");
    screen.wait_for_line("third");
    copy_last_output(&mut screen);
    screen.type_text("kl");
    screen.type_text("Vk");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"first\nsecond\n".to_vec()]);
}

#[test]
fn copy_mode_goes_to_the_other_end_of_a_selection() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo one two three");
    screen.wait_for_line("one two three");
    copy_last_output(&mut screen);
    screen.type_text("w ");
    screen.type_text("eob");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"one two".to_vec()]);
}

#[test]
fn copy_mode_copies_to_the_end_of_the_line() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo one two three");
    screen.wait_for_line("one two three");
    copy_last_output(&mut screen);
    screen.type_text("wD");
    assert_eq!(screen.wait_for_clipboard(), [b"two three".to_vec()]);
    screen.wait_for_no_text("[0/");
}

#[test]
fn escape_clears_the_selection_and_stays_in_copy_mode() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo one two three");
    screen.wait_for_line("one two three");
    copy_last_output(&mut screen);
    screen.type_text(" ");
    screen.type_text("w");
    screen.key(key::ESCAPE, 0, "", 0);
    screen.type_text(" ");
    screen.type_text("e");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"two".to_vec()]);
}

#[test]
fn a_count_repeats_the_next_key_in_copy_mode() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo a b c d e f g h i j k l");
    screen.wait_for_line("a b c d e f g h i j k l");
    copy_last_output(&mut screen);
    screen.type_text("1");
    screen.wait_for_text("(repeat) 1");
    screen.type_text("0w");
    screen.type_text(" ");
    screen.type_text("$");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"k l".to_vec()]);
}

#[test]
fn copy_mode_goes_to_a_line() {
    let mut screen = Screen::new(80, 24);
    screen.run("seq 1 100");
    screen.wait_for_line("100");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text(":");
    screen.wait_for_text("(goto line)");
    screen.run("30");
    screen.wait_for("the view to scroll", |lines| lines[0].contains("[30/"));
}

#[test]
fn copy_mode_moves_within_the_view_and_by_paragraph() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; printf 'one\\ntwo\\n\\nthree\\nfour\\n\\nfive\\n'");
    screen.wait_for_line("five");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("H ");
    screen.type_text("}");
    screen.enter();
    assert_eq!(
        screen.wait_for_clipboard(),
        [b"one\ntwo\n\n".to_vec()],
        "as in tmux, a blank line at the end of a selection counts"
    );

    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("L{{");
    screen.type_text(" ");
    screen.type_text("M");
    screen.enter();
    let copied = screen.wait_for_clipboard();
    assert!(copied[0].starts_with(b"\nthree\nfour"), "{copied:?}");
}

/// The background of the first cell of some text on the screen.
fn background_of(screen: &mut Screen, text: &str) -> Option<u8> {
    screen.pump();
    screen.terminal.refresh();
    let mut background = None;
    screen.terminal.draw_rows(true, |_, cells| {
        let line: String = cells.iter().map(|cell| cell.text.as_str()).collect();
        if let Some(start) = line.find(text) {
            background = background.or(cells[start].style.bg.as_palette());
        }
    });
    background
}

#[test]
fn searches_through_history_in_both_directions() {
    let mut screen = Screen::new(80, 24);
    screen.run("seq 1 200 | sed 's/^/line /'");
    screen.wait_for_line("line 200");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("?");
    screen.wait_for_text("(search up)");
    screen.run("line 7");
    let lines = screen.wait_for("the search to go up", |lines| {
        lines[0].contains("(11 results)") && lines.iter().any(|line| line.trim() == "line 79")
    });
    assert!(
        lines.iter().any(|line| line.trim() == "line 70"),
        "{lines:?}"
    );

    // The cursor is on what it found, for copying from there.
    screen.type_text(" ");
    screen.type_text("$");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"line 79".to_vec()]);

    screen.prefix("[");
    screen.wait_for_no_text("(11 results)");
    screen.type_text("g/");
    screen.wait_for_text("(search down)");
    screen.run("line 7");
    screen.wait_for_text("(11 results)");
    screen.type_text("n ");
    screen.type_text("$");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"line 70".to_vec()]);
}

#[test]
fn searching_goes_around_the_ends_of_the_screen() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; printf 'one target\\ntwo target\\n'");
    screen.wait_for_line("two target");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("/");
    screen.run("target");
    screen.wait_for_text("(2 results)");
    // Down from the prompt there is nothing, so around to the first one. On to
    // the second, down around to the first again, and up around to the second.
    screen.type_text("nnN0 ");
    screen.type_text("$");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"two target".to_vec()]);
}

#[test]
fn search_highlights_matches_and_the_one_under_the_cursor() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo first-mark; echo second-mark");
    screen.wait_for_line("second-mark");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("?");
    screen.run("-mark");
    screen.wait_for_text("(2 results)");
    let cyan = 6;
    let magenta = 5;
    let start = std::time::Instant::now();
    while background_of(&mut screen, "-mark") != Some(cyan)
        && start.elapsed() < Duration::from_secs(5)
    {
        screen.pump();
    }
    assert_eq!(background_of(&mut screen, "-mark"), Some(cyan));
    let lines = screen.lines();
    let row = lines.iter().position(|line| line == "second-mark").unwrap();
    let mut colors = Vec::new();
    screen.terminal.draw_rows(true, |y, cells| {
        if usize::from(y) == row {
            colors = cells
                .iter()
                .map(|cell| cell.style.bg.as_palette())
                .collect();
        }
    });
    assert_eq!(colors[5], None, "the rest of the line stays as it is");
    assert_eq!(colors[6], Some(magenta), "the cursor is on this match");

    screen.type_text("q");
    screen.wait_for_no_text("results)");
    assert_eq!(background_of(&mut screen, "-mark"), None);
}

#[test]
fn copying_without_a_selection_takes_the_match_under_the_cursor() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo some words here");
    screen.wait_for_line("some words here");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("?");
    screen.run("words");
    screen.wait_for_text("(1 results)");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"words".to_vec()]);
}

#[test]
fn star_searches_for_the_word_under_the_cursor() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; printf 'alpha beta\\ngamma alpha\\n'");
    screen.wait_for_line("gamma alpha");
    copy_last_output(&mut screen);
    screen.type_text("w*");
    screen.wait_for_text("(2 results)");
    screen.type_text(" ");
    screen.type_text("$");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"alpha beta".to_vec()]);
}

#[test]
fn n_goes_on_with_the_last_search_in_a_later_copy_mode() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; printf 'mark one\\nmark two\\n'");
    screen.wait_for_line("mark two");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("?");
    screen.run("mark");
    screen.wait_for_text("results)");
    screen.type_text("q");
    screen.wait_for_no_text("[0/");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.type_text("n");
    screen.wait_for_text("results)");
    screen.enter();
    assert_eq!(screen.wait_for_clipboard(), [b"mark".to_vec()]);
}

#[test]
fn pastes_from_the_outer_terminal_are_bracketed_for_those_who_ask() {
    let mut screen = Screen::new(80, 24);
    screen.run_setup_then_cat(r"printf '\033[?2004h'");
    screen.send_bytes(b"\x1b[200~pasted\x1b[201~");
    screen.enter();
    screen.wait_for_text("^[[200~pasted^[[201~");
}

#[test]
fn clicking_a_pane_selects_it() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.click(5, 5);
    screen.run("echo clicked");
    let lines = screen.wait_for_text("echo clicked");
    let row = lines
        .iter()
        .find(|line| line.contains("echo clicked"))
        .unwrap();
    assert!(row.find("echo clicked").unwrap() < 39, "{row}");
}

#[test]
fn clicking_the_status_line_selects_a_window() {
    let mut screen = Screen::new(80, 24);
    screen.run("echo in-zero");
    screen.wait_for_text("in-zero");
    screen.prefix("c");
    let lines = screen.wait_for("a second window", |lines| {
        lines.last().is_some_and(|status| status.contains(" 1:"))
    });
    let status = lines.last().unwrap();
    let x = status.find("0:").unwrap();
    screen.click(u16::try_from(x).unwrap(), 23);
    screen.wait_for_text("in-zero");
}

#[test]
fn dragging_a_border_resizes_panes() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("a border", |lines| lines[5].contains('│'));
    screen.mouse(0, 40, 5, true);
    screen.mouse(32, 30, 5, true);
    screen.mouse(32, 20, 5, true);
    screen.mouse(0, 20, 5, false);
    screen.wait_for("the border to move", |lines| {
        lines[5].chars().nth(20) == Some('│')
    });
    screen.run("stty size");
    screen.wait_for_text("23 59");
}

#[test]
fn dragging_selects_and_copies_text() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo drag-me-please");
    let y = screen.wait_for_line("drag-me-please");
    screen.mouse(0, 0, y, true);
    screen.mouse(32, 3, y, true);
    screen.mouse(32, 6, y, true);
    screen.mouse(0, 6, y, false);
    assert_eq!(screen.wait_for_clipboard(), [b"drag-me".to_vec()]);
}

#[test]
fn dragging_past_the_top_of_a_pane_scrolls_and_selects() {
    let mut screen = Screen::new(80, 24);
    screen.run("seq 1 100");
    let y = screen.wait_for_line("100");
    screen.mouse(0, 2, y, true);
    screen.mouse(32, 0, 0, true);
    screen.wait_for("the view to scroll", |lines| lines[0].contains("[5/"));
    screen.mouse(0, 0, 0, false);
    let copied = screen.wait_for_clipboard();
    let text = String::from_utf8_lossy(&copied[0]);
    let first: u32 = text.lines().next().unwrap().parse().unwrap();
    assert!(first < 78, "{text}");
    assert!(text.ends_with("100"), "{text}");
    screen.wait_for("the end of history", |lines| {
        lines.iter().any(|line| line.trim() == "100") && !lines[0].contains('/')
    });
}

#[test]
fn dragging_past_the_bottom_of_a_pane_scrolls_back_down() {
    let mut screen = Screen::new(80, 24);
    screen.run("seq 1 100");
    screen.wait_for_line("100");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.key(key::PAGE_UP, 0, "", 0);
    screen.key(key::PAGE_UP, 0, "", 0);
    screen.wait_for("the view to scroll", |lines| lines[0].contains("[42/"));
    screen.mouse(0, 0, 10, true);
    // The status line is below the pane.
    screen.mouse(32, 0, 23, true);
    screen.wait_for("the view to scroll", |lines| lines[0].contains("[37/"));
    screen.mouse(0, 0, 23, false);
    let copied = screen.wait_for_clipboard();
    let text = String::from_utf8_lossy(&copied[0]);
    assert!(
        text.lines().count() > 15,
        "the rows that came into view count: {text}"
    );
}

#[test]
fn moving_along_the_top_row_while_dragging_does_not_scroll() {
    let mut screen = Screen::new(80, 24);
    screen.run("seq 1 100");
    let lines = screen.wait_for("the numbers", |lines| lines[22].trim() == "$");
    let top = lines[0].trim().to_owned();
    screen.mouse(0, 0, 0, true);
    screen.mouse(32, 1, 0, true);
    screen.wait_for_text("[0/");
    std::thread::sleep(Duration::from_millis(120));
    screen.mouse(0, 1, 0, false);
    assert_eq!(screen.wait_for_clipboard(), [top.as_bytes()[..2].to_vec()]);
}

#[test]
fn a_click_in_copy_mode_stays_in_copy_mode() {
    let mut screen = Screen::new(80, 24);
    screen.run("seq 1 100");
    screen.wait_for_line("100");
    screen.prefix("[");
    screen.wait_for_text("[0/");
    screen.click(3, 3);
    screen.type_text("k");
    screen.wait_for_text("[0/");
    screen.type_text("q");
    screen.wait_for_no_text("[0/");
}

#[test]
fn dragging_after_a_double_click_selects_whole_words() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo one wordy three four");
    let y = screen.wait_for_line("one wordy three four");
    screen.click(6, y);
    screen.mouse(0, 6, y, true);
    screen.mouse(32, 11, y, true);
    screen.mouse(0, 11, y, false);
    assert_eq!(screen.wait_for_clipboard(), [b"wordy three".to_vec()]);
}

#[test]
fn triple_click_selects_a_line() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo one wordy three");
    let y = screen.wait_for_line("one wordy three");
    screen.click(6, y);
    screen.click(6, y);
    assert_eq!(screen.wait_for_clipboard(), [b"wordy".to_vec()]);
    screen.click(6, y);
    assert_eq!(screen.wait_for_clipboard(), [b"one wordy three".to_vec()]);
}

#[test]
fn double_click_selects_a_word() {
    let mut screen = Screen::new(80, 24);
    screen.run("clear; echo one wordy three");
    let y = screen.wait_for_line("one wordy three");
    screen.click(6, y);
    screen.click(6, y);
    assert_eq!(screen.wait_for_clipboard(), [b"wordy".to_vec()]);
}

#[test]
fn the_wheel_scrolls_history_and_returns() {
    let mut screen = Screen::new(80, 24);
    screen.run("seq 1 100");
    screen.wait_for_line("100");
    screen.mouse(64, 5, 5, true);
    screen.wait_for("the position indicator", |lines| lines[0].contains("[3/"));
    screen.mouse(65, 5, 5, true);
    screen.wait_for("copy mode to end", |lines| !lines[0].contains('/'));
}

#[test]
fn programs_that_track_the_mouse_get_its_events() {
    let mut screen = Screen::new(80, 24);
    screen.prefix("%");
    screen.wait_for("the new pane's prompt", |lines| lines[0].ends_with("│$"));
    screen.run_setup_then_cat(r"printf '\033[?1000h\033[?1006h'");
    // Inside the right pane, which starts at column 41.
    screen.mouse(0, 43, 3, true);
    screen.mouse(0, 43, 3, false);
    screen.enter();
    screen.wait_for_text("^[[<0;3;4M");
}

#[test]
fn the_wheel_scrolls_full_screen_programs_with_arrow_keys() {
    let mut screen = Screen::new(80, 24);
    screen.run_setup_then_cat(r"printf '\033[?1049h'");
    screen.mouse(65, 5, 5, true);
    screen.enter();
    screen.wait_for_text("^[[B^[[B^[[B");
}

#[test]
fn links_reach_the_outer_terminal() {
    let mut screen = Screen::new(80, 24);
    screen.run(&format!(
        "clear; {}; echo ' and'; {}; echo",
        print_link("https://a.example/?b=1;c=2", "first"),
        print_link("file:///etc/hosts", "second"),
    ));
    screen.wait_for_links(&[
        ("first", "https://a.example/?b=1;c=2"),
        ("second", "file:///etc/hosts"),
    ]);
    screen.wait_for_text("first and");
}

#[test]
fn no_link_stays_open_past_a_frame() {
    let mut screen = Screen::new(80, 24);
    // The link runs to the end of a row, right up against the status line.
    screen.run(&format!(
        "clear; printf '\\033[23;71H'; {}; read x",
        print_link("https://a.example", "0123456789"),
    ));
    screen.wait_for_links(&[("0123456789", "https://a.example")]);
    screen.prefix("c");
    screen.wait_for_links(&[]);
    screen.prefix("p");
    screen.wait_for_links(&[("0123456789", "https://a.example")]);

    let text = screen.raw_text();
    for frame in text.split("\x1b[?2026l") {
        if let Some((_, last)) = frame.rsplit_once("\x1b]8;") {
            assert!(last.starts_with(";\x1b\\"), "open at the end of {frame:?}");
        }
    }
}

#[test]
fn a_link_redrawn_in_part_stays_one_link() {
    let mut screen = Screen::new(80, 24);
    let uri = "https://a.example";
    screen.run(&format!(
        r"clear; {}; read x; printf '\033[1;3H'; {}; read x",
        print_link(uri, "abcdef"),
        print_link(uri, "X"),
    ));
    screen.wait_for_links(&[("abcdef", uri)]);
    screen.enter();
    screen.wait_for_links(&[("abXdef", uri)]);

    let ids = screen.link_ids(uri);
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_eq!(ids[0], ids[1]);
    assert!(ids[0].starts_with("kl"), "{ids:?}");
}

#[test]
fn panes_keep_their_links_apart() {
    let mut screen = Screen::new(80, 24);
    let (shared, own) = ("https://shared.example", "https://own.example");
    screen.run(&format!("clear; {}; echo", print_link(shared, "left")));
    screen.wait_for_links(&[("left", shared)]);
    screen.prefix("%");
    screen.wait_for("the new pane's prompt", |lines| lines[0].contains("│$"));
    screen.run(&format!(
        "clear; {}; {}; echo",
        print_link(shared, "right"),
        print_link(own, "more")
    ));
    screen.wait_for_links(&[("left", shared), ("right", shared), ("more", own)]);

    // The same URI in two panes is not the same link.
    let mut ids = screen.link_ids(shared);
    ids.dedup();
    assert_eq!(ids.len(), 2, "{ids:?}");

    // With the right pane gone, its links go as well.
    screen.prefix("x");
    screen.wait_for_text("kill-pane");
    screen.type_text("y");
    screen.wait_for_links(&[("left", shared)]);
}

#[test]
fn links_cover_all_of_a_wide_character() {
    let mut screen = Screen::new(80, 24);
    let uri = "https://a.example";
    screen.run(&format!(
        "clear; printf 'x'; {}; echo y",
        print_link(uri, "\u{4e16}\u{754c}")
    ));
    screen.wait_for_links(&[("\u{4e16}\u{754c}", uri)]);
    screen.wait_for_text("x\u{4e16}\u{754c}y");
}

#[test]
fn links_move_with_their_text() {
    let mut screen = Screen::new(80, 24);
    let uri = "https://a.example";
    screen.run(&format!(
        r"clear; printf '\033[10;1H'; {}; echo",
        print_link(uri, "link")
    ));
    screen.wait_for_links(&[("link", uri)]);

    screen.run("seq 1 15");
    screen.wait_for_line("15");
    let lines = screen.lines();
    assert_eq!(lines[5], "link", "{lines:?}");
    screen.wait_for_links(&[("link", uri)]);

    screen.run("seq 1 40");
    screen.wait_for_line("40");
    screen.wait_for_links(&[]);
}

#[test]
fn copy_mode_shows_the_links_in_history() {
    let mut screen = Screen::new(80, 24);
    let uri = "https://a.example";
    screen.run(&format!("{}; echo; seq 1 100", print_link(uri, "link")));
    screen.wait_for_line("100");
    screen.wait_for_links(&[]);

    screen.prefix("[");
    screen.wait_for("the position indicator", |lines| lines[0].contains("[0/"));
    screen.type_text("g");
    screen.wait_for_text("link");
    screen.wait_for_links(&[("link", uri)]);

    screen.type_text("q");
    screen.wait_for_line("100");
    screen.wait_for_links(&[]);
}

#[test]
fn reattaching_brings_links_back() {
    let mut screen = Screen::new(80, 24);
    let uri = "https://a.example";
    screen.run(&format!("{}; echo", print_link(uri, "link")));
    screen.wait_for_links(&[("link", uri)]);
    let ids = screen.link_ids(uri);
    screen.prefix("d");
    screen.wait_for_exit();

    let Screen { dir, .. } = screen;
    let mut screen = Screen::attach(dir, 80, 24);
    screen.wait_for_links(&[("link", uri)]);
    assert_eq!(screen.link_ids(uri), ids);
}

#[test]
fn a_restart_brings_links_back() {
    let mut screen = Screen::new(80, 24);
    let uri = "https://a.example";
    screen.run(&format!("{}; echo", print_link(uri, "link")));
    screen.wait_for_links(&[("link", uri)]);
    screen.restart_server();
    screen.wait_for_links(&[("link", uri)]);
}

/// A URI read from the live terminal while drawing the frame from before a hold
/// would pair old text with a new URI.
#[test]
fn a_held_screen_keeps_old_text_with_its_old_link() {
    let mut screen = Screen::new(80, 24);
    let (old, new) = ("https://old.example", "https://new.example");
    screen.run(&format!(
        r"clear; {}; read x; printf '\033[?2026h\033[1;1H'; {}; read x; printf '\033[?2026l'; read x",
        print_link(old, "aaa"),
        print_link(new, "bbb"),
    ));
    screen.wait_for_links(&[("aaa", old)]);
    screen.enter();
    // Going to another window and back draws the whole pane again, from what
    // the render state held on to.
    screen.prefix("c");
    screen.wait_for_no_text("aaa");
    screen.prefix("p");
    let pair = |links: &[(String, String)]| {
        matches!(
            links,
            [(text, uri)] if (text == "aaa" && uri == old) || (text == "bbb" && uri == new)
        )
    };
    let deadline = Instant::now() + TIMEOUT;
    let mut links = screen.links();
    while links.is_empty() {
        assert!(Instant::now() < deadline, "the link did not come back");
        links = screen.links();
    }
    assert!(pair(&links), "{links:?}");
    screen.enter();
    screen.wait_for_links(&[("bbb", new)]);
}

/// A pane takes URIs that are too long for the outer terminal once they have an
/// `id`, and the outer terminal ignores those.
#[test]
fn a_link_refused_by_the_outer_terminal_does_not_take_on_the_last_one() {
    let mut screen = Screen::new(80, 24);
    screen.run(&format!(
        r"u=$(printf '%2035s' | tr ' ' x); clear; {}; printf '\033]8;;https://b/'$u'\033\\b\033]8;;\033\\\n'",
        print_link("https://a.example", "a"),
    ));
    screen.wait_for_text("ab");
    screen.wait_for_links(&[("a", "https://a.example")]);
}

#[test]
fn a_uri_cannot_write_to_the_outer_terminal() {
    let mut screen = Screen::new(80, 24);
    // C1 ST as UTF-8, a raw byte that is one in some terminals, and a DEL.
    screen.run(&format!(
        "clear; {}; echo",
        print_link(r"https://a.example/\302\234\234\177", "link")
    ));
    screen.wait_for_links(&[("link", "https://a.example/%C2%9C%9C%7F")]);
}
