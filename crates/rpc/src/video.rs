//! Where video goes: the windows that show a call's pictures.
//!
//! A FreeRDP plugin has no window of its own to draw in, so the pictures go to a helper process,
//! `teams-redir-video` (`crates/video-window`), over `videoipc`. The helper is started when the
//! first picture arrives and keeps one window per `MediaElement`. A missing or crashing helper
//! costs the pictures and nothing else: the call, the audio and the RDP session carry on.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use media::VideoFrame;
use videoipc::{MAGIC, Message};

/// Receives what the video elements show. Called from engine threads, so it must not block.
pub trait VideoSurface: Send + Sync {
    /// The newest picture of a window. A picture that has not been shown when the next arrives is
    /// dropped.
    fn frame(&self, window: u32, frame: &VideoFrame);
    /// Anything else: visibility, mirroring, fit, title, close.
    fn control(&self, message: Message);
}

/// Shows nothing.
pub struct NullSurface;

impl VideoSurface for NullSurface {
    fn frame(&self, _window: u32, _frame: &VideoFrame) {}
    fn control(&self, _message: Message) {}
}

#[derive(Default)]
struct Outbox {
    controls: VecDeque<Message>,
    /// The newest unsent frame of each window.
    frames: HashMap<u32, Message>,
    shutdown: bool,
}

struct Shared {
    outbox: Mutex<Outbox>,
    wake: Condvar,
}

/// Feeds the helper process from a thread of its own.
pub struct HelperSurface {
    shared: Arc<Shared>,
    /// False while the helper cannot be started, so pictures are not even copied.
    available: Arc<Mutex<bool>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

type Log = Arc<dyn Fn(&str) + Send + Sync>;

impl HelperSurface {
    /// `helper` is the program to run; `None` looks for `teams-redir-video` next to the add-in and
    /// then in `PATH`. `log` gets the reasons the helper could not run.
    pub fn new(helper: Option<PathBuf>, log: Log) -> HelperSurface {
        let shared = Arc::new(Shared {
            outbox: Mutex::new(Outbox::default()),
            wake: Condvar::new(),
        });
        let available = Arc::new(Mutex::new(true));
        let thread = {
            let (shared, available) = (shared.clone(), available.clone());
            std::thread::Builder::new()
                .name("teams-redir-video".into())
                .spawn(move || run(&shared, &available, helper, log))
                .ok()
        };
        HelperSurface {
            shared,
            available,
            thread: Mutex::new(thread),
        }
    }

    fn usable(&self) -> bool {
        *self.available.lock().unwrap()
    }
}

impl VideoSurface for HelperSurface {
    fn frame(&self, window: u32, f: &VideoFrame) {
        if !self.usable() {
            return;
        }
        let m = Message::frame(window, f.width, f.height, &f.y, &f.u, &f.v);
        self.shared.outbox.lock().unwrap().frames.insert(window, m);
        self.shared.wake.notify_one();
    }

    fn control(&self, message: Message) {
        if !self.usable() {
            return;
        }
        let mut o = self.shared.outbox.lock().unwrap();
        // A closed window needs no picture that was waiting for it.
        if let Message::Close { window } = &message {
            o.frames.remove(window);
        }
        o.controls.push_back(message);
        drop(o);
        self.shared.wake.notify_one();
    }
}

impl Drop for HelperSurface {
    fn drop(&mut self) {
        self.shared.outbox.lock().unwrap().shutdown = true;
        self.shared.wake.notify_one();
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

/// How long to leave a helper that would not start alone before trying again.
const RETRY: Duration = Duration::from_secs(30);

fn run(shared: &Shared, available: &Mutex<bool>, helper: Option<PathBuf>, log: Log) {
    let mut child: Option<(Child, std::process::ChildStdin)> = None;
    let mut failed_at: Option<Instant> = None;
    loop {
        let (controls, frames) = {
            let mut o = shared.outbox.lock().unwrap();
            while o.controls.is_empty() && o.frames.is_empty() && !o.shutdown {
                o = shared.wake.wait(o).unwrap();
            }
            // On shutdown what is queued is still sent: the loop ends once there is nothing left.
            if o.controls.is_empty() && o.frames.is_empty() {
                break;
            }
            (
                std::mem::take(&mut o.controls),
                std::mem::take(&mut o.frames),
            )
        };
        if child.is_none() {
            if failed_at.is_some_and(|t| t.elapsed() < RETRY) {
                continue;
            }
            match spawn(helper.as_deref()) {
                Ok(c) => {
                    child = Some(c);
                    failed_at = None;
                    *available.lock().unwrap() = true;
                }
                Err(e) => {
                    log(&format!("video windows are off: {e}"));
                    failed_at = Some(Instant::now());
                    // Pictures are not copied while there is nowhere to send them.
                    *available.lock().unwrap() = false;
                    continue;
                }
            }
        }
        let Some((c, stdin)) = child.as_mut() else {
            continue;
        };
        let mut bytes = Vec::new();
        for m in controls.iter().chain(frames.values()) {
            m.encode(&mut bytes);
        }
        if stdin
            .write_all(&bytes)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            let _ = c.wait();
            log("the video helper stopped; it starts again with the next picture");
            child = None;
        }
    }
    if let Some((mut c, stdin)) = child.take() {
        drop(stdin);
        let _ = c.wait();
    }
}

fn spawn(helper: Option<&std::path::Path>) -> Result<(Child, std::process::ChildStdin), String> {
    let path = match helper {
        Some(p) => p.to_owned(),
        None => find_helper().ok_or("teams-redir-video not found next to the add-in or in PATH")?,
    };
    let mut child = Command::new(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", path.display()))?;
    let mut stdin = child.stdin.take().ok_or("no helper stdin")?;
    stdin.write_all(&MAGIC).map_err(|e| e.to_string())?;
    Ok((child, stdin))
}

/// `teams-redir-video` in the directory of this module, else in `PATH`.
fn find_helper() -> Option<PathBuf> {
    const NAME: &str = "teams-redir-video";
    if let Some(dir) = module_dir() {
        let p = dir.join(NAME);
        if p.is_file() {
            return Some(p);
        }
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(NAME))
            .find(|p| p.is_file())
    })
}

/// The directory `libteams-redir-client.so` was loaded from, read from the process's mappings.
fn module_dir() -> Option<PathBuf> {
    module_dir_in(&std::fs::read_to_string("/proc/self/maps").ok()?)
}

fn module_dir_in(maps: &str) -> Option<PathBuf> {
    let path = maps
        .lines()
        .filter_map(|l| l.split_whitespace().nth(5))
        .find(|p| p.contains("libteams-redir-client"))?;
    PathBuf::from(path).parent().map(PathBuf::from)
}

#[cfg(test)]
pub mod fake {
    use super::*;

    /// Records what it is given.
    #[derive(Default)]
    pub struct RecordingSurface {
        /// `(window, width, height)` of each frame.
        pub frames: Mutex<Vec<(u32, u32, u32)>>,
        pub controls: Mutex<Vec<Message>>,
    }

    impl VideoSurface for RecordingSurface {
        fn frame(&self, window: u32, f: &VideoFrame) {
            self.frames
                .lock()
                .unwrap()
                .push((window, f.width, f.height));
        }

        fn control(&self, message: Message) {
            self.controls.lock().unwrap().push(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn frame() -> VideoFrame {
        VideoFrame {
            width: 4,
            height: 2,
            y: vec![9; 8],
            u: vec![1; 2],
            v: vec![2; 2],
        }
    }

    /// A stand-in helper: a script that copies its input to a file.
    fn script(name: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("teams-redir-video-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("received");
        let sh = dir.join("helper.sh");
        std::fs::write(&sh, format!("#!/bin/sh\nexec cat > '{}'\n", out.display())).unwrap();
        std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
        (sh, out)
    }

    fn silent() -> Log {
        Arc::new(|_| ())
    }

    #[test]
    fn messages_reach_the_helper_in_order_and_old_frames_are_dropped() {
        let (sh, out) = script("order");
        let surface = HelperSurface::new(Some(sh), silent());
        surface.control(Message::Title {
            window: 1,
            title: "me".into(),
        });
        surface.control(Message::Mirror {
            window: 1,
            mirrored: true,
        });
        surface.frame(1, &frame());
        drop(surface); // flushes and waits for the helper to end
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[..4], &MAGIC);
        let mut r = &bytes[4..];
        let mut got = Vec::new();
        while let Some(m) = videoipc::read_message(&mut r).unwrap() {
            got.push(m);
        }
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(matches!(&got[0], Message::Title { window: 1, .. }));
        assert!(matches!(&got[1], Message::Mirror { mirrored: true, .. }));
        assert!(matches!(
            &got[2],
            Message::Frame {
                window: 1,
                width: 4,
                ..
            }
        ));
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }

    #[test]
    fn a_helper_that_cannot_start_is_reported_once_and_pictures_stop_being_copied() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let l = lines.clone();
        let surface = HelperSurface::new(
            Some(PathBuf::from("/nonexistent/teams-redir-video")),
            Arc::new(move |s| l.lock().unwrap().push(s.to_owned())),
        );
        surface.frame(1, &frame());
        let start = Instant::now();
        while lines.lock().unwrap().is_empty() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            lines.lock().unwrap()[0].contains("video windows are off"),
            "{:?}",
            lines.lock().unwrap()
        );
        assert!(!surface.usable());
        surface.frame(1, &frame());
        surface.control(Message::Close { window: 1 });
        assert!(surface.shared.outbox.lock().unwrap().frames.is_empty());
        assert_eq!(lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_modules_directory_is_read_from_the_mappings() {
        let maps = "\
7f00 r-xp 00000000 08:01 12 /usr/lib/libc.so.6
7f10 r--p 00000000 08:01 34 /home/u/opt/freerdp/lib/freerdp3/libteams-redir-client.so
7f20 r-xp 00001000 08:01 34 /home/u/opt/freerdp/lib/freerdp3/libteams-redir-client.so
";
        assert_eq!(
            module_dir_in(maps),
            Some(PathBuf::from("/home/u/opt/freerdp/lib/freerdp3"))
        );
        assert_eq!(
            module_dir_in("7f00 r-xp 0 08:01 12 /usr/lib/libc.so.6\n"),
            None
        );
    }

    #[test]
    fn closing_a_window_drops_its_waiting_picture() {
        let surface = HelperSurface {
            shared: Arc::new(Shared {
                outbox: Mutex::new(Outbox::default()),
                wake: Condvar::new(),
            }),
            available: Arc::new(Mutex::new(true)),
            thread: Mutex::new(None),
        };
        surface.frame(1, &frame());
        surface.frame(2, &frame());
        surface.control(Message::Close { window: 1 });
        let o = surface.shared.outbox.lock().unwrap();
        assert!(!o.frames.contains_key(&1) && o.frames.contains_key(&2));
        assert_eq!(o.controls.len(), 1);
    }
}
