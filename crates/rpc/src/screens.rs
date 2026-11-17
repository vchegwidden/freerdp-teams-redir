//! Screen sharing: the sources the host may share, and their previews. The capture itself is the
//! engine's (`MediaEngine::create_screen_capture`); `createMediaStream` sends a screen request
//! there (see `is_screen_source`).
//!
//! On Linux the desktop decides what is shared: on Wayland the capture asks xdg-desktop-portal,
//! which shows its own monitor/window picker. So this client lists a single source, "the
//! screen", and the real choice happens in the system dialog when Teams starts the share.
//!
//! Shapes are the Windows add-in's (`teams-optimization-windows-rpc-handlers.md`,
//! `msrdc-reply-structures.md` §2) [O], except where marked GUESS:
//! - `MediaDevices.getScreensAsync` / `getWindowsAsync` reply `result` = array of
//!   `{sourceId, sharingSourceType, description, boundsRect: {x, y, width, height}}`.
//! - `ScreenCaptureSource.getPreviewAsync` / `getIcon`: `rpcArgs[0]` = `{sourceId, width, height}`,
//!   `result` = `{base64PNGData}`; an unknown source is `preview not found for sourceId=%d` /
//!   `icon not found for sourceId=%d`.
//! - GUESS: the value of `sharingSourceType` for a monitor (`SharingSourceType::Monitor`, the
//!   first enumerator, so 0), and how Teams names the source when it creates the stream.

use proto::envelope::{self, RpcRequest};
use proto::hresult;
use serde_json::{Value, json};

use crate::peerconnection::{base64, failure};
use crate::redirector::Outcome;

pub const MEDIA_DEVICES: &str = "MediaDevices";
pub const SCREEN_CAPTURE_SOURCE: &str = "ScreenCaptureSource";

/// The one source listed: whatever the user picks in the system dialog.
pub const SCREEN_SOURCE_ID: u64 = 1;
/// GUESS: `SharingSourceType::Monitor`.
const SHARING_SOURCE_MONITOR: u64 = 0;

/// `None` for calls not handled here. `enabled` is the screen-share setting: without it the
/// source lists are empty.
pub fn handle(enabled: bool, req: &RpcRequest) -> Option<Outcome> {
    Some(match (req.object_type.as_str(), req.name.as_str()) {
        (MEDIA_DEVICES, "getScreensAsync") => {
            let list = if enabled { screens() } else { json!([]) };
            let n = list.as_array().map_or(0, Vec::len);
            reply(req, list, format!("{n} screens"))
        }
        // Windows are offered by the system picker along with screens, so none are listed.
        (MEDIA_DEVICES, "getWindowsAsync") => reply(req, json!([]), "no windows listed".into()),
        (MEDIA_DEVICES, "notifyWindowsSourcesFromHost" | "setScreenSharePanelId") => Outcome {
            replies: envelope::ok_empty(req).into_iter().collect(),
            note: Some("noted".into()),
        },
        (SCREEN_CAPTURE_SOURCE, "getPreviewAsync" | "getIcon") => image(req),
        _ => return None,
    })
}

fn reply(req: &RpcRequest, result: Value, note: String) -> Outcome {
    Outcome {
        replies: envelope::ok_response(req, result).into_iter().collect(),
        note: Some(note),
    }
}

/// True when a `createMediaStream` video request is for the screen, not a camera. Teams asks for
/// a camera by `mandatory.sourceId` and for the screen with no source at all
/// (`{resizeMode: "none", width: {max: 1920}, height: {max: 1080}, frameRate: 15}`, seen
/// 2026-11-17), which is the Windows add-in's rule too: no source id means `display`
/// (`teams-optimization-windows-rpc-handlers.md`). Also a source id this client listed, the names
/// used for the screen, or a Chrome-style desktop source.
pub fn is_screen_source(source: &str, video: &Value) -> bool {
    let named = matches!(source, "" | "display" | "screen" | "window" | "desktop")
        || source == SCREEN_SOURCE_ID.to_string()
        || source.starts_with("screen:")
        || source.starts_with("window:");
    let chrome = ["chromeMediaSource", "mediaSource"].iter().any(|k| {
        let v = video
            .get(*k)
            .or_else(|| video.get("mandatory").and_then(|m| m.get(*k)));
        matches!(
            v.and_then(Value::as_str),
            Some("desktop" | "screen" | "window")
        )
    });
    named || chrome
}

fn screens() -> Value {
    let (width, height) = monitor_size();
    json!([{
        "sourceId": SCREEN_SOURCE_ID,
        "sharingSourceType": SHARING_SOURCE_MONITOR,
        "description": "Screen (choose in the system dialog)",
        "boundsRect": { "x": 0, "y": 0, "width": width, "height": height },
    }])
}

/// The size of the first connected display, from the kernel's DRM connectors; 1920x1080 when it
/// cannot be read. Only Teams' picker shows it.
fn monitor_size() -> (u32, u32) {
    let read = || -> Option<(u32, u32)> {
        let mut dirs: Vec<_> = std::fs::read_dir("/sys/class/drm")
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        dirs.sort();
        dirs.iter().find_map(|d| {
            let status = std::fs::read_to_string(d.join("status")).ok()?;
            if status.trim() != "connected" {
                return None;
            }
            first_mode(&std::fs::read_to_string(d.join("modes")).ok()?)
        })
    };
    read().unwrap_or((1920, 1080))
}

/// The first `WxH` line of a DRM connector's `modes` file (the preferred mode).
fn first_mode(modes: &str) -> Option<(u32, u32)> {
    let line = modes.lines().next()?;
    let (w, h) = line.split_once('x')?;
    let h: String = h.chars().take_while(char::is_ascii_digit).collect();
    Some((w.trim().parse().ok()?, h.parse().ok()?))
}

/// `getPreviewAsync` / `getIcon`. A real thumbnail would need a capture, which on Wayland means the
/// portal dialog, so the preview is a plain tile of the asked size.
fn image(req: &RpcRequest) -> Outcome {
    let args = req.args_array().first().cloned().unwrap_or(Value::Null);
    let icon = req.name == "getIcon";
    let source = args["sourceId"].as_u64();
    if source != Some(SCREEN_SOURCE_ID) {
        let what = if icon { "icon" } else { "preview" };
        let id = args["sourceId"].as_i64().unwrap_or(-1);
        return failure(
            req,
            hresult::ERROR_NOT_FOUND,
            &format!("{what} not found for sourceId={id}"),
        );
    }
    let dim = |k: &str, default: u32| {
        args[k]
            .as_u64()
            .map(|v| v as u32)
            .filter(|v| *v > 0)
            .unwrap_or(default)
            .min(640)
    };
    let (w, h) = if icon {
        (dim("width", 32), dim("height", 32))
    } else {
        (dim("width", 320), dim("height", 180))
    };
    let png = solid_png(w, h, [0x2b, 0x2d, 0x33]);
    reply(
        req,
        json!({ "base64PNGData": base64(&png) }),
        format!("{w}x{h} placeholder"),
    )
}

/// A `w` x `h` PNG of one colour (deflate "stored" blocks, so no compressor is needed).
fn solid_png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let row: Vec<u8> = std::iter::once(0) // filter: none
        .chain(rgb.iter().copied().cycle().take(3 * w as usize))
        .collect();
    let raw = row.repeat(h as usize);
    let mut zlib = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        zlib.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = chunks.next() {
        zlib.push(u8::from(chunks.peek().is_none()));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", &ihdr), (b"IDAT", &zlib), (b"IEND", &Vec::new())] {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = png.len();
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let crc = crc32(&png[start..]);
        png.extend_from_slice(&crc.to_be_bytes());
    }
    png
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(json: &str) -> RpcRequest {
        envelope::parse_request(json).unwrap()
    }

    fn reply_of(enabled: bool, text: &str) -> Value {
        serde_json::from_str(&handle(enabled, &req(text)).unwrap().replies[0]).unwrap()
    }

    #[test]
    fn one_screen_is_listed_with_the_windows_entry_shape() {
        let v = reply_of(
            true,
            r#"{"rpcObjectType":"MediaDevices","rpcName":"getScreensAsync","rpcArgs":[],"rpcCallId":7}"#,
        );
        assert_eq!(v["hr"], json!(0));
        let list = v["result"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["sourceId"], json!(SCREEN_SOURCE_ID));
        assert!(list[0]["boundsRect"]["width"].as_u64().unwrap() > 0);
        assert!(list[0]["description"].is_string());
        let off = reply_of(
            false,
            r#"{"rpcObjectType":"MediaDevices","rpcName":"getScreensAsync","rpcCallId":8}"#,
        );
        assert_eq!(off["result"], json!([]));
        let windows = reply_of(
            true,
            r#"{"rpcObjectType":"MediaDevices","rpcName":"getWindowsAsync","rpcCallId":9}"#,
        );
        assert_eq!(windows["result"], json!([]));
    }

    #[test]
    fn a_preview_is_a_png_of_the_asked_size() {
        let v = reply_of(
            true,
            r#"{"rpcObjectType":"ScreenCaptureSource","rpcName":"getPreviewAsync","rpcArgs":[{"sourceId":1,"width":160,"height":90}],"rpcCallId":10}"#,
        );
        assert_eq!(v["hr"], json!(0));
        let b64 = v["result"]["base64PNGData"].as_str().unwrap();
        assert!(b64.starts_with("iVBORw0KGgo"), "{b64}");
        let png = solid_png(160, 90, [0x2b, 0x2d, 0x33]);
        assert_eq!(&png[16..24], &[0, 0, 0, 160, 0, 0, 0, 90]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn an_unknown_source_has_no_preview() {
        let v = reply_of(
            true,
            r#"{"rpcObjectType":"ScreenCaptureSource","rpcName":"getIcon","rpcArgs":[{"sourceId":9}],"rpcCallId":11}"#,
        );
        assert_ne!(v["hr"], json!(0));
    }

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn screen_requests_are_told_from_camera_requests() {
        let none = json!({});
        assert!(is_screen_source("display", &none));
        assert!(is_screen_source("1", &none));
        assert!(is_screen_source("screen:0:0", &none));
        assert!(is_screen_source(
            "",
            &json!({"mandatory": {"chromeMediaSource": "desktop"}})
        ));
        assert!(!is_screen_source("v4l2:usb-0000:00:14.0-3:0", &none));
        assert!(is_screen_source(
            "",
            &json!({"resizeMode": "none", "width": {"max": 1920}, "height": {"max": 1080}, "frameRate": 15})
        ));
        assert!(!is_screen_source("default", &json!({"width": 1280})));
    }

    #[test]
    fn the_preferred_mode_is_the_first_line() {
        assert_eq!(first_mode("2560x1600\n1920x1080\n"), Some((2560, 1600)));
        assert_eq!(first_mode("1920x1080i\n"), Some((1920, 1080)));
        assert_eq!(first_mode(""), None);
    }
}
