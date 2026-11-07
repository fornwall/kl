//! Whatever the parser produces has to survive the trip through libghostty-vt's
//! encoders, in whatever modes the program in the pane asked for.

#![no_main]

use kl_fuzz::ghostty::{MouseInput, Terminal};
use kl_fuzz::input::{Event, Parser};
use libfuzzer_sys::fuzz_target;

const COLS: u16 = 80;
const ROWS: u16 = 24;

fuzz_target!(|input: (&[u8], &[u8])| {
    let (setup, stream) = input;
    let Ok(mut terminal) = Terminal::new(COLS, ROWS) else {
        return;
    };
    terminal.write(setup);
    for event in Parser::default().feed(stream) {
        match event {
            Event::Key(key) => drop(terminal.encode_key(&key.as_input())),
            Event::Mouse(mouse) => {
                let input = MouseInput {
                    action: mouse.action,
                    button: mouse.button,
                    mods: mouse.mods,
                    x: mouse.x,
                    y: mouse.y,
                    cell_size: (1, 1),
                    any_pressed: mouse.button.is_some(),
                };
                drop(terminal.encode_mouse(&input, COLS, ROWS));
            }
            Event::Paste(text) => {
                drop(terminal.encode_paste(&text, false));
                drop(terminal.encode_paste(&text, true));
            }
            Event::Focus(focused) => drop(terminal.encode_focus(focused)),
            _ => {}
        }
    }
});
