//! `cpc.1`: the SlimCore hello. Messages are bare JSON of the form
//! `{"contentType":"scpie","messageName":"<Name>",...}` [O].
//!
//! The macOS Store plugin answers every `VMHello` with a `ClientHello` that carries an error
//! code and never loads SlimCore, and the host then falls back to `webrtc.1` (mac-decompile
//! §2.4) [O for the code path, I for the fallback].

use serde_json::{Map, Value, json};

use crate::errc;

pub const CONTENT_TYPE: &str = "scpie";

#[derive(Debug, Clone, PartialEq)]
pub struct VmHello {
    pub request_id: u32,
    /// `/version`, logged as `scVer`.
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    VmHello(VmHello),
    Ping,
    VmGoodbye,
    Other(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Json,
    NotScpie,
    MissingName,
    /// `VMHello` without a numeric `requestId`. The macOS stub returns silently here.
    BadVmHello,
}

pub fn parse(text: &str) -> Result<Message, ParseError> {
    let v: Value = serde_json::from_str(text).map_err(|_| ParseError::Json)?;
    if v.get("contentType").and_then(Value::as_str) != Some(CONTENT_TYPE) {
        return Err(ParseError::NotScpie);
    }
    let name = v
        .get("messageName")
        .and_then(Value::as_str)
        .ok_or(ParseError::MissingName)?;
    Ok(match name {
        "VMHello" => {
            let request_id = v
                .get("requestId")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(ParseError::BadVmHello)?;
            Message::VmHello(VmHello {
                request_id,
                version: v.get("version").and_then(Value::as_str).map(str::to_owned),
            })
        }
        "Ping" => Message::Ping,
        "VMGoodbye" => Message::VmGoodbye,
        other => Message::Other(other.to_owned()),
    })
}

/// Fields of `vdi::comm::BuildClientHello` (libMsTeamsPluginAvd 0x6480) that this client
/// sets. Optional fields are omitted when `None`, as the original omits empty strings and
/// zero error codes. Not set: `scPlatform`, `scHostDeploy_errc`, `scHostVersion`, the
/// `system*` fields (hardware identifiers) and `browser_errc` / `browserUserAgent`.
#[derive(Debug, Clone, Default)]
pub struct ClientHelloArgs {
    pub request_id: u32,
    pub platform: String,
    /// macOS sends 5. What the other values mean is [U].
    pub vdi_vendor: i32,
    /// Plugin `errc` (unencoded). `None` or `Some(0)` omits `load_errc`.
    pub load_errc: Option<i32>,
    /// Written as `install_errc`.
    pub deploy_errc: Option<i32>,
    pub rd_client_version: Option<String>,
    /// Spelled `rdClientProcuctName` on the wire [O].
    pub rd_client_product_name: Option<String>,
    pub rd_client_process_name: Option<String>,
    pub rd_client_sandboxed: bool,
    pub client_os_version: Option<String>,
    pub node_id: Option<u64>,
    pub version: Option<String>,
    pub plugin_version: Option<String>,
    pub telemetry: Option<String>,
    pub diagnostics_correlation_id: Option<String>,
    pub is_seamless: bool,
}

fn start(name: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("contentType".into(), json!(CONTENT_TYPE));
    m.insert("messageName".into(), json!(name));
    m
}

fn put_nonempty(m: &mut Map<String, Value>, key: &str, v: &Option<String>) {
    if let Some(s) = v.as_deref().filter(|s| !s.is_empty()) {
        m.insert(key.into(), json!(s));
    }
}

fn put_errc(m: &mut Map<String, Value>, key: &str, v: Option<i32>) {
    if let Some(code) = v.filter(|&c| c != 0) {
        m.insert(key.into(), json!(errc::encode_plugin(code)));
    }
}

/// Keys are inserted in the order of `BuildClientHello`.
pub fn client_hello_value(a: &ClientHelloArgs) -> Value {
    let mut m = start("ClientHello");
    m.insert("requestId".into(), json!(a.request_id));
    m.insert("platform".into(), json!(a.platform));
    m.insert("vdiVendor".into(), json!(a.vdi_vendor));
    put_errc(&mut m, "load_errc", a.load_errc);
    put_errc(&mut m, "install_errc", a.deploy_errc);
    put_nonempty(&mut m, "rdClientVersion", &a.rd_client_version);
    put_nonempty(&mut m, "rdClientProcuctName", &a.rd_client_product_name);
    put_nonempty(&mut m, "rdClientProcessName", &a.rd_client_process_name);
    m.insert("rdClientSandboxed".into(), json!(a.rd_client_sandboxed));
    put_nonempty(&mut m, "clientOsVersion", &a.client_os_version);
    if let Some(id) = a.node_id.filter(|&n| n != 0) {
        m.insert("node_id".into(), json!(id));
    }
    put_nonempty(&mut m, "version", &a.version);
    put_nonempty(&mut m, "pluginVersion", &a.plugin_version);
    put_nonempty(&mut m, "telemetry", &a.telemetry);
    put_nonempty(
        &mut m,
        "diagnosticsCorrelationId",
        &a.diagnostics_correlation_id,
    );
    m.insert("isSeamless".into(), json!(a.is_seamless));
    Value::Object(m)
}

pub fn build_client_hello(a: &ClientHelloArgs) -> String {
    client_hello_value(a).to_string()
}

/// Reply to `Ping`. The VM side holds the `{"contentType":"scpie","messageName":"Pong"`
/// prefix (win-findings §4) [O strings, I role]; extra fields, if any, are [U].
pub fn build_pong() -> String {
    Value::Object(start("Pong")).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vmhello_and_friends() {
        let hello =
            r#"{"contentType":"scpie","messageName":"VMHello","requestId":42,"version":"2026.37"}"#;
        assert_eq!(
            parse(hello),
            Ok(Message::VmHello(VmHello {
                request_id: 42,
                version: Some("2026.37".into())
            }))
        );
        assert_eq!(
            parse(r#"{"contentType":"scpie","messageName":"Ping"}"#),
            Ok(Message::Ping)
        );
        assert_eq!(
            parse(r#"{"contentType":"scpie","messageName":"VMGoodbye"}"#),
            Ok(Message::VmGoodbye)
        );
        assert_eq!(
            parse(r#"{"contentType":"scpie","messageName":"Zzz"}"#),
            Ok(Message::Other("Zzz".into()))
        );
    }

    #[test]
    fn rejects_foreign_or_broken_messages() {
        assert_eq!(parse("not json"), Err(ParseError::Json));
        assert_eq!(
            parse(r#"{"messageName":"VMHello"}"#),
            Err(ParseError::NotScpie)
        );
        assert_eq!(
            parse(r#"{"contentType":"scpie","messageName":"VMHello"}"#),
            Err(ParseError::BadVmHello)
        );
    }

    #[test]
    fn client_hello_matches_the_macos_store_shape() {
        let args = ClientHelloArgs {
            request_id: 5,
            platform: "mac-arm64".into(),
            vdi_vendor: 5,
            load_errc: Some(errc::CLIENT_APP_STORE_BUILD),
            deploy_errc: Some(errc::CLIENT_APP_STORE_BUILD),
            plugin_version: Some("2026.23.01.1".into()),
            ..Default::default()
        };
        let s = build_client_hello(&args);
        assert!(s.starts_with(r#"{"contentType":"scpie","messageName":"ClientHello","requestId":5,"platform":"mac-arm64","vdiVendor":5,"load_errc":16066,"install_errc":16066,"#), "{s}");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["pluginVersion"], json!("2026.23.01.1"));
        assert_eq!(v["rdClientSandboxed"], json!(false));
        assert!(v.get("rdClientVersion").is_none());
    }

    #[test]
    fn zero_error_codes_are_omitted() {
        let args = ClientHelloArgs {
            load_errc: Some(0),
            ..Default::default()
        };
        let v: Value = serde_json::from_str(&build_client_hello(&args)).unwrap();
        assert!(v.get("load_errc").is_none());
        assert!(v.get("install_errc").is_none());
    }

    #[test]
    fn pong_has_the_expected_prefix() {
        assert_eq!(
            build_pong(),
            r#"{"contentType":"scpie","messageName":"Pong"}"#
        );
    }
}
