//! The messages clients and the server exchange over the unix socket.
//!
//! Each message is a one byte tag, a little-endian `u32` payload length and the
//! payload. Client and server are always the same binary, but a server can
//! outlive an upgrade, so both opening messages carry [`VERSION`].

use crate::sys::WinSize;

/// Bumped on any change to the messages below.
pub const VERSION: u32 = 5;

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
const TAG_REATTACH: u8 = 8;

/// How a client comes to attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachMode {
    New,
    /// It was attached to the server that this one replaced, so that coming back
    /// is not something its user did.
    Reattach,
}

impl AttachMode {
    const fn word(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reattach => "reattach",
        }
    }

    /// Anything else is from a client of another version, which is not going to
    /// get any further than being told so.
    fn from_word(word: &str) -> Self {
        if word == Self::Reattach.word() {
            Self::Reattach
        } else {
            Self::New
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMessage {
    /// Makes this connection one of those showing the session.
    Attach {
        version: u32,
        size: WinSize,
        mode: AttachMode,
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
        /// The pane this was run from, which is what it is about by default.
        pane: Option<u32>,
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
    /// The server is being replaced, and the next one is already listening.
    Reattach,
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

pub fn size_bytes(size: WinSize) -> Vec<u8> {
    [size.cols, size.rows, size.width_px, size.height_px]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Reads a value off the front of a payload.
pub struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Malformed> {
        let (head, tail) = self.0.split_at_checked(len).ok_or(Malformed)?;
        self.0 = tail;
        Ok(head)
    }

    pub fn take_array<const LEN: usize>(&mut self) -> Result<[u8; LEN], Malformed> {
        let (head, tail) = self.0.split_first_chunk().ok_or(Malformed)?;
        self.0 = tail;
        Ok(*head)
    }

    fn take_u16(&mut self) -> Result<u16, Malformed> {
        self.take_array().map(u16::from_le_bytes)
    }

    pub fn take_u32(&mut self) -> Result<u32, Malformed> {
        self.take_array().map(u32::from_le_bytes)
    }

    pub fn take_bool(&mut self) -> Result<bool, Malformed> {
        self.take_array().map(|[byte]| byte != 0)
    }

    pub fn take_size(&mut self) -> Result<WinSize, Malformed> {
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

    pub fn take_bytes(&mut self) -> Result<&'a [u8], Malformed> {
        let len = usize::try_from(self.take_u32()?).ok().ok_or(Malformed)?;
        self.take(len)
    }

    pub fn take_text(&mut self) -> Result<String, Malformed> {
        let bytes = self.take_bytes()?;
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

    /// Texts from [`put_texts_counted`].
    pub fn take_texts_counted(&mut self) -> Result<Vec<String>, Malformed> {
        (0..self.take_u32()?).map(|_| self.take_text()).collect()
    }
}

pub fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
}

fn put_texts(out: &mut Vec<u8>, texts: &[String]) {
    for text in texts {
        put_bytes(out, text.as_bytes());
    }
}

/// Texts with what comes after them in the same buffer.
pub fn put_texts_counted(out: &mut Vec<u8>, texts: &[String]) {
    let count = u32::try_from(texts.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    put_texts(out, texts);
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
                mode,
                environment,
            } => {
                let mut payload = version.to_le_bytes().to_vec();
                payload.extend(size_bytes(*size));
                put_bytes(&mut payload, mode.word().as_bytes());
                put_texts(&mut payload, environment);
                put_frame(out, TAG_ATTACH, &payload);
            }
            Self::Input(bytes) => put_frame(out, TAG_INPUT, bytes),
            Self::Resize(size) => put_frame(out, TAG_RESIZE, &size_bytes(*size)),
            Self::Command {
                version,
                pane,
                args,
            } => {
                let mut payload = version.to_le_bytes().to_vec();
                // As one of the words, since this message has to stay readable
                // to servers of other versions.
                if let Some(pane) = pane {
                    put_bytes(&mut payload, format!("%{pane}").as_bytes());
                }
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
            TAG_ATTACH => {
                let version = reader.take_u32()?;
                let size = reader.take_size()?;
                // Texts all the way, which has been the case in every version,
                // so that any client gets as far as hearing that it does not fit.
                let mut texts = reader.take_texts()?.into_iter();
                let mode = texts.next().ok_or(Malformed)?;
                Self::Attach {
                    version,
                    size,
                    mode: AttachMode::from_word(&mode),
                    environment: texts.collect(),
                }
            }
            TAG_INPUT => Self::Input(reader.take_rest().to_vec()),
            TAG_RESIZE => Self::Resize(reader.take_size()?),
            TAG_COMMAND => {
                let version = reader.take_u32()?;
                let mut args = reader.take_texts()?;
                let first = args.first().and_then(|word| word.strip_prefix('%'));
                let pane = first.and_then(|pane| pane.parse().ok());
                if pane.is_some() {
                    args.remove(0);
                }
                Self::Command {
                    version,
                    pane,
                    args,
                }
            }
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
            Self::Reattach => put_frame(out, TAG_REATTACH, &[]),
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
            TAG_REATTACH => Self::Reattach,
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
            mode: AttachMode::Reattach,
            environment: vec!["DISPLAY=:0".to_owned(), "XAUTHORITY".to_owned()],
        });
        round_trip_client(&ClientMessage::Input(vec![0, 1, 255]));
        round_trip_client(&ClientMessage::Resize(size));
        round_trip_client(&ClientMessage::Command {
            version: 7,
            pane: None,
            args: vec!["split-window".to_owned(), "-h".to_owned(), String::new()],
        });
        round_trip_client(&ClientMessage::Command {
            version: 7,
            pane: Some(12),
            args: vec!["list-panes".to_owned()],
        });
        round_trip_server(&ServerMessage::Output(vec![27, 91]));
        round_trip_server(&ServerMessage::Exit("[detached]".to_owned()));
        round_trip_server(&ServerMessage::Result {
            success: false,
            text: "unknown command".to_owned(),
        });
        round_trip_server(&ServerMessage::Reattach);
    }

    /// What a client needs from a server of any version.
    #[test]
    fn a_command_without_a_pane_is_a_version_and_words() {
        let mut bytes = Vec::new();
        ClientMessage::Command {
            version: 7,
            pane: None,
            args: vec!["restart-server".to_owned()],
        }
        .encode(&mut bytes);
        assert_eq!(bytes, b"\x04\x16\0\0\0\x07\0\0\0\x0e\0\0\0restart-server");
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
