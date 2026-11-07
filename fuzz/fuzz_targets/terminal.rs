//! What programs write is the least trusted input kl has, and all of it goes
//! through the wrapper around libghostty-vt on its way to the screen.

#![no_main]

use kl_fuzz::ghostty::Terminal;
use libfuzzer_sys::fuzz_target;

const COLS: u16 = 40;
const ROWS: u16 = 12;

fn draw(terminal: &mut Terminal, all: bool, rows: u16) {
    terminal.refresh();
    let _ = terminal.cursor();
    terminal.draw_rows(all, |y, cells| {
        assert!(y < rows);
        for cell in cells {
            let _ = cell.text.as_str().len();
        }
    });
}

fuzz_target!(|input: (u16, u16, u16, u16, &[u8], &[u8])| {
    let (x, y, cols, rows, first, second) = input;
    let Ok(mut terminal) = Terminal::new(COLS, ROWS) else {
        return;
    };
    let (x, y) = (x % COLS, y % ROWS);

    terminal.write(first);
    draw(&mut terminal, true, ROWS);
    let _ = terminal.title();
    let _ = terminal.pwd();
    let _ = terminal.scrollbar();

    terminal.select_start(x, y);
    terminal.select_extend(y % COLS, x % ROWS);
    let _ = terminal.selected_text();
    for _ in 0..3 {
        // Once for a character, twice for a word and three times for a line.
        terminal.press_selection(x, y);
        terminal.drag_selection(y % COLS, x % ROWS, None);
        terminal.release_selection();
        let _ = terminal.selected_text();
    }

    terminal.scroll_by(-1);
    draw(&mut terminal, false, ROWS);
    terminal.scroll_to_top();
    draw(&mut terminal, false, ROWS);

    let (cols, rows) = (cols % 100 + 1, rows % 50 + 1);
    if terminal.resize(cols, rows, 1, 1).is_ok() {
        terminal.write(second);
        draw(&mut terminal, false, rows);
        let _ = terminal.selected_text();
    }
});
