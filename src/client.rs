//! The client: connects a terminal to the server, starting one if needed.

use std::env;
use std::error::Error;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::proto::{self, ClientMessage, ServerMessage};
use crate::sys::{self, SIGHUP, SIGTERM, SIGWINCH, Selector};

/// How long to wait for a newly started server to come up.
const SERVER_START_TIME: Duration = Duration::from_secs(5);

/// Sets up the terminal for kl. Order matters: the keyboard mode lives on a
/// stack per screen, so it goes on after switching to the alternate screen.
///
/// The keyboard flags ask for everything, so panes can get full-fidelity events
/// whatever they ask for. The queries at the end find out the default colors,
/// to report to programs that ask. Grapheme clustering is saved rather than just
/// turned off afterwards, since it is on by default in Ghostty.
const ENTER_TERMINAL: &[u8] = b"\x1b[?1049h\
    \x1b[>31u\
    \x1b[?2027s\x1b[?2027h\
    \x1b[?1003h\x1b[?1006h\
    \x1b[?2004h\
    \x1b[?1004h\
    \x1b]10;?\x1b\\\x1b]11;?\x1b\\";

/// Undoes [`ENTER_TERMINAL`], in reverse, along with what frames leave behind.
/// The client does this rather than the server, so that it happens even if the
/// server dies.
const LEAVE_TERMINAL: &[u8] = b"\x1b[?1004l\
    \x1b[?2004l\
    \x1b[?1006l\x1b[?1003l\
    \x1b[?2027r\
    \x1b[<u\
    \x1b[0m\x1b[0 q\x1b[?25h\
    \x1b[?1049l";

/// Where the server's socket lives, in a directory only this user can enter.
///
/// # Errors
///
/// Fails if the directory cannot be created or does not look safe to use.
pub fn socket_path() -> io::Result<PathBuf> {
    let base = env::var_os("KL_TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let directory = base.join(format!("kl-{}", sys::user_id()));
    match DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    // /tmp is shared, so someone else could have made this directory first.
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir()
        || metadata.uid() != sys::user_id()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::other(format!(
            "{} is not a private directory of this user",
            directory.display()
        )));
    }
    Ok(directory.join("default"))
}

fn log_file(socket: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(socket.with_file_name("server.log"))
}

fn start_server(socket: &Path) -> io::Result<()> {
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("--server")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file(socket)?));
    sys::spawn_daemon(command).map(drop)
}

/// Connects to the server, starting one if none is running.
fn connect_or_start(socket: &Path) -> io::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
    start_server(socket)?;
    let deadline = Instant::now() + SERVER_START_TIME;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            // These mean the server is not listening yet. Anything else, such as
            // a path too long for a socket, will not get better by waiting.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

fn send(mut stream: &UnixStream, message: &ClientMessage) -> io::Result<()> {
    let mut bytes = Vec::new();
    message.encode(&mut bytes);
    stream.write_all(&bytes)
}

/// What the server sent, for the attach loop to act on.
enum Incoming {
    Message(ServerMessage),
    Closed,
    Nothing,
}

/// Buffers what the server sends, and splits it into messages.
#[derive(Default)]
struct Receiver {
    buffer: Vec<u8>,
}

impl Receiver {
    /// Takes one decoded message off the buffer, if there is a whole one.
    fn next_message(&mut self) -> Result<Option<ServerMessage>, proto::Malformed> {
        let Some((message, used)) = ServerMessage::decode(&self.buffer)? else {
            return Ok(None);
        };
        self.buffer.drain(..used);
        Ok(Some(message))
    }

    fn read(&mut self, mut stream: &UnixStream) -> io::Result<Incoming> {
        let mut buf = [0; 16 * 1024];
        match stream.read(&mut buf) {
            Ok(0) => Ok(Incoming::Closed),
            Ok(len) => {
                self.buffer.extend_from_slice(buf.get(..len).unwrap_or(&[]));
                Ok(Incoming::Nothing)
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(Incoming::Nothing),
            Err(error) => Err(error),
        }
    }

    /// Reads until a message is complete.
    fn wait(&mut self, stream: &UnixStream) -> Result<Incoming, Box<dyn Error>> {
        loop {
            if let Some(message) = self.next_message()? {
                return Ok(Incoming::Message(message));
            }
            if matches!(self.read(stream)?, Incoming::Closed) {
                return Ok(Incoming::Closed);
            }
        }
    }
}

/// Attaches this terminal, returning what to tell the user afterwards.
///
/// # Errors
///
/// Fails if this is not a terminal or the server cannot be reached.
pub fn attach(socket: &Path) -> Result<String, Box<dyn Error>> {
    if env::var_os("KL").is_some() {
        return Err("sessions should be nested with care, unset $KL to force".into());
    }
    let stdin = io::stdin();
    if !stdin.is_terminal() || !io::stdout().is_terminal() {
        return Err("kl needs a terminal to attach to".into());
    }
    let stream = connect_or_start(socket)?;
    let terminal = stdin.as_fd();
    let _raw_mode = sys::RawMode::enter(terminal)?;
    let signals = sys::watch_signals(&[SIGWINCH, SIGHUP, SIGTERM])?;

    let mut stdout = io::stdout();
    stdout.write_all(ENTER_TERMINAL)?;
    stdout.flush()?;

    let result = attached_loop(&stream, &signals);

    stdout.write_all(LEAVE_TERMINAL)?;
    stdout.flush()?;
    result
}

fn attached_loop(stream: &UnixStream, signals: &File) -> Result<String, Box<dyn Error>> {
    let stdin = io::stdin();
    let terminal = stdin.as_fd();
    let mut input = File::from(terminal.try_clone_to_owned()?);
    let mut stdout = io::stdout();
    let mut receiver = Receiver::default();

    let size = sys::winsize(terminal)?;
    let version = proto::VERSION;
    send(stream, &ClientMessage::Attach { version, size })?;

    loop {
        let mut selector = Selector::new();
        selector.want_read(input.as_raw_fd());
        selector.want_read(stream.as_raw_fd());
        selector.want_read(signals.as_raw_fd());
        selector.wait(None)?;

        if selector.readable(signals.as_raw_fd()) {
            for signal in sys::pending_signals(signals) {
                if signal == SIGWINCH {
                    send(stream, &ClientMessage::Resize(sys::winsize(terminal)?))?;
                } else {
                    return Ok("[terminated]".to_owned());
                }
            }
        }
        if selector.readable(input.as_raw_fd()) {
            let mut buf = [0; 16 * 1024];
            match input.read(&mut buf)? {
                0 => return Ok("[detached]".to_owned()),
                len => {
                    let bytes = buf.get(..len).unwrap_or(&[]).to_vec();
                    send(stream, &ClientMessage::Input(bytes))?;
                }
            }
        }
        if selector.readable(stream.as_raw_fd()) {
            if matches!(receiver.read(stream)?, Incoming::Closed) {
                return Ok("[lost server]".to_owned());
            }
            while let Some(message) = receiver.next_message()? {
                match message {
                    ServerMessage::Output(bytes) => {
                        stdout.write_all(&bytes)?;
                        stdout.flush()?;
                    }
                    ServerMessage::Exit(text) => return Ok(text),
                    ServerMessage::Result { .. } => {}
                }
            }
        }
    }
}

/// Runs one command on the running server, returning whether it succeeded and
/// what it printed.
///
/// # Errors
///
/// Fails if no server is running.
pub fn run_command(socket: &Path, args: &[String]) -> Result<(bool, String), Box<dyn Error>> {
    let stream = UnixStream::connect(socket).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            format!("no server running on {}", socket.display())
        }
        _ => format!("connecting to the server: {error}"),
    })?;
    send(
        &stream,
        &ClientMessage::Command {
            version: proto::VERSION,
            args: args.to_vec(),
        },
    )?;
    let mut receiver = Receiver::default();
    match receiver.wait(&stream)? {
        Incoming::Message(ServerMessage::Result { success, text }) => Ok((success, text)),
        // kill-server can take the server down before it gets to answer.
        Incoming::Message(ServerMessage::Exit(_)) | Incoming::Closed => Ok((true, String::new())),
        Incoming::Message(ServerMessage::Output(_)) | Incoming::Nothing => {
            Err("unexpected reply from the server".into())
        }
    }
}
