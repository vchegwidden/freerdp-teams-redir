//! The byte stream between the add-in and `teams-redir-video`, the helper that owns the video
//! windows. A separate process keeps a graphics stack out of the RDP client, and works with any
//! FreeRDP client on X11 or Wayland.
//!
//! The stream starts with [`MAGIC`] and then holds messages: `[type: u8][window: u32 le][length:
//! u32 le][payload]`. A window is an id the add-in chooses (the `MediaElement` id); the helper
//! opens one on its first frame. Frames are I420: `[width: u32][height: u32]` and then the Y, U and V
//! planes, tightly packed, chroma at half the size rounded up.

use std::io::{self, Read};

pub const MAGIC: [u8; 4] = *b"TRV1";

/// Larger than any frame this carries (8K I420 is 50 MB).
const MAX_PAYLOAD: u32 = 64 * 1024 * 1024;
const MAX_SIDE: u32 = 16384;

/// How a picture fills a window of another shape, as CSS `object-fit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Stretch to the window.
    Fill = 0,
    /// All of the picture, with bars where the shapes differ.
    Contain = 1,
    /// The window full, with the picture cropped.
    Cover = 2,
}

impl Fit {
    fn from_u8(n: u8) -> Option<Fit> {
        Some(match n {
            0 => Fit::Fill,
            1 => Fit::Contain,
            2 => Fit::Cover,
            _ => return None,
        })
    }

    /// The CSS name Teams uses (`fill`, `contain`, `cover`, `scale-down`).
    pub fn from_css(name: &str) -> Option<Fit> {
        Some(match name {
            "fill" => Fit::Fill,
            "contain" | "scale-down" => Fit::Contain,
            "cover" => Fit::Cover,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Frame {
        window: u32,
        width: u32,
        height: u32,
        /// Y, then U, then V, tightly packed.
        planes: Vec<u8>,
    },
    /// Close and forget the window.
    Close {
        window: u32,
    },
    Visible {
        window: u32,
        visible: bool,
    },
    /// Flip the picture left to right (the self view is mirrored).
    Mirror {
        window: u32,
        mirrored: bool,
    },
    Fit {
        window: u32,
        fit: Fit,
    },
    Title {
        window: u32,
        title: String,
    },
}

const FRAME: u8 = 1;
const CLOSE: u8 = 2;
const VISIBLE: u8 = 3;
const MIRROR: u8 = 4;
const FIT: u8 = 5;
const TITLE: u8 = 6;

/// Bytes of one I420 plane set.
pub fn frame_len(width: u32, height: u32) -> usize {
    let (w, h) = (width as usize, height as usize);
    w * h + 2 * (w.div_ceil(2) * h.div_ceil(2))
}

impl Message {
    /// A frame from three separate planes.
    pub fn frame(window: u32, width: u32, height: u32, y: &[u8], u: &[u8], v: &[u8]) -> Message {
        let mut planes = Vec::with_capacity(y.len() + u.len() + v.len());
        planes.extend_from_slice(y);
        planes.extend_from_slice(u);
        planes.extend_from_slice(v);
        Message::Frame {
            window,
            width,
            height,
            planes,
        }
    }

    pub fn window(&self) -> u32 {
        match self {
            Message::Frame { window, .. }
            | Message::Close { window }
            | Message::Visible { window, .. }
            | Message::Mirror { window, .. }
            | Message::Fit { window, .. }
            | Message::Title { window, .. } => *window,
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        let (kind, payload): (u8, Vec<u8>) = match self {
            Message::Frame {
                width,
                height,
                planes,
                ..
            } => {
                let mut p = Vec::with_capacity(8 + planes.len());
                p.extend_from_slice(&width.to_le_bytes());
                p.extend_from_slice(&height.to_le_bytes());
                p.extend_from_slice(planes);
                (FRAME, p)
            }
            Message::Close { .. } => (CLOSE, Vec::new()),
            Message::Visible { visible, .. } => (VISIBLE, vec![u8::from(*visible)]),
            Message::Mirror { mirrored, .. } => (MIRROR, vec![u8::from(*mirrored)]),
            Message::Fit { fit, .. } => (FIT, vec![*fit as u8]),
            Message::Title { title, .. } => (TITLE, title.as_bytes().to_vec()),
        };
        out.push(kind);
        out.extend_from_slice(&self.window().to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
    }
}

/// Reads the next message. `Ok(None)` is a clean end of stream between messages; a stream cut in
/// the middle of one, or a message that makes no sense, is an error.
pub fn read_message<R: Read>(r: &mut R) -> io::Result<Option<Message>> {
    let mut head = [0u8; 9];
    match r.read(&mut head[..1])? {
        0 => return Ok(None),
        _ => r.read_exact(&mut head[1..])?,
    }
    let window = u32::from_le_bytes(head[1..5].try_into().unwrap());
    let len = u32::from_le_bytes(head[5..9].try_into().unwrap());
    if len > MAX_PAYLOAD {
        return Err(bad("message too large"));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    let flag = |p: &[u8]| -> io::Result<bool> {
        match p {
            [b] if *b <= 1 => Ok(*b == 1),
            _ => Err(bad("bad flag")),
        }
    };
    Ok(Some(match head[0] {
        FRAME => {
            if payload.len() < 8 {
                return Err(bad("short frame"));
            }
            let width = u32::from_le_bytes(payload[0..4].try_into().unwrap());
            let height = u32::from_le_bytes(payload[4..8].try_into().unwrap());
            if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
                return Err(bad("bad frame size"));
            }
            if payload.len() - 8 != frame_len(width, height) {
                return Err(bad("frame length does not match its size"));
            }
            Message::Frame {
                window,
                width,
                height,
                planes: payload[8..].to_vec(),
            }
        }
        CLOSE => Message::Close { window },
        VISIBLE => Message::Visible {
            window,
            visible: flag(&payload)?,
        },
        MIRROR => Message::Mirror {
            window,
            mirrored: flag(&payload)?,
        },
        FIT => Message::Fit {
            window,
            fit: payload
                .first()
                .and_then(|b| Fit::from_u8(*b))
                .ok_or_else(|| bad("bad fit"))?,
        },
        TITLE => Message::Title {
            window,
            title: String::from_utf8(payload).map_err(|_| bad("title is not UTF-8"))?,
        },
        _ => return Err(bad("unknown message type")),
    }))
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: &Message) -> Message {
        let mut bytes = Vec::new();
        m.encode(&mut bytes);
        read_message(&mut bytes.as_slice()).unwrap().unwrap()
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        let (y, u, v) = (vec![1u8; 4 * 2], vec![2u8; 2], vec![3u8; 2]);
        for m in [
            Message::frame(7, 4, 2, &y, &u, &v),
            Message::Close { window: 7 },
            Message::Visible {
                window: 7,
                visible: false,
            },
            Message::Mirror {
                window: 7,
                mirrored: true,
            },
            Message::Fit {
                window: 7,
                fit: Fit::Cover,
            },
            Message::Title {
                window: 7,
                title: "Teams: you".into(),
            },
        ] {
            assert_eq!(roundtrip(&m), m);
        }
    }

    #[test]
    fn odd_sizes_round_chroma_up() {
        assert_eq!(frame_len(4, 2), 8 + 2 + 2);
        assert_eq!(frame_len(5, 3), 15 + 2 * (3 * 2));
        let planes = vec![0u8; frame_len(5, 3)];
        let m = Message::Frame {
            window: 1,
            width: 5,
            height: 3,
            planes,
        };
        assert_eq!(roundtrip(&m), m);
    }

    #[test]
    fn a_stream_is_read_message_by_message_until_it_ends() {
        let mut bytes = Vec::new();
        Message::Close { window: 1 }.encode(&mut bytes);
        Message::Close { window: 2 }.encode(&mut bytes);
        let mut r = bytes.as_slice();
        assert_eq!(
            read_message(&mut r).unwrap(),
            Some(Message::Close { window: 1 })
        );
        assert_eq!(
            read_message(&mut r).unwrap(),
            Some(Message::Close { window: 2 })
        );
        assert_eq!(read_message(&mut r).unwrap(), None);
    }

    #[test]
    fn damaged_streams_are_errors() {
        let mut ok = Vec::new();
        Message::Title {
            window: 1,
            title: "x".into(),
        }
        .encode(&mut ok);
        // Cut in the middle of the payload.
        assert!(read_message(&mut &ok[..ok.len() - 1]).is_err());
        // Cut in the middle of the header.
        assert!(read_message(&mut &ok[..4]).is_err());
        let mut unknown = ok.clone();
        unknown[0] = 99;
        assert!(read_message(&mut unknown.as_slice()).is_err());
        // A frame whose planes do not match its size.
        let mut frame = Vec::new();
        Message::Frame {
            window: 1,
            width: 4,
            height: 2,
            planes: vec![0; 5],
        }
        .encode(&mut frame);
        assert!(read_message(&mut frame.as_slice()).is_err());
        // A huge declared length is refused before anything is allocated.
        let huge = [FRAME, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert!(read_message(&mut huge.as_slice()).is_err());
    }

    #[test]
    fn css_names() {
        assert_eq!(Fit::from_css("scale-down"), Some(Fit::Contain));
        assert_eq!(Fit::from_css("cover"), Some(Fit::Cover));
        assert_eq!(Fit::from_css("bogus"), None);
    }
}
