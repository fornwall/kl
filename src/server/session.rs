//! The single session: its windows and panes.

use std::collections::BTreeMap;

use super::layout::{self, Node, PaneId, Rect};
use super::pane::{Pane, Spawn};
use crate::command::{Direction, SplitKind};
use crate::ghostty::{Progress, Rgb};
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

impl Window {
    /// The panes on display in the given area.
    fn visible(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        if self.zoomed {
            vec![(self.active, area)]
        } else {
            self.root.panes(area)
        }
    }
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
    /// What new panes get of the environment of the latest client, in the form
    /// of [`crate::proto::ClientMessage::Attach`].
    pub environment: Vec<String>,
    /// A pane reported progress, which the outer terminal wants to hear even if
    /// it is the same as before: it gives up on a report that stays quiet.
    pub progress_reported: bool,
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
            environment: Vec::new(),
            progress_reported: false,
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
        self.window()
            .map_or_else(Vec::new, |window| window.visible(self.area()))
    }

    /// Whether this pane is on the screen right now.
    pub fn is_visible(&self, pane: PaneId) -> bool {
        self.visible_panes().iter().any(|(id, _)| *id == pane)
    }

    /// The progress to show, as there is room for just one: that of the active
    /// pane, or failing that of another pane that can be seen.
    pub fn progress(&self) -> Option<Progress> {
        let of = |id: &PaneId| self.panes.get(id)?.terminal.progress();
        let visible = self.visible_panes();
        self.active_id()
            .and_then(|id| of(&id))
            .or_else(|| visible.iter().find_map(|(id, _)| of(id)))
    }

    /// Fits every pane of every window to the current size.
    pub fn relayout(&mut self) {
        let area = self.area();
        let cell_size = self.cell_size();
        for window in &self.windows {
            for (id, rect) in window.visible(area) {
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

    fn spawn_pane(&mut self, program: &[String], size: Rect) -> Result<PaneId, String> {
        let id = self.next_pane;
        let cwd = self.active_pane().and_then(Pane::working_directory);
        let spawn = Spawn {
            program,
            cwd: cwd.as_deref(),
            socket: &self.socket,
            environment: &self.environment,
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
        let Some(position) = self.windows.iter().position(|w| w.root.contains(id)) else {
            return;
        };
        let window = &mut self.windows[position];
        let root = std::mem::replace(&mut window.root, Node::Leaf(id));
        let Some(root) = root.remove(id) else {
            let index = self.windows.remove(position).index;
            self.after_window_removed(index);
            return;
        };
        window.root = root;
        window.zoomed = false;
        if window.last_active == Some(id) {
            window.last_active = None;
        }
        if window.active == id {
            let next = window.last_active.take();
            window.active = next.unwrap_or_else(|| window.root.first_pane());
            if window.index == self.current {
                self.report_focus(true);
            }
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
                self.select_window(next);
            }
        }
        self.relayout();
    }

    pub fn kill_current_window(&mut self) {
        let position = self.windows.iter().position(|w| w.index == self.current);
        let Some(position) = position else {
            return;
        };
        let window = self.windows.remove(position);
        for (id, _) in window.root.panes(Rect::default()) {
            self.panes.remove(&id);
        }
        self.after_window_removed(window.index);
    }

    /// Tells the active pane that it gained or lost the keyboard.
    pub fn report_focus(&mut self, focused: bool) {
        if let Some(pane) = self.active_pane_mut() {
            pane.report_focus(focused);
        }
    }

    pub fn select_window(&mut self, index: u32) -> bool {
        if !self.windows.iter().any(|w| w.index == index) {
            return false;
        }
        if self.current != index {
            self.report_focus(false);
            self.last =
                Some(self.current).filter(|old| self.windows.iter().any(|w| w.index == *old));
            self.current = index;
            self.report_focus(true);
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
        let selectable = |window: &Window| window.active != id && window.root.contains(id);
        if !self.window().is_some_and(selectable) {
            return;
        }
        self.report_focus(false);
        if let Some(window) = self.window_mut() {
            window.last_active = Some(window.active);
            window.active = id;
        }
        self.report_focus(true);
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
