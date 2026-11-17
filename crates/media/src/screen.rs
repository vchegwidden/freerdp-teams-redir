//! Screen capture for screen sharing, through libwebrtc's desktop capturer.
//!
//! On Wayland that capturer is the PipeWire one: when it starts it asks xdg-desktop-portal for a
//! ScreenCast session, and the portal shows the desktop's own picker (monitor or window). On X11
//! libwebrtc captures the screen directly. Either way nothing here lists screens; the user picks
//! in the system dialog.
//!
//! The portal is spoken to with GDBus, whose replies are dispatched on the GLib main context that
//! was the thread default when the request was made. Nothing in libwebrtc runs a GLib loop (a
//! browser's own message pump does), so every capture gets a thread of its own that owns a
//! private context, pumps it, and asks for a frame at the wanted rate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libwebrtc::desktop_capturer::{
    CaptureError, DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions, DesktopFrame,
};
use libwebrtc::native::yuv_helper;
use libwebrtc::video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation};
use libwebrtc::video_source::native::NativeVideoSource;

use crate::{VideoConstraints, VideoSink};

/// The largest frame sent when the request names no size: screens are scaled down to fit.
const DEFAULT_MAX: (u32, u32) = (1920, 1080);
/// Screen content changes slowly; Teams asks for its own rate when it cares.
const DEFAULT_FPS: f64 = 15.0;

#[allow(non_camel_case_types)]
type GMainContext = std::ffi::c_void;

// libwebrtc's portal code calls GIO (GDBus) and GObject but its build links neither; the add-in's
// CMake links `gio-2.0` too, for the same reason.
#[link(name = "gio-2.0")]
#[link(name = "gobject-2.0")]
#[link(name = "glib-2.0")]
unsafe extern "C" {
    fn g_main_context_new() -> *mut GMainContext;
    fn g_main_context_push_thread_default(context: *mut GMainContext);
    fn g_main_context_pop_thread_default(context: *mut GMainContext);
    fn g_main_context_iteration(context: *mut GMainContext, may_block: i32) -> i32;
    fn g_main_context_unref(context: *mut GMainContext);
}

/// A GLib main context that is this thread's default for as long as it lives.
struct ThreadContext(*mut GMainContext);

impl ThreadContext {
    fn new() -> ThreadContext {
        // SAFETY: plain GLib calls; the context is popped and released in `drop` on this thread.
        unsafe {
            let ctx = g_main_context_new();
            g_main_context_push_thread_default(ctx);
            ThreadContext(ctx)
        }
    }

    /// Runs whatever is ready, without waiting.
    fn pump(&self) {
        // SAFETY: the context is valid and owned by this thread.
        unsafe { while g_main_context_iteration(self.0, 0) != 0 {} }
    }
}

impl Drop for ThreadContext {
    fn drop(&mut self) {
        self.pump();
        // SAFETY: pushed in `new` on this same thread.
        unsafe {
            g_main_context_pop_thread_default(self.0);
            g_main_context_unref(self.0);
        }
    }
}

/// The one screen capture of the process, shared by every screen track. Teams makes several
/// screen streams for one share (previews and the one it sends, seen 2026-11-17), and each would
/// otherwise open its own portal dialog. The capture runs while any track on it is enabled and not
/// stopped; once none is, it ends, and the next share asks the portal again.
#[derive(Default)]
pub struct ScreenHub {
    outputs: Arc<Mutex<Vec<Output>>>,
    state: Mutex<HubState>,
}

#[derive(Default)]
struct HubState {
    run: Option<Run>,
    next_id: u64,
    /// The constraints of the latest track: they set the size and rate of the next capture.
    want: VideoConstraints,
}

/// One track's share of the capture.
struct Output {
    id: u64,
    source: NativeVideoSource,
    preview: Arc<Mutex<Option<VideoSink>>>,
    active: bool,
}

/// A running capture thread; dropping it tells the thread to stop.
struct Run {
    stop: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl Drop for Run {
    fn drop(&mut self) {
        // Not joined: the thread notices within one frame interval, and the caller (an RPC
        // handler) should not wait for the portal to close.
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl ScreenHub {
    /// A track on the capture, enabled; starts the capture if it is not running.
    pub fn add(
        self: &Arc<Self>,
        source: NativeVideoSource,
        want: VideoConstraints,
    ) -> Result<ScreenFeed, String> {
        let preview = Arc::new(Mutex::new(None));
        let id = {
            let mut st = self.state.lock().unwrap();
            st.next_id += 1;
            st.want = want;
            st.next_id
        };
        self.outputs.lock().unwrap().push(Output {
            id,
            source: source.clone(),
            preview: preview.clone(),
            active: true,
        });
        self.refresh()?;
        Ok(ScreenFeed {
            hub: self.clone(),
            id,
            source,
            preview,
            stopped: AtomicBool::new(false),
        })
    }

    fn set_active(&self, id: u64, active: bool) {
        for o in self.outputs.lock().unwrap().iter_mut() {
            if o.id == id {
                o.active = active;
            }
        }
        let _ = self.refresh();
    }

    fn remove(&self, id: u64) {
        self.outputs.lock().unwrap().retain(|o| o.id != id);
        let _ = self.refresh();
    }

    /// Starts or ends the capture to match the tracks.
    fn refresh(&self) -> Result<(), String> {
        let wanted = self.outputs.lock().unwrap().iter().any(|o| o.active);
        let mut st = self.state.lock().unwrap();
        if !wanted {
            st.run = None;
            return Ok(());
        }
        if st.run.is_some() {
            return Ok(());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let job = Job {
            stop: stop.clone(),
            outputs: self.outputs.clone(),
            max: (
                st.want.width.unwrap_or(DEFAULT_MAX.0).max(2),
                st.want.height.unwrap_or(DEFAULT_MAX.1).max(2),
            ),
            fps: st
                .want
                .frame_rate
                .filter(|f| *f > 0.0)
                .unwrap_or(DEFAULT_FPS)
                .clamp(1.0, 30.0),
        };
        let thread = std::thread::Builder::new()
            .name("teams-redir-screen".into())
            .spawn(move || job.run())
            .map_err(|e| format!("screen capture thread: {e}"))?;
        st.run = Some(Run {
            stop,
            _thread: thread,
        });
        Ok(())
    }
}

/// The screen behind one video track.
pub struct ScreenFeed {
    hub: Arc<ScreenHub>,
    id: u64,
    pub source: NativeVideoSource,
    pub preview: Arc<Mutex<Option<VideoSink>>>,
    stopped: AtomicBool,
}

impl ScreenFeed {
    pub fn set_enabled(&self, enabled: bool) {
        if !self.stopped.load(Ordering::SeqCst) {
            self.hub.set_active(self.id, enabled);
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.hub.remove(self.id);
    }
}

impl Drop for ScreenFeed {
    fn drop(&mut self) {
        self.hub.remove(self.id);
    }
}

struct Job {
    stop: Arc<AtomicBool>,
    outputs: Arc<Mutex<Vec<Output>>>,
    max: (u32, u32),
    fps: f64,
}

impl Job {
    fn run(self) {
        let ctx = ThreadContext::new();
        let mut options = DesktopCapturerOptions::new(DesktopCaptureSourceType::Generic);
        options.set_include_cursor(true);
        // A track whose capture cannot start, or whose picker the user cancels, stays black.
        let Some(mut capturer) = DesktopCapturer::new(options) else {
            return;
        };
        let failed = Arc::new(AtomicBool::new(false));
        {
            let failed = failed.clone();
            let outputs = self.outputs.clone();
            let max = self.max;
            capturer.start_capture(None, move |result| match result {
                Ok(frame) => {
                    if let Some(f) = convert(&frame, max) {
                        deliver(&outputs, &f);
                    }
                }
                // Before the user has picked, and between frames that did not change.
                Err(CaptureError::Temporary) => {}
                Err(CaptureError::Permanent) => failed.store(true, Ordering::SeqCst),
            });
        }
        let interval = Duration::from_secs_f64(1.0 / self.fps);
        let mut next = Instant::now();
        while !self.stop.load(Ordering::SeqCst) && !failed.load(Ordering::SeqCst) {
            ctx.pump();
            capturer.capture_frame();
            next += interval;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        // Closing the capturer closes the portal session; its last D-Bus calls go out on `ctx`,
        // which is pumped once more when it is dropped.
        drop(capturer);
    }
}

/// The size a `w` x `h` frame is sent at: scaled down (never up) to fit `max`, even sides.
pub fn fit(w: u32, h: u32, max: (u32, u32)) -> (u32, u32) {
    let scale = (max.0 as f64 / w as f64)
        .min(max.1 as f64 / h as f64)
        .min(1.0);
    let even = |v: f64| ((v as u32) & !1).max(2);
    (even(w as f64 * scale), even(h as f64 * scale))
}

/// A captured frame (BGRA in memory, libyuv's "ARGB") as tightly packed I420, scaled to fit
/// `max`. `None` for a frame that is not usable. This runs inside a libwebrtc callback, where a
/// panic would abort the process, so sizes are checked before the converters assert on them.
fn convert(frame: &DesktopFrame, max: (u32, u32)) -> Option<crate::VideoFrame> {
    let (w, h) = (frame.width(), frame.height());
    if w < 2 || h < 2 {
        return None;
    }
    let (w, h) = (w as u32 & !1, h as u32 & !1);
    let stride = frame.stride();
    let data = frame.data();
    if stride < w * 4 || (data.len() as u64) < stride as u64 * h as u64 {
        return None;
    }
    let mut buf = I420Buffer::new(w, h);
    let (sy, su, sv) = buf.strides();
    let (y, u, v) = buf.data_mut();
    yuv_helper::argb_to_i420(data, stride, y, sy, u, su, v, sv, w as i32, h as i32);
    let (tw, th) = fit(w, h, max);
    let buf = if (tw, th) != (w, h) {
        buf.scale(tw as i32, th as i32)
    } else {
        buf
    };
    tight(&buf)
}

/// Hands a frame to every enabled track on the capture, and to their previews.
fn deliver(outputs: &Mutex<Vec<Output>>, f: &crate::VideoFrame) {
    let (w, h) = (f.width, f.height);
    for o in outputs.lock().unwrap().iter().filter(|o| o.active) {
        let mut buf = I420Buffer::with_strides(w, h, w, w.div_ceil(2), w.div_ceil(2));
        let (y, u, v) = buf.data_mut();
        if y.len() == f.y.len() && u.len() == f.u.len() && v.len() == f.v.len() {
            y.copy_from_slice(&f.y);
            u.copy_from_slice(&f.u);
            v.copy_from_slice(&f.v);
            o.source
                .capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, buf));
        }
        let sink = o.preview.lock().unwrap().clone();
        if let Some(sink) = sink {
            sink(f);
        }
    }
}

/// The buffer as tightly packed planes.
fn tight(buf: &I420Buffer) -> Option<crate::VideoFrame> {
    let (w, h) = (buf.width() as usize, buf.height() as usize);
    let (cw, ch) = (buf.chroma_width() as usize, buf.chroma_height() as usize);
    let (sy, su, sv) = buf.strides();
    let (y, u, v) = buf.data();
    let copy = |src: &[u8], stride: usize, width: usize, rows: usize| -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(width * rows);
        for r in 0..rows {
            out.extend_from_slice(src.get(r * stride..r * stride + width)?);
        }
        Some(out)
    };
    Some(crate::VideoFrame {
        width: w as u32,
        height: h as u32,
        y: copy(y, sy as usize, w, h)?,
        u: copy(u, su as usize, cw, ch)?,
        v: copy(v, sv as usize, cw, ch)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_fit_the_limit_without_growing() {
        assert_eq!(fit(2560, 1600, (1920, 1080)), (1728, 1080));
        assert_eq!(fit(1280, 720, (1920, 1080)), (1280, 720));
    }
}
