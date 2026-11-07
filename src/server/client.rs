//! What belongs to one terminal attached to the server, rather than to the
//! session they all show.

use std::ffi::c_int;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::Server;
use super::dispatch::{Click, Drag, listing};
use super::format::Context;
use super::render::{Canvas, Renderer, Viewer};
use super::session::{SESSION_NAME, Session};
use super::ui::{List, ListRow, Ui};
use crate::command::{Command, Detach};
use crate::ghostty::OuterColors;
use crate::input::Parser;
use crate::proto::{ClientInfo, ServerMessage};
use crate::sys::WinSize;

/// What `list-clients` shows by default, except for tmux's other users.
const LIST_CLIENTS: &str = "#{client_name}: #{session_name} \
     [#{client_width}x#{client_height} #{client_termname}] (#{client_flags})";

/// What `choose-client` shows, with the size where tmux has the time, as that is
/// what tells them apart.
const CHOOSE_CLIENT: &str = "#{client_name}: #{client_width}x#{client_height}: \
     session #{session_name}";

fn unix_seconds(time: SystemTime) -> u64 {
    let since = time.duration_since(UNIX_EPOCH);
    since.map_or(0, |since| since.as_secs())
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "these have nothing to do with each other"
)]
pub struct Client {
    pub info: ClientInfo,
    created: SystemTime,
    /// The size of its terminal.
    pub size: WinSize,
    /// What its terminal said about its colors, which starts out as what the
    /// session goes by. A terminal that came back from a restart is not asked
    /// about everything again, and the rest is on its way.
    pub colors: OuterColors,
    /// Whether its terminal has the keyboard, which it does until it says not.
    pub focused: bool,
    /// When its user last did something.
    pub active_at: Instant,
    /// There is something new to show, which may have to wait for the terminal.
    pub dirty: bool,
    pub last_frame: Instant,
    pub ui: Ui,
    pub renderer: Renderer,
    pub parser: Parser,
    /// When a lone escape byte came in, which the rest of a sequence may follow.
    pub escape_since: Option<Instant>,
    /// The prefix key was pressed, so the next key is a command.
    pub prefix: bool,
    /// Until when a repeatable binding works without the prefix.
    pub repeat_until: Option<Instant>,
    /// Keys kl acted on, whose release programs should not see either.
    pub swallowed: Vec<c_int>,
    pub drag: Drag,
    pub last_click: Option<Click>,
    /// Mouse reports come in pixels, as of the event being handled.
    pub pixel_mouse: bool,
    /// Where the mouse pointer was last seen on its terminal, once it has been.
    pub pointer: Option<(u16, u16)>,
}

impl Client {
    pub fn new(size: WinSize, colors: OuterColors, active_at: Instant, info: ClientInfo) -> Self {
        Self {
            info,
            created: SystemTime::now(),
            size,
            colors,
            focused: true,
            active_at,
            dirty: true,
            last_frame: Instant::now(),
            ui: Ui::default(),
            renderer: Renderer::default(),
            parser: Parser::default(),
            escape_since: None,
            prefix: false,
            repeat_until: None,
            swallowed: Vec::new(),
            drag: Drag::None,
            last_click: None,
            pixel_mouse: false,
            pointer: None,
        }
    }

    pub const fn cell_size(&self) -> (u16, u16) {
        self.size.cell_size()
    }

    /// Whether this is what tmux would take this name for: its terminal, with or
    /// without `/dev/`, and with the colon of a target if there is one.
    fn is_called(&self, name: &str) -> bool {
        let name = name.strip_suffix(':').unwrap_or(name);
        let tty = &self.info.tty;
        tty == name || tty.strip_prefix("/dev/") == Some(name)
    }

    /// What a format can ask of a client.
    pub fn value(&self, name: &str) -> Option<String> {
        let (cell_width, cell_height) = self.cell_size();
        let active = SystemTime::now().checked_sub(self.active_at.elapsed());
        Some(match name {
            "client_activity" => unix_seconds(active.unwrap_or(self.created)).to_string(),
            "client_cell_height" => cell_height.to_string(),
            "client_cell_width" => cell_width.to_string(),
            "client_created" => unix_seconds(self.created).to_string(),
            "client_flags" => {
                let focused = if self.focused { "focused," } else { "" };
                format!("attached,{focused}UTF-8")
            }
            "client_height" => self.size.rows.to_string(),
            "client_name" | "client_tty" => self.info.tty.clone(),
            "client_pid" => self.info.pid.to_string(),
            "client_session" => SESSION_NAME.to_owned(),
            "client_termname" => self.info.term.clone(),
            "client_width" => self.size.cols.to_string(),
            _ => return None,
        })
    }

    /// The bytes that bring its terminal up to date.
    pub fn frame(&mut self, canvas: &Canvas, session: &Session) -> Vec<u8> {
        let viewer = Viewer {
            ui: &self.ui,
            size: self.size,
            pointer: self.pointer,
        };
        self.renderer.frame(canvas, session, &viewer)
    }
}

impl Server {
    /// The connection of the client with this name, or else of the acting one.
    fn find_client(&self, name: Option<&str>) -> Result<u64, String> {
        let Some(name) = name else {
            let acting = self.acting.filter(|_| self.acting_client().is_some());
            return acting.ok_or_else(|| "no current client".to_owned());
        };
        let mut connections = self.connections.iter();
        let connection = connections.find(|connection| {
            let client = connection.client.as_ref();
            client.is_some_and(|client| client.is_called(name))
        });
        connection
            .map(|connection| connection.id)
            .ok_or_else(|| format!("can't find client: {name}"))
    }

    /// Whether there is a client for what makes no sense without one.
    pub fn require_client(&self) -> Result<(), String> {
        self.acting_client()
            .map(drop)
            .ok_or_else(|| "no current client".to_owned())
    }

    /// Says goodbye to a client. It takes [`Server::after_client_left`] to go on
    /// without it.
    pub fn detach_client(&mut self, id: u64) {
        if let Some(connection) = self.connection_mut(id)
            && connection.client.take().is_some()
        {
            let text = format!("[detached (from session {SESSION_NAME})]");
            connection.finish(&ServerMessage::Exit(text));
        }
    }

    pub fn detach_clients(&mut self, target: Option<&str>, who: Detach) -> Result<(), String> {
        let ids = match who {
            Detach::Client => vec![self.find_client(target)?],
            Detach::Others => {
                let kept = self.find_client(target)?;
                let mut ids = self.client_ids();
                ids.retain(|id| *id != kept);
                ids
            }
            Detach::All => self.client_ids(),
        };
        for id in ids {
            self.detach_client(id);
        }
        self.after_client_left();
        Ok(())
    }

    /// A context for each client, in the order they attached.
    fn client_contexts(&self) -> impl Iterator<Item = Context<'_>> {
        self.clients().map(|client| Context {
            client: Some(client),
            ..Context::current(&self.session)
        })
    }

    pub fn list_clients(&self, format: Option<&str>, filter: Option<&str>) -> String {
        let format = format.unwrap_or(LIST_CLIENTS);
        listing(self.client_contexts(), format, filter)
    }

    /// Lets the user pick a client to detach from a list, on the row given.
    pub fn choose_client(&mut self, selected: usize) {
        let row = |context: Context<'_>| {
            let detach = Command::DetachClient {
                session: None,
                target: context.client.map(|client| client.info.tty.clone()),
                who: Detach::Client,
            };
            ListRow::new(context.expand(CHOOSE_CLIENT), detach.clone()).with_delete(detach)
        };
        let rows: Vec<ListRow> = self.client_contexts().map(row).collect();
        let selected = selected.min(rows.len().saturating_sub(1));
        self.client().ui.list = (!rows.is_empty()).then(|| List::new(rows, selected));
    }
}
