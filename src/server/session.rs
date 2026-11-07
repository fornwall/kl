//! The single session: its windows and panes.

use std::collections::BTreeMap;
use std::path::Path;

use super::layout::{self, Layout, Node, PaneId, Rect};
use super::pane::{self, Pane, Spawn};
use crate::command::{Direction, Place};
use crate::ghostty::{DEFAULT_POINTER_SHAPE, Link, MODE_SGR_PIXELS, OuterColors, Progress};
use crate::sys::WinSize;

/// What `#S` says, and the name a target can use for the session.
pub const SESSION_NAME: &str = "kl";

pub struct Window {
    /// Identifies a window for the life of the server, unlike its index.
    pub id: u32,
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
    pub monitor: Monitor,
    /// The preset layout applied last.
    pub layout: Option<Layout>,
    /// What looks like it is taking over the foreground, for [`Window::follow`].
    pub next_name: Option<String>,
}

/// Whether output in a window is flagged, as tmux's `monitor-activity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Monitor {
    Off,
    Watching,
    /// A pane wrote something while this window was not being looked at.
    Flagged,
}

/// What came of a look at what runs in a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameCheck {
    Unchanged,
    Changed,
    /// Something else is running, which it takes another look to believe.
    LookAgain,
}

impl Window {
    /// Follows what runs in the foreground. A name has to be seen twice in a
    /// row, or what a shell runs on its way to a prompt would make it flicker.
    pub fn follow(&mut self, name: String) -> NameCheck {
        if name == self.name {
            self.next_name = None;
            NameCheck::Unchanged
        } else if self.next_name.as_ref() == Some(&name) {
            self.next_name = None;
            self.name = name;
            NameCheck::Changed
        } else {
            self.next_name = Some(name);
            NameCheck::LookAgain
        }
    }

    pub fn has_activity(&self) -> bool {
        self.monitor == Monitor::Flagged
    }

    /// The panes in the order that layouts place them. A mirrored layout has the
    /// main pane at the end, where tmux keeps it in front wherever it is drawn.
    fn layout_order(&self, area: Rect) -> Vec<PaneId> {
        let mut panes: Vec<PaneId> = self.root.panes(area).iter().map(|pane| pane.0).collect();
        if self.layout.is_some_and(Layout::is_mirrored)
            && let Node::Split { children, .. } = &self.root
            && let Some(main) = children.last()
        {
            panes.rotate_right(main.panes(area).len());
        }
        panes
    }

    /// The panes on display in the given area.
    fn visible(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        if self.zoomed {
            vec![(self.active, area)]
        } else {
            self.root.panes(area)
        }
    }
}

/// Takes note of `out` having gone from a window and `into` coming in its place,
/// or of the two having changed places if they are both in it.
fn swap_active(window: &mut Window, out: PaneId, into: PaneId, detached: bool, same_window: bool) {
    let was_active = window.active;
    if same_window && detached {
        if window.active == out {
            window.active = into;
        } else if window.active == into {
            window.active = out;
        }
    } else if window.active == out || !detached {
        window.active = into;
    }
    if window.active != was_active {
        window.last_active = Some(was_active).filter(|id| window.root.contains(*id));
    }
    if window.last_active == Some(window.active) {
        window.last_active = None;
    }
    if !same_window && window.last_active == Some(out) {
        window.last_active = None;
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
    pub colors: OuterColors,
    /// What new panes get of the environment of the latest client, in the form
    /// of [`crate::proto::ClientMessage::Attach`].
    pub environment: Vec<String>,
    /// Mouse reports come in pixels, as of the event being handled.
    pub pixel_mouse: bool,
    /// Where the mouse pointer was last seen, once it has been.
    pub pointer: Option<(u16, u16)>,
    /// A pane reported progress, which the outer terminal wants to hear even if
    /// it is the same as before: it gives up on a report that stays quiet.
    pub progress_reported: bool,
    pub socket: String,
    pub next_pane: PaneId,
    pub next_window: u32,
    /// The pane the command being run came from, if it was run from a pane.
    pub origin: Option<PaneId>,
    /// The pane `select-pane -m` marked, which may be gone by now.
    pub marked: Option<PaneId>,
}

impl Session {
    pub fn new(socket: String) -> Self {
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
            colors: OuterColors::default(),
            environment: Vec::new(),
            pixel_mouse: false,
            pointer: None,
            progress_reported: false,
            socket,
            next_pane: 0,
            next_window: 0,
            origin: None,
            marked: None,
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

    pub fn cell_size(&self) -> (u16, u16) {
        (
            self.size.width_px.checked_div(self.size.cols).unwrap_or(0),
            self.size.height_px.checked_div(self.size.rows).unwrap_or(0),
        )
    }

    pub fn window(&self) -> Option<&Window> {
        self.window_at(self.current)
    }

    pub fn window_mut(&mut self) -> Option<&mut Window> {
        self.window_at_mut(self.current)
    }

    pub fn window_at(&self, index: u32) -> Option<&Window> {
        self.windows.iter().find(|w| w.index == index)
    }

    pub fn window_at_mut(&mut self, index: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.index == index)
    }

    /// The window a pane is in.
    pub fn window_of(&self, pane: PaneId) -> Option<&Window> {
        self.windows.iter().find(|w| w.root.contains(pane))
    }

    /// The window a pane is in.
    pub fn window_of_mut(&mut self, pane: PaneId) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.root.contains(pane))
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

    /// The pane on display at a position of the screen.
    pub fn pane_at(&self, x: u16, y: u16) -> Option<PaneId> {
        let panes = self.visible_panes();
        let pane = panes.iter().find(|(_, rect)| rect.contains(x, y));
        pane.map(|(id, _)| *id)
    }

    /// The shape for the pointer, which is about what is under it: what that
    /// pane asked for, or the active pane's while the pointer has not been seen.
    pub fn pointer_shape(&self) -> &'static str {
        let id = self
            .pointer
            .map_or_else(|| self.active_id(), |(x, y)| self.pane_at(x, y));
        let pane = id.and_then(|id| self.panes.get(&id));
        pane.map_or(DEFAULT_POINTER_SHAPE, |pane| pane.terminal.pointer_shape())
    }

    /// Whether a program on display wants the mouse in pixels. Making anything
    /// of those takes knowing the size of a cell.
    pub fn wants_pixel_mouse(&self) -> bool {
        let wants = |id: &PaneId| {
            let pane = self.panes.get(id);
            pane.is_some_and(|pane| pane.terminal.mode(MODE_SGR_PIXELS))
        };
        let (width, height) = self.cell_size();
        width > 0 && height > 0 && self.visible_panes().iter().any(|(id, _)| wants(id))
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

    /// Passes on what the outer terminal said about its colors.
    pub fn apply_colors(&mut self) {
        for pane in self.panes.values_mut() {
            pane.set_outer_colors(&self.colors);
        }
    }

    pub fn marked_pane(&self) -> Option<PaneId> {
        self.marked.filter(|id| self.panes.contains_key(id))
    }

    /// The marks after a window's name on the status line.
    pub fn window_flags(&self, window: &Window) -> String {
        let mut flags = String::new();
        if window.has_activity() {
            flags.push('#');
        }
        if window.bell {
            flags.push('!');
        }
        if window.index == self.current {
            flags.push('*');
        } else if self.last == Some(window.index) {
            flags.push('-');
        }
        if self
            .marked_pane()
            .is_some_and(|id| window.root.contains(id))
        {
            flags.push('M');
        }
        if window.zoomed {
            flags.push('Z');
        }
        flags
    }

    /// The URI of a link in a pane.
    pub fn link_uri(&self, link: Link) -> Option<&[u8]> {
        let mut panes = self.panes.values();
        panes.find_map(|pane| pane.terminal.link_uri(link))
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

    /// Starts a program in `directory`, or else where the pane `from` is at.
    fn spawn_pane(
        &mut self,
        program: &[String],
        size: Rect,
        directory: Option<&Path>,
        from: Option<PaneId>,
    ) -> Result<PaneId, String> {
        let id = self.next_pane;
        let from = from.and_then(|from| self.panes.get(&from));
        let cwd = from.and_then(Pane::working_directory);
        let cwd = directory.or(cwd.as_deref());
        let spawn = Spawn {
            program,
            cwd,
            socket: &self.socket,
            environment: &self.environment,
        };
        let size = WinSize {
            cols: size.width,
            rows: size.height,
            width_px: 0,
            height_px: 0,
        };
        let pane = Pane::spawn(id, &spawn, size, &self.colors)
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

    /// Opens a window at `index`, or else at the first one free, and returns
    /// its pane.
    pub fn new_window(
        &mut self,
        name: Option<String>,
        program: &[String],
        directory: Option<&Path>,
        index: Option<u32>,
        detached: bool,
    ) -> Result<PaneId, String> {
        let id = self.spawn_pane(program, self.area(), directory, self.current_pane())?;
        let index = index.unwrap_or_else(|| self.free_index());
        let window = Window {
            id: self.next_window,
            index,
            automatic_name: name.is_none(),
            name: name.unwrap_or_else(|| pane::default_name(program)),
            root: Node::Leaf(id),
            active: id,
            last_active: None,
            zoomed: false,
            bell: false,
            monitor: Monitor::Off,
            layout: None,
            next_name: None,
        };
        self.next_window += 1;
        let position = self.windows.partition_point(|w| w.index < index);
        self.windows.insert(position, window);
        if !detached || self.windows.len() == 1 {
            self.select_window(index);
        }
        self.relayout();
        Ok(id)
    }

    /// Opens a pane next to `target`, and returns it.
    pub fn split(
        &mut self,
        target: PaneId,
        place: Place,
        program: &[String],
        directory: Option<&Path>,
        detached: bool,
    ) -> Result<PaneId, String> {
        let area = self.area();
        let Some(window) = self.window_of(target) else {
            return Err("no such pane".to_owned());
        };
        let index = window.index;
        let panes = window.root.panes(area);
        let rect = panes
            .iter()
            .find(|(id, _)| *id == target && !place.full)
            .map_or(area, |(_, rect)| *rect);
        // Two panes of a cell each, and the line between them.
        if rect.extent(place.kind) < 3 {
            return Err("no space for a new pane".to_owned());
        }
        let id = self.spawn_pane(program, rect, directory, Some(target))?;
        if let Some(window) = self.window_at_mut(index) {
            window.zoomed = false;
            window.root.place(target, id, place, area);
        }
        if !detached {
            self.select_pane(id);
        }
        self.relayout();
        Ok(id)
    }

    /// Ends a pane, and closes its window if that was the last one in it.
    pub fn remove_pane(&mut self, id: PaneId) {
        self.panes.remove(&id);
        self.detach_pane(id);
    }

    /// Takes a pane out of its window, and closes the window if that was the
    /// last one in it.
    fn detach_pane(&mut self, id: PaneId) {
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

    pub fn kill_window(&mut self, index: u32) {
        let position = self.windows.iter().position(|w| w.index == index);
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
            if window.monitor == Monitor::Flagged {
                window.monitor = Monitor::Watching;
            }
        }
        self.repaint = true;
        true
    }

    /// Goes to the next or previous window, or the nearest one with an alert.
    /// Returns whether there was another window to go to.
    fn step_window(&mut self, forward: bool, alert: bool) -> bool {
        let count = self.windows.len();
        let Some(position) = self.windows.iter().position(|w| w.index == self.current) else {
            return false;
        };
        let target = (1..count)
            .map(|step| {
                let step = if forward { step } else { count - step };
                (position + step) % count
            })
            .filter_map(|position| self.windows.get(position))
            .find(|window| !alert || window.bell || window.has_activity())
            .map(|window| window.index);
        target.is_some_and(|index| self.select_window(index))
    }

    pub fn next_window(&mut self, alert: bool) -> bool {
        self.step_window(true, alert)
    }

    pub fn previous_window(&mut self, alert: bool) -> bool {
        self.step_window(false, alert)
    }

    /// Makes a pane the active one of its window.
    pub fn select_pane(&mut self, id: PaneId) {
        let Some(window) = self.window_of(id).filter(|window| window.active != id) else {
            return;
        };
        let index = window.index;
        let current = index == self.current;
        if current {
            self.report_focus(false);
        }
        if let Some(window) = self.window_at_mut(index) {
            window.last_active = Some(window.active);
            window.active = id;
        }
        if current {
            self.report_focus(true);
        }
        self.repaint = true;
    }

    /// Puts two panes in each other's place, which can be in different windows.
    /// The target stays or becomes the active one, unless `detached`: then what
    /// is active stays where it is on the screen.
    pub fn swap_panes(&mut self, source: PaneId, target: PaneId, detached: bool) {
        let index = |id| self.window_of(id).map(|window| window.index);
        let (Some(source_window), Some(target_window)) = (index(source), index(target)) else {
            return;
        };
        if source == target {
            return;
        }
        self.unzoom(source_window);
        self.unzoom(target_window);
        let before = self.active_id();
        for window in &mut self.windows {
            if window.index == source_window || window.index == target_window {
                window.root.swap(source, target);
                let (out, into) = if window.index == source_window {
                    (source, target)
                } else {
                    (target, source)
                };
                swap_active(window, out, into, detached, source_window == target_window);
            }
        }
        self.report_focus_change(before);
        self.relayout();
    }

    /// Tells panes about the active one having changed from this one.
    fn report_focus_change(&mut self, before: Option<PaneId>) {
        let after = self.active_id();
        if before != after {
            if let Some(pane) = before.and_then(|id| self.panes.get_mut(&id)) {
                pane.report_focus(false);
            }
            self.report_focus(true);
        }
    }

    /// Moves a pane to a window of its own, at `index` or else at the first one
    /// free. A pane alone in its window takes the window along.
    pub fn break_pane(
        &mut self,
        id: PaneId,
        name: Option<String>,
        index: Option<u32>,
        detached: bool,
    ) -> Result<(), String> {
        let new_index = index.unwrap_or_else(|| self.free_index());
        if self.window_at(new_index).is_some() {
            return Err(format!("index in use: {new_index}"));
        }
        let Some(position) = self.windows.iter().position(|w| w.root.contains(id)) else {
            return Err("no such pane".to_owned());
        };
        let window = if matches!(self.windows[position].root, Node::Leaf(_)) {
            let mut window = self.windows.remove(position);
            if self.current == window.index {
                self.current = new_index;
            }
            window.index = new_index;
            if let Some(name) = name {
                window.name = name;
                window.automatic_name = false;
            }
            window
        } else {
            self.detach_pane(id);
            let foreground = self.panes.get(&id).and_then(Pane::foreground_name);
            let window = Window {
                id: self.next_window,
                index: new_index,
                automatic_name: name.is_none(),
                name: name
                    .or(foreground)
                    .unwrap_or_else(|| pane::default_name(&[])),
                root: Node::Leaf(id),
                active: id,
                last_active: None,
                zoomed: false,
                bell: false,
                monitor: Monitor::Off,
                layout: None,
                next_name: None,
            };
            self.next_window += 1;
            window
        };
        let position = self.windows.partition_point(|w| w.index < new_index);
        self.windows.insert(position, window);
        if !detached {
            self.select_window(new_index);
        }
        self.relayout();
        Ok(())
    }

    /// Moves a pane next to another one, as `place` says.
    pub fn join_pane(
        &mut self,
        source: PaneId,
        target: PaneId,
        place: Place,
        detached: bool,
    ) -> Result<(), String> {
        if source == target {
            return Err("source and target panes must be different".to_owned());
        }
        let area = self.area();
        let Some(window) = self.window_of(target) else {
            return Err("no such pane".to_owned());
        };
        let panes = window.root.panes(area);
        let rect = panes
            .iter()
            .find(|(id, _)| *id == target && !place.full)
            .map_or(area, |(_, rect)| *rect);
        if rect.extent(place.kind) < 3 {
            return Err("no space for a new pane".to_owned());
        }
        self.detach_pane(source);
        let index = self.window_of(target).map(|window| window.index);
        if let Some(window) = self.window_of_mut(target) {
            window.zoomed = false;
            window.root.place(target, source, place, area);
        }
        self.marked = None;
        if !detached {
            self.select_pane(source);
            if let Some(index) = index {
                self.select_window(index);
            }
        }
        self.relayout();
        Ok(())
    }

    /// Applies a preset layout to a window.
    pub fn select_layout(&mut self, index: u32, layout: Layout) {
        let area = self.area();
        if let Some(window) = self.window_at_mut(index) {
            let panes = window.layout_order(area);
            if let Some(root) = layout.arrange(&panes, area) {
                window.root = root;
                window.layout = Some(layout);
                window.zoomed = false;
                self.relayout();
            }
        }
    }

    /// Moves every pane on to the next place in its window, where the active
    /// place stays the active one.
    pub fn rotate_window(&mut self, index: u32, down: bool) {
        let area = self.area();
        let before = self.active_id();
        if let Some(window) = self.window_at_mut(index) {
            let mut panes: Vec<PaneId> = window.root.panes(area).iter().map(|p| p.0).collect();
            let active = panes.iter().position(|id| *id == window.active);
            if down {
                panes.rotate_right(1);
            } else {
                panes.rotate_left(1);
            }
            window.root.fill(&mut panes.iter().copied());
            window.active = active
                .and_then(|active| panes.get(active).copied())
                .unwrap_or(window.active);
            window.zoomed = false;
        }
        self.report_focus_change(before);
        self.relayout();
    }

    /// Makes a pane active, showing it if its window is zoomed in on another.
    pub fn show_pane(&mut self, id: PaneId) {
        if let Some(index) = self.window_of(id).map(|window| window.index) {
            self.unzoom(index);
            self.select_pane(id);
        }
    }

    /// Selects a pane of the current window by its number.
    pub fn select_pane_at(&mut self, index: usize) {
        let panes = self.window().map(|w| w.root.panes(self.area()));
        if let Some((id, _)) = panes.and_then(|panes| panes.get(index).copied()) {
            self.show_pane(id);
        }
    }

    pub fn select_direction(&mut self, from: PaneId, direction: Direction) {
        let area = self.area();
        let Some(window) = self.window_of(from) else {
            return;
        };
        let target = layout::neighbor(&window.root.panes(area), from, direction, area);
        if let Some(target) = target {
            self.show_pane(target);
        }
    }

    fn unzoom(&mut self, index: u32) {
        if let Some(window) = self.window_at_mut(index)
            && std::mem::take(&mut window.zoomed)
        {
            self.relayout();
        }
    }

    pub fn toggle_zoom(&mut self, id: PaneId) {
        let Some(window) = self.window_of(id) else {
            return;
        };
        if matches!(window.root, Node::Leaf(_)) {
            return;
        }
        let index = window.index;
        self.select_pane(id);
        if let Some(window) = self.window_at_mut(index) {
            window.zoomed = !window.zoomed;
        }
        self.relayout();
    }

    pub fn resize_pane(&mut self, id: PaneId, direction: Direction, cells: u16) {
        let area = self.area();
        if let Some(window) = self.window_of_mut(id) {
            window.zoomed = false;
            window.root.resize(id, area, direction, cells);
            self.relayout();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> Window {
        Window {
            id: 0,
            index: 0,
            name: "sh".to_owned(),
            automatic_name: true,
            root: Node::Leaf(0),
            active: 0,
            last_active: None,
            zoomed: false,
            bell: false,
            monitor: Monitor::Off,
            layout: None,
            next_name: None,
        }
    }

    #[test]
    fn a_name_takes_two_looks_to_change() {
        let mut window = window();
        assert_eq!(window.follow("sh".to_owned()), NameCheck::Unchanged);
        assert_eq!(window.follow("vim".to_owned()), NameCheck::LookAgain);
        assert_eq!(window.name, "sh");
        assert_eq!(window.follow("vim".to_owned()), NameCheck::Changed);
        assert_eq!(window.name, "vim");
        assert_eq!(window.follow("vim".to_owned()), NameCheck::Unchanged);
    }

    #[test]
    fn what_comes_and_goes_leaves_the_name_alone() {
        let mut window = window();
        assert_eq!(window.follow("git".to_owned()), NameCheck::LookAgain);
        assert_eq!(window.follow("sh".to_owned()), NameCheck::Unchanged);
        assert_eq!(window.follow("git".to_owned()), NameCheck::LookAgain);
        assert_eq!(window.name, "sh");
    }

    #[test]
    fn programs_in_a_row_do_not_add_up() {
        let mut window = window();
        assert_eq!(window.follow("git".to_owned()), NameCheck::LookAgain);
        assert_eq!(window.follow("sed".to_owned()), NameCheck::LookAgain);
        assert_eq!(window.name, "sh");
    }
}
