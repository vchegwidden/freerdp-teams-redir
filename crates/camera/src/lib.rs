//! Webcams through V4L2: which ones there are, and frames from one as I420.
//!
//! No libwebrtc types here: the media engine turns a [`Frame`] into whatever its video source
//! wants. A camera is on only while a [`Capture`] exists, so dropping it turns the light off.
//!
//! Formats: MJPEG is preferred (a UVC webcam gives 720p30 or better only that way) and YUYV is the
//! fallback. Both end up as limited-range BT.601 I420, which is what WebRTC encoders expect.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use v4l::buffer::Type;
use v4l::capability::Flags;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture as _;
use v4l::{Device, FourCC};

pub mod convert;

/// A camera as the device list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// Stable across restarts and replugging into the same port: the bus the camera sits on and
    /// its V4L2 node index.
    pub id: String,
    pub label: String,
    /// Shared by the nodes of one physical camera.
    pub group: String,
    pub path: PathBuf,
}

/// What the caller would like. Every field is a preference: the closest mode the camera has wins.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Constraints {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
}

/// One I420 frame, tightly packed (strides equal widths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

const MJPG: FourCC = FourCC { repr: *b"MJPG" };
const YUYV: FourCC = FourCC { repr: *b"YUYV" };

/// The camera nodes of this machine. A UVC camera has several `/dev/video*` nodes (the second is
/// usually metadata), so only nodes that can capture video in a format we read are listed.
pub fn list() -> Vec<CameraInfo> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/sys/class/video4linux") else {
        return out;
    };
    let mut names: Vec<String> = dir
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("video"))
        .collect();
    names.sort_by_key(|n| {
        n.trim_start_matches("video")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    for name in names {
        let path = PathBuf::from("/dev").join(&name);
        if let Some(info) = probe(&path) {
            out.push(info);
        }
    }
    out
}

fn probe(path: &Path) -> Option<CameraInfo> {
    let dev = Device::with_path(path).ok()?;
    let caps = dev.query_caps().ok()?;
    // The crate reports what this node does (the driver's `device_caps`), not the whole device's.
    if !caps
        .capabilities
        .contains(Flags::VIDEO_CAPTURE | Flags::STREAMING)
    {
        return None;
    }
    let formats = dev.enum_formats().ok()?;
    if !formats.iter().any(|f| f.fourcc == MJPG || f.fourcc == YUYV) {
        return None;
    }
    let index = path
        .file_name()?
        .to_str()?
        .trim_start_matches("video")
        .parse::<u32>()
        .ok()?;
    Some(CameraInfo {
        id: format!("v4l2:{}:{}", caps.bus, index),
        label: friendly(&caps.card),
        group: caps.bus,
        path: path.to_owned(),
    })
}

/// UVC cameras name a node "<product>: <interface>" (and the kernel cuts it at 31 characters, which
/// is why "Integrated_Webcam_FHD: Integrat" is what shows up). The interface part means nothing to
/// a person.
fn friendly(card: &str) -> String {
    card.split_once(": ")
        .map_or(card, |(product, _)| product)
        .trim()
        .to_owned()
}

/// A running capture. Dropping it (or [`Capture::stop`]) stops the thread and closes the camera.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The mode that was chosen.
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
}

impl Capture {
    /// Opens the camera and starts delivering frames to `sink` from a thread. The sink must not
    /// block: it runs between frames.
    pub fn start(
        path: &Path,
        want: Constraints,
        mut sink: Box<dyn FnMut(Frame) + Send>,
    ) -> Result<Capture, String> {
        let dev = Device::with_path(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mode = choose_mode(&dev, want)?;
        let mut fmt = dev.format().map_err(|e| e.to_string())?;
        fmt.width = mode.width;
        fmt.height = mode.height;
        fmt.fourcc = mode.fourcc;
        let fmt = dev
            .set_format(&fmt)
            .map_err(|e| format!("set format: {e}"))?;
        // The driver may adjust what was asked for.
        let (width, height) = (fmt.width, fmt.height);
        let mut frame_rate = mode.fps;
        if let Ok(mut params) = dev.params() {
            params.interval = v4l::Fraction::new(1, mode.fps.round().max(1.0) as u32);
            if let Ok(set) = dev.set_params(&params) {
                frame_rate =
                    f64::from(set.interval.denominator) / f64::from(set.interval.numerator.max(1));
            }
        }
        let is_mjpeg = fmt.fourcc == MJPG;
        if !is_mjpeg && fmt.fourcc != YUYV {
            return Err(format!("the camera gave {}, not MJPG or YUYV", fmt.fourcc));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("teams-redir-camera".into())
            .spawn(move || {
                let Ok(mut stream) = Stream::with_buffers(&dev, Type::VideoCapture, 4) else {
                    return;
                };
                while !flag.load(Ordering::SeqCst) {
                    let Ok((buf, meta)) = stream.next() else {
                        break;
                    };
                    let data = &buf[..(meta.bytesused as usize).min(buf.len())];
                    let frame = if is_mjpeg {
                        convert::mjpeg_to_i420(data)
                    } else {
                        convert::yuyv_to_i420(data, width, height)
                    };
                    // A damaged frame is dropped; webcams produce one now and then.
                    if let Some(frame) = frame {
                        sink(frame);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Capture {
            stop,
            thread: Some(thread),
            width,
            height,
            frame_rate,
        })
    }

    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.halt();
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Mode {
    fourcc: FourCC,
    width: u32,
    height: u32,
    fps: f64,
}

fn choose_mode(dev: &Device, want: Constraints) -> Result<Mode, String> {
    let formats = dev.enum_formats().map_err(|e| e.to_string())?;
    let mut modes = Vec::new();
    for fourcc in [MJPG, YUYV] {
        if !formats.iter().any(|f| f.fourcc == fourcc) {
            continue;
        }
        for size in dev.enum_framesizes(fourcc).map_err(|e| e.to_string())? {
            for d in size.size.to_discrete() {
                let fps = dev
                    .enum_frameintervals(fourcc, d.width, d.height)
                    .ok()
                    .and_then(|iv| {
                        iv.iter()
                            .filter_map(|i| match &i.interval {
                                v4l::frameinterval::FrameIntervalEnum::Discrete(f) => {
                                    Some(f64::from(f.denominator) / f64::from(f.numerator.max(1)))
                                }
                                _ => None,
                            })
                            .reduce(f64::max)
                    })
                    .unwrap_or(30.0);
                modes.push(Mode {
                    fourcc,
                    width: d.width,
                    height: d.height,
                    fps,
                });
            }
        }
    }
    pick(&modes, want).ok_or_else(|| "the camera reports no usable mode".to_owned())
}

/// The mode closest to what was asked for. Size matters most, then frame rate; MJPEG beats YUYV
/// at equal distance because it carries more frames over USB. The default is 1280x720 at 30.
fn pick(modes: &[Mode], want: Constraints) -> Option<Mode> {
    let (w, h) = (want.width.unwrap_or(1280), want.height.unwrap_or(720));
    let fps = want.frame_rate.unwrap_or(30.0);
    modes.iter().copied().min_by(|a, b| {
        let cost = |m: &Mode| {
            let size = (f64::from(m.width) - f64::from(w)).abs()
                + (f64::from(m.height) - f64::from(h)).abs();
            // A mode that cannot reach the frame rate costs; one that exceeds it does not.
            let rate = (fps - m.fps).max(0.0) * 20.0;
            let format = if m.fourcc == MJPG { 0.0 } else { 1.0 };
            size + rate + format
        };
        cost(a).total_cmp(&cost(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uvc_interface_suffix_is_dropped_from_labels() {
        assert_eq!(
            friendly("Integrated_Webcam_FHD: Integrat"),
            "Integrated_Webcam_FHD"
        );
        assert_eq!(friendly("Logitech BRIO"), "Logitech BRIO");
    }

    fn m(f: FourCC, w: u32, h: u32, fps: f64) -> Mode {
        Mode {
            fourcc: f,
            width: w,
            height: h,
            fps,
        }
    }

    #[test]
    fn the_closest_mode_wins_and_mjpeg_breaks_ties() {
        let modes = [
            m(YUYV, 640, 480, 30.0),
            m(YUYV, 1280, 720, 10.0),
            m(MJPG, 640, 480, 30.0),
            m(MJPG, 1280, 720, 30.0),
            m(MJPG, 1920, 1080, 30.0),
        ];
        assert_eq!(pick(&modes, Constraints::default()), Some(modes[3]));
        let vga = Constraints {
            width: Some(640),
            height: Some(480),
            frame_rate: Some(30.0),
        };
        assert_eq!(pick(&modes, vga), Some(modes[2]));
        let full = Constraints {
            width: Some(1920),
            height: Some(1080),
            frame_rate: None,
        };
        assert_eq!(pick(&modes, full), Some(modes[4]));
        // A mode that cannot reach the frame rate loses to a smaller one that can.
        let slow = [m(YUYV, 1280, 720, 10.0), m(MJPG, 1024, 576, 30.0)];
        assert_eq!(pick(&slow, Constraints::default()), Some(slow[1]));
        assert_eq!(pick(&[], Constraints::default()), None);
    }
}

#[cfg(test)]
mod hardware {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;

    /// Needs a webcam. Captures about two seconds, keeps only numbers (never an image), and checks
    /// that frames arrive at roughly the chosen rate with sensible content.
    /// `cargo test -p teams-redir-camera captures_from_the_first_camera -- --ignored --nocapture`
    #[test]
    #[ignore = "turns the camera on"]
    fn captures_from_the_first_camera() {
        let cameras = list();
        println!("cameras: {cameras:#?}");
        let cam = cameras.first().expect("no camera");
        let stats = Arc::new(Mutex::new((0u32, 0u64, 0u32, 0u32)));
        let s = stats.clone();
        let capture = Capture::start(
            &cam.path,
            Constraints::default(),
            Box::new(move |f| {
                let mut s = s.lock().unwrap();
                s.0 += 1;
                let mean = f.y.iter().map(|&b| u64::from(b)).sum::<u64>() / f.y.len() as u64;
                if s.0 <= 3 || s.0.is_multiple_of(30) {
                    println!(
                        "frame {}: mean luma {mean}, min {:?}, max {:?}",
                        s.0,
                        f.y.iter().min(),
                        f.y.iter().max()
                    );
                }
                s.1 += mean;
                s.2 = f.width;
                s.3 = f.height;
                assert_eq!(f.u.len(), f.y.len() / 4);
                assert!(
                    f.y.iter().all(|&b| (16..=235).contains(&b)),
                    "luma outside limited range"
                );
            }),
        )
        .unwrap();
        println!(
            "mode: {}x{} at {} fps",
            capture.width, capture.height, capture.frame_rate
        );
        std::thread::sleep(Duration::from_secs(2));
        capture.stop();
        let (n, luma_sum, w, h) = *stats.lock().unwrap();
        println!(
            "{n} frames of {w}x{h}, mean luma {}",
            luma_sum / u64::from(n.max(1))
        );
        assert!(n > 20, "only {n} frames in two seconds");
    }
}
