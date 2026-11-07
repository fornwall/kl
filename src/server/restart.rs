//! Replacing the server with another binary while every program keeps running.
//!
//! `exec` keeps the process, so programs stay its children, and it keeps every
//! descriptor that is not close-on-exec, so their ptys stay open and the socket
//! keeps taking connections. What is left to carry over is what lives in memory,
//! which goes through a file next to the socket.

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::Server;
use super::buffers::{Buffer, Buffers};
use super::copy_mode::CopyMode;
use super::layout::{Layout, Node, PaneId};
use super::pane::Pane;
use super::session::{Monitor, Session, Window};
use crate::command::SplitKind;
use crate::ghostty::{OuterColors, Rgb, Terminal};
use crate::proto::{self, Malformed, Reader, ServerMessage};
use crate::sys;

/// Bumped on any change to the file written here. Failing to read it would end
/// every program, so a binary with another version is refused beforehand.
pub const STATE_VERSION: u32 = 9;

/// What a server starts from when it takes over from another one.
pub struct Restored {
    pub listener: UnixListener,
    pub session: Session,
    pub buffers: Buffers,
}

/// Whether this binary can read what this server is about to write.
fn check_successor(binary: &Path) -> Result<(), String> {
    let output = Command::new(binary)
        .arg("--state-version")
        .output()
        .map_err(|error| format!("{}: {error}", binary.display()))?;
    if String::from_utf8_lossy(&output.stdout).trim() == STATE_VERSION.to_string() {
        Ok(())
    } else {
        Err(format!(
            "{} cannot take over from this server",
            binary.display()
        ))
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bool(out: &mut Vec<u8>, value: bool) {
    out.push(u8::from(value));
}

fn put_optional_u32(out: &mut Vec<u8>, value: Option<u32>) {
    put_bool(out, value.is_some());
    put_u32(out, value.unwrap_or(0));
}

fn take_optional_u32(reader: &mut Reader<'_>) -> Result<Option<u32>, Malformed> {
    let present = reader.take_bool()?;
    let value = reader.take_u32()?;
    Ok(present.then_some(value))
}

fn put_color(out: &mut Vec<u8>, color: Option<Rgb>) {
    put_bool(out, color.is_some());
    let Rgb { r, g, b } = color.unwrap_or_default();
    out.extend_from_slice(&[r, g, b]);
}

fn take_color(reader: &mut Reader<'_>) -> Result<Option<Rgb>, Malformed> {
    let present = reader.take_bool()?;
    let [r, g, b] = reader.take_array()?;
    Ok(present.then_some(Rgb { r, g, b }))
}

fn put_colors(out: &mut Vec<u8>, colors: &OuterColors) {
    put_color(out, colors.foreground);
    put_color(out, colors.background);
    put_bool(out, colors.dark.is_some());
    put_bool(out, colors.dark.unwrap_or(false));
    put_bool(out, colors.cursor_style.is_some());
    out.push(colors.cursor_style.unwrap_or(0));
    put_u32(out, u32::try_from(colors.palette.len()).unwrap_or(0));
    for (index, Rgb { r, g, b }) in &colors.palette {
        out.extend_from_slice(&[*index, *r, *g, *b]);
    }
}

fn take_colors(reader: &mut Reader<'_>) -> Result<OuterColors, Malformed> {
    let foreground = take_color(reader)?;
    let background = take_color(reader)?;
    let known = reader.take_bool()?;
    let dark = reader.take_bool()?;
    let has_cursor_style = reader.take_bool()?;
    let [cursor_style] = reader.take_array()?;
    let mut palette = BTreeMap::new();
    for _ in 0..reader.take_u32()? {
        let [index, r, g, b] = reader.take_array()?;
        palette.insert(index, Rgb { r, g, b });
    }
    Ok(OuterColors {
        foreground,
        background,
        palette,
        dark: known.then_some(dark),
        cursor_style: has_cursor_style.then_some(cursor_style),
    })
}

fn put_fd(out: &mut Vec<u8>, fd: BorrowedFd<'_>) {
    put_u32(out, fd.as_raw_fd().cast_unsigned());
}

/// Takes ownership of a descriptor the previous server left open.
fn take_fd(reader: &mut Reader<'_>) -> Result<OwnedFd, Box<dyn Error>> {
    let fd: RawFd = reader.take_u32()?.cast_signed();
    // SAFETY: The previous server wrote this number for a descriptor it kept
    // open across `exec`, and each one appears once, so nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    sys::set_cloexec(fd.as_fd(), true)?;
    Ok(fd)
}

fn put_node(out: &mut Vec<u8>, node: &Node) {
    match node {
        Node::Leaf(id) => {
            out.push(0);
            put_u32(out, *id);
        }
        Node::Split {
            kind,
            shares,
            children,
        } => {
            out.push(match kind {
                SplitKind::Horizontal => 1,
                SplitKind::Vertical => 2,
            });
            put_u32(out, u32::try_from(children.len()).unwrap_or(0));
            for (share, child) in shares.iter().zip(children) {
                put_u32(out, *share);
                put_node(out, child);
            }
        }
    }
}

fn take_node(reader: &mut Reader<'_>) -> Result<Node, Malformed> {
    let kind = match reader.take_array()? {
        [0] => return reader.take_u32().map(Node::Leaf),
        [1] => SplitKind::Horizontal,
        [2] => SplitKind::Vertical,
        _ => return Err(Malformed),
    };
    let count = reader.take_u32()?;
    if count < 2 {
        return Err(Malformed);
    }
    let mut shares = Vec::new();
    let mut children = Vec::new();
    for _ in 0..count {
        shares.push(reader.take_u32()?);
        children.push(take_node(reader)?);
    }
    Ok(Node::Split {
        kind,
        shares,
        children,
    })
}

const fn monitor_byte(monitor: Monitor) -> u8 {
    match monitor {
        Monitor::Off => 0,
        Monitor::Watching => 1,
        Monitor::Flagged => 2,
    }
}

fn take_monitor(reader: &mut Reader<'_>) -> Result<Monitor, Malformed> {
    match reader.take_array()? {
        [0] => Ok(Monitor::Off),
        [1] => Ok(Monitor::Watching),
        [2] => Ok(Monitor::Flagged),
        _ => Err(Malformed),
    }
}

fn put_window(out: &mut Vec<u8>, window: &Window) {
    put_u32(out, window.id);
    put_u32(out, window.index);
    proto::put_bytes(out, window.name.as_bytes());
    put_bool(out, window.automatic_name);
    put_node(out, &window.root);
    put_u32(out, window.active);
    put_optional_u32(out, window.last_active);
    put_bool(out, window.zoomed);
    put_bool(out, window.bell);
    out.push(monitor_byte(window.monitor));
    let layout = window
        .layout
        .and_then(|layout| Layout::ALL.iter().position(|other| *other == layout));
    put_optional_u32(out, layout.and_then(|index| u32::try_from(index).ok()));
}

fn take_window(reader: &mut Reader<'_>) -> Result<Window, Malformed> {
    Ok(Window {
        id: reader.take_u32()?,
        index: reader.take_u32()?,
        name: reader.take_text()?,
        automatic_name: reader.take_bool()?,
        root: take_node(reader)?,
        active: reader.take_u32()?,
        last_active: take_optional_u32(reader)?,
        zoomed: reader.take_bool()?,
        bell: reader.take_bool()?,
        monitor: take_monitor(reader)?,
        layout: take_optional_u32(reader)?
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| Layout::ALL.get(index).copied()),
        next_name: None,
    })
}

fn put_pane(out: &mut Vec<u8>, pane: &Pane) {
    put_u32(out, pane.id);
    put_u32(out, pane.pid);
    put_bool(out, pane.start_command.is_some());
    proto::put_bytes(out, pane.start_command.as_deref().unwrap_or("").as_bytes());
    put_fd(out, pane.pty());
    // A terminal that cannot be saved right now loses what it shows, but its
    // program keeps running, and full screen ones draw again when resized.
    proto::put_bytes(out, &pane.terminal.snapshot().unwrap_or_default());
}

fn take_pane(reader: &mut Reader<'_>, colors: &OuterColors) -> Result<Pane, Box<dyn Error>> {
    let id: PaneId = reader.take_u32()?;
    let pid = reader.take_u32()?;
    let has_start_command = reader.take_bool()?;
    let start_command = reader.take_text()?;
    let pty = File::from(take_fd(reader)?);
    let snapshot = reader.take_bytes()?;
    let mut terminal = Terminal::restore(snapshot).or_else(|error| {
        sys::log(format_args!("restoring pane {id}: {error}"));
        Terminal::new(1, 1)
    })?;
    terminal.set_outer_colors(colors);
    let start_command = has_start_command.then_some(start_command);
    Ok(Pane::adopt(id, pid, start_command, pty, terminal))
}

fn put_buffers(out: &mut Vec<u8>, buffers: &Buffers) {
    put_u32(out, buffers.next_index);
    put_u32(out, u32::try_from(buffers.list.len()).unwrap_or(u32::MAX));
    for buffer in &buffers.list {
        proto::put_bytes(out, buffer.name.as_bytes());
        proto::put_bytes(out, buffer.text.as_bytes());
        put_bool(out, buffer.automatic);
        out.extend_from_slice(&buffer.created.to_le_bytes());
    }
}

fn take_buffers(reader: &mut Reader<'_>) -> Result<Buffers, Malformed> {
    let next_index = reader.take_u32()?;
    let list = (0..reader.take_u32()?)
        .map(|_| {
            Ok(Buffer {
                name: reader.take_text()?,
                text: reader.take_text()?,
                automatic: reader.take_bool()?,
                created: u64::from_le_bytes(reader.take_array()?),
            })
        })
        .collect::<Result<_, Malformed>>()?;
    Ok(Buffers { list, next_index })
}

/// Reads what [`Server::restart`] left behind, which is only good for one use.
pub fn read_state(state: &Path, socket: &Path) -> Result<Restored, Box<dyn Error>> {
    let bytes = fs::read(state)?;
    fs::remove_file(state)?;
    let mut reader = Reader::new(&bytes);
    let reader = &mut reader;

    let listener = UnixListener::from(take_fd(reader)?);
    let buffers = take_buffers(reader)?;
    let mut session = Session::new(socket.display().to_string());
    session.current = reader.take_u32()?;
    session.last = take_optional_u32(reader)?;
    session.size = reader.take_size()?;
    session.colors = take_colors(reader)?;
    session.next_pane = reader.take_u32()?;
    session.next_window = reader.take_u32()?;
    session.marked = take_optional_u32(reader)?;
    session.environment = reader.take_texts_counted()?;
    for _ in 0..reader.take_u32()? {
        session.windows.push(take_window(reader)?);
    }
    for _ in 0..reader.take_u32()? {
        let pane = take_pane(reader, &session.colors)?;
        session.panes.insert(pane.id, pane);
    }
    session.relayout();
    Ok(Restored {
        listener,
        session,
        buffers,
    })
}

impl Server {
    fn encode_state(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let session = &self.session;
        put_fd(&mut out, self.listener.as_fd());
        put_buffers(&mut out, &self.buffers);
        put_u32(&mut out, session.current);
        put_optional_u32(&mut out, session.last);
        out.extend(proto::size_bytes(session.size));
        put_colors(&mut out, &session.colors);
        put_u32(&mut out, session.next_pane);
        put_u32(&mut out, session.next_window);
        put_optional_u32(&mut out, session.marked);
        proto::put_texts_counted(&mut out, &session.environment);
        let count = |len| u32::try_from(len).unwrap_or(u32::MAX);
        put_u32(&mut out, count(session.windows.len()));
        for window in &session.windows {
            put_window(&mut out, window);
        }
        put_u32(&mut out, count(session.panes.len()));
        for pane in session.panes.values() {
            put_pane(&mut out, pane);
        }
        out
    }

    /// What has to stay open for the next server.
    fn inherited(&self) -> Vec<BorrowedFd<'_>> {
        let panes = self.session.panes.values().map(Pane::pty);
        panes.chain([self.listener.as_fd()]).collect()
    }

    /// Becomes `binary`, or this binary again. Only returns if that failed, and
    /// then this server just carries on.
    pub fn restart(&mut self, binary: Option<PathBuf>) -> Result<(), String> {
        let binary = binary
            .map_or_else(env::current_exe, Ok)
            .map_err(|error| format!("finding this binary: {error}"))?;
        check_successor(&binary)?;
        for pane in self.session.panes.values_mut() {
            if pane.copy_mode.is_some() {
                CopyMode::exit(pane);
            }
        }
        let socket = PathBuf::from(&self.session.socket);
        let state = socket.with_file_name("restart-state");
        fs::write(&state, self.encode_state())
            .map_err(|error| format!("{}: {error}", state.display()))?;
        self.goodbye(&ServerMessage::Reattach);

        let result = self
            .inherited()
            .into_iter()
            .try_for_each(|fd| sys::set_cloexec(fd, false));
        let error = match result {
            Ok(()) => Command::new(&binary)
                .arg("--server")
                .arg(socket)
                .arg("--restore")
                .arg(&state)
                .exec(),
            Err(error) => error,
        };
        for fd in self.inherited() {
            if let Err(error) = sys::set_cloexec(fd, true) {
                sys::log(format_args!("after failing to restart: {error}"));
            }
        }
        if let Err(error) = fs::remove_file(&state) {
            sys::log(format_args!("{}: {error}", state.display()));
        }
        Err(format!("{}: {error}", binary.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::layout::Rect;
    use super::*;
    use crate::command::Place;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    const fn place(kind: SplitKind) -> Place {
        Place {
            kind,
            before: false,
            full: false,
            size: None,
        }
    }

    #[test]
    fn layouts_survive_a_round_trip() {
        let mut node = Node::Leaf(1);
        node.place(1, 2, place(SplitKind::Horizontal), AREA);
        node.place(2, 3, place(SplitKind::Vertical), AREA);
        let mut bytes = Vec::new();
        put_node(&mut bytes, &node);
        assert_eq!(take_node(&mut Reader::new(&bytes)), Ok(node));
    }

    #[test]
    fn a_truncated_layout_is_an_error() {
        let mut node = Node::Leaf(1);
        node.place(1, 2, place(SplitKind::Horizontal), AREA);
        let mut bytes = Vec::new();
        put_node(&mut bytes, &node);
        bytes.pop();
        assert_eq!(take_node(&mut Reader::new(&bytes)), Err(Malformed));
    }

    #[test]
    fn buffers_survive_a_round_trip() {
        let buffers = Buffers {
            list: vec![Buffer {
                name: "name".to_owned(),
                text: "text".to_owned(),
                automatic: true,
                created: 1_800_000_000,
            }],
            next_index: 3,
        };
        let mut bytes = Vec::new();
        put_buffers(&mut bytes, &buffers);
        assert_eq!(take_buffers(&mut Reader::new(&bytes)), Ok(buffers));
    }

    #[test]
    fn colors_survive_a_round_trip() {
        let rgb = Rgb { r: 1, g: 2, b: 3 };
        let colors = OuterColors {
            foreground: Some(rgb),
            background: None,
            palette: BTreeMap::from([(0, rgb), (255, rgb)]),
            dark: Some(false),
            cursor_style: Some(5),
        };
        let mut bytes = Vec::new();
        put_colors(&mut bytes, &colors);
        let restored = take_colors(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(restored.foreground, colors.foreground);
        assert_eq!(restored.background, colors.background);
        assert_eq!(restored.palette, colors.palette);
        assert_eq!(restored.dark, colors.dark);
        assert_eq!(restored.cursor_style, colors.cursor_style);
    }

    #[test]
    fn refuses_a_binary_that_cannot_take_over() {
        assert!(check_successor(Path::new("/bin/false")).is_err());
        assert!(check_successor(Path::new("/no/such/binary")).is_err());
    }
}
