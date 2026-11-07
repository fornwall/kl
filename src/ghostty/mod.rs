//! Safe wrapper around libghostty-vt.
//!
//! A [`Terminal`] is one complete terminal emulator. Bytes go in through
//! [`Terminal::write`], and everything a program asks of the terminal that it
//! cannot do by itself lands in [`Effects`] for the caller to act on.

mod ffi;
pub mod key;
mod links;

use std::collections::BTreeMap;
use std::ffi::{c_int, c_void};
use std::ptr;
use std::slice;
use std::time::{Duration, Instant};

use links::Links;

pub use ffi::{Rgb, Scrollbar, Style, StyleColor};
pub use links::Link;

/// How long a program may freeze its pane with synchronized output.
const RENDER_HOLD_LIMIT: Duration = Duration::from_secs(1);

/// The library's default is 10,000 bytes, about one page of history. This
/// matches the default of the Ghostty app.
const SCROLLBACK_BYTES: usize = 10_000_000;

/// How long one round of compressing may keep the server from everything else.
const COMPRESSION_BUDGET: Duration = Duration::from_millis(2);

/// How much of an unfinished escape sequence a snapshot can carry over. A
/// terminal in the middle of a longer one cannot be saved at that moment.
const CONTINUATION_BYTES: usize = 64 * 1024;

const VERSION_REPLY: &str = concat!("kl ", env!("CARGO_PKG_VERSION"));

/// How close together clicks must be to count as a double or triple click.
pub const MULTI_CLICK_TIME: Duration = Duration::from_millis(400);

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
pub const MODE_SGR_PIXELS: u16 = 1016;
pub const MODE_BRACKETED_PASTE: u16 = 2004;
const MODE_SYNCHRONIZED_OUTPUT: u16 = 2026;
const MODE_GRAPHEME_CLUSTER: u16 = 2027;
const MODE_REPORT_COLOR_SCHEME: u16 = 2031;

/// The names OSC 22 knows pointer shapes by, in the order of `GhosttyMouseShape`.
const POINTER_SHAPES: [&str; 34] = [
    "default",
    "context-menu",
    "help",
    "pointer",
    "progress",
    "wait",
    "cell",
    "crosshair",
    "text",
    "vertical-text",
    "alias",
    "copy",
    "move",
    "no-drop",
    "not-allowed",
    "grab",
    "grabbing",
    "all-scroll",
    "col-resize",
    "row-resize",
    "n-resize",
    "e-resize",
    "s-resize",
    "w-resize",
    "ne-resize",
    "nw-resize",
    "se-resize",
    "sw-resize",
    "ew-resize",
    "ns-resize",
    "nesw-resize",
    "nwse-resize",
    "zoom-in",
    "zoom-out",
];

/// The pointer shape of a terminal nobody asked anything of.
pub const DEFAULT_POINTER_SHAPE: &str = "text";

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
    pub link: Option<Link>,
}

/// One row of text, for moving around in it rather than for drawing it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    /// What each cell starts with: a space for an empty one, and a NUL for the
    /// right half of a double width character.
    pub cells: Vec<char>,
    /// The text goes on in the next row.
    pub wrapped: bool,
    /// A shell prompt starts in this row.
    pub prompt: bool,
}

impl Line {
    pub fn cell(&self, x: u16) -> char {
        self.cells.get(usize::from(x)).copied().unwrap_or(' ')
    }

    pub fn is_padding(&self, x: u16) -> bool {
        self.cell(x) == '\0'
    }

    /// Up to where there is text.
    pub fn length(&self) -> u16 {
        let text = self.cells.iter().rposition(|c| *c != ' ');
        text.map_or(0, |last| u16::try_from(last + 1).unwrap_or(u16::MAX))
    }

    /// The column of the last character.
    pub fn limit(&self) -> u16 {
        let mut x = self.length().saturating_sub(1);
        while x > 0 && self.is_padding(x) {
            x -= 1;
        }
        x
    }
}

/// A way the viewport can follow a pointer at its edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autoscroll {
    Up,
    Down,
}

/// A place a search found its text, as positions of the screen in reading order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Match {
    /// Column and row of the first and last cell, with the row counted from
    /// the top of the history.
    pub start: (u16, u32),
    pub end: (u16, u32),
}

impl Match {
    /// Whether a position of the screen is in this match.
    pub fn contains(&self, x: u16, y: u32) -> bool {
        let after_start = (y, x) >= (self.start.1, self.start.0);
        after_start && (y, x) <= (self.end.1, self.end.0)
    }
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

/// How far along a program says it is, from OSC 9;4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// The state as the sequence numbers it: 1 is plain progress, 2 an error,
    /// 3 progress of unknown extent and 4 paused.
    pub state: c_int,
    pub percent: Option<u8>,
}

/// A desktop notification, from OSC 9 or OSC 777.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// Empty when the program gave none, which OSC 9 has no room for.
    pub title: String,
    pub body: String,
}

/// A mouse event in a terminal.
#[derive(Clone, Copy, Debug)]
pub struct MouseInput {
    pub action: MouseAction,
    pub button: Option<MouseButton>,
    pub mods: u16,
    /// The position in pixels, of which the encoder makes cells for programs
    /// that want those.
    pub x: u16,
    pub y: u16,
    /// The size of a cell in pixels.
    pub cell_size: (u16, u16),
    pub any_pressed: bool,
}

/// What the outer terminal said about its colors.
#[derive(Clone, Debug, Default)]
pub struct OuterColors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    /// The entries of its palette it told about.
    pub palette: BTreeMap<u8, Rgb>,
    /// Whether it is in a dark color scheme, if it says.
    pub dark: Option<bool>,
}

impl OuterColors {
    /// The `GhosttyColorScheme` to tell programs. Going by the background is
    /// the next best thing for a terminal that does not say, and dark is what
    /// most people have.
    fn color_scheme(&self) -> c_int {
        let brightness = |rgb: Rgb| u16::from(rgb.r) + u16::from(rgb.g) + u16::from(rgb.b);
        let dark = self
            .dark
            .or_else(|| self.background.map(|rgb| brightness(rgb) < 3 * 128))
            .unwrap_or(true);
        if dark {
            ffi::COLOR_SCHEME_DARK
        } else {
            ffi::COLOR_SCHEME_LIGHT
        }
    }
}

/// Colors a program changed for itself. The outer terminal is shared with other
/// panes, so it does not hear of them: they go out as RGB, while all other colors
/// stay symbolic and follow the outer theme.
#[derive(Default, PartialEq, Eq)]
struct OwnColors {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    /// One for each palette entry, or empty if the program changed none.
    palette: Vec<Option<Rgb>>,
}

impl OwnColors {
    fn resolve(&self, color: StyleColor, default: Option<Rgb>) -> StyleColor {
        let own = match color.tag {
            ffi::STYLE_COLOR_NONE => default,
            ffi::STYLE_COLOR_PALETTE => {
                let entry = self.palette.get(usize::from(color.value[0]));
                entry.copied().flatten()
            }
            _ => None,
        };
        own.map_or(color, StyleColor::rgb)
    }

    fn apply(&self, style: &mut Style) {
        style.fg = self.resolve(style.fg, self.foreground);
        style.bg = self.resolve(style.bg, self.background);
        style.underline_color = self.resolve(style.underline_color, None);
    }
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
    /// Oldest first.
    pub notifications: Vec<Notification>,
    /// The program said something about its progress, even if nothing changed.
    pub progress_reported: bool,
    progress: Option<Progress>,
    render_state: ffi::Handle,
    rows: ffi::Handle,
    links: Links,
    /// Reused between cells to avoid allocating for each one.
    uri: Vec<u8>,
    hold_started: Option<Instant>,
    size: (u16, u16, u32, u32),
    /// A `GhosttyColorScheme` value.
    color_scheme: c_int,
}

impl Effects {
    /// Brings the render state up to date with the terminal, and copies what it
    /// leaves out. Only a grid reference has that, which reads the live terminal,
    /// so the time is now, while the two agree. During a hold they will not.
    ///
    /// This runs in the middle of a write when a hold starts, as the library's
    /// rendering guide calls for. Sequences are dispatched one at a time, so none
    /// of the terminal is half done then, and a grid reference touches nothing
    /// that the update did not just read.
    fn update_render_state(&mut self, terminal: ffi::Handle) -> Result<(), Error> {
        // SAFETY: Both handles are live. The render state is not the terminal,
        // so this does not re-enter a write that called us.
        check(unsafe { ffi::ghostty_render_state_update(self.render_state, terminal) })?;
        self.copy_links(terminal);
        Ok(())
    }

    /// Reads the links of the rows that changed. Only rows and cells with the
    /// library's hyperlink flag get as far as a grid reference.
    fn copy_links(&mut self, terminal: ffi::Handle) {
        let mut rows = self.rows;
        let out = (&raw mut rows).cast();
        let state = self.render_state;
        // SAFETY: The render state and iterator are live. The call points the
        // iterator at this state's rows, and `out` is valid for a handle.
        let result =
            unsafe { ffi::ghostty_render_state_get(state, ffi::RENDER_DATA_ROW_ITERATOR, out) };
        if result != ffi::SUCCESS {
            return;
        }
        let all = fully_dirty(state);
        let mut y = 0;
        // SAFETY: The iterator was populated above and the state is unchanged.
        while unsafe { ffi::ghostty_render_state_row_iterator_next(rows) } {
            if all || row_dirty(rows) {
                self.links.clear_row(usize::from(y));
                if row_has_links(rows) {
                    self.copy_row_links(terminal, y);
                }
            }
            y = y.saturating_add(1);
        }
        self.links.forget_unused();
    }

    fn copy_row_links(&mut self, terminal: ffi::Handle, y: u16) {
        for (x, cell) in (0..).zip(raw_cells(self.rows)) {
            if cell_value::<bool>(*cell, ffi::CELL_DATA_HAS_HYPERLINK)
                && let Some(grid_ref) = grid_ref(terminal, ffi::POINT_VIEWPORT, x, u32::from(y))
            {
                fill_buffer(&mut self.uri, |buf, len, written| {
                    // SAFETY: The reference was made just now, `buf` is valid
                    // for `len` bytes, and `written` for a write.
                    unsafe {
                        ffi::ghostty_grid_ref_hyperlink_uri(&raw const grid_ref, buf, len, written)
                    }
                });
                if !self.uri.is_empty() {
                    self.links.add(x, usize::from(y), &self.uri);
                }
            }
        }
    }
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

/// What the Ghostty app says: a VT220 with color and clipboard access, the last
/// of which some programs look for before they use OSC 52.
unsafe extern "C" fn on_device_attributes(
    _terminal: ffi::Handle,
    _userdata: *mut c_void,
    attributes: *mut ffi::DeviceAttributes,
) -> bool {
    // SAFETY: libghostty-vt passes zeroed attributes for this call to fill in.
    let attributes = unsafe { &mut *attributes };
    attributes.primary.conformance_level = 62;
    attributes.primary.features[..2].copy_from_slice(&[22, 52]);
    attributes.primary.num_features = 2;
    attributes.secondary.device_type = 1;
    attributes.secondary.firmware_version = 10;
    true
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
    // SAFETY: libghostty-vt passes a valid pointer to write the answer to.
    unsafe { out.write(effects.color_scheme) };
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

unsafe extern "C" fn on_notification(
    _terminal: ffi::Handle,
    userdata: *mut c_void,
    notification: *const ffi::DesktopNotification,
) {
    // SAFETY: Called by libghostty-vt with the registered userdata.
    let effects = unsafe { effects_of(userdata) };
    // SAFETY: libghostty-vt passes a request valid for the duration of the call.
    let notification = unsafe { &*notification };
    let text = |text: ffi::Bytes| {
        // SAFETY: The strings are valid for the duration of the call.
        String::from_utf8_lossy(unsafe { bytes_of(text.ptr, text.len) }).into_owned()
    };
    effects.notifications.push(Notification {
        title: text(notification.title),
        body: text(notification.body),
    });
}

unsafe extern "C" fn on_progress(
    _terminal: ffi::Handle,
    userdata: *mut c_void,
    report: *const ffi::ProgressReport,
) {
    // SAFETY: Called by libghostty-vt with the registered userdata.
    let effects = unsafe { effects_of(userdata) };
    // SAFETY: libghostty-vt passes a report valid for the duration of the call.
    let report = unsafe { &*report };
    effects.progress_reported = true;
    effects.progress = (report.state != ffi::PROGRESS_STATE_REMOVE).then_some(Progress {
        state: report.state,
        percent: u8::try_from(report.progress).ok(),
    });
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
        let updated = effects.update_render_state(terminal);
        effects.hold_started = updated.is_ok().then(Instant::now);
    } else {
        effects.hold_started = None;
    }
}

/// Owned handles for one libghostty-vt object each, freed on drop.
struct Handles {
    terminal: ffi::Handle,
    /// Made once something is searched for.
    search: ffi::Handle,
    /// What the pointer is up to in selecting text.
    gesture: ffi::Handle,
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
            search: ptr::null_mut(),
            gesture: ptr::null_mut(),
            render_state: ptr::null_mut(),
            rows: ptr::null_mut(),
            cells: ptr::null_mut(),
            key_encoder: ptr::null_mut(),
            key_event: ptr::null_mut(),
            mouse_encoder: ptr::null_mut(),
            mouse_event: ptr::null_mut(),
        }
    }

    /// Allocates what goes with the terminal. On error the handles made so far
    /// are freed by drop.
    fn allocate(&mut self) -> Result<(), Error> {
        let none = ptr::null();
        // SAFETY: A null allocator selects the default, and the out pointer is valid.
        check(unsafe { ffi::ghostty_selection_gesture_new(none, &raw mut self.gesture) })?;
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
        // SAFETY: As above. The search goes before the terminal it holds state in.
        unsafe { ffi::ghostty_search_free(self.search) };
        // SAFETY: As above, and the gesture is passed the terminal it was used with.
        unsafe { ffi::ghostty_selection_gesture_free(self.gesture, self.terminal) };
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
    /// As of the frame being drawn.
    own_colors: OwnColors,
    /// What times the clicks are told apart by start from.
    clock: Instant,
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
        // SAFETY: A null allocator selects the default, and the out pointer is valid.
        check(unsafe {
            ffi::ghostty_terminal_new(ptr::null(), &raw mut handles.terminal, cols, rows)
        })?;
        let mut terminal = Self::around(handles, cols, rows)?;
        terminal.default_to_grapheme_clustering(true)?;
        Ok(terminal)
    }

    /// Brings back a terminal from [`Self::snapshot`]. It holds what the original
    /// did, but none of what the caller set on that one.
    pub fn restore(snapshot: &[u8]) -> Result<Self, Error> {
        let mut handles = Handles::null();
        decode_snapshot(snapshot, &mut handles.terminal)?;
        let mut restored = Self::around(handles, 1, 1)?;
        let cols = restored.get(ffi::DATA_COLS).unwrap_or(1);
        let rows = restored.get(ffi::DATA_ROWS).unwrap_or(1);
        restored.effects().size = (cols, rows, 1, 1);
        let clustering = restored.mode(MODE_GRAPHEME_CLUSTER);
        restored.default_to_grapheme_clustering(clustering)?;
        Ok(restored)
    }

    /// Unlike the library, the Ghostty app has grapheme clustering on unless
    /// configured otherwise, which decides the width of emoji such as keycaps.
    /// This is about what a reset goes back to, and `on` about how it is now.
    fn default_to_grapheme_clustering(&mut self, on: bool) -> Result<(), Error> {
        let mode = MODE_GRAPHEME_CLUSTER;
        let default = ffi::ModeConfig { mode, value: true };
        self.set(ffi::OPT_MODE_DEFAULT, (&raw const default).cast())?;
        let current = ffi::ModeConfig { mode, value: on };
        self.set(ffi::OPT_MODE, (&raw const current).cast())
    }

    /// Everything about the terminal that is not part of a snapshot. Takes the
    /// terminal in `handles`, and makes the rest of them.
    fn around(mut handles: Handles, cols: u16, rows: u16) -> Result<Self, Error> {
        handles.allocate()?;
        let effects = Box::into_raw(Box::new(Effects {
            replies: Vec::new(),
            bell: false,
            title_changed: false,
            clipboard: Vec::new(),
            notifications: Vec::new(),
            progress_reported: false,
            progress: None,
            render_state: handles.render_state,
            rows: handles.rows,
            links: Links::default(),
            uri: Vec::new(),
            hold_started: None,
            size: (cols, rows, 1, 1),
            color_scheme: ffi::COLOR_SCHEME_DARK,
        }));
        let mut terminal = Self {
            handles,
            effects,
            row: Vec::new(),
            own_colors: OwnColors::default(),
            clock: Instant::now(),
            compression: Compression::default(),
        };
        terminal.register_effects()?;
        let scrollback = SCROLLBACK_BYTES;
        terminal.set(
            ffi::OPT_SCROLLBACK_MAX_BYTES,
            (&raw const scrollback).cast(),
        )?;
        let continuation = CONTINUATION_BYTES;
        terminal.set(
            ffi::OPT_CONTINUATION_MAX_BYTES,
            (&raw const continuation).cast(),
        )?;
        Ok(terminal)
    }

    /// The complete state of the terminal, for [`Self::restore`]. `None` if that
    /// cannot be had right now, as in the middle of a very long escape sequence.
    pub fn snapshot(&self) -> Option<Vec<u8>> {
        let mut bytes: *mut u8 = ptr::null_mut();
        let mut len = 0;
        let (terminal, none) = (self.handles.terminal, ptr::null());
        // SAFETY: The terminal is live, and the out pointers are valid.
        let result = unsafe {
            ffi::ghostty_snapshot_encode_alloc(terminal, none, &raw mut bytes, &raw mut len)
        };
        check(result).ok()?;
        // SAFETY: On success the library returns an allocation of `len` bytes.
        let copy = unsafe { bytes_of(bytes, len) }.to_vec();
        // SAFETY: Allocated by the call above with the default allocator.
        unsafe { ffi::ghostty_free(none, bytes, len) };
        Some(copy)
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
            ffi::OPT_DEVICE_ATTRIBUTES,
            on_device_attributes as *const c_void,
        )?;
        self.set(
            ffi::OPT_CLIPBOARD_WRITE,
            on_clipboard_write as *const c_void,
        )?;
        self.set(
            ffi::OPT_DESKTOP_NOTIFICATION,
            on_notification as *const c_void,
        )?;
        self.set(ffi::OPT_PROGRESS_REPORT, on_progress as *const c_void)?;
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

    /// Takes over what the outer terminal said about its colors, for programs
    /// that ask. Returns the report for a program that wants to hear when the
    /// color scheme changes, if it just did.
    pub fn set_outer_colors(&mut self, colors: &OuterColors) -> Vec<u8> {
        if let Some(rgb) = colors.foreground {
            best_effort(self.set(ffi::OPT_COLOR_FOREGROUND, (&raw const rgb).cast()));
        }
        if let Some(rgb) = colors.background {
            best_effort(self.set(ffi::OPT_COLOR_BACKGROUND, (&raw const rgb).cast()));
        }
        self.set_default_palette(&colors.palette);
        let scheme = colors.color_scheme();
        let changed = std::mem::replace(&mut self.effects().color_scheme, scheme) != scheme;
        if !changed || !self.mode(MODE_REPORT_COLOR_SCHEME) {
            return Vec::new();
        }
        encode_into(|buf, len, written| {
            // SAFETY: `buf` is valid for `len` bytes, and `written` for a write.
            unsafe { ffi::ghostty_color_scheme_report_encode(scheme, buf, len, written) }
        })
    }

    /// The library keeps what programs changed, on top of whatever this sets.
    fn set_default_palette(&mut self, entries: &BTreeMap<u8, Rgb>) {
        let mut palette = [Rgb::default(); 256];
        if entries.is_empty() || !self.read(ffi::DATA_COLOR_PALETTE_DEFAULT, &mut palette) {
            return;
        }
        for (index, rgb) in entries {
            if let Some(entry) = palette.get_mut(usize::from(*index)) {
                *entry = *rgb;
            }
        }
        best_effort(self.set(ffi::OPT_COLOR_PALETTE, palette.as_ptr().cast()));
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

    /// The name of the pointer shape the program asked for with OSC 22.
    pub fn pointer_shape(&self) -> &'static str {
        self.get::<c_int>(ffi::DATA_MOUSE_SHAPE)
            .and_then(|shape| POINTER_SHAPES.get(usize::try_from(shape).ok()?))
            .copied()
            .unwrap_or(DEFAULT_POINTER_SHAPE)
    }

    /// How far along the program last said it was, unless it finished.
    pub const fn progress(&self) -> Option<Progress> {
        // SAFETY: Owned by `self`, and callbacks cannot run during this read.
        unsafe { (*self.effects).progress }
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

    /// Puts this row of the screen at the top of the viewport.
    pub fn scroll_to_row(&mut self, row: u32) {
        self.scroll(ffi::SCROLL_ROW, row.try_into().unwrap_or(i32::MAX));
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
            let terminal = self.handles.terminal;
            best_effort(self.effects().update_render_state(terminal));
        }
    }

    /// When an active hold expires, if there is one.
    pub fn hold_deadline(&self) -> Option<Instant> {
        // SAFETY: Owned by `self`, and callbacks cannot run during this read.
        let started = unsafe { (*self.effects).hold_started };
        started.map(|started| started + RENDER_HOLD_LIMIT)
    }

    /// What differs between the colors as of the last [`Terminal::refresh`] and
    /// those of the outer terminal. Reverse video needs nothing of its own here,
    /// since it shows up as a foreground and background that traded places.
    fn read_own_colors(&self) -> OwnColors {
        let mut colors = ffi::RenderColors {
            size: size_of::<ffi::RenderColors>(),
            background: Rgb::default(),
            foreground: Rgb::default(),
            cursor: Rgb::default(),
            cursor_has_value: false,
            palette: [Rgb::default(); 256],
        };
        let out = (&raw mut colors).cast();
        let state = self.handles.render_state;
        // SAFETY: The render state is live, and `out` is a sized colors struct.
        let result = unsafe { ffi::ghostty_render_state_get(state, ffi::RENDER_DATA_COLORS, out) };
        let mut defaults = [Rgb::default(); 256];
        if result != ffi::SUCCESS || !self.read(ffi::DATA_COLOR_PALETTE_DEFAULT, &mut defaults) {
            return OwnColors::default();
        }
        let own = |color: Rgb, default: Rgb| (color != default).then_some(color);
        let palette = if colors.palette == defaults {
            Vec::new()
        } else {
            let entries = colors.palette.iter().zip(&defaults);
            entries
                .map(|(color, default)| own(*color, *default))
                .collect()
        };
        // Without knowing the outer colors, there is nothing to tell them from.
        let foreground = self.get(ffi::DATA_COLOR_FOREGROUND_DEFAULT);
        let background = self.get(ffi::DATA_COLOR_BACKGROUND_DEFAULT);
        let (foreground, background) =
            foreground
                .zip(background)
                .map_or((None, None), |(foreground, background)| {
                    (
                        own(colors.foreground, foreground),
                        own(colors.background, background),
                    )
                });
        OwnColors {
            foreground,
            background,
            palette,
        }
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
        // Rows do not always hear of it: see UPSTREAM.md on reverse video.
        let own_colors = self.read_own_colors();
        let all = all || fully_dirty(state) || own_colors != self.own_colors;
        self.own_colors = own_colors;
        let mut y = 0;
        // SAFETY: The iterator was populated above and the state is unchanged.
        while unsafe { ffi::ghostty_render_state_row_iterator_next(rows) } {
            if all || row_dirty(rows) {
                let mut row = std::mem::take(&mut self.row);
                self.read_row(y, &mut row);
                draw(y, &row);
                self.row = row;
            }
            y += 1;
        }
        // SAFETY: The render state is live.
        best_effort(check(unsafe { ffi::ghostty_render_state_clean(state) }));
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

    fn read_row(&self, y: u16, row: &mut Vec<Cell>) {
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
            self.own_colors.apply(&mut cell.style);
            cell.selected = selection.is_some_and(|(start, end)| (start..=end).contains(&x));
            row.push(cell);
            x = x.saturating_add(1);
        }
        self.links().apply(usize::from(y), row);
    }

    const fn links(&self) -> &Links {
        // SAFETY: Owned by `self`, and callbacks cannot run during this borrow,
        // since they only run during `write`, which takes `&mut self`.
        unsafe { &(*self.effects).links }
    }

    /// The URI of a link in a cell of this terminal, for as long as the cell is
    /// in the viewport.
    pub fn link_uri(&self, link: Link) -> Option<&[u8]> {
        self.links().uri(link)
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
        let (cell_width, cell_height) = input.cell_size;
        let (cell_width, cell_height) = (cell_width.max(1).into(), cell_height.max(1).into());
        let size = ffi::MouseEncoderSize {
            size: size_of::<ffi::MouseEncoderSize>(),
            screen_width: u32::from(cols) * cell_width,
            screen_height: u32::from(rows) * cell_height,
            cell_width,
            cell_height,
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

    pub fn width(&self) -> u16 {
        self.get(ffi::DATA_COLS).unwrap_or(0)
    }

    /// Reads a row of the screen, with history counted in. Empty if there is no
    /// such row. Too slow for drawing, and meant for looking around in the text.
    pub fn screen_line(&self, y: u32) -> Line {
        let mut line = Line::default();
        for x in 0..self.width() {
            let Some(grid_ref) = self.grid_ref(ffi::POINT_SCREEN, x, y) else {
                break;
            };
            if x == 0 {
                read_row_flags(&grid_ref, &mut line);
            }
            line.cells.push(read_char(&grid_ref));
        }
        line
    }

    fn grid_ref(&self, tag: c_int, x: u16, y: u32) -> Option<ffi::GridRef> {
        grid_ref(self.handles.terminal, tag, x, y)
    }

    fn set_selection(&mut self, selection: &ffi::Selection) {
        best_effort(self.set(ffi::OPT_SELECTION, ptr::from_ref(selection).cast()));
    }

    fn selection(&self) -> Option<ffi::Selection> {
        let mut selection = selection_at(NO_REF);
        self.read(ffi::DATA_SELECTION, &mut selection)
            .then_some(selection)
    }

    fn viewport_ref(&self, x: u16, y: u16) -> Option<ffi::GridRef> {
        self.grid_ref(ffi::POINT_VIEWPORT, x, u32::from(y))
    }

    /// Starts a selection at a viewport position.
    pub fn select_start(&mut self, x: u16, y: u16) {
        if let Some(grid_ref) = self.viewport_ref(x, y) {
            self.set_selection(&selection_at(grid_ref));
        }
    }

    /// Moves the end of the current selection to a viewport position. The
    /// terminal keeps the start in place as the viewport scrolls.
    pub fn select_extend(&mut self, x: u16, y: u16) {
        let end = self.viewport_ref(x, y);
        if let (Some(selection), Some(end)) = (self.selection(), end) {
            self.set_selection(&ffi::Selection { end, ..selection });
        }
    }

    /// Selects between two positions of the screen, with history counted in.
    pub fn select_range(&mut self, start: (u16, u32), end: (u16, u32)) {
        let start = self.grid_ref(ffi::POINT_SCREEN, start.0, start.1);
        let end = self.grid_ref(ffi::POINT_SCREEN, end.0, end.1);
        if let (Some(start), Some(end)) = (start, end) {
            self.set_selection(&ffi::Selection {
                start,
                end,
                rectangle: false,
            });
        }
    }

    /// Makes the current selection a rectangle, or stops it being one.
    pub fn set_selection_rectangle(&mut self, rectangle: bool) {
        if let Some(selection) = self.selection() {
            self.set_selection(&ffi::Selection {
                rectangle,
                ..selection
            });
        }
    }

    /// Turns the selection around, returning where the end of it is now, as a
    /// position of the screen.
    pub fn swap_selection_ends(&mut self) -> Option<(u16, u32)> {
        let selection = self.selection()?;
        self.set_selection(&ffi::Selection {
            start: selection.end,
            end: selection.start,
            ..selection
        });
        self.screen_position(&selection.start)
    }

    /// Where the selection starts and ends, as positions of the screen. A
    /// rectangle has its corners here, whichever way it was drawn.
    pub fn selection_bounds(&self) -> Option<((u16, u32), (u16, u32))> {
        let selection = self.selection()?;
        let start = self.screen_position(&selection.start)?;
        let end = self.screen_position(&selection.end)?;
        Some(if selection.rectangle {
            (
                (start.0.min(end.0), start.1.min(end.1)),
                (start.0.max(end.0), start.1.max(end.1)),
            )
        } else if (start.1, start.0) <= (end.1, end.0) {
            (start, end)
        } else {
            (end, start)
        })
    }

    fn screen_position(&self, grid_ref: &ffi::GridRef) -> Option<(u16, u32)> {
        let mut point = ffi::PointCoordinate::default();
        let terminal = self.handles.terminal;
        let tag = ffi::POINT_SCREEN;
        // SAFETY: The terminal is live, and both pointers are valid. The
        // reference comes from this terminal, which has not changed since.
        let result = unsafe {
            ffi::ghostty_terminal_point_from_grid_ref(terminal, grid_ref, tag, &raw mut point)
        };
        (result == ffi::SUCCESS).then_some((point.x, point.y))
    }

    /// Finds some text in the whole screen, and goes on looking for it as the
    /// terminal changes, until [`Terminal::end_search`]. Everything it found so
    /// far comes back in reading order. Asking for the same text again only
    /// looks at what changed.
    pub fn search(&mut self, text: &str) -> Vec<Match> {
        let needle = ffi::Bytes {
            ptr: text.as_ptr(),
            len: text.len(),
        };
        if self.ensure_search().is_err() {
            return Vec::new();
        }
        let value = (&raw const needle).cast();
        // SAFETY: The search is live, and the library copies the needle.
        let result =
            unsafe { ffi::ghostty_search_set(self.handles.search, ffi::SEARCH_OPT_NEEDLE, value) };
        best_effort(check(result));
        self.run_search();
        self.matches(ffi::SEARCH_DATA_MATCHES)
    }

    /// The matches on and just around the viewport as of now, for drawing them.
    /// Empty if nothing is being searched for.
    pub fn visible_matches(&self) -> Vec<Match> {
        if self.handles.search.is_null() {
            return Vec::new();
        }
        self.run_search();
        self.matches(ffi::SEARCH_DATA_VIEWPORT_MATCHES)
    }

    /// How many matches the search has, as of the last time it was brought up
    /// to date.
    pub fn match_count(&self) -> usize {
        let mut count = 0_usize;
        let out = (&raw mut count).cast();
        let search = self.handles.search;
        if !search.is_null() {
            let data = ffi::SEARCH_DATA_TOTAL_MATCHES;
            // SAFETY: The search is live, and `out` is valid for what this data
            // documents.
            best_effort(check(unsafe { ffi::ghostty_search_get(search, data, out) }));
        }
        count
    }

    /// Stops looking, which releases what the search holds in the terminal.
    pub fn end_search(&mut self) {
        let search = std::mem::replace(&mut self.handles.search, ptr::null_mut());
        // SAFETY: Owned by this terminal and not used again. A null one is fine.
        unsafe { ffi::ghostty_search_free(search) };
    }

    fn ensure_search(&mut self) -> Result<(), Error> {
        if !self.handles.search.is_null() {
            return Ok(());
        }
        let handles = &mut self.handles;
        let none = ptr::null();
        // SAFETY: A null allocator selects the default, the out pointer is
        // valid, and the terminal is live.
        check(unsafe { ffi::ghostty_search_new(none, &raw mut handles.search, handles.terminal) })
    }

    fn run_search(&self) {
        // SAFETY: The search is live, and nothing else uses the terminal now.
        best_effort(check(unsafe {
            ffi::ghostty_search_run(self.handles.search)
        }));
    }

    fn matches(&self, data: c_int) -> Vec<Match> {
        let mut found = Vec::new();
        let mut buffer = ffi::SelectionBuffer {
            ptr: ptr::null_mut(),
            cap: 0,
            len: 0,
        };
        let search = self.handles.search;
        // SAFETY: The search is live, and an empty buffer asks for the size.
        let result = unsafe { ffi::ghostty_search_get(search, data, (&raw mut buffer).cast()) };
        if result == ffi::OUT_OF_SPACE {
            found.resize(buffer.len, selection_at(NO_REF));
            buffer = ffi::SelectionBuffer {
                ptr: found.as_mut_ptr(),
                cap: found.len(),
                len: 0,
            };
            // SAFETY: As above, with room for `cap` entries.
            let result = unsafe { ffi::ghostty_search_get(search, data, (&raw mut buffer).cast()) };
            if result != ffi::SUCCESS {
                return Vec::new();
            }
            found.truncate(buffer.len);
        }
        let mut matches: Vec<Match> = found
            .iter()
            .filter_map(|selection| {
                let start = self.screen_position(&selection.start)?;
                let end = self.screen_position(&selection.end)?;
                Some(Match { start, end })
            })
            .collect();
        matches.sort_unstable_by_key(|found| (found.start.1, found.start.0));
        matches
    }

    /// The left button went down at a viewport position, which starts selecting.
    /// A second and a third press in that spot select a word and a line.
    pub fn press_selection(&mut self, x: u16, y: u16) {
        let Some(grid_ref) = self.viewport_ref(x, y) else {
            return;
        };
        let mut event = GestureEvent::new(ffi::GESTURE_EVENT_PRESS);
        let now = self.clock.elapsed().as_nanos();
        let now = u64::try_from(now).unwrap_or(u64::MAX);
        let interval = u64::try_from(MULTI_CLICK_TIME.as_nanos()).unwrap_or(u64::MAX);
        event.set(ffi::GESTURE_OPT_REF, &grid_ref);
        event.set(ffi::GESTURE_OPT_POSITION, &cell_center(x, y));
        event.set(ffi::GESTURE_OPT_REPEAT_DISTANCE, &0.5_f64);
        event.set(ffi::GESTURE_OPT_TIME_NS, &now);
        event.set(ffi::GESTURE_OPT_REPEAT_INTERVAL_NS, &interval);
        match self.fire_gesture(&event) {
            Some(selection) => self.set_selection(&selection),
            None => self.clear_selection(),
        }
    }

    /// The pointer moved to a viewport position with the button down. `edge`
    /// says if it wants the viewport to move, for [`Terminal::autoscroll_selection`].
    pub fn drag_selection(&mut self, x: u16, y: u16, edge: Option<Autoscroll>) {
        let Some(grid_ref) = self.viewport_ref(x, y) else {
            return;
        };
        let mut event = GestureEvent::new(ffi::GESTURE_EVENT_DRAG);
        event.set(ffi::GESTURE_OPT_REF, &grid_ref);
        self.continue_gesture(event, x, y, edge);
    }

    /// Scrolls a row further while the pointer stays at the edge it is at, and
    /// takes the selection along. Returns whether there is more to do.
    pub fn autoscroll_selection(&mut self, x: u16, y: u16, edge: Autoscroll) -> bool {
        let mut event = GestureEvent::new(ffi::GESTURE_EVENT_AUTOSCROLL_TICK);
        let viewport = ffi::PointCoordinate { x, y: u32::from(y) };
        event.set(ffi::GESTURE_OPT_VIEWPORT, &viewport);
        self.continue_gesture(event, x, y, Some(edge));
        let none = ffi::GESTURE_AUTOSCROLL_NONE;
        self.gesture_value(ffi::GESTURE_DATA_AUTOSCROLL, none)
            .is_some_and(|scroll| scroll != none)
    }

    /// The button came up.
    pub fn release_selection(&self) {
        self.fire_gesture(&GestureEvent::new(ffi::GESTURE_EVENT_RELEASE));
    }

    /// Passes on a drag. The library cares about the height only for what it
    /// makes of the position, so that is how `edge` gets there. Pointers are not
    /// more precise than a cell here, where the library goes by how far into a
    /// cell they are, so that a drag by cell has to be worked out here too.
    fn continue_gesture(
        &mut self,
        mut event: GestureEvent,
        x: u16,
        y: u16,
        edge: Option<Autoscroll>,
    ) {
        let height = match edge {
            Some(Autoscroll::Up) => 0.0,
            None => 1.5,
            Some(Autoscroll::Down) => 3.0,
        };
        let geometry = ffi::GestureGeometry {
            columns: u32::from(self.width()),
            cell_width: 1,
            padding_left: 0,
            screen_height: 3,
        };
        let position = ffi::SurfacePosition {
            x: f64::from(x),
            y: height,
        };
        event.set(ffi::GESTURE_OPT_GEOMETRY, &geometry);
        event.set(ffi::GESTURE_OPT_POSITION, &position);
        let selection = self.fire_gesture(&event);
        let cell = ffi::GESTURE_BEHAVIOR_CELL;
        let by_cell = self.gesture_value(ffi::GESTURE_DATA_BEHAVIOR, cell) == Some(cell);
        let anchor = self.gesture_value(ffi::GESTURE_DATA_ANCHOR, NO_REF);
        if let (true, Some(start), Some(end)) = (by_cell, anchor, self.viewport_ref(x, y)) {
            self.set_selection(&ffi::Selection {
                start,
                end,
                rectangle: false,
            });
        } else if let Some(selection) = selection {
            self.set_selection(&selection);
        }
    }

    /// Hands the gesture an event, and returns the selection that comes of it.
    fn fire_gesture(&self, event: &GestureEvent) -> Option<ffi::Selection> {
        let mut selection = selection_at(NO_REF);
        let handles = &self.handles;
        let out = &raw mut selection;
        // SAFETY: All three handles are live, and `out` is valid for a write.
        let result = unsafe {
            ffi::ghostty_selection_gesture_event(handles.gesture, handles.terminal, event.0, out)
        };
        (result == ffi::SUCCESS).then_some(selection)
    }

    /// Reads something about the gesture, into a value of the type `data` has.
    fn gesture_value<T>(&self, data: c_int, mut value: T) -> Option<T> {
        let handles = &self.handles;
        let out = (&raw mut value).cast();
        // SAFETY: Both handles are live, and `out` is valid for what `data`
        // documents, as each caller picks `T` to match.
        let result = unsafe {
            ffi::ghostty_selection_gesture_get(handles.gesture, handles.terminal, data, out)
        };
        (result == ffi::SUCCESS).then_some(value)
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
        // A rectangle's rows are separate pieces of text, unless it reaches the
        // edge of the screen where they wrap. Spaces in them were asked for.
        let reaches_edge = self.selection_bounds().map(|(_, end)| end.0 + 1) == Some(self.width());
        let unwrap = !selection.rectangle || reaches_edge;
        let text = format_selection(formatter, &selection, unwrap);
        // SAFETY: Created above and not used again.
        unsafe { ffi::ghostty_formatter_free(formatter) };
        text
    }
}

/// Decodes a snapshot into a new terminal.
fn decode_snapshot(snapshot: &[u8], terminal: &mut ffi::Handle) -> Result<(), Error> {
    let mut decoder: ffi::Handle = ptr::null_mut();
    let (bytes, len) = (snapshot.as_ptr(), snapshot.len());
    // SAFETY: A null allocator selects the default. The decoder borrows the
    // bytes, which outlive it, and the out pointer is valid.
    check(unsafe {
        ffi::ghostty_snapshot_decoder_new_buf(ptr::null(), &raw mut decoder, bytes, len)
    })?;
    // Without this, a program that was halfway through a sequence could not be
    // saved a second time until it finished it.
    let retain = true;
    let option = ffi::SNAPSHOT_OPT_RETAIN_CONTINUATION;
    // SAFETY: The decoder is live, and the value matches the option's type.
    let result = check(unsafe {
        ffi::ghostty_snapshot_decoder_set(decoder, option, (&raw const retain).cast())
    })
    // SAFETY: The decoder is live, and the out pointer is valid.
    .and_then(|()| check(unsafe { ffi::ghostty_snapshot_decoder_decode(decoder, terminal) }));
    // SAFETY: Created above and not used again. It only borrowed the terminal.
    unsafe { ffi::ghostty_snapshot_decoder_free(decoder) };
    result
}

/// One event for the selection gesture, which lives for as long as it takes to
/// hand it over.
struct GestureEvent(ffi::Handle);

impl GestureEvent {
    fn new(kind: c_int) -> Self {
        let mut event = ptr::null_mut();
        let none = ptr::null();
        // SAFETY: A null allocator selects the default, and the out pointer is
        // valid. A failure leaves a null event, which the gesture turns down.
        best_effort(check(unsafe {
            ffi::ghostty_selection_gesture_event_new(none, &raw mut event, kind)
        }));
        Self(event)
    }

    /// Sets an option to a value of the type it documents, which gets copied.
    fn set<T>(&mut self, option: c_int, value: &T) {
        let value = ptr::from_ref(value).cast();
        // SAFETY: The event is live or null, which the library turns down, and
        // `value` is valid for what `option` documents as each caller makes sure.
        best_effort(check(unsafe {
            ffi::ghostty_selection_gesture_event_set(self.0, option, value)
        }));
    }
}

impl Drop for GestureEvent {
    fn drop(&mut self) {
        // SAFETY: Owned by this struct and not used again. Null is fine.
        unsafe { ffi::ghostty_selection_gesture_event_free(self.0) };
    }
}

/// The middle of a cell, as the gesture's idea of where the pointer is.
fn cell_center(x: u16, y: u16) -> ffi::SurfacePosition {
    ffi::SurfacePosition {
        x: f64::from(x) + 0.5,
        y: f64::from(y) + 0.5,
    }
}

const NO_REF: ffi::GridRef = ffi::GridRef {
    node: ptr::null_mut(),
    x: 0,
    y: 0,
};

fn grid_ref(terminal: ffi::Handle, tag: c_int, x: u16, y: u32) -> Option<ffi::GridRef> {
    let point = ffi::Point {
        tag,
        value: ffi::PointValue {
            coordinate: ffi::PointCoordinate { x, y },
            padding: 0,
        },
    };
    let mut grid_ref = NO_REF;
    // SAFETY: The terminal is live, and the out pointer is valid.
    let result = unsafe { ffi::ghostty_terminal_grid_ref(terminal, point, &raw mut grid_ref) };
    (result == ffi::SUCCESS).then_some(grid_ref)
}

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

fn format_selection(
    formatter: ffi::Handle,
    selection: &ffi::Selection,
    unwrap: bool,
) -> Option<String> {
    let unwrap = (&raw const unwrap).cast();
    // SAFETY: The formatter is live, and the value matches the option's type.
    let result =
        unsafe { ffi::ghostty_formatter_set(formatter, ffi::FORMATTER_OPT_UNWRAP, unwrap) };
    check(result).ok()?;
    let trim = !selection.rectangle;
    let trim = (&raw const trim).cast();
    // SAFETY: As above.
    let result = unsafe { ffi::ghostty_formatter_set(formatter, ffi::FORMATTER_OPT_TRIM, trim) };
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

/// What a cell of a [`Line`] holds.
fn read_char(grid_ref: &ffi::GridRef) -> char {
    let mut raw = 0;
    // SAFETY: The reference was just made, and the out pointer is valid.
    let result = unsafe { ffi::ghostty_grid_ref_cell(grid_ref, &raw mut raw) };
    if result != ffi::SUCCESS {
        return ' ';
    }
    if cell_value::<c_int>(raw, ffi::CELL_DATA_WIDE) == ffi::CELL_WIDE_SPACER_TAIL {
        return '\0';
    }
    match char::from_u32(cell_value(raw, ffi::CELL_DATA_CODEPOINT)) {
        None | Some('\0') => ' ',
        Some(c) => c,
    }
}

fn read_row_flags(grid_ref: &ffi::GridRef, line: &mut Line) {
    let mut row = 0;
    // SAFETY: The reference was just made, and the out pointer is valid.
    if unsafe { ffi::ghostty_grid_ref_row(grid_ref, &raw mut row) } != ffi::SUCCESS {
        return;
    }
    let out = (&raw mut line.wrapped).cast();
    // SAFETY: `out` is valid for the bool this data documents.
    best_effort(check(unsafe {
        ffi::ghostty_row_get(row, ffi::SCREEN_ROW_DATA_WRAP, out)
    }));
    let mut prompt: c_int = 0;
    let out = (&raw mut prompt).cast();
    // SAFETY: `out` is valid for the enum this data documents.
    best_effort(check(unsafe {
        ffi::ghostty_row_get(row, ffi::SCREEN_ROW_DATA_SEMANTIC_PROMPT, out)
    }));
    line.prompt = prompt == ffi::ROW_SEMANTIC_PROMPT;
}

/// Whether every row needs drawing, whatever each row says about itself.
fn fully_dirty(state: ffi::Handle) -> bool {
    let mut dirty = ffi::RENDER_DIRTY_FULL;
    let out = (&raw mut dirty).cast();
    // SAFETY: The render state is live, and `out` is valid for an enum.
    let result = unsafe { ffi::ghostty_render_state_get(state, ffi::RENDER_DATA_DIRTY, out) };
    result != ffi::SUCCESS || dirty == ffi::RENDER_DIRTY_FULL
}

fn row_dirty(rows: ffi::Handle) -> bool {
    let mut dirty = true;
    let out = (&raw mut dirty).cast();
    // SAFETY: The iterator points at a row, and `out` is valid for a bool.
    let result = unsafe { ffi::ghostty_render_state_row_get(rows, ffi::ROW_DATA_DIRTY, out) };
    result != ffi::SUCCESS || dirty
}

/// Whether a row may have links in it, which sometimes turns out not to be so.
fn row_has_links(rows: ffi::Handle) -> bool {
    let mut row: u64 = 0;
    let out = (&raw mut row).cast();
    // SAFETY: The iterator points at a row, and `out` is valid for its raw value.
    let result = unsafe { ffi::ghostty_render_state_row_get(rows, ffi::ROW_DATA_RAW, out) };
    let mut linked = false;
    let out = (&raw mut linked).cast();
    // SAFETY: `out` is valid for a bool.
    result == ffi::SUCCESS
        && unsafe { ffi::ghostty_row_get(row, ffi::SCREEN_ROW_DATA_HYPERLINK, out) } == ffi::SUCCESS
        && linked
}

/// The cells of the row the iterator is at, as raw values. Valid until the next
/// update of the render state.
fn raw_cells<'a>(rows: ffi::Handle) -> &'a [u64] {
    let mut view = ffi::CellsView {
        ptr: ptr::null(),
        len: 0,
    };
    let out = (&raw mut view).cast();
    // SAFETY: The iterator points at a row, and `out` is valid for a view.
    let result = unsafe { ffi::ghostty_render_state_row_get(rows, ffi::ROW_DATA_CELLS_RAW, out) };
    if result != ffi::SUCCESS || view.ptr.is_null() {
        return &[];
    }
    // SAFETY: The library guarantees `len` values at `ptr`.
    unsafe { slice::from_raw_parts(view.ptr, view.len) }
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
        link: None,
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
fn encode_into(encode: impl FnMut(*mut u8, usize, *mut usize) -> c_int) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    fill_buffer(&mut buf, encode);
    buf
}

/// As [`encode_into`], with a buffer that may have room left from earlier. It
/// ends up empty if the encoder fails.
fn fill_buffer(buf: &mut Vec<u8>, mut encode: impl FnMut(*mut u8, usize, *mut usize) -> c_int) {
    buf.resize(buf.capacity(), 0);
    let mut written = 0;
    let mut result = encode(buf.as_mut_ptr(), buf.len(), &raw mut written);
    if result == ffi::OUT_OF_SPACE {
        buf.resize(written, 0);
        result = encode(buf.as_mut_ptr(), buf.len(), &raw mut written);
    }
    buf.truncate(if result == ffi::SUCCESS { written } else { 0 });
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

    #[test]
    fn reports_device_attributes_like_the_app() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        terminal.write(b"\x1b[c\x1b[>c");
        assert_eq!(
            terminal.effects().replies.escape_ascii().to_string(),
            b"\x1b[?62;22;52c\x1b[>1;10;0c".escape_ascii().to_string()
        );
    }

    #[test]
    fn clusters_graphemes_by_default_like_the_app() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        assert!(terminal.mode(MODE_GRAPHEME_CLUSTER));
        // A keycap is one cluster, of a wide character.
        terminal.write("1\u{fe0f}\u{20e3}".as_bytes());
        terminal.refresh();
        assert_eq!(terminal.cursor().unwrap().position, Some((2, 0)));

        terminal.write(b"\x1b[?2027l");
        assert!(!terminal.mode(MODE_GRAPHEME_CLUSTER));
        let restored = Terminal::restore(&terminal.snapshot().unwrap());
        let mut restored = restored.unwrap();
        assert!(!restored.mode(MODE_GRAPHEME_CLUSTER));

        restored.write(b"\x1bc");
        assert!(restored.mode(MODE_GRAPHEME_CLUSTER));
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
        assert_layout!(m, "GhosttySelectionBuffer", ffi::SelectionBuffer,
            "ptr" => ptr, "cap" => cap, "len" => len);
        assert_layout!(m, "GhosttySurfacePosition", ffi::SurfacePosition, "x" => x, "y" => y);
        assert_layout!(m, "GhosttySelectionGestureGeometry", ffi::GestureGeometry,
            "columns" => columns, "cell_width" => cell_width,
            "padding_left" => padding_left, "screen_height" => screen_height);
        assert_layout!(m, "GhosttyTerminalModeConfig", ffi::ModeConfig,
            "mode" => mode, "value" => value);
        assert_layout!(m, "GhosttyDeviceAttributesPrimary", ffi::DeviceAttributesPrimary,
            "conformance_level" => conformance_level, "features" => features,
            "num_features" => num_features);
        assert_layout!(m, "GhosttyDeviceAttributesSecondary", ffi::DeviceAttributesSecondary,
            "device_type" => device_type, "firmware_version" => firmware_version,
            "rom_cartridge" => rom_cartridge);
        assert_layout!(m, "GhosttyDeviceAttributesTertiary", ffi::DeviceAttributesTertiary,
            "unit_id" => unit_id);
        assert_layout!(m, "GhosttyDeviceAttributes", ffi::DeviceAttributes,
            "primary" => primary, "secondary" => secondary, "tertiary" => tertiary);
        assert_layout!(m, "GhosttyCellsView", ffi::CellsView, "ptr" => ptr, "len" => len);
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
        assert_layout!(m, "GhosttyRenderStateColors", ffi::RenderColors,
            "size" => size, "background" => background, "foreground" => foreground,
            "cursor" => cursor, "cursor_has_value" => cursor_has_value, "palette" => palette);
        assert_layout!(m, "GhosttyTerminalDesktopNotification", ffi::DesktopNotification,
            "title" => title, "body" => body);
        assert_layout!(m, "GhosttyTerminalProgressReport", ffi::ProgressReport,
            "state" => state, "progress" => progress);
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

    /// What is drawn for each linked cell, as its text and URI.
    fn links(terminal: &mut Terminal) -> Vec<(String, String)> {
        terminal.refresh();
        let mut cells = Vec::new();
        terminal.draw_rows(true, |_, row| cells.extend_from_slice(row));
        let linked = cells.iter().filter_map(|cell| {
            let uri = terminal.link_uri(cell.link?).unwrap();
            let uri = String::from_utf8_lossy(uri).into_owned();
            Some((cell.text.as_str().to_owned(), uri))
        });
        linked.collect()
    }

    fn link(uri: &str, text: &str) -> String {
        format!("\x1b]8;;{uri}\x1b\\{text}\x1b]8;;\x1b\\")
    }

    #[test]
    fn a_cell_is_no_bigger_for_having_a_link() {
        assert_eq!(size_of::<Cell>(), 48);
    }

    #[test]
    fn reads_the_uri_of_each_cell() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(link("https://a", "a").as_bytes());
        terminal.write(b"-");
        terminal.write(link("https://b", "b").as_bytes());
        assert_eq!(
            links(&mut terminal),
            [
                ("a".to_owned(), "https://a".to_owned()),
                ("b".to_owned(), "https://b".to_owned())
            ]
        );
    }

    #[test]
    fn reads_uris_of_any_length() {
        let uri = format!("https://{}", "x".repeat(1500));
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(link(&uri, "a").as_bytes());
        assert_eq!(links(&mut terminal), [("a".to_owned(), uri)]);
    }

    #[test]
    fn a_uri_is_one_link_wherever_it_shows_up() {
        let mut terminal = Terminal::new(10, 3).unwrap();
        let text = link("https://a", "ab");
        terminal.write(format!("{text}\r\n-{text}\x1b[3;1H").as_bytes());
        terminal.write(link("https://b", "c").as_bytes());
        terminal.refresh();
        let mut cells = Vec::new();
        terminal.draw_rows(true, |_, row| cells.extend_from_slice(row));
        let numbers: Vec<_> = cells.iter().filter_map(|cell| cell.link).collect();
        assert_eq!(numbers.len(), 5);
        assert!(numbers[..4].iter().all(|link| *link == numbers[0]));
        assert_ne!(numbers[4], numbers[0]);
    }

    #[test]
    fn the_same_uri_is_a_different_link_in_another_terminal() {
        let read = || {
            let mut terminal = Terminal::new(10, 1).unwrap();
            terminal.write(link("https://a", "a").as_bytes());
            terminal.refresh();
            let mut found = None;
            terminal.draw_rows(true, |_, row| found = row[0].link);
            found.unwrap()
        };
        assert_ne!(read(), read());
    }

    #[test]
    fn a_link_goes_when_its_text_is_overwritten() {
        let mut terminal = Terminal::new(10, 1).unwrap();
        terminal.write(link("https://a", "a").as_bytes());
        assert_eq!(links(&mut terminal).len(), 1);
        terminal.write(b"\rb");
        assert_eq!(links(&mut terminal), []);
    }

    #[test]
    fn links_are_forgotten_once_they_are_gone_from_the_viewport() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        for number in 0..10_000 {
            terminal.write(link(&format!("https://{number}"), "a\r").as_bytes());
            terminal.refresh();
        }
        assert!(
            terminal.links().known() < 1000,
            "{}",
            terminal.links().known()
        );
    }

    /// The start of a hold arrives in the same write as what it hides, so this
    /// reads links in the middle of the write.
    #[test]
    fn a_hold_keeps_old_text_with_its_old_link() {
        let mut terminal = Terminal::new(10, 1).unwrap();
        let old = link("https://old", "o");
        let new = link("https://new", "n");
        terminal.write(format!("{old}\x1b[?2026h\r{new}").as_bytes());
        let held = [("o".to_owned(), "https://old".to_owned())];
        assert_eq!(links(&mut terminal), held);
        assert_eq!(links(&mut terminal), held, "drawing it again");
        terminal.write(b"\x1b[?2026l");
        assert_eq!(
            links(&mut terminal),
            [("n".to_owned(), "https://new".to_owned())]
        );
    }

    #[test]
    fn a_hold_starting_from_scrolled_back_history_keeps_its_links() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(link("https://old", "o").as_bytes());
        terminal.write(&b"\r\nline".repeat(100));
        terminal.scroll_to_top();
        terminal.write(b"\x1b[?2026h\r\nmore");
        let held = [("o".to_owned(), "https://old".to_owned())];
        assert_eq!(links(&mut terminal), held);
    }

    #[test]
    fn a_hold_can_start_over_compressed_scrollback() {
        let mut terminal = Terminal::new(40, 2).unwrap();
        terminal.write(link("https://old", "o").as_bytes());
        for line in 0..20_000 {
            terminal.write(format!("\r\nline {line}").as_bytes());
        }
        terminal.compress_idle_scrollback();
        while !terminal.compression.finished {
            terminal.compress_idle_scrollback();
        }
        terminal.scroll_to_top();
        terminal.write(b"\x1b[?2026h\r\nmore");
        let held = [("o".to_owned(), "https://old".to_owned())];
        assert_eq!(links(&mut terminal), held);
    }

    /// Whatever a program does, a cell's text and link come from the same moment.
    /// Each letter is only ever written with its own URI. With a debug build of
    /// the library, this also runs the integrity checks of the terminal around
    /// reading links during writes.
    #[test]
    fn text_and_links_stay_paired_through_holds() {
        let mut terminal = Terminal::new(12, 4).unwrap();
        let mut seed: u32 = 1;
        let mut random = |bound: u32| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) % bound
        };
        for round in 0..20_000 {
            let letter = char::from(b'a' + u8::try_from(random(26)).unwrap());
            let (x, y) = (random(12) + 1, random(4) + 1);
            let text = link(&format!("https://{letter}"), &letter.to_string().repeat(3));
            let mut bytes = format!("\x1b[{y};{x}H{text}").into_bytes();
            match random(9) {
                0 => bytes.extend_from_slice(b"\x1b[?2026h"),
                1 => bytes.extend_from_slice(b"\x1b[?2026l"),
                2 => bytes = [b"\x1b[?2026h", &bytes[..]].concat(),
                3 => bytes.extend_from_slice(b"\x1b[4;1H\r\n"),
                4 => bytes.extend_from_slice(b"\x1b[?1049h"),
                5 => bytes.extend_from_slice(b"\x1b[?1049l"),
                6 => terminal.resize(12, 4 + (round % 2), 1, 1).unwrap(),
                7 => terminal.scroll_by(-1),
                _ => terminal.scroll_to_bottom(),
            }
            terminal.write(&bytes);
            if random(3) == 0 {
                for (text, uri) in links(&mut terminal) {
                    assert_eq!(uri, format!("https://{text}"), "in round {round}");
                }
            }
        }
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
    fn a_snapshot_brings_back_the_screen_and_its_history() {
        let mut terminal = Terminal::new(20, 3).unwrap();
        for line in 0..1000 {
            terminal.write(format!("line {line}\r\n").as_bytes());
        }
        let history = terminal.scrollbar().unwrap().total;
        terminal.write(b"\x1b[?1049h\x1b[?2004h\x1b]2;a title\x07full screen");

        let mut restored = Terminal::restore(&terminal.snapshot().unwrap()).unwrap();
        assert_eq!(lines(&mut restored), ["", "", "full screen"]);
        assert!(restored.alternate_screen());
        assert!(restored.mode(MODE_BRACKETED_PASTE));
        assert_eq!(restored.title(), "a title");

        // It works like any other terminal from there on, with effects and all.
        restored.write(b"\x1b[?1049l\x1b[6n\x07");
        assert_eq!(restored.effects().replies, b"\x1b[3;1R");
        assert!(restored.effects().bell);
        assert_eq!(restored.scrollbar().unwrap().total, history);
        restored.scroll_to_top();
        assert_eq!(lines(&mut restored)[0], "line 0");
    }

    #[test]
    fn a_snapshot_carries_an_unfinished_sequence_over() {
        let mut terminal = Terminal::new(20, 1).unwrap();
        terminal.write(b"\x1b[3");
        let mut restored = Terminal::restore(&terminal.snapshot().unwrap()).unwrap();
        restored.write(b"1mred");
        restored.refresh();
        let mut row = Vec::new();
        restored.draw_rows(true, |_, cells| row = cells.to_vec());
        assert_eq!(row[0].text.as_str(), "r");
        assert_eq!(row[0].style.fg.as_palette(), Some(1));
        // And again, from the restored one.
        restored.write(b"\x1b]2;par");
        let mut again = Terminal::restore(&restored.snapshot().unwrap()).unwrap();
        again.write(b"tial\x07");
        assert_eq!(again.title(), "partial");
    }

    #[test]
    fn a_broken_snapshot_is_an_error() {
        assert!(Terminal::restore(b"not a snapshot").is_err());
        let snapshot = Terminal::new(4, 1).unwrap().snapshot().unwrap();
        assert!(Terminal::restore(&snapshot[..snapshot.len() / 2]).is_err());
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

    const WHITE: Rgb = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };
    const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };

    fn outer_terminal() -> Terminal {
        let mut terminal = Terminal::new(4, 1).unwrap();
        terminal.set_outer_colors(&OuterColors {
            foreground: Some(WHITE),
            background: Some(BLACK),
            ..OuterColors::default()
        });
        terminal
    }

    fn first_cell(terminal: &mut Terminal) -> Style {
        terminal.refresh();
        let mut style = Style::default();
        terminal.draw_rows(true, |_, cells| style = cells[0].style);
        style
    }

    #[test]
    fn colors_nobody_changed_stay_symbolic() {
        let mut terminal = outer_terminal();
        terminal.write(b"\x1b[31;4:1;58:5:2mx");
        let style = first_cell(&mut terminal);
        assert_eq!(style.fg.as_palette(), Some(1));
        assert_eq!(style.underline_color.as_palette(), Some(2));
        assert_eq!(style.bg, StyleColor::default());
    }

    #[test]
    fn a_redefined_palette_entry_becomes_rgb() {
        let mut terminal = outer_terminal();
        terminal.write(b"\x1b]4;1;rgb:01/02/03\x1b\\\x1b[31;42;4:1;58:5:1mx");
        let style = first_cell(&mut terminal);
        assert_eq!(style.fg.as_rgb(), Some([1, 2, 3]));
        assert_eq!(style.underline_color.as_rgb(), Some([1, 2, 3]));
        assert_eq!(style.bg.as_palette(), Some(2));
        terminal.write(b"\x1b]104;1\x1b\\");
        assert_eq!(first_cell(&mut terminal).fg.as_palette(), Some(1));
    }

    #[test]
    fn erased_cells_follow_a_redefined_palette_entry() {
        let mut terminal = outer_terminal();
        terminal.write(b"\x1b]4;1;rgb:01/02/03\x1b\\\x1b[41m\x1b[K");
        assert_eq!(first_cell(&mut terminal).bg.as_rgb(), Some([1, 2, 3]));
    }

    #[test]
    fn the_outer_palette_is_not_mistaken_for_a_program_changing_it() {
        let mut terminal = outer_terminal();
        let rgb = Rgb { r: 9, g: 8, b: 7 };
        terminal.set_outer_colors(&OuterColors {
            palette: BTreeMap::from([(1, rgb)]),
            ..OuterColors::default()
        });
        terminal.write(b"\x1b[31mx\x1b]4;1;?\x1b\\");
        assert_eq!(first_cell(&mut terminal).fg.as_palette(), Some(1));
        let reply = String::from_utf8_lossy(&terminal.effects().replies).into_owned();
        assert!(reply.contains("rgb:0909/0808/0707"), "{reply:?}");
    }

    #[test]
    fn a_program_keeps_its_palette_entry_when_the_outer_one_changes() {
        let mut terminal = outer_terminal();
        terminal.write(b"\x1b]4;1;rgb:01/02/03\x1b\\\x1b[31mx");
        terminal.set_outer_colors(&OuterColors {
            palette: BTreeMap::from([(1, WHITE)]),
            ..OuterColors::default()
        });
        assert_eq!(first_cell(&mut terminal).fg.as_rgb(), Some([1, 2, 3]));
    }

    #[test]
    fn changed_default_colors_become_rgb() {
        let mut terminal = outer_terminal();
        terminal.write(b"\x1b]10;rgb:01/02/03\x1b\\\x1b]11;rgb:04/05/06\x1b\\x");
        let style = first_cell(&mut terminal);
        assert_eq!(style.fg.as_rgb(), Some([1, 2, 3]));
        assert_eq!(style.bg.as_rgb(), Some([4, 5, 6]));
        terminal.write(b"\x1b]110\x1b\\\x1b]111\x1b\\");
        assert_eq!(first_cell(&mut terminal), Style::default());
    }

    #[test]
    fn reverse_video_trades_the_default_colors() {
        let mut terminal = outer_terminal();
        terminal.write(b"\x1b[?5hx");
        let style = first_cell(&mut terminal);
        assert_eq!(style.fg.as_rgb(), Some([0, 0, 0]));
        assert_eq!(style.bg.as_rgb(), Some([255, 255, 255]));
        terminal.write(b"\x1b[?5l");
        assert_eq!(first_cell(&mut terminal), Style::default());
    }

    #[test]
    fn reverse_video_redraws_rows_nothing_wrote_to() {
        let mut terminal = outer_terminal();
        terminal.write(b"x");
        first_cell(&mut terminal);
        terminal.write(b"\x1b[?5h");
        terminal.refresh();
        let mut drawn = 0;
        terminal.draw_rows(false, |_, _| drawn += 1);
        assert_eq!(drawn, 1);
        terminal.refresh();
        terminal.draw_rows(false, |_, _| drawn += 1);
        assert_eq!(drawn, 1, "nothing changed the second time");
    }

    #[test]
    fn reverse_video_leaves_explicit_colors_alone() {
        let mut terminal = outer_terminal();
        terminal.write(b"\x1b[?5h\x1b[31mx");
        let style = first_cell(&mut terminal);
        assert_eq!(style.fg.as_palette(), Some(1));
        assert_eq!(style.bg.as_rgb(), Some([255, 255, 255]));
    }

    #[test]
    fn prefers_the_scheme_the_outer_terminal_reports() {
        let guessed = OuterColors {
            background: Some(WHITE),
            ..OuterColors::default()
        };
        assert_eq!(guessed.color_scheme(), ffi::COLOR_SCHEME_LIGHT);
        let reported = OuterColors {
            dark: Some(true),
            ..guessed
        };
        assert_eq!(reported.color_scheme(), ffi::COLOR_SCHEME_DARK);
        assert_eq!(
            OuterColors::default().color_scheme(),
            ffi::COLOR_SCHEME_DARK
        );
    }

    #[test]
    fn reports_a_new_color_scheme_only_to_programs_that_asked() {
        let light = OuterColors {
            dark: Some(false),
            ..OuterColors::default()
        };
        let dark = OuterColors {
            dark: Some(true),
            ..OuterColors::default()
        };
        let mut terminal = Terminal::new(10, 2).unwrap();
        assert_eq!(terminal.set_outer_colors(&light), b"");
        terminal.write(b"\x1b[?2031h");
        assert_eq!(terminal.set_outer_colors(&light), b"");
        assert_eq!(terminal.set_outer_colors(&dark), b"\x1b[?997;1n");
        assert_eq!(terminal.set_outer_colors(&light), b"\x1b[?997;2n");
    }

    #[test]
    fn names_every_pointer_shape_as_the_library_does() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        assert_eq!(terminal.pointer_shape(), DEFAULT_POINTER_SHAPE);
        for shape in POINTER_SHAPES {
            terminal.write(format!("\x1b]22;{shape}\x1b\\").as_bytes());
            assert_eq!(terminal.pointer_shape(), shape);
        }
        terminal.write(b"\x1b]22;\x1b\\");
        assert_eq!(terminal.pointer_shape(), DEFAULT_POINTER_SHAPE);
    }

    #[test]
    fn reports_notifications() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(b"\x1b]9;done\x1b\\\x1b]777;notify;build;failed\x1b\\");
        let notification = |title: &str, body: &str| Notification {
            title: title.to_owned(),
            body: body.to_owned(),
        };
        assert_eq!(
            terminal.effects().notifications,
            [notification("", "done"), notification("build", "failed")]
        );
    }

    #[test]
    fn remembers_progress_until_it_is_removed() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(b"\x1b]9;4;1;50\x1b\\");
        assert!(terminal.effects().progress_reported);
        let half = Progress {
            state: 1,
            percent: Some(50),
        };
        assert_eq!(terminal.progress(), Some(half));
        terminal.write(b"\x1b]9;4;3\x1b\\");
        let unknown = Progress {
            state: 3,
            percent: None,
        };
        assert_eq!(terminal.progress(), Some(unknown));
        terminal.write(b"\x1b]9;4;0\x1b\\");
        assert_eq!(terminal.progress(), None);
    }

    #[test]
    fn selects_and_extracts_text() {
        let mut terminal = Terminal::new(20, 2).unwrap();
        terminal.write(b"hello world");
        terminal.select_start(0, 0);
        terminal.select_extend(4, 0);
        assert_eq!(terminal.selected_text().as_deref(), Some("hello"));
        terminal.clear_selection();
        assert_eq!(terminal.selected_text(), None);
    }

    #[test]
    fn a_second_and_third_press_select_a_word_and_a_line() {
        let mut terminal = Terminal::new(20, 2).unwrap();
        terminal.write(b"hello there world");
        let press = |terminal: &mut Terminal| {
            terminal.press_selection(7, 0);
            terminal.release_selection();
            terminal.selected_text()
        };
        assert_eq!(press(&mut terminal), None);
        assert_eq!(press(&mut terminal).as_deref(), Some("there"));
        assert_eq!(press(&mut terminal).as_deref(), Some("hello there world"));
    }

    #[test]
    fn dragging_selects_from_the_pressed_cell_both_ways() {
        let mut terminal = Terminal::new(20, 2).unwrap();
        terminal.write(b"hello world");
        terminal.press_selection(4, 0);
        terminal.drag_selection(7, 0, None);
        assert_eq!(terminal.selected_text().as_deref(), Some("o wo"));
        terminal.drag_selection(1, 0, None);
        assert_eq!(terminal.selected_text().as_deref(), Some("ello"));
    }

    #[test]
    fn dragging_after_a_double_press_goes_by_word() {
        let mut terminal = Terminal::new(40, 2).unwrap();
        terminal.write(b"one two three four");
        terminal.press_selection(5, 0);
        terminal.release_selection();
        terminal.press_selection(5, 0);
        terminal.drag_selection(9, 0, None);
        assert_eq!(terminal.selected_text().as_deref(), Some("two three"));
    }

    #[test]
    fn a_drag_at_the_edge_scrolls_a_row_at_a_time() {
        let mut terminal = Terminal::new(20, 3).unwrap();
        for line in 0..20 {
            terminal.write(format!("line {line}\r\n").as_bytes());
        }
        terminal.press_selection(0, 1);
        terminal.drag_selection(0, 0, Some(Autoscroll::Up));
        let below = |terminal: &Terminal| terminal.scrollbar().unwrap().lines_below();
        assert_eq!(below(&terminal), 0, "ticks do the scrolling");
        assert!(terminal.autoscroll_selection(0, 0, Autoscroll::Up));
        assert!(terminal.autoscroll_selection(0, 0, Autoscroll::Up));
        assert_eq!(below(&terminal), 2);
        assert_eq!(
            terminal.selected_text().as_deref(),
            Some("line 16\nline 17\nline 18\nl"),
            "the rows that scrolled in join the selection"
        );
    }

    #[test]
    fn searches_all_of_the_history() {
        let mut terminal = Terminal::new(20, 3).unwrap();
        for line in 0..50 {
            terminal.write(format!("line {line}\r\n").as_bytes());
        }
        let matches = terminal.search("line 4");
        assert_eq!(matches.len(), 11);
        assert_eq!(terminal.match_count(), 11);
        assert_eq!(matches[0].start, (0, 4));
        assert_eq!(matches[0].end, (5, 4));
        assert!(matches.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(terminal.visible_matches().iter().any(|m| m.start.1 == 49));

        terminal.write(b"line 4 again");
        assert_eq!(terminal.search("line 4").len(), 12);
        terminal.end_search();
        assert_eq!(terminal.match_count(), 0);
        assert_eq!(terminal.visible_matches(), []);
    }

    #[test]
    fn a_search_finds_text_across_a_wrapped_row() {
        let mut terminal = Terminal::new(5, 3).unwrap();
        terminal.write(b"abcdefgh");
        let matches = terminal.search("defg");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start, (3, 0));
        assert_eq!(matches[0].end, (1, 1));
        assert!(matches[0].contains(4, 0));
        assert!(matches[0].contains(0, 1));
        assert!(!matches[0].contains(2, 1));
        assert!(!matches[0].contains(2, 0));
    }

    #[test]
    fn reads_rows_of_the_screen() {
        let mut terminal = Terminal::new(6, 3).unwrap();
        terminal.write("ab \u{4e16}\r\nabcdefgh".as_bytes());
        let line = terminal.screen_line(0);
        assert_eq!(line.cells[..5], ['a', 'b', ' ', '\u{4e16}', '\0']);
        assert_eq!(line.length(), 5);
        assert_eq!(line.limit(), 3);
        assert!(!line.wrapped);
        assert!(terminal.screen_line(1).wrapped);
        assert_eq!(terminal.screen_line(2).length(), 2);
        assert_eq!(terminal.screen_line(9), Line::default());
    }

    #[test]
    fn a_rectangle_keeps_its_rows_apart() {
        let mut terminal = Terminal::new(4, 3).unwrap();
        terminal.write(b"abcdefgh");
        terminal.select_start(1, 0);
        terminal.select_extend(2, 1);
        terminal.set_selection_rectangle(true);
        assert_eq!(terminal.selected_text().as_deref(), Some("bc\nfg"));
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
            cell_size: (1, 1),
            any_pressed: true,
        };
        assert_eq!(terminal.encode_mouse(&click, 10, 5), b"");
        terminal.write(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(terminal.encode_mouse(&click, 10, 5), b"\x1b[<0;3;4M");
    }

    #[test]
    fn encodes_pixels_for_those_who_ask_and_cells_for_the_rest() {
        let mut terminal = Terminal::new(10, 5).unwrap();
        let click = MouseInput {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            mods: 0,
            x: 37,
            y: 65,
            cell_size: (10, 20),
            any_pressed: true,
        };
        terminal.write(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(terminal.encode_mouse(&click, 10, 5), b"\x1b[<0;4;4M");
        terminal.write(b"\x1b[?1016h");
        assert_eq!(terminal.encode_mouse(&click, 10, 5), b"\x1b[<0;37;65M");
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
