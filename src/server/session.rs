//! The single session: its windows and panes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::layout::{self, Node, PaneId, Rect};
use super::pane::{Pane, Spawn};
use crate::command::{Direction, SplitKind};
use crate::ghostty::Rgb;
use crate::sys::WinSize;

pub struct Window {
    pub index: u32,
    pub name: String,
    /// Whether the name follows what is running, until set by hand.
    pub automatic_name: bool,
    pub root: Node,
    pub active: PaneId,
    pub last_active: Option<PaneId>,
    pub zoomed: bool,
    /// A pane rang the bell while this window was not being looked at.
    pub bell: bool,
}

pub struct Session {
    /// Sorted by index.
    pub windows: Vec<Window>,
    pub panes: BTreeMap<PaneId, Pane>,
    /// The index of the window on display.
    pub current: u32,
    pub last: Option<u32>,
    /// The size of the attached client's terminal.
    pub size: WinSize,
    /// The layout changed, so the whole screen must be composed again.
    pub repaint: bool,
    /// The outer terminal's default foreground and background.
    pub colors: (Option<Rgb>, Option<Rgb>),
    socket: String,
    next_pane: PaneId,
}

impl Session {
    pub const fn new(socket: String) -> Self {
        Self {
            windows: Vec::new(),
            panes: BTreeMap::new(),
            current: 0,
            last: None,
            size: WinSize {
                cols: 80,
                rows: 24,
                width_px: 0,
                height_px: 0,
            },
            repaint: true,
            colors: (None, None),
            socket,
            next_pane: 0,
        }
    }

    /// The area for panes. The last row belongs to the status line.
    pub const fn area(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.size.cols,
            height: self.size.rows.saturating_sub(1),
        }
    }

    fn cell_size(&self) -> (u16, u16) {
        (
            self.size.width_px.checked_div(self.size.cols).unwrap_or(0),
            self.size.height_px.checked_div(self.size.rows).unwrap_or(0),
        )
    }

    pub fn window(&self) -> Option<&Window> {
        self.windows.iter().find(|w| w.index == self.current)
    }

    pub fn window_mut(&mut self) -> Option<&mut Window> {
        let current = self.current;
        self.windows.iter_mut().find(|w| w.index == current)
    }

    pub fn active_id(&self) -> Option<PaneId> {
        self.window().map(|window| window.active)
    }

    pub fn active_pane(&self) -> Option<&Pane> {
        self.panes.get(&self.active_id()?)
    }

    pub fn active_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.active_id()?;
        self.panes.get_mut(&id)
    }

    /// The panes of the current window that can be seen, with their areas.
    pub fn visible_panes(&self) -> Vec<(PaneId, Rect)> {
        let Some(window) = self.window() else {
            return Vec::new();
        };
        if window.zoomed {
            vec![(window.active, self.area())]
        } else {
            window.root.panes(self.area())
        }
    }

    fn window_of(&self, pane: PaneId) -> Option<u32> {
        self.windows
            .iter()
            .find(|window| window.root.contains(pane))
            .map(|window| window.index)
    }

    /// Whether this pane is on the screen right now.
    pub fn is_visible(&self, pane: PaneId) -> bool {
        self.visible_panes().iter().any(|(id, _)| *id == pane)
    }

    /// Fits every pane of every window to the current size.
    pub fn relayout(&mut self) {
        let area = self.area();
        let cell_size = self.cell_size();
        for window in &self.windows {
            let placements = if window.zoomed {
                vec![(window.active, area)]
            } else {
                window.root.panes(area)
            };
            for (id, rect) in placements {
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.place(rect, cell_size);
                }
            }
        }
        self.repaint = true;
    }

    pub fn resize(&mut self, size: WinSize) {
        if size != self.size {
            self.size = size;
            self.relayout();
        }
    }

    /// The directory for a new pane, which follows the active pane.
    fn new_pane_cwd(&self) -> Option<PathBuf> {
        self.active_pane()?.working_directory()
    }

    fn spawn_pane(&mut self, program: &[String], size: Rect) -> Result<PaneId, String> {
        let id = self.next_pane;
        let cwd = self.new_pane_cwd();
        let spawn = Spawn {
            program,
            cwd: cwd.as_deref(),
            socket: &self.socket,
        };
        let size = WinSize {
            cols: size.width,
            rows: size.height,
            width_px: 0,
            height_px: 0,
        };
        let pane = Pane::spawn(id, &spawn, size, self.colors)
            .map_err(|error| format!("cannot start a pane: {error}"))?;
        self.panes.insert(id, pane);
        self.next_pane += 1;
        Ok(id)
    }

    fn free_index(&self) -> u32 {
        (0..=u32::MAX)
            .find(|index| self.windows.iter().all(|w| w.index != *index))
            .unwrap_or(0)
    }

    pub fn new_window(&mut self, name: Option<String>, program: &[String]) -> Result<(), String> {
        let id = self.spawn_pane(program, self.area())?;
        let index = self.free_index();
        let window = Window {
            index,
            automatic_name: name.is_none(),
            name: name.unwrap_or_else(|| "kl".to_owned()),
            root: Node::Leaf(id),
            active: id,
            last_active: None,
            zoomed: false,
            bell: false,
        };
        let position = self.windows.partition_point(|w| w.index < index);
        self.windows.insert(position, window);
        self.select_window(index);
        self.relayout();
        Ok(())
    }

    pub fn split(&mut self, kind: SplitKind, program: &[String]) -> Result<(), String> {
        let area = self.area();
        let Some(window) = self.window() else {
            return Err("no current window".to_owned());
        };
        let target = window.active;
        let panes = window.root.panes(area);
        let rect = panes
            .iter()
            .find(|(id, _)| *id == target)
            .map_or(area, |(_, rect)| *rect);
        let room = match kind {
            SplitKind::Horizontal => rect.width,
            SplitKind::Vertical => rect.height,
        };
        // Two panes of a cell each, and the line between them.
        if room < 3 {
            return Err("no space for a new pane".to_owned());
        }
        let id = self.spawn_pane(program, rect)?;
        if let Some(window) = self.window_mut() {
            window.zoomed = false;
            window.root.split(target, id, kind);
        }
        self.select_pane(id);
        self.relayout();
        Ok(())
    }

    /// Takes a pane out of its window, and closes the window if that was the
    /// last one in it.
    pub fn remove_pane(&mut self, id: PaneId) {
        self.panes.remove(&id);
        let Some(index) = self.window_of(id) else {
            return;
        };
        let Some(position) = self.windows.iter().position(|w| w.index == index) else {
            return;
        };
        let window = &mut self.windows[position];
        let root = std::mem::replace(&mut window.root, Node::Leaf(id));
        let Some(root) = root.remove(id) else {
            self.windows.remove(position);
            self.after_window_removed(index);
            return;
        };
        window.root = root;
        window.zoomed = false;
        if window.last_active == Some(id) {
            window.last_active = None;
        }
        if window.active == id {
            let fallback = window
                .root
                .panes(Rect::default())
                .first()
                .map(|(id, _)| *id);
            let next = window.last_active.take().or(fallback);
            window.active = next.unwrap_or(id);
        }
        self.relayout();
    }

    fn after_window_removed(&mut self, index: u32) {
        if self.last == Some(index) {
            self.last = None;
        }
        if self.current == index {
            let next = self.last.take().or_else(|| {
                let after = self.windows.iter().find(|w| w.index > index);
                after.or_else(|| self.windows.last()).map(|w| w.index)
            });
            if let Some(next) = next {
                self.current = next;
            }
        }
        self.relayout();
    }

    /// Closes all panes of a window, returning them to be disposed of.
    pub fn kill_current_window(&mut self) {
        let Some(window) = self.window() else {
            return;
        };
        let panes: Vec<PaneId> = window
            .root
            .panes(Rect::default())
            .iter()
            .map(|(id, _)| *id)
            .collect();
        for id in panes {
            self.remove_pane(id);
        }
    }

    pub fn select_window(&mut self, index: u32) -> bool {
        if !self.windows.iter().any(|w| w.index == index) {
            return false;
        }
        if self.current != index {
            self.last =
                Some(self.current).filter(|old| self.windows.iter().any(|w| w.index == *old));
            self.current = index;
        }
        if let Some(window) = self.window_mut() {
            window.bell = false;
        }
        self.repaint = true;
        true
    }

    fn step_window(&mut self, forward: bool) {
        let position = self.windows.iter().position(|w| w.index == self.current);
        let (Some(position), count) = (position, self.windows.len()) else {
            return;
        };
        let target = if forward {
            (position + 1) % count
        } else {
            (position + count - 1) % count
        };
        if let Some(window) = self.windows.get(target) {
            self.select_window(window.index);
        }
    }

    pub fn next_window(&mut self) {
        self.step_window(true);
    }

    pub fn previous_window(&mut self) {
        self.step_window(false);
    }

    pub fn select_pane(&mut self, id: PaneId) {
        let Some(window) = self.window_mut() else {
            return;
        };
        if window.active == id || !window.root.contains(id) {
            return;
        }
        let old = window.active;
        window.last_active = Some(old);
        window.active = id;
        // Programs that asked for it should know they lost or gained the keyboard.
        for (pane, focused) in [(old, false), (id, true)] {
            if let Some(pane) = self.panes.get_mut(&pane) {
                let report = pane.terminal.encode_focus(focused);
                pane.send(&report);
            }
        }
        self.repaint = true;
    }

    pub fn select_direction(&mut self, direction: Direction) {
        let area = self.area();
        let Some(window) = self.window() else {
            return;
        };
        let target = layout::neighbor(&window.root.panes(area), window.active, direction, area);
        if let Some(target) = target {
            self.unzoom();
            self.select_pane(target);
        }
    }

    pub fn select_next_pane(&mut self) {
        let Some(window) = self.window() else {
            return;
        };
        let panes = window.root.panes(self.area());
        let position = panes.iter().position(|(id, _)| *id == window.active);
        let next = position.map_or(0, |position| (position + 1) % panes.len());
        if let Some((id, _)) = panes.get(next).copied() {
            self.unzoom();
            self.select_pane(id);
        }
    }

    pub fn select_last_pane(&mut self) {
        if let Some(last) = self.window().and_then(|window| window.last_active) {
            self.unzoom();
            self.select_pane(last);
        }
    }

    fn unzoom(&mut self) {
        if let Some(window) = self.window_mut()
            && std::mem::take(&mut window.zoomed)
        {
            self.relayout();
        }
    }

    pub fn toggle_zoom(&mut self) {
        let Some(window) = self.window_mut() else {
            return;
        };
        if matches!(window.root, Node::Leaf(_)) {
            return;
        }
        window.zoomed = !window.zoomed;
        self.relayout();
    }

    pub fn resize_pane(&mut self, direction: Direction, cells: u16) {
        let area = self.area();
        if let Some(window) = self.window_mut() {
            window.zoomed = false;
            let active = window.active;
            window.root.resize(active, area, direction, cells);
            self.relayout();
        }
    }
}
