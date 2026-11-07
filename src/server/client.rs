//! What belongs to one terminal attached to the server, rather than to the
//! session they all show.

use std::ffi::c_int;
use std::time::Instant;

use super::dispatch::{Click, Drag};
use super::render::Renderer;
use super::ui::Ui;
use crate::input::Parser;

#[derive(Default)]
pub struct Client {
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
    /// Where the mouse pointer was last seen, once it has been.
    pub pointer: Option<(u16, u16)>,
}
