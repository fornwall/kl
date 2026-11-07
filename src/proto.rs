//! The messages clients and the server exchange over the unix socket.
//!
//! Each message is a one byte tag, a little-endian `u32` payload length and the
//! payload. Client and server are always the same binary, but a server can
//! outlive an upgrade, so both opening messages carry [`VERSION`].

use crate::sys::WinSize;

/// Bumped on any change to the messages below.
pub const VERSION: u32 = 2;

/// Larger than anything sent in practice, to catch a corrupt stream.
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

const HEADER_LEN: usize = 5;

const TAG_ATTACH: u8 = 1;
const TAG_INPUT: u8 = 2;
const TAG_RESIZE: u8 = 3;
const TAG_COMMAND: u8 = 4;
const TAG_OUTPUT: u8 = 5;
const TAG_EXIT: u8 = 6;
const TAG_RESULT: u8 = 7;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMessage {
    /// Makes this connection the one showing the session.
    Attach {
        version: u32,
        size: WinSize,
        /// What new panes should get of the client's environment: `NAME=value`
        /// to set a variable, and a bare `NAME` to remove it.
        environment: Vec<String>,
    },
    /// Bytes the terminal sent, to be parsed by the server.
    Input(Vec<u8>),
    Resize(WinSize),
    /// Runs one command and asks for its [`ServerMessage::Result`].
    Command {
        version: u32,
        args: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerMessage {
    /// Bytes for the terminal.
    Output(Vec<u8>),
    /// The client should restore the terminal, print this and quit.
    Exit(String),
    Result {
        success: bool,
        text: String,
    },
}

/// The stream is corrupt or from an incompatible version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("malformed message")
    }
}

impl std::error::Error for Malformed {}

fn put_frame(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    out.push(tag);
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
}

fn size_bytes(size: WinSize) -> Vec<u8> {
    [size.cols, size.rows, size.width_px, size.height_px]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Reads a value off the front of a payload.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], Malformed> {
        let (head, tail) = self.0.split_at_checked(len).ok_or(Malformed)?;
        self.0 = tail;
        Ok(head)
    }

    fn take_array<const LEN: usize>(&mut self) -> Result<[u8; LEN], Malformed> {
        let (head, tail) = self.0.split_first_chunk().ok_or(Malformed)?;
        self.0 = tail;
        Ok(*head)
    }

    fn take_u16(&mut self) -> Result<u16, Malformed> {
        self.take_array().map(u16::from_le_bytes)
    }

    fn take_u32(&mut self) -> Result<u32, Malformed> {
        self.take_array().map(u32::from_le_bytes)
    }

    fn take_size(&mut self) -> Result<WinSize, Malformed> {
        Ok(WinSize {
            cols: self.take_u16()?,
            rows: self.take_u16()?,
            width_px: self.take_u16()?,
            height_px: self.take_u16()?,
        })
    }

    fn take_rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn take_text(&mut self) -> Result<String, Malformed> {
        let len = usize::try_from(self.take_u32()?).ok().ok_or(Malformed)?;
        let bytes = self.take(len)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// The texts that make up the rest of a payload.
    fn take_texts(&mut self) -> Result<Vec<String>, Malformed> {
        let mut texts = Vec::new();
        while !self.0.is_empty() {
            texts.push(self.take_text()?);
        }
        Ok(texts)
    }
}

fn put_texts(out: &mut Vec<u8>, texts: &[String]) {
    for text in texts {
        let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
}

/// A frame's tag and payload, and how many bytes it took up.
type Frame<'a> = (u8, &'a [u8], usize);

/// Splits one complete frame off the front of `buffer`.
fn split_frame(buffer: &[u8]) -> Result<Option<Frame<'_>>, Malformed> {
    let Some((&[tag, a, b, c, d], rest)) = buffer.split_first_chunk::<HEADER_LEN>() else {
        return Ok(None);
    };
    let len = usize::try_from(u32::from_le_bytes([a, b, c, d]))
        .ok()
        .ok_or(Malformed)?;
    if len > MAX_PAYLOAD {
        return Err(Malformed);
    }
    Ok(rest
        .get(..len)
        .map(|payload| (tag, payload, HEADER_LEN + len)))
}

impl ClientMessage {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Attach {
                version,
                size,
                environment,
            } => {
                let mut payload = version.to_le_bytes().to_vec();
                payload.extend(size_bytes(*size));
                put_texts(&mut payload, environment);
                put_frame(out, TAG_ATTACH, &payload);
            }
            Self::Input(bytes) => put_frame(out, TAG_INPUT, bytes),
            Self::Resize(size) => put_frame(out, TAG_RESIZE, &size_bytes(*size)),
            Self::Command { version, args } => {
                let mut payload = version.to_le_bytes().to_vec();
                put_texts(&mut payload, args);
                put_frame(out, TAG_COMMAND, &payload);
            }
        }
    }

    /// Decodes the first message in `buffer`, returning how much of it was
    /// used, or `None` if more bytes are needed.
    pub fn decode(buffer: &[u8]) -> Result<Option<(Self, usize)>, Malformed> {
        let Some((tag, payload, used)) = split_frame(buffer)? else {
            return Ok(None);
        };
        let mut reader = Reader(payload);
        let message = match tag {
            TAG_ATTACH => Self::Attach {
                version: reader.take_u32()?,
                size: reader.take_size()?,
                environment: reader.take_texts()?,
            },
            TAG_INPUT => Self::Input(reader.take_rest().to_vec()),
            TAG_RESIZE => Self::Resize(reader.take_size()?),
            TAG_COMMAND => Self::Command {
                version: reader.take_u32()?,
                args: reader.take_texts()?,
            },
            _ => return Err(Malformed),
        };
        Ok(Some((message, used)))
    }
}

impl ServerMessage {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Output(bytes) => put_frame(out, TAG_OUTPUT, bytes),
            Self::Exit(text) => put_frame(out, TAG_EXIT, text.as_bytes()),
            Self::Result { success, text } => {
                let mut payload = vec![u8::from(*success)];
                payload.extend_from_slice(text.as_bytes());
                put_frame(out, TAG_RESULT, &payload);
            }
        }
    }

    /// Decodes the first message in `buffer`, returning how much of it was
    /// used, or `None` if more bytes are needed.
    pub fn decode(buffer: &[u8]) -> Result<Option<(Self, usize)>, Malformed> {
        let Some((tag, payload, used)) = split_frame(buffer)? else {
            return Ok(None);
        };
        let mut reader = Reader(payload);
        let message = match tag {
            TAG_OUTPUT => Self::Output(reader.take_rest().to_vec()),
            TAG_EXIT => Self::Exit(String::from_utf8_lossy(reader.take_rest()).into_owned()),
            TAG_RESULT => {
                let success = reader.take(1)? != [0];
                let text = String::from_utf8_lossy(reader.take_rest()).into_owned();
                Self::Result { success, text }
            }
            _ => return Err(Malformed),
        };
        Ok(Some((message, used)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_client(message: &ClientMessage) {
        let mut bytes = Vec::new();
        message.encode(&mut bytes);
        assert_eq!(
            ClientMessage::decode(&bytes).unwrap(),
            Some((message.clone(), bytes.len()))
        );
    }

    fn round_trip_server(message: &ServerMessage) {
        let mut bytes = Vec::new();
        message.encode(&mut bytes);
        assert_eq!(
            ServerMessage::decode(&bytes).unwrap(),
            Some((message.clone(), bytes.len()))
        );
    }

    #[test]
    fn messages_survive_a_round_trip() {
        let size = WinSize {
            cols: 80,
            rows: 24,
            width_px: 800,
            height_px: 480,
        };
        round_trip_client(&ClientMessage::Attach {
            version: 7,
            size,
            environment: vec!["DISPLAY=:0".to_owned(), "XAUTHORITY".to_owned()],
        });
        round_trip_client(&ClientMessage::Input(vec![0, 1, 255]));
        round_trip_client(&ClientMessage::Resize(size));
        round_trip_client(&ClientMessage::Command {
            version: 7,
            args: vec!["split-window".to_owned(), "-h".to_owned(), String::new()],
        });
        round_trip_server(&ServerMessage::Output(vec![27, 91]));
        round_trip_server(&ServerMessage::Exit("[detached]".to_owned()));
        round_trip_server(&ServerMessage::Result {
            success: false,
            text: "unknown command".to_owned(),
        });
    }

    #[test]
    fn waits_for_a_complete_frame() {
        let mut bytes = Vec::new();
        ClientMessage::Input(b"hello".to_vec()).encode(&mut bytes);
        for len in 0..bytes.len() {
            assert_eq!(ClientMessage::decode(&bytes[..len]).unwrap(), None);
        }
    }

    #[test]
    fn leaves_the_next_frame_in_the_buffer() {
        let mut bytes = Vec::new();
        ClientMessage::Input(b"a".to_vec()).encode(&mut bytes);
        let first = bytes.len();
        ClientMessage::Input(b"b".to_vec()).encode(&mut bytes);
        let (_, used) = ClientMessage::decode(&bytes).unwrap().unwrap();
        assert_eq!(used, first);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(ClientMessage::decode(&[99, 0, 0, 0, 0]), Err(Malformed));
        assert_eq!(ServerMessage::decode(&[1, 0, 0, 0, 0]), Err(Malformed));
        assert_eq!(
            ClientMessage::decode(&[TAG_INPUT, 255, 255, 255, 255]),
            Err(Malformed)
        );
        // A resize that is too short for its fields.
        assert_eq!(
            ClientMessage::decode(&[TAG_RESIZE, 1, 0, 0, 0, 9]),
            Err(Malformed)
        );
    }
}
