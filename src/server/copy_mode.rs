//! Scrolling through a pane's history and selecting text in it with the
//! keyboard. The viewport and selection themselves live in the terminal.

use super::pane::Pane;
use crate::ghostty::key;
use crate::input::KeyEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyMode {
    /// The cursor, relative to the viewport.
    pub x: u16,
    pub y: u16,
    pub selecting: bool,
    /// Entered by scrolling the wheel, so scrolling back down ends it.
    pub exit_at_bottom: bool,
}

/// What should happen after a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Cancel,
    /// Copy the selection, then leave.
    Copy,
}

impl CopyMode {
    /// Starts with the cursor where the pane has its own.
    pub fn enter(pane: &mut Pane, exit_at_bottom: bool) {
        if pane.copy_mode.is_some() {
            return;
        }
        pane.terminal.refresh();
        let position = pane.terminal.cursor().and_then(|cursor| cursor.position);
        let (x, y) = position.unwrap_or_else(|| (0, pane.rect.height.saturating_sub(1)));
        pane.copy_mode = Some(Self {
            x,
            y,
            selecting: false,
            exit_at_bottom,
        });
        pane.redraw = true;
    }

    pub fn exit(pane: &mut Pane) {
        pane.copy_mode = None;
        pane.terminal.clear_selection();
        pane.terminal.scroll_to_bottom();
        pane.redraw = true;
    }
}

/// Moves the cursor by some rows, scrolling when it reaches an edge.
fn move_rows(pane: &mut Pane, mode: &mut CopyMode, rows: i32) {
    let height = i32::from(pane.rect.height.max(1));
    let target = i32::from(mode.y) + rows;
    let clamped = target.clamp(0, height - 1);
    let overflow = target - clamped;
    if overflow != 0 {
        pane.terminal.scroll_by(overflow);
    }
    mode.y = u16::try_from(clamped).unwrap_or(0);
}

fn move_cols(pane: &Pane, mode: &mut CopyMode, cols: i32) {
    let width = i32::from(pane.rect.width.max(1));
    let target = (i32::from(mode.x) + cols).clamp(0, width - 1);
    mode.x = u16::try_from(target).unwrap_or(0);
}

/// The keys of copy mode's vi-style table that do not end it.
fn handle_movement(pane: &mut Pane, mode: &mut CopyMode, event: &KeyEvent) {
    let height = i32::from(pane.rect.height.max(1));
    match (event.key, event.char()) {
        _ if event.is_ctrl('f') => pane.terminal.scroll_by(height),
        _ if event.is_ctrl('u') => pane.terminal.scroll_by(-height / 2),
        _ if event.is_ctrl('d') => pane.terminal.scroll_by(height / 2),
        (key::ARROW_LEFT, _) | (_, Some('h')) => move_cols(pane, mode, -1),
        (key::ARROW_RIGHT, _) | (_, Some('l')) => move_cols(pane, mode, 1),
        (key::ARROW_UP, _) | (_, Some('k')) => move_rows(pane, mode, -1),
        (key::ARROW_DOWN, _) | (_, Some('j')) => move_rows(pane, mode, 1),
        (key::PAGE_UP, _) => pane.terminal.scroll_by(-height),
        (key::PAGE_DOWN, _) => pane.terminal.scroll_by(height),
        (key::HOME, _) | (_, Some('0')) => mode.x = 0,
        (key::END, _) | (_, Some('$')) => mode.x = pane.rect.width.saturating_sub(1),
        (_, Some('g')) => pane.terminal.scroll_to_top(),
        (_, Some('G')) => pane.terminal.scroll_to_bottom(),
        (key::SPACE, _) | (_, Some('v')) => {
            mode.selecting = true;
            pane.terminal.select_start(mode.x, mode.y);
        }
        _ => {}
    }
}

/// Handles a key press in a pane in copy mode.
pub fn handle_key(pane: &mut Pane, event: &KeyEvent) -> Outcome {
    let Some(mut mode) = pane.copy_mode else {
        return Outcome::Cancel;
    };
    let outcome = match (event.key, event.char()) {
        _ if event.is_ctrl('c') => Outcome::Cancel,
        (key::ESCAPE, _) | (_, Some('q')) => Outcome::Cancel,
        (key::ENTER, _) | (_, Some('y')) => Outcome::Copy,
        _ => {
            handle_movement(pane, &mut mode, event);
            Outcome::Stay
        }
    };
    if mode.selecting {
        pane.terminal.select_extend(mode.x, mode.y);
    }
    pane.copy_mode = Some(mode);
    pane.redraw = true;
    outcome
}
