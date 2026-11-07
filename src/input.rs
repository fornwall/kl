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
        }
    }

    fn text(text: String) -> Self {
        let unshifted = single_char(&text).map_or(0, u32::from);
        Self {
            action: KeyAction::Press,
            key: key::from_codepoint(unshifted),
            mods: 0,
            text,
            unshifted,
            base: 0,
        }
    }

    /// The form the libghostty-vt encoder takes. Shift that went into producing
    /// the text must not also be reported as a modifier of it.
    pub fn as_input(&self) -> KeyInput<'_> {
        KeyInput {
            action: self.action,
            key: self.key,
            mods: self.mods,
            consumed_mods: if self.text.is_empty() {
                0
            } else {
                self.mods & MOD_SHIFT
            },
            text: &self.text,
            unshifted: self.unshifted,
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
    /// The terminal's color scheme, as asked for or from mode 2031.
    ColorScheme {
        dark: bool,
    },
}

impl Event {
    pub const fn is_about_colors(&self) -> bool {
        matches!(
            self,
            Self::Foreground(_) | Self::Background(_) | Self::ColorScheme { .. }
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
        if paste.len() + text.len() <= MAX_PASTE {
            paste.extend_from_slice(&text);
        }
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
        [0x1b, b'P' | b'_' | b'^' | b'X', rest @ ..] => parse_string(rest, 2, |_| None),
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
    let Some(character) = input.get(..len) else {
        return Step::Incomplete;
    };
    std::str::from_utf8(character).map_or(Step::Consumed(1, None), |text| {
        Step::Consumed(len, Some(Event::Key(KeyEvent::text(text.to_owned()))))
    })
}

fn parse_ss3(rest: &[u8]) -> Step {
    let Some(&letter) = rest.first() else {
        return Step::Incomplete;
    };
    Step::Consumed(
        3,
        key::from_final_letter(letter).map(|key| key_event(key, 0)),
    )
}

/// Consumes a string sequence, which ends with BEL or ST.
fn parse_string(rest: &[u8], prefix: usize, handle: impl Fn(&[u8]) -> Option<Event>) -> Step {
    let bel = rest.iter().position(|byte| *byte == 0x07);
    let st = find(rest, b"\x1b\\");
    let (end, terminator) = match (bel, st) {
        (Some(bel), Some(st)) if st < bel => (st, 2),
        (Some(bel), _) => (bel, 1),
        (None, Some(st)) => (st, 2),
        (None, None) => return Step::Incomplete,
    };
    let body = rest.get(..end).unwrap_or(&[]);
    Step::Consumed(prefix + end + terminator, handle(body))
}

/// Replies to the color queries the client sends on attaching.
fn parse_osc(body: &[u8]) -> Option<Event> {
    let body = std::str::from_utf8(body).ok()?;
    let (command, color) = body.split_once(';')?;
    let rgb = parse_color(color)?;
    match command {
        "10" => Some(Event::Foreground(rgb)),
        "11" => Some(Event::Background(rgb)),
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
    let is_end = |byte: &u8| (0x40..=0x7e).contains(byte) || *byte == 0x1b;
    let Some(end) = rest.iter().position(is_end) else {
        return Step::Incomplete;
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
            key,
            mods,
            text,
            unshifted: 0,
            base: 0,
        });
    }
    // The physical key, from the base layout code when the terminal knows it.
    let base = codes.get(2).copied().flatten().unwrap_or(0);
    Some(KeyEvent {
        action,
        key: key::from_codepoint(if base == 0 { code } else { base }),
        mods,
        text,
        unshifted: code,
        base,
    })
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
    let mut fields = params.split(';').map(str::parse::<u16>);
    let (code, x, y) = (
        fields.next()?.ok()?,
        fields.next()?.ok()?,
        fields.next()?.ok()?,
    );

    let mut mods = 0;
    for (bit, value) in [(4, MOD_SHIFT), (8, MOD_ALT), (16, MOD_CTRL)] {
        if code & bit != 0 {
            mods |= value;
        }
    }
    // What is left without the modifiers and the motion bit. The buttons from
    // 128 up are the extra ones on the side of a mouse.
    let button = match code & !0b11_1100 {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        64 => Some(MouseButton::WheelUp),
        65 => Some(MouseButton::WheelDown),
        66 => Some(MouseButton::WheelLeft),
        67 => Some(MouseButton::WheelRight),
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_extra_mouse_buttons_are_not_clicks() {
        assert_eq!(
            events(b"\x1b[<128;1;1M"),
            [Event::Mouse(MouseEvent {
                action: MouseAction::Press,
                button: None,
                mods: 0,
                x: 0,
                y: 0,
            })]
        );
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
