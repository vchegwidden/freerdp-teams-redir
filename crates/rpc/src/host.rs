/// The two DVCs the plugin listens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Webrtc,
    Cpc,
}

impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Channel::Webrtc => "webrtc",
            Channel::Cpc => "cpc",
        }
    }
}

/// Numbered to match wLog's levels (`WLOG_TRACE` = 0 … `WLOG_ERROR` = 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

/// One accepted DVC connection. The shim numbers them; the host may open several on the same
/// channel name at once (three `webrtc.1` connections within 3 ms were seen).
pub type ConnId = u32;

/// What the core needs from the process it runs in. Called from the dispatcher thread, so the
/// implementation must be thread-safe.
pub trait Host: Send + Sync + 'static {
    /// Sends one complete message on a connection. Returns false if it could not be sent.
    fn write(&self, conn: ConnId, data: &[u8]) -> bool;
    /// Closes a connection (used when the core disables itself after a panic).
    fn close(&self, conn: ConnId);
    fn log(&self, level: LogLevel, msg: &str);
}
