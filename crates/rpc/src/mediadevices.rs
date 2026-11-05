//! `MediaDevices` and `HidManager`, the first calls the host makes once it accepts the endpoint.
//! Reply shapes are the Windows add-in's (`msrdc-reply-structures.md` §2 and §3g) [O].
//!
//! - `enumerateDevices` reads no arguments and replies `result` = a flat array of
//!   `{deviceId, groupId, kind, label}`.
//! - `HidManager` is Windows-only. This client advertises no HID, but the host calls it anyway, so
//!   it gets what the shipped client sends with no devices: `startWithDeviceFilter` and `stop`
//!   reply `result` = `"RPC succeeded."`, `enumerateDevices` replies `[]`. Other methods are left
//!   to the caller's unknown-method reply.

use proto::envelope::{self, RpcRequest};
use serde_json::json;

use crate::devices::{DeviceSource, media_device_infos};
use crate::redirector::Outcome;

pub const MEDIA_DEVICES: &str = "MediaDevices";
pub const HID_MANAGER: &str = "HidManager";

pub fn handle(devices: &dyn DeviceSource, req: &RpcRequest) -> Option<Outcome> {
    match (req.object_type.as_str(), req.name.as_str()) {
        (MEDIA_DEVICES, "enumerateDevices") => Some(enumerate_devices(devices, req)),
        (HID_MANAGER, "startWithDeviceFilter" | "stop") => Some(Outcome {
            replies: envelope::ok_response(req, json!("RPC succeeded."))
                .into_iter()
                .collect(),
            note: Some("HID is not supported on this client".into()),
        }),
        (HID_MANAGER, "enumerateDevices") => Some(Outcome {
            replies: envelope::ok_response(req, json!([])).into_iter().collect(),
            note: Some("no HID devices".into()),
        }),
        _ => None,
    }
}

fn enumerate_devices(devices: &dyn DeviceSource, req: &RpcRequest) -> Outcome {
    let (infos, note) = match devices.list() {
        Ok(list) => {
            let infos = media_device_infos(&list);
            let n = infos.as_array().map_or(0, Vec::len);
            (infos, format!("{n} devices"))
        }
        // An empty array is what the shipped client sends with no devices.
        Err(e) => (json!([]), format!("no devices: {e}")),
    };
    Outcome {
        replies: envelope::ok_response(req, infos).into_iter().collect(),
        note: Some(note),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::devices::fake::*;

    fn req(json: &str) -> RpcRequest {
        envelope::parse_request(json).unwrap()
    }

    const ENUM: &str = r#"{"rpcObjectType":"MediaDevices","rpcName":"enumerateDevices","rpcArgs":[],"rpcCallId":3}"#;

    fn reply(src: &FakeDevices, text: &str) -> Value {
        serde_json::from_str(&handle(src, &req(text)).unwrap().replies[0]).unwrap()
    }

    #[test]
    fn enumerate_devices_replies_with_a_flat_array_and_hr_zero() {
        let src = FakeDevices(Ok(vec![mic("m", true), speaker("s", true)]));
        let v = reply(&src, ENUM);
        assert_eq!(v["rpcCallId"], json!(3));
        assert_eq!(v["rpcObjectType"], json!("MediaDevices"));
        assert_eq!(v["rpcName"], json!("enumerateDevices"));
        assert_eq!(v["hr"], json!(0));
        assert_eq!(v["result"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn a_failing_sound_server_gives_an_empty_array_not_an_error() {
        let src = FakeDevices(Err("pactl: gone".into()));
        let out = handle(&src, &req(ENUM)).unwrap();
        let v: Value = serde_json::from_str(&out.replies[0]).unwrap();
        assert_eq!(v["result"], json!([]));
        assert_eq!(v["hr"], json!(0));
        assert!(out.note.unwrap().contains("gone"));
    }

    #[test]
    fn hid_replies_match_a_client_without_devices() {
        let src = FakeDevices(Ok(vec![]));
        let start = reply(
            &src,
            r#"{"rpcObjectType":"HidManager","rpcName":"startWithDeviceFilter","rpcArgs":[{"deviceFilter":1}],"rpcCallId":4}"#,
        );
        assert_eq!(start["result"], json!("RPC succeeded."));
        assert_eq!(start["hr"], json!(0));
        let list = reply(
            &src,
            r#"{"rpcObjectType":"HidManager","rpcName":"enumerateDevices","rpcArgs":{},"rpcCallId":5}"#,
        );
        assert_eq!(list["result"], json!([]));
    }

    #[test]
    fn other_methods_are_left_to_the_caller() {
        let src = FakeDevices(Ok(vec![]));
        for text in [
            r#"{"rpcObjectType":"MediaDevices","rpcName":"getScreensAsync","rpcCallId":5}"#,
            r#"{"rpcObjectType":"HidDevice","rpcName":"open","rpcArgs":[{"id":1}],"rpcCallId":6}"#,
        ] {
            assert!(handle(&src, &req(text)).is_none());
        }
    }
}
