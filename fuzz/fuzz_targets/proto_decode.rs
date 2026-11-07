//! Both decoders must survive anything a socket can deliver, and what they do
//! accept must make sense.

#![no_main]

use kl_fuzz::proto::{ClientMessage, Malformed, ServerMessage};
use libfuzzer_sys::fuzz_target;

/// What the two message types have in common.
trait Message: Clone + PartialEq + std::fmt::Debug {
    fn decode(buffer: &[u8]) -> Result<Option<(Self, usize)>, Malformed>;
    fn encode(&self, out: &mut Vec<u8>);
}

impl Message for ClientMessage {
    fn decode(buffer: &[u8]) -> Result<Option<(Self, usize)>, Malformed> {
        Self::decode(buffer)
    }
    fn encode(&self, out: &mut Vec<u8>) {
        self.encode(out);
    }
}

impl Message for ServerMessage {
    fn decode(buffer: &[u8]) -> Result<Option<(Self, usize)>, Malformed> {
        Self::decode(buffer)
    }
    fn encode(&self, out: &mut Vec<u8>) {
        self.encode(out);
    }
}

/// Decodes a stream the way the receiving ends do.
fn check_stream<M: Message>(mut stream: &[u8]) {
    while let Ok(Some((message, used))) = M::decode(stream) {
        assert!((5..=stream.len()).contains(&used));
        let frame = &stream[..used];

        // A frame stands by itself, and is not complete before its last byte.
        assert_eq!(M::decode(frame), Ok(Some((message.clone(), used))));
        assert_eq!(M::decode(&frame[..used - 1]), Ok(None));

        // What gets decoded can be sent again. The frame may differ, as text is
        // decoded lossily and some messages ignore bytes at their end.
        let mut encoded = Vec::new();
        message.encode(&mut encoded);
        let len = encoded.len();
        assert_eq!(M::decode(&encoded), Ok(Some((message, len))));

        stream = &stream[used..];
    }
}

fuzz_target!(|data: &[u8]| {
    check_stream::<ClientMessage>(data);
    check_stream::<ServerMessage>(data);
});
