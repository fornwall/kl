//! The quoting of the `:` prompt, and what the words of a command can do.

#![no_main]

use kl_fuzz::command::{key_bytes, parse, split_words};
use libfuzzer_sys::fuzz_target;

/// Quotes a word so that it comes back as it went in, one of three ways.
fn quote(word: &str, style: u8) -> String {
    let mut quoted = String::new();
    match style % 3 {
        0 if !word.contains('\'') => {
            quoted.push('\'');
            quoted.push_str(word);
            quoted.push('\'');
        }
        1 => {
            quoted.push('"');
            for c in word.chars() {
                if matches!(c, '"' | '\\') {
                    quoted.push('\\');
                }
                quoted.push(c);
            }
            quoted.push('"');
        }
        _ => {
            if word.is_empty() {
                quoted.push_str("''");
            }
            for c in word.chars() {
                quoted.push('\\');
                quoted.push(c);
            }
        }
    }
    quoted
}

fuzz_target!(|input: (&str, Vec<(String, u8)>)| {
    let (line, quotable) = input;
    if let Ok(words) = split_words(line) {
        let _ = parse(&words);
        for word in &words {
            let _ = key_bytes(word);
        }
    }

    let line: Vec<String> = quotable
        .iter()
        .map(|(word, style)| quote(word, *style))
        .collect();
    let words: Vec<String> = quotable.into_iter().map(|(word, _)| word).collect();
    assert_eq!(split_words(&line.join(" ")).as_ref(), Ok(&words));
    let _ = parse(&words);
});
