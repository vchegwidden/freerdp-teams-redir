//! `RDWebRTCRedirector`: the host-side service's own object. Observed from a real host on
//! 2026-10-29 (`websocketService` 1.53.2408.19001, `shim` 1.1.2602.23001):
//!
//! - `setVersionInfo` is a notification carrying `version`, `session` and `features` at the top
//!   level. We answer with the `sessioninfo` event (mac-decompile §3.3).
//! - `rtcSession` is a call with four positional string arguments (a hex id, a millisecond
//!   timestamp, a 64-digit hex value and the shim version). The Windows add-in does not know it and
//!   answers with the unknown-method error, which is what the core sends.
//! - `setTeamsAppVolume` is a notification with an object in `rpcArgs`.
//! - `playNotifyAudio` / `pauseNotifyAudio` are calls that need an audio path; not handled yet.
//!
//! The `sessioninfo` event follows the Windows add-in's key set (`msrdc-reply-structures.md` §5):
//! `"enabled"` / `"disabled"` strings, `microphone` left out when there is a capture device and
//! `"disabled"` when there is none. The host refuses an endpoint whose microphone is `"disabled"`
//! ("endpoint microphone access is disabled"), so Teams shows "Not optimized" in that case.
//!
//! For that, `<state dir>/sessioninfo-patch.json` is read on every `setVersionInfo` and applied to
//! `rpcEventArgs` as a JSON merge patch (RFC 7386: objects merge, `null` removes a key). Restarting
//! Teams is enough to try a new patch; no reconnect is needed.

use proto::envelope::{self, RpcRequest};
use proto::patch;
use serde_json::{Value, json};

use crate::config::{Config, SessionInfo};
use crate::devices::{DeviceKind, DeviceSource};

pub const OBJECT_TYPE: &str = "RDWebRTCRedirector";

/// What a handler wants done: messages to send, and a line for the log.
pub struct Outcome {
    pub replies: Vec<String>,
    pub note: Option<String>,
}

/// Returns `None` for methods this object does not handle.
pub fn handle(
    config: &Config,
    session: &SessionInfo,
    devices: &dyn DeviceSource,
    req: &RpcRequest,
) -> Option<Outcome> {
    match req.name.as_str() {
        "setVersionInfo" => Some(set_version_info(config, session, devices, req)),
        // `rtcSession` is not in the Windows add-in; the unknown-method reply it gets from the core
        // is what the shipped client sends (`msrdc-reply-structures.md` §3a). Its arguments are
        // logged by the core.
        //
        // Accepted and logged (plan §4.2); the volume is applied once there is playout.
        "setTeamsAppVolume" => Some(Outcome {
            replies: envelope::ok_empty(req).into_iter().collect(),
            note: Some(format!("{} {}", req.name, req.args)),
        }),
        // Asynchronous in the shipped client: no reply, even to a call.
        "setTeamsCallInfo" => Some(Outcome {
            replies: Vec::new(),
            note: Some(format!("{} {}", req.name, req.args)),
        }),
        _ => None,
    }
}

fn set_version_info(
    config: &Config,
    session: &SessionInfo,
    devices: &dyn DeviceSource,
    req: &RpcRequest,
) -> Outcome {
    let microphone = has_microphone(devices);
    let (args, patch_note) = patched_session_info_args(config, session, microphone);
    let note = format!(
        "host: version={} session={} features={}; {patch_note}",
        req.field("version").unwrap_or(&Value::Null),
        req.field("session").unwrap_or(&Value::Null),
        req.field("features").unwrap_or(&Value::Null),
    );
    // `setVersionInfo` is answered by the event alone; the shipped client sends no RPC reply to it
    // (`msrdc-reply-structures.md` §1.4).
    Outcome {
        replies: vec![envelope::event(
            OBJECT_TYPE,
            req.object_id.as_ref(),
            "sessioninfo",
            args,
        )],
        note: Some(note),
    }
}

/// Whether the client can offer a microphone. The host refuses an endpoint without one
/// ("endpoint microphone access is disabled"), so this is the honest answer. If the sound server
/// cannot be asked, the answer is yes: a broken query should not cost the user optimization.
fn has_microphone(devices: &dyn DeviceSource) -> bool {
    devices
        .list()
        .map(|l| l.iter().any(|d| d.kind == DeviceKind::AudioInput))
        .unwrap_or(true)
}

pub const PATCH_FILE: &str = "sessioninfo-patch.json";

/// The default arguments with the patch file applied, and a line for the log saying what happened.
fn patched_session_info_args(
    config: &Config,
    session: &SessionInfo,
    microphone: bool,
) -> (Value, String) {
    let mut args = session_info_args(config, session, microphone);
    let note = patch::apply_file(&config.state_dir, PATCH_FILE, "sessioninfo", &mut args);
    (args, note)
}

/// Every feature `disabled` except `unifiedplan`; `microphone` is left out when there is an input device; `clipRectMode` disabled and no simulcast.
pub fn session_info_args(config: &Config, session: &SessionInfo, microphone: bool) -> Value {
    let id = &config.identity;
    let mut info = json!({
        "display": { "width": session.width, "height": session.height },
        "version": {
            "clientPlatform": id.client_platform,
            "clientAppName": id.client_app_name,
            "clientAppVersion": id.client_app_version,
            "clientFlavor": id.client_flavor,
            "webRTC": id.web_rtc,
            "webRTCRedirector": id.web_rtc_redirector,
        },
        "session": { "vdiMode": 0, "endpointId": config.endpoint_id() },
        "features": {
            "clipRectMode": "disabled",
            "screenshare": "disabled",
            "multimonitorscreenshare": "disabled",
                        "unifiedplan": "enabled",
            "datachannel": "disabled",
            "backgroundeffects": "disabled",
            "givecontrolv2": "disabled",
            "mirrormyselfvideo": "disabled",
            "secondaryringer": "disabled",
            "maxSimulcastLayers": 0,
        },
    });
    // The shipped client writes `"disabled"` when it has no capture access and leaves the key out
    // otherwise (`msrdc-reply-structures.md` §5); the host refuses the endpoint on `"disabled"`.
    if !microphone {
        info["features"]["microphone"] = json!("disabled");
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::fake::*;

    fn config() -> Config {
        let dir =
            std::env::temp_dir().join(format!("teams-redir-redirector-{}", std::process::id()));
        let dir = dir.to_string_lossy().into_owned();
        Config::from_lookup(move |k| match k {
            "TEAMS_REDIR_STATE_DIR" => Some(dir.clone()),
            _ => None,
        })
    }

    fn cleanup() {
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("teams-redir-redirector-{}", std::process::id())),
        );
    }

    const VERSION_INFO: &str = r#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"setVersionInfo","version":{"websocketService":"1.53.2408.19001","shim":"1.1.2602.23001","hostOS":"Version 2009 (OS Build 26200.9711)"},"session":{"remoteApp":false},"features":{"appshareServiceSupport":"enabled","givecontrolv2ServiceSupport":"enabled"}}"#;

    #[test]
    fn the_real_set_version_info_notification_gets_only_sessioninfo() {
        let req = envelope::parse_request(VERSION_INFO).unwrap();
        let session = SessionInfo {
            width: 1920,
            height: 1080,
            ..Default::default()
        };
        let out = handle(
            &config(),
            &session,
            &FakeDevices(Ok(vec![mic("m", true)])),
            &req,
        )
        .unwrap();
        assert_eq!(out.replies.len(), 1);
        let ev: Value = serde_json::from_str(&out.replies[0]).unwrap();
        assert_eq!(ev["rpcEventName"], json!("sessioninfo"));
        assert_eq!(
            ev["rpcEventTarget"]["rpcObjectType"],
            json!("RDWebRTCRedirector")
        );
        assert!(ev["rpcEventTarget"].get("rpcObjectId").is_none());
        let args = &ev["rpcEventArgs"];
        assert_eq!(args["display"]["width"], json!(1920));
        assert_eq!(args["features"]["unifiedplan"], json!("enabled"));
        assert!(args["features"].get("microphone").is_none());
        assert_eq!(args["session"]["vdiMode"], json!(0));
        assert_eq!(ev["hr"], json!(0));
        assert_eq!(args["features"]["maxSimulcastLayers"], json!(0));
        assert!(args["session"]["endpointId"].as_str().unwrap().len() >= 32);
        assert!(out.note.unwrap().contains("1.1.2602.23001"));
        cleanup();
    }

    #[test]
    fn patch_file_is_applied_on_each_handshake_and_bad_files_are_ignored() {
        let dir = std::env::temp_dir().join(format!("teams-redir-patch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.to_string_lossy().into_owned();
        let config =
            Config::from_lookup(move |k| (k == "TEAMS_REDIR_STATE_DIR").then(|| state.clone()));
        let session = SessionInfo::default();

        let (_, note) = patched_session_info_args(&config, &session, true);
        assert_eq!(note, "no sessioninfo patch");

        std::fs::write(
            dir.join(PATCH_FILE),
            r#"{"features":{"datachannel":"enabled"},"version":{"webRTC":"x"}}"#,
        )
        .unwrap();
        let (args, note) = patched_session_info_args(&config, &session, true);
        assert_eq!(args["features"]["datachannel"], json!("enabled"));
        assert_eq!(args["features"]["unifiedplan"], json!("enabled")); // untouched keys stay
        assert_eq!(args["version"]["webRTC"], json!("x"));
        assert!(note.starts_with("sessioninfo patch applied"));

        // Edited between handshakes: the next one sees the new content.
        std::fs::write(dir.join(PATCH_FILE), r#"{"features":{"datachannel":null}}"#).unwrap();
        let (args, _) = patched_session_info_args(&config, &session, true);
        assert!(args["features"].get("datachannel").is_none());

        std::fs::write(dir.join(PATCH_FILE), "{ nope").unwrap();
        let (args, note) = patched_session_info_args(&config, &session, true);
        assert_eq!(args["features"]["datachannel"], json!("disabled"));
        assert!(note.contains("ignored"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn microphone_follows_the_input_devices() {
        let cfg = config();
        let s = SessionInfo::default();
        let mic_of = |src: FakeDevices| {
            let req = envelope::parse_request(VERSION_INFO).unwrap();
            let out = handle(&cfg, &s, &src, &req).unwrap();
            let ev: Value = serde_json::from_str(&out.replies[0]).unwrap();
            ev["rpcEventArgs"]["features"]["microphone"].clone()
        };
        assert_eq!(mic_of(FakeDevices(Ok(vec![mic("m", true)]))), Value::Null);
        assert_eq!(
            mic_of(FakeDevices(Ok(vec![speaker("s", true)]))),
            json!("disabled")
        );
        assert_eq!(mic_of(FakeDevices(Err("down".into()))), Value::Null);
        cleanup();
    }

    fn run(text: &str) -> Option<Outcome> {
        let req = envelope::parse_request(text).unwrap();
        handle(
            &config(),
            &SessionInfo::default(),
            &FakeDevices(Ok(vec![])),
            &req,
        )
    }

    #[test]
    fn rtc_session_is_left_to_the_unknown_method_reply() {
        assert!(
            run(r#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"rtcSession","rpcArgs":["04c2","1793283854666","048c","1.1.2602.23001"],"rpcCallId":43}"#)
                .is_none()
        );
    }

    #[test]
    fn volume_notification_gets_no_reply_and_a_call_gets_hr_zero() {
        let out = run(
            r#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"setTeamsAppVolume","rpcArgs":{"sessionId":0,"sessionVolume":1}}"#,
        )
        .unwrap();
        assert!(out.replies.is_empty());
        assert!(out.note.is_some());

        let out = run(
            r#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"setTeamsAppVolume","rpcArgs":{"sessionId":0,"sessionVolume":1},"rpcCallId":8}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out.replies[0]).unwrap();
        assert_eq!(v["hr"], json!(0));
        assert!(v.get("result").is_none());
    }

    #[test]
    fn set_teams_call_info_is_never_answered() {
        let out = run(
            r#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"setTeamsCallInfo","rpcArgs":[{}],"rpcCallId":9}"#,
        )
        .unwrap();
        assert!(out.replies.is_empty());
    }

    #[test]
    fn unknown_methods_are_left_to_the_caller() {
        let req = envelope::parse_request(
            r#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"playNotifyAudio","rpcCallId":3}"#,
        )
        .unwrap();
        assert!(
            handle(
                &config(),
                &SessionInfo::default(),
                &FakeDevices(Ok(vec![])),
                &req
            )
            .is_none()
        );
    }
}
