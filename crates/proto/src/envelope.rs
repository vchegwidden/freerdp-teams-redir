//! `webrtc.1` request, response and event envelopes.
//!
//! Request keys (win-rpc §2) [O]: `rpcCallId`, `rpcName`, `rpcObjectType`, `rpcObjectId`,
//! `rpcArgs`. Event keys (mac-decompile §3.2) [O]: `rpcEventTarget.rpcObjectType`,
//! `rpcEventTarget.rpcObjectId`, `rpcEventName`, `rpcEventArgs.*`.
//!
//! A real host transcript (2026-10-29) showed that `rpcCallId` is optional: a request without one
//! is a notification and gets no reply. `setVersionInfo` is one, and carries its payload at the
//! top level instead of in `rpcArgs`. `rpcArgs` is an array for some calls and an object for others.
//!
//! The macOS response and event writers emit `rpcObjectId` only when it is non-zero
//! (`if d8 != 0` in 0x1010ab61c) [O], so this does too.
//!
//! Replies and events follow the Windows add-in (`msrdc-reply-structures.md` §1, read from its
//! decompiled code [O]): a reply is a fresh object `{result?, errorDetails?, rpcObjectType,
//! rpcObjectId?, rpcName, rpcCallId, hr}`. `hr` is the HRESULT as a **signed** decimal number
//! (`0x80070490` is `-2147023728`). `result` is only there when the method has content, and
//! `errorDetails` only for failures with a message. Events carry `hr: 0` too.

use std::fmt;

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct RpcRequest {
    /// `None` for a notification. Echoed unchanged in the reply.
    pub call_id: Option<Value>,
    pub name: String,
    pub object_type: String,
    /// Echoed unchanged. The macOS client reads it as a double, so keep the JSON value.
    pub object_id: Option<Value>,
    /// `rpcArgs`: an array, an object, or `Null` if absent.
    pub args: Value,
    /// The whole request, for calls that carry their payload at the top level.
    pub raw: Value,
}

impl RpcRequest {
    /// Positional arguments, if `rpcArgs` is an array.
    pub fn args_array(&self) -> &[Value] {
        self.args.as_array().map_or(&[], Vec::as_slice)
    }

    /// A top-level field of the request, such as `version` in `setVersionInfo`.
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }
}

#[derive(Debug)]
pub enum RequestError {
    Json(serde_json::Error),
    NotAnObject,
    Missing(&'static str),
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestError::Json(e) => write!(f, "invalid JSON: {e}"),
            RequestError::NotAnObject => f.write_str("request is not a JSON object"),
            RequestError::Missing(k) => write!(f, "missing {k}"),
        }
    }
}

impl std::error::Error for RequestError {}

pub fn parse_request(text: &str) -> Result<RpcRequest, RequestError> {
    let value: Value = serde_json::from_str(text).map_err(RequestError::Json)?;
    let obj = value.as_object().ok_or(RequestError::NotAnObject)?;
    let string = |key: &'static str| {
        obj.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(RequestError::Missing(key))
    };
    let present = |key: &str| obj.get(key).filter(|v| !v.is_null()).cloned();
    Ok(RpcRequest {
        call_id: present("rpcCallId"),
        name: string("rpcName")?,
        object_type: string("rpcObjectType")?,
        object_id: present("rpcObjectId"),
        args: obj.get("rpcArgs").cloned().unwrap_or(Value::Null),
        raw: value,
    })
}

/// True for a non-zero number or any non-null non-number.
fn is_set(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => true,
    }
}

fn identity(req: &RpcRequest, call_id: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("rpcCallId".into(), call_id.clone());
    m.insert("rpcObjectType".into(), json!(req.object_type));
    if let Some(id) = req.object_id.as_ref().filter(|v| is_set(v)) {
        m.insert("rpcObjectId".into(), id.clone());
    }
    m.insert("rpcName".into(), json!(req.name));
    m
}

fn reply(
    req: &RpcRequest,
    hr: u32,
    result: Option<Value>,
    error_details: Option<&str>,
) -> Option<String> {
    let mut m = identity(req, req.call_id.as_ref()?);
    if let Some(result) = result {
        m.insert("result".into(), result);
    }
    if let Some(text) = error_details {
        m.insert("errorDetails".into(), json!(text));
    }
    m.insert("hr".into(), json!(hr as i32));
    Some(Value::Object(m).to_string())
}

/// A success with content in `result`. `None` for a notification, which gets no reply.
pub fn ok_response(req: &RpcRequest, result: Value) -> Option<String> {
    reply(req, 0, Some(result), None)
}

/// A success with no `result` at all, which is most "set" calls.
pub fn ok_empty(req: &RpcRequest) -> Option<String> {
    reply(req, 0, None, None)
}

/// A failure whose `result` carries the details (`RtcError`, `RtcErrorDetails`).
pub fn error_with_result(req: &RpcRequest, hr: u32, result: Value) -> Option<String> {
    reply(req, hr, Some(result), None)
}

/// A failure. Handlers that have a message put it in `errorDetails`; an unknown method has none.
pub fn error_response(req: &RpcRequest, hr: u32, details: Option<&str>) -> Option<String> {
    reply(req, hr, None, details)
}

/// The reply of an asynchronous method (`createOffer`, `createAnswer`, `setLocalDescription`,
/// `setRemoteDescription`, `getStats`): sent from a libwebrtc callback with only `result`,
/// `rpcCallId` and `hr`, no echo of the object or method (`msrdc-reply-structures.md` §3c) [O].
pub fn async_reply(call_id: &Value, hr: u32, result: Option<Value>) -> String {
    let mut m = Map::new();
    if let Some(result) = result {
        m.insert("result".into(), result);
    }
    m.insert("rpcCallId".into(), call_id.clone());
    m.insert("hr".into(), json!(hr as i32));
    Value::Object(m).to_string()
}

/// Event from client to host. A zero or absent `object_id` is left out, as the macOS client does,
/// and so is `rpcEventArgs` when `args` is null (`negotiationneeded` has none).
pub fn event(object_type: &str, object_id: Option<&Value>, name: &str, args: Value) -> String {
    let mut target = Map::new();
    target.insert("rpcObjectType".into(), json!(object_type));
    if let Some(id) = object_id.filter(|v| is_set(v)) {
        target.insert("rpcObjectId".into(), id.clone());
    }
    let mut m = Map::new();
    if !args.is_null() {
        m.insert("rpcEventArgs".into(), args);
    }
    m.insert("rpcEventTarget".into(), Value::Object(target));
    m.insert("rpcEventName".into(), json!(name));
    m.insert("hr".into(), json!(0));
    Value::Object(m).to_string()
}

/// The macOS client rewrites `streams":{` into an array form before parsing
/// (mac-decompile §3.5; the exact replacement is [U]). Callers accept both shapes through
/// this helper instead of rewriting text.
pub fn one_or_many(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        other => vec![other],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALL: &str = r#"{"rpcCallId":7,"rpcName":"setTeamsCallInfo","rpcObjectType":"RDWebRTCRedirector","rpcObjectId":3,"rpcArgs":[{"callID":"x"}]}"#;

    // Shapes seen from a real host on 2026-10-29.
    const NOTIFICATION: &str = r#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"setVersionInfo","version":{"websocketService":"1.53.2408.19001","shim":"1.1.2602.23001"},"session":{"remoteApp":false},"features":{"appshareServiceSupport":"enabled"}}"#;
    const OBJECT_ARGS: &str = r#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"setTeamsAppVolume","rpcArgs":{"sessionId":0,"sessionVolume":1}}"#;

    #[test]
    fn async_replies_carry_no_echo() {
        let ok: Value =
            serde_json::from_str(&async_reply(&json!(47), 0, Some(json!({"desc": 1})))).unwrap();
        assert_eq!(ok, json!({"result": {"desc": 1}, "rpcCallId": 47, "hr": 0}));
        let failed: Value =
            serde_json::from_str(&async_reply(&json!(48), 0x8000_4005, None)).unwrap();
        assert_eq!(failed, json!({"rpcCallId": 48, "hr": -2147467259}));
    }

    #[test]
    fn parses_a_call() {
        let r = parse_request(CALL).unwrap();
        assert_eq!(r.call_id, Some(json!(7)));
        assert_eq!(r.name, "setTeamsCallInfo");
        assert_eq!(r.object_id, Some(json!(3)));
        assert_eq!(r.args_array().len(), 1);
    }

    #[test]
    fn parses_a_notification_with_top_level_payload() {
        let r = parse_request(NOTIFICATION).unwrap();
        assert_eq!(r.call_id, None);
        assert_eq!(r.object_id, None);
        assert!(r.args_array().is_empty());
        assert_eq!(r.field("version").unwrap()["shim"], json!("1.1.2602.23001"));
        assert_eq!(r.field("session").unwrap()["remoteApp"], json!(false));
    }

    #[test]
    fn rpc_args_may_be_an_object() {
        let r = parse_request(OBJECT_ARGS).unwrap();
        assert_eq!(r.args["sessionVolume"], json!(1));
        assert!(r.args_array().is_empty());
    }

    #[test]
    fn rejects_incomplete_requests() {
        assert!(matches!(
            parse_request("[]"),
            Err(RequestError::NotAnObject)
        ));
        assert!(matches!(parse_request("{"), Err(RequestError::Json(_))));
        assert!(matches!(
            parse_request(r#"{"rpcObjectType":"y"}"#),
            Err(RequestError::Missing("rpcName"))
        ));
    }

    #[test]
    fn notifications_get_no_reply() {
        let r = parse_request(NOTIFICATION).unwrap();
        assert_eq!(ok_response(&r, json!({})), None);
        assert_eq!(ok_empty(&r), None);
        assert_eq!(error_response(&r, 1, Some("x")), None);
    }

    #[test]
    fn hr_is_a_signed_number_and_errors_carry_details() {
        let r = parse_request(CALL).unwrap();
        let v: Value = serde_json::from_str(
            &error_response(&r, 0x8007_0490, Some("Track not found")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["rpcCallId"], json!(7));
        assert_eq!(v["rpcObjectId"], json!(3));
        assert_eq!(v["hr"], json!(-2147023728));
        assert_eq!(v["errorDetails"], json!("Track not found"));
        assert!(v.get("result").is_none());
        assert!(v.get("hresult").is_none());
    }

    #[test]
    fn success_has_hr_zero_and_result_only_when_given() {
        let r = parse_request(CALL).unwrap();
        let v: Value = serde_json::from_str(&ok_response(&r, json!([1])).unwrap()).unwrap();
        assert_eq!(v["hr"], json!(0));
        assert_eq!(v["result"], json!([1]));
        let v: Value = serde_json::from_str(&ok_empty(&r).unwrap()).unwrap();
        assert_eq!(v["hr"], json!(0));
        assert!(v.get("result").is_none());
        assert!(v.get("errorDetails").is_none());
    }

    #[test]
    fn replies_echo_identity_and_omit_zero_object_ids() {
        let zero =
            parse_request(r#"{"rpcCallId":"a","rpcName":"n","rpcObjectType":"t","rpcObjectId":0}"#)
                .unwrap();
        let v: Value = serde_json::from_str(&ok_empty(&zero).unwrap()).unwrap();
        assert_eq!(v["rpcCallId"], json!("a"));
        assert_eq!(v["rpcName"], json!("n"));
        assert_eq!(v["rpcObjectType"], json!("t"));
        assert!(v.get("rpcObjectId").is_none());
    }

    #[test]
    fn events_omit_the_object_id_when_zero_or_absent() {
        let v: Value =
            serde_json::from_str(&event("RDWebRTCRedirector", None, "sessioninfo", json!({})))
                .unwrap();
        assert!(v["rpcEventTarget"].get("rpcObjectId").is_none());
        assert_eq!(v["hr"], json!(0));
        let v: Value =
            serde_json::from_str(&event("MediaStream", Some(&json!(4)), "x", json!({}))).unwrap();
        assert_eq!(v["rpcEventTarget"]["rpcObjectId"], json!(4));
        let v: Value =
            serde_json::from_str(&event("MediaStream", Some(&json!(0)), "x", json!({}))).unwrap();
        assert!(v["rpcEventTarget"].get("rpcObjectId").is_none());
    }

    #[test]
    fn one_or_many_accepts_object_and_array() {
        let obj = json!({"id": 1});
        let arr = json!([{"id": 1}, {"id": 2}]);
        assert_eq!(one_or_many(&obj).len(), 1);
        assert_eq!(one_or_many(&arr).len(), 2);
        assert!(one_or_many(&Value::Null).is_empty());
    }
}
