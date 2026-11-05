//! C interface for the shim (plan §2.2). Every function runs inside `catch_unwind`; a panic must
//! never unwind into FreeRDP. Buffers are copied at the boundary.
//!
//! Differences from the plan's draft: channels are plain `int`s (an out-of-range value from C
//! would be undefined behaviour in a Rust enum), `tr_host_api` has a `close` callback for the
//! panic path, and `tr_core_wants_channel` lets the shim skip listeners for experiment run A.
//! Messages are addressed by connection id, not channel: the host opens several `webrtc.1`
//! connections at once, and each has its own conversation.

#![allow(non_camel_case_types)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use rpc::{Channel, Config, ConnId, Core, Host, LogLevel, SessionInfo};

pub const TR_CH_WEBRTC: c_int = 1;
pub const TR_CH_CPC: c_int = 2;

/// Callbacks into the shim. They are called from the dispatcher thread and must be thread-safe.
#[repr(C)]
pub struct tr_host_api {
    pub user: *mut c_void,
    /// Sends one complete message on a connection. Returns 0 on success.
    pub write: Option<
        unsafe extern "C" fn(user: *mut c_void, conn: u32, data: *const u8, len: usize) -> c_int,
    >,
    /// Closes a connection.
    pub close: Option<unsafe extern "C" fn(user: *mut c_void, conn: u32)>,
    /// `level` uses wLog's numbering (0 trace … 4 error). `msg` is NUL-terminated.
    pub log: Option<unsafe extern "C" fn(user: *mut c_void, level: c_int, msg: *const c_char)>,
}

pub struct tr_core {
    core: Core,
}

struct CHost {
    api: tr_host_api,
}

// The shim guarantees the callbacks and `user` are usable from any thread.
unsafe impl Send for CHost {}
unsafe impl Sync for CHost {}

fn channel_from(ch: c_int) -> Option<Channel> {
    match ch {
        TR_CH_WEBRTC => Some(Channel::Webrtc),
        TR_CH_CPC => Some(Channel::Cpc),
        _ => None,
    }
}

impl Host for CHost {
    fn write(&self, conn: ConnId, data: &[u8]) -> bool {
        match self.api.write {
            // SAFETY: the shim supplied the callback with this contract.
            Some(f) => unsafe { f(self.api.user, conn, data.as_ptr(), data.len()) == 0 },
            None => false,
        }
    }

    fn close(&self, conn: ConnId) {
        if let Some(f) = self.api.close {
            // SAFETY: as above.
            unsafe { f(self.api.user, conn) }
        }
    }

    fn log(&self, level: LogLevel, msg: &str) {
        if let Some(f) = self.api.log {
            let text: Vec<u8> = msg
                .bytes()
                .filter(|&b| b != 0)
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: `text` is NUL-terminated and outlives the call.
            unsafe { f(self.api.user, level as c_int, text.as_ptr().cast()) }
        }
    }
}

unsafe fn c_str(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the caller passes a NUL-terminated string.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Creates the core. `session_json` is `{"display":{"width":W,"height":H},"client":{"name":..,"version":..}}`.
/// Returns NULL on failure.
///
/// # Safety
/// `api` must point to a valid `tr_host_api` (copied), `session_json` to a NUL-terminated string or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_core_new(
    api: *const tr_host_api,
    session_json: *const c_char,
) -> *mut tr_core {
    catch_unwind(AssertUnwindSafe(|| {
        if api.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: checked non-null; the struct is plain data.
        let api = unsafe { std::ptr::read(api) };
        let session = SessionInfo::from_json(&unsafe { c_str(session_json) });
        let core = Core::new(
            Arc::new(CHost { api }),
            Config::from_env(),
            session,
            engine(),
        );
        Box::into_raw(Box::new(tr_core { core }))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// The media engine: libwebrtc, or with `--no-default-features` the fake one, for builds that only
/// exercise the protocol and must not download libwebrtc.
#[cfg(feature = "libwebrtc")]
fn engine() -> Arc<dyn media::MediaEngine> {
    Arc::new(media::lk::LkEngine::new())
}

#[cfg(not(feature = "libwebrtc"))]
fn engine() -> Arc<dyn media::MediaEngine> {
    Arc::new(media::fake::FakeEngine::default())
}

/// Whether the shim should listen on `ch` (experiment run A and the off switch say no).
///
/// # Safety
/// `c` must come from `tr_core_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_core_wants_channel(c: *const tr_core, ch: c_int) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: per the contract above.
        let (Some(c), Some(ch)) = (unsafe { c.as_ref() }, channel_from(ch)) else {
            return 0;
        };
        c.core.wants_channel(ch) as c_int
    }))
    .unwrap_or(0)
}

/// A connection was accepted on channel `ch`. `conn` must be unique for the life of the core.
///
/// # Safety
/// `c` must come from `tr_core_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_core_channel_opened(c: *mut tr_core, ch: c_int, conn: u32) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let (Some(c), Some(ch)) = (unsafe { c.as_ref() }, channel_from(ch)) {
            c.core.channel_opened(conn, ch);
        }
    }));
}

/// Queues one complete message; the buffer is copied.
///
/// # Safety
/// `c` must come from `tr_core_new`; `data` must be valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_core_data(c: *mut tr_core, conn: u32, data: *const u8, len: usize) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if data.is_null() {
            return;
        }
        if let Some(c) = unsafe { c.as_ref() } {
            // SAFETY: per the contract above.
            c.core
                .data(conn, unsafe { std::slice::from_raw_parts(data, len) });
        }
    }));
}

/// # Safety
/// `c` must come from `tr_core_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_core_channel_closed(c: *mut tr_core, conn: u32) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(c) = unsafe { c.as_ref() } {
            c.core.channel_closed(conn);
        }
    }));
}

/// # Safety
/// `c` must come from `tr_core_new`; `session_json` a NUL-terminated string or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_core_session_changed(c: *mut tr_core, session_json: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(c) = unsafe { c.as_ref() } {
            c.core
                .session_changed(SessionInfo::from_json(&unsafe { c_str(session_json) }));
        }
    }));
}

/// Stops the dispatcher thread and frees the core. The shim must not hold a lock the host
/// callbacks take while calling this, since it waits for the thread to finish.
///
/// # Safety
/// `c` must come from `tr_core_new` and is invalid afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_core_free(c: *mut tr_core) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !c.is_null() {
            // SAFETY: ownership returns from the shim.
            drop(unsafe { Box::from_raw(c) });
        }
    }));
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::sync::Mutex;

    use super::*;

    static WRITES: Mutex<Vec<(u32, Vec<u8>)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn write(_: *mut c_void, conn: u32, data: *const u8, len: usize) -> c_int {
        WRITES.lock().unwrap().push((
            conn,
            unsafe { std::slice::from_raw_parts(data, len) }.to_vec(),
        ));
        0
    }

    #[test]
    fn round_trip_through_the_c_interface() {
        let api = tr_host_api {
            user: std::ptr::null_mut(),
            write: Some(write),
            close: None,
            log: None,
        };
        let session = CString::new(r#"{"display":{"width":800,"height":600}}"#).unwrap();
        unsafe {
            let c = tr_core_new(&api, session.as_ptr());
            assert!(!c.is_null());
            assert_eq!(tr_core_wants_channel(c, TR_CH_WEBRTC), 1);
            assert_eq!(tr_core_wants_channel(c, 99), 0);
            tr_core_channel_opened(c, TR_CH_CPC, 7);
            tr_core_channel_opened(c, 99, 8); // ignored, not UB
            let ping = br#"{"contentType":"scpie","messageName":"Ping"}"#;
            tr_core_data(c, 7, ping.as_ptr(), ping.len());
            tr_core_data(c, 8, ping.as_ptr(), ping.len()); // unknown connection: dropped
            tr_core_free(c); // drains the queue before returning
        }
        let w = WRITES.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, 7);
        assert_eq!(w[0].1, br#"{"contentType":"scpie","messageName":"Pong"}"#);
    }
}
