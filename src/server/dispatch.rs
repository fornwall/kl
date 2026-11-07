//! What happens with input: key bindings, the mouse, and running commands.

use std::ffi::{CString, c_int};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::copy_mode::{
    self, Action, Binding as CopyBinding, CopyMode, CopyPrompt, Outcome, Scroll,
};
use super::format::Context;
use super::keys;
use super::layout::{Divider, Layout, PaneId};
use super::pane::Pane;
use super::session::{Monitor, Window};
use super::ui::{List, ListOutcome, ListRow, Prompt, PromptAction, PromptOutcome};
use super::{Lifecycle, Server, color_queries};
use crate::command::{self, BufferCommand, Command, Direction, PaneScope, PasteOptions};
use crate::ghostty::key::{self, MOD_CTRL, MOD_SHIFT};
use crate::ghostty::{
    Autoscroll, KeyAction, KeyInput, MODE_ALTERNATE_SCROLL, MULTI_CLICK_TIME, MouseAction,
    MouseButton, MouseInput,
};
use crate::input::{Event, KeyEvent, MouseEvent};
use crate::proto::ServerMessage;
use crate::sys;

/// How long dragging at the edge of a pane takes to scroll a row, as in tmux.
const AUTOSCROLL_TIME: Duration = Duration::from_millis(50);

/// How long a repeatable binding keeps working without the prefix, which is
/// tmux's `repeat-time`.
const REPEAT_TIME: Duration = Duration::from_millis(500);

/// What `display-message` shows by default.
const DISPLAY_MESSAGE: &str = "[#{session_name}] #{window_index}:#{window_name}, \
     current pane #{pane_index} - (%H:%M %d-%b-%y)";

/// What `list-windows` shows by default, except for tmux's window layout.
const LIST_WINDOWS: &str = "#{window_index}: #{window_name}#{window_flags} \
     (#{window_panes} panes) [#{window_width}x#{window_height}] \
     #{window_id}#{?window_active, (active),}";

/// What `list-panes` shows by default, except for tmux's history figures.
const LIST_PANES: &str = "#{pane_index}: [#{pane_width}x#{pane_height}] \
     #{pane_id}#{?pane_active, (active),}";

/// What dragging the mouse is doing right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Drag {
    #[default]
    None,
    /// Moving the line between two panes.
    Divider(Divider),
    /// Selecting text in a pane, from the press on.
    Selecting(Selecting),
    /// A program asked for the mouse, so it gets everything until the release.
    Forwarding(PaneId),
}

impl Drag {
    /// When a pointer at the edge of a pane moves the text along next.
    pub const fn next_scroll(&self) -> Option<Instant> {
        match self {
            Self::Selecting(Selecting {
                scroll: Some((_, at)),
                ..
            }) => Some(*at),
            _ => None,
        }
    }
}

/// The last click on a list, for recognizing double clicks.
#[derive(Clone, Copy, Debug)]
pub struct Click {
    at: Instant,
    x: u16,
    y: u16,
    count: u8,
}

/// The state of a drag that selects text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selecting {
    pane: PaneId,
    /// Where the pointer is, relative to the pane and inside of it.
    x: u16,
    y: u16,
    /// Which way the view follows the pointer, and when it does so next.
    scroll: Option<(Autoscroll, Instant)>,
}

/// A line of a format for each of what `-f` lets through.
fn listing<'a>(
    contexts: impl Iterator<Item = Context<'a>>,
    format: &str,
    filter: Option<&str>,
) -> String {
    let mut text = String::new();
    for context in contexts.filter(|context| filter.is_none_or(|filter| context.holds(filter))) {
        text.push_str(&context.expand(format));
        text.push('\n');
    }
    text
}

/// Fills in the time as `strftime` would, which `display-message` does.
fn with_local_time(format: &str) -> String {
    if !format.contains('%') {
        return format.to_owned();
    }
    let time = CString::new(format).map(|format| sys::local_time(&format));
    time.ok()
        .filter(|time| !time.is_empty())
        .unwrap_or_else(|| format.to_owned())
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

/// The way a pointer moving from `last` to `now` wants the view to go. Moving
/// along the first or last row leaves it alone, for selecting text there.
const fn edge_of(pane: &Pane, last: (u16, u16), now: (u16, u16)) -> Option<Autoscroll> {
    if last.1 == now.1 && last.0 != now.0 {
        None
    } else if now.1 == 0 {
        Some(Autoscroll::Up)
    } else if now.1 + 1 >= pane.rect.height {
        Some(Autoscroll::Down)
    } else {
        None
    }
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

/// A position relative to a pane in pixels, held inside of it like [`in_pane`].
/// Without real pixels to go by, it is the corner of the cell.
fn pixels_in_pane(
    pane: &Pane,
    event: MouseEvent,
    pixels: Option<(u16, u16)>,
    (width, height): (u16, u16),
) -> (u16, u16) {
    let (width, height) = (width.max(1), height.max(1));
    let rect = pane.rect;
    let corner = || {
        (
            event.x.saturating_mul(width),
            event.y.saturating_mul(height),
        )
    };
    let (x, y) = pixels.unwrap_or_else(corner);
    (
        x.saturating_sub(rect.x.saturating_mul(width))
            .min(rect.width.saturating_mul(width).saturating_sub(1)),
        y.saturating_sub(rect.y.saturating_mul(height))
            .min(rect.height.saturating_mul(height).saturating_sub(1)),
    )
}

impl Server {
    pub fn handle_input(&mut self, bytes: &[u8]) {
        let events = self.client().parser.feed(bytes);
        let colors = events.iter().any(Event::is_about_colors);
        for event in events {
            self.handle_event(event);
        }
        // After all of them, as a single reply can come as many events.
        if colors {
            self.session.apply_colors();
            self.dirty = true;
        }
        let client = self.client();
        client.escape_since = client.parser.has_pending_escape().then(Instant::now);
    }

    pub fn handle_event(&mut self, event: Event) {
        self.names_may_change();
        match event {
            Event::Key(event) => self.handle_key(&event),
            Event::Mouse(event) => self.handle_mouse(self.in_cells(event)),
            Event::Paste(text) => self.handle_paste(&text),
            Event::Focus(focused) => self.session.report_focus(focused),
            Event::PixelMouse(pixels) => self.client().pixel_mouse = pixels,
            Event::Foreground(rgb) => self.session.colors.foreground = Some(rgb),
            Event::Background(rgb) => self.session.colors.background = Some(rgb),
            Event::Palette(index, rgb) => {
                self.session.colors.palette.insert(index, rgb);
            }
            Event::CursorStyle(style) => self.session.colors.cursor_style = Some(style),
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
        let ui = &mut self.client().ui;
        if let Some(prompt) = &mut ui.prompt {
            prompt.insert(&String::from_utf8_lossy(text));
            self.dirty = true;
        } else if ui.list.is_none()
            && let Some(pane) = self.session.active_pane_mut()
            && pane.copy_mode.is_none()
        {
            let bytes = pane.terminal.encode_paste(text, true);
            pane.send(&bytes);
        }
    }

    fn swallow(&mut self, key: c_int) {
        let swallowed = &mut self.client().swallowed;
        if !swallowed.contains(&key) {
            swallowed.push(key);
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
        let client = self.client();
        let in_time = client
            .repeat_until
            .is_some_and(|until| Instant::now() < until);
        let repeatable = || keys::binding(event).is_some_and(|binding| binding.repeat);
        let repeats = in_time && (key::is_modifier(event.key) || repeatable());
        if !repeats {
            client.repeat_until = None;
        }
        repeats
    }

    fn handle_key(&mut self, event: &KeyEvent) {
        let repeats = self.repeats(event);
        let in_copy_mode = self.in_copy_mode();
        let client = self.client();
        let captured = client.ui.prompt.is_some()
            || client.ui.list.is_some()
            || client.prefix
            || repeats
            || in_copy_mode;
        if event.action == KeyAction::Release {
            if let Some(position) = client.swallowed.iter().position(|key| *key == event.key) {
                client.swallowed.swap_remove(position);
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
        let client = self.client();
        if client.ui.prompt.is_some() {
            self.prompt_key(event);
        } else if std::mem::take(&mut client.prefix) || repeats {
            self.run_binding(event);
        } else if event.is_ctrl('b') {
            // As in tmux, the prefix goes before the keys of copy mode too.
            client.prefix = true;
        } else if client.ui.list.is_some() {
            self.list_key(event);
        } else if self.in_copy_mode() {
            self.copy_mode_key(event);
        } else {
            self.forward_key(event);
        }
    }

    pub fn hide_pane_numbers(&mut self) {
        self.client().ui.pane_numbers = None;
        self.session.repaint = true;
        self.dirty = true;
    }

    /// Any key ends `display-panes`. A digit is for picking a pane, while the
    /// others go on to do what they usually do. Returns whether this was a digit.
    fn pane_numbers_key(&mut self, event: &KeyEvent) -> bool {
        if self.client().ui.pane_numbers.is_none() || key::is_modifier(event.key) {
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
        self.client().repeat_until = binding.repeat.then(|| Instant::now() + REPEAT_TIME);
        self.run_line(binding.command);
    }

    /// Runs a command the user gave here, showing what comes of it.
    fn run_interactive(&mut self, command: Command) {
        match self.execute(command) {
            Ok(text) if text.is_empty() => {}
            Ok(text) => {
                let rows = text.lines().map(|line| ListRow::text(line.to_owned()));
                self.client().ui.list = Some(List::new(rows.collect(), 0));
            }
            Err(text) => self.client().ui.show_message(text),
        }
    }

    fn list_height(&self) -> usize {
        usize::from(self.session.area().height)
    }

    fn close_list(&mut self) {
        self.client().ui.list = None;
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
            ListOutcome::Delete(command, row) => {
                self.close_list();
                self.run_interactive(command);
                self.choose_buffer(row);
            }
        }
    }

    fn list_key(&mut self, event: &KeyEvent) {
        let height = self.list_height();
        if let Some(list) = &mut self.client().ui.list {
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
        let Some(list) = &mut self.client().ui.list else {
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
            let target = Some(format!("@{}", window.id));
            ListRow::new(text, Command::SelectWindow { target })
        };
        let rows = session.windows.iter().map(row).collect();
        let current = session
            .windows
            .iter()
            .position(|w| w.index == session.current);
        let list = List::new(rows, current.unwrap_or(0));
        self.client().ui.list = Some(list);
    }

    fn prompt_key(&mut self, event: &KeyEvent) {
        let Some(mut prompt) = self.client().ui.prompt.take() else {
            return;
        };
        self.dirty = true;
        match prompt.handle_key(event) {
            PromptOutcome::Editing => self.client().ui.prompt = Some(prompt),
            PromptOutcome::Cancelled => {}
            PromptOutcome::Done(text) => match prompt.action {
                PromptAction::Command(Some(template)) => {
                    let text = text.replace('\\', "\\\\").replace('"', "\\\"");
                    self.run_line(&template.replace("%%", &text));
                }
                PromptAction::Command(None) => self.run_line(&text),
                PromptAction::Confirm(command) => self.run_interactive(*command),
                PromptAction::CopyMode(copy_prompt) => self.copy_prompt_done(copy_prompt, &text),
            },
            PromptOutcome::DoneBefore(text) => {
                if let PromptAction::CopyMode(copy_prompt) = prompt.action {
                    self.copy_prompt_done(copy_prompt, &text);
                }
                self.copy_mode_key(event);
            }
        }
    }

    fn copy_prompt_done(&mut self, prompt: CopyPrompt, text: &str) {
        match prompt {
            CopyPrompt::Search(forward) => {
                if let Some(pane) = self.session.active_pane_mut()
                    && !text.is_empty()
                {
                    copy_mode::search(pane, text, forward);
                }
            }
            CopyPrompt::GotoLine => {
                if let Ok(line) = text.trim().parse() {
                    self.copy_mode_action(Action::GotoLine(line));
                }
            }
            CopyPrompt::Jump(kind) => {
                if let Some(c) = text.chars().next() {
                    self.copy_mode_action(Action::Jump(kind, c));
                }
            }
            CopyPrompt::Repeat => {
                let pane = self.session.active_pane_mut();
                if let (Some(mode), Ok(repeat)) =
                    (pane.and_then(|pane| pane.copy_mode.as_mut()), text.parse())
                {
                    mode.set_repeat(repeat);
                }
            }
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
            Err(error) => self.client().ui.show_message(error),
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
        self.client().ui.prompt = Some(Prompt::new(&label, &initial, action));
    }

    fn confirm_before(&mut self, prompt: Option<&str>, command: Box<Command>) {
        let label = prompt.map_or_else(
            || "Confirm? (y/n) ".to_owned(),
            |prompt| format!("{} ", Context::current(&self.session).expand(prompt)),
        );
        let action = PromptAction::Confirm(command);
        self.client().ui.prompt = Some(Prompt::new(&label, "", action));
    }

    fn send_prefix(&mut self, id: PaneId) {
        let input = KeyInput {
            action: KeyAction::Press,
            key: key::from_codepoint(u32::from('b')),
            mods: MOD_CTRL,
            consumed_mods: 0,
            text: "",
            unshifted: u32::from('b'),
        };
        if let Some(pane) = self.session.panes.get_mut(&id) {
            let bytes = pane.terminal.encode_key(&input);
            pane.send(&bytes);
        }
    }

    fn copy_mode_key(&mut self, event: &KeyEvent) {
        self.dirty = true;
        match copy_mode::binding(event) {
            Some(CopyBinding::Run(action)) => self.copy_mode_action(action),
            Some(CopyBinding::Prompt(prompt, typed)) => {
                let typed = typed.map(String::from).unwrap_or_default();
                let action = PromptAction::CopyMode(prompt);
                self.client().ui.prompt = Some(Prompt::new(prompt.label(), &typed, action));
            }
            None => {}
        }
    }

    fn copy_mode_action(&mut self, action: Action) {
        let Some(pane) = self.session.active_pane_mut() else {
            return;
        };
        if pane.copy_mode.is_none() {
            return;
        }
        let outcome = copy_mode::run(pane, action);
        let copied = matches!(outcome, Outcome::Copy | Outcome::Append)
            .then(|| copy_mode::selected_text(pane))
            .flatten();
        if outcome != Outcome::Stay {
            CopyMode::exit(pane);
        }
        match copied {
            Some(text) if outcome == Outcome::Append => self.append_buffer(&text),
            Some(text) => self.store_buffer(text),
            None => {}
        }
    }

    /// Runs a command, returning any output for the user.
    #[expect(clippy::too_many_lines, reason = "it takes an arm for each command")]
    pub fn execute(&mut self, command: Command) -> Result<String, String> {
        self.dirty = true;
        self.names_may_change();
        match command {
            Command::BreakPane {
                source,
                target,
                name,
                detached,
                print,
            } => {
                let id = self.session.find_pane(source.as_deref())?;
                let index = target.map(|target| self.session.find_index(&target));
                let index = index.transpose()?.flatten();
                self.session.break_pane(id, name, index, detached)?;
                return Ok(self.tell_about(id, print.as_deref()));
            }
            Command::Buffer(command) => return self.run_buffer_command(command),
            Command::ChooseTree => self.choose_tree(),
            Command::Prompt {
                prompt,
                initial,
                template,
            } => self.command_prompt(prompt.as_deref(), initial.as_deref(), template),
            Command::ConfirmBefore { prompt, command } => {
                self.confirm_before(prompt.as_deref(), command);
            }
            Command::CopyMode { page_up, target } => {
                let id = self.session.find_pane(target.as_deref())?;
                self.start_copy_mode(id, page_up);
            }
            Command::DetachClient => self.detach(),
            Command::DisplayMessage {
                target,
                print,
                format,
            } => return self.display_message(target.as_deref(), print, format.as_deref()),
            Command::DisplayPanes(time) => self.client().ui.show_pane_numbers(time),
            Command::JoinPane {
                source,
                target,
                place,
                detached,
            } => {
                let source = self.session.find_source(source.as_deref())?;
                let target = self.session.find_pane(target.as_deref())?;
                self.session.join_pane(source, target, place, detached)?;
            }
            Command::KillPane { target, others } => {
                let id = self.session.find_pane(target.as_deref())?;
                self.kill_pane(id, others);
            }
            Command::KillServer => {
                self.exit_message = "[server exited]";
                self.lifecycle = Lifecycle::Quitting;
            }
            Command::KillWindow { target, others } => {
                let index = self.session.find_window(target.as_deref())?;
                self.kill_window(index, others);
            }
            Command::LastPane { target } => {
                let index = self.session.find_window(target.as_deref())?;
                let window = self.session.window_at(index);
                let last = window.and_then(|window| window.last_active);
                self.session.show_pane(last.ok_or("no last pane")?);
            }
            Command::LastWindow => {
                let last = self.session.last.ok_or("no last window")?;
                self.session.select_window(last);
            }
            Command::ListKeys { notes } => {
                return Ok(format!("{}\n", keys::list(notes).join("\n")));
            }
            Command::ListPanes {
                scope,
                target,
                format,
                filter,
            } => {
                let format = format.as_deref();
                return self.list_panes(scope, target.as_deref(), format, filter.as_deref());
            }
            Command::ListWindows {
                all,
                format,
                filter,
            } => return Ok(self.list_windows(all, format.as_deref(), filter.as_deref())),
            Command::MarkPane { target, clear } => {
                let id = self.session.find_pane(target.as_deref())?;
                let marked = self.session.marked_pane() == Some(id);
                self.session.marked = (!clear && !marked).then_some(id);
                self.session.repaint = true;
            }
            Command::MoveWindow(moving) => self.session.run_move_window(moving)?,
            Command::NewWindow {
                target,
                next_to,
                name,
                directory,
                detached,
                print,
                program,
            } => {
                let index = target.map(|target| self.session.find_index(&target));
                let index = index.transpose()?.flatten();
                let index = self.session.new_window_index(index, next_to)?;
                let directory = self.expand_directory(directory);
                let id = self.session.new_window(
                    name,
                    &program,
                    directory.as_deref(),
                    index,
                    detached,
                )?;
                return Ok(self.tell_about(id, print.as_deref()));
            }
            Command::NextLayout { target, forward } => {
                let id = self.session.find_pane(target.as_deref())?;
                self.step_layout(id, forward);
            }
            Command::NextWindow { alert } => {
                if !self.session.next_window(alert) {
                    return Err("no next window".to_owned());
                }
            }
            Command::PreviousWindow { alert } => {
                if !self.session.previous_window(alert) {
                    return Err("no previous window".to_owned());
                }
            }
            Command::RenameWindow { target, name } => {
                let index = self.session.find_window(target.as_deref())?;
                if let Some(window) = self.session.window_at_mut(index) {
                    window.name = name;
                    window.automatic_name = false;
                }
            }
            Command::ResizePane {
                target,
                direction,
                cells,
            } => {
                let id = self.session.find_pane(target.as_deref())?;
                self.session.resize_pane(id, direction, cells);
            }
            Command::RestartServer(binary) => self.restart(binary)?,
            Command::RotateWindow { target, down } => {
                let index = self.session.find_window(target.as_deref())?;
                self.session.rotate_window(index, down);
            }
            Command::SelectLayout { target, name } => {
                let id = self.session.find_pane(target.as_deref())?;
                self.select_layout(id, name.as_deref())?;
            }
            Command::SelectPane { target, direction } => {
                let id = self.session.find_pane(target.as_deref())?;
                match direction {
                    Some(direction) => self.session.select_direction(id, direction),
                    None => self.session.show_pane(id),
                }
            }
            Command::SelectWindow { target } => {
                let index = self.session.find_window(target.as_deref())?;
                self.session.select_window(index);
            }
            Command::SendKeys {
                target,
                literal,
                keys,
            } => {
                let id = self.session.find_pane(target.as_deref())?;
                let bytes: Vec<u8> = keys
                    .iter()
                    .flat_map(|key| {
                        if literal {
                            key.as_bytes().to_vec()
                        } else {
                            command::key_bytes(key)
                        }
                    })
                    .collect();
                if let Some(pane) = self.session.panes.get_mut(&id) {
                    pane.send(&bytes);
                }
            }
            Command::SendPrefix { target } => {
                let id = self.session.find_pane(target.as_deref())?;
                self.send_prefix(id);
            }
            Command::SetMonitorActivity { target, value } => {
                let index = self.session.find_window(target.as_deref())?;
                if let Some(window) = self.session.window_at_mut(index) {
                    let on = value.unwrap_or(window.monitor == Monitor::Off);
                    window.monitor = if on { Monitor::Watching } else { Monitor::Off };
                }
            }
            Command::SplitWindow {
                target,
                place,
                directory,
                detached,
                print,
                program,
            } => {
                let target = self.session.find_pane(target.as_deref())?;
                let directory = self.expand_directory(directory);
                let id =
                    self.session
                        .split(target, place, &program, directory.as_deref(), detached)?;
                return Ok(self.tell_about(id, print.as_deref()));
            }
            Command::SpreadLayout { target } => {
                let id = self.session.find_pane(target.as_deref())?;
                let area = self.session.area();
                if let Some(window) = self.session.window_of_mut(id) {
                    window.zoomed = false;
                    window.root.spread(id, area);
                    self.session.relayout();
                }
            }
            Command::SwapPane {
                source,
                target,
                direction,
                detached,
            } => {
                let target = self.session.find_pane(target.as_deref())?;
                let source = match direction {
                    Some(direction) => {
                        let next = self
                            .session
                            .pane_next_to(target, direction == Direction::Down);
                        next.unwrap_or(target)
                    }
                    None => self.session.find_source(source.as_deref())?,
                };
                self.session.swap_panes(source, target, detached);
            }
            Command::SwapWindow {
                source,
                target,
                detached,
            } => {
                let target = self.session.find_window(target.as_deref())?;
                let source = self.session.find_source_window(source.as_deref())?;
                self.session.swap_windows(source, target, detached);
            }
            Command::ToggleZoom { target } => {
                let id = self.session.find_pane(target.as_deref())?;
                self.session.toggle_zoom(id);
            }
        }
        Ok(String::new())
    }

    fn step_layout(&mut self, id: PaneId, forward: bool) {
        if let Some(window) = self.session.window_of(id) {
            let index = window.index;
            let last = Layout::ALL.last().copied();
            let first = Layout::ALL.first().copied();
            let from_nothing = if forward { first } else { last };
            let layout = window.layout.map(|layout| layout.step(forward));
            if let Some(layout) = layout.or(from_nothing) {
                self.session.select_layout(index, layout);
            }
        }
    }

    /// Applies the layout of a name, or else the one applied last.
    fn select_layout(&mut self, id: PaneId, name: Option<&str>) -> Result<(), String> {
        let Some(window) = self.session.window_of(id) else {
            return Ok(());
        };
        let index = window.index;
        let layout = match name {
            Some(name) => {
                Some(Layout::find(name).ok_or_else(|| format!("malformed layout header: {name}"))?)
            }
            None => window.layout,
        };
        if let Some(layout) = layout {
            self.session.select_layout(index, layout);
        }
        Ok(())
    }

    /// What `-c` says, which is a format.
    fn expand_directory(&self, directory: Option<String>) -> Option<PathBuf> {
        let context = Context::current(&self.session);
        directory.map(|directory| PathBuf::from(context.expand(&directory)))
    }

    /// What `-P` prints about a new pane.
    fn tell_about(&self, id: PaneId, format: Option<&str>) -> String {
        format.map_or_else(String::new, |format| {
            let text = Context::of_pane(&self.session, id).expand(format);
            format!("{text}\n")
        })
    }

    fn kill_pane(&mut self, id: PaneId, others: bool) {
        if !others {
            self.session.remove_pane(id);
            return;
        }
        let area = self.session.area();
        let panes = self.session.window_of(id).map(|w| w.root.panes(area));
        for (other, _) in panes.unwrap_or_default() {
            if other != id {
                self.session.remove_pane(other);
            }
        }
    }

    fn kill_window(&mut self, index: u32, others: bool) {
        if !others {
            self.session.kill_window(index);
            return;
        }
        let windows = self.session.windows.iter().map(|window| window.index);
        let others: Vec<u32> = windows.filter(|other| *other != index).collect();
        for other in others {
            self.session.kill_window(other);
        }
    }

    fn display_message(
        &mut self,
        target: Option<&str>,
        print: bool,
        format: Option<&str>,
    ) -> Result<String, String> {
        let id = self.session.find_pane(target)?;
        let format = format.unwrap_or(DISPLAY_MESSAGE);
        let format = with_local_time(format);
        let text = Context::of_pane(&self.session, id).expand(&format);
        if print {
            return Ok(format!("{text}\n"));
        }
        self.client().ui.show_message(text);
        Ok(String::new())
    }

    fn detach(&mut self) {
        if let Some(id) = self.acting
            && let Some(connection) = self.connection_mut(id)
            && connection.client.take().is_some()
        {
            connection.finish(&ServerMessage::Exit("[detached]".to_owned()));
        }
    }

    fn start_copy_mode(&mut self, id: PaneId, page_up: bool) {
        if let Some(pane) = self.session.panes.get_mut(&id) {
            CopyMode::enter(pane, false);
            if page_up {
                copy_mode::run(pane, Action::Scroll(Scroll::PageUp));
            }
        }
    }

    fn list_windows(&self, all: bool, format: Option<&str>, filter: Option<&str>) -> String {
        let session = &self.session;
        let default = if all {
            format!("#{{session_name}}:{LIST_WINDOWS}")
        } else {
            LIST_WINDOWS.to_owned()
        };
        let format = format.unwrap_or(&default);
        let windows = session.windows.iter();
        let contexts = windows.map(|window| Context::of_window(session, window));
        listing(contexts, format, filter)
    }

    fn list_panes(
        &self,
        scope: PaneScope,
        target: Option<&str>,
        format: Option<&str>,
        filter: Option<&str>,
    ) -> Result<String, String> {
        let session = &self.session;
        let default = match scope {
            PaneScope::Window => LIST_PANES.to_owned(),
            PaneScope::Session => format!("#{{window_index}}.{LIST_PANES}"),
            PaneScope::All => format!("#{{session_name}}:#{{window_index}}.{LIST_PANES}"),
        };
        let index = session.find_window(target)?;
        let windows = session.windows.iter();
        let windows = windows.filter(|window| scope != PaneScope::Window || window.index == index);
        let area = session.area();
        let panes = windows.flat_map(|window| window.root.panes(area));
        let contexts = panes.map(|(id, _)| Context::of_pane(session, id));
        Ok(listing(contexts, format.unwrap_or(&default), filter))
    }

    /// A report in pixels as the cell it is in, for everything but the program
    /// that asked for them. Pixels can lie in the padding beyond the last cell.
    fn in_cells(&self, event: MouseEvent) -> MouseEvent {
        if !self.client_ref().pixel_mouse {
            return event;
        }
        let (width, height) = self.session.cell_size();
        let (x, y) = event.report;
        let size = self.session.size;
        MouseEvent {
            x: x.checked_div(width)
                .unwrap_or(0)
                .min(size.cols.saturating_sub(1)),
            y: y.checked_div(height)
                .unwrap_or(0)
                .min(size.rows.saturating_sub(1)),
            ..event
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) {
        self.dirty = true;
        let client = self.client();
        client.pointer = Some((event.x, event.y));
        if client.ui.list.is_some() {
            self.list_mouse(event);
            return;
        }
        match client.drag.clone() {
            Drag::None => {
                if event.action == MouseAction::Press {
                    self.mouse_press(event);
                } else if event.action == MouseAction::Motion {
                    self.mouse_hover(event);
                }
            }
            Drag::Divider(divider) => self.drag_divider(&divider, event),
            Drag::Selecting(selecting) => self.drag_selection(selecting, event),
            Drag::Forwarding(pane) => {
                self.forward_mouse(pane, event);
                if event.action == MouseAction::Release {
                    self.client().drag = Drag::None;
                }
            }
        }
    }

    /// Programs that track all motion get it without a button held.
    fn mouse_hover(&mut self, event: MouseEvent) {
        if let Some(id) = self.session.pane_at(event.x, event.y)
            && self.pane_takes_mouse(id, event)
        {
            self.forward_mouse(id, event);
        }
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
        let cell_size = self.session.cell_size();
        let pixels = self.client().pixel_mouse.then_some(event.report);
        let Some(pane) = self.session.panes.get_mut(&id) else {
            return;
        };
        let (x, y) = pixels_in_pane(pane, event, pixels, cell_size);
        let held = event.action == MouseAction::Press
            || (event.action == MouseAction::Motion && event.button.is_some());
        let input = MouseInput {
            action: event.action,
            button: event.button,
            mods: event.mods,
            x,
            y,
            cell_size,
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
                self.client().drag = Drag::Divider(divider);
            }
        } else if let Some(id) = self.session.pane_at(event.x, event.y) {
            self.press_in_pane(id, event);
        }
    }

    fn click_status(&mut self, event: MouseEvent) {
        match event.button {
            Some(MouseButton::Left) => {
                let entries = &self.client().renderer.status_entries;
                let entry = entries
                    .iter()
                    .find(|entry| (entry.start..entry.end).contains(&event.x));
                if let Some(index) = entry.map(|entry| entry.window) {
                    self.session.select_window(index);
                }
            }
            Some(MouseButton::WheelUp) => {
                self.session.previous_window(false);
            }
            Some(MouseButton::WheelDown) => {
                self.session.next_window(false);
            }
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
            self.client().drag = Drag::Forwarding(id);
            return;
        }
        match event.button {
            Some(MouseButton::Left) => self.press_to_select(id, event),
            Some(MouseButton::Middle) => {
                let options = PasteOptions {
                    bracket: true,
                    ..PasteOptions::default()
                };
                self.run_interactive(Command::Buffer(BufferCommand::Paste(options)));
            }
            _ => {}
        }
    }

    fn click_count(&mut self, event: MouseEvent) -> u8 {
        let now = Instant::now();
        let client = self.client();
        let count = match client.last_click {
            Some(click)
                if click.x == event.x
                    && click.y == event.y
                    && now.duration_since(click.at) < MULTI_CLICK_TIME =>
            {
                click.count % 3 + 1
            }
            _ => 1,
        };
        client.last_click = Some(Click {
            at: now,
            x: event.x,
            y: event.y,
            count,
        });
        count
    }

    fn press_to_select(&mut self, id: PaneId, event: MouseEvent) {
        let Some(pane) = self.session.panes.get_mut(&id) else {
            return;
        };
        let (x, y) = in_pane(pane, event);
        copy_mode::forget_selection(pane);
        pane.terminal.press_selection(x, y);
        pane.redraw = true;
        self.client().drag = Drag::Selecting(Selecting {
            pane: id,
            x,
            y,
            scroll: None,
        });
    }

    fn drag_divider(&mut self, divider: &Divider, event: MouseEvent) {
        match event.action {
            MouseAction::Motion => {
                if let Some(window) = self.session.window_mut() {
                    window.root.move_divider(divider, event.x, event.y);
                }
                self.session.relayout();
            }
            MouseAction::Release | MouseAction::Press => self.client().drag = Drag::None,
        }
    }

    fn drag_selection(&mut self, selecting: Selecting, event: MouseEvent) {
        if event.action != MouseAction::Motion {
            self.client().drag = Drag::None;
        }
        let Some(pane) = self.session.panes.get_mut(&selecting.pane) else {
            self.client().drag = Drag::None;
            return;
        };
        if event.action != MouseAction::Motion {
            pane.terminal.release_selection();
            let Some(text) = pane.terminal.selected_text() else {
                return;
            };
            if pane.copy_mode.is_some() {
                CopyMode::exit(pane);
            } else {
                pane.terminal.clear_selection();
                pane.redraw = true;
            }
            self.store_buffer(text);
            return;
        }
        let (x, y) = in_pane(pane, event);
        let edge = edge_of(pane, (selecting.x, selecting.y), (x, y));
        // Going through copy mode as tmux does means that scrolling while
        // selecting ends the way scrolling otherwise does.
        CopyMode::enter(pane, false);
        pane.terminal.drag_selection(x, y, edge);
        copy_mode::follow_pointer(pane, x, y);
        // It goes along right away, and at its own pace from there.
        let scroll = edge.map(|edge| match selecting.scroll {
            Some((current, at)) if current == edge => (edge, at),
            _ => (edge, Instant::now()),
        });
        self.client().drag = Drag::Selecting(Selecting {
            x,
            y,
            scroll,
            ..selecting
        });
    }

    /// Moves the text along with a pointer that stays at the edge of a pane.
    pub fn autoscroll(&mut self) {
        let Drag::Selecting(selecting) = self.client().drag else {
            return;
        };
        let Some((edge, _)) = selecting.scroll else {
            return;
        };
        let Some(pane) = self.session.panes.get_mut(&selecting.pane) else {
            self.client().drag = Drag::None;
            return;
        };
        let (x, y) = (selecting.x, selecting.y);
        let goes_on = pane.terminal.autoscroll_selection(x, y, edge);
        copy_mode::follow_pointer(pane, x, y);
        self.dirty = true;
        let next = Instant::now() + AUTOSCROLL_TIME;
        self.client().drag = Drag::Selecting(Selecting {
            scroll: goes_on.then_some((edge, next)),
            ..selecting
        });
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
            send_arrow(pane, up);
        } else {
            scroll_history(pane, up);
        }
    }
}

/// Full screen programs without mouse support scroll with the arrow keys, one
/// for each event, as Ghostty already reports a notch of the wheel as several,
/// by its `mouse-scroll-multiplier`.
fn send_arrow(pane: &mut Pane, up: bool) {
    let input = KeyInput {
        action: KeyAction::Press,
        key: if up { key::ARROW_UP } else { key::ARROW_DOWN },
        mods: 0,
        consumed_mods: 0,
        text: "",
        unshifted: 0,
    };
    let bytes = pane.terminal.encode_key(&input);
    pane.send(&bytes);
}

/// The wheel scrolls history, by way of copy mode as in tmux, a row at a time for
/// the same reason as above.
fn scroll_history(pane: &mut Pane, up: bool) {
    if up {
        CopyMode::enter(pane, true);
        copy_mode::scroll_view(pane, -1);
    } else if pane.copy_mode.is_some() {
        copy_mode::scroll_view(pane, 1);
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
