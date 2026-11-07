//! Raw declarations for the parts of the libghostty-vt C API that kl uses.
//!
//! Hand-written to avoid a libclang build dependency, which the library's
//! stable C ABI makes reasonable. A test in the parent module checks the
//! struct layouts against the manifest the library itself reports.

use std::ffi::{c_char, c_int, c_void};

/// All libghostty-vt handles are opaque pointers.
pub type Handle = *mut c_void;

pub const SUCCESS: c_int = 0;
pub const OUT_OF_SPACE: c_int = -3;

pub const CURSOR_STYLE_BAR: c_int = 0;
pub const CURSOR_STYLE_BLOCK: c_int = 1;
pub const CURSOR_STYLE_UNDERLINE: c_int = 2;

pub const OPT_USERDATA: c_int = 1;
pub const OPT_WRITE_PTY: c_int = 2;
pub const OPT_BELL: c_int = 3;
pub const OPT_XTVERSION: c_int = 5;
pub const OPT_TITLE_CHANGED: c_int = 6;
pub const OPT_SIZE: c_int = 7;
pub const OPT_COLOR_SCHEME: c_int = 8;
pub const OPT_DEVICE_ATTRIBUTES: c_int = 9;
pub const OPT_COLOR_FOREGROUND: c_int = 12;
pub const OPT_COLOR_BACKGROUND: c_int = 13;
pub const OPT_COLOR_PALETTE: c_int = 15;
pub const OPT_KITTY_IMAGE_STORAGE_LIMIT: c_int = 16;
pub const OPT_SELECTION: c_int = 22;
pub const OPT_DEFAULT_CURSOR_STYLE: c_int = 23;
pub const OPT_DEFAULT_CURSOR_BLINK: c_int = 24;
pub const OPT_CLIPBOARD_WRITE: c_int = 27;
pub const OPT_SCROLLBACK_MAX_BYTES: c_int = 28;
pub const OPT_DESKTOP_NOTIFICATION: c_int = 30;
pub const OPT_PROGRESS_REPORT: c_int = 31;
pub const OPT_CONTINUATION_MAX_BYTES: c_int = 32;
pub const OPT_MODE_DEFAULT: c_int = 34;
pub const OPT_MODE: c_int = 35;
pub const OPT_TERMINFO_NAME: c_int = 38;
pub const OPT_RENDER_HOLD: c_int = 42;

pub const DATA_COLS: c_int = 1;
pub const DATA_ROWS: c_int = 2;
pub const DATA_ACTIVE_SCREEN: c_int = 6;
pub const DATA_SCROLLBAR: c_int = 9;
pub const DATA_MOUSE_TRACKING: c_int = 11;
pub const DATA_TITLE: c_int = 12;
pub const DATA_PWD: c_int = 13;
pub const DATA_COLOR_CURSOR: c_int = 20;
pub const DATA_COLOR_FOREGROUND_DEFAULT: c_int = 22;
pub const DATA_COLOR_BACKGROUND_DEFAULT: c_int = 23;
pub const DATA_COLOR_PALETTE_DEFAULT: c_int = 25;
pub const DATA_SELECTION: c_int = 31;
pub const DATA_KITTY_GRAPHICS: c_int = 30;
pub const DATA_MODE: c_int = 37;
pub const DATA_MOUSE_SHAPE: c_int = 41;
#[cfg(test)]
pub const DATA_MEMORY_USAGE: c_int = 42;

pub const SCREEN_ALTERNATE: c_int = 1;

pub const SNAPSHOT_OPT_RETAIN_CONTINUATION: c_int = 2;

pub const COMPRESSION_MODE_INCREMENTAL: c_int = 0;
pub const COMPRESSION_RESULT_PENDING: c_int = 1;

pub const SCROLL_BOTTOM: c_int = 1;
pub const SCROLL_DELTA: c_int = 2;
pub const SCROLL_TOP: c_int = 0;
pub const SCROLL_ROW: c_int = 3;

pub const POINT_VIEWPORT: c_int = 1;
pub const POINT_SCREEN: c_int = 2;

pub const RENDER_DATA_DIRTY: c_int = 3;
pub const RENDER_DATA_ROW_ITERATOR: c_int = 4;
pub const RENDER_DATA_CURSOR: c_int = 18;
pub const RENDER_DATA_COLORS: c_int = 19;
pub const RENDER_DATA_IMAGE_ITERATOR: c_int = 22;

pub const IMAGE_PROTOCOL_KITTY: c_int = 0;

pub const IMAGE_DATA_PROTOCOL: c_int = 1;
pub const IMAGE_DATA_IMAGE_ID: c_int = 2;
pub const IMAGE_DATA_IMAGE_GENERATION: c_int = 3;
pub const IMAGE_DATA_Z: c_int = 5;
pub const IMAGE_DATA_VIEWPORT_COL: c_int = 6;
pub const IMAGE_DATA_VIEWPORT_ROW: c_int = 7;
pub const IMAGE_DATA_CELL_OFFSET_X: c_int = 10;
pub const IMAGE_DATA_CELL_OFFSET_Y: c_int = 11;
pub const IMAGE_DATA_PIXEL_WIDTH: c_int = 12;
pub const IMAGE_DATA_PIXEL_HEIGHT: c_int = 13;
pub const IMAGE_DATA_SOURCE_X: c_int = 14;
pub const IMAGE_DATA_SOURCE_Y: c_int = 15;
pub const IMAGE_DATA_SOURCE_WIDTH: c_int = 16;
pub const IMAGE_DATA_SOURCE_HEIGHT: c_int = 17;

pub const KITTY_IMAGE_DATA_WIDTH: c_int = 3;
pub const KITTY_IMAGE_DATA_HEIGHT: c_int = 4;
pub const KITTY_IMAGE_DATA_FORMAT: c_int = 5;
pub const KITTY_IMAGE_DATA_PIXELS: c_int = 7;

pub const KITTY_IMAGE_FORMAT_RGB: c_int = 0;
pub const KITTY_IMAGE_FORMAT_RGBA: c_int = 1;

pub const RENDER_DIRTY_FULL: c_int = 2;

pub const ROW_DATA_DIRTY: c_int = 1;
pub const ROW_DATA_RAW: c_int = 2;
pub const ROW_DATA_CELLS: c_int = 3;
pub const ROW_DATA_SELECTION: c_int = 4;
pub const ROW_DATA_CELLS_RAW: c_int = 5;

pub const CELLS_DATA_RAW: c_int = 1;
pub const CELLS_DATA_STYLE: c_int = 2;
pub const CELLS_DATA_HAS_STYLING: c_int = 8;
pub const CELLS_DATA_GRAPHEMES_UTF8: c_int = 9;

pub const CELL_DATA_CODEPOINT: c_int = 1;
pub const CELL_DATA_CONTENT_TAG: c_int = 2;
pub const CELL_DATA_WIDE: c_int = 3;
pub const CELL_DATA_HAS_HYPERLINK: c_int = 7;
pub const CELL_DATA_SEMANTIC_CONTENT: c_int = 9;
pub const CELL_DATA_COLOR_PALETTE: c_int = 10;
pub const CELL_DATA_COLOR_RGB: c_int = 11;

pub const SCREEN_ROW_DATA_WRAP: c_int = 1;
pub const SCREEN_ROW_DATA_HYPERLINK: c_int = 5;
pub const SCREEN_ROW_DATA_SEMANTIC_PROMPT: c_int = 6;
pub const SCREEN_ROW_DATA_KITTY_VIRTUAL_PLACEHOLDER: c_int = 7;

pub const ROW_SEMANTIC_PROMPT: c_int = 1;

pub const CELL_SEMANTIC_OUTPUT: c_int = 0;

pub const CELL_CONTENT_BG_PALETTE: c_int = 2;
pub const CELL_CONTENT_BG_RGB: c_int = 3;

pub const CELL_WIDE_WIDE: c_int = 1;
pub const CELL_WIDE_SPACER_TAIL: c_int = 2;

pub const STYLE_COLOR_NONE: u8 = 0;
pub const STYLE_COLOR_PALETTE: u8 = 1;
pub const STYLE_COLOR_RGB: u8 = 2;

pub const KEY_OPT_MACOS_OPTION_AS_ALT: c_int = 7;
pub const OPTION_AS_ALT_TRUE: c_int = 1;

pub const MOUSE_OPT_SIZE: c_int = 3;
pub const MOUSE_OPT_ANY_BUTTON_PRESSED: c_int = 4;

pub const GESTURE_EVENT_PRESS: c_int = 0;
pub const GESTURE_EVENT_RELEASE: c_int = 1;
pub const GESTURE_EVENT_DRAG: c_int = 2;
pub const GESTURE_EVENT_AUTOSCROLL_TICK: c_int = 3;

pub const GESTURE_OPT_REF: c_int = 1;
pub const GESTURE_OPT_POSITION: c_int = 2;
pub const GESTURE_OPT_REPEAT_DISTANCE: c_int = 3;
pub const GESTURE_OPT_TIME_NS: c_int = 4;
pub const GESTURE_OPT_REPEAT_INTERVAL_NS: c_int = 5;
pub const GESTURE_OPT_GEOMETRY: c_int = 9;
pub const GESTURE_OPT_VIEWPORT: c_int = 10;

pub const GESTURE_DATA_AUTOSCROLL: c_int = 3;
pub const GESTURE_DATA_BEHAVIOR: c_int = 4;
pub const GESTURE_DATA_ANCHOR: c_int = 5;

pub const GESTURE_AUTOSCROLL_NONE: c_int = 0;
pub const GESTURE_BEHAVIOR_CELL: c_int = 0;

pub const SEARCH_OPT_NEEDLE: c_int = 1;
pub const SEARCH_DATA_TOTAL_MATCHES: c_int = 3;
pub const SEARCH_DATA_MATCHES: c_int = 6;
pub const SEARCH_DATA_VIEWPORT_MATCHES: c_int = 7;

pub const FORMATTER_OPT_UNWRAP: c_int = 2;
pub const FORMATTER_OPT_TRIM: c_int = 3;
pub const FORMATTER_OPT_SELECTION: c_int = 4;

pub const CLIPBOARD_WRITE_SUCCESS: c_int = 0;

pub const PROGRESS_STATE_REMOVE: c_int = 0;

pub const COLOR_SCHEME_LIGHT: c_int = 0;
pub const COLOR_SCHEME_DARK: c_int = 1;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Bytes {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
pub struct Buffer {
    pub ptr: *mut u8,
    pub cap: usize,
    pub len: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// `GhosttyStyleColor`. The C union of a palette index and an RGB value is
/// represented as its three bytes, which keeps reading it free of `unsafe`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct StyleColor {
    pub tag: u8,
    pub value: [u8; 3],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Style {
    pub fg: StyleColor,
    pub bg: StyleColor,
    pub underline_color: StyleColor,
    pub flags: u16,
    pub reserved: u16,
}

/// Position of the viewport in the scrollback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

/// `GhosttyTerminalScrollViewport`, with the union as its padding words.
#[repr(C)]
pub struct ScrollViewport {
    pub tag: c_int,
    pub value: [u64; 2],
}

#[repr(C)]
pub struct Point {
    pub tag: c_int,
    pub value: PointValue,
}

/// `GhosttyPointValue`: the coordinate arm of the C union, padded to its size.
#[repr(C)]
pub struct PointValue {
    pub coordinate: PointCoordinate,
    pub padding: u64,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct PointCoordinate {
    pub x: u16,
    pub y: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct GridRef {
    pub node: *mut c_void,
    pub x: u16,
    pub y: u16,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Selection {
    pub start: GridRef,
    pub end: GridRef,
    pub rectangle: bool,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct SurfacePosition {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
pub struct GestureGeometry {
    pub columns: u32,
    pub cell_width: u32,
    pub padding_left: u32,
    pub screen_height: u32,
}

#[repr(C)]
pub struct SelectionBuffer {
    pub ptr: *mut Selection,
    pub cap: usize,
    pub len: usize,
}

#[repr(C)]
pub struct ModeConfig {
    pub mode: u16,
    pub value: bool,
}

/// `GhosttyCellsView`.
#[repr(C)]
pub struct CellsView {
    pub ptr: *const u64,
    pub len: usize,
}

#[repr(C)]
pub struct RowSelection {
    pub size: usize,
    pub start_x: u16,
    pub end_x: u16,
}

#[repr(C)]
pub struct RenderCursor {
    pub size: usize,
    pub viewport_has_value: bool,
    pub viewport_x: u16,
    pub viewport_y: u16,
    pub wide_tail: bool,
    pub visible: bool,
    pub blinking: bool,
    pub password_input: bool,
    pub visual_style: c_int,
}

#[repr(C)]
pub struct RenderColors {
    pub size: usize,
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    pub cursor_has_value: bool,
    pub palette: [Rgb; 256],
}

#[repr(C)]
pub struct SizeReport {
    pub rows: u16,
    pub columns: u16,
    pub cell_width: u32,
    pub cell_height: u32,
}

/// `GhosttyTerminalMemoryUsage`, with what the tests look at named and the rest
/// as filler.
#[cfg(test)]
#[repr(C)]
pub struct MemoryUsage {
    pub size: usize,
    pub compression_supported: bool,
    pub primary_pages: u64,
    pub primary_virtual_bytes: u64,
    pub primary_resident_bytes: u64,
    pub primary_compressed_pages: u64,
    pub rest: [u64; 8],
}

#[repr(C)]
pub struct MouseEncoderSize {
    pub size: usize,
    pub screen_width: u32,
    pub screen_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub padding_top: u32,
    pub padding_bottom: u32,
    pub padding_right: u32,
    pub padding_left: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct MousePosition {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
pub struct ClipboardContent {
    pub mime: Bytes,
    pub data: Bytes,
}

#[repr(C)]
pub struct ClipboardWrite {
    pub location: c_int,
    pub contents: *const ClipboardContent,
    pub contents_len: usize,
    pub name: Bytes,
    pub granted: bool,
    pub can_remember: bool,
    pub ctx: *const c_void,
    pub reply: unsafe extern "C" fn(*const Self, *const ClipboardWriteReply),
}

#[repr(C)]
pub struct DeviceAttributesPrimary {
    pub conformance_level: u16,
    pub features: [u16; 64],
    pub num_features: usize,
}

#[repr(C)]
pub struct DeviceAttributesSecondary {
    pub device_type: u16,
    pub firmware_version: u16,
    pub rom_cartridge: u16,
}

#[repr(C)]
pub struct DeviceAttributesTertiary {
    pub unit_id: u32,
}

#[repr(C)]
pub struct DeviceAttributes {
    pub primary: DeviceAttributesPrimary,
    pub secondary: DeviceAttributesSecondary,
    pub tertiary: DeviceAttributesTertiary,
}

#[repr(C)]
pub struct ClipboardWriteReply {
    pub size: usize,
    pub result: c_int,
    pub remember: bool,
}

#[repr(C)]
pub struct DesktopNotification {
    pub title: Bytes,
    pub body: Bytes,
}

#[repr(C)]
pub struct ProgressReport {
    pub state: c_int,
    pub progress: i8,
}

unsafe extern "C" {
    pub fn ghostty_terminal_new(
        allocator: *const c_void,
        terminal: *mut Handle,
        cols: u16,
        rows: u16,
    ) -> c_int;
    pub fn ghostty_terminal_free(terminal: Handle);
    pub fn ghostty_terminal_resize(
        terminal: Handle,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> c_int;
    pub fn ghostty_terminal_set(terminal: Handle, option: c_int, value: *const c_void) -> c_int;
    pub fn ghostty_terminal_get(terminal: Handle, data: c_int, out: *mut c_void) -> c_int;
    pub fn ghostty_terminal_vt_write(terminal: Handle, data: *const u8, len: usize);
    pub fn ghostty_terminal_compression_activity(terminal: Handle, activity: *mut u64) -> c_int;
    pub fn ghostty_terminal_compress(terminal: Handle, mode: c_int, result: *mut c_int) -> c_int;
    pub fn ghostty_terminal_scroll_viewport(terminal: Handle, behavior: ScrollViewport) -> c_int;
    pub fn ghostty_terminal_grid_ref(terminal: Handle, point: Point, out: *mut GridRef) -> c_int;
    pub fn ghostty_terminal_point_from_grid_ref(
        terminal: Handle,
        grid_ref: *const GridRef,
        tag: c_int,
        out: *mut PointCoordinate,
    ) -> c_int;
    pub fn ghostty_grid_ref_cell(grid_ref: *const GridRef, out: *mut u64) -> c_int;
    pub fn ghostty_grid_ref_row(grid_ref: *const GridRef, out: *mut u64) -> c_int;
    pub fn ghostty_row_get(row: u64, data: c_int, out: *mut c_void) -> c_int;
    pub fn ghostty_grid_ref_hyperlink_uri(
        grid_ref: *const GridRef,
        buf: *mut u8,
        buf_len: usize,
        written: *mut usize,
    ) -> c_int;
    pub fn ghostty_selection_gesture_new(allocator: *const c_void, gesture: *mut Handle) -> c_int;
    pub fn ghostty_selection_gesture_free(gesture: Handle, terminal: Handle);
    pub fn ghostty_selection_gesture_get(
        gesture: Handle,
        terminal: Handle,
        data: c_int,
        value: *mut c_void,
    ) -> c_int;
    pub fn ghostty_selection_gesture_event_new(
        allocator: *const c_void,
        event: *mut Handle,
        kind: c_int,
    ) -> c_int;
    pub fn ghostty_selection_gesture_event_free(event: Handle);
    pub fn ghostty_selection_gesture_event_set(
        event: Handle,
        option: c_int,
        value: *const c_void,
    ) -> c_int;
    pub fn ghostty_selection_gesture_event(
        gesture: Handle,
        terminal: Handle,
        event: Handle,
        selection: *mut Selection,
    ) -> c_int;

    pub fn ghostty_search_new(
        allocator: *const c_void,
        search: *mut Handle,
        terminal: Handle,
    ) -> c_int;
    pub fn ghostty_search_free(search: Handle);
    pub fn ghostty_search_run(search: Handle) -> c_int;
    pub fn ghostty_search_set(search: Handle, option: c_int, value: *const c_void) -> c_int;
    pub fn ghostty_search_get(search: Handle, data: c_int, value: *mut c_void) -> c_int;

    pub fn ghostty_render_state_new(allocator: *const c_void, state: *mut Handle) -> c_int;
    pub fn ghostty_render_state_free(state: Handle);
    pub fn ghostty_render_state_update(state: Handle, terminal: Handle) -> c_int;
    pub fn ghostty_render_state_clean(state: Handle) -> c_int;
    pub fn ghostty_render_state_get(state: Handle, data: c_int, out: *mut c_void) -> c_int;
    pub fn ghostty_render_state_image_iterator_new(
        allocator: *const c_void,
        iterator: *mut Handle,
    ) -> c_int;
    pub fn ghostty_render_state_image_iterator_free(iterator: Handle);
    pub fn ghostty_render_state_image_iterator_next(iterator: Handle) -> bool;
    pub fn ghostty_render_state_image_get(iterator: Handle, data: c_int, out: *mut c_void)
    -> c_int;
    pub fn ghostty_kitty_graphics_image(graphics: Handle, image_id: u32) -> Handle;
    pub fn ghostty_kitty_graphics_image_get(image: Handle, data: c_int, out: *mut c_void) -> c_int;
    pub fn ghostty_render_state_row_iterator_new(
        allocator: *const c_void,
        iterator: *mut Handle,
    ) -> c_int;
    pub fn ghostty_render_state_row_iterator_free(iterator: Handle);
    pub fn ghostty_render_state_row_iterator_next(iterator: Handle) -> bool;
    pub fn ghostty_render_state_row_get(iterator: Handle, data: c_int, out: *mut c_void) -> c_int;
    pub fn ghostty_render_state_row_cells_new(
        allocator: *const c_void,
        cells: *mut Handle,
    ) -> c_int;
    pub fn ghostty_render_state_row_cells_free(cells: Handle);
    pub fn ghostty_render_state_row_cells_next(cells: Handle) -> bool;
    pub fn ghostty_render_state_row_cells_get(
        cells: Handle,
        data: c_int,
        out: *mut c_void,
    ) -> c_int;
    pub fn ghostty_cell_get(cell: u64, data: c_int, out: *mut c_void) -> c_int;

    pub fn ghostty_key_encoder_new(allocator: *const c_void, encoder: *mut Handle) -> c_int;
    pub fn ghostty_key_encoder_free(encoder: Handle);
    pub fn ghostty_key_encoder_set(encoder: Handle, option: c_int, value: *const c_void) -> c_int;
    pub fn ghostty_key_encoder_set_from_terminal(encoder: Handle, terminal: Handle) -> c_int;
    pub fn ghostty_key_encoder_encode(
        encoder: Handle,
        event: Handle,
        buf: *mut u8,
        buf_len: usize,
        written: *mut usize,
    ) -> c_int;
    pub fn ghostty_key_event_new(allocator: *const c_void, event: *mut Handle) -> c_int;
    pub fn ghostty_key_event_free(event: Handle);
    pub fn ghostty_key_event_set_action(event: Handle, action: c_int) -> c_int;
    pub fn ghostty_key_event_set_key(event: Handle, key: c_int) -> c_int;
    pub fn ghostty_key_event_set_mods(event: Handle, mods: u16) -> c_int;
    pub fn ghostty_key_event_set_consumed_mods(event: Handle, mods: u16) -> c_int;
    pub fn ghostty_key_event_set_utf8(event: Handle, utf8: Bytes) -> c_int;
    pub fn ghostty_key_event_set_unshifted_codepoint(event: Handle, codepoint: u32) -> c_int;

    pub fn ghostty_mouse_encoder_new(allocator: *const c_void, encoder: *mut Handle) -> c_int;
    pub fn ghostty_mouse_encoder_free(encoder: Handle);
    pub fn ghostty_mouse_encoder_set(encoder: Handle, option: c_int, value: *const c_void)
    -> c_int;
    pub fn ghostty_mouse_encoder_set_from_terminal(encoder: Handle, terminal: Handle) -> c_int;
    pub fn ghostty_mouse_encoder_encode(
        encoder: Handle,
        event: Handle,
        buf: *mut u8,
        buf_len: usize,
        written: *mut usize,
    ) -> c_int;
    pub fn ghostty_mouse_event_new(allocator: *const c_void, event: *mut Handle) -> c_int;
    pub fn ghostty_mouse_event_free(event: Handle);
    pub fn ghostty_mouse_event_set_action(event: Handle, action: c_int) -> c_int;
    pub fn ghostty_mouse_event_set_button(event: Handle, button: c_int) -> c_int;
    pub fn ghostty_mouse_event_clear_button(event: Handle) -> c_int;
    pub fn ghostty_mouse_event_set_mods(event: Handle, mods: u16) -> c_int;
    pub fn ghostty_mouse_event_set_position(event: Handle, position: MousePosition) -> c_int;

    pub fn ghostty_focus_encode(
        event: c_int,
        buf: *mut u8,
        buf_len: usize,
        written: *mut usize,
    ) -> c_int;
    pub fn ghostty_color_scheme_report_encode(
        scheme: c_int,
        buf: *mut u8,
        buf_len: usize,
        written: *mut usize,
    ) -> c_int;
    pub fn ghostty_paste_encode(
        data: *mut c_char,
        data_len: usize,
        bracketed: bool,
        buf: *mut u8,
        buf_len: usize,
        written: *mut usize,
    ) -> c_int;

    pub fn ghostty_formatter_terminal_new(
        allocator: *const c_void,
        formatter: *mut Handle,
        terminal: Handle,
    ) -> c_int;
    pub fn ghostty_formatter_set(formatter: Handle, option: c_int, value: *const c_void) -> c_int;
    pub fn ghostty_formatter_format_alloc(
        formatter: Handle,
        allocator: *const c_void,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
    pub fn ghostty_formatter_free(formatter: Handle);
    pub fn ghostty_free(allocator: *const c_void, ptr: *mut u8, len: usize);

    pub fn ghostty_snapshot_encode_alloc(
        terminal: Handle,
        allocator: *const c_void,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
    pub fn ghostty_snapshot_decoder_new_buf(
        allocator: *const c_void,
        decoder: *mut Handle,
        ptr: *const u8,
        len: usize,
    ) -> c_int;
    pub fn ghostty_snapshot_decoder_set(
        decoder: Handle,
        option: c_int,
        value: *const c_void,
    ) -> c_int;
    pub fn ghostty_snapshot_decoder_decode(decoder: Handle, terminal: *mut Handle) -> c_int;
    pub fn ghostty_snapshot_decoder_free(decoder: Handle);

    pub fn ghostty_unicode_codepoint_width(codepoint: u32) -> u8;

    #[cfg(test)]
    pub fn ghostty_type_json() -> *const c_char;
}
