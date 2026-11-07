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

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ghostty::key::{self, MOD_ALT, MOD_CTRL, MOD_SHIFT};
use ghostty::{KeyAction, KeyInput, Notification, OuterColors, Progress, Rgb, Terminal, Width};
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

    fn wait_for_no_text(&mut self, text: &str) -> Vec<String> {
        self.wait_for(&format!("{text} to go away"), |lines| {
            !lines.iter().any(|line| line.contains(text))
        })
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        self.pty.write_all(bytes).unwrap();
    }

    /// Presses and releases a key, as the outer terminal would report it.
    fn key(&mut self, key: i32, mods: u16, text: &str, unshifted: u32) {
        for action in [KeyAction::Press, KeyAction::Release] {
            let input = KeyInput {
                action,
                key,
                mods,
                consumed_mods: if text.is_empty() { 0 } else { mods & MOD_SHIFT },
                text: if action == KeyAction::Release {
                    ""
                } else {
                    text
                },
                unshifted,
            };
            let bytes = self.terminal.encode_key(&input);
            self.send_bytes(&bytes);
        }
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
        self.key(ghostty_key(c), MOD_CTRL, "", u32::from(c));
    }

    /// The prefix followed by a key.
    fn prefix(&mut self, text: &str) {
        self.ctrl('b');
        self.type_text(text);
    }

    fn command(&self, args: &[&str]) -> Output {
        kl_command(&self.dir.0).args(args).output().unwrap()
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
        Some(39),
        "{lines:?}"
    );

    screen.run("echo second");
    let lines = screen.wait_for_text("second");
    let row = lines
        .iter()
        .find(|line| line.contains("echo second"))
        .unwrap();
    assert!(row.find("echo second").unwrap() > 39, "{row}");

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
    screen.wait_for_text("kill-pane?");
    screen.type_text("n");
    screen.wait_for_no_text("kill-pane?");
    assert!(screen.lines()[5].contains('│'));
    screen.prefix("x");
    screen.wait_for_text("kill-pane?");
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
        "\x1b[?2004l",
        "\x1b[?25h",
    ] {
        assert!(raw.contains(sequence), "{sequence:?} missing");
    }
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
        lines[5].chars().nth(59) == Some('│')
    });
    screen.run("stty size");
    screen.wait_for_text("39 60");
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
    screen.run(r"printf '\033]11;?\033\\'; cat -v");
    screen.enter();
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
    });
    screen.prefix("0");
    screen.wait_for_text("^[[?997;2n");

    // The query is answered from the same knowledge, and the new colors are in.
    screen.prefix("o");
    screen.run(r"printf '\033[?996n\033]11;?\033\\'; cat -v");
    screen.enter();
    let lines = screen.wait_for_text("rgb:eeee/dddd/cccc");
    assert!(lines.iter().any(|line| line.contains("^[[?997;2n")));
}

#[test]
fn full_screen_programs_get_their_own_screen() {
    let mut screen = Screen::new(80, 24);
    screen.run("echo before");
    screen.wait_for_text("before");
    screen.run(r"printf '\033[?1049h\033[Halt''-screen'; sleep 0.3; printf '\033[?1049l'");
    screen.wait_for_text("alt-screen");
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
    let lines = screen.wait_for("the view to scroll", |lines| lines[0].contains("[23/"));
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
    screen.type_text("v");
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
    screen.mouse(0, 39, 5, true);
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
    // Inside the right pane, which starts at column 40.
    screen.mouse(0, 42, 3, true);
    screen.mouse(0, 42, 3, false);
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
