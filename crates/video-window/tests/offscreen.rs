//! Runs the helper on SDL's offscreen video driver (no window appears) and checks what it draws
//! from what it prints in `TEAMS_REDIR_VIDEO_CHECK` mode.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use videoipc::{Fit, MAGIC, Message};

/// BT.601 limited-range (Y, U, V) of pure red, green and blue.
const RED: (u8, u8, u8) = (81, 90, 240);
const BLUE: (u8, u8, u8) = (41, 240, 110);

/// A frame whose left half is `left` and right half `right`.
fn split_frame(window: u32, w: u32, h: u32, left: (u8, u8, u8), right: (u8, u8, u8)) -> Message {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut y = Vec::new();
    for _ in 0..h {
        for x in 0..w {
            y.push(if x < w / 2 { left.0 } else { right.0 });
        }
    }
    let mut u = Vec::new();
    let mut v = Vec::new();
    for _ in 0..ch {
        for x in 0..cw {
            let c = if x < cw / 2 { left } else { right };
            u.push(c.1);
            v.push(c.2);
        }
    }
    Message::frame(window, w, h, &y, &u, &v)
}

struct Helper {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Helper {
    fn start() -> Helper {
        Helper::start_with("0")
    }

    /// `open_delay` is `TEAMS_REDIR_VIDEO_OPEN_DELAY_MS`.
    fn start_with(open_delay: &str) -> Helper {
        let mut child = Command::new(env!("CARGO_BIN_EXE_teams-redir-video"))
            .env("SDL_VIDEODRIVER", "offscreen")
            .env("TEAMS_REDIR_VIDEO_CHECK", "1")
            .env("TEAMS_REDIR_VIDEO_OPEN_DELAY_MS", open_delay)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the helper");
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let mut h = Helper { child, lines: rx };
        h.send_raw(&MAGIC);
        h
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    fn send(&mut self, m: &Message) {
        let mut bytes = Vec::new();
        m.encode(&mut bytes);
        self.send_raw(&bytes);
    }

    /// The first `window N left .. middle .. right ..` line for which `ok` holds, as the three
    /// colours. Redraws (the window being exposed, say) print too, so lines from before a change
    /// are skipped; if none ever matches, the last one seen is returned for the assertion message.
    fn wait_samples(&self, window: u32, ok: impl Fn(&[String; 3]) -> bool) -> [String; 3] {
        let mut last = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let s = self.next_samples(window);
            if ok(&s) {
                return s;
            }
            last = Some(s);
        }
        last.expect("no samples")
    }

    /// The next `window N opened` or `window N reused` line: how the helper got a window for N.
    fn next_how(&self, window: u32) -> String {
        loop {
            let line = self
                .lines
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the helper printed nothing");
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() == 3 && p[0] == "window" && p[1] == window.to_string() {
                return p[2].to_owned();
            }
            assert!(!line.contains("cannot"), "helper error: {line}");
        }
    }

    /// The next `window N left .. middle .. right ..` line, as the three colours.
    fn next_samples(&self, window: u32) -> [String; 3] {
        loop {
            let line = self
                .lines
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the helper printed nothing");
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() == 8 && p[0] == "window" && p[1] == window.to_string() {
                return [p[3].to_owned(), p[5].to_owned(), p[7].to_owned()];
            }
            assert!(!line.contains("cannot"), "helper error: {line}");
        }
    }
}

/// Whether a printed colour is within `tol` of the expected one on every channel.
fn near(hex: &str, rgb: (u8, u8, u8), tol: i32) -> bool {
    let c = |i: usize| i32::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap());
    (c(0) - i32::from(rgb.0)).abs() <= tol
        && (c(2) - i32::from(rgb.1)).abs() <= tol
        && (c(4) - i32::from(rgb.2)).abs() <= tol
}

#[test]
fn frames_are_drawn_in_the_right_colours_and_mirroring_flips_them() {
    let mut h = Helper::start();
    h.send(&split_frame(3, 64, 48, RED, BLUE));
    let [left, _, right] = h.next_samples(3);
    assert!(near(&left, (255, 0, 0), 24), "left {left}");
    assert!(near(&right, (0, 0, 255), 24), "right {right}");

    h.send(&Message::Mirror {
        window: 3,
        mirrored: true,
    });
    let [left, _, right] = h.wait_samples(3, |s| near(&s[0], (0, 0, 255), 24));
    assert!(near(&left, (0, 0, 255), 24), "mirrored left {left}");
    assert!(near(&right, (255, 0, 0), 24), "mirrored right {right}");

    // What is said before a window's first frame applies when the frame opens it.
    h.send(&Message::Mirror {
        window: 5,
        mirrored: true,
    });
    h.send(&split_frame(5, 64, 48, RED, BLUE));
    let [left, _, _] = h.wait_samples(5, |_| true);
    assert!(
        near(&left, (0, 0, 255), 24),
        "pending mirror ignored: {left}"
    );
    h.send(&Message::Close { window: 5 });

    // A new picture size replaces the texture, and a second window is independent.
    h.send(&split_frame(9, 32, 24, BLUE, BLUE));
    let [left, _, _] = h.next_samples(9);
    assert!(near(&left, (0, 0, 255), 24));
    h.send(&Message::Fit {
        window: 3,
        fit: Fit::Fill,
    });
    let [left, _, _] = h.wait_samples(3, |_| true);
    assert!(near(&left, (0, 0, 255), 24), "still mirrored: {left}");

    h.send(&Message::Close { window: 3 });
    h.send(&Message::Close { window: 9 });
    // End of input ends the helper.
    drop(h.child.stdin.take());
    let status = h.child.wait().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn a_closed_window_is_taken_over_by_the_next_element_instead_of_a_new_one_opening() {
    let mut h = Helper::start();
    h.send(&split_frame(1, 64, 48, RED, BLUE));
    assert_eq!(h.next_how(1), "opened");
    h.send(&Message::Close { window: 1 });
    // Teams closes one element and opens another a moment later, for the same picture.
    h.send(&split_frame(2, 64, 48, BLUE, RED));
    assert_eq!(h.next_how(2), "reused");
    let [left, _, right] = h.wait_samples(2, |_| true);
    assert!(near(&left, (0, 0, 255), 24), "left {left}");
    assert!(near(&right, (255, 0, 0), 24), "right {right}");
    // Two at once need two windows.
    h.send(&split_frame(3, 64, 48, RED, BLUE));
    assert_eq!(h.next_how(3), "opened");
    drop(h.child.stdin.take());
    assert!(h.child.wait().unwrap().success());
}

#[test]
fn a_window_opens_only_once_its_picture_has_kept_coming() {
    let mut h = Helper::start_with("400");
    // An element that is gone again within the delay never gets a window.
    h.send(&split_frame(1, 64, 48, RED, BLUE));
    h.send(&Message::Close { window: 1 });
    // One that stays does, from the first frame after the delay.
    h.send(&split_frame(2, 64, 48, RED, BLUE));
    std::thread::sleep(std::time::Duration::from_millis(500));
    h.send(&split_frame(2, 64, 48, RED, BLUE));
    assert_eq!(h.next_how(2), "opened");
    drop(h.child.stdin.take());
    assert!(h.child.wait().unwrap().success());
}

#[test]
fn a_stream_without_the_header_is_refused() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_teams-redir-video"))
        .env("SDL_VIDEODRIVER", "offscreen")
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"nope").unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(2));
}
