use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread::{self, JoinHandle};

use media::MediaEngine;
use proto::cpc::{self, ClientHelloArgs, Message};
use proto::envelope;
use proto::frame;
use proto::hresult;
use proto::patch;
use transcript::{Dir, Transcript};

use crate::callalert::CallAlerts;
use crate::config::{self, Config, SessionInfo};
use crate::devices::{DeviceSource, SystemDevices};
use crate::host::{Channel, ConnId, Host, LogLevel};
use crate::media_objects;
use crate::mediadevices;
use crate::notify::{self, Notifier};
use crate::peerconnection::{self, Deferred, PeerConnections};
use crate::redirector;

/// Merge patch for the `ClientHello`, read at every `VMHello`; see `proto::patch`.
pub const CLIENT_HELLO_PATCH: &str = "clienthello-patch.json";

enum Input {
    Opened(ConnId, Channel),
    Data(ConnId, Vec<u8>),
    Closed(ConnId),
    SessionChanged(SessionInfo),
    /// Work for the peer connections, queued by an engine callback on another thread.
    Deferred(Deferred),
    Barrier(SyncSender<()>),
    Shutdown,
}

/// Handle to the dispatcher thread. Every method only queues work and returns, so FreeRDP's
/// channel thread is never blocked. Dropping the handle stops the thread after it drains its queue.
pub struct Core {
    tx: Option<Sender<Input>>,
    join: Option<JoinHandle<()>>,
    config: Config,
}

impl Core {
    pub fn new(
        host: Arc<dyn Host>,
        config: Config,
        session: SessionInfo,
        engine: Arc<dyn MediaEngine>,
    ) -> Core {
        Core::with_devices(host, config, session, Arc::new(SystemDevices), engine)
    }

    pub fn with_devices(
        host: Arc<dyn Host>,
        config: Config,
        session: SessionInfo,
        devices: Arc<dyn DeviceSource>,
        engine: Arc<dyn MediaEngine>,
    ) -> Core {
        let (tx, rx) = mpsc::channel();
        // Engine threads queue their work through a clone of the sender. The thread stops on an
        // explicit `Shutdown` because these clones keep the channel open.
        let defer_tx = std::sync::Mutex::new(tx.clone());
        let defer: peerconnection::Defer = Arc::new(move |work| {
            if let Ok(tx) = defer_tx.lock() {
                let _ = tx.send(Input::Deferred(work));
            }
        });
        // Pictures go to the helper that owns the video windows, unless video is switched off.
        let surface: Arc<dyn crate::video::VideoSurface> = if config.video {
            let log_host = host.clone();
            Arc::new(crate::video::HelperSurface::new(
                config.video_helper.clone(),
                Arc::new(move |m| log_host.log(LogLevel::Warn, m)),
            ))
        } else {
            Arc::new(crate::video::NullSurface)
        };
        let state = State {
            host,
            config: config.clone(),
            session,
            devices,
            pcs: PeerConnections::new(engine, defer, surface, config.video),
            notify: Notifier::new(&config),
            alerts: CallAlerts::new(&config),
            transcript: None,
            disabled: false,
            conns: BTreeMap::new(),
        };
        let join = thread::Builder::new()
            .name("teams-redir".into())
            .spawn(move || run(state, rx))
            .expect("spawn dispatcher thread");
        Core {
            tx: Some(tx),
            join: Some(join),
            config,
        }
    }

    pub fn wants_channel(&self, channel: Channel) -> bool {
        self.config.wants_channel(channel)
    }

    /// A connection was accepted on `channel`. `conn` is unique for the life of the core.
    pub fn channel_opened(&self, conn: ConnId, channel: Channel) {
        self.send(Input::Opened(conn, channel));
    }

    pub fn data(&self, conn: ConnId, data: &[u8]) {
        self.send(Input::Data(conn, data.to_vec()));
    }

    pub fn channel_closed(&self, conn: ConnId) {
        self.send(Input::Closed(conn));
    }

    pub fn session_changed(&self, session: SessionInfo) {
        self.send(Input::SessionChanged(session));
    }

    /// Blocks until everything queued so far has been handled. For tests and the replay tool.
    pub fn flush(&self) {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        self.send(Input::Barrier(done_tx));
        let _ = done_rx.recv();
    }

    fn send(&self, input: Input) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(input);
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.send(Input::Shutdown);
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(mut state: State, rx: mpsc::Receiver<Input>) {
    state.open_transcript();
    while let Ok(input) = rx.recv() {
        if let Input::Barrier(done) = input {
            let _ = done.send(());
            continue;
        }
        if matches!(input, Input::Shutdown) {
            break;
        }
        if state.disabled {
            continue;
        }
        if catch_unwind(AssertUnwindSafe(|| state.handle(input))).is_err() {
            state.disable();
        }
    }
}

struct State {
    host: Arc<dyn Host>,
    config: Config,
    session: SessionInfo,
    devices: Arc<dyn DeviceSource>,
    pcs: PeerConnections,
    notify: Notifier,
    alerts: CallAlerts,
    transcript: Option<Transcript>,
    disabled: bool,
    /// Open connections. Each has its own conversation with the host.
    conns: BTreeMap<ConnId, Channel>,
}

impl State {
    fn log(&self, level: LogLevel, msg: impl AsRef<str>) {
        self.host.log(level, msg.as_ref());
    }

    fn open_transcript(&mut self) {
        let Some(dir) = self.config.transcript_dir.clone() else {
            return;
        };
        match Transcript::create(&dir) {
            Ok(t) => {
                self.log(
                    LogLevel::Info,
                    format!("transcript: {}", t.path().display()),
                );
                self.transcript = Some(t);
            }
            Err(e) => self.log(
                LogLevel::Warn,
                format!("cannot write transcripts to {}: {e}", dir.display()),
            ),
        }
    }

    /// A panic anywhere in the core ends up here: the RDP session carries on, Teams loses
    /// optimization (plan §2.3). What Teams does when `webrtc.1` closes is a phase 1 question.
    fn disable(&mut self) {
        self.disabled = true;
        self.log(
            LogLevel::Error,
            "panic in the Teams redirection core; disabling it and closing every connection",
        );
        for conn in std::mem::take(&mut self.conns).into_keys() {
            let _ = catch_unwind(AssertUnwindSafe(|| self.host.close(conn)));
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Opened(conn, ch) => {
                self.conns.insert(conn, ch);
                self.log(
                    LogLevel::Info,
                    format!("channel opened: {} (conn {conn})", ch.name()),
                );
            }
            Input::Closed(conn) => {
                self.pcs.conn_closed(conn);
                let name = self.conns.remove(&conn).map_or("unknown", Channel::name);
                self.log(
                    LogLevel::Info,
                    format!("channel closed: {name} (conn {conn})"),
                );
            }
            Input::SessionChanged(s) => {
                self.log(
                    LogLevel::Debug,
                    format!("session changed: {}x{}", s.width, s.height),
                );
                self.session = s;
            }
            Input::Data(conn, data) => match self.conns.get(&conn).copied() {
                Some(Channel::Webrtc) => self.on_webrtc(conn, &data),
                Some(Channel::Cpc) => self.on_cpc(conn, &data),
                None => self.log(
                    LogLevel::Warn,
                    format!("data on unknown conn {conn} dropped"),
                ),
            },
            Input::Deferred(work) => {
                for out in work(&mut self.pcs) {
                    // A connection that closed meanwhile has nobody to tell.
                    if self.conns.get(&out.conn) == Some(&Channel::Webrtc) {
                        self.send(out.conn, Channel::Webrtc, &out.json);
                    }
                }
            }
            Input::Barrier(_) | Input::Shutdown => {}
        }
    }

    fn record(&mut self, dir: Dir, conn: ConnId, data: &[u8]) {
        let name = self.conns.get(&conn).map_or("unknown", |c| c.name());
        if let Some(t) = self.transcript.as_mut()
            && let Err(e) = t.record(dir, name, conn, data)
        {
            self.transcript = None;
            self.log(
                LogLevel::Warn,
                format!("transcript write failed, logging stopped: {e}"),
            );
        }
    }

    fn send(&mut self, conn: ConnId, channel: Channel, json: &str) {
        let bytes = match channel {
            Channel::Webrtc => frame::webrtc_outbound(json),
            Channel::Cpc => frame::cpc_outbound(json),
        };
        self.record(Dir::Out, conn, &bytes);
        // Reply sizes are logged because the host-side size cap is unknown (plan §8).
        self.log(
            LogLevel::Debug,
            format!("-> {} conn {conn} ({} bytes)", channel.name(), bytes.len()),
        );
        if !self.host.write(conn, &bytes) {
            self.log(
                LogLevel::Warn,
                format!("write on {} conn {conn} failed", channel.name()),
            );
        }
    }

    fn on_webrtc(&mut self, conn: ConnId, data: &[u8]) {
        self.record(Dir::In, conn, data);
        let text = match frame::webrtc_inbound(data) {
            Ok(t) => t,
            Err(e) => return self.log(LogLevel::Warn, format!("webrtc: dropped message: {e}")),
        };
        self.log(
            LogLevel::Debug,
            format!("webrtc conn {conn}: RPC request: {text}"),
        );
        let req = match envelope::parse_request(text) {
            Ok(r) => r,
            Err(e) => {
                return self.log(
                    LogLevel::Warn,
                    format!("webrtc: unusable request ({e}): {text}"),
                );
            }
        };
        let outcome = match req.object_type.as_str() {
            redirector::OBJECT_TYPE
                if matches!(req.name.as_str(), notify::PLAY | notify::PAUSE) =>
            {
                if let Some(note) = self.alerts.observe(&req) {
                    self.log(LogLevel::Info, format!("webrtc conn {conn}: {note}"));
                }
                self.notify.handle(self.devices.as_ref(), &req)
            }
            redirector::OBJECT_TYPE => {
                redirector::handle(&self.config, &self.session, self.devices.as_ref(), &req)
            }
            peerconnection::OBJECT_TYPE => self.pcs.handle(conn, &req),
            t if media_objects::handles(t) => {
                self.pcs.handle_object(conn, self.devices.as_ref(), &req)
            }
            _ => mediadevices::handle(self.devices.as_ref(), &req),
        };
        let replies = match outcome {
            Some(o) => {
                if let Some(note) = o.note {
                    self.log(
                        LogLevel::Info,
                        format!("webrtc conn {conn}: {}: {note}", req.name),
                    );
                }
                o.replies
            }
            None => {
                // Every other RPC gets the unknown-method reply (`hr` 0x80070032, no details) and a log line; that sequence is the phase 2
                // backlog (plan §3.1). A notification (no rpcCallId) gets no reply.
                self.log(
                    LogLevel::Info,
                    format!(
                        "webrtc conn {conn}: unhandled {}.{}",
                        req.object_type, req.name
                    ),
                );
                envelope::error_response(&req, hresult::ERROR_NOT_SUPPORTED, None)
                    .into_iter()
                    .collect()
            }
        };
        for reply in replies {
            self.send(conn, Channel::Webrtc, &reply);
        }
    }

    fn on_cpc(&mut self, conn: ConnId, data: &[u8]) {
        self.record(Dir::In, conn, data);
        let text = match frame::cpc_inbound(data) {
            Ok(t) => t,
            Err(e) => return self.log(LogLevel::Warn, format!("cpc: dropped message: {e}")),
        };
        match cpc::parse(text) {
            Ok(Message::VmHello(hello)) => {
                self.log(
                    LogLevel::Info,
                    format!(
                        "Incoming VMHello message: reqId={}, scVer={}",
                        hello.request_id,
                        hello.version.as_deref().unwrap_or("")
                    ),
                );
                let args = self.client_hello_args(hello.request_id);
                self.log(
                    LogLevel::Info,
                    format!(
                        "Sending ClientHello reqId={}, platform={}, load_errc={}, deploy_errc={}",
                        args.request_id,
                        args.platform,
                        args.load_errc.unwrap_or(0),
                        args.deploy_errc.unwrap_or(0)
                    ),
                );
                let mut hello = cpc::client_hello_value(&args);
                let note = patch::apply_file(
                    &self.config.state_dir,
                    CLIENT_HELLO_PATCH,
                    "clienthello",
                    &mut hello,
                );
                self.log(LogLevel::Info, format!("cpc: {note}"));
                self.send(conn, Channel::Cpc, &hello.to_string());
            }
            Ok(Message::Ping) => self.send(conn, Channel::Cpc, &cpc::build_pong()),
            Ok(Message::VmGoodbye) => self.log(LogLevel::Info, "cpc: VMGoodbye"),
            Ok(Message::Other(name)) => {
                self.log(LogLevel::Info, format!("cpc: unhandled message {name}"))
            }
            // The macOS stub returns silently when a VMHello does not parse.
            Err(cpc::ParseError::BadVmHello) => {
                self.log(LogLevel::Debug, "cpc: VMHello without requestId ignored")
            }
            Err(e) => self.log(
                LogLevel::Warn,
                format!("cpc: unparsable message ({e:?}): {text}"),
            ),
        }
    }

    fn client_hello_args(&self, request_id: u32) -> ClientHelloArgs {
        let id = &self.config.identity;
        let code = self.config.cpc_errc();
        let nonempty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        ClientHelloArgs {
            request_id,
            platform: id.cpc_platform.clone(),
            vdi_vendor: id.vdi_vendor,
            load_errc: Some(code),
            deploy_errc: Some(code),
            rd_client_version: nonempty(&self.session.client_version),
            rd_client_product_name: nonempty(&self.session.client_name),
            rd_client_process_name: config::process_name(),
            rd_client_sandboxed: config::is_sandboxed(),
            client_os_version: config::os_version(),
            plugin_version: Some(id.plugin_version.clone()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_json::{Value, json};

    use super::*;
    use crate::devices::fake::*;

    #[derive(Default)]
    struct MockHost {
        writes: Mutex<Vec<(ConnId, Vec<u8>)>>,
        closed: Mutex<Vec<ConnId>>,
        panic_on_write: AtomicBool,
    }

    impl Host for MockHost {
        fn write(&self, conn: ConnId, data: &[u8]) -> bool {
            assert!(!self.panic_on_write.load(Ordering::SeqCst), "boom");
            self.writes.lock().unwrap().push((conn, data.to_vec()));
            true
        }
        fn close(&self, conn: ConnId) {
            self.closed.lock().unwrap().push(conn);
        }
        fn log(&self, _: LogLevel, _: &str) {}
    }

    fn core(host: &Arc<MockHost>, env: &[(&str, &str)]) -> Core {
        core_with_devices(host, env, vec![mic("m", true)])
    }

    fn core_with_devices(
        host: &Arc<MockHost>,
        env: &[(&str, &str)],
        devices: Vec<crate::devices::Device>,
    ) -> Core {
        let state_dir =
            std::env::temp_dir().join(format!("teams-redir-core-{}", std::process::id()));
        let mut pairs: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        pairs.push((
            "TEAMS_REDIR_STATE_DIR".into(),
            state_dir.to_string_lossy().into_owned(),
        ));
        pairs.push(("TEAMS_REDIR_TRANSCRIPTS".into(), "off".into()));
        // The replays must not make sound on the machine that runs them.
        pairs.push(("TEAMS_REDIR_BUILTIN_TONES".into(), "off".into()));
        let config = Config::from_lookup(move |k| {
            pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        });
        let session = SessionInfo {
            width: 1280,
            height: 720,
            client_name: "FreeRDP".into(),
            client_version: "3.32.2".into(),
        };
        let devices = Arc::new(FakeDevices(Ok(devices)));
        Core::with_devices(
            host.clone(),
            config,
            session,
            devices,
            Arc::new(media::fake::FakeEngine::default()),
        )
    }

    fn writes(host: &MockHost, conn: ConnId) -> Vec<Vec<u8>> {
        host.writes
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, _)| *c == conn)
            .map(|(_, d)| d.clone())
            .collect()
    }

    fn json_of(msg: &[u8]) -> Value {
        let end = msg.iter().position(|&b| b == 0).unwrap_or(msg.len());
        serde_json::from_slice(&msg[..end]).unwrap()
    }

    const SET_VERSION_INFO: &[u8] = b"{\"rpcObjectType\":\"RDWebRTCRedirector\",\"rpcName\":\"setVersionInfo\",\"version\":{\"shim\":\"1.1.2602.23001\"},\"session\":{\"remoteApp\":false},\"features\":{}}\0";

    #[test]
    fn vmhello_gets_a_client_hello_with_the_honest_decline() {
        let host = Arc::new(MockHost::default());
        let c = core(&host, &[]);
        c.channel_opened(1, Channel::Cpc);
        c.data(
            1,
            br#"{"contentType":"scpie","messageName":"VMHello","requestId":9,"version":"2026.37"}"#,
        );
        c.flush();
        let out = writes(&host, 1);
        assert_eq!(out.len(), 1);
        let v: Value = serde_json::from_slice(&out[0]).unwrap(); // no trailing NUL on cpc
        assert_eq!(v["messageName"], json!("ClientHello"));
        assert_eq!(v["requestId"], json!(9));
        assert_eq!(v["load_errc"], json!(16002));
        assert_eq!(v["install_errc"], json!(16002));
        assert_eq!(v["rdClientProcuctName"], json!("FreeRDP"));
    }

    #[test]
    fn macstore_mode_sends_the_known_good_decline() {
        let host = Arc::new(MockHost::default());
        let c = core(&host, &[("TEAMS_REDIR_CPC", "macstore")]);
        c.channel_opened(1, Channel::Cpc);
        c.data(
            1,
            br#"{"contentType":"scpie","messageName":"VMHello","requestId":1}"#,
        );
        c.flush();
        let v: Value = serde_json::from_slice(&writes(&host, 1)[0]).unwrap();
        assert_eq!(v["platform"], json!("mac-arm64"));
        assert_eq!(v["load_errc"], json!(16066));
        assert_eq!(v["pluginVersion"], json!("2026.23.01.1"));
    }

    #[test]
    fn client_hello_patch_file_is_applied_per_vmhello() {
        let host = Arc::new(MockHost::default());
        let dir = std::env::temp_dir().join(format!("teams-redir-chpatch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.to_string_lossy().into_owned();
        let config = Config::from_lookup(move |k| match k {
            "TEAMS_REDIR_STATE_DIR" => Some(state.clone()),
            "TEAMS_REDIR_TRANSCRIPTS" => Some("off".into()),
            _ => None,
        });
        let c = Core::new(
            host.clone(),
            config,
            SessionInfo::default(),
            Arc::new(media::fake::FakeEngine::default()),
        );
        c.channel_opened(1, Channel::Cpc);
        let hello = br#"{"contentType":"scpie","messageName":"VMHello","requestId":3}"#;

        c.data(1, hello);
        c.flush(); // the first reply must be built before the patch exists
        std::fs::write(
            dir.join(CLIENT_HELLO_PATCH),
            r#"{"load_errc":16066,"platform":"mac-arm64","rdClientVersion":null}"#,
        )
        .unwrap();
        c.data(1, hello);
        c.flush();

        let out = writes(&host, 1);
        let before: Value = serde_json::from_slice(&out[0]).unwrap();
        let after: Value = serde_json::from_slice(&out[1]).unwrap();
        assert_eq!(before["load_errc"], json!(16002));
        assert_eq!(after["load_errc"], json!(16066));
        assert_eq!(after["install_errc"], json!(16002)); // only what the patch names changes
        assert_eq!(after["platform"], json!("mac-arm64"));
        assert!(after.get("rdClientVersion").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ping_gets_pong_and_goodbye_gets_nothing() {
        let host = Arc::new(MockHost::default());
        let c = core(&host, &[]);
        c.channel_opened(1, Channel::Cpc);
        c.data(1, br#"{"contentType":"scpie","messageName":"Ping"}"#);
        c.data(1, br#"{"contentType":"scpie","messageName":"VMGoodbye"}"#);
        c.flush();
        assert_eq!(
            writes(&host, 1),
            vec![br#"{"contentType":"scpie","messageName":"Pong"}"#.to_vec()]
        );
    }

    #[test]
    fn set_version_info_notification_gets_a_nul_terminated_sessioninfo() {
        let host = Arc::new(MockHost::default());
        let c = core(&host, &[]);
        c.channel_opened(1, Channel::Webrtc);
        c.data(1, SET_VERSION_INFO);
        c.flush();
        let out = writes(&host, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].last(), Some(&0));
        let ev = json_of(&out[0]);
        assert_eq!(ev["rpcEventName"], json!("sessioninfo"));
        assert_eq!(ev["rpcEventArgs"]["display"]["height"], json!(720));
    }

    #[test]
    fn several_webrtc_connections_are_answered_independently() {
        let host = Arc::new(MockHost::default());
        let c = core(&host, &[]);
        for conn in [1, 2, 3] {
            c.channel_opened(conn, Channel::Webrtc);
        }
        c.data(2, SET_VERSION_INFO);
        c.data(3, br#"{"rpcObjectType":"RTCPeerConnection","rpcName":"addTrack","rpcObjectId":2,"rpcCallId":5,"rpcArgs":[{"trackRpcObjectId":7}]}"#);
        c.flush();
        assert!(writes(&host, 1).is_empty());
        assert_eq!(
            json_of(&writes(&host, 2)[0])["rpcEventName"],
            json!("sessioninfo")
        );
        let err = json_of(&writes(&host, 3)[0]);
        assert_eq!(err["rpcCallId"], json!(5));
        assert_eq!(err["hr"], json!(hresult::ERROR_NOT_SUPPORTED as i32));
        assert!(err.get("errorDetails").is_none());
    }

    #[test]
    fn a_call_is_negotiated_through_the_dispatcher() {
        let host = Arc::new(MockHost::default());
        let c = core(&host, &[]);
        c.channel_opened(1, Channel::Webrtc);
        let rpc = |call: u32, name: &str, args: &str| {
            format!(
                r#"{{"rpcCallId":{call},"rpcObjectType":"RTCPeerConnection","rpcObjectId":3,"rpcName":"{name}","rpcArgs":{args}}}"#
            )
        };
        c.data(
            1,
            rpc(
                1,
                "createPeerConnection",
                r#"[{"sdpSemantics":"unified-plan"}]"#,
            )
            .as_bytes(),
        );
        let offer = r#"[{"type":"offer","sdp":"v=0\r\nm=audio 9 X 111\r\na=mid:0\r\n"}]"#;
        c.data(1, rpc(2, "setRemoteDescription", offer).as_bytes());
        c.data(1, rpc(3, "createAnswer", "[]").as_bytes());
        c.flush();
        // Completions are queued by the engine; one more flush runs what they added.
        c.flush();
        let all: Vec<Value> = writes(&host, 1).iter().map(|w| json_of(w)).collect();
        let by_call = |n: u32| all.iter().find(|m| m["rpcCallId"] == json!(n)).unwrap();
        assert!(by_call(1)["result"]["sendCapabilities"]["audio"]["codecs"].is_array());
        assert_eq!(by_call(2)["hr"], json!(0));
        assert_eq!(
            by_call(2)["result"]["transceivers"][0]["rpcObjectId"],
            json!(4294967298u64)
        );
        assert_eq!(by_call(3)["result"]["desc"]["type"], json!("answer"));
        let names: Vec<&str> = all
            .iter()
            .filter_map(|m| m["rpcEventName"].as_str())
            .collect();
        assert_eq!(names, ["track", "signalingstatechange"]);
    }

    /// What Teams sent when a test call started (2026-10-31, credentials redacted): every call
    /// that has a handler is answered with success, and what is still missing is named here, so a
    /// new gap shows up as a failure.
    #[test]
    fn the_test_call_requests_are_answered() {
        replay_call(include_str!(
            "../../../golden/2026-10-31-avd-test-call-requests.jsonl"
        ));
    }

    /// The second attempt, after the call setup worked: it goes on to negotiate, uses the data
    /// channel and polls `getStats`, and ends with `removeTrack`.
    #[test]
    fn the_second_test_call_requests_are_answered() {
        replay_call(include_str!(
            "../../../golden/2026-10-31-avd-test-call-2-requests.jsonl"
        ));
    }

    /// The call where the echo service answered and played the message back: the receive side
    /// (`track` events, `setSinkId`, `setAttribute srcObject`, `play`) all get success replies.
    #[test]
    fn the_working_echo_call_requests_are_answered() {
        replay_call(include_str!(
            "../../../golden/2026-11-01-avd-test-call-echo-works-requests.jsonl"
        ));
    }

    fn replay_call(golden: &str) {
        // `rtcSession` is not in the Windows add-in either: the unknown-method reply is right.
        const STILL_UNHANDLED: &[&str] = &["rtcSession"];
        // Handled, but the test has no local sound to play.
        const NO_LOCAL_SOUND: &[&str] = &["playNotifyAudio"];
        // Asynchronous in the shipped client: never answered, even though it has a call id.
        const NEVER_ANSWERED: &[&str] = &["setTeamsCallInfo"];
        let host = Arc::new(MockHost::default());
        let mic_id = "alsa_input.pci-0000_00_1f.3-platform-sof_sdw.HiFi__Mic__source";
        let speaker_id = "alsa_output.pci-0000_00_1f.3-platform-sof_sdw.HiFi__Speaker__sink";
        let c = core_with_devices(
            &host,
            &[],
            vec![mic(mic_id, true), speaker(speaker_id, true)],
        );
        c.channel_opened(1, Channel::Webrtc);
        let mut checked = 0;
        // A transcript can hold several sessions whose call ids restart, so each request is
        // matched only with what was written after it.
        for entry in golden.lines().filter_map(transcript::parse_line) {
            let req: Value = serde_json::from_str(&entry.msg).unwrap();
            let before = writes(&host, 1).len();
            c.data(1, entry.msg.as_bytes());
            c.flush();
            c.flush();
            let Some(call) = req["rpcCallId"].as_u64() else {
                continue;
            };
            let name = req["rpcName"].as_str().unwrap();
            let object = req["rpcObjectType"].as_str().unwrap();
            if NEVER_ANSWERED.contains(&name) {
                continue;
            }
            let new: Vec<Value> = writes(&host, 1)[before..]
                .iter()
                .map(|w| json_of(w))
                .collect();
            let Some(reply) = new.iter().find(|r| r["rpcCallId"] == json!(call)) else {
                panic!("{object}.{name} (call {call}) got no reply");
            };
            let unhandled = reply["hr"] == json!(hresult::ERROR_NOT_SUPPORTED as i32);
            assert_eq!(
                unhandled,
                STILL_UNHANDLED.contains(&name),
                "{object}.{name} (call {call}): {reply}"
            );
            if NO_LOCAL_SOUND.contains(&name) {
                assert_eq!(
                    reply["hr"],
                    json!(hresult::ERROR_NOT_FOUND as i32),
                    "{reply}"
                );
            } else if !unhandled {
                assert_eq!(reply["hr"], json!(0), "{object}.{name}: {reply}");
            }
            checked += 1;
        }
        assert!(checked > 40, "{checked} calls checked");
    }

    /// The core hands ring sounds to the call alerts: with notifications switched off in the
    /// state directory, the ring still plays (or fails to find a file) and nothing is shown.
    #[test]
    fn a_ringing_call_reaches_the_alerts() {
        let dir =
            std::env::temp_dir().join(format!("teams-redir-core-alert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::callalert::SWITCH_FILE), "off").unwrap();
        let host = Arc::new(MockHost::default());
        let state = dir.to_string_lossy().into_owned();
        let c = core_with_devices(&host, &[("TEAMS_REDIR_STATE_DIR", state.as_str())], vec![]);
        c.channel_opened(1, Channel::Webrtc);
        c.data(1, br#"{"rpcCallId":3,"rpcObjectType":"RDWebRTCRedirector","rpcName":"playNotifyAudio","rpcArgs":[{"audioId":"a","uri":"https://h/Ring.mp3","sinkId":"x","loop":true}]}"#);
        c.flush();
        // The audio part still answers (no local file here).
        let reply = json_of(&writes(&host, 1)[0]);
        assert_eq!(reply["hr"], json!(hresult::ERROR_NOT_FOUND as i32));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unhandled_call_gets_an_error_but_an_unhandled_notification_gets_nothing() {
        let host = Arc::new(MockHost::default());
        let c = core(&host, &[]);
        c.channel_opened(1, Channel::Webrtc);
        c.data(1, br#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"playNotifyAudio","rpcArgs":[{}],"rpcCallId":3}"#);
        c.data(
            1,
            br#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"somethingNew"}"#,
        );
        c.flush();
        let out = writes(&host, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(json_of(&out[0])["rpcCallId"], json!(3));
    }

    #[test]
    fn garbage_and_unknown_connections_are_dropped_without_a_reply() {
        let host = Arc::new(MockHost::default());
        let c = core(&host, &[]);
        c.channel_opened(1, Channel::Webrtc);
        c.channel_opened(2, Channel::Cpc);
        c.data(1, b"not json\0");
        c.data(1, b"");
        c.data(2, b"\xff\xfe");
        c.data(99, SET_VERSION_INFO);
        c.channel_closed(1);
        c.data(1, SET_VERSION_INFO); // closed connections stop receiving
        c.flush();
        assert!(host.writes.lock().unwrap().is_empty());
        assert!(host.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_panic_disables_the_core_and_closes_every_open_connection() {
        let host = Arc::new(MockHost::default());
        let c = core(&host, &[]);
        c.channel_opened(4, Channel::Webrtc);
        c.channel_opened(5, Channel::Cpc);
        c.channel_opened(6, Channel::Webrtc);
        c.channel_closed(4);
        host.panic_on_write.store(true, Ordering::SeqCst);
        c.data(5, br#"{"contentType":"scpie","messageName":"Ping"}"#);
        c.flush();
        assert_eq!(*host.closed.lock().unwrap(), vec![5, 6]);
        // Later input is ignored rather than handled.
        host.panic_on_write.store(false, Ordering::SeqCst);
        c.data(5, br#"{"contentType":"scpie","messageName":"Ping"}"#);
        c.flush();
        assert!(host.writes.lock().unwrap().is_empty());
    }
}
