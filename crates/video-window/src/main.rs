//! `teams-redir-video`: shows the video of a Teams call in ordinary windows, one per video element.
//!
//! The add-in starts this program and writes `videoipc` messages to its standard input; the
//! program exits when that closes. It exists as its own process because a FreeRDP plugin has no
//! window of its own to draw in, and because a graphics stack in the RDP client's address space is
//! a risk that is not needed. It draws with SDL3 (the toolkit FreeRDP's SDL client uses), so it
//! works on Wayland and on X11, scales on the GPU where there is one, and shows up under the
//! application id `teams-redir-video`, which a window rule can match to place the windows.
//!
//! `TEAMS_REDIR_VIDEO_CHECK=1` makes it print the colours of three points of a window after every
//! draw to stderr, for tests.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io::{self, Read};
use std::ptr;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use sdl3_sys::everything::*;
use videoipc::{Fit, MAGIC, Message, read_message};

struct Window {
    window: *mut SDL_Window,
    renderer: *mut SDL_Renderer,
    texture: *mut SDL_Texture,
    texture_size: (u32, u32),
    /// The picture as last drawn, to redraw after a resize.
    has_picture: bool,
    mirrored: bool,
    fit: Fit,
    /// Hidden by the add-in (`visible: false`) or closed by the user.
    visible: bool,
}

/// What the add-in said about a window before its first frame, which is what opens it.
#[derive(Default)]
struct Pending {
    title: Option<String>,
    mirrored: Option<bool>,
    fit: Option<Fit>,
    visible: Option<bool>,
}

struct App {
    windows: HashMap<u32, Window>,
    pending: HashMap<u32, Pending>,
    check: bool,
}

fn sdl_error() -> String {
    // SAFETY: SDL_GetError always returns a valid, NUL-terminated string.
    unsafe {
        CStr::from_ptr(SDL_GetError())
            .to_string_lossy()
            .into_owned()
    }
}

impl App {
    fn window(&mut self, id: u32, first_frame: Option<(u32, u32)>) -> Option<&mut Window> {
        if !self.windows.contains_key(&id) {
            let (fw, fh) = first_frame?;
            let title = CString::new(format!("Teams video {id}")).ok()?;
            // A window opens at a width of 640 and the picture's shape.
            let height = (640.0 * f64::from(fh) / f64::from(fw.max(1)))
                .round()
                .max(1.0) as i32;
            // SAFETY: plain SDL calls with valid arguments; failures are checked.
            let (window, renderer) = unsafe {
                let window = SDL_CreateWindow(title.as_ptr(), 640, height, SDL_WINDOW_RESIZABLE);
                if window.is_null() {
                    eprintln!("teams-redir-video: cannot open a window: {}", sdl_error());
                    return None;
                }
                let renderer = SDL_CreateRenderer(window, ptr::null());
                if renderer.is_null() {
                    eprintln!(
                        "teams-redir-video: cannot create a renderer: {}",
                        sdl_error()
                    );
                    SDL_DestroyWindow(window);
                    return None;
                }
                SDL_SetRenderVSync(renderer, 1);
                (window, renderer)
            };
            let said = self.pending.remove(&id).unwrap_or_default();
            let mut w = Window {
                window,
                renderer,
                texture: ptr::null_mut(),
                texture_size: (0, 0),
                has_picture: false,
                mirrored: said.mirrored.unwrap_or(false),
                fit: said.fit.unwrap_or(Fit::Contain),
                visible: true,
            };
            if let Some(Ok(t)) = said.title.map(CString::new) {
                // SAFETY: the window is live and `t` is NUL-terminated.
                unsafe { SDL_SetWindowTitle(window, t.as_ptr()) };
            }
            if said.visible == Some(false) {
                w.set_visible(false);
            }
            self.windows.insert(id, w);
        }
        self.windows.get_mut(&id)
    }

    fn handle(&mut self, m: Message) {
        let check = self.check;
        match m {
            Message::Frame {
                window,
                width,
                height,
                planes,
            } => {
                let Some(w) = self.window(window, Some((width, height))) else {
                    return;
                };
                w.upload(width, height, &planes);
                w.draw(check, window);
            }
            Message::Close { window } => {
                self.pending.remove(&window);
                if let Some(w) = self.windows.remove(&window) {
                    w.destroy();
                }
            }
            Message::Visible { window, visible } => match self.windows.get_mut(&window) {
                Some(w) => w.set_visible(visible),
                None => self.pending.entry(window).or_default().visible = Some(visible),
            },
            Message::Mirror { window, mirrored } => match self.windows.get_mut(&window) {
                Some(w) => {
                    w.mirrored = mirrored;
                    w.draw(check, window);
                }
                None => self.pending.entry(window).or_default().mirrored = Some(mirrored),
            },
            Message::Fit { window, fit } => match self.windows.get_mut(&window) {
                Some(w) => {
                    w.fit = fit;
                    w.draw(check, window);
                }
                None => self.pending.entry(window).or_default().fit = Some(fit),
            },
            Message::Title { window, title } => match self.windows.get(&window) {
                Some(w) => {
                    if let Ok(t) = CString::new(title) {
                        // SAFETY: the window is live and `t` is NUL-terminated.
                        unsafe { SDL_SetWindowTitle(w.window, t.as_ptr()) };
                    }
                }
                None => self.pending.entry(window).or_default().title = Some(title),
            },
        }
    }

    /// Redraws windows after the user resized or exposed them, and notes ones they closed.
    fn on_sdl_event(&mut self, ev: &SDL_Event) {
        // SAFETY: `type` is the first field of every variant of the union.
        let kind = SDL_EventType(unsafe { ev.r#type });
        let is = |k: SDL_EventType| kind == k;
        if !(is(SDL_EVENT_WINDOW_EXPOSED)
            || is(SDL_EVENT_WINDOW_RESIZED)
            || is(SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED)
            || is(SDL_EVENT_WINDOW_CLOSE_REQUESTED))
        {
            return;
        }
        // SAFETY: those are all window events, whose variant is `window`.
        let sdl_window = unsafe { ev.window.windowID };
        let check = self.check;
        let found = self.windows.iter_mut().find(|(_, w)| {
            // SAFETY: the window pointer is live while it is in the map.
            unsafe { SDL_GetWindowID(w.window) == sdl_window }
        });
        if let Some((id, w)) = found {
            if is(SDL_EVENT_WINDOW_CLOSE_REQUESTED) {
                // The user closed it: keep it hidden until the add-in shows it again.
                w.set_visible(false);
            } else {
                w.draw(check, *id);
            }
        }
    }
}

impl Window {
    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        // SAFETY: the window is live.
        unsafe {
            if visible {
                SDL_ShowWindow(self.window);
            } else {
                SDL_HideWindow(self.window);
            }
        }
    }

    fn upload(&mut self, width: u32, height: u32, planes: &[u8]) {
        // SAFETY: the renderer and texture belong to this window; the planes are laid out and sized
        // as `videoipc` promises (checked when the message was read).
        unsafe {
            if self.texture.is_null() || self.texture_size != (width, height) {
                if !self.texture.is_null() {
                    SDL_DestroyTexture(self.texture);
                }
                self.texture = SDL_CreateTexture(
                    self.renderer,
                    SDL_PIXELFORMAT_IYUV,
                    SDL_TEXTUREACCESS_STREAMING,
                    width as i32,
                    height as i32,
                );
                if self.texture.is_null() {
                    eprintln!(
                        "teams-redir-video: cannot create a texture: {}",
                        sdl_error()
                    );
                    self.has_picture = false;
                    return;
                }
                SDL_SetTextureScaleMode(self.texture, SDL_SCALEMODE_LINEAR);
                self.texture_size = (width, height);
            }
            let (w, h) = (width as usize, height as usize);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let (y, rest) = planes.split_at(w * h);
            let (u, v) = rest.split_at(cw * ch);
            let ok = SDL_UpdateYUVTexture(
                self.texture,
                ptr::null(),
                y.as_ptr(),
                w as i32,
                u.as_ptr(),
                cw as i32,
                v.as_ptr(),
                cw as i32,
            );
            self.has_picture = ok;
        }
    }

    fn draw(&mut self, check: bool, id: u32) {
        if !self.has_picture || !self.visible {
            return;
        }
        let (tw, th) = (self.texture_size.0 as f32, self.texture_size.1 as f32);
        // SAFETY: the renderer and texture are live; the rectangles are on the stack.
        unsafe {
            let (mut ow, mut oh) = (0i32, 0i32);
            if !SDL_GetCurrentRenderOutputSize(self.renderer, &mut ow, &mut oh)
                || ow <= 0
                || oh <= 0
            {
                return;
            }
            let (src, dst) = place(self.fit, (tw, th), (ow as f32, oh as f32));
            let src = SDL_FRect {
                x: src.0,
                y: src.1,
                w: src.2,
                h: src.3,
            };
            let dst = SDL_FRect {
                x: dst.0,
                y: dst.1,
                w: dst.2,
                h: dst.3,
            };
            SDL_SetRenderDrawColor(self.renderer, 0, 0, 0, 255);
            SDL_RenderClear(self.renderer);
            let flip = if self.mirrored {
                SDL_FLIP_HORIZONTAL
            } else {
                SDL_FLIP_NONE
            };
            SDL_RenderTextureRotated(
                self.renderer,
                self.texture,
                &src,
                &dst,
                0.0,
                ptr::null(),
                flip,
            );
            if check {
                report_samples(self.renderer, id, ow, oh);
            }
            SDL_RenderPresent(self.renderer);
        }
    }

    fn destroy(self) {
        // SAFETY: each pointer is destroyed once, texture first.
        unsafe {
            if !self.texture.is_null() {
                SDL_DestroyTexture(self.texture);
            }
            SDL_DestroyRenderer(self.renderer);
            SDL_DestroyWindow(self.window);
        }
    }
}

/// `(x, y, width, height)`.
type Rect = (f32, f32, f32, f32);

/// Where a picture of size `pic` goes in an output of size `out`: the part of the picture shown
/// (x, y, w, h) and the part of the output it fills.
fn place(fit: Fit, pic: (f32, f32), out: (f32, f32)) -> (Rect, Rect) {
    let whole = (0.0, 0.0, pic.0, pic.1);
    match fit {
        Fit::Fill => (whole, (0.0, 0.0, out.0, out.1)),
        Fit::Contain => {
            let scale = (out.0 / pic.0).min(out.1 / pic.1);
            let (w, h) = (pic.0 * scale, pic.1 * scale);
            (whole, ((out.0 - w) / 2.0, (out.1 - h) / 2.0, w, h))
        }
        Fit::Cover => {
            // Show the middle of the picture, in the output's shape.
            let scale = (out.0 / pic.0).max(out.1 / pic.1);
            let (w, h) = (out.0 / scale, out.1 / scale);
            (
                ((pic.0 - w) / 2.0, (pic.1 - h) / 2.0, w, h),
                (0.0, 0.0, out.0, out.1),
            )
        }
    }
}

/// Prints `window <id> left RRGGBB middle RRGGBB right RRGGBB` for three points of what was just
/// drawn: a quarter, half and three quarters of the way across the middle row.
///
/// # Safety
/// `renderer` must be live.
unsafe fn report_samples(renderer: *mut SDL_Renderer, id: u32, w: i32, h: i32) {
    // SAFETY: per the contract above; each surface is freed before the next is read.
    unsafe {
        let mut line = format!("window {id}");
        for (name, x) in [("left", w / 4), ("middle", w / 2), ("right", w * 3 / 4)] {
            let rect = SDL_Rect {
                x,
                y: h / 2,
                w: 1,
                h: 1,
            };
            let surface = SDL_RenderReadPixels(renderer, &rect);
            if surface.is_null() {
                eprintln!("window {id} {name} unreadable: {}", sdl_error());
                return;
            }
            let (mut r, mut g, mut b, mut a) = (0u8, 0u8, 0u8, 0u8);
            SDL_ReadSurfacePixel(surface, 0, 0, &mut r, &mut g, &mut b, &mut a);
            SDL_DestroySurface(surface);
            line.push_str(&format!(" {name} {r:02X}{g:02X}{b:02X}"));
        }
        eprintln!("{line}");
    }
}

/// Reads messages on a thread and wakes the SDL loop for each, so a frame is drawn at once.
fn read_input(wake: u32) -> Receiver<Message> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut magic = [0u8; 4];
        if stdin.read_exact(&mut magic).is_err() || magic != MAGIC {
            eprintln!("teams-redir-video: not started by the add-in (bad stream header)");
            std::process::exit(2);
        }
        loop {
            match read_message(&mut stdin) {
                Ok(Some(m)) => {
                    if tx.send(m).is_err() {
                        break;
                    }
                    // SAFETY: SDL_PushEvent may be called from any thread.
                    unsafe {
                        let mut ev: SDL_Event = std::mem::zeroed();
                        ev.r#type = wake;
                        SDL_PushEvent(&mut ev);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("teams-redir-video: {e}");
                    break;
                }
            }
        }
        // The add-in is gone or the stream broke: wake the loop so it can see the channel closed.
        // SAFETY: as above.
        unsafe {
            let mut ev: SDL_Event = std::mem::zeroed();
            ev.r#type = wake;
            SDL_PushEvent(&mut ev);
        }
    });
    rx
}

fn main() {
    let check = std::env::var_os("TEAMS_REDIR_VIDEO_CHECK").is_some();
    // SAFETY: plain SDL setup calls; strings are NUL-terminated literals.
    let wake = unsafe {
        SDL_SetHint(SDL_HINT_APP_ID, c"teams-redir-video".as_ptr());
        SDL_SetAppMetadata(
            c"Teams video".as_ptr(),
            c"0.1".as_ptr(),
            c"teams-redir-video".as_ptr(),
        );
        if !SDL_Init(SDL_INIT_VIDEO) {
            eprintln!("teams-redir-video: cannot start SDL video: {}", sdl_error());
            std::process::exit(1);
        }
        SDL_RegisterEvents(1)
    };
    let rx = read_input(wake);
    let mut app = App {
        windows: HashMap::new(),
        pending: HashMap::new(),
        check,
    };
    'run: loop {
        // SAFETY: `ev` is written by SDL before it is read.
        let mut ev: SDL_Event = unsafe { std::mem::zeroed() };
        // Wait for anything, but not forever: a closed channel is noticed through the wake event.
        let got = unsafe { SDL_WaitEventTimeout(&mut ev, 250) };
        if got {
            app.on_sdl_event(&ev);
            if SDL_EventType(unsafe { ev.r#type }) == SDL_EVENT_QUIT {
                break 'run;
            }
        }
        // Everything that has arrived: controls in order, and only the newest frame of a window.
        let mut batch = Vec::new();
        loop {
            match rx.recv_timeout(Duration::ZERO) {
                Ok(m) => batch.push(m),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    for m in coalesce(batch) {
                        app.handle(m);
                    }
                    break 'run;
                }
            }
        }
        for m in coalesce(batch) {
            app.handle(m);
        }
    }
    for (_, w) in app.windows.drain() {
        w.destroy();
    }
    // SAFETY: nothing of SDL's is used after this.
    unsafe { SDL_Quit() };
}

/// Drops frames that a later frame of the same window supersedes; everything else stays in order.
fn coalesce(batch: Vec<Message>) -> Vec<Message> {
    let mut newest: HashMap<u32, usize> = HashMap::new();
    for (i, m) in batch.iter().enumerate() {
        if matches!(m, Message::Frame { .. }) {
            newest.insert(m.window(), i);
        }
    }
    batch
        .into_iter()
        .enumerate()
        .filter(|(i, m)| !matches!(m, Message::Frame { .. }) || newest.get(&m.window()) == Some(i))
        .map(|(_, m)| m)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contain_letterboxes_cover_crops_fill_stretches() {
        let close = |a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)| {
            [a.0 - b.0, a.1 - b.1, a.2 - b.2, a.3 - b.3]
                .iter()
                .all(|d| d.abs() < 0.01)
        };
        // A 16:9 picture in a square 100x100 output.
        let pic = (160.0, 90.0);
        let out = (100.0, 100.0);
        let (src, dst) = place(Fit::Contain, pic, out);
        assert!(close(src, (0.0, 0.0, 160.0, 90.0)));
        assert!(close(dst, (0.0, 21.875, 100.0, 56.25)), "{dst:?}");
        // Cover shows the middle 90x90 of the picture, over the whole output.
        let (src, dst) = place(Fit::Cover, pic, out);
        assert!(close(dst, (0.0, 0.0, 100.0, 100.0)));
        assert!(close(src, (35.0, 0.0, 90.0, 90.0)), "{src:?}");
        let (src, dst) = place(Fit::Fill, pic, out);
        assert!(close(src, (0.0, 0.0, 160.0, 90.0)));
        assert!(close(dst, (0.0, 0.0, 100.0, 100.0)));
    }

    #[test]
    fn only_the_newest_frame_per_window_survives() {
        let f = |w: u32, tag: u8| Message::Frame {
            window: w,
            width: 2,
            height: 2,
            planes: vec![tag; 6],
        };
        let kept = coalesce(vec![
            f(1, 1),
            Message::Mirror {
                window: 1,
                mirrored: true,
            },
            f(1, 2),
            f(2, 3),
            Message::Close { window: 2 },
        ]);
        assert_eq!(
            kept,
            vec![
                Message::Mirror {
                    window: 1,
                    mirrored: true
                },
                f(1, 2),
                f(2, 3),
                Message::Close { window: 2 },
            ]
        );
    }
}
