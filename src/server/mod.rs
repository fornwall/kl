//! The server: owns the session and runs it in a single-threaded event loop,
//! independently of any client, so that panes outlive detaching.

mod copy_mode;
mod dispatch;
mod layout;
mod pane;
mod render;
mod session;
mod ui;

use std::error::Error;
use std::ffi::c_int;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::base64;
use crate::input::Parser;
use crate::proto::{self, ClientMessage, ServerMessage};
use crate::sys::{self, SIGCHLD, SIGHUP, SIGINT, SIGTERM, Selector, WinSize};
use dispatch::{Click, Drag};
use layout::PaneId;
use pane::Activity;
use render::Renderer;
use session::Session;
use ui::Ui;

/// The shortest time between frames, to coalesce heavy output.
const FRAME_TIME: Duration = Duration::from_millis(8);

/// How long a lone escape byte may wait for the rest of a sequence.
const ESCAPE_TIME: Duration = Duration::from_millis(25);

/// How often to refresh window names and the clock.
const TICK_TIME: Duration = Duration::from_secs(1);

/// How long a server waits for its first client before giving up.
const STARTUP_TIME: Duration = Duration::from_secs(10);

/// How many copied texts to keep.
const MAX_BUFFERS: usize = 50;

struct Connection {
    id: u64,
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    /// Close once the output is written.
    closing: bool,
    closed: bool,
}

impl Connection {
    fn send(&mut self, message: &ServerMessage) {
        message.encode(&mut self.output);
        self.flush();
    }

    /// Sends a final message, after which the connection goes away.
    fn finish(&mut self, message: &ServerMessage) {
        self.closing = true;
        self.send(message);
    }

    fn flush(&mut self) {
        if sys::flush_some(&self.stream, &mut self.output).is_err() {
            self.closed = true;
        }
        if self.closing && self.output.is_empty() {
            self.closed = true;
        }
    }

    /// Reads what is available and decodes the complete messages in it.
    fn read(&mut self) -> Vec<ClientMessage> {
        let mut buf = [0; 16 * 1024];
        match (&self.stream).read(&mut buf) {
            Ok(0) => self.closed = true,
            Ok(len) => self.input.extend_from_slice(buf.get(..len).unwrap_or(&[])),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => self.closed = true,
        }
        let mut messages = Vec::new();
        let mut start = 0;
        loop {
            match ClientMessage::decode(self.input.get(start..).unwrap_or(&[])) {
                Ok(Some((message, used))) => {
                    messages.push(message);
                    start += used;
                }
                Ok(None) => break,
                Err(error) => {
                    eprintln!("connection {}: {error}", self.id);
                    self.closed = true;
                    break;
                }
            }
        }
        self.input.drain(..start);
        messages
    }
}

pub struct Server {
    listener: UnixListener,
    signals: File,
    connections: Vec<Connection>,
    next_connection: u64,
    /// The connection showing the session.
    attached: Option<u64>,
    session: Session,
    ui: Ui,
    renderer: Renderer,
    parser: Parser,
    /// The prefix key was pressed, so the next key is a command.
    prefix: bool,
    /// Keys kl acted on, whose release programs should not see either.
    swallowed: Vec<c_int>,
    drag: Drag,
    last_click: Option<Click>,
    /// Copied texts, newest last.
    buffers: Vec<String>,
    /// Something changed that may need a new frame.
    dirty: bool,
    last_frame: Instant,
    last_tick: Instant,
    started_at: Instant,
    escape_since: Option<Instant>,
    /// A pane may be gone, so check them.
    check_panes: bool,
    lifecycle: Lifecycle,
    exit_message: &'static str,
}

/// Where the server is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    /// Waiting for the first client, which the first window is made for.
    Starting,
    /// Running out of windows from here on means the end.
    Running,
    Quitting,
}

/// Binds the socket, taking over from a server that died. Returns `None` if a
/// live server already has it, which happens when two clients start at once.
fn bind(path: &Path) -> io::Result<Option<UnixListener>> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(Some(listener)),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Ok(None);
            }
            fs::remove_file(path)?;
            UnixListener::bind(path).map(Some)
        }
        Err(error) => Err(error),
    }
}

/// Runs a server on the given socket until the session ends.
///
/// # Errors
///
/// Fails if the socket or the event loop fails.
pub fn run(socket: &Path) -> Result<(), Box<dyn Error>> {
    let Some(listener) = bind(socket)? else {
        return Ok(());
    };
    listener.set_nonblocking(true)?;
    let signals = sys::watch_signals(&[SIGCHLD, SIGHUP, SIGINT, SIGTERM])?;
    let now = Instant::now();
    let mut server = Server {
        listener,
        signals,
        connections: Vec::new(),
        next_connection: 0,
        attached: None,
        session: Session::new(socket.display().to_string()),
        ui: Ui::default(),
        renderer: Renderer::default(),
        parser: Parser::default(),
        prefix: false,
        swallowed: Vec::new(),
        drag: Drag::None,
        last_click: None,
        buffers: Vec::new(),
        dirty: false,
        last_frame: now,
        last_tick: now,
        started_at: now,
        escape_since: None,
        check_panes: false,
        lifecycle: Lifecycle::Starting,
        exit_message: "[exited]",
    };
    let result = server.run();
    server.shutdown();
    fs::remove_file(socket)?;
    Ok(result?)
}

impl Server {
    fn run(&mut self) -> io::Result<()> {
        while self.lifecycle != Lifecycle::Quitting {
            let mut selector = Selector::new();
            self.register(&mut selector);
            let timeout = self.next_wakeup().saturating_duration_since(Instant::now());
            selector.wait(Some(timeout))?;

            self.handle_signals(&selector);
            self.accept_connections(&selector);
            self.service_connections(&selector);
            self.service_panes(&selector);
            self.handle_timers();
            self.reap_panes();
            self.render();
            self.check_finished();
        }
        Ok(())
    }

    fn register(&self, selector: &mut Selector) {
        selector.want_read(self.listener.as_raw_fd());
        selector.want_read(self.signals.as_raw_fd());
        for connection in &self.connections {
            selector.want_read(connection.stream.as_raw_fd());
            if !connection.output.is_empty() {
                selector.want_write(connection.stream.as_raw_fd());
            }
        }
        for pane in self.session.panes.values() {
            selector.want_read(pane.fd());
            if pane.wants_write() {
                selector.want_write(pane.fd());
            }
        }
    }

    fn next_wakeup(&self) -> Instant {
        let mut next = self.last_tick + TICK_TIME;
        if self.dirty && self.can_draw() {
            next = next.min(self.last_frame + FRAME_TIME);
        }
        if let Some(since) = self.escape_since {
            next = next.min(since + ESCAPE_TIME);
        }
        if let Some(deadline) = self.ui.message_deadline() {
            next = next.min(deadline);
        }
        for (id, _) in self.session.visible_panes() {
            let hold = self.session.panes.get(&id);
            if let Some(deadline) = hold.and_then(|pane| pane.terminal.hold_deadline()) {
                next = next.min(deadline);
            }
        }
        next
    }

    fn handle_signals(&mut self, selector: &Selector) {
        if !selector.readable(self.signals.as_raw_fd()) {
            return;
        }
        for signal in sys::pending_signals(&self.signals) {
            if signal == SIGCHLD {
                self.check_panes = true;
            } else {
                self.exit_message = "[server exited]";
                self.lifecycle = Lifecycle::Quitting;
            }
        }
    }

    fn accept_connections(&mut self, selector: &Selector) {
        if !selector.readable(self.listener.as_raw_fd()) {
            return;
        }
        while let Ok((stream, _)) = self.listener.accept() {
            if !sys::selectable(stream.as_raw_fd()) || stream.set_nonblocking(true).is_err() {
                continue;
            }
            self.connections.push(Connection {
                id: self.next_connection,
                stream,
                input: Vec::new(),
                output: Vec::new(),
                closing: false,
                closed: false,
            });
            self.next_connection += 1;
        }
    }

    fn service_connections(&mut self, selector: &Selector) {
        let mut messages = Vec::new();
        for connection in &mut self.connections {
            let fd = connection.stream.as_raw_fd();
            if selector.writable(fd) {
                connection.flush();
            }
            if selector.readable(fd) {
                let id = connection.id;
                messages.extend(connection.read().into_iter().map(|message| (id, message)));
            }
        }
        for (id, message) in messages {
            self.handle_message(id, message);
        }
        let attached = self.attached;
        self.connections.retain(|connection| !connection.closed);
        if attached.is_some_and(|id| !self.connections.iter().any(|c| c.id == id)) {
            self.attached = None;
        }
    }

    fn connection(&mut self, id: u64) -> Option<&mut Connection> {
        self.connections.iter_mut().find(|c| c.id == id)
    }

    fn handle_message(&mut self, id: u64, message: ClientMessage) {
        match message {
            ClientMessage::Attach { version, size } => self.attach(id, version, size),
            ClientMessage::Input(bytes) if self.attached == Some(id) => self.handle_input(&bytes),
            ClientMessage::Resize(size) if self.attached == Some(id) => {
                self.session.resize(size);
                self.dirty = true;
            }
            ClientMessage::Input(_) | ClientMessage::Resize(_) => {}
            ClientMessage::Command { version, args } => self.run_client_command(id, version, &args),
        }
    }

    fn version_error(version: u32) -> Option<String> {
        (version != proto::VERSION).then(|| {
            format!(
                "this client (protocol {version}) does not match the running server \
                 (protocol {}): stop it with the old binary's kill-server",
                proto::VERSION
            )
        })
    }

    fn attach(&mut self, id: u64, version: u32, size: WinSize) {
        if let Some(error) = Self::version_error(version) {
            if let Some(connection) = self.connection(id) {
                connection.finish(&ServerMessage::Exit(error));
            }
            return;
        }
        if let Some(previous) = self.attached.replace(id)
            && let Some(connection) = self.connection(previous)
        {
            connection.finish(&ServerMessage::Exit(
                "[detached (from another client)]".to_owned(),
            ));
        }
        self.parser = Parser::default();
        self.prefix = false;
        self.swallowed.clear();
        self.drag = Drag::None;
        self.session.resize(size);
        if self.lifecycle == Lifecycle::Starting {
            self.lifecycle = Lifecycle::Running;
            if let Err(error) = self.session.new_window(None, &[]) {
                if let Some(connection) = self.connection(id) {
                    connection.finish(&ServerMessage::Exit(error));
                }
                self.lifecycle = Lifecycle::Quitting;
                return;
            }
        }
        self.renderer.invalidate();
        self.dirty = true;
    }

    fn run_client_command(&mut self, id: u64, version: u32, args: &[String]) {
        let result = Self::version_error(version).map_or_else(
            || crate::command::parse(args).and_then(|command| self.execute(command)),
            Err,
        );
        let (success, text) = match result {
            Ok(text) => (true, text),
            Err(text) => (false, text),
        };
        if let Some(connection) = self.connection(id) {
            connection.finish(&ServerMessage::Result { success, text });
        }
    }

    fn service_panes(&mut self, selector: &Selector) {
        let ids: Vec<PaneId> = self.session.panes.keys().copied().collect();
        for id in ids {
            let Some(pane) = self.session.panes.get_mut(&id) else {
                continue;
            };
            if selector.writable(pane.fd()) {
                pane.flush();
            }
            if selector.readable(pane.fd()) {
                let activity = pane.read();
                self.after_read(id, &activity);
            }
        }
    }

    fn after_read(&mut self, id: PaneId, activity: &Activity) {
        // Read also finds out about programs that closed the pty.
        self.check_panes = true;
        let visible = self.session.is_visible(id);
        self.dirty |= visible || activity.title_changed;
        for text in &activity.clipboard {
            self.send_clipboard(text);
        }
        if activity.bell {
            if visible {
                self.send_output(b"\x07");
            } else if let Some(window) = self
                .session
                .windows
                .iter_mut()
                .find(|window| window.root.contains(id))
            {
                window.bell = true;
                self.dirty = true;
            }
        }
    }

    fn send_output(&mut self, bytes: &[u8]) {
        if let Some(id) = self.attached
            && let Some(connection) = self.connection(id)
        {
            connection.send(&ServerMessage::Output(bytes.to_vec()));
        }
    }

    /// Puts text on the system clipboard, through the outer terminal.
    fn send_clipboard(&mut self, text: &[u8]) {
        let mut osc = b"\x1b]52;c;".to_vec();
        osc.extend_from_slice(base64::encode(text).as_bytes());
        osc.extend_from_slice(b"\x1b\\");
        self.send_output(&osc);
    }

    fn store_buffer(&mut self, text: String) {
        self.send_clipboard(text.as_bytes());
        self.buffers.push(text);
        if self.buffers.len() > MAX_BUFFERS {
            self.buffers.remove(0);
        }
    }

    fn handle_timers(&mut self) {
        let now = Instant::now();
        if self
            .escape_since
            .is_some_and(|since| now >= since + ESCAPE_TIME)
        {
            self.escape_since = None;
            if let Some(event) = self.parser.flush_escape() {
                self.handle_event(event);
            }
        }
        self.dirty |= self.ui.expire_message();
        for (id, _) in self.session.visible_panes() {
            let hold = self.session.panes.get(&id);
            let deadline = hold.and_then(|pane| pane.terminal.hold_deadline());
            self.dirty |= deadline.is_some_and(|deadline| now >= deadline);
        }
        if now >= self.last_tick + TICK_TIME {
            self.last_tick = now;
            self.refresh_window_names();
            self.dirty = true;
        }
    }

    fn refresh_window_names(&mut self) {
        for window in &mut self.session.windows {
            if !window.automatic_name {
                continue;
            }
            let pane = self.session.panes.get(&window.active);
            if let Some(name) = pane.and_then(pane::Pane::foreground_name) {
                window.name = name;
            }
        }
    }

    fn reap_panes(&mut self) {
        if !std::mem::take(&mut self.check_panes) {
            return;
        }
        let exited: Vec<PaneId> = self
            .session
            .panes
            .iter_mut()
            .filter_map(|(id, pane)| pane.has_exited().then_some(*id))
            .collect();
        for id in exited {
            self.session.remove_pane(id);
            self.dirty = true;
        }
    }

    fn can_draw(&self) -> bool {
        // Holding off while the client catches up is the flow control: what it
        // misses never gets sent, since each frame is a diff of the latest state.
        let connection = self.attached.and_then(|id| {
            self.connections
                .iter()
                .find(|connection| connection.id == id)
        });
        connection.is_some_and(|connection| connection.output.is_empty())
    }

    fn render(&mut self) {
        if !self.dirty || !self.can_draw() || self.last_frame.elapsed() < FRAME_TIME {
            return;
        }
        self.dirty = false;
        self.last_frame = Instant::now();
        let frame = self.renderer.frame(&mut self.session, &self.ui);
        if !frame.is_empty() {
            self.send_output(&frame);
        }
    }

    fn check_finished(&mut self) {
        let finished = match self.lifecycle {
            Lifecycle::Starting => {
                self.connections.is_empty() && self.started_at.elapsed() >= STARTUP_TIME
            }
            Lifecycle::Running => self.session.windows.is_empty(),
            Lifecycle::Quitting => true,
        };
        if finished {
            self.lifecycle = Lifecycle::Quitting;
        }
    }

    /// Tells clients goodbye. Blocking is fine here, since nothing is left to
    /// serve, and it makes sure they get to restore their terminals.
    fn shutdown(&mut self) {
        let goodbye = ServerMessage::Exit(self.exit_message.to_owned());
        for connection in &mut self.connections {
            let mut message = std::mem::take(&mut connection.output);
            goodbye.encode(&mut message);
            if connection.stream.set_nonblocking(false).is_ok()
                && (&connection.stream).write_all(&message).is_err()
            {
                eprintln!("connection {} left without a goodbye", connection.id);
            }
        }
    }
}
