//! `GhosttyKey` values and modifier bits, and how the kitty keyboard protocol's
//! key codes map onto them.

use std::ffi::c_int;

pub const MOD_SHIFT: u16 = 1 << 0;
pub const MOD_CTRL: u16 = 1 << 1;
pub const MOD_ALT: u16 = 1 << 2;
pub const MOD_SUPER: u16 = 1 << 3;
pub const MOD_CAPS_LOCK: u16 = 1 << 4;
pub const MOD_NUM_LOCK: u16 = 1 << 5;

pub const UNIDENTIFIED: c_int = 0;
const BACKQUOTE: c_int = 1;
const BACKSLASH: c_int = 2;
const BRACKET_LEFT: c_int = 3;
const BRACKET_RIGHT: c_int = 4;
const COMMA: c_int = 5;
const DIGIT_0: c_int = 6;
const EQUAL: c_int = 16;
const LETTER_A: c_int = 20;
const MINUS: c_int = 46;
const PERIOD: c_int = 47;
const QUOTE: c_int = 48;
const SEMICOLON: c_int = 49;
const SLASH: c_int = 50;
const ALT_LEFT: c_int = 51;
const ALT_RIGHT: c_int = 52;
pub const BACKSPACE: c_int = 53;
const CAPS_LOCK: c_int = 54;
const CONTEXT_MENU: c_int = 55;
const CONTROL_LEFT: c_int = 56;
const CONTROL_RIGHT: c_int = 57;
pub const ENTER: c_int = 58;
const META_LEFT: c_int = 59;
const META_RIGHT: c_int = 60;
const SHIFT_LEFT: c_int = 61;
const SHIFT_RIGHT: c_int = 62;
pub const SPACE: c_int = 63;
pub const TAB: c_int = 64;
pub const DELETE: c_int = 68;
pub const END: c_int = 69;
pub const HOME: c_int = 71;
pub const INSERT: c_int = 72;
pub const PAGE_DOWN: c_int = 73;
pub const PAGE_UP: c_int = 74;
pub const ARROW_DOWN: c_int = 75;
pub const ARROW_LEFT: c_int = 76;
pub const ARROW_RIGHT: c_int = 77;
pub const ARROW_UP: c_int = 78;
const NUM_LOCK: c_int = 79;
const NUMPAD_0: c_int = 80;
const NUMPAD_ADD: c_int = 90;
const NUMPAD_DECIMAL: c_int = 95;
const NUMPAD_DIVIDE: c_int = 96;
const NUMPAD_ENTER: c_int = 97;
const NUMPAD_EQUAL: c_int = 98;
const NUMPAD_MULTIPLY: c_int = 104;
const NUMPAD_SUBTRACT: c_int = 107;
const NUMPAD_SEPARATOR: c_int = 108;
const NUMPAD_UP: c_int = 109;
const NUMPAD_DOWN: c_int = 110;
const NUMPAD_RIGHT: c_int = 111;
const NUMPAD_LEFT: c_int = 112;
const NUMPAD_BEGIN: c_int = 113;
const NUMPAD_HOME: c_int = 114;
const NUMPAD_END: c_int = 115;
const NUMPAD_INSERT: c_int = 116;
const NUMPAD_DELETE: c_int = 117;
const NUMPAD_PAGE_UP: c_int = 118;
const NUMPAD_PAGE_DOWN: c_int = 119;
pub const ESCAPE: c_int = 120;
pub const F1: c_int = 121;
const PRINT_SCREEN: c_int = 148;
const SCROLL_LOCK: c_int = 149;
const PAUSE: c_int = 150;

/// The key whose unshifted character is `codepoint` on a US layout.
#[must_use]
pub fn from_codepoint(codepoint: u32) -> c_int {
    let Some(c) = char::from_u32(codepoint) else {
        return UNIDENTIFIED;
    };
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => LETTER_A + c_int::from(c as u8 - b'a'),
        c @ '0'..='9' => DIGIT_0 + c_int::from(c as u8 - b'0'),
        '`' => BACKQUOTE,
        '\\' => BACKSLASH,
        '[' => BRACKET_LEFT,
        ']' => BRACKET_RIGHT,
        ',' => COMMA,
        '=' => EQUAL,
        '-' => MINUS,
        '.' => PERIOD,
        '\'' => QUOTE,
        ';' => SEMICOLON,
        '/' => SLASH,
        ' ' => SPACE,
        _ => UNIDENTIFIED,
    }
}

/// The key that a `CSI code u` sequence names. `None` means the code stands for
/// a character, which [`from_codepoint`] handles.
#[must_use]
pub const fn from_kitty_code(code: u32) -> Option<c_int> {
    Some(match code {
        9 => TAB,
        13 => ENTER,
        27 => ESCAPE,
        127 => BACKSPACE,
        57358 => CAPS_LOCK,
        57359 => SCROLL_LOCK,
        57360 => NUM_LOCK,
        57361 => PRINT_SCREEN,
        57362 => PAUSE,
        57363 => CONTEXT_MENU,
        // F13 to F25, which are consecutive in both numberings. Kitty goes on
        // to F35, which have no GhosttyKey.
        57376..=57388 => F1 + 12 + (code - 57376).cast_signed(),
        57399..=57408 => NUMPAD_0 + (code - 57399).cast_signed(),
        57409 => NUMPAD_DECIMAL,
        57410 => NUMPAD_DIVIDE,
        57411 => NUMPAD_MULTIPLY,
        57412 => NUMPAD_SUBTRACT,
        57413 => NUMPAD_ADD,
        57414 => NUMPAD_ENTER,
        57415 => NUMPAD_EQUAL,
        57416 => NUMPAD_SEPARATOR,
        57417 => NUMPAD_LEFT,
        57418 => NUMPAD_RIGHT,
        57419 => NUMPAD_UP,
        57420 => NUMPAD_DOWN,
        57421 => NUMPAD_PAGE_UP,
        57422 => NUMPAD_PAGE_DOWN,
        57423 => NUMPAD_HOME,
        57424 => NUMPAD_END,
        57425 => NUMPAD_INSERT,
        57426 => NUMPAD_DELETE,
        57427 => NUMPAD_BEGIN,
        57441 => SHIFT_LEFT,
        57442 => CONTROL_LEFT,
        57443 => ALT_LEFT,
        57444 | 57446 => META_LEFT,
        57447 => SHIFT_RIGHT,
        57448 => CONTROL_RIGHT,
        57449 => ALT_RIGHT,
        57450 | 57452 => META_RIGHT,
        _ => return None,
    })
}

/// The key of a `CSI number ~` sequence.
#[must_use]
pub const fn from_tilde_code(code: u32) -> Option<c_int> {
    Some(match code {
        1 | 7 => HOME,
        2 => INSERT,
        3 => DELETE,
        4 | 8 => END,
        5 => PAGE_UP,
        6 => PAGE_DOWN,
        11..=15 => F1 + (code - 11).cast_signed(),
        17..=21 => F1 + 5 + (code - 17).cast_signed(),
        23 | 24 => F1 + 10 + (code - 23).cast_signed(),
        _ => return None,
    })
}

/// The key of a `CSI 1 ; mods letter` or `SS3 letter` sequence.
#[must_use]
pub const fn from_final_letter(letter: u8) -> Option<c_int> {
    Some(match letter {
        b'A' => ARROW_UP,
        b'B' => ARROW_DOWN,
        b'C' => ARROW_RIGHT,
        b'D' => ARROW_LEFT,
        b'F' => END,
        b'H' => HOME,
        b'P' => F1,
        b'Q' => F1 + 1,
        b'R' => F1 + 2,
        b'S' => F1 + 3,
        _ => return None,
    })
}

/// Whether this is a modifier key on its own, which the kitty protocol reports
/// when asked to report all keys.
#[must_use]
pub const fn is_modifier(key: c_int) -> bool {
    matches!(
        key,
        ALT_LEFT
            | ALT_RIGHT
            | CONTROL_LEFT
            | CONTROL_RIGHT
            | META_LEFT
            | META_RIGHT
            | SHIFT_LEFT
            | SHIFT_RIGHT
            | CAPS_LOCK
            | NUM_LOCK
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_letters_and_digits() {
        assert_eq!(from_codepoint(u32::from('a')), 20);
        assert_eq!(from_codepoint(u32::from('Z')), 45);
        assert_eq!(from_codepoint(u32::from('9')), 15);
        assert_eq!(from_codepoint(u32::from('\u{e9}')), UNIDENTIFIED);
    }

    #[test]
    fn maps_function_keys_across_their_gaps() {
        assert_eq!(from_final_letter(b'P'), Some(F1));
        assert_eq!(from_tilde_code(15), Some(F1 + 4));
        assert_eq!(from_tilde_code(17), Some(F1 + 5));
        assert_eq!(from_tilde_code(24), Some(F1 + 11));
        assert_eq!(from_kitty_code(57376), Some(F1 + 12));
        assert_eq!(from_kitty_code(57388), Some(145));
    }

    #[test]
    fn leaves_character_codes_alone() {
        assert_eq!(from_kitty_code(u32::from('a')), None);
        assert_eq!(from_kitty_code(57441), Some(SHIFT_LEFT));
        assert!(is_modifier(SHIFT_LEFT));
        assert!(!is_modifier(ENTER));
    }
}
