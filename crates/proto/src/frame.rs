//! One reassembled DVC message is one JSON text, with no header and no length prefix
//! (mac-decompile §3.5-3.6, win-findings §4) [O]. FreeRDP's DVC layer reassembles
//! DATA_FIRST/DATA fragments before the callback [I].

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    Empty,
    NotUtf8,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Empty => f.write_str("empty message"),
            FrameError::NotUtf8 => f.write_str("message is not valid UTF-8"),
        }
    }
}

impl std::error::Error for FrameError {}

fn text(data: &[u8]) -> Result<&str, FrameError> {
    if data.is_empty() {
        return Err(FrameError::Empty);
    }
    std::str::from_utf8(data).map_err(|_| FrameError::NotUtf8)
}

/// Inbound `webrtc.1`: a trailing NUL is optional, and the text is cut at the first NUL.
/// Windows requires the NUL; macOS accepts both. We accept both.
pub fn webrtc_inbound(data: &[u8]) -> Result<&str, FrameError> {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    text(&data[..end])
}

/// Outbound `webrtc.1`: one JSON text with a trailing NUL, the stricter Windows convention.
pub fn webrtc_outbound(json: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(json.len() + 1);
    out.extend_from_slice(json.as_bytes());
    out.push(0);
    out
}

/// Inbound `cpc.1`: bare JSON. A trailing NUL is tolerated.
pub fn cpc_inbound(data: &[u8]) -> Result<&str, FrameError> {
    webrtc_inbound(data)
}

/// Outbound `cpc.1`: bare JSON, passed to `SendAsync` unchanged on macOS [O].
pub fn cpc_outbound(json: &str) -> Vec<u8> {
    json.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_accepts_nul_or_none_and_cuts_at_first_nul() {
        assert_eq!(webrtc_inbound(b"{\"a\":1}"), Ok("{\"a\":1}"));
        assert_eq!(webrtc_inbound(b"{\"a\":1}\0"), Ok("{\"a\":1}"));
        assert_eq!(webrtc_inbound(b"{\"a\":1}\0garbage"), Ok("{\"a\":1}"));
    }

    #[test]
    fn inbound_rejects_empty_and_invalid_utf8() {
        assert_eq!(webrtc_inbound(b""), Err(FrameError::Empty));
        assert_eq!(webrtc_inbound(b"\0"), Err(FrameError::Empty));
        assert_eq!(webrtc_inbound(&[0xff, 0xfe]), Err(FrameError::NotUtf8));
    }

    #[test]
    fn outbound_framing() {
        assert_eq!(webrtc_outbound("{}"), b"{}\0");
        assert_eq!(cpc_outbound("{}"), b"{}");
    }
}
