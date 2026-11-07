//! The server: owns the session and runs it in a single-threaded event loop,
//! independently of any client, so that panes outlive detaching.

mod buffers;
mod client;
mod copy_commands;
mod copy_mode;
mod dispatch;
mod format;
mod images;
mod keys;
mod layout;
mod motion;
mod notation;
mod pane;
mod render;
mod restart;
mod session;
mod target;
mod tree;
mod ui;

use std::cmp::Reverse;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::base64;
use crate::ghostty::{Notification, OuterColors};
use crate::proto::{self, AttachMode, ClientInfo, ClientMessage, ServerMessage};
use crate::sys::{self, SIGCHLD, SIGHUP, SIGINT, SIGTERM, Selector, WinSize};
use buffers::Buffers;
use client::Client;
use layout::PaneId;
use pane::Activity;
use render::{Canvas, osc_text};
pub use restart::STATE_VERSION;
use session::{Monitor, NameCheck, Session};

/// The shortest time between frames, to coalesce heavy output.
const FRAME_TIME: Duration = Duration::from_millis(8);

/// How long a lone escape byte may wait for the rest of a sequence.
const ESCAPE_TIME: Duration = Duration::from_millis(25);

/// How much output a client may have waiting before it misses what is not part
/// of a frame. Frames wait until it has none, and make up for what it missed, but
/// bells and the like would add up for a client that stopped reading.
const MAX_PENDING: usize = 1024 * 1024;

/// The most output to put in one message. Pixels of a large image would otherwise
/// add up to more than a message can hold.
const OUTPUT_PIECE: usize = 1024 * 1024;

/// How often to refresh window names and the clock.
const TICK_TIME: Duration = Duration::from_secs(1);

/// How long after something happened to look at what runs in each window, and
/// to look again for [`session::Window::follow`]. A shell takes a moment to start what was
/// typed, and may print nothing when it does, so looking right away would see the
/// shell itself.
const NAME_DELAY: Duration = Duration::from_millis(50);

/// How long a server waits for its first client before giving up.
const STARTUP_TIME: Duration = Duration::from_secs(10);

/// How long a client gets to take in the goodbye, since one that stopped reading
/// must not keep the server alive.
const GOODBYE_TIME: Duration = Duration::from_secs(1);

struct Connection {
    id: u64,
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    /// Close once the output is written.
    closing: bool,
    closed: bool,
    /// The terminal at the other end, if this connection is showing the session
    /// rather than running a command.
    client: Option<Client>,
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

    /// Sends what is no frame, unless this client has fallen too far behind.
    fn send_unless_behind(&mut self, bytes: &[u8]) {
        if self.output.len() < MAX_PENDING {
            self.send(&ServerMessage::Output(bytes.to_vec()));
        }
    }

    /// Holding off while a client catches up is the flow control: what it misses
    /// never gets sent, since each frame is a diff of the latest state.
    const fn can_draw(&self) -> bool {
        self.client.is_some() && self.output.is_empty()
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
                    sys::log(format_args!("connection {}: {error}", self.id));
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
    /// The connection of the client that what is being handled came from.
    acting: Option<u64>,
    /// What [`Server::client`] gives without a client, which nobody gets to see.
    nobody: Client,
    session: Session,
    canvas: Canvas,
    buffers: Buffers,
    /// Something changed that may need a new frame, for any client.
    dirty: bool,
    last_tick: Instant,
    started_at: Instant,
    /// When to next look at what runs in each window, ahead of the tick.
    names_due: Option<Instant>,
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

/// Asks the outer terminal about its colors, for programs that want to know.
/// The server does this rather than the client, since it needs to ask again
/// when the color scheme changes. Each palette entry gets a sequence of its own,
/// as the Ghostty app gives up on a reply that does not fit in 1 KiB.
fn color_queries() -> Vec<u8> {
    let mut queries = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n".to_vec();
    for index in 0..=u8::MAX {
        queries.extend_from_slice(format!("\x1b]4;{index};?\x1b\\").as_bytes());
    }
    queries
}

/// OSC 9 for what it can carry. It has no title, and a body starting with a
/// digit would read as one of the numbered commands that share OSC 9 with it.
fn notification_sequence(notification: &Notification) -> Vec<u8> {
    let Notification { title, body } = notification;
    let plain = title.is_empty() && !body.starts_with(|c: char| c.is_ascii_digit());
    let mut osc = if plain {
        b"\x1b]9;".to_vec()
    } else {
        let mut osc = b"\x1b]777;notify;".to_vec();
        osc.extend(osc_text(title));
        osc.push(b';');
        osc
    };
    osc.extend(osc_text(body));
    osc.extend_from_slice(b"\x1b\\");
    osc
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

/// Runs a server on the given socket until the session ends, taking over from
/// the server that left `state` if there is one.
pub fn run(socket: &Path, state: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let (listener, session, buffers, lifecycle) = if let Some(state) = state {
        let restored = restart::read_state(state, socket)?;
        let (listener, session, buffers) = (restored.listener, restored.session, restored.buffers);
        (listener, session, buffers, Lifecycle::Running)
    } else {
        let Some(listener) = bind(socket)? else {
            return Ok(());
        };
        let session = Session::new(socket.display().to_string());
        (listener, session, Buffers::default(), Lifecycle::Starting)
    };
    listener.set_nonblocking(true)?;
    let signals = sys::watch_signals(&[SIGCHLD, SIGHUP, SIGINT, SIGTERM])?;
    let now = Instant::now();
    let mut server = Server {
        listener,
        signals,
        connections: Vec::new(),
        next_connection: 0,
        acting: None,
        nobody: Client::new(
            WinSize::default(),
            OuterColors::default(),
            now,
            ClientInfo::default(),
        ),
        session,
        canvas: Canvas::default(),
        buffers,
        dirty: false,
        last_tick: now,
        started_at: now,
        names_due: None,
        lifecycle,
        exit_message: "[exited]",
    };
    // No signal is coming for a program that ended during a handover.
    server.collect_children();
    let result = server.run();
    server.goodbye(&ServerMessage::Exit(server.exit_message.to_owned()));
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
        for connection in self.connections.iter().filter(|c| c.can_draw()) {
            if let Some(client) = &connection.client
                && (self.dirty || client.dirty)
            {
                next = next.min(client.last_frame + FRAME_TIME);
            }
        }
        // Drawing is what ends a hold, so without it there is nothing to do.
        if self.can_draw()
            && let Some(deadline) = self.hold_deadline()
        {
            next = next.min(deadline);
        }
        for client in self.clients() {
            let escape = client.escape_since.map(|since| since + ESCAPE_TIME);
            let deadlines = [
                escape,
                client.drag.next_scroll(),
                client.ui.message_deadline(),
                client.ui.pane_numbers_deadline(),
            ];
            next = deadlines.into_iter().flatten().fold(next, Instant::min);
        }
        if let Some(due) = self.names_due {
            next = next.min(due);
        }
        next
    }

    /// When the first program holding a visible pane gets to be ignored.
    fn hold_deadline(&self) -> Option<Instant> {
        let panes = self.session.visible_panes();
        let hold = |id| self.session.panes.get(id)?.terminal.hold_deadline();
        panes.iter().filter_map(|(id, _)| hold(id)).min()
    }

    fn handle_signals(&mut self, selector: &Selector) {
        if !selector.readable(self.signals.as_raw_fd()) {
            return;
        }
        for signal in sys::pending_signals(&self.signals) {
            if signal == SIGCHLD {
                self.collect_children();
            } else {
                self.exit_message = "[server exited]";
                self.lifecycle = Lifecycle::Quitting;
            }
        }
    }

    fn collect_children(&mut self) {
        let pids = sys::reap_children();
        for pane in self.session.panes.values_mut() {
            pane.exited |= pids.contains(&pane.pid);
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
                client: None,
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
        let before = self.clients().count();
        self.connections.retain(|connection| !connection.closed);
        if self.clients().count() != before {
            self.after_client_left();
        }
    }

    /// The window goes on with those who are left, if any.
    pub fn after_client_left(&mut self) {
        self.follow_latest();
        self.update_focus();
    }

    /// The client active last decides what the window is like, as with tmux's
    /// `window-size latest`: its size, and the colors programs get to hear of. The
    /// first one wins a tie, which is the first back from a restart.
    pub fn follow_latest(&mut self) {
        let connections = self.connections.iter().rev();
        let clients = connections.filter_map(|connection| connection.client.as_ref());
        let Some(latest) = clients.max_by_key(|client| client.active_at) else {
            return;
        };
        let (size, colors) = (latest.size, &latest.colors);
        if *colors != self.session.colors {
            let colors = colors.clone();
            self.session.set_colors(&colors);
            self.dirty = true;
        }
        if size != self.session.size {
            self.session.resize(size);
            self.dirty = true;
        }
    }

    /// The acting client's user did something.
    pub fn note_activity(&mut self) {
        self.client().active_at = Instant::now();
        self.follow_latest();
    }

    /// Programs have the keyboard while any terminal showing them does.
    pub fn update_focus(&mut self) {
        let focused = self.clients().any(|client| client.focused);
        if focused != self.session.focused {
            self.session.focused = focused;
            self.session.report_focus(focused);
        }
    }

    /// The connection of the client commands from outside are for: tmux's best
    /// client, which with a single session is the one active last.
    fn latest_client(&self) -> Option<u64> {
        let connections = self.connections.iter().rev();
        let attached = connections.filter_map(|connection| {
            let client = connection.client.as_ref()?;
            Some((client.active_at, connection.id))
        });
        attached
            .max_by_key(|(active_at, _)| *active_at)
            .map(|(_, id)| id)
    }

    /// The clients attached right now.
    fn clients(&self) -> impl Iterator<Item = &Client> {
        let connections = self.connections.iter();
        connections.filter_map(|connection| connection.client.as_ref())
    }

    /// The connections of the clients attached right now.
    fn client_ids(&self) -> Vec<u64> {
        let connections = self.connections.iter();
        let attached = connections.filter(|connection| connection.client.is_some());
        attached.map(|connection| connection.id).collect()
    }

    /// The client that what is being handled came from. Commands can come from
    /// the command line with no client attached, and then this is a stand-in, as
    /// what they leave for a client is not meant for anyone else.
    pub fn client(&mut self) -> &mut Client {
        let acting = self.acting;
        let connections = self.connections.iter_mut();
        let connection = connections
            .filter(|connection| Some(connection.id) == acting)
            .find_map(|connection| connection.client.as_mut());
        connection.unwrap_or(&mut self.nobody)
    }

    fn acting_client(&self) -> Option<&Client> {
        let connections = self.connections.iter();
        connections
            .filter(|connection| Some(connection.id) == self.acting)
            .find_map(|connection| connection.client.as_ref())
    }

    /// See [`Server::client`].
    pub fn client_ref(&self) -> &Client {
        self.acting_client().unwrap_or(&self.nobody)
    }

    fn connection_mut(&mut self, id: u64) -> Option<&mut Connection> {
        self.connections.iter_mut().find(|c| c.id == id)
    }

    fn handle_message(&mut self, id: u64, message: ClientMessage) {
        self.acting = Some(id);
        let attached = self
            .connection_mut(id)
            .is_some_and(|connection| connection.client.is_some());
        match message {
            ClientMessage::Attach {
                version,
                size,
                mode,
                info,
                environment,
            } => self.attach(id, version, size, mode, info, environment),
            ClientMessage::Input(bytes) if attached => self.handle_input(&bytes),
            ClientMessage::Resize(size) if attached => {
                self.client().size = size;
                self.note_activity();
                self.dirty = true;
            }
            ClientMessage::Input(_) | ClientMessage::Resize(_) => {}
            ClientMessage::Command {
                version,
                pane,
                args,
            } => {
                self.acting = self.latest_client();
                self.session.origin = pane;
                self.run_client_command(id, version, &args);
                self.session.origin = None;
            }
        }
        self.acting = None;
    }

    fn version_error(version: u32) -> Option<String> {
        (version != proto::VERSION).then(|| {
            format!(
                "this client (protocol {version}) does not match the running server \
                 (protocol {}): replace it with restart-server",
                proto::VERSION
            )
        })
    }

    fn attach(
        &mut self,
        id: u64,
        version: u32,
        size: WinSize,
        mode: AttachMode,
        info: ClientInfo,
        environment: Vec<String>,
    ) {
        if let Some(error) = Self::version_error(version) {
            if let Some(connection) = self.connection_mut(id) {
                connection.finish(&ServerMessage::Exit(error));
            }
            return;
        }
        let colors = self.session.colors.clone();
        if mode == AttachMode::Alone {
            for other in self.client_ids() {
                self.detach_client(other);
            }
        }
        // Clients coming back behave as they did, as far as that is known.
        let active_at = if mode == AttachMode::Reattach {
            self.started_at
        } else {
            self.session.environment = environment;
            Instant::now()
        };
        if let Some(connection) = self.connection_mut(id) {
            connection.client = Some(Client::new(size, colors, active_at, info));
        }
        self.follow_latest();
        self.update_focus();
        if self.lifecycle == Lifecycle::Starting {
            self.lifecycle = Lifecycle::Running;
            if let Err(error) = self.session.new_window(None, &[], None, None, false) {
                if let Some(connection) = self.connection_mut(id) {
                    connection.finish(&ServerMessage::Exit(error));
                }
                self.lifecycle = Lifecycle::Quitting;
                return;
            }
        }
        self.dirty = true;
        self.ask_about_colors();
    }

    /// Asks the terminal of the acting client about its colors.
    pub fn ask_about_colors(&mut self) {
        if let Some(id) = self.acting
            && let Some(connection) = self.connection_mut(id)
        {
            connection.send(&ServerMessage::Output(color_queries()));
        }
    }

    fn run_client_command(&mut self, id: u64, version: u32, args: &[String]) {
        // Replacing the server is how a client of another version gets to talk
        // to it, so that much has to work across versions.
        let restarting = args.first().is_some_and(|name| name == "restart-server");
        let result = Self::version_error(version)
            .filter(|_| !restarting)
            .map_or_else(
                || crate::command::parse(args).and_then(|command| self.execute(command)),
                Err,
            );
        let (success, text) = match result {
            Ok(text) => (true, text),
            Err(text) => (false, text),
        };
        if let Some(connection) = self.connection_mut(id) {
            connection.finish(&ServerMessage::Result { success, text });
        }
    }

    fn service_panes(&mut self, selector: &Selector) {
        let ids: Vec<PaneId> = self.session.panes.keys().copied().collect();
        let active = self.session.active_id();
        for id in ids {
            let Some(pane) = self.session.panes.get_mut(&id) else {
                continue;
            };
            if selector.writable(pane.fd()) {
                pane.flush();
            }
            if selector.readable(pane.fd()) {
                // A reply goes to the program's pty, which can lead to another
                // host, so this is for the pane in use, and never gets further
                // than what was copied in kl.
                let readable = (active == Some(id)).then(|| self.buffers.top());
                let clipboard = readable.flatten().map(|buffer| buffer.text.as_bytes());
                let activity = pane.read(clipboard);
                self.after_read(id, &activity);
            }
        }
    }

    fn after_read(&mut self, id: PaneId, activity: &Activity) {
        self.names_may_change();
        let visible = self.session.is_visible(id);
        self.dirty |= visible || activity.title_changed;
        for text in &activity.clipboard {
            self.send_clipboard(text);
        }
        // Reaching the user from a pane out of sight is what these are for.
        for notification in &activity.notifications {
            self.send_output(&notification_sequence(notification));
        }
        if activity.progress_reported {
            self.session.progress_reports += 1;
            self.dirty = true;
        }
        if activity.bell {
            if visible {
                self.send_output(b"\x07");
            } else if let Some(window) = self.session.window_of_mut(id) {
                window.bell = true;
                self.dirty = true;
            }
        }
        let current = self.session.current;
        if activity.output > 0
            && let Some(window) = self.session.window_of_mut(id)
            && window.monitor == Monitor::Watching
            && window.index != current
        {
            window.monitor = Monitor::Flagged;
            self.dirty = true;
        }
    }

    /// Tells every terminal, as tmux does with bells and the clipboard. There is
    /// no telling who a program means, and leaving someone out loses more than
    /// hearing it twice does.
    fn send_output(&mut self, bytes: &[u8]) {
        for connection in &mut self.connections {
            if connection.client.is_some() {
                connection.send_unless_behind(bytes);
            }
        }
    }

    /// Puts text on the system clipboard, through the outer terminals.
    fn send_clipboard(&mut self, text: &[u8]) {
        let mut osc = b"\x1b]52;c;".to_vec();
        osc.extend_from_slice(base64::encode(text).as_bytes());
        osc.extend_from_slice(b"\x1b\\");
        self.send_output(&osc);
    }

    fn handle_timers(&mut self) {
        let now = Instant::now();
        for id in self.client_ids() {
            self.acting = Some(id);
            self.handle_client_timers(now);
        }
        self.acting = None;
        self.dirty |= self.hold_deadline().is_some_and(|deadline| now >= deadline);
        if self.names_due.is_some_and(|due| now >= due) {
            self.refresh_window_names();
        }
        if now >= self.last_tick + TICK_TIME {
            self.last_tick = now;
            self.refresh_window_names();
            for pane in self.session.panes.values_mut() {
                pane.terminal.compress_idle_scrollback();
            }
            self.dirty = true;
        }
    }

    fn handle_client_timers(&mut self, now: Instant) {
        let client = self.client();
        let escape_over = |since| now >= since + ESCAPE_TIME;
        if client.escape_since.is_some_and(escape_over) {
            client.escape_since = None;
            if let Some(event) = client.parser.flush_escape() {
                self.handle_event(event);
            }
        }
        if self.client().drag.next_scroll().is_some_and(|at| now >= at) {
            self.autoscroll();
        }
        self.dirty |= self.client().ui.expire_message();
        if self.client().ui.expire_pane_numbers() {
            self.hide_pane_numbers();
        }
    }

    /// Something happened that may put another program in the foreground.
    fn names_may_change(&mut self) {
        self.names_due
            .get_or_insert_with(|| Instant::now() + NAME_DELAY);
    }

    fn refresh_window_names(&mut self) {
        self.names_due = None;
        let mut look_again = false;
        for window in &mut self.session.windows {
            if !window.automatic_name {
                continue;
            }
            let pane = self.session.panes.get(&window.active);
            if let Some(name) = pane.and_then(pane::Pane::foreground_name) {
                match window.follow(name) {
                    NameCheck::Unchanged => {}
                    NameCheck::Changed => self.dirty = true,
                    NameCheck::LookAgain => look_again = true,
                }
            }
        }
        if look_again {
            self.names_may_change();
        }
    }

    fn reap_panes(&mut self) {
        let panes = self.session.panes.values();
        let exited: Vec<PaneId> = panes
            .filter(|pane| pane.exited)
            .map(|pane| pane.id)
            .collect();
        for id in exited {
            self.session.remove_pane(id);
            self.dirty = true;
        }
    }

    fn can_draw(&self) -> bool {
        self.connections.iter().any(Connection::can_draw)
    }

    /// Draws for the clients that have something new and can take it. Others get
    /// their turn when they caught up, without holding up anything else.
    fn render(&mut self) {
        let dirty = std::mem::take(&mut self.dirty);
        let clients = self.connections.iter_mut();
        for client in clients.filter_map(|connection| connection.client.as_mut()) {
            client.dirty |= dirty;
        }
        let due = |connection: &Connection| {
            let client = connection.client.as_ref();
            connection.can_draw()
                && client.is_some_and(|c| c.dirty && c.last_frame.elapsed() >= FRAME_TIME)
        };
        if !self.connections.iter().any(due) {
            return;
        }
        self.canvas.update(&mut self.session);
        let now = Instant::now();
        for connection in &mut self.connections {
            if !due(connection) {
                continue;
            }
            if let Some(client) = &mut connection.client {
                client.last_frame = now;
                let frame = client.frame(&self.canvas, &self.session);
                client.dirty = client.renderer.transferring();
                for part in frame.chunks(OUTPUT_PIECE) {
                    connection.send(&ServerMessage::Output(part.to_vec()));
                }
            }
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

    /// Sends every client a last message. Blocking is fine here, since nothing
    /// is left to serve, and it gives them the best chance to take it in. The
    /// client active last goes first, as that tends to be how they come back.
    fn goodbye(&mut self, goodbye: &ServerMessage) {
        let active_at = |connection: &Connection| {
            let client = connection.client.as_ref();
            Reverse(client.map(|client| client.active_at))
        };
        self.connections.sort_by_key(active_at);
        for connection in &mut self.connections {
            let mut message = std::mem::take(&mut connection.output);
            goodbye.encode(&mut message);
            let stream = &connection.stream;
            let result = stream
                .set_nonblocking(false)
                .and_then(|()| stream.set_write_timeout(Some(GOODBYE_TIME)))
                .and_then(|()| { stream }.write_all(&message));
            if let Err(error) = result {
                sys::log(format_args!("goodbye to {}: {error}", connection.id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_palette_entries_one_by_one() {
        let queries = color_queries();
        let text = String::from_utf8_lossy(&queries);
        assert_eq!(text.matches("\x1b]4;").count(), 256);
        assert!(text.contains("\x1b]4;255;?\x1b\\"));
    }

    fn sequence(title: &str, body: &str) -> String {
        let notification = Notification {
            title: title.to_owned(),
            body: body.to_owned(),
        };
        String::from_utf8(notification_sequence(&notification)).unwrap()
    }

    #[test]
    fn output_to_a_client_that_stopped_reading_stays_bounded() {
        let (stream, _other_end) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut connection = Connection {
            id: 0,
            stream,
            input: Vec::new(),
            output: Vec::new(),
            closing: false,
            closed: false,
            client: None,
        };
        let piece = vec![7; 64 * 1024];
        for _ in 0..1024 {
            connection.send_unless_behind(&piece);
        }
        assert!(connection.output.len() >= MAX_PENDING - piece.len());
        assert!(connection.output.len() < MAX_PENDING + 2 * piece.len());
    }

    #[test]
    fn notifications_keep_the_form_they_need() {
        assert_eq!(sequence("", "done"), "\x1b]9;done\x1b\\");
        assert_eq!(sequence("make", "done"), "\x1b]777;notify;make;done\x1b\\");
    }

    #[test]
    fn a_notification_cannot_turn_into_another_command() {
        assert_eq!(sequence("", "4;1;50"), "\x1b]777;notify;;4;1;50\x1b\\");
        assert_eq!(sequence("", "a\x1b]2;b"), "\x1b]9;a]2;b\x1b\\");
    }
}
