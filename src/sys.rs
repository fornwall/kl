//! Thin safe wrappers over the libc calls that std does not cover.

use std::ffi::{CStr, c_int};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

pub use libc::{SIGCHLD, SIGHUP, SIGINT, SIGTERM, SIGWINCH};

fn check(result: c_int) -> io::Result<c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// Size of a terminal in cells and, if the terminal reports it, pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u16,
    pub height_px: u16,
}

impl WinSize {
    const fn to_libc(self) -> libc::winsize {
        libc::winsize {
            ws_row: self.rows,
            ws_col: self.cols,
            ws_xpixel: self.width_px,
            ws_ypixel: self.height_px,
        }
    }
}

/// # Errors
///
/// Fails if `fd` is not a terminal.
pub fn winsize(fd: BorrowedFd<'_>) -> io::Result<WinSize> {
    let mut size = MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: TIOCGWINSZ writes a `winsize` to the valid pointer it gets.
    check(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) })?;
    // SAFETY: Zero-initialized above, and every bit pattern is a valid `winsize`.
    let size = unsafe { size.assume_init() };
    Ok(WinSize {
        cols: size.ws_col,
        rows: size.ws_row,
        width_px: size.ws_xpixel,
        height_px: size.ws_ypixel,
    })
}

/// # Errors
///
/// Fails if `fd` is not a terminal.
pub fn set_winsize(fd: BorrowedFd<'_>, size: WinSize) -> io::Result<()> {
    let size = size.to_libc();
    // SAFETY: TIOCSWINSZ reads a `winsize` from the valid pointer it gets.
    check(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) }).map(drop)
}

/// # Errors
///
/// Fails if `fd` is not open.
pub fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: F_GETFL takes no argument and does not touch memory.
    let flags = check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })?;
    // SAFETY: F_SETFL takes an integer and does not touch memory.
    check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }).map(drop)
}

fn set_cloexec(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: F_SETFD takes an integer and does not touch memory.
    check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) }).map(drop)
}

/// Opens a pty, returning its master and slave sides.
fn open_pty(size: WinSize) -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (-1, -1);
    let mut size = size.to_libc();
    // SAFETY: The out pointers are valid, a null name and termios are allowed,
    // and the `winsize` is only read. The mut cast is for platforms whose
    // declaration lacks `const`.
    check(unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            ptr::null_mut(),
            ptr::null_mut(),
            (&raw mut size).cast(),
        )
    })?;
    // SAFETY: openpty succeeded, so `master` is a new fd that nothing else owns.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: As above, for `slave`.
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    set_cloexec(master.as_fd())?;
    set_cloexec(slave.as_fd())?;
    Ok((master, slave))
}

/// `TIOCSCTTY`, in the type `ioctl` takes, which is wider on macOS.
#[cfg(target_os = "linux")]
const SET_CONTROLLING_TTY: libc::Ioctl = libc::TIOCSCTTY;
#[cfg(target_os = "macos")]
const SET_CONTROLLING_TTY: libc::c_ulong = libc::TIOCSCTTY as libc::c_ulong;

/// Makes the child a session leader with the pty as its controlling terminal.
///
/// Runs in the child between fork and exec, where only async-signal-safe calls
/// are allowed, which `setsid` and `ioctl` both are.
fn become_session_leader() -> io::Result<()> {
    // SAFETY: Takes no arguments and does not touch memory.
    check(unsafe { libc::setsid() })?;
    // SAFETY: stdin is the pty slave at this point. TIOCSCTTY takes an integer.
    check(unsafe { libc::ioctl(libc::STDIN_FILENO, SET_CONTROLLING_TTY, 0) }).map(drop)
}

/// Spawns `command` on a new pty, returning the master side and the child.
///
/// # Errors
///
/// Fails if no pty is available or the command cannot be started.
pub fn spawn_on_pty(mut command: Command, size: WinSize) -> io::Result<(File, Child)> {
    let (master, slave) = open_pty(size)?;
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: The hook only makes async-signal-safe calls.
    let child = unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, become_session_leader)
    }
    .spawn()?;
    set_nonblocking(master.as_fd())?;
    Ok((File::from(master), child))
}

/// Starts a program detached from the terminal, in a session of its own.
///
/// # Errors
///
/// Fails if the command cannot be started.
pub fn spawn_daemon(mut command: Command) -> io::Result<Child> {
    fn detach() -> io::Result<()> {
        // SAFETY: Takes no arguments and does not touch memory.
        check(unsafe { libc::setsid() }).map(drop)
    }
    // SAFETY: The hook only makes an async-signal-safe call.
    unsafe { std::os::unix::process::CommandExt::pre_exec(&mut command, detach) }.spawn()
}

/// Puts a terminal in raw mode until dropped.
pub struct RawMode<'a> {
    fd: BorrowedFd<'a>,
    saved: libc::termios,
}

impl<'a> RawMode<'a> {
    /// # Errors
    ///
    /// Fails if `fd` is not a terminal.
    pub fn enter(fd: BorrowedFd<'a>) -> io::Result<Self> {
        let mut termios = MaybeUninit::<libc::termios>::zeroed();
        // SAFETY: Writes a `termios` to the valid pointer it gets.
        check(unsafe { libc::tcgetattr(fd.as_raw_fd(), termios.as_mut_ptr()) })?;
        // SAFETY: Filled in above, and it was zero-initialized regardless.
        let saved = unsafe { termios.assume_init() };
        let mut raw = saved;
        // SAFETY: Only modifies the valid `termios` it gets.
        unsafe { libc::cfmakeraw(&raw mut raw) };
        // SAFETY: Reads the valid `termios` it gets.
        check(unsafe { libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, &raw const raw) })?;
        Ok(Self { fd, saved })
    }
}

impl Drop for RawMode<'_> {
    fn drop(&mut self) {
        // SAFETY: Reads the valid `termios` it gets. A failure here means the
        // terminal is gone, which leaves nothing to restore.
        let result =
            unsafe { libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSANOW, &raw const self.saved) };
        if result < 0 {
            eprintln!("restoring the terminal: {}", io::Error::last_os_error());
        }
    }
}

/// The write end of the signal pipe, for the handler to reach.
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    // Signal numbers are small, so this cannot truncate.
    let byte = signal.to_ne_bytes()[0];
    // SAFETY: `write` is async-signal-safe and `byte` is a valid local. If the
    // pipe is full, a wakeup is already pending, so a failure can be ignored.
    let result = unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
    std::hint::black_box(result);
}

/// Delivers the given signals as bytes on the returned pipe, so that the event
/// loop can wait for them along with everything else.
///
/// # Errors
///
/// Fails if the pipe cannot be created or a handler cannot be installed.
pub fn watch_signals(signals: &[c_int]) -> io::Result<File> {
    let (reader, writer) = io::pipe()?;
    let reader = OwnedFd::from(reader);
    let writer = OwnedFd::from(writer);
    set_nonblocking(reader.as_fd())?;
    set_nonblocking(writer.as_fd())?;
    // The handler uses this for the rest of the process's life.
    SIGNAL_PIPE.store(writer.into_raw_fd(), Ordering::Relaxed);

    for &signal in signals {
        // SAFETY: Zero is a valid value for every field of this plain C struct.
        let mut action = unsafe { MaybeUninit::<libc::sigaction>::zeroed().assume_init() };
        // SAFETY: Fills in the valid mask it gets.
        check(unsafe { libc::sigemptyset(&raw mut action.sa_mask) })?;
        action.sa_sigaction = on_signal as extern "C" fn(c_int) as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: `action` is valid, and the handler is async-signal-safe.
        check(unsafe { libc::sigaction(signal, &raw const action, ptr::null_mut()) })?;
    }
    Ok(File::from(reader))
}

/// Reads all pending signal numbers from a pipe made by [`watch_signals`].
pub fn pending_signals(mut pipe: &File) -> Vec<c_int> {
    let mut buf = [0_u8; 64];
    pipe.read(&mut buf).map_or_else(
        |_| Vec::new(),
        |len| {
            let signals = buf.get(..len).unwrap_or(&[]);
            signals.iter().map(|byte| c_int::from(*byte)).collect()
        },
    )
}

/// A set of file descriptors to wait on, using `select`.
///
/// `poll` would avoid the limit on descriptor numbers, but it does not work
/// reliably on ttys on macOS, and using one mechanism everywhere means the code
/// path tested on Linux is the one that runs on macOS.
pub struct Selector {
    read: libc::fd_set,
    write: libc::fd_set,
    max: RawFd,
}

fn empty_fd_set() -> libc::fd_set {
    let mut set = MaybeUninit::<libc::fd_set>::zeroed();
    // SAFETY: Initializes the valid set it gets.
    unsafe { libc::FD_ZERO(set.as_mut_ptr()) };
    // SAFETY: Initialized by FD_ZERO above.
    unsafe { set.assume_init() }
}

/// Whether `select` can handle this descriptor at all.
#[must_use]
pub fn selectable(fd: RawFd) -> bool {
    usize::try_from(fd).is_ok_and(|fd| fd < libc::FD_SETSIZE)
}

impl Selector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            read: empty_fd_set(),
            write: empty_fd_set(),
            max: -1,
        }
    }

    pub fn want_read(&mut self, fd: RawFd) {
        if selectable(fd) {
            // SAFETY: `fd` is within the bounds of the set, as checked above.
            unsafe { libc::FD_SET(fd, &raw mut self.read) };
            self.max = self.max.max(fd);
        }
    }

    pub fn want_write(&mut self, fd: RawFd) {
        if selectable(fd) {
            // SAFETY: `fd` is within the bounds of the set, as checked above.
            unsafe { libc::FD_SET(fd, &raw mut self.write) };
            self.max = self.max.max(fd);
        }
    }

    /// Blocks until something is ready or the timeout passes. An interrupting
    /// signal counts as waking up, since its handler made the pipe readable.
    ///
    /// # Errors
    ///
    /// Fails if a descriptor in the set is invalid.
    #[cfg_attr(
        target_os = "linux",
        expect(
            clippy::unnecessary_fallible_conversions,
            reason = "suseconds_t is narrower on macOS"
        )
    )]
    pub fn wait(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let mut time = timeout.map(|timeout| libc::timeval {
            tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_usec: libc::suseconds_t::try_from(timeout.subsec_micros()).unwrap_or(0),
        });
        let time_ptr = time.as_mut().map_or(ptr::null_mut(), ptr::from_mut);
        // SAFETY: The sets and the timeout are valid for the call.
        let result = unsafe {
            libc::select(
                self.max + 1,
                &raw mut self.read,
                &raw mut self.write,
                ptr::null_mut(),
                time_ptr,
            )
        };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            self.read = empty_fd_set();
            self.write = empty_fd_set();
            return Ok(());
        }
        Err(error)
    }

    #[must_use]
    pub fn readable(&self, fd: RawFd) -> bool {
        // SAFETY: The set is valid, and it is only read.
        selectable(fd) && unsafe { libc::FD_ISSET(fd, &raw const self.read) }
    }

    #[must_use]
    pub fn writable(&self, fd: RawFd) -> bool {
        // SAFETY: The set is valid, and it is only read.
        selectable(fd) && unsafe { libc::FD_ISSET(fd, &raw const self.write) }
    }
}

#[must_use]
pub fn user_id() -> u32 {
    // SAFETY: Takes no arguments, and it cannot fail.
    unsafe { libc::getuid() }
}

/// The process group in the foreground of a pty, such as an editor that the
/// shell started.
fn foreground_group(master: BorrowedFd<'_>) -> Option<libc::pid_t> {
    // SAFETY: Takes a descriptor and does not touch memory.
    let group = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
    (group > 0).then_some(group)
}

/// The name of the program in the foreground of a pty.
#[must_use]
#[cfg(target_os = "linux")]
pub fn foreground_name(master: BorrowedFd<'_>) -> Option<String> {
    let group = foreground_group(master)?;
    let name = std::fs::read_to_string(format!("/proc/{group}/comm")).ok()?;
    Some(name.trim_end().to_owned())
}

/// The name of the program in the foreground of a pty.
#[must_use]
#[cfg(target_os = "macos")]
pub fn foreground_name(master: BorrowedFd<'_>) -> Option<String> {
    let group = foreground_group(master)?;
    let mut name = [0_u8; 256];
    let len = u32::try_from(name.len()).ok()?;
    // SAFETY: Writes at most `len` bytes to the valid buffer it gets.
    let written = unsafe { libc::proc_name(group, name.as_mut_ptr().cast(), len) };
    let written = usize::try_from(written).ok()?;
    let name = name.get(..written).filter(|name| !name.is_empty())?;
    Some(String::from_utf8_lossy(name).into_owned())
}

/// The working directory of the program in the foreground of a pty.
#[must_use]
#[cfg(target_os = "linux")]
pub fn foreground_cwd(master: BorrowedFd<'_>) -> Option<PathBuf> {
    let group = foreground_group(master)?;
    std::fs::read_link(format!("/proc/{group}/cwd")).ok()
}

/// macOS needs `proc_pidinfo` for this, so until then new panes there rely on
/// the directory that shell integration reports.
#[must_use]
#[cfg(target_os = "macos")]
pub const fn foreground_cwd(_master: BorrowedFd<'_>) -> Option<PathBuf> {
    None
}

/// The current local time, as `strftime` formats it.
#[must_use]
pub fn local_time(format: &CStr) -> String {
    // SAFETY: A null argument asks only for the return value.
    let now = unsafe { libc::time(ptr::null_mut()) };
    let mut parts = MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: Both pointers are valid. The `_r` form is the thread-safe one.
    if unsafe { libc::localtime_r(&raw const now, parts.as_mut_ptr()) }.is_null() {
        return String::new();
    }
    // SAFETY: Filled in by the successful call above.
    let parts = unsafe { parts.assume_init() };
    let mut text = [0_u8; 64];
    // SAFETY: Writes at most `len` bytes, and all pointers are valid.
    let len = unsafe {
        libc::strftime(
            text.as_mut_ptr().cast(),
            text.len(),
            format.as_ptr(),
            &raw const parts,
        )
    };
    String::from_utf8_lossy(text.get(..len).unwrap_or(&[])).into_owned()
}

/// Writes as much as the descriptor takes without blocking, removing what was
/// written from the front of `pending`.
///
/// # Errors
///
/// Fails if the other side is gone.
pub fn flush_some(mut output: impl Write, pending: &mut Vec<u8>) -> io::Result<()> {
    let mut written = 0;
    let result = loop {
        match output.write(pending.get(written..).unwrap_or(&[])) {
            Ok(0) => break Ok(()),
            Ok(len) => {
                written += len;
                if written == pending.len() {
                    break Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => break Err(error),
        }
    };
    pending.drain(..written);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_reports_ready_descriptors() {
        let (reader, mut writer) = io::pipe().unwrap();
        let fd = reader.as_raw_fd();

        let mut selector = Selector::new();
        selector.want_read(fd);
        selector.wait(Some(Duration::ZERO)).unwrap();
        assert!(!selector.readable(fd));

        writer.write_all(b"x").unwrap();
        let mut selector = Selector::new();
        selector.want_read(fd);
        selector.wait(Some(Duration::from_secs(5))).unwrap();
        assert!(selector.readable(fd));
    }

    #[test]
    fn selector_ignores_descriptors_it_cannot_represent() {
        let mut selector = Selector::new();
        selector.want_read(100_000);
        assert!(!selector.readable(100_000));
        assert!(!selectable(-1));
    }

    #[test]
    fn spawns_on_a_pty_with_the_requested_size() {
        let mut command = Command::new("sh");
        command.args(["-c", "stty size; tty -s && echo isatty"]);
        let size = WinSize {
            cols: 33,
            rows: 11,
            ..WinSize::default()
        };
        let (mut master, mut child) = spawn_on_pty(command, size).unwrap();
        child.wait().unwrap();
        let mut output = Vec::new();
        let mut buf = [0; 256];
        // The child has exited, so this ends with EIO, or with EOF on macOS.
        while let Ok(len @ 1..) = master.read(&mut buf) {
            output.extend_from_slice(&buf[..len]);
        }
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("11 33"), "{output}");
        assert!(output.contains("isatty"), "{output}");
    }

    #[test]
    fn flush_keeps_what_did_not_fit() {
        struct TakesTwo(Vec<u8>);
        impl Write for TakesTwo {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                if self.0.len() >= 2 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.0.push(data[0]);
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut pending = b"abcd".to_vec();
        let mut output = TakesTwo(Vec::new());
        flush_some(&mut output, &mut pending).unwrap();
        assert_eq!(output.0, b"ab");
        assert_eq!(pending, b"cd");
    }
}
