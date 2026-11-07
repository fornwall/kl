//! Every message survives encoding and decoding, whatever follows it, and
//! however little of it has arrived yet.

#![no_main]

use kl_fuzz::proto::{AttachMode, ClientInfo, ClientMessage, ServerMessage};
use kl_fuzz::sys::WinSize;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (u8, u32, [u16; 4], Vec<String>, Vec<u8>)| {
    let (choice, version, size, words, bytes) = input;
    let text = words.first().cloned().unwrap_or_default();
    let [cols, rows, width_px, height_px] = size;
    let size = WinSize {
        cols,
        rows,
        width_px,
        height_px,
    };

    let client = match choice % 4 {
        0 => ClientMessage::Attach {
            version,
            size,
            mode: match choice >> 6 {
                0 => AttachMode::New,
                1 => AttachMode::Alone,
                _ => AttachMode::Reattach,
            },
            info: ClientInfo {
                pid: version,
                tty: text.clone(),
                term: text.clone(),
            },
            environment: words,
        },
        1 => ClientMessage::Input(bytes.clone()),
        2 => ClientMessage::Resize(size),
        _ => ClientMessage::Command {
            version,
            pane: Some(version),
            args: words,
        },
    };
    let mut encoded = Vec::new();
    client.encode(&mut encoded);
    let len = encoded.len();
    for prefix in 0..len {
        assert_eq!(ClientMessage::decode(&encoded[..prefix]), Ok(None));
    }
    encoded.extend_from_slice(&bytes);
    assert_eq!(ClientMessage::decode(&encoded), Ok(Some((client, len))));

    let server = match choice % 3 {
        0 => ServerMessage::Output(bytes.clone()),
        1 => ServerMessage::Exit(text),
        _ => ServerMessage::Result {
            success: choice & 0x80 != 0,
            text,
        },
    };
    let mut encoded = Vec::new();
    server.encode(&mut encoded);
    let len = encoded.len();
    for prefix in 0..len {
        assert_eq!(ServerMessage::decode(&encoded[..prefix]), Ok(None));
    }
    encoded.extend_from_slice(&bytes);
    assert_eq!(ServerMessage::decode(&encoded), Ok(Some((server, len))));
});
