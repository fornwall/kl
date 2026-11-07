//! What belongs to one terminal attached to the server, rather than to the
//! session they all show.

use std::ffi::c_int;
use std::time::Instant;

use super::dispatch::{Click, Drag};
use super::render::{Canvas, Renderer, Viewer};
use super::session::Session;
use super::ui::Ui;
use crate::ghostty::OuterColors;
use crate::input::Parser;
use crate::sys::WinSize;

#[expect(
    clippy::struct_excessive_bools,
    reason = "these have nothing to do with each other"
)]
pub struct Client {
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
    pub fn new(size: WinSize, colors: OuterColors, active_at: Instant) -> Self {
        Self {
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
