//! Parses what the outer terminal sends into events.
//!
//! The client asks for the kitty keyboard protocol with every flag on, so that
//! panes can get the same full-fidelity events they would get from the outer
//! terminal directly. Legacy sequences are understood too, since they arrive
//! from terminals that ignore that request and before it takes effect.

use std::ffi::c_int;

use crate::ghostty::key::{
    self, MOD_ALT, MOD_CAPS_LOCK, MOD_CTRL, MOD_NUM_LOCK, MOD_SHIFT, MOD_SUPER,
};
use crate::ghostty::{KeyAction, KeyInput, MouseAction, MouseButton, Rgb};

/// A pasted text is capped so a broken terminal cannot grow the buffer forever.
const MAX_PASTE: usize = 64 * 1024 * 1024;

/// Nothing a terminal reports comes close to this, so a longer sequence is not
/// going to end, and must not grow the buffer forever either.
const MAX_SEQUENCE: usize = 1024 * 1024;

const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub action: KeyAction,
    /// A `GhosttyKey` value.
    pub key: c_int,
    pub mods: u16,
    /// The text the key produces, if any.
    pub text: String,
    /// The key's character without shift, or 0.
    pub unshifted: u32,
    /// What the key would produce on a US layout, or 0 if that is not known.
    base: u32,
    /// The character the key stands for with these modifiers. The kitty
    /// protocol leaves the text out of releases and when control or alt is
    /// held, but it decides how the key is encoded for a program all the same.
    character: String,
    /// Shift went into producing the character, so it is not also a modifier.
    consumed_shift: bool,
    /// Text that came as it is rather than as a key. With every kitty flag on,
    /// that is text from dead keys or an input method, and it stays just text
    /// for programs too, whatever they asked for.
    composed: bool,
}

impl KeyEvent {
    const fn press(key: c_int, mods: u16) -> Self {
        Self {
            action: KeyAction::Press,
            key,
            mods,
            text: String::new(),
            unshifted: 0,
            base: 0,
            character: String::new(),
            consumed_shift: false,
            composed: false,
        }
    }

    fn text(text: String) -> Self {
        let unshifted = single_char(&text).map_or(0, u32::from);
        Self {
            key: key::from_codepoint(unshifted),
            unshifted,
            character: text.clone(),
            text,
            composed: true,
            ..Self::press(key::UNIDENTIFIED, 0)
        }
    }

    /// The form the libghostty-vt encoder takes.
    pub fn as_input(&self) -> KeyInput<'_> {
        KeyInput {
            action: self.action,
            key: if self.composed {
                key::UNIDENTIFIED
            } else {
                self.key
            },
            mods: self.mods,
            consumed_mods: if self.consumed_shift { MOD_SHIFT } else { 0 },
            text: &self.character,
            unshifted: if self.composed { 0 } else { self.unshifted },
        }
    }

    /// The modifiers a key binding cares about.
    pub const fn binding_mods(&self) -> u16 {
        self.mods & !(MOD_CAPS_LOCK | MOD_NUM_LOCK)
    }

    /// The key on a US layout, for when this one has no Latin letter to match
    /// a binding by. A layout that does have one keeps its own place for it, so
    /// that `C-b` stays on the key labeled B on a Dvorak keyboard.
    fn latin_base(&self, c: char) -> Option<char> {
        (!c.is_ascii() && self.base != 0)
            .then(|| char::from_u32(self.base))
            .flatten()
    }

    /// Whether this is the given character with control held.
    pub fn is_ctrl(&self, c: char) -> bool {
        let unshifted = char::from_u32(self.unshifted);
        self.binding_mods() == MOD_CTRL
            && unshifted.is_some_and(|own| self.latin_base(own).unwrap_or(own) == c)
    }

    /// The character a binding should see: the produced text if there is any,
    /// otherwise the bare key, which makes control keys match their letter.
    pub fn char(&self) -> Option<char> {
        let own = single_char(&self.text).or_else(|| char::from_u32(self.unshifted))?;
        Some(self.latin_base(own).map_or(own, |base| {
            if self.mods & MOD_SHIFT == 0 {
                base
            } else {
                key::shifted(base)
            }
        }))
    }
}

fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub action: MouseAction,
    /// For motion, the button being dragged.
    pub button: Option<MouseButton>,
    pub mods: u16,
    /// Zero-based cell position.
    pub x: u16,
    pub y: u16,
    /// The position as reported, which is in pixels if the terminal was asked
    /// for that. Only the server knows, and it knows the size of a cell too.
    pub report: (u16, u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(Vec<u8>),
    Focus(bool),
    /// The terminal's default foreground color, from OSC 10.
    Foreground(Rgb),
    /// The terminal's default background color, from OSC 11.
    Background(Rgb),
    /// Whether mouse reports come in pixels from here on, in answer to asking
    /// about the mode. A report does not look any different.
    PixelMouse(bool),
    /// An entry of the terminal's palette, from OSC 4.
    Palette(u8, Rgb),
    /// The terminal's cursor style as a DECSCUSR number, from DECRQSS.
    CursorStyle(u8),
    /// The terminal's color scheme, as asked for or from mode 2031.
    ColorScheme {
        dark: bool,
    },
}

impl Event {
    pub const fn is_about_colors(&self) -> bool {
        matches!(
            self,
            Self::Foreground(_)
                | Self::Background(_)
                | Self::Palette(..)
                | Self::CursorStyle(_)
                | Self::ColorScheme { .. }
        )
    }
}

#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
    /// Text of a bracketed paste in progress.
    paste: Option<Vec<u8>>,
}

/// What came of looking at the front of the buffer.
enum Step {
    /// More bytes are needed to make sense of it.
    Incomplete,
    /// Consumed this many bytes, producing this event if any.
    Consumed(usize, Option<Event>),
    /// A paste begins after this many bytes.
    PasteStart(usize),
}

impl Parser {
    /// Whether a lone escape is waiting to see if a sequence follows it.
    pub fn has_pending_escape(&self) -> bool {
        self.paste.is_none() && self.pending == [0x1b]
    }

    /// Takes a pending lone escape as the escape key. Only legacy terminals
    /// need this: with the kitty protocol the key arrives as a full sequence.
    pub fn flush_escape(&mut self) -> Option<Event> {
        if !self.has_pending_escape() {
            return None;
        }
        self.pending.clear();
        Some(Event::Key(KeyEvent::press(key::ESCAPE, 0)))
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut start = 0;
        loop {
            let input = self.pending.get(start..).unwrap_or(&[]);
            if input.is_empty() {
                break;
            }
            if self.paste.is_some() {
                let (used, event) = self.feed_paste(start);
                events.extend(event);
                if used == 0 {
                    break;
                }
                start += used;
                continue;
            }
            match parse(input) {
                Step::Incomplete => break,
                Step::Consumed(used, event) => {
                    events.extend(event);
                    start += used;
                }
                Step::PasteStart(used) => {
                    self.paste = Some(Vec::new());
                    start += used;
                }
            }
        }
        self.pending.drain(..start);
        events
    }

    /// Moves paste text out of the buffer, returning how much it took. Keeps
    /// back anything that might be the start of the end marker.
    fn feed_paste(&mut self, start: usize) -> (usize, Option<Event>) {
        let input = self.pending.get(start..).unwrap_or(&[]);
        let end = find(input, PASTE_END);
        let safe = end.unwrap_or_else(|| input.len() - partial_suffix(input, PASTE_END));
        let text = input.get(..safe).unwrap_or(&[]).to_vec();
        let Some(paste) = self.paste.as_mut() else {
            return (0, None);
        };
        let room = MAX_PASTE.saturating_sub(paste.len());
        paste.extend_from_slice(text.get(..room).unwrap_or(&text));
        if end.is_none() {
            return (safe, None);
        }
        let text = self.paste.take().unwrap_or_default();
        (safe + PASTE_END.len(), Some(Event::Paste(text)))
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// How many trailing bytes of `input` are a proper prefix of `marker`.
fn partial_suffix(input: &[u8], marker: &[u8]) -> usize {
    (1..marker.len().min(input.len() + 1))
        .rev()
        .find(|len| input.ends_with(marker.get(..*len).unwrap_or(&[])))
        .unwrap_or(0)
}

fn parse(input: &[u8]) -> Step {
    match input {
        [] | [0x1b] => Step::Incomplete,
        [0x1b, b'[', rest @ ..] => parse_csi(rest),
        [0x1b, b'O', rest @ ..] => parse_ss3(rest),
        [0x1b, b']', rest @ ..] => parse_string(rest, 2, parse_osc),
        [0x1b, b'P', rest @ ..] => parse_string(rest, 2, parse_dcs),
        [0x1b, b'_' | b'^' | b'X', rest @ ..] => parse_string(rest, 2, |_| None),
        [0x1b, 0x1b, ..] => Step::Consumed(1, Some(key_event(key::ESCAPE, 0))),
        [0x1b, rest @ ..] => parse_alt(rest),
        [byte, ..] if *byte < 0x20 || *byte == 0x7f => {
            Step::Consumed(1, Some(Event::Key(control_key(*byte, 0))))
        }
        _ => parse_text(input),
    }
}

const fn key_event(key: c_int, mods: u16) -> Event {
    Event::Key(KeyEvent::press(key, mods))
}

/// A legacy control byte, which is how such a terminal reports control keys.
fn control_key(byte: u8, mods: u16) -> KeyEvent {
    let named = match byte {
        b'\t' => Some(key::TAB),
        b'\r' | b'\n' => Some(key::ENTER),
        0x1b => Some(key::ESCAPE),
        0x7f => Some(key::BACKSPACE),
        _ => None,
    };
    if let Some(named) = named {
        return KeyEvent::press(named, mods);
    }
    // 0x01 to 0x1a are the letters, and the rest are the characters that follow
    // `@` in ASCII, down to 0x00 for ctrl+space.
    let c = match byte {
        0 => ' ',
        0x01..=0x1a => char::from(byte + b'a' - 1),
        _ => char::from(byte + 0x40),
    };
    let code = u32::from(c);
    KeyEvent {
        unshifted: code,
        ..KeyEvent::press(key::from_codepoint(code), mods | MOD_CTRL)
    }
}

/// `ESC x` is how a legacy terminal reports alt+x.
fn parse_alt(rest: &[u8]) -> Step {
    match rest {
        [byte, ..] if *byte < 0x20 || *byte == 0x7f => {
            Step::Consumed(2, Some(Event::Key(control_key(*byte, MOD_ALT))))
        }
        _ => match parse_text(rest) {
            Step::Consumed(used, Some(Event::Key(mut event))) => {
                event.mods |= MOD_ALT;
                event.composed = false;
                Step::Consumed(used + 1, Some(Event::Key(event)))
            }
            other => other,
        },
    }
}

/// Consumes one UTF-8 character. Invalid bytes are skipped.
fn parse_text(input: &[u8]) -> Step {
    let Some(&first) = input.first() else {
        return Step::Incomplete;
    };
    let len = match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Step::Consumed(1, None),
    };
    match std::str::from_utf8(input.get(..len).unwrap_or(input)) {
        Ok(text) => Step::Consumed(len, Some(Event::Key(KeyEvent::text(text.to_owned())))),
        Err(error) if error.error_len().is_none() => Step::Incomplete,
        Err(_) => Step::Consumed(1, None),
    }
}

fn parse_ss3(rest: &[u8]) -> Step {
    match rest {
        [] => Step::Incomplete,
        // As with CSI, an escape cancels the sequence and starts a new one.
        [0x1b, ..] => Step::Consumed(2, None),
        [letter, ..] => Step::Consumed(
            3,
            key::from_final_letter(*letter).map(|key| key_event(key, 0)),
        ),
    }
}

/// Consumes a string sequence, which ends with BEL or ST. Any other escape
/// cancels it and starts a new sequence, as it does for the terminal itself.
/// Without that, a string that never ends would swallow every key from then on,
/// since keys are escape sequences too.
fn parse_string(rest: &[u8], prefix: usize, handle: impl Fn(&[u8]) -> Option<Event>) -> Step {
    let Some(end) = find_within_limit(rest, |byte| matches!(byte, 0x07 | 0x1b)) else {
        return give_up_at_limit(rest, prefix);
    };
    let (body, tail) = rest.split_at(end);
    match tail {
        [0x07, ..] => Step::Consumed(prefix + end + 1, handle(body)),
        [0x1b, b'\\', ..] => Step::Consumed(prefix + end + 2, handle(body)),
        [0x1b] => Step::Incomplete,
        _ => Step::Consumed(prefix + end, None),
    }
}

/// Where the sequence in `rest` ends, if it does so within [`MAX_SEQUENCE`].
fn find_within_limit(rest: &[u8], is_end: impl Fn(u8) -> bool) -> Option<usize> {
    rest.iter()
        .take(MAX_SEQUENCE)
        .position(|byte| is_end(*byte))
}

/// What to do with a sequence without an end in sight. Consuming exactly up to
/// the limit, rather than whatever is there, keeps this independent of how the
/// input happened to arrive.
const fn give_up_at_limit(rest: &[u8], prefix: usize) -> Step {
    if rest.len() < MAX_SEQUENCE {
        Step::Incomplete
    } else {
        Step::Consumed(prefix + MAX_SEQUENCE, None)
    }
}

/// The reply to the client asking for the cursor style when attaching.
fn parse_dcs(body: &[u8]) -> Option<Event> {
    let style = body.strip_prefix(b"1$r")?.strip_suffix(b" q")?;
    let style = std::str::from_utf8(style).ok()?;
    style.parse().ok().map(Event::CursorStyle)
}

/// Replies to the color queries the client sends on attaching.
fn parse_osc(body: &[u8]) -> Option<Event> {
    let body = std::str::from_utf8(body).ok()?;
    let (command, rest) = body.split_once(';')?;
    match command {
        "4" => {
            let (index, color) = rest.split_once(';')?;
            Some(Event::Palette(index.parse().ok()?, parse_color(color)?))
        }
        "10" => parse_color(rest).map(Event::Foreground),
        "11" => parse_color(rest).map(Event::Background),
        _ => None,
    }
}

/// Parses `rgb:RRRR/GGGG/BBBB`, with one to four hex digits per component.
fn parse_color(color: &str) -> Option<Rgb> {
    let mut parts = color.strip_prefix("rgb:")?.split('/');
    let component = |part: Option<&str>| -> Option<u8> {
        let part = part.filter(|part| part.len() <= 4)?;
        let value = u32::from_str_radix(part, 16).ok()?;
        let max = 16_u32.pow(u32::try_from(part.len()).ok()?) - 1;
        u8::try_from(value * 255 / max).ok()
    };
    Some(Rgb {
        r: component(parts.next())?,
        g: component(parts.next())?,
        b: component(parts.next())?,
    })
}

fn parse_csi(rest: &[u8]) -> Step {
    let is_end = |byte| (0x40..=0x7e).contains(&byte) || byte == 0x1b;
    let Some(end) = find_within_limit(rest, is_end) else {
        return give_up_at_limit(rest, 2);
    };
    let Some((&last, params)) = rest.get(..=end).and_then(<[u8]>::split_last) else {
        return Step::Incomplete;
    };
    // An escape cancels the sequence and starts a new one.
    if last == 0x1b {
        return Step::Consumed(2 + end, None);
    }
    let used = 2 + end + 1;
    let Ok(params) = std::str::from_utf8(params) else {
        return Step::Consumed(used, None);
    };
    match (last, params) {
        (b'~', "200") => Step::PasteStart(used),
        (b'Z', "") => Step::Consumed(used, Some(key_event(key::TAB, MOD_SHIFT))),
        (b'I', "") => Step::Consumed(used, Some(Event::Focus(true))),
        (b'O', "") => Step::Consumed(used, Some(Event::Focus(false))),
        (b'y', "?1016;1$" | "?1016;3$") => Step::Consumed(used, Some(Event::PixelMouse(true))),
        (b'y', _) if params.starts_with("?1016;") => {
            Step::Consumed(used, Some(Event::PixelMouse(false)))
        }
        (b'n', "?997;1") => Step::Consumed(used, Some(Event::ColorScheme { dark: true })),
        (b'n', "?997;2") => Step::Consumed(used, Some(Event::ColorScheme { dark: false })),
        (b'M' | b'm', _) if params.starts_with('<') => {
            let event = parse_mouse(params.get(1..).unwrap_or(""), last == b'M');
            Step::Consumed(used, event.map(Event::Mouse))
        }
        (b'u', _) => Step::Consumed(used, parse_kitty_key(params).map(Event::Key)),
        (b'~', _) => Step::Consumed(used, parse_functional(params, key::from_tilde_code)),
        _ => {
            let key = key::from_final_letter(last);
            Step::Consumed(
                used,
                key.and_then(|key| parse_functional(params, |_| Some(key))),
            )
        }
    }
}

/// Splits `a:b:c` into numbers, with missing ones as `None`.
fn subparams(param: &str) -> Vec<Option<u32>> {
    param.split(':').map(|part| part.parse().ok()).collect()
}

/// Decodes the `modifiers:event-type` field of a key sequence.
fn parse_modifiers(param: Option<&str>) -> (u16, KeyAction) {
    let parts = param.map(subparams).unwrap_or_default();
    let encoded = parts
        .first()
        .copied()
        .flatten()
        .unwrap_or(1)
        .saturating_sub(1);
    let mut mods = 0;
    for (bit, value) in [
        (1, MOD_SHIFT),
        (2, MOD_ALT),
        (4, MOD_CTRL),
        (8, MOD_SUPER),
        (64, MOD_CAPS_LOCK),
        (128, MOD_NUM_LOCK),
    ] {
        if encoded & bit != 0 {
            mods |= value;
        }
    }
    let action = match parts.get(1).copied().flatten() {
        Some(2) => KeyAction::Repeat,
        Some(3) => KeyAction::Release,
        _ => KeyAction::Press,
    };
    (mods, action)
}

/// `CSI code:shifted:base ; modifiers:event ; text u`
fn parse_kitty_key(params: &str) -> Option<KeyEvent> {
    let mut fields = params.split(';');
    let codes = subparams(fields.next()?);
    let code = codes.first().copied().flatten()?;
    let (mods, action) = parse_modifiers(fields.next());
    let text: String = fields
        .next()
        .map(subparams)
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .filter_map(char::from_u32)
        // Keys like enter are not text, whatever a terminal may report for them.
        .filter(|c| !c.is_control())
        .collect();

    if let Some(key) = key::from_kitty_code(code) {
        return Some(KeyEvent {
            action,
            character: text.clone(),
            text,
            ..KeyEvent::press(key, mods)
        });
    }
    let shifted = codes.get(1).copied().flatten();
    let character = if text.is_empty() {
        key_character(shifted.unwrap_or(code))
    } else {
        text.clone()
    };
    // The physical key, from the base layout code when the terminal knows it.
    let base = codes.get(2).copied().flatten().unwrap_or(0);
    Some(KeyEvent {
        action,
        key: key::from_codepoint(if base == 0 { code } else { base }),
        mods,
        text,
        unshifted: code,
        base,
        consumed_shift: mods & MOD_SHIFT != 0
            && single_char(&character).map(u32::from) != Some(code),
        character,
        composed: false,
    })
}

/// The text of a key's code, which functional keys without a [`key`] of their
/// own do not have: they are in the private use area, and stand for no text.
fn key_character(code: u32) -> String {
    char::from_u32(code)
        .filter(|c| !c.is_control() && !(0xe000..=0xf8ff).contains(&code))
        .map(String::from)
        .unwrap_or_default()
}

/// `CSI number ; modifiers:event ~` or `CSI 1 ; modifiers:event letter`.
fn parse_functional(params: &str, lookup: impl Fn(u32) -> Option<c_int>) -> Option<Event> {
    let mut fields = params.split(';');
    let number = fields
        .next()
        .and_then(|field| field.parse().ok())
        .unwrap_or(1);
    let key = lookup(number)?;
    let (mods, action) = parse_modifiers(fields.next());
    Some(Event::Key(KeyEvent {
        action,
        ..KeyEvent::press(key, mods)
    }))
}

/// `CSI < button ; x ; y M` for a press or motion, with `m` for a release.
fn parse_mouse(params: &str, pressed: bool) -> Option<MouseEvent> {
    let mut fields = params.split(';');
    let code = fields.next()?.parse::<u16>().ok()?;
    // Pixels count from the text, so that the padding around it is negative.
    let mut position = || {
        let value = fields.next()?.parse::<i32>().ok()?;
        Some(u16::try_from(value.max(0)).unwrap_or(u16::MAX))
    };
    let (x, y) = (position()?, position()?);

    let mut mods = 0;
    for (bit, value) in [(4, MOD_SHIFT), (8, MOD_ALT), (16, MOD_CTRL)] {
        if code & bit != 0 {
            mods |= value;
        }
    }
    // What is left without the modifiers and the motion bit.
    let button = match code & !0b11_1100 {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        64 => Some(MouseButton::WheelUp),
        65 => Some(MouseButton::WheelDown),
        66 => Some(MouseButton::WheelLeft),
        67 => Some(MouseButton::WheelRight),
        128 => Some(MouseButton::Eight),
        129 => Some(MouseButton::Nine),
        130 => Some(MouseButton::Ten),
        131 => Some(MouseButton::Eleven),
        _ => None,
    };
    let action = match (code & 32 != 0, pressed) {
        (true, _) => MouseAction::Motion,
        (false, true) => MouseAction::Press,
        (false, false) => MouseAction::Release,
    };
    Some(MouseEvent {
        action,
        button,
        mods,
        x: x.saturating_sub(1),
        y: y.saturating_sub(1),
        report: (x, y),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ghostty::Terminal;

    fn events(bytes: &[u8]) -> Vec<Event> {
        Parser::default().feed(bytes)
    }

    /// The key of a list of events that should be just that one key.
    fn the_key(events: &[Event]) -> KeyEvent {
        assert_eq!(events.len(), 1, "{events:?}");
        let key = events.iter().find_map(|event| match event {
            Event::Key(event) => Some(event.clone()),
            _ => None,
        });
        key.expect("a key event")
    }

    fn only_key(bytes: &[u8]) -> KeyEvent {
        the_key(&events(bytes))
    }

    #[test]
    fn reads_plain_text() {
        let event = only_key(b"a");
        assert_eq!(event.text, "a");
        assert_eq!(event.char(), Some('a'));
        assert_eq!(event.key, key::from_codepoint(u32::from('a')));
    }

    #[test]
    fn reads_multibyte_text_split_across_reads() {
        let mut parser = Parser::default();
        let bytes = "\u{4e16}".as_bytes();
        assert_eq!(parser.feed(&bytes[..1]), []);
        assert_eq!(parser.feed(&bytes[1..2]), []);
        assert_eq!(the_key(&parser.feed(&bytes[2..])).text, "\u{4e16}");
    }

    #[test]
    fn skips_invalid_utf8() {
        assert_eq!(events(b"\xff"), []);
        assert_eq!(only_key(b"\xffa").text, "a");
    }

    #[test]
    fn invalid_utf8_does_not_hold_back_what_follows() {
        assert_eq!(only_key(b"\xf0a").text, "a");
        assert_eq!(only_key(b"\xe4\xb8a").text, "a");
    }

    #[test]
    fn reads_legacy_control_keys() {
        let event = only_key(&[0x02]);
        assert!(event.is_ctrl('b'));
        assert_eq!(only_key(b"\r").key, key::ENTER);
        assert_eq!(only_key(b"\t").key, key::TAB);
        assert_eq!(only_key(&[0x7f]).key, key::BACKSPACE);
        assert!(only_key(&[0]).is_ctrl(' '));
        assert!(only_key(&[0x1f]).is_ctrl('_'));
    }

    #[test]
    fn reads_legacy_alt_keys() {
        let event = only_key(b"\x1bx");
        assert_eq!(event.mods, MOD_ALT);
        assert_eq!(event.text, "x");
    }

    #[test]
    fn reads_legacy_sequences() {
        assert_eq!(only_key(b"\x1b[A").key, key::ARROW_UP);
        assert_eq!(only_key(b"\x1bOB").key, key::ARROW_DOWN);
        assert_eq!(only_key(b"\x1b[5~").key, key::PAGE_UP);
        let event = only_key(b"\x1b[1;5C");
        assert_eq!(event.key, key::ARROW_RIGHT);
        assert_eq!(event.mods, MOD_CTRL);
    }

    #[test]
    fn reads_legacy_shift_tab() {
        let event = only_key(b"\x1b[Z");
        assert_eq!(event.key, key::TAB);
        assert_eq!(event.mods, MOD_SHIFT);
    }

    #[test]
    fn holds_a_lone_escape_until_flushed() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"\x1b"), []);
        assert!(parser.has_pending_escape());
        assert_eq!(
            parser.flush_escape(),
            Some(Event::Key(KeyEvent::press(key::ESCAPE, 0)))
        );
        assert_eq!(parser.flush_escape(), None);
    }

    #[test]
    fn a_lone_escape_can_still_grow_into_a_sequence() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"\x1b"), []);
        assert_eq!(the_key(&parser.feed(b"[A")).key, key::ARROW_UP);
    }

    #[test]
    fn reads_kitty_keys() {
        let event = only_key(b"\x1b[97;1;97u");
        assert_eq!(event.text, "a");
        assert_eq!(event.unshifted, 97);
        assert_eq!(event.action, KeyAction::Press);

        let event = only_key(b"\x1b[98;5u");
        assert!(event.is_ctrl('b'));
        assert_eq!(event.text, "");
    }

    #[test]
    fn reads_kitty_shifted_keys() {
        let event = only_key(b"\x1b[53:37;2;37u");
        assert_eq!(event.unshifted, 53);
        assert_eq!(event.text, "%");
        assert_eq!(event.char(), Some('%'));
        assert_eq!(event.mods, MOD_SHIFT);
        assert_eq!(event.as_input().consumed_mods, MOD_SHIFT);
    }

    #[test]
    fn control_characters_are_not_text() {
        let event = only_key(b"\x1b[13;1;13u");
        assert_eq!(event.key, key::ENTER);
        assert_eq!(event.text, "");
    }

    #[test]
    fn reads_kitty_event_types() {
        assert_eq!(only_key(b"\x1b[97;1:2u").action, KeyAction::Repeat);
        assert_eq!(only_key(b"\x1b[97;1:3u").action, KeyAction::Release);
        assert_eq!(only_key(b"\x1b[A").action, KeyAction::Press);
        assert_eq!(only_key(b"\x1b[1;1:3A").action, KeyAction::Release);
    }

    #[test]
    fn reads_kitty_functional_keys() {
        assert_eq!(only_key(b"\x1b[27u").key, key::ESCAPE);
        assert_eq!(only_key(b"\x1b[13u").key, key::ENTER);
        assert!(key::is_modifier(only_key(b"\x1b[57441;2u").key));
    }

    #[test]
    fn uses_the_base_layout_key_when_reported() {
        // Cyrillic ф on the physical A key.
        let event = only_key(b"\x1b[1092::97;1;1092u");
        assert_eq!(event.key, key::from_codepoint(u32::from('a')));
        assert_eq!(event.unshifted, 1092);
    }

    #[test]
    fn bindings_work_on_layouts_without_latin_letters() {
        // Control with the Cyrillic и on the physical B key.
        assert!(only_key(b"\x1b[1080::98;5u").is_ctrl('b'));
        // The Cyrillic с on the physical C key, and Х with shift on [.
        assert_eq!(only_key(b"\x1b[1089::99;1;1089u").char(), Some('c'));
        assert_eq!(only_key(b"\x1b[1093:1061:91;2;1061u").char(), Some('{'),);
    }

    #[test]
    fn bindings_stay_with_the_letter_on_latin_layouts() {
        // Dvorak, where b is on the physical N key.
        let event = only_key(b"\x1b[98::110;5u");
        assert!(event.is_ctrl('b'));
        assert!(!event.is_ctrl('n'));
        // Text from the layout wins over the key it happens to be on.
        assert_eq!(only_key(b"\x1b[98::110;1;98u").char(), Some('b'));
    }

    #[test]
    fn ignores_lock_keys_in_bindings() {
        let event = only_key(b"\x1b[98;133u");
        assert!(event.mods & MOD_NUM_LOCK != 0);
        assert!(event.is_ctrl('b'));
    }

    #[test]
    fn reads_mouse_events() {
        let press = MouseEvent {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            mods: 0,
            x: 9,
            y: 19,
            report: (10, 20),
        };
        assert_eq!(events(b"\x1b[<0;10;20M"), [Event::Mouse(press)]);
        assert_eq!(
            events(b"\x1b[<0;10;20m"),
            [Event::Mouse(MouseEvent {
                action: MouseAction::Release,
                ..press
            })]
        );
        assert_eq!(
            events(b"\x1b[<32;10;20M"),
            [Event::Mouse(MouseEvent {
                action: MouseAction::Motion,
                ..press
            })]
        );
        assert_eq!(
            events(b"\x1b[<35;10;20M"),
            [Event::Mouse(MouseEvent {
                action: MouseAction::Motion,
                button: None,
                ..press
            })]
        );
        assert_eq!(
            events(b"\x1b[<65;10;20M"),
            [Event::Mouse(MouseEvent {
                button: Some(MouseButton::WheelDown),
                ..press
            })]
        );
        assert_eq!(
            events(b"\x1b[<16;10;20M"),
            [Event::Mouse(MouseEvent {
                mods: MOD_CTRL,
                ..press
            })]
        );
    }

    #[test]
    fn reads_the_extra_mouse_buttons() {
        assert_eq!(
            events(b"\x1b[<129;1;1M"),
            [Event::Mouse(MouseEvent {
                action: MouseAction::Press,
                button: Some(MouseButton::Nine),
                mods: 0,
                x: 0,
                y: 0,
                report: (1, 1),
            })]
        );
    }

    #[test]
    fn keeps_a_mouse_report_in_the_padding_around_the_text() {
        assert!(matches!(
            events(b"\x1b[<0;-3;70000M")[..],
            [Event::Mouse(MouseEvent {
                report: (0, u16::MAX),
                ..
            })]
        ));
    }

    #[test]
    fn hears_when_mouse_reports_change_to_pixels() {
        assert_eq!(events(b"\x1b[?1016;1$y"), [Event::PixelMouse(true)]);
        assert_eq!(events(b"\x1b[?1016;3$y"), [Event::PixelMouse(true)]);
        assert_eq!(events(b"\x1b[?1016;2$y"), [Event::PixelMouse(false)]);
        // A terminal that does not know the mode is not in it.
        assert_eq!(events(b"\x1b[?1016;0$y"), [Event::PixelMouse(false)]);
    }

    #[test]
    fn reads_focus_events() {
        assert_eq!(
            events(b"\x1b[I\x1b[O"),
            [Event::Focus(true), Event::Focus(false)]
        );
    }

    #[test]
    fn reads_pastes() {
        assert_eq!(
            events(b"\x1b[200~hi \x1b[Athere\x1b[201~"),
            [Event::Paste(b"hi \x1b[Athere".to_vec())]
        );
    }

    #[test]
    fn reads_pastes_with_the_end_marker_split_across_reads() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"\x1b[200~abc\x1b[20"), []);
        assert_eq!(parser.feed(b"1"), []);
        assert_eq!(
            parser.feed(b"~x"),
            [
                Event::Paste(b"abc".to_vec()),
                Event::Key(KeyEvent::text("x".to_owned()))
            ]
        );
    }

    #[test]
    fn a_paste_can_contain_what_looks_like_the_start_of_its_end() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"\x1b[200~a\x1b[2"), []);
        assert_eq!(
            parser.feed(b"2b\x1b[201~"),
            [Event::Paste(b"a\x1b[22b".to_vec())]
        );
    }

    #[test]
    fn reads_color_replies() {
        let foreground = Rgb {
            r: 255,
            g: 128,
            b: 0,
        };
        assert_eq!(
            events(b"\x1b]10;rgb:ffff/8080/0000\x1b\\"),
            [Event::Foreground(foreground)]
        );
        let background = Rgb {
            r: 17,
            g: 34,
            b: 51,
        };
        assert_eq!(
            events(b"\x1b]11;rgb:11/22/33\x07"),
            [Event::Background(background)]
        );
        assert_eq!(events(b"\x1b]11;rgb:fffffff/0/0\x07"), []);
    }

    #[test]
    fn reads_palette_replies() {
        let rgb = Rgb {
            r: 17,
            g: 34,
            b: 51,
        };
        assert_eq!(
            events(b"\x1b]4;200;rgb:1111/2222/3333\x1b\\"),
            [Event::Palette(200, rgb)]
        );
        // Entries past the palette are the special colors of other sequences.
        assert_eq!(events(b"\x1b]4;256;rgb:11/22/33\x1b\\"), []);
        assert_eq!(events(b"\x1b]4;1\x1b\\"), []);
    }

    #[test]
    fn reads_color_scheme_reports() {
        assert_eq!(
            events(b"\x1b[?997;1n\x1b[?997;2n"),
            [
                Event::ColorScheme { dark: true },
                Event::ColorScheme { dark: false }
            ]
        );
    }

    #[test]
    fn reads_cursor_style_replies() {
        assert_eq!(events(b"\x1bP1$r5 q\x1b\\"), [Event::CursorStyle(5)]);
        // That setting is not valid, or this is a reply about another one.
        assert_eq!(events(b"\x1bP0$r\x1b\\"), []);
        assert_eq!(events(b"\x1bP1$r0m\x1b\\"), []);
    }

    #[test]
    fn skips_sequences_it_does_not_know() {
        assert_eq!(events(b"\x1b]52;c;aGk=\x07"), []);
        assert_eq!(events(b"\x1bP1$r0m\x1b\\"), []);
        assert_eq!(events(b"\x1b_Gi=1\x1b\\"), []);
        assert_eq!(events(b"\x1b[?62;22c"), []);
        assert_eq!(only_key(b"\x1b[?998;1na").text, "a");
    }

    #[test]
    fn waits_for_incomplete_sequences() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"\x1b[1;"), []);
        assert_eq!(parser.feed(b"\x1b]10;rgb"), []);
        assert!(!parser.has_pending_escape());
    }

    #[test]
    fn an_escape_cancels_an_unfinished_sequence() {
        assert_eq!(only_key(b"\x1b[1;\x1b[A").key, key::ARROW_UP);
    }

    #[test]
    fn an_escape_cancels_an_unfinished_ss3_sequence() {
        assert_eq!(only_key(b"\x1bO\x1b[A").key, key::ARROW_UP);
    }

    #[test]
    fn an_escape_cancels_an_unfinished_string() {
        assert_eq!(only_key(b"\x1b]11;rgb:\x1b[A").key, key::ARROW_UP);
        assert_eq!(only_key(b"\x1bPabc\x1b[97u").text, "");
        // Whether it is the start of the terminator is yet to be seen.
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"\x1b]11;rgb:1/2/3\x1b"), []);
        assert_eq!(
            parser.feed(b"\\"),
            [Event::Background(Rgb {
                r: 17,
                g: 34,
                b: 51
            })]
        );
    }

    #[test]
    fn a_sequence_that_never_ends_does_not_grow_the_buffer() {
        for start in [&b"\x1b["[..], b"\x1b]", b"\x1bP"] {
            let mut parser = Parser::default();
            parser.feed(start);
            let half = vec![b'0'; MAX_SEQUENCE / 2];
            for _ in 0..3 {
                parser.feed(&half);
                assert!(parser.pending.len() <= MAX_SEQUENCE + start.len());
            }
        }
    }

    #[test]
    fn a_paste_over_the_limit_keeps_its_start() {
        let mut parser = Parser::default();
        parser.feed(b"\x1b[200~");
        parser.feed(&vec![b'a'; MAX_PASTE - 1]);
        parser.feed(b"bcd");
        parser.feed(b"e");
        assert!(matches!(
            parser.feed(b"\x1b[201~").as_slice(),
            [Event::Paste(text)] if text.len() == MAX_PASTE && text.last() == Some(&b'b')
        ));
    }

    fn kitty_terminal() -> Terminal {
        let mut terminal = Terminal::new(80, 24).unwrap();
        terminal.write(b"\x1b[>31u");
        terminal
    }

    /// Every key with a code of its own, as Ghostty sends it to kl.
    #[test]
    fn reads_every_key_the_terminal_encodes() {
        let mut terminal = kitty_terminal();
        let mut encoded = 0;
        for key in 1..=175 {
            for mods in [
                0,
                MOD_SHIFT,
                MOD_CTRL,
                MOD_ALT,
                MOD_SUPER,
                MOD_CTRL | MOD_SHIFT,
                MOD_CAPS_LOCK,
                MOD_NUM_LOCK,
            ] {
                for action in [KeyAction::Press, KeyAction::Repeat, KeyAction::Release] {
                    let bytes = terminal.encode_key(&KeyInput {
                        action,
                        key,
                        mods,
                        consumed_mods: 0,
                        text: "",
                        unshifted: 0,
                    });
                    // Keys for text need their character, which is the next test.
                    if bytes.is_empty() {
                        continue;
                    }
                    encoded += 1;
                    let event = only_key(&bytes);
                    assert_eq!(
                        (event.key, event.mods, event.action),
                        (key, mods, action),
                        "{}",
                        bytes.escape_ascii()
                    );
                }
            }
        }
        assert!(encoded > 1500, "{encoded}");
    }

    /// Every key of a US layout with a character on it, typed in the ways
    /// that matter for bindings and programs.
    #[test]
    fn reads_every_character_the_terminal_encodes() {
        let mut terminal = kitty_terminal();
        for unshifted in (' '..='~').filter(|c| !c.is_ascii_uppercase()) {
            let key = key::from_codepoint(u32::from(unshifted));
            if key == key::UNIDENTIFIED {
                continue;
            }
            let shifted = key::shifted(unshifted);
            let plain = unshifted.to_string();
            let upper = shifted.to_string();
            for (mods, text, binding) in [
                (0, plain.as_str(), unshifted),
                (MOD_SHIFT, upper.as_str(), shifted),
                (MOD_CTRL, "", unshifted),
                (MOD_ALT, plain.as_str(), unshifted),
            ] {
                let bytes = terminal.encode_key(&KeyInput {
                    action: KeyAction::Press,
                    key,
                    mods,
                    consumed_mods: if text.is_empty() { 0 } else { mods & MOD_SHIFT },
                    text,
                    unshifted: u32::from(unshifted),
                });
                let event = only_key(&bytes);
                let context = bytes.escape_ascii();
                assert_eq!(event.key, key, "{context}");
                assert_eq!(event.mods, mods, "{context}");
                // Text that alt went into is not reported, just the key.
                let reported = if mods == MOD_ALT { "" } else { text };
                assert_eq!(event.text, reported, "{context}");
                assert_eq!(event.unshifted, u32::from(unshifted), "{context}");
                assert_eq!(event.char(), Some(binding), "{context}");
                assert_eq!(event.is_ctrl(unshifted), mods == MOD_CTRL, "{context}");
            }
        }
    }

    /// Sequences logged from the Ghostty app, in the forms it really has for
    /// them, which leave out a good deal more than the encoder does for a test.
    #[test]
    fn reads_what_the_ghostty_app_sends() {
        for (bytes, key, mods, action) in [
            (
                &b"\x1b[97;;97u"[..],
                key::from_codepoint(97),
                0,
                KeyAction::Press,
            ),
            (
                b"\x1b[97;1:3u",
                key::from_codepoint(97),
                0,
                KeyAction::Release,
            ),
            (b"\x1b[13u", key::ENTER, 0, KeyAction::Press),
            (b"\x1b[1;1:1A", key::ARROW_UP, 0, KeyAction::Press),
            (b"\x1b[1;5:3A", key::ARROW_UP, MOD_CTRL, KeyAction::Release),
            (b"\x1b[1;1:1H", key::HOME, 0, KeyAction::Press),
            (b"\x1b[1;1:1P", key::F1, 0, KeyAction::Press),
            (b"\x1b[5;1:3~", key::PAGE_UP, 0, KeyAction::Release),
        ] {
            let event = only_key(bytes);
            let context = bytes.escape_ascii();
            assert_eq!(
                (event.key, event.mods, event.action),
                (key, mods, action),
                "{context}"
            );
        }
        // The prefix, as the modifier, the key and their releases.
        let events = events(b"\x1b[57442;5u\x1b[98;5u\x1b[98;5:3u\x1b[57442;1:3u");
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[1], Event::Key(event) if event.is_ctrl('b')));
    }

    /// What programs got from the Ghostty app when running directly in it, for
    /// the same keys that reach kl as `outer` with every flag on.
    #[test]
    fn encodes_keys_for_programs_as_ghostty_would() {
        const LEGACY: &[u8] = b"";
        for (outer, flags, expected) in [
            // Ctrl, alt and super keep a shifted character out of the sequence.
            (&b"\x1b[120:88;6u"[..], LEGACY, &b"\x1b[120;6u"[..]),
            (b"\x1b[120:88;4u", LEGACY, b"\x1bX"),
            (b"\x1b[120;9u", LEGACY, b"x"),
            // The same for a character that has no control code.
            (b"\x1b[91;5u", LEGACY, b"\x1b[91;5u"),
            (b"\x1b[99;5u", LEGACY, b"\x03"),
            (b"\x1b[120;3u", LEGACY, b"\x1bx"),
            // Shift is a modifier unless it made a difference to the character.
            (b"\x1b[32;2;32u", b"\x1b[=1;1u", b"\x1b[32;2u"),
            (b"\x1b[97:65;2;65u", b"\x1b[=1;1u", b"A"),
            // Shifted characters are part of releases too, without their text.
            (b"\x1b[97:65;2:3u", b"\x1b[=31;1u", b"\x1b[97:65;2:3u"),
            (b"\x1b[53:37;2:3u", b"\x1b[=31;1u", b"\x1b[53:37;2:3u"),
            (b"\x1b[120:88;6u", b"\x1b[=31;1u", b"\x1b[120:88;6u"),
            // Text from dead keys, which is text whatever the flags are.
            ("\u{e9}".as_bytes(), LEGACY, "\u{e9}".as_bytes()),
            ("\u{e9}".as_bytes(), b"\x1b[=31;1u", "\u{e9}".as_bytes()),
            (b"'", b"\x1b[=31;1u", b"'"),
        ] {
            let mut terminal = Terminal::new(80, 24).unwrap();
            terminal.write(flags);
            let event = only_key(outer);
            assert_eq!(
                terminal
                    .encode_key(&event.as_input())
                    .escape_ascii()
                    .to_string(),
                expected.escape_ascii().to_string(),
                "{} with {}",
                outer.escape_ascii(),
                flags.escape_ascii()
            );
        }
    }

    #[test]
    fn functional_keys_without_a_name_are_not_text() {
        // Media play in the kitty protocol.
        assert_eq!(only_key(b"\x1b[57428u").as_input().text, "");
    }

    #[test]
    fn every_split_of_a_stream_parses_the_same() {
        let stream: &[u8] =
            b"a\x1b[A\x1b[<0;1;2M\x1b[97;1;97u\x1b[200~p\x1b[201~\x1b]11;rgb:1/2/3\x07z";
        let expected = events(stream);
        assert_eq!(expected.len(), 7);
        for split in 0..stream.len() {
            let mut parser = Parser::default();
            let mut got = parser.feed(&stream[..split]);
            got.extend(parser.feed(&stream[split..]));
            assert_eq!(got, expected, "split at {split}");
        }
    }
}
