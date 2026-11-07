//! What happens with input: key bindings, the mouse, and running commands.

use std::ffi::c_int;
use std::time::{Duration, Instant};

use super::copy_mode::{self, CopyMode, Outcome};
use super::format::Context;
use super::keys;
use super::layout::{Divider, PaneId, Rect};
use super::pane::Pane;
use super::session::{Monitor, Window};
use super::ui::{List, ListOutcome, ListRow, Prompt, PromptAction, PromptOutcome};
use super::{Lifecycle, Server, color_queries};
use crate::command::{self, Command};
use crate::ghostty::key::{self, MOD_CTRL, MOD_SHIFT};
use crate::ghostty::{
    KeyAction, KeyInput, MODE_ALTERNATE_SCROLL, MouseAction, MouseButton, MouseInput,
};
use crate::input::{Event, KeyEvent, MouseEvent};
use crate::proto::ServerMessage;

/// How close together clicks must be to count as a double or triple click.
const MULTI_CLICK_TIME: Duration = Duration::from_millis(400);

/// How long a repeatable binding keeps working without the prefix, which is
/// tmux's `repeat-time`.
const REPEAT_TIME: Duration = Duration::from_millis(500);

/// Rows moved for each step of the wheel.
const WHEEL_ROWS: i32 = 3;

/// What dragging the mouse is doing right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Drag {
    #[default]
    None,
    /// Moving the line between two panes.
    Divider(Divider),
    /// Pressed in a pane, without having moved yet.
    Pressed { pane: PaneId, x: u16, y: u16 },
    /// Selecting text in a pane.
    Selecting(PaneId),
    /// A program asked for the mouse, so it gets everything until the release.
    Forwarding(PaneId),
}

/// The last click, for recognizing double and triple clicks.
#[derive(Clone, Copy, Debug)]
pub struct Click {
    at: Instant,
    x: u16,
    y: u16,
    count: u8,
}

const fn is_wheel(button: Option<MouseButton>) -> bool {
    matches!(
        button,
        Some(
            MouseButton::WheelUp
                | MouseButton::WheelDown
                | MouseButton::WheelLeft
                | MouseButton::WheelRight
        )
    )
}

/// A position relative to a pane, held inside of it for drags that leave it.
fn in_pane(pane: &Pane, event: MouseEvent) -> (u16, u16) {
    let rect = pane.rect;
    (
        event
            .x
            .saturating_sub(rect.x)
            .min(rect.width.saturating_sub(1)),
        event
            .y
            .saturating_sub(rect.y)
            .min(rect.height.saturating_sub(1)),
    )
}

impl Server {
    pub fn handle_input(&mut self, bytes: &[u8]) {
        let events = self.parser.feed(bytes);
        let colors = events.iter().any(Event::is_about_colors);
        for event in events {
            self.handle_event(event);
        }
        // After all of them, as a single reply can come as many events.
        if colors {
            self.session.apply_colors();
            self.dirty = true;
        }
        self.escape_since = self.parser.has_pending_escape().then(Instant::now);
    }

    pub fn handle_event(&mut self, event: Event) {
        self.names_may_change();
        match event {
            Event::Key(event) => self.handle_key(&event),
            Event::Mouse(event) => self.handle_mouse(event),
            Event::Paste(text) => self.handle_paste(&text),
            Event::Focus(focused) => self.session.report_focus(focused),
            Event::Foreground(rgb) => self.session.colors.foreground = Some(rgb),
            Event::Background(rgb) => self.session.colors.background = Some(rgb),
            Event::Palette(index, rgb) => {
                self.session.colors.palette.insert(index, rgb);
            }
            Event::ColorScheme { dark } => {
                // A new scheme most likely brought new colors along.
                let old = self.session.colors.dark.replace(dark);
                if old.is_some_and(|old| old != dark) {
                    self.send_output(&color_queries());
                }
            }
        }
    }

    fn handle_paste(&mut self, text: &[u8]) {
        if let Some(prompt) = &mut self.ui.prompt {
            prompt.insert(&String::from_utf8_lossy(text));
            self.dirty = true;
        } else if self.ui.list.is_none()
            && let Some(pane) = self.session.active_pane_mut()
            && pane.copy_mode.is_none()
        {
            let bytes = pane.terminal.encode_paste(text);
            pane.send(&bytes);
        }
    }

    fn swallow(&mut self, key: c_int) {
        if !self.swallowed.contains(&key) {
            self.swallowed.push(key);
        }
    }

    fn in_copy_mode(&self) -> bool {
        self.session
            .active_pane()
            .is_some_and(|pane| pane.copy_mode.is_some())
    }

    /// Whether this key continues a run of repeatable bindings. Any other key
    /// ends the run, and then means what it would mean otherwise.
    fn repeats(&mut self, event: &KeyEvent) -> bool {
        if event.action == KeyAction::Release {
            return false;
        }
        let in_time = self
            .repeat_until
            .is_some_and(|until| Instant::now() < until);
        let repeatable = || keys::binding(event).is_some_and(|binding| binding.repeat);
        let repeats = in_time && (key::is_modifier(event.key) || repeatable());
        if !repeats {
            self.repeat_until = None;
        }
        repeats
    }

    fn handle_key(&mut self, event: &KeyEvent) {
        let repeats = self.repeats(event);
        let captured = self.ui.prompt.is_some()
            || self.ui.list.is_some()
            || self.prefix
            || repeats
            || self.in_copy_mode();
        if event.action == KeyAction::Release {
            if let Some(position) = self.swallowed.iter().position(|key| *key == event.key) {
                self.swallowed.swap_remove(position);
            } else if !captured {
                self.forward_key(event);
            }
            return;
        }
        // Holding shift for `%` must not count as the key after the prefix.
        if captured && key::is_modifier(event.key) {
            return;
        }
        if captured || event.is_ctrl('b') {
            self.swallow(event.key);
        }
        if self.pane_numbers_key(event) {
            self.swallow(event.key);
            return;
        }
        if self.ui.prompt.is_some() {
            self.prompt_key(event);
        } else if std::mem::take(&mut self.prefix) || repeats {
            self.run_binding(event);
        } else if event.is_ctrl('b') {
            // As in tmux, the prefix goes before the keys of copy mode too.
            self.prefix = true;
        } else if self.ui.list.is_some() {
            self.list_key(event);
        } else if self.in_copy_mode() {
            self.copy_mode_key(event);
        } else {
            self.forward_key(event);
        }
    }

    pub const fn hide_pane_numbers(&mut self) {
        self.ui.pane_numbers = None;
        self.session.repaint = true;
        self.dirty = true;
    }

    /// Any key ends `display-panes`. A digit is for picking a pane, while the
    /// others go on to do what they usually do. Returns whether this was a digit.
    fn pane_numbers_key(&mut self, event: &KeyEvent) -> bool {
        if self.ui.pane_numbers.is_none() || key::is_modifier(event.key) {
            return false;
        }
        self.hide_pane_numbers();
        let index = event.char().and_then(|c| c.to_digit(10));
        let Some(index) = index.and_then(|index| usize::try_from(index).ok()) else {
            return false;
        };
        self.session.select_pane_at(index);
        true
    }

    fn forward_key(&mut self, event: &KeyEvent) {
        if let Some(pane) = self.session.active_pane_mut() {
            let bytes = pane.terminal.encode_key(&event.as_input());
            pane.send(&bytes);
        }
    }

    fn run_binding(&mut self, event: &KeyEvent) {
        self.dirty = true;
        let Some(binding) = keys::binding(event) else {
            return;
        };
        self.repeat_until = binding.repeat.then(|| Instant::now() + REPEAT_TIME);
        self.run_line(binding.command);
    }

    /// Runs a command the user gave here, showing what comes of it.
    fn run_interactive(&mut self, command: Command) {
        match self.execute(command) {
            Ok(text) if text.is_empty() => {}
            Ok(text) => {
                let rows = text.lines().map(|line| ListRow::text(line.to_owned()));
                self.ui.list = Some(List::new(rows.collect(), 0));
            }
            Err(text) => self.ui.show_message(text),
        }
    }

    fn list_height(&self) -> usize {
        usize::from(self.session.area().height)
    }

    fn close_list(&mut self) {
        self.ui.list = None;
        self.session.repaint = true;
        self.dirty = true;
    }

    fn after_list(&mut self, outcome: ListOutcome) {
        self.dirty = true;
        match outcome {
            ListOutcome::Stay => {}
            ListOutcome::Close => self.close_list(),
            ListOutcome::Run(command) => {
                self.close_list();
                self.run_interactive(command);
            }
        }
    }

    fn list_key(&mut self, event: &KeyEvent) {
        let height = self.list_height();
        if let Some(list) = &mut self.ui.list {
            let outcome = list.handle_key(event, height);
            self.after_list(outcome);
        }
    }

    /// The wheel moves through a list, a click selects a row and a double click
    /// chooses it, as in tmux's lists.
    fn list_mouse(&mut self, event: MouseEvent) {
        if event.action != MouseAction::Press {
            return;
        }
        let height = self.list_height();
        let clicks = if event.button == Some(MouseButton::Left) {
            self.click_count(event)
        } else {
            0
        };
        let Some(list) = &mut self.ui.list else {
            return;
        };
        let outcome = match event.button {
            Some(MouseButton::WheelUp) => {
                list.step(false);
                ListOutcome::Stay
            }
            Some(MouseButton::WheelDown) => {
                list.step(true);
                ListOutcome::Stay
            }
            Some(MouseButton::Left) => {
                list.select_at(usize::from(event.y), height);
                if clicks == 2 {
                    list.choose()
                } else {
                    ListOutcome::Stay
                }
            }
            _ => ListOutcome::Stay,
        };
        self.after_list(outcome);
    }

    /// The window chooser, on the row of the current window.
    fn choose_tree(&mut self) {
        let session = &self.session;
        let row = |window: &Window| {
            let context = Context {
                session,
                window: Some(window),
                pane: session.panes.get(&window.active),
            };
            let text = context.expand("#I: #W#F#{?pane_title, \"#T\",}");
            ListRow::new(text, Command::SelectWindow(window.index))
        };
        let rows = session.windows.iter().map(row).collect();
        let current = session
            .windows
            .iter()
            .position(|w| w.index == session.current);
        self.ui.list = Some(List::new(rows, current.unwrap_or(0)));
    }

    fn prompt_key(&mut self, event: &KeyEvent) {
        let Some(mut prompt) = self.ui.prompt.take() else {
            return;
        };
        self.dirty = true;
        match prompt.handle_key(event) {
            PromptOutcome::Editing => self.ui.prompt = Some(prompt),
            PromptOutcome::Cancelled => {}
            PromptOutcome::Done(text) => match prompt.action {
                PromptAction::Command(Some(template)) => {
                    let text = text.replace('\\', "\\\\").replace('"', "\\\"");
                    self.run_line(&template.replace("%%", &text));
                }
                PromptAction::Command(None) => self.run_line(&text),
                PromptAction::Confirm(command) => self.run_interactive(*command),
            },
        }
    }

    /// Runs a line of text as a command. An empty one is nothing to complain about.
    fn run_line(&mut self, line: &str) {
        let words = command::split_words(line);
        if words.as_ref().is_ok_and(Vec::is_empty) {
            return;
        }
        match words.and_then(|words| command::parse(&words)) {
            Ok(command) => self.run_interactive(command),
            Err(error) => self.ui.show_message(error),
        }
    }

    fn command_prompt(
        &mut self,
        prompt: Option<&str>,
        initial: Option<&str>,
        template: Option<String>,
    ) {
        let context = Context::current(&self.session);
        let label = match (prompt, &template) {
            (Some(prompt), _) => format!("{} ", context.expand(prompt)),
            (None, Some(template)) => {
                format!("({}) ", template.split(' ').next().unwrap_or(template))
            }
            (None, None) => ":".to_owned(),
        };
        let initial = initial.map_or_else(String::new, |initial| context.expand(initial));
        let action = PromptAction::Command(template);
        self.ui.prompt = Some(Prompt::new(&label, &initial, action));
    }

    fn confirm_before(&mut self, prompt: Option<&str>, command: Box<Command>) {
        let label = prompt.map_or_else(
            || "Confirm? (y/n) ".to_owned(),
            |prompt| format!("{} ", Context::current(&self.session).expand(prompt)),
        );
        let action = PromptAction::Confirm(command);
        self.ui.prompt = Some(Prompt::new(&label, "", action));
    }

    fn send_prefix(&mut self) {
        let input = KeyInput {
            action: KeyAction::Press,
            key: key::from_codepoint(u32::from('b')),
            mods: MOD_CTRL,
            consumed_mods: 0,
            text: "",
            unshifted: u32::from('b'),
        };
        if let Some(pane) = self.session.active_pane_mut() {
            let bytes = pane.terminal.encode_key(&input);
            pane.send(&bytes);
        }
    }

    fn copy_mode_key(&mut self, event: &KeyEvent) {
        let Some(pane) = self.session.active_pane_mut() else {
            return;
        };
        let outcome = copy_mode::handle_key(pane, event);
        let copied = (outcome == Outcome::Copy)
            .then(|| pane.terminal.selected_text())
            .flatten();
        if outcome != Outcome::Stay {
            CopyMode::exit(pane);
        }
        if let Some(text) = copied {
            self.store_buffer(text);
        }
        self.dirty = true;
    }

    /// Runs a command, returning any output for the user.
    pub fn execute(&mut self, command: Command) -> Result<String, String> {
        self.dirty = true;
        self.names_may_change();
        match command {
            Command::ChooseTree => self.choose_tree(),
            Command::Prompt {
                prompt,
                initial,
                template,
            } => self.command_prompt(prompt.as_deref(), initial.as_deref(), template),
            Command::ConfirmBefore { prompt, command } => {
                self.confirm_before(prompt.as_deref(), command);
            }
            Command::CopyMode { page_up } => self.start_copy_mode(page_up),
            Command::DetachClient => self.detach(),
            Command::DisplayMessage(text) => self.ui.show_message(text),
            Command::DisplayPanes(time) => self.ui.show_pane_numbers(time),
            Command::KillPane => {
                if let Some(id) = self.session.active_id() {
                    self.session.remove_pane(id);
                }
            }
            Command::KillServer => {
                self.exit_message = "[server exited]";
                self.lifecycle = Lifecycle::Quitting;
            }
            Command::KillWindow => self.session.kill_current_window(),
            Command::LastPane => self.session.select_last_pane(),
            Command::LastWindow => {
                let last = self.session.last.ok_or("no last window")?;
                self.session.select_window(last);
            }
            Command::ListKeys { notes } => {
                return Ok(format!("{}\n", keys::list(notes).join("\n")));
            }
            Command::ListPanes => return Ok(self.list_panes()),
            Command::ListWindows => return Ok(self.list_windows()),
            Command::NewWindow { name, program } => self.session.new_window(name, &program)?,
            Command::NextPane => self.session.select_next_pane(),
            Command::NextWindow => self.session.next_window(),
            Command::PasteBuffer => self.paste_buffer(),
            Command::PreviousWindow => self.session.previous_window(),
            Command::RenameWindow(name) => {
                if let Some(window) = self.session.window_mut() {
                    window.name = name;
                    window.automatic_name = false;
                }
            }
            Command::ResizePane { direction, cells } => {
                self.session.resize_pane(direction, cells);
            }
            Command::RestartServer(binary) => self.restart(binary)?,
            Command::SelectPane(direction) => self.session.select_direction(direction),
            Command::SelectWindow(index) => {
                if !self.session.select_window(index) {
                    return Err(format!("no window {index}"));
                }
            }
            Command::SendKeys(keys) => {
                let bytes: Vec<u8> = keys
                    .iter()
                    .flat_map(|key| command::key_bytes(key))
                    .collect();
                if let Some(pane) = self.session.active_pane_mut() {
                    pane.send(&bytes);
                }
            }
            Command::SendPrefix => self.send_prefix(),
            Command::SetMonitorActivity(value) => {
                if let Some(window) = self.session.window_mut() {
                    let on = value.unwrap_or(window.monitor == Monitor::Off);
                    window.monitor = if on { Monitor::Watching } else { Monitor::Off };
                }
            }
            Command::SplitWindow { kind, program } => self.session.split(kind, &program)?,
            Command::ToggleZoom => self.session.toggle_zoom(),
        }
        Ok(String::new())
    }

    fn detach(&mut self) {
        if let Some(id) = self.attached.take()
            && let Some(connection) = self.connection_mut(id)
        {
            connection.finish(&ServerMessage::Exit("[detached]".to_owned()));
        }
    }

    fn start_copy_mode(&mut self, page_up: bool) {
        if let Some(pane) = self.session.active_pane_mut() {
            CopyMode::enter(pane, false);
            if page_up {
                pane.terminal.scroll_by(-i32::from(pane.rect.height));
            }
        }
    }

    fn paste_buffer(&mut self) {
        let Some(text) = &self.buffer else {
            return;
        };
        if let Some(pane) = self.session.active_pane_mut() {
            let bytes = pane.terminal.encode_paste(text.as_bytes());
            pane.send(&bytes);
        }
    }

    fn list_windows(&self) -> String {
        let area = self.session.area();
        let (width, height) = (area.width, area.height);
        let line = |window: &Window| {
            let panes = window.root.panes(area).len();
            let current = window.index == self.session.current;
            let marker = if current { " (active)" } else { "" };
            let (index, name) = (window.index, &window.name);
            format!("{index}: {name} ({panes} panes) [{width}x{height}]{marker}\n")
        };
        self.session.windows.iter().map(line).collect()
    }

    fn list_panes(&self) -> String {
        let Some(window) = self.session.window() else {
            return String::new();
        };
        let panes = window.root.panes(self.session.area());
        let line = |(index, (id, rect)): (usize, &(PaneId, Rect))| {
            let active = *id == window.active;
            let marker = if active { " (active)" } else { "" };
            let (width, height) = (rect.width, rect.height);
            format!("{index}: [{width}x{height}] %{id}{marker}\n")
        };
        panes.iter().enumerate().map(line).collect()
    }

    fn handle_mouse(&mut self, event: MouseEvent) {
        self.dirty = true;
        if self.ui.list.is_some() {
            self.list_mouse(event);
            return;
        }
        match self.drag.clone() {
            Drag::None => {
                if event.action == MouseAction::Press {
                    self.mouse_press(event);
                } else if event.action == MouseAction::Motion {
                    self.mouse_hover(event);
                }
            }
            Drag::Divider(divider) => self.drag_divider(&divider, event),
            Drag::Pressed { pane, x, y } => self.drag_from_press(pane, (x, y), event),
            Drag::Selecting(pane) => self.drag_selection(pane, event),
            Drag::Forwarding(pane) => {
                self.forward_mouse(pane, event);
                if event.action == MouseAction::Release {
                    self.drag = Drag::None;
                }
            }
        }
    }

    /// Programs that track all motion get it without a button held.
    fn mouse_hover(&mut self, event: MouseEvent) {
        if let Some(id) = self.pane_at(event.x, event.y)
            && self.pane_takes_mouse(id, event)
        {
            self.forward_mouse(id, event);
        }
    }

    fn pane_at(&self, x: u16, y: u16) -> Option<PaneId> {
        let panes = self.session.visible_panes();
        panes
            .iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(id, _)| *id)
    }

    fn divider_at(&self, x: u16, y: u16) -> Option<Divider> {
        let window = self.session.window().filter(|window| !window.zoomed)?;
        let dividers = window.root.dividers(self.session.area());
        dividers.into_iter().find(|d| d.line.contains(x, y))
    }

    /// Whether a mouse event belongs to the program. Holding shift gets the
    /// mouse back from a program, as in most terminals.
    fn pane_takes_mouse(&self, id: PaneId, event: MouseEvent) -> bool {
        self.session.panes.get(&id).is_some_and(|pane| {
            pane.copy_mode.is_none()
                && pane.terminal.mouse_tracking()
                && event.mods & MOD_SHIFT == 0
        })
    }

    fn forward_mouse(&mut self, id: PaneId, event: MouseEvent) {
        let Some(pane) = self.session.panes.get_mut(&id) else {
            return;
        };
        let (x, y) = in_pane(pane, event);
        let held = event.action == MouseAction::Press
            || (event.action == MouseAction::Motion && event.button.is_some());
        let input = MouseInput {
            action: event.action,
            button: event.button,
            mods: event.mods,
            x,
            y,
            any_pressed: held,
        };
        let (cols, rows) = (pane.rect.width.max(1), pane.rect.height.max(1));
        let bytes = pane.terminal.encode_mouse(&input, cols, rows);
        pane.send(&bytes);
    }

    fn mouse_press(&mut self, event: MouseEvent) {
        let status_row = self.session.size.rows.saturating_sub(1);
        if event.y == status_row {
            self.click_status(event);
        } else if let Some(divider) = self.divider_at(event.x, event.y) {
            if event.button == Some(MouseButton::Left) {
                self.drag = Drag::Divider(divider);
            }
        } else if let Some(id) = self.pane_at(event.x, event.y) {
            self.press_in_pane(id, event);
        }
    }

    fn click_status(&mut self, event: MouseEvent) {
        match event.button {
            Some(MouseButton::Left) => {
                let entries = &self.renderer.status_entries;
                let entry = entries
                    .iter()
                    .find(|entry| (entry.start..entry.end).contains(&event.x));
                if let Some(index) = entry.map(|entry| entry.window) {
                    self.session.select_window(index);
                }
            }
            Some(MouseButton::WheelUp) => self.session.previous_window(),
            Some(MouseButton::WheelDown) => self.session.next_window(),
            _ => {}
        }
    }

    fn press_in_pane(&mut self, id: PaneId, event: MouseEvent) {
        if is_wheel(event.button) {
            self.wheel(id, event);
            return;
        }
        self.session.select_pane(id);
        if self.pane_takes_mouse(id, event) {
            self.forward_mouse(id, event);
            self.drag = Drag::Forwarding(id);
            return;
        }
        match event.button {
            Some(MouseButton::Left) => self.press_to_select(id, event),
            Some(MouseButton::Middle) => self.paste_buffer(),
            _ => {}
        }
    }

    fn click_count(&mut self, event: MouseEvent) -> u8 {
        let now = Instant::now();
        let count = match self.last_click {
            Some(click)
                if click.x == event.x
                    && click.y == event.y
                    && now.duration_since(click.at) < MULTI_CLICK_TIME =>
            {
                click.count % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some(Click {
            at: now,
            x: event.x,
            y: event.y,
            count,
        });
        count
    }

    fn press_to_select(&mut self, id: PaneId, event: MouseEvent) {
        let count = self.click_count(event);
        let Some(pane) = self.session.panes.get_mut(&id) else {
            return;
        };
        let (x, y) = in_pane(pane, event);
        pane.terminal.clear_selection();
        pane.redraw = true;
        self.drag = match count {
            1 => Drag::Pressed { pane: id, x, y },
            2 => {
                pane.terminal.select_word(x, y);
                Drag::Selecting(id)
            }
            _ => {
                pane.terminal.select_line(x, y);
                Drag::Selecting(id)
            }
        };
    }

    fn drag_divider(&mut self, divider: &Divider, event: MouseEvent) {
        match event.action {
            MouseAction::Motion => {
                if let Some(window) = self.session.window_mut() {
                    window.root.move_divider(divider, event.x, event.y);
                }
                self.session.relayout();
            }
            MouseAction::Release | MouseAction::Press => self.drag = Drag::None,
        }
    }

    fn drag_from_press(&mut self, id: PaneId, start: (u16, u16), event: MouseEvent) {
        match event.action {
            MouseAction::Motion => {
                if let Some(pane) = self.session.panes.get_mut(&id) {
                    pane.terminal.select_start(start.0, start.1);
                }
                self.drag = Drag::Selecting(id);
                self.drag_selection(id, event);
            }
            MouseAction::Release | MouseAction::Press => self.drag = Drag::None,
        }
    }

    fn drag_selection(&mut self, id: PaneId, event: MouseEvent) {
        let Some(pane) = self.session.panes.get_mut(&id) else {
            self.drag = Drag::None;
            return;
        };
        if event.action == MouseAction::Motion {
            let (x, y) = in_pane(pane, event);
            pane.terminal.select_extend(x, y);
            pane.redraw = true;
            return;
        }
        self.drag = Drag::None;
        let text = pane.terminal.selected_text();
        if pane.copy_mode.is_some() {
            CopyMode::exit(pane);
        } else {
            pane.terminal.clear_selection();
            pane.redraw = true;
        }
        if let Some(text) = text {
            self.store_buffer(text);
        }
    }

    fn wheel(&mut self, id: PaneId, event: MouseEvent) {
        if self.pane_takes_mouse(id, event) {
            self.forward_mouse(id, event);
            return;
        }
        let up = match event.button {
            Some(MouseButton::WheelUp) => true,
            Some(MouseButton::WheelDown) => false,
            _ => return,
        };
        let Some(pane) = self.session.panes.get_mut(&id) else {
            return;
        };
        let terminal = &pane.terminal;
        if terminal.alternate_screen() && terminal.mode(MODE_ALTERNATE_SCROLL) {
            send_arrows(pane, up);
        } else {
            scroll_history(pane, up);
        }
    }
}

/// Full screen programs without mouse support scroll with the arrow keys.
fn send_arrows(pane: &mut Pane, up: bool) {
    let input = KeyInput {
        action: KeyAction::Press,
        key: if up { key::ARROW_UP } else { key::ARROW_DOWN },
        mods: 0,
        consumed_mods: 0,
        text: "",
        unshifted: 0,
    };
    for _ in 0..WHEEL_ROWS {
        let bytes = pane.terminal.encode_key(&input);
        pane.send(&bytes);
    }
}

/// The wheel scrolls history, by way of copy mode as in tmux.
fn scroll_history(pane: &mut Pane, up: bool) {
    if up {
        CopyMode::enter(pane, true);
        pane.terminal.scroll_by(-WHEEL_ROWS);
    } else if pane.copy_mode.is_some() {
        pane.terminal.scroll_by(WHEEL_ROWS);
        let at_bottom = pane
            .terminal
            .scrollbar()
            .is_some_and(|scrollbar| scrollbar.lines_below() == 0);
        if at_bottom && pane.copy_mode.is_some_and(|mode| mode.exit_at_bottom) {
            CopyMode::exit(pane);
        }
    }
    pane.redraw = true;
}
