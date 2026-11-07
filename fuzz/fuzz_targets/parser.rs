//! `Parser::feed` must survive anything, and how a stream is cut into reads must
//! not change what comes out of it.

#![no_main]

use kl_fuzz::ghostty::key::ARROW_UP;
use kl_fuzz::input::{Event, Parser};
use libfuzzer_sys::fuzz_target;

/// Finishes whatever is unfinished, so that what the parser held back shows up
/// in the events too, and ends with a key that must always get through.
const FINISH: &[u8] = b"\x07\x1b\\\x1b[201~\x1b[A";

/// Exercises what the server does with an event short of acting on it.
fn inspect(events: &[Event]) {
    for event in events {
        if let Event::Key(key) = event {
            let input = key.as_input();
            assert_eq!(input.consumed_mods & !input.mods, 0);
            assert!(!key.is_ctrl('\0') || key.unshifted == 0);
            let _ = key.binding_mods();
            let _ = key.char();
        }
    }
}

fn feed_in_chunks<'a>(chunks: impl Iterator<Item = &'a [u8]>) -> (Vec<Event>, bool) {
    let mut parser = Parser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.feed(chunk));
    }
    let escape = parser.has_pending_escape();
    assert_eq!(parser.flush_escape().is_some(), escape);
    events.extend(parser.feed(FINISH));
    assert!(matches!(events.last(), Some(Event::Key(key)) if key.key == ARROW_UP));
    (events, escape)
}

fuzz_target!(|input: (Vec<u16>, &[u8])| {
    let (cuts, stream) = input;
    let whole = feed_in_chunks(std::iter::once(stream));
    inspect(&whole.0);

    assert_eq!(feed_in_chunks(stream.chunks(1)), whole, "byte by byte");

    let mut cuts: Vec<usize> = cuts
        .into_iter()
        .map(|cut| usize::from(cut).min(stream.len()))
        .collect();
    cuts.sort_unstable();
    let ends = cuts.iter().copied().chain([stream.len()]);
    let starts = std::iter::once(0).chain(cuts.iter().copied());
    let chunks = starts.zip(ends).map(|(start, end)| &stream[start..end]);
    assert_eq!(feed_in_chunks(chunks), whole, "cut at {cuts:?}");
});
