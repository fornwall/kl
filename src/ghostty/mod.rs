//! Safe wrapper around libghostty-vt.
//!
//! A [`Terminal`] is one complete terminal emulator. Bytes go in through
//! [`Terminal::write`], and everything a program asks of the terminal that it
//! cannot do by itself lands in [`Effects`] for the caller to act on.

mod ffi;
pub mod key;

use std::ffi::{c_int, c_void};
use std::ptr;
use std::slice;
use std::time::{Duration, Instant};

pub use ffi::{Rgb, Scrollbar, Style, StyleColor};

/// How long a program may freeze its pane with synchronized output.
const RENDER_HOLD_LIMIT: Duration = Duration::from_secs(1);

/// The library's default is 10,000 bytes, about one page of history. This
/// matches the default of the Ghostty app.
const SCROLLBACK_BYTES: usize = 10_000_000;

/// How long one round of compressing may keep the server from everything else.
const COMPRESSION_BUDGET: Duration = Duration::from_millis(2);

const VERSION_REPLY: &str = concat!("kl ", env!("CARGO_PKG_VERSION"));

pub const STYLE_BOLD: u16 = 1 << 0;
pub const STYLE_ITALIC: u16 = 1 << 1;
pub const STYLE_FAINT: u16 = 1 << 2;
pub const STYLE_BLINK: u16 = 1 << 3;
pub const STYLE_INVERSE: u16 = 1 << 4;
pub const STYLE_INVISIBLE: u16 = 1 << 5;
pub const STYLE_STRIKETHROUGH: u16 = 1 << 6;
pub const STYLE_OVERLINE: u16 = 1 << 7;
pub const STYLE_UNDERLINE_SHIFT: u16 = 8;
pub const STYLE_UNDERLINE_MASK: u16 = 0x7 << STYLE_UNDERLINE_SHIFT;

/// DEC private modes, as `GhosttyMode` values.
pub const MODE_FOCUS_EVENT: u16 = 1004;
pub const MODE_ALTERNATE_SCROLL: u16 = 1007;
pub const MODE_BRACKETED_PASTE: u16 = 2004;
const MODE_SYNCHRONIZED_OUTPUT: u16 = 2026;

/// A `GhosttyResult` other than success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(c_int);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "libghostty-vt error {}", self.0)
    }
}

impl std::error::Error for Error {}

const fn check(result: c_int) -> Result<(), Error> {
    if result == ffi::SUCCESS {
        Ok(())
    } else {
        Err(Error(result))
    }
}

/// For calls whose failure only degrades the result, such as a color programs
/// can no longer query. Logged, since there is nothing better to do with it.
fn best_effort(result: Result<(), Error>) {
    if let Err(error) = result {
        crate::sys::log(error);
    }
}

/// Text of one cell. Almost every grapheme fits inline, which keeps compositing
/// a frame free of allocations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Text {
    #[default]
    Blank,
    Inline {
        len: u8,
        bytes: [u8; 14],
    },
    Heap(Box<str>),
}

impl Text {
    pub fn new(text: &str) -> Self {
        let mut bytes = [0; 14];
        match (bytes.get_mut(..text.len()), u8::try_from(text.len())) {
            (_, Ok(0)) => Self::Blank,
            (Some(slot), Ok(len)) => {
                slot.copy_from_slice(text.as_bytes());
                Self::Inline { len, bytes }
            }
            _ => Self::Heap(text.into()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Blank => " ",
            Self::Inline { len, bytes } => bytes
                .get(..usize::from(*len))
                .and_then(|text| std::str::from_utf8(text).ok())
                .unwrap_or(" "),
            Self::Heap(text) => text,
        }
    }
}

/// Which part of a character a cell holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Width {
    #[default]
    Narrow,
    /// The left half of a double width character.
    Wide,
    /// The right half of a double width character. It holds no text.
    Tail,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub text: Text,
    pub style: Style,
    pub width: Width,
    pub selected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Position in the viewport, if the cursor is inside of it.
    pub position: Option<(u16, u16)>,
    pub visible: bool,
    pub blinking: bool,
    pub shape: CursorShape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Bar,
    Block,
    Underline,
}

impl Scrollbar {
    /// Rows between the bottom of the viewport and the end of the screen.
    pub const fn lines_below(self) -> u64 {
        self.total.saturating_sub(self.offset + self.len)
    }

    /// Rows of history above the active screen.
    pub const fn history(self) -> u64 {
        self.total.saturating_sub(self.len)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Release = 0,
    Press = 1,
    Repeat = 2,
}

/// A key event in the form the encoder wants it.
#[derive(Clone, Copy, Debug)]
pub struct KeyInput<'a> {
    pub action: KeyAction,
    /// A `GhosttyKey` value, see [`key`].
    pub key: c_int,
    pub mods: u16,
    pub consumed_mods: u16,
    pub text: &'a str,
    pub unshifted: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press = 0,
    Release = 1,
    Motion = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left = 1,
    Right = 2,
    Middle = 3,
    WheelUp = 4,
    WheelDown = 5,
    WheelLeft = 6,
    WheelRight = 7,
}

/// A mouse event at a cell position of a terminal.
#[derive(Clone, Copy, Debug)]
pub struct MouseInput {
    pub action: MouseAction,
    pub button: Option<MouseButton>,
    pub mods: u16,
    pub x: u16,
    pub y: u16,
    pub any_pressed: bool,
}

/// What programs asked of the terminal. Filled in during [`Terminal::write`]
/// because libghostty-vt reports these from callbacks, which must not touch the
/// terminal. The caller drains it after each write.
pub struct Effects {
    /// Bytes to send back to the program, such as query replies.
    pub replies: Vec<u8>,
    pub bell: bool,
    pub title_changed: bool,
    /// Text a program wants on the clipboard, oldest first.
    pub clipboard: Vec<Vec<u8>>,
    render_state: ffi::Handle,
    hold_started: Option<Instant>,
    size: (u16, u16, u32, u32),
    dark: bool,
}

/// # Safety
///
/// `userdata` must be the pointer registered in [`Terminal::new`].
unsafe fn effects_of<'a>(userdata: *mut c_void) -> &'a mut Effects {
    // SAFETY: Per this function's contract, this is the live allocation that
    // `Terminal` owns. The terminal is inside `vt_write`, so no other reference
    // to it exists during the callback.
    unsafe { &mut *userdata.cast::<Effects>() }
}

/// # Safety
///
/// Unless `len` is zero, `ptr` must be valid for reads of `len` bytes.
const unsafe fn bytes_of<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 || ptr.is_null() {
        return &[];
    }
    // SAFETY: Non-empty here, and the caller guarantees validity.
    unsafe { slice::from_raw_parts(ptr, len) }
}

unsafe extern "C" fn on_write_pty(
    _terminal: ffi::Handle,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    // SAFETY: Called by libghostty-vt with the registered userdata.
    let effects = unsafe { effects_of(userdata) };
    // SAFETY: libghostty-vt passes a buffer valid for the duration of the call.
    let data = unsafe { bytes_of(data, len) };
    effects.replies.extend_from_slice(data);
}

unsafe extern "C" fn on_bell(_terminal: ffi::Handle, userdata: *mut c_void) {
    // SAFETY: Called by libghostty-vt with the registered userdata.
    let effects = unsafe { effects_of(userdata) };
    effects.bell = true;
}

unsafe extern "C" fn on_title_changed(_terminal: ffi::Handle, userdata: *mut c_void) {
    // SAFETY: Called by libghostty-vt with the registered userdata.
    let effects = unsafe { effects_of(userdata) };
    effects.title_changed = true;
}

const unsafe extern "C" fn on_xtversion(
    _terminal: ffi::Handle,
    _userdata: *mut c_void,
) -> ffi::Bytes {
    ffi::Bytes {
        ptr: VERSION_REPLY.as_ptr(),
        len: VERSION_REPLY.len(),
    }
}

unsafe extern "C" fn on_size(
    _terminal: ffi::Handle,
    userdata: *mut c_void,
    out: *mut ffi::SizeReport,
) -> bool {
    // SAFETY: Called by libghostty-vt with the registered userdata.
    let effects = unsafe { effects_of(userdata) };
    let (columns, rows, cell_width, cell_height) = effects.size;
    let report = ffi::SizeReport {
        rows,
        columns,
        cell_width,
        cell_height,
    };
    // SAFETY: libghostty-vt passes a valid pointer to write the answer to.
    unsafe { out.write(report) };
    true
}

unsafe extern "C" fn on_color_scheme(
    _terminal: ffi::Handle,
    userdata: *mut c_void,
    out: *mut c_int,
) -> bool {
    // SAFETY: Called by libghostty-vt with the registered userdata.
    let effects = unsafe { effects_of(userdata) };
    let scheme = if effects.dark {
        ffi::COLOR_SCHEME_DARK
    } else {
        ffi::COLOR_SCHEME_LIGHT
    };
    // SAFETY: libghostty-vt passes a valid pointer to write the answer to.
    unsafe { out.write(scheme) };
    true
}

unsafe extern "C" fn on_clipboard_write(
    _terminal: ffi::Handle,
    userdata: *mut c_void,
    write: *const ffi::ClipboardWrite,
) {
    // SAFETY: Called by libghostty-vt with the registered userdata.
    let effects = unsafe { effects_of(userdata) };
    // SAFETY: libghostty-vt passes a request valid for the duration of the call.
    let request = unsafe { &*write };
    // SAFETY: `contents` holds `contents_len` entries.
    let contents = unsafe { contents_of(request) };
    // Every entry is a representation of the same value, and the outer terminal
    // can only take text, which comes first.
    if let Some(content) = contents.first() {
        // SAFETY: The content bytes are valid for the duration of the call.
        let data = unsafe { bytes_of(content.data.ptr, content.data.len) };
        effects.clipboard.push(data.to_vec());
    }
    let reply = ffi::ClipboardWriteReply {
        size: size_of::<ffi::ClipboardWriteReply>(),
        result: ffi::CLIPBOARD_WRITE_SUCCESS,
        remember: false,
    };
    // SAFETY: `reply` is the callback the request says to answer with.
    unsafe { (request.reply)(write, &raw const reply) };
}

/// # Safety
///
/// `request` must come from libghostty-vt.
const unsafe fn contents_of(request: &ffi::ClipboardWrite) -> &[ffi::ClipboardContent] {
    if request.contents_len == 0 || request.contents.is_null() {
        return &[];
    }
    // SAFETY: The library guarantees `contents_len` valid entries.
    unsafe { slice::from_raw_parts(request.contents, request.contents_len) }
}

/// Per the library's rendering guide, a hold captures the frame the program
/// wants left on screen at the moment it starts.
unsafe extern "C" fn on_render_hold(terminal: ffi::Handle, userdata: *mut c_void, held: bool) {
    // SAFETY: Called by libghostty-vt with the registered userdata.
    let effects = unsafe { effects_of(userdata) };
    if held {
        // SAFETY: Both handles are live. The render state is not the terminal,
        // so this does not re-enter the write that called us.
        let result = unsafe { ffi::ghostty_render_state_update(effects.render_state, terminal) };
        effects.hold_started = (result == ffi::SUCCESS).then(Instant::now);
    } else {
        effects.hold_started = None;
    }
}

/// Owned handles for one libghostty-vt object each, freed on drop.
struct Handles {
    terminal: ffi::Handle,
    render_state: ffi::Handle,
    rows: ffi::Handle,
    cells: ffi::Handle,
    key_encoder: ffi::Handle,
    key_event: ffi::Handle,
    mouse_encoder: ffi::Handle,
    mouse_event: ffi::Handle,
}

impl Handles {
    const fn null() -> Self {
        Self {
            terminal: ptr::null_mut(),
            render_state: ptr::null_mut(),
            rows: ptr::null_mut(),
            cells: ptr::null_mut(),
            key_encoder: ptr::null_mut(),
            key_event: ptr::null_mut(),
            mouse_encoder: ptr::null_mut(),
            mouse_event: ptr::null_mut(),
        }
    }

    /// Allocates everything. On error the handles made so far are freed by drop.
    fn allocate(&mut self, cols: u16, rows: u16) -> Result<(), Error> {
        let none = ptr::null();
        // SAFETY: A null allocator selects the default, and the out pointer is valid.
        check(unsafe { ffi::ghostty_terminal_new(none, &raw mut self.terminal, cols, rows) })?;
        // SAFETY: As above.
        check(unsafe { ffi::ghostty_render_state_new(none, &raw mut self.render_state) })?;
        // SAFETY: As above.
        check(unsafe { ffi::ghostty_render_state_row_iterator_new(none, &raw mut self.rows) })?;
        // SAFETY: As above.
        check(unsafe { ffi::ghostty_render_state_row_cells_new(none, &raw mut self.cells) })?;
        // SAFETY: As above.
        check(unsafe { ffi::ghostty_key_encoder_new(none, &raw mut self.key_encoder) })?;
        // SAFETY: As above.
        check(unsafe { ffi::ghostty_key_event_new(none, &raw mut self.key_event) })?;
        // SAFETY: As above.
        check(unsafe { ffi::ghostty_mouse_encoder_new(none, &raw mut self.mouse_encoder) })?;
        // SAFETY: As above.
        check(unsafe { ffi::ghostty_mouse_event_new(none, &raw mut self.mouse_event) })
    }
}

impl Drop for Handles {
    fn drop(&mut self) {
        // SAFETY: Each handle is either null, which the free functions accept,
        // or owned by this struct and not used again.
        unsafe { ffi::ghostty_mouse_event_free(self.mouse_event) };
        // SAFETY: As above.
        unsafe { ffi::ghostty_mouse_encoder_free(self.mouse_encoder) };
        // SAFETY: As above.
        unsafe { ffi::ghostty_key_event_free(self.key_event) };
        // SAFETY: As above.
        unsafe { ffi::ghostty_key_encoder_free(self.key_encoder) };
        // SAFETY: As above.
        unsafe { ffi::ghostty_render_state_row_cells_free(self.cells) };
        // SAFETY: As above.
        unsafe { ffi::ghostty_render_state_row_iterator_free(self.rows) };
        // SAFETY: As above.
        unsafe { ffi::ghostty_render_state_free(self.render_state) };
        // SAFETY: As above.
        unsafe { ffi::ghostty_terminal_free(self.terminal) };
    }
}

/// Where compressing the scrollback of a terminal has got to.
#[derive(Default)]
struct Compression {
    /// The library's token for what there is to compress.
    activity: u64,
    finished: bool,
}

pub struct Terminal {
    handles: Handles,
    /// Owned, and freed on drop. A raw pointer rather than a `Box` because
    /// libghostty-vt holds on to it and hands it to the callbacks.
    effects: *mut Effects,
    /// Reused between rows to avoid allocating for each one.
    row: Vec<Cell>,
    compression: Compression,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // The terminal must go first: it holds the pointer freed below.
        self.handles = Handles::null();
        // SAFETY: The pointer came from `Box::into_raw` in `new`, and with the
        // terminal gone nothing else refers to it.
        drop(unsafe { Box::from_raw(self.effects) });
    }
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Result<Self, Error> {
        let mut handles = Handles::null();
        handles.allocate(cols, rows)?;
        let effects = Box::into_raw(Box::new(Effects {
            replies: Vec::new(),
            bell: false,
            title_changed: false,
            clipboard: Vec::new(),
            render_state: handles.render_state,
            hold_started: None,
            size: (cols, rows, 1, 1),
            dark: true,
        }));
        let mut terminal = Self {
            handles,
            effects,
            row: Vec::new(),
            compression: Compression::default(),
        };
        terminal.register_effects()?;
        let scrollback = SCROLLBACK_BYTES;
        terminal.set(
            ffi::OPT_SCROLLBACK_MAX_BYTES,
            (&raw const scrollback).cast(),
        )?;
        Ok(terminal)
    }

    fn register_effects(&mut self) -> Result<(), Error> {
        self.set(ffi::OPT_USERDATA, self.effects.cast())?;
        self.set(ffi::OPT_WRITE_PTY, on_write_pty as *const c_void)?;
        self.set(ffi::OPT_BELL, on_bell as *const c_void)?;
        self.set(ffi::OPT_TITLE_CHANGED, on_title_changed as *const c_void)?;
        self.set(ffi::OPT_XTVERSION, on_xtversion as *const c_void)?;
        self.set(ffi::OPT_SIZE, on_size as *const c_void)?;
        self.set(ffi::OPT_COLOR_SCHEME, on_color_scheme as *const c_void)?;
        self.set(
            ffi::OPT_CLIPBOARD_WRITE,
            on_clipboard_write as *const c_void,
        )?;
        self.set(ffi::OPT_RENDER_HOLD, on_render_hold as *const c_void)
    }

    fn set(&mut self, option: c_int, value: *const c_void) -> Result<(), Error> {
        let handles = &mut self.handles;
        // SAFETY: The terminal is live, and each caller passes the value type
        // its option documents, which the library copies or keeps as registered.
        check(unsafe { ffi::ghostty_terminal_set(handles.terminal, option, value) })
    }

    /// Fills in a value of the type that `data` documents.
    fn read<T>(&self, data: c_int, value: &mut T) -> bool {
        let out = ptr::from_mut(value).cast();
        // SAFETY: The terminal is live, and `out` is valid for the type `data`
        // documents, as each caller picks `T` to match.
        unsafe { ffi::ghostty_terminal_get(self.handles.terminal, data, out) == ffi::SUCCESS }
    }

    fn get<T: Default>(&self, data: c_int) -> Option<T> {
        let mut value = T::default();
        self.read(data, &mut value).then_some(value)
    }

    /// What programs asked for since the caller last took it.
    pub const fn effects(&mut self) -> &mut Effects {
        // SAFETY: Owned by `self`. Callbacks only run during `write`, which
        // takes `&mut self`, so they cannot overlap with this borrow.
        unsafe { &mut *self.effects }
    }

    /// Feeds output of the program into the terminal.
    pub fn write(&mut self, data: &[u8]) {
        // SAFETY: `data` is valid for reads of its length during the call.
        unsafe { ffi::ghostty_terminal_vt_write(self.handles.terminal, data.as_ptr(), data.len()) };
    }

    /// Compresses a little of the scrollback, if it did not change since the
    /// last call. Meant to be called now and then. History stays as it was, and
    /// compressed pages come back by themselves as they are needed.
    pub fn compress_idle_scrollback(&mut self) {
        let mut activity = 0;
        let terminal = self.handles.terminal;
        // SAFETY: The terminal is live, and the out pointer is valid.
        let result =
            unsafe { ffi::ghostty_terminal_compression_activity(terminal, &raw mut activity) };
        if result != ffi::SUCCESS {
            return;
        }
        let state = &mut self.compression;
        if activity != state.activity {
            *state = Compression {
                activity,
                finished: false,
            };
            return;
        }
        if state.finished {
            return;
        }
        let started = Instant::now();
        loop {
            let mut outcome = 0;
            let mode = ffi::COMPRESSION_MODE_INCREMENTAL;
            // SAFETY: The terminal is live, and the out pointer is valid.
            let result =
                unsafe { ffi::ghostty_terminal_compress(terminal, mode, &raw mut outcome) };
            if result != ffi::SUCCESS || outcome != ffi::COMPRESSION_RESULT_PENDING {
                state.finished = true;
                return;
            }
            if started.elapsed() >= COMPRESSION_BUDGET {
                return;
            }
        }
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<(), Error> {
        self.effects().size = (cols, rows, cell_width, cell_height);
        let terminal = self.handles.terminal;
        // SAFETY: The terminal is live.
        check(unsafe {
            ffi::ghostty_terminal_resize(terminal, cols, rows, cell_width, cell_height)
        })
    }

    /// Sets the colors reported to programs that ask for them.
    pub fn set_default_colors(&mut self, foreground: Option<Rgb>, background: Option<Rgb>) {
        if let Some(rgb) = foreground {
            best_effort(self.set(ffi::OPT_COLOR_FOREGROUND, (&raw const rgb).cast()));
        }
        if let Some(rgb) = background {
            best_effort(self.set(ffi::OPT_COLOR_BACKGROUND, (&raw const rgb).cast()));
            let brightness = u16::from(rgb.r) + u16::from(rgb.g) + u16::from(rgb.b);
            self.effects().dark = brightness < 3 * 128;
        }
    }

    fn string(&self, data: c_int) -> String {
        let mut value = ffi::Bytes {
            ptr: ptr::null(),
            len: 0,
        };
        if !self.read(data, &mut value) {
            return String::new();
        }
        // SAFETY: The borrowed string stays valid until the next mutating call,
        // and it is copied out before returning.
        let bytes = unsafe { bytes_of(value.ptr, value.len) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    pub fn title(&self) -> String {
        self.string(ffi::DATA_TITLE)
    }

    /// The working directory the shell reported with OSC 7, if any.
    pub fn pwd(&self) -> String {
        self.string(ffi::DATA_PWD)
    }

    pub fn mode(&self, mode: u16) -> bool {
        let mut config = ffi::ModeConfig { mode, value: false };
        self.read(ffi::DATA_MODE, &mut config) && config.value
    }

    pub fn alternate_screen(&self) -> bool {
        self.get::<c_int>(ffi::DATA_ACTIVE_SCREEN) == Some(ffi::SCREEN_ALTERNATE)
    }

    /// Whether the program in this terminal wants mouse events.
    pub fn mouse_tracking(&self) -> bool {
        self.get::<bool>(ffi::DATA_MOUSE_TRACKING).unwrap_or(false)
    }

    pub fn scrollbar(&self) -> Option<Scrollbar> {
        self.get(ffi::DATA_SCROLLBAR)
    }

    fn scroll(&mut self, tag: c_int, delta: i32) {
        let behavior = ffi::ScrollViewport {
            tag,
            value: [i64::from(delta).cast_unsigned(), 0],
        };
        let handles = &mut self.handles;
        // SAFETY: The terminal is live, and the behavior is passed by value.
        let result = unsafe { ffi::ghostty_terminal_scroll_viewport(handles.terminal, behavior) };
        best_effort(check(result));
    }

    /// Moves the viewport into the scrollback, up for negative values.
    pub fn scroll_by(&mut self, delta: i32) {
        self.scroll(ffi::SCROLL_DELTA, delta);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll(ffi::SCROLL_TOP, 0);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll(ffi::SCROLL_BOTTOM, 0);
    }

    /// Brings the render state up to date, unless the program is holding the
    /// screen. Gives up on a hold that lasts too long, since the terminal has no
    /// clock and would otherwise keep a crashed program's pane frozen forever.
    pub fn refresh(&mut self) {
        let held = self.effects().hold_started;
        if held.is_some_and(|started| started.elapsed() >= RENDER_HOLD_LIMIT) {
            let config = ffi::ModeConfig {
                mode: MODE_SYNCHRONIZED_OUTPUT,
                value: false,
            };
            best_effort(self.set(ffi::OPT_MODE, (&raw const config).cast()));
            self.effects().hold_started = None;
        }
        if self.effects().hold_started.is_none() {
            let (state, terminal) = (self.handles.render_state, self.handles.terminal);
            // SAFETY: Both handles are live.
            best_effort(check(unsafe {
                ffi::ghostty_render_state_update(state, terminal)
            }));
        }
    }

    /// When an active hold expires, if there is one.
    pub fn hold_deadline(&self) -> Option<Instant> {
        // SAFETY: Owned by `self`, and callbacks cannot run during this read.
        let started = unsafe { (*self.effects).hold_started };
        started.map(|started| started + RENDER_HOLD_LIMIT)
    }

    /// Whether every row needs drawing, whatever each row says about itself.
    fn fully_dirty(&self) -> bool {
        let mut dirty = ffi::RENDER_DIRTY_FULL;
        let out = (&raw mut dirty).cast();
        let state = self.handles.render_state;
        // SAFETY: The render state is live, and `out` is valid for an enum.
        let result = unsafe { ffi::ghostty_render_state_get(state, ffi::RENDER_DATA_DIRTY, out) };
        result != ffi::SUCCESS || dirty == ffi::RENDER_DIRTY_FULL
    }

    /// The cursor as of the last [`Terminal::refresh`].
    pub fn cursor(&self) -> Option<Cursor> {
        let mut cursor = ffi::RenderCursor {
            size: size_of::<ffi::RenderCursor>(),
            viewport_has_value: false,
            viewport_x: 0,
            viewport_y: 0,
            wide_tail: false,
            visible: false,
            blinking: false,
            password_input: false,
            visual_style: 0,
        };
        let out = (&raw mut cursor).cast();
        let state = self.handles.render_state;
        // SAFETY: The render state is live, and `out` is a sized cursor struct.
        let result = unsafe { ffi::ghostty_render_state_get(state, ffi::RENDER_DATA_CURSOR, out) };
        (result == ffi::SUCCESS).then_some(Cursor {
            position: cursor
                .viewport_has_value
                .then_some((cursor.viewport_x, cursor.viewport_y)),
            visible: cursor.visible,
            blinking: cursor.blinking,
            shape: match cursor.visual_style {
                0 => CursorShape::Bar,
                2 => CursorShape::Underline,
                _ => CursorShape::Block,
            },
        })
    }

    /// Calls `draw` with each changed row of the viewport, or with every row if
    /// `all` is set, then marks the render state as drawn.
    pub fn draw_rows(&mut self, all: bool, mut draw: impl FnMut(u16, &[Cell])) {
        let mut rows = self.handles.rows;
        let out = (&raw mut rows).cast();
        let state = self.handles.render_state;
        // SAFETY: The render state and iterator are live. The call points the
        // iterator at this state's rows, and `out` is valid for a handle.
        let result =
            unsafe { ffi::ghostty_render_state_get(state, ffi::RENDER_DATA_ROW_ITERATOR, out) };
        if result != ffi::SUCCESS {
            return;
        }
        let all = all || self.fully_dirty();
        let mut y = 0;
        // SAFETY: The iterator was populated above and the state is unchanged.
        while unsafe { ffi::ghostty_render_state_row_iterator_next(rows) } {
            if all || self.row_dirty() {
                let mut row = std::mem::take(&mut self.row);
                self.read_row(&mut row);
                draw(y, &row);
                self.row = row;
            }
            y += 1;
        }
        // SAFETY: The render state is live.
        best_effort(check(unsafe { ffi::ghostty_render_state_clean(state) }));
    }

    fn row_dirty(&self) -> bool {
        let mut dirty = true;
        let out = (&raw mut dirty).cast();
        let rows = self.handles.rows;
        // SAFETY: The iterator points at a row, and `out` is valid for a bool.
        let result = unsafe { ffi::ghostty_render_state_row_get(rows, ffi::ROW_DATA_DIRTY, out) };
        result != ffi::SUCCESS || dirty
    }

    fn row_selection(&self) -> Option<(u16, u16)> {
        let mut selection = ffi::RowSelection {
            size: size_of::<ffi::RowSelection>(),
            start_x: 0,
            end_x: 0,
        };
        let out = (&raw mut selection).cast();
        let rows = self.handles.rows;
        // SAFETY: The iterator points at a row, and `out` is a sized struct.
        let result =
            unsafe { ffi::ghostty_render_state_row_get(rows, ffi::ROW_DATA_SELECTION, out) };
        (result == ffi::SUCCESS).then_some((selection.start_x, selection.end_x))
    }

    fn read_row(&self, row: &mut Vec<Cell>) {
        row.clear();
        let mut cells = self.handles.cells;
        let out = (&raw mut cells).cast();
        let rows = self.handles.rows;
        // SAFETY: The iterator points at a row, and `out` is valid for a handle.
        let result = unsafe { ffi::ghostty_render_state_row_get(rows, ffi::ROW_DATA_CELLS, out) };
        if result != ffi::SUCCESS {
            return;
        }
        let selection = self.row_selection();
        let mut x: u16 = 0;
        // SAFETY: The cells were populated above and the state is unchanged.
        while unsafe { ffi::ghostty_render_state_row_cells_next(cells) } {
            let mut cell = read_cell(cells);
            cell.selected = selection.is_some_and(|(start, end)| (start..=end).contains(&x));
            row.push(cell);
            x = x.saturating_add(1);
        }
    }

    /// Bytes for the program in this terminal, in the encoding its modes ask
    /// for. Empty if the event means nothing to it.
    pub fn encode_key(&mut self, input: &KeyInput<'_>) -> Vec<u8> {
        let handles = &mut self.handles;
        let (event, encoder) = (handles.key_event, handles.key_encoder);
        if fill_key_event(event, input).is_err() {
            return Vec::new();
        }
        // SAFETY: Both handles are live.
        let synced =
            unsafe { ffi::ghostty_key_encoder_set_from_terminal(encoder, handles.terminal) };
        if synced != ffi::SUCCESS {
            return Vec::new();
        }
        encode_into(|buf, len, written| {
            // SAFETY: `buf` is valid for `len` bytes, and `written` for a write.
            unsafe { ffi::ghostty_key_encoder_encode(encoder, event, buf, len, written) }
        })
    }

    /// Bytes for the program in this terminal. Empty if the event means nothing
    /// to it, which depends on the mouse modes it enabled.
    pub fn encode_mouse(&mut self, input: &MouseInput, cols: u16, rows: u16) -> Vec<u8> {
        let handles = &mut self.handles;
        let (encoder, event) = (handles.mouse_encoder, handles.mouse_event);
        // Reporting cell positions with one pixel per cell keeps every format
        // but SGR-pixels exact, without needing the outer terminal's cell size.
        let size = ffi::MouseEncoderSize {
            size: size_of::<ffi::MouseEncoderSize>(),
            screen_width: u32::from(cols),
            screen_height: u32::from(rows),
            cell_width: 1,
            cell_height: 1,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        let configured = configure_mouse_encoder(encoder, handles.terminal, &size, input);
        if configured
            .and_then(|()| fill_mouse_event(event, input))
            .is_err()
        {
            return Vec::new();
        }
        encode_into(|buf, len, written| {
            // SAFETY: `buf` is valid for `len` bytes, and `written` for a write.
            unsafe { ffi::ghostty_mouse_encoder_encode(encoder, event, buf, len, written) }
        })
    }

    /// Bytes reporting a focus change, if this terminal's program wants them.
    pub fn encode_focus(&self, focused: bool) -> Vec<u8> {
        if !self.mode(MODE_FOCUS_EVENT) {
            return Vec::new();
        }
        let event = c_int::from(!focused);
        encode_into(|buf, len, written| {
            // SAFETY: `buf` is valid for `len` bytes, and `written` for a write.
            unsafe { ffi::ghostty_focus_encode(event, buf, len, written) }
        })
    }

    /// Bytes for pasting `text`, with control characters made harmless.
    pub fn encode_paste(&self, text: &[u8]) -> Vec<u8> {
        let bracketed = self.mode(MODE_BRACKETED_PASTE);
        let mut data = text.to_vec();
        let (ptr, len) = (data.as_mut_ptr().cast(), data.len());
        encode_into(|buf, buf_len, written| {
            // SAFETY: `ptr` is valid for `len` bytes, which the call may rewrite
            // in place. `buf` is valid for `buf_len`, and `written` for a write.
            unsafe { ffi::ghostty_paste_encode(ptr, len, bracketed, buf, buf_len, written) }
        })
    }

    fn grid_ref(&self, x: u16, y: u16) -> Option<ffi::GridRef> {
        let point = ffi::Point {
            tag: ffi::POINT_VIEWPORT,
            value: ffi::PointValue {
                coordinate: ffi::PointCoordinate { x, y: u32::from(y) },
                padding: 0,
            },
        };
        let mut grid_ref = NO_REF;
        let terminal = self.handles.terminal;
        // SAFETY: The terminal is live, and the out pointer is valid.
        let result = unsafe { ffi::ghostty_terminal_grid_ref(terminal, point, &raw mut grid_ref) };
        (result == ffi::SUCCESS).then_some(grid_ref)
    }

    fn set_selection(&mut self, selection: &ffi::Selection) {
        best_effort(self.set(ffi::OPT_SELECTION, ptr::from_ref(selection).cast()));
    }

    fn selection(&self) -> Option<ffi::Selection> {
        let mut selection = selection_at(NO_REF);
        self.read(ffi::DATA_SELECTION, &mut selection)
            .then_some(selection)
    }

    /// Starts a selection at a viewport position.
    pub fn select_start(&mut self, x: u16, y: u16) {
        if let Some(grid_ref) = self.grid_ref(x, y) {
            self.set_selection(&selection_at(grid_ref));
        }
    }

    /// Moves the end of the current selection to a viewport position. The
    /// terminal keeps the start in place as the viewport scrolls.
    pub fn select_extend(&mut self, x: u16, y: u16) {
        let end = self.grid_ref(x, y);
        if let (Some(selection), Some(end)) = (self.selection(), end) {
            self.set_selection(&ffi::Selection { end, ..selection });
        }
    }

    /// Selects what `find` comes up with for a viewport position.
    fn select_around(
        &mut self,
        x: u16,
        y: u16,
        find: impl FnOnce(ffi::GridRef, *mut ffi::Selection) -> c_int,
    ) {
        let Some(grid_ref) = self.grid_ref(x, y) else {
            return;
        };
        let mut selection = selection_at(grid_ref);
        if find(grid_ref, &raw mut selection) == ffi::SUCCESS {
            self.set_selection(&selection);
        }
    }

    /// Selects the word at a viewport position.
    pub fn select_word(&mut self, x: u16, y: u16) {
        let terminal = self.handles.terminal;
        self.select_around(x, y, |grid_ref, selection| {
            let options = ffi::SelectWordOptions {
                size: size_of::<ffi::SelectWordOptions>(),
                grid_ref,
                boundary_codepoints: ptr::null(),
                boundary_codepoints_len: 0,
            };
            // SAFETY: The terminal is live, and both pointers are valid.
            unsafe { ffi::ghostty_terminal_select_word(terminal, &raw const options, selection) }
        });
    }

    /// Selects the line at a viewport position.
    pub fn select_line(&mut self, x: u16, y: u16) {
        let terminal = self.handles.terminal;
        self.select_around(x, y, |grid_ref, selection| {
            let options = ffi::SelectLineOptions {
                size: size_of::<ffi::SelectLineOptions>(),
                grid_ref,
                whitespace: ptr::null(),
                whitespace_len: 0,
                semantic_prompt_boundary: false,
            };
            // SAFETY: The terminal is live, and both pointers are valid.
            unsafe { ffi::ghostty_terminal_select_line(terminal, &raw const options, selection) }
        });
    }

    pub fn clear_selection(&mut self) {
        best_effort(self.set(ffi::OPT_SELECTION, ptr::null()));
    }

    /// The selected text, with soft-wrapped lines joined back together.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection()?;
        let mut formatter: ffi::Handle = ptr::null_mut();
        let terminal = self.handles.terminal;
        let none = ptr::null();
        // SAFETY: A null allocator selects the default, and the out pointer is valid.
        let result =
            unsafe { ffi::ghostty_formatter_terminal_new(none, &raw mut formatter, terminal) };
        check(result).ok()?;
        let text = format_selection(formatter, &selection);
        // SAFETY: Created above and not used again.
        unsafe { ffi::ghostty_formatter_free(formatter) };
        text
    }
}

const NO_REF: ffi::GridRef = ffi::GridRef {
    node: ptr::null_mut(),
    x: 0,
    y: 0,
};

const fn selection_at(grid_ref: ffi::GridRef) -> ffi::Selection {
    ffi::Selection {
        start: grid_ref,
        end: grid_ref,
        rectangle: false,
    }
}

fn fill_key_event(event: ffi::Handle, input: &KeyInput<'_>) -> Result<(), Error> {
    let utf8 = ffi::Bytes {
        ptr: input.text.as_ptr(),
        len: input.text.len(),
    };
    // SAFETY: The event is live, and this stores a plain value.
    check(unsafe { ffi::ghostty_key_event_set_action(event, input.action as c_int) })?;
    // SAFETY: As above.
    check(unsafe { ffi::ghostty_key_event_set_key(event, input.key) })?;
    // SAFETY: As above.
    check(unsafe { ffi::ghostty_key_event_set_mods(event, input.mods) })?;
    // SAFETY: As above.
    check(unsafe { ffi::ghostty_key_event_set_consumed_mods(event, input.consumed_mods) })?;
    // SAFETY: The event borrows the text, which outlives the encode in
    // `encode_key` because it is part of `input`.
    check(unsafe { ffi::ghostty_key_event_set_utf8(event, utf8) })?;
    // SAFETY: As for the plain values above.
    check(unsafe { ffi::ghostty_key_event_set_unshifted_codepoint(event, input.unshifted) })
}

fn configure_mouse_encoder(
    encoder: ffi::Handle,
    terminal: ffi::Handle,
    size: &ffi::MouseEncoderSize,
    input: &MouseInput,
) -> Result<(), Error> {
    // SAFETY: Both handles are live.
    check(unsafe { ffi::ghostty_mouse_encoder_set_from_terminal(encoder, terminal) })?;
    let size = ptr::from_ref(size).cast();
    // SAFETY: The encoder is live, and copies the value of the option's type.
    check(unsafe { ffi::ghostty_mouse_encoder_set(encoder, ffi::MOUSE_OPT_SIZE, size) })?;
    let pressed = (&raw const input.any_pressed).cast();
    let option = ffi::MOUSE_OPT_ANY_BUTTON_PRESSED;
    // SAFETY: As above.
    check(unsafe { ffi::ghostty_mouse_encoder_set(encoder, option, pressed) })
}

fn fill_mouse_event(event: ffi::Handle, input: &MouseInput) -> Result<(), Error> {
    let position = ffi::MousePosition {
        x: f32::from(input.x),
        y: f32::from(input.y),
    };
    // SAFETY: The event is live, and this stores a plain value.
    check(unsafe { ffi::ghostty_mouse_event_set_action(event, input.action as c_int) })?;
    check(input.button.map_or_else(
        // SAFETY: As above.
        || unsafe { ffi::ghostty_mouse_event_clear_button(event) },
        // SAFETY: As above.
        |button| unsafe { ffi::ghostty_mouse_event_set_button(event, button as c_int) },
    ))?;
    // SAFETY: As above.
    check(unsafe { ffi::ghostty_mouse_event_set_mods(event, input.mods) })?;
    // SAFETY: As above.
    check(unsafe { ffi::ghostty_mouse_event_set_position(event, position) })
}

fn format_selection(formatter: ffi::Handle, selection: &ffi::Selection) -> Option<String> {
    let unwrap = true;
    let unwrap_ptr = (&raw const unwrap).cast();
    // SAFETY: The formatter is live, and the value matches the option's type.
    let result =
        unsafe { ffi::ghostty_formatter_set(formatter, ffi::FORMATTER_OPT_UNWRAP, unwrap_ptr) };
    check(result).ok()?;
    let selection_ptr = ptr::from_ref(selection).cast();
    // SAFETY: As above.
    let result = unsafe {
        ffi::ghostty_formatter_set(formatter, ffi::FORMATTER_OPT_SELECTION, selection_ptr)
    };
    check(result).ok()?;

    let mut text: *mut u8 = ptr::null_mut();
    let mut len = 0;
    let none = ptr::null();
    // SAFETY: The formatter is live, and the out pointers are valid.
    let result = unsafe {
        ffi::ghostty_formatter_format_alloc(formatter, none, &raw mut text, &raw mut len)
    };
    check(result).ok()?;
    // SAFETY: On success the library returns an allocation of `len` bytes.
    let bytes = unsafe { bytes_of(text, len) };
    let copy = String::from_utf8_lossy(bytes).into_owned();
    // SAFETY: Allocated by the call above with the default allocator.
    unsafe { ffi::ghostty_free(none, text, len) };
    Some(copy)
}

fn read_cell(cells: ffi::Handle) -> Cell {
    let mut raw: u64 = 0;
    let out = (&raw mut raw).cast();
    // SAFETY: The iterator points at a cell, and `out` is valid for its raw value.
    let result =
        unsafe { ffi::ghostty_render_state_row_cells_get(cells, ffi::CELLS_DATA_RAW, out) };
    if result != ffi::SUCCESS {
        return Cell::default();
    }
    let mut style = read_style(cells);
    apply_cell_background(raw, &mut style);
    Cell {
        text: read_text(cells),
        style,
        width: match cell_value::<c_int>(raw, ffi::CELL_DATA_WIDE) {
            ffi::CELL_WIDE_WIDE => Width::Wide,
            ffi::CELL_WIDE_SPACER_TAIL => Width::Tail,
            _ => Width::Narrow,
        },
        selected: false,
    }
}

fn cell_value<T: Default>(raw: u64, data: c_int) -> T {
    let mut value = T::default();
    let out = (&raw mut value).cast();
    // SAFETY: `out` is valid for the type `data` documents, as callers pick `T`
    // to match. On failure the default stays in place.
    best_effort(check(unsafe { ffi::ghostty_cell_get(raw, data, out) }));
    value
}

fn read_style(cells: ffi::Handle) -> Style {
    let mut styled = false;
    let out = (&raw mut styled).cast();
    // SAFETY: The iterator points at a cell, and `out` is valid for a bool.
    let result =
        unsafe { ffi::ghostty_render_state_row_cells_get(cells, ffi::CELLS_DATA_HAS_STYLING, out) };
    let mut style = Style::default();
    if result == ffi::SUCCESS && styled {
        let out = (&raw mut style).cast();
        // SAFETY: The iterator points at a cell, and `out` is valid for a style.
        let result =
            unsafe { ffi::ghostty_render_state_row_cells_get(cells, ffi::CELLS_DATA_STYLE, out) };
        best_effort(check(result));
    }
    style
}

/// Erasing with a background color stores that color in the cell itself, not in
/// a style, so blank colored cells would otherwise lose it.
fn apply_cell_background(raw: u64, style: &mut Style) {
    match cell_value::<c_int>(raw, ffi::CELL_DATA_CONTENT_TAG) {
        ffi::CELL_CONTENT_BG_PALETTE => {
            let index = cell_value::<u8>(raw, ffi::CELL_DATA_COLOR_PALETTE);
            style.bg = StyleColor::palette(index);
        }
        ffi::CELL_CONTENT_BG_RGB => {
            let rgb = cell_value::<Rgb>(raw, ffi::CELL_DATA_COLOR_RGB);
            style.bg = StyleColor::rgb(rgb);
        }
        _ => {}
    }
}

fn read_text(cells: ffi::Handle) -> Text {
    let mut inline = [0_u8; 64];
    let mut buffer = ffi::Buffer {
        ptr: inline.as_mut_ptr(),
        cap: inline.len(),
        len: 0,
    };
    let data = ffi::CELLS_DATA_GRAPHEMES_UTF8;
    let out = (&raw mut buffer).cast();
    // SAFETY: The iterator points at a cell, and the buffer is valid for `cap`.
    let result = unsafe { ffi::ghostty_render_state_row_cells_get(cells, data, out) };
    if result == ffi::SUCCESS {
        let text = inline.get(..buffer.len).unwrap_or(&[]);
        return std::str::from_utf8(text).map_or(Text::Blank, Text::new);
    }
    if result != ffi::OUT_OF_SPACE {
        return Text::Blank;
    }
    // A long grapheme cluster. `len` now says how much room it needs.
    let mut heap = vec![0_u8; buffer.len];
    buffer = ffi::Buffer {
        ptr: heap.as_mut_ptr(),
        cap: heap.len(),
        len: 0,
    };
    let out = (&raw mut buffer).cast();
    // SAFETY: As above, with a buffer of the size the library asked for.
    let result = unsafe { ffi::ghostty_render_state_row_cells_get(cells, data, out) };
    if result != ffi::SUCCESS {
        return Text::Blank;
    }
    heap.truncate(buffer.len);
    String::from_utf8(heap).map_or(Text::Blank, |text| Text::Heap(text.into()))
}

/// Runs an encoder that writes into a caller buffer, growing it if it asks.
fn encode_into(mut encode: impl FnMut(*mut u8, usize, *mut usize) -> c_int) -> Vec<u8> {
    let mut buf = vec![0; 128];
    let mut written = 0;
    let mut result = encode(buf.as_mut_ptr(), buf.len(), &raw mut written);
    if result == ffi::OUT_OF_SPACE {
        buf.resize(written, 0);
        result = encode(buf.as_mut_ptr(), buf.len(), &raw mut written);
    }
    if result != ffi::SUCCESS {
        return Vec::new();
    }
    buf.truncate(written);
    buf
}

impl StyleColor {
    pub const fn palette(index: u8) -> Self {
        Self {
            tag: ffi::STYLE_COLOR_PALETTE,
            value: [index, 0, 0],
        }
    }

    pub const fn rgb(rgb: Rgb) -> Self {
        Self {
            tag: ffi::STYLE_COLOR_RGB,
            value: [rgb.r, rgb.g, rgb.b],
        }
    }

    /// The palette index of an indexed color.
    pub const fn as_palette(self) -> Option<u8> {
        if self.tag == ffi::STYLE_COLOR_PALETTE {
            Some(self.value[0])
        } else {
            None
        }
    }

    /// The components of a direct color.
    pub const fn as_rgb(self) -> Option<[u8; 3]> {
        if self.tag == ffi::STYLE_COLOR_RGB {
            Some(self.value)
        } else {
            None
        }
    }
}

/// How many cells a character takes up.
pub fn char_width(c: char) -> u16 {
    // SAFETY: A pure function of its argument.
    u16::from(unsafe { ffi::ghostty_unicode_codepoint_width(u32::from(c)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(terminal: &mut Terminal) -> Vec<String> {
        terminal.refresh();
        let mut lines = Vec::new();
        terminal.draw_rows(true, |_, cells| {
            let text: String = cells
                .iter()
                .filter(|cell| cell.width != Width::Tail)
                .map(|cell| cell.text.as_str())
                .collect();
            lines.push(text.trim_end().to_owned());
        });
        lines
    }

    /// The library's description of its types, with whitespace removed.
    fn manifest() -> String {
        // SAFETY: Returns a static NUL-terminated string.
        let manifest = unsafe { ffi::ghostty_type_json() };
        // SAFETY: The pointer is valid and static, as stated above.
        let manifest = unsafe { std::ffi::CStr::from_ptr(manifest) }.to_string_lossy();
        manifest.split_whitespace().collect()
    }

    /// A type's entry in the library's manifest.
    fn manifest_entry<'a>(manifest: &'a str, name: &str) -> &'a str {
        let key = format!("\"{name}\":{{\"kind\":");
        let (_, entry) = manifest.split_once(&key).expect(name);
        // It ends where the next type's entry starts.
        entry
            .split_once("\"kind\":")
            .map_or(entry, |(entry, _)| entry)
    }

    fn number_after(text: &str, key: &str) -> usize {
        let (_, rest) = text.split_once(key).expect(key);
        rest.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap()
    }

    /// Checks a struct's size and the offset of each listed field. Size alone
    /// is not enough: padding can make a wrong layout add up to the right total.
    macro_rules! assert_layout {
        ($manifest:expr, $c_name:literal, $rust:ty, $($c_field:literal => $field:ident),+ $(,)?) => {
            let entry = manifest_entry($manifest, $c_name);
            assert_eq!(number_after(entry, "\"size\":"), size_of::<$rust>(), "size of {}", $c_name);
            $(
                let key = concat!("\"", $c_field, "\":{\"offset\":");
                assert_eq!(
                    number_after(entry, key),
                    std::mem::offset_of!($rust, $field),
                    "offset of {}.{}", $c_name, $c_field,
                );
            )+
        };
    }

    #[test]
    fn struct_layouts_match_the_library() {
        let manifest = manifest();
        let m = manifest.as_str();

        assert_layout!(m, "GhosttyString", ffi::Bytes, "ptr" => ptr, "len" => len);
        assert_layout!(m, "GhosttyBuffer", ffi::Buffer, "ptr" => ptr, "cap" => cap, "len" => len);
        assert_layout!(m, "GhosttyColorRgb", Rgb, "r" => r, "g" => g, "b" => b);
        assert_layout!(m, "GhosttyStyleColor", StyleColor, "tag" => tag, "value" => value);
        assert_layout!(m, "GhosttyStyle", Style,
            "fg_color" => fg, "bg_color" => bg, "underline_color" => underline_color,
            "flags" => flags, "reserved" => reserved);
        assert_layout!(m, "GhosttyTerminalScrollbar", ffi::Scrollbar,
            "total" => total, "offset" => offset, "len" => len);
        assert_layout!(m, "GhosttyTerminalScrollViewport", ffi::ScrollViewport,
            "tag" => tag, "value" => value);
        assert_layout!(m, "GhosttyPoint", ffi::Point, "tag" => tag, "value" => value);
        assert_eq!(
            number_after(manifest_entry(m, "GhosttyPointValue"), "\"size\":"),
            size_of::<ffi::PointValue>(),
        );
        assert_layout!(m, "GhosttyPointCoordinate", ffi::PointCoordinate, "x" => x, "y" => y);
    }

    #[test]
    fn more_struct_layouts_match_the_library() {
        let manifest = manifest();
        let m = manifest.as_str();

        assert_layout!(m, "GhosttyGridRef", ffi::GridRef, "node" => node, "x" => x, "y" => y);
        assert_layout!(m, "GhosttySelection", ffi::Selection,
            "start" => start, "end" => end, "rectangle" => rectangle);
        assert_layout!(m, "GhosttyTerminalSelectWordOptions", ffi::SelectWordOptions,
            "size" => size, "ref" => grid_ref, "boundary_codepoints" => boundary_codepoints,
            "boundary_codepoints_len" => boundary_codepoints_len);
        assert_layout!(m, "GhosttyTerminalSelectLineOptions", ffi::SelectLineOptions,
            "size" => size, "ref" => grid_ref, "whitespace" => whitespace,
            "whitespace_len" => whitespace_len,
            "semantic_prompt_boundary" => semantic_prompt_boundary);
        assert_layout!(m, "GhosttyTerminalModeConfig", ffi::ModeConfig,
            "mode" => mode, "value" => value);
        assert_layout!(m, "GhosttyRenderStateRowSelection", ffi::RowSelection,
            "size" => size, "start_x" => start_x, "end_x" => end_x);
        assert_layout!(m, "GhosttyRenderStateCursor", ffi::RenderCursor,
            "size" => size, "viewport_has_value" => viewport_has_value,
            "viewport_x" => viewport_x, "viewport_y" => viewport_y, "wide_tail" => wide_tail,
            "visible" => visible, "blinking" => blinking, "password_input" => password_input,
            "visual_style" => visual_style);
        assert_layout!(m, "GhosttySizeReportSize", ffi::SizeReport,
            "rows" => rows, "columns" => columns,
            "cell_width" => cell_width, "cell_height" => cell_height);
        assert_layout!(m, "GhosttyMouseEncoderSize", ffi::MouseEncoderSize,
            "size" => size, "screen_width" => screen_width, "screen_height" => screen_height,
            "cell_width" => cell_width, "cell_height" => cell_height,
            "padding_top" => padding_top, "padding_bottom" => padding_bottom,
            "padding_right" => padding_right, "padding_left" => padding_left);
        assert_layout!(m, "GhosttyMousePosition", ffi::MousePosition, "x" => x, "y" => y);
        assert_layout!(m, "GhosttyTerminalMemoryUsage", ffi::MemoryUsage,
            "size" => size, "compression_supported" => compression_supported,
            "primary_pages" => primary_pages, "primary_virtual_bytes" => primary_virtual_bytes,
            "primary_resident_bytes" => primary_resident_bytes,
            "primary_compressed_pages" => primary_compressed_pages);
        assert_layout!(m, "GhosttyClipboardContent", ffi::ClipboardContent,
            "mime" => mime, "data" => data);
        assert_layout!(m, "GhosttyClipboardWrite", ffi::ClipboardWrite,
            "location" => location, "contents" => contents, "contents_len" => contents_len,
            "name" => name, "granted" => granted, "can_remember" => can_remember,
            "ctx" => ctx, "reply" => reply);
        assert_layout!(m, "GhosttyClipboardWriteReply", ffi::ClipboardWriteReply,
            "size" => size, "result" => result, "remember" => remember);
    }

    #[test]
    fn draws_written_text() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(b"hi\r\n\x1b[1mthere");
        assert_eq!(lines(&mut terminal), ["hi", "there"]);
    }

    #[test]
    fn reads_styles_and_wide_characters() {
        let mut terminal = Terminal::new(10, 1).unwrap();
        terminal.write("\x1b[1;31mA\x1b[0m\u{4e16}".as_bytes());
        terminal.refresh();
        let mut row = Vec::new();
        terminal.draw_rows(true, |_, cells| row = cells.to_vec());
        assert_eq!(row[0].style.flags & STYLE_BOLD, STYLE_BOLD);
        assert_eq!(row[0].style.fg.as_palette(), Some(1));
        assert_eq!(row[1].width, Width::Wide);
        assert_eq!(row[1].text.as_str(), "\u{4e16}");
        assert_eq!(row[2].width, Width::Tail);
    }

    #[test]
    fn keeps_erased_background_colors() {
        let mut terminal = Terminal::new(4, 1).unwrap();
        terminal.write(b"\x1b[41m\x1b[K");
        terminal.refresh();
        let mut row = Vec::new();
        terminal.draw_rows(true, |_, cells| row = cells.to_vec());
        assert_eq!(row[3].style.bg.as_palette(), Some(1));
    }

    #[test]
    fn keeps_erased_direct_background_colors() {
        let mut terminal = Terminal::new(4, 1).unwrap();
        terminal.write(b"\x1b[48;2;1;2;3m\x1b[K");
        terminal.refresh();
        let mut row = Vec::new();
        terminal.draw_rows(true, |_, cells| row = cells.to_vec());
        assert_eq!(row[3].style.bg.as_rgb(), Some([1, 2, 3]));
    }

    #[test]
    fn answers_queries_through_effects() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(b"\x1b[6n");
        assert_eq!(terminal.effects().replies, b"\x1b[1;1R");
    }

    #[test]
    fn keeps_a_useful_amount_of_scrollback() {
        let mut terminal = Terminal::new(20, 5).unwrap();
        for line in 0..5000 {
            terminal.write(format!("line {line}\r\n").as_bytes());
        }
        assert!(terminal.scrollbar().unwrap().history() > 4000);
    }

    #[test]
    fn compresses_scrollback_once_it_has_been_left_alone() {
        let mut terminal = Terminal::new(40, 5).unwrap();
        for line in 0..20_000 {
            terminal.write(format!("line {line}\r\n").as_bytes());
        }
        let usage = |terminal: &Terminal| {
            let mut usage = ffi::MemoryUsage {
                size: size_of::<ffi::MemoryUsage>(),
                compression_supported: false,
                primary_pages: 0,
                primary_virtual_bytes: 0,
                primary_resident_bytes: 0,
                primary_compressed_pages: 0,
                rest: [0; 8],
            };
            assert!(terminal.read(ffi::DATA_MEMORY_USAGE, &mut usage));
            usage
        };
        let before = usage(&terminal);
        if !before.compression_supported {
            return;
        }

        // The first call only sees that the scrollback changed.
        terminal.compress_idle_scrollback();
        assert_eq!(usage(&terminal).primary_compressed_pages, 0);

        while !terminal.compression.finished {
            terminal.compress_idle_scrollback();
        }
        let after = usage(&terminal);
        assert!(after.primary_compressed_pages > 0);
        assert!(after.primary_resident_bytes < before.primary_resident_bytes / 2);

        terminal.scroll_to_top();
        assert_eq!(lines(&mut terminal)[0], "line 0");
    }

    #[test]
    fn reports_titles_and_bells() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(b"\x1b]2;hello\x07\x07");
        assert!(terminal.effects().title_changed);
        assert!(terminal.effects().bell);
        assert_eq!(terminal.title(), "hello");
    }

    #[test]
    fn reports_clipboard_writes() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(terminal.effects().clipboard, [b"hello".to_vec()]);
    }

    #[test]
    fn selects_and_extracts_text() {
        let mut terminal = Terminal::new(20, 2).unwrap();
        terminal.write(b"hello world");
        terminal.select_start(0, 0);
        terminal.select_extend(4, 0);
        assert_eq!(terminal.selected_text().as_deref(), Some("hello"));
        terminal.select_word(7, 0);
        assert_eq!(terminal.selected_text().as_deref(), Some("world"));
        terminal.clear_selection();
        assert_eq!(terminal.selected_text(), None);
    }

    #[test]
    fn encodes_keys_for_the_current_modes() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        let up = KeyInput {
            action: KeyAction::Press,
            key: key::ARROW_UP,
            mods: 0,
            consumed_mods: 0,
            text: "",
            unshifted: 0,
        };
        assert_eq!(terminal.encode_key(&up), b"\x1b[A");
        terminal.write(b"\x1b[?1h");
        assert_eq!(terminal.encode_key(&up), b"\x1bOA");
    }

    #[test]
    fn encodes_mouse_events_only_when_tracked() {
        let mut terminal = Terminal::new(10, 5).unwrap();
        let click = MouseInput {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            mods: 0,
            x: 2,
            y: 3,
            any_pressed: true,
        };
        assert_eq!(terminal.encode_mouse(&click, 10, 5), b"");
        terminal.write(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(terminal.encode_mouse(&click, 10, 5), b"\x1b[<0;3;4M");
    }

    #[test]
    fn brackets_pastes_when_asked_to() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        assert_eq!(terminal.encode_paste(b"a\nb"), b"a\rb");
        terminal.write(b"\x1b[?2004h");
        assert_eq!(terminal.encode_paste(b"ab"), b"\x1b[200~ab\x1b[201~");
    }

    #[test]
    fn pastes_more_than_the_encoder_buffer_at_first_holds() {
        let terminal = Terminal::new(10, 2).unwrap();
        let text = vec![b'a'; 1000];
        assert_eq!(terminal.encode_paste(&text), text);
    }

    #[test]
    fn small_text_stays_inline() {
        assert!(matches!(Text::new("a"), Text::Inline { len: 1, .. }));
        assert_eq!(Text::new(""), Text::Blank);
        let long = "a".repeat(40);
        assert_eq!(Text::new(&long).as_str(), long);
    }
}
