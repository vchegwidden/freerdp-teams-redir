//! `RTCPeerConnection`: the calls Teams makes to set up and negotiate a call, and the events it
//! gets back. Reply and event shapes are the Windows add-in's (`msrdc-reply-structures.md`
//! §3b, §3c and §4) [O].
//!
//! The engine completes most calls on its own threads. Those completions, and every engine event,
//! come back to the dispatcher thread as a [`Deferred`] closure, so this state is only ever touched
//! from one thread and replies are written in dispatcher order.
//!
//! Ids: the host chooses the id of the peer connection (the `rpcObjectId` of
//! `createPeerConnection`). Transceivers, receivers, senders, remote tracks and remote streams are
//! created from SDP, so this side chooses their ids, from one counter that starts at 2^32 like the
//! Windows client's (`RtcManager+0x128`). A transceiver gets three ids in a row: itself, its
//! receiver, its sender.
//!
//! Transceivers the host creates itself (`addTransceiver`) are the exception: the host chooses all
//! three ids. Both kinds live in one list per peer connection that mirrors the engine's, which only
//! ever appends, so an index identifies a transceiver for the life of the connection.
//!
//! The local streams, tracks and elements the host creates around a peer connection are in
//! `media_objects.rs`.
//!
//! Not yet handled (they get the core's unknown-method reply and a log line): `addTrack`,
//! `removeTrack`; `setConfiguration` is refused as the shipped client does, and `getStats` answers
//! only the legacy form with no reports.

use std::collections::HashMap;
use std::sync::Arc;

use media::{
    Capabilities, DataChannel, DataChannelEvent, Direction, EngineError, ErrorKind,
    IceCandidateInit, IceServer, Kind, MediaEngine, PcConfig, PcEvent, PeerConnection,
    RtpCapabilities, SdpType, SessionDescription, Track, TrackInfo, Transceiver, candidate,
};
use proto::envelope::{self, RpcRequest};
use proto::hresult;
use serde_json::{Map, Value, json};

use crate::host::ConnId;
use crate::redirector::Outcome;
use crate::video::VideoSurface;

pub const OBJECT_TYPE: &str = "RTCPeerConnection";

/// The first id this side allocates: the host's own ids (peer connection, stream, track) stay
/// below it.
const FIRST_CLIENT_ID: u64 = 1 << 32;

/// A message to send on a connection.
pub struct Out {
    pub conn: ConnId,
    pub json: String,
}

/// Work an engine callback hands to the dispatcher thread.
pub type Deferred = Box<dyn FnOnce(&mut PeerConnections) -> Vec<Out> + Send>;
/// Queues a [`Deferred`] for the dispatcher thread.
pub type Defer = Arc<dyn Fn(Deferred) + Send + Sync>;

pub(crate) struct TransceiverRecord {
    pub(crate) id: u64,
    pub(crate) receiver: u64,
    pub(crate) sender: u64,
    /// Set when the ids were allocated but nothing has told the host about them yet: the next
    /// set-description reply then carries `addedFromSdp`, `receiver` and `sender` once. A `track`
    /// event that allocates first clears it for its own transceiver only (§3c); the others it
    /// found stay pending, and a receive-only video section never gets an event at all.
    unreported: bool,
    pub(crate) handle: Arc<dyn Transceiver>,
}

pub(crate) struct Pc {
    pub(crate) conn: ConnId,
    pub(crate) engine: Arc<dyn PeerConnection>,
    /// Mirrors `engine.transceivers()` by index.
    pub(crate) transceivers: Vec<TransceiverRecord>,
}

/// A local media stream the host created: its capture tracks (a microphone, a camera, both), or
/// none for a stream that only collects remote tracks.
pub(crate) struct LocalStream {
    pub(crate) conn: ConnId,
    pub(crate) tracks: Vec<Arc<dyn Track>>,
}

pub(crate) struct LocalTrack {
    pub(crate) track: Arc<dyn Track>,
}

/// A `MediaElement`: the host's stand-in for an `<audio>` or `<video>` tag. Remote audio plays
/// through the engine on its own; a video element is a window.
pub(crate) struct Element {
    pub(crate) conn: ConnId,
    pub(crate) kind: Kind,
    /// The stream last set as `srcObject`, and the video track of it that is showing here.
    pub(crate) source: Option<u64>,
    pub(crate) bound: Option<Arc<dyn Track>>,
}

pub(crate) struct DataChannelRecord {
    pub(crate) conn: ConnId,
    pub(crate) channel: Arc<dyn DataChannel>,
}

/// Where a host-chosen transceiver, sender or receiver id points.
#[derive(Clone, Copy)]
pub(crate) struct TransceiverRef {
    pub(crate) pc: u64,
    pub(crate) index: usize,
}

pub struct PeerConnections {
    pub(crate) engine: Arc<dyn MediaEngine>,
    pub(crate) defer: Defer,
    pub(crate) pcs: HashMap<u64, Pc>,
    /// Remote streams and tracks by their string id.
    streams: HashMap<String, u64>,
    tracks: HashMap<String, u64>,
    next_id: u64,
    pub(crate) local_streams: HashMap<u64, LocalStream>,
    pub(crate) local_tracks: HashMap<u64, LocalTrack>,
    pub(crate) elements: HashMap<u64, Element>,
    /// Remote tracks and the streams they belong to, by the ids this side allocated.
    pub(crate) remote_tracks: HashMap<u64, Arc<dyn Track>>,
    pub(crate) remote_streams: HashMap<u64, Vec<u64>>,
    pub(crate) surface: Arc<dyn VideoSurface>,
    advertise_video: bool,
    pub(crate) channels: HashMap<u64, DataChannelRecord>,
    /// By the id of the transceiver, its sender, or its receiver, whoever chose it.
    pub(crate) transceiver_ids: HashMap<u64, TransceiverRef>,
    pub(crate) sender_ids: HashMap<u64, TransceiverRef>,
}

impl PeerConnections {
    pub fn new(
        engine: Arc<dyn MediaEngine>,
        defer: Defer,
        surface: Arc<dyn VideoSurface>,
        advertise_video: bool,
    ) -> PeerConnections {
        PeerConnections {
            engine,
            defer,
            pcs: HashMap::new(),
            streams: HashMap::new(),
            tracks: HashMap::new(),
            next_id: FIRST_CLIENT_ID,
            local_streams: HashMap::new(),
            local_tracks: HashMap::new(),
            elements: HashMap::new(),
            remote_tracks: HashMap::new(),
            remote_streams: HashMap::new(),
            surface,
            advertise_video,
            channels: HashMap::new(),
            transceiver_ids: HashMap::new(),
            sender_ids: HashMap::new(),
        }
    }

    /// Returns `None` for methods not handled here.
    pub fn handle(&mut self, conn: ConnId, req: &RpcRequest) -> Option<Outcome> {
        Some(match req.name.as_str() {
            "createPeerConnection" => self.create(conn, req),
            "createOffer" => self.create_description(conn, req, true),
            "createAnswer" => self.create_description(conn, req, false),
            "setLocalDescription" => self.set_description(conn, req, true),
            "setRemoteDescription" => self.set_description(conn, req, false),
            "addIceCandidate" => self.add_ice_candidate(conn, req),
            "close" => self.close(req),
            "addTransceiver" => self.add_transceiver(req),
            "createDataChannel" => self.create_data_channel(conn, req),
            // A stub in the shipped client: it always answers E_NOTIMPL, with no details.
            "setConfiguration" => Outcome {
                replies: envelope::error_response(req, hresult::E_NOTIMPL, None)
                    .into_iter()
                    .collect(),
                note: None,
            },
            "getStats" => self.get_stats(conn, req),
            "removeTrack" => self.remove_track(req),
            _ => return None,
        })
    }

    /// The connection is gone: so are its calls.
    pub fn conn_closed(&mut self, conn: ConnId) {
        let ids: Vec<u64> = self
            .pcs
            .iter()
            .filter(|(_, pc)| pc.conn == conn)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.remove_pc(id);
        }
        self.channels.retain(|_, c| {
            let keep = c.conn != conn;
            if !keep {
                c.channel.close();
            }
            keep
        });
        self.local_streams.retain(|_, s| {
            let keep = s.conn != conn;
            if !keep {
                s.tracks.iter().for_each(|t| t.stop());
            }
            keep
        });
        let gone: Vec<u64> = self
            .elements
            .iter()
            .filter(|(_, e)| e.conn == conn)
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.drop_element(id);
        }
    }

    fn remove_pc(&mut self, id: u64) {
        if let Some(pc) = self.pcs.remove(&id) {
            pc.engine.close();
        }
        self.transceiver_ids.retain(|_, r| r.pc != id);
        self.sender_ids.retain(|_, r| r.pc != id);
    }

    pub fn len(&self) -> usize {
        self.pcs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pcs.is_empty()
    }

    pub(crate) fn alloc(&mut self, n: u64) -> u64 {
        let id = self.next_id;
        self.next_id += n;
        id
    }

    fn create(&mut self, conn: ConnId, req: &RpcRequest) -> Outcome {
        let Some(id) = object_id(req) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "missing rpcObjectId");
        };
        if self.pcs.contains_key(&id) {
            return failure(
                req,
                hresult::ERROR_ALREADY_EXISTS,
                "RTCPeerConnection already exists",
            );
        }
        let Some(config) = parse_config(req.args_array().first()) else {
            return failure(
                req,
                hresult::E_INVALIDARG,
                "invalid peerConnection configuration",
            );
        };
        let defer = self.defer.clone();
        let sink =
            Arc::new(move |event: PcEvent| defer(Box::new(move |pcs| pcs.on_event(id, event))));
        let engine_pc = match self.engine.create_peer_connection(config, sink) {
            Ok(pc) => Arc::from(pc),
            Err(e) => {
                let mut out = failure(
                    req,
                    hresult::E_INVALIDARG,
                    "unable to create peerConnection",
                );
                out.note = Some(format!("unable to create peerConnection: {e}"));
                return out;
            }
        };
        self.pcs.insert(
            id,
            Pc {
                conn,
                engine: engine_pc,
                transceivers: Vec::new(),
            },
        );
        let caps = capabilities_json(&self.engine.capabilities(), self.advertise_video);
        Outcome {
            replies: envelope::ok_response(req, caps).into_iter().collect(),
            note: Some(format!("RTCPeerConnection {id} created")),
        }
    }

    pub(crate) fn pc(&self, req: &RpcRequest) -> Result<(u64, Arc<dyn PeerConnection>), Outcome> {
        let Some(id) = object_id(req) else {
            return Err(failure(
                req,
                hresult::ERROR_NOT_FOUND,
                "rpcObjectId field not found",
            ));
        };
        match self.pcs.get(&id) {
            Some(pc) => Ok((id, pc.engine.clone())),
            None => Err(failure(
                req,
                hresult::ERROR_NOT_FOUND,
                "RTCPeerConnection not found",
            )),
        }
    }

    /// `createOffer` / `createAnswer`: no arguments are read; the reply comes from the engine.
    fn create_description(&mut self, conn: ConnId, req: &RpcRequest, offer: bool) -> Outcome {
        let (_, pc) = match self.pc(req) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let call_id = req.call_id.clone();
        let defer = self.defer.clone();
        let done = Box::new(move |res: Result<SessionDescription, EngineError>| {
            defer(Box::new(move |_| {
                let json = description_reply(call_id.as_ref(), res);
                if json.is_empty() {
                    Vec::new()
                } else {
                    vec![Out { conn, json }]
                }
            }))
        });
        if offer {
            pc.create_offer(done);
        } else {
            pc.create_answer(done);
        }
        pending(req)
    }

    /// `setLocalDescription` / `setRemoteDescription`: `rpcArgs[0]` is the description, either
    /// `{type, sdp}` or wrapped as `{desc: {type, sdp}}` (the two decompiled reports differ).
    fn set_description(&mut self, conn: ConnId, req: &RpcRequest, local: bool) -> Outcome {
        let (id, pc) = match self.pc(req) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let desc = match parse_description(req.args_array().first()) {
            Ok(d) => d,
            Err((hr, msg)) => return failure(req, hr, msg),
        };
        let req2 = req.clone();
        let defer = self.defer.clone();
        let done = Box::new(move |res: Result<(), EngineError>| {
            defer(Box::new(move |pcs| pcs.finish_set(conn, id, &req2, res)))
        });
        if local {
            pc.set_local_description(desc, done);
        } else {
            pc.set_remote_description(desc, done);
        }
        pending(req)
    }

    fn finish_set(
        &mut self,
        conn: ConnId,
        id: u64,
        req: &RpcRequest,
        res: Result<(), EngineError>,
    ) -> Vec<Out> {
        let Some(call_id) = req.call_id.as_ref() else {
            return Vec::new();
        };
        let json = match res {
            Ok(()) => {
                let transceivers = self.report_transceivers(id);
                envelope::async_reply(call_id, 0, Some(json!({ "transceivers": transceivers })))
            }
            Err(e) => engine_failure(req, call_id, &e),
        };
        vec![Out { conn, json }]
    }

    /// Appends a record for every engine transceiver that has none. The ids are allocated here
    /// unless the host just created the last one and passes its own. Returns how many records
    /// there were before.
    pub(crate) fn sync_transceivers(&mut self, id: u64, host: Option<(u64, u64, u64)>) -> usize {
        let Some(engine) = self.pcs.get(&id).map(|pc| pc.engine.clone()) else {
            return 0;
        };
        let all = engine.transceivers();
        let have = self.pcs.get(&id).map_or(0, |pc| pc.transceivers.len());
        for (index, handle) in all.iter().enumerate().skip(have) {
            let host_ids = host.filter(|_| index + 1 == all.len());
            let (tid, rid, sid) = host_ids.unwrap_or_else(|| {
                let base = self.alloc(3);
                (base, base + 1, base + 2)
            });
            if let Some(pc) = self.pcs.get_mut(&id) {
                pc.transceivers.push(TransceiverRecord {
                    id: tid,
                    receiver: rid,
                    sender: sid,
                    // Ids the host did not choose have to be reported to it once.
                    unreported: host_ids.is_none(),
                    handle: handle.clone(),
                });
            }
            let at = TransceiverRef { pc: id, index };
            self.transceiver_ids.insert(tid, at);
            self.sender_ids.insert(sid, at);
        }
        have
    }

    /// `result.transceivers` of a set-description reply (§3c): one entry per transceiver with a
    /// mid. Ids are allocated the first time a transceiver is seen.
    fn report_transceivers(&mut self, id: u64) -> Vec<Value> {
        self.sync_transceivers(id, None);
        let Some(pc) = self.pcs.get_mut(&id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for rec in &mut pc.transceivers {
            let info = rec.handle.info();
            let (Some(mid), true) = (info.mid, info.direction != Direction::Stopped) else {
                continue;
            };
            let mut m = Map::new();
            m.insert("rpcObjectId".into(), json!(rec.id));
            m.insert("direction".into(), json!(info.direction.as_str()));
            m.insert("mid".into(), json!(mid));
            if let Some(kind) = info.kind {
                m.insert("kind".into(), json!(kind.as_str()));
            }
            if std::mem::take(&mut rec.unreported) {
                m.insert("addedFromSdp".into(), json!(true));
                m.insert("receiver".into(), json!({ "rpcObjectId": rec.receiver }));
                m.insert("sender".into(), json!({ "rpcObjectId": rec.sender }));
            }
            out.push(Value::Object(m));
        }
        out
    }

    /// `addTransceiver`: the host chooses the ids of the transceiver, its sender and its receiver,
    /// and may attach a local track.
    fn add_transceiver(&mut self, req: &RpcRequest) -> Outcome {
        let (id, pc) = match self.pc(req) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let Some(args) = req.args_array().first() else {
            return failure(req, hresult::E_INVALIDARG, "missing rpcArgs");
        };
        let kind = match args["kind"].as_str() {
            None => {
                return failure(
                    req,
                    hresult::E_INVALIDARG,
                    "missing kind for new transceiver",
                );
            }
            Some("audio") => Kind::Audio,
            Some("video") => Kind::Video,
            Some(_) => {
                return failure(
                    req,
                    hresult::E_INVALIDARG,
                    "transceiver.kind was not audio or video",
                );
            }
        };
        let ids = (
            args["transceiverRpcObjectId"].as_u64(),
            args["receiverRpcObjectId"].as_u64(),
            args["senderRpcObjectId"].as_u64(),
        );
        let (Some(t), Some(r), Some(s)) = ids else {
            return failure(
                req,
                hresult::E_INVALIDARG,
                "missing rpcObjectId for transceiver, sender, and/or receiver",
            );
        };
        let direction = args["direction"]
            .as_str()
            .and_then(parse_direction)
            .unwrap_or(Direction::SendRecv);
        let track = match args["trackRpcObjectId"].as_u64().filter(|n| *n != 0) {
            None => None,
            Some(n) => match self.local_tracks.get(&n) {
                Some(t) => Some(t.track.clone()),
                None => return failure(req, hresult::ERROR_NOT_FOUND, "Track not found"),
            },
        };
        let handle = match pc.add_transceiver(kind, direction) {
            Ok(h) => h,
            Err(e) => return rtc_error(req, &e),
        };
        self.sync_transceivers(id, Some((t, r, s)));
        if let Some(track) = track
            && let Err(e) = handle.set_sender_track(Some(track))
        {
            return rtc_error(req, &e);
        }
        Outcome {
            replies: envelope::ok_empty(req).into_iter().collect(),
            note: Some(format!(
                "RTCPeerConnection {id}: {} transceiver {t} ({})",
                kind.as_str(),
                direction.as_str()
            )),
        }
    }

    /// `createDataChannel`: the host chooses the channel's id; no init options are read.
    fn create_data_channel(&mut self, conn: ConnId, req: &RpcRequest) -> Outcome {
        let (_, pc) = match self.pc(req) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let Some(args) = req.args_array().first() else {
            return failure(req, hresult::E_FAIL, "missing rpcArgs");
        };
        let (Some(label), Some(dc_id)) = (args["label"].as_str(), args["rpcObjectId"].as_u64())
        else {
            return failure(req, hresult::E_FAIL, "missing rpcArgs");
        };
        if self.channels.contains_key(&dc_id) {
            return failure(
                req,
                hresult::ERROR_ALREADY_EXISTS,
                "RTCDataChannel already exists",
            );
        }
        let defer = self.defer.clone();
        let sink = Arc::new(move |event: DataChannelEvent| {
            defer(Box::new(move |pcs| pcs.on_data_channel_event(dc_id, event)))
        });
        match pc.create_data_channel(label, sink) {
            Ok(channel) => {
                self.channels
                    .insert(dc_id, DataChannelRecord { conn, channel });
                Outcome {
                    replies: envelope::ok_empty(req).into_iter().collect(),
                    note: Some(format!("RTCDataChannel {dc_id} \"{label}\"")),
                }
            }
            Err(e) => {
                let mut out = failure(
                    req,
                    hresult::E_FAIL,
                    "failed to create data channel on peer connection",
                );
                out.note = Some(format!("createDataChannel: {e}"));
                out
            }
        }
    }

    /// `open`, `closing`, `close` and `message` events of a data channel (§4).
    fn on_data_channel_event(&mut self, dc_id: u64, event: DataChannelEvent) -> Vec<Out> {
        let Some(rec) = self.channels.get(&dc_id) else {
            return Vec::new();
        };
        let sctp = rec.channel.id();
        let (name, args) = match event {
            DataChannelEvent::Open => ("open", json!({ "id": sctp })),
            DataChannelEvent::Closing => ("closing", json!({ "id": sctp })),
            DataChannelEvent::Closed => ("close", json!({ "id": sctp })),
            DataChannelEvent::Message(data) => (
                "message",
                json!({ "id": sctp, "base64MessageData": base64(&data) }),
            ),
        };
        let json = envelope::event("RTCDataChannel", Some(&json!(dc_id)), name, args);
        vec![Out {
            conn: rec.conn,
            json,
        }]
    }

    /// `rpcArgs[0] = {sdp_mid, sdp_mlineindex, sdp}`; the reply carries the remote description.
    fn add_ice_candidate(&mut self, conn: ConnId, req: &RpcRequest) -> Outcome {
        let (id, pc) = match self.pc(req) {
            Ok(p) => p,
            Err(o) => return o,
        };
        let Some(args) = req.args_array().first() else {
            return failure(req, hresult::E_INVALIDARG, "missing rpcArgs");
        };
        let Some(line) = args["sdp"].as_str() else {
            return failure(req, hresult::E_INVALIDARG, "missing sdp");
        };
        let init = IceCandidateInit {
            sdp_mid: args["sdp_mid"].as_str().unwrap_or("").to_owned(),
            sdp_mline_index: args["sdp_mlineindex"].as_i64().unwrap_or(0) as i32,
            candidate: line.to_owned(),
        };
        let req2 = req.clone();
        let defer = self.defer.clone();
        pc.add_ice_candidate(
            init,
            Box::new(move |res| {
                defer(Box::new(move |pcs| {
                    let json = match res {
                        Ok(()) => {
                            let desc = pcs
                                .pcs
                                .get(&id)
                                .and_then(|pc| pc.engine.remote_description());
                            let result = match desc {
                                Some(d) => json!({ "desc": description_json(&d) }),
                                None => json!({}),
                            };
                            envelope::ok_response(&req2, result)
                        }
                        Err(e) => envelope::error_response(&req2, error_hr(&e), Some(&e.message)),
                    };
                    json.map(|json| Out { conn, json }).into_iter().collect()
                }))
            }),
        );
        pending(req)
    }

    fn close(&mut self, req: &RpcRequest) -> Outcome {
        let (id, pc) = match self.pc(req) {
            Ok(p) => p,
            Err(o) => return o,
        };
        pc.close();
        Outcome {
            replies: envelope::ok_empty(req).into_iter().collect(),
            note: Some(format!("RTCPeerConnection {id} closed")),
        }
    }

    /// `getStats` (§3f). An empty selector asks for the legacy report, answered with no reports:
    /// a client without libwebrtc's legacy stats may send both lists empty. A selector names the
    /// peer connection, a sender or a receiver, and gets the engine's report as `result.stats`
    /// next to `result.receivers`, the contributing sources, which are empty without a mixing
    /// server. The bindings only report for the whole connection, so a sender or receiver selector
    /// gets that report too. An unknown object answers `"{}"`, a string, as the shipped client does.
    fn get_stats(&mut self, conn: ConnId, req: &RpcRequest) -> Outcome {
        let args = req.args_array().first();
        let selector = args
            .and_then(|a| a["selectorRpcObjectType"].as_str())
            .unwrap_or("");
        let selector_id = args.and_then(|a| a["selectorRpcObjectId"].as_u64());
        let Some(call_id) = req.call_id.clone() else {
            return pending(req);
        };
        let reply = |result: Value| Outcome {
            replies: vec![envelope::async_reply(&call_id, 0, Some(result))],
            note: None,
        };
        let pc = match self.pc(req) {
            Ok((_, pc)) => pc,
            Err(_) => return reply(json!("{}")),
        };
        match selector {
            "" => reply(json!({ "reports": [], "receivers": [] })),
            "RTCPeerConnection" => self.stats_report(conn, pc, call_id),
            "RTCRtpSender" | "RTCRtpReceiver" => {
                let known = selector_id.is_some_and(|n| {
                    if selector == "RTCRtpSender" {
                        self.sender_ids.contains_key(&n)
                    } else {
                        self.pcs
                            .values()
                            .any(|pc| pc.transceivers.iter().any(|t| t.receiver == n))
                    }
                });
                if known {
                    self.stats_report(conn, pc, call_id)
                } else {
                    reply(json!("{}"))
                }
            }
            _ => failure(
                req,
                hresult::ERROR_NOT_SUPPORTED,
                "unexpected selector rpcObjectType",
            ),
        }
    }

    fn stats_report(&self, conn: ConnId, pc: Arc<dyn PeerConnection>, call_id: Value) -> Outcome {
        let defer = self.defer.clone();
        pc.get_stats(Box::new(move |res| {
            defer(Box::new(move |_| {
                let json = match res.and_then(|text| parse_stats(&text)) {
                    Ok(stats) => envelope::async_reply(
                        &call_id,
                        0,
                        Some(json!({ "stats": stats, "receivers": [] })),
                    ),
                    Err(e) => rtc_error_reply(&call_id, &e),
                };
                vec![Out { conn, json }]
            }))
        }));
        Outcome {
            replies: Vec::new(),
            note: None,
        }
    }

    /// `removeTrack`: `rpcArgs[0].senderRpcObjectId`.
    fn remove_track(&mut self, req: &RpcRequest) -> Outcome {
        if let Err(o) = self.pc(req) {
            return o;
        }
        let Some(sender) = req
            .args_array()
            .first()
            .and_then(|a| a["senderRpcObjectId"].as_u64())
        else {
            return failure(req, hresult::ERROR_NOT_FOUND, "missing senderRpcObjectId");
        };
        let handle = self
            .sender_ids
            .get(&sender)
            .and_then(|at| self.pcs.get(&at.pc)?.transceivers.get(at.index))
            .map(|r| r.handle.clone());
        let Some(handle) = handle else {
            return failure(req, hresult::ERROR_NOT_FOUND, "Sender not found");
        };
        match handle.remove_track() {
            Ok(()) => Outcome {
                replies: envelope::ok_empty(req).into_iter().collect(),
                note: Some(format!("removeTrack: sender {sender}")),
            },
            Err(e) => rtc_error(req, &e),
        }
    }

    /// An engine event for peer connection `id`.
    pub fn on_event(&mut self, id: u64, event: PcEvent) -> Vec<Out> {
        let Some(pc) = self.pcs.get(&id) else {
            return Vec::new();
        };
        let conn = pc.conn;
        let target = json!(id);
        let ev = |name: &str, args: Value| envelope::event(OBJECT_TYPE, Some(&target), name, args);
        let json = match event {
            PcEvent::SignalingState(s) => {
                ev("signalingstatechange", json!({ "state": s.as_str() }))
            }
            PcEvent::IceConnectionState(s) => {
                ev("iceconnectionstatechange", json!({ "state": s.as_str() }))
            }
            PcEvent::ConnectionState(s) => {
                ev("connectionstatechange", json!({ "state": s.as_str() }))
            }
            PcEvent::IceGatheringState(s) => {
                ev("icegatheringstatechange", json!({ "state": s.as_str() }))
            }
            PcEvent::NegotiationNeeded => ev("negotiationneeded", Value::Null),
            PcEvent::IceCandidate(c) => {
                let Some(args) = ice_candidate_args(&c, pc.engine.local_description()) else {
                    return Vec::new();
                };
                ev("icecandidate", args)
            }
            PcEvent::Track(t) => match self.track_args(id, t) {
                Some(args) => ev("track", args),
                None => return Vec::new(),
            },
        };
        vec![Out { conn, json }]
    }

    /// The `track` event's arguments (§4). Ids come from the one counter, in the order the
    /// shipped client takes them: the stream, then the track, then the transceiver triple unless
    /// a set-description reply already allocated it (`reuseReceiver`).
    fn track_args(&mut self, id: u64, t: TrackInfo) -> Option<Value> {
        // The shipped client skips records without a mid, and a remote track has one by now.
        let mid = t.transceiver.mid.clone()?;
        let stream = match t.stream_ids.first() {
            Some(sid) => {
                let n = match self.streams.get(sid) {
                    Some(n) => *n,
                    None => {
                        let n = self.alloc(1);
                        self.streams.insert(sid.clone(), n);
                        n
                    }
                };
                Some((n, sid.clone()))
            }
            None => None,
        };
        let track_n = match self.tracks.get(&t.track_id) {
            Some(n) => *n,
            None => {
                let n = self.alloc(1);
                self.tracks.insert(t.track_id.clone(), n);
                n
            }
        };
        // The set-description reply may have made the record already (or the host did, for a
        // transceiver of its own), and then its ids are reused.
        let known = self.sync_transceivers(id, None);
        let index = self
            .pcs
            .get(&id)?
            .transceivers
            .iter()
            .position(|r| r.handle.info().mid.as_deref() == Some(mid.as_str()))?;
        let reuse = index < known;
        // This event is what tells the host about its transceiver; any other one that was found
        // along the way is still to be reported by a reply.
        let pc = self.pcs.get_mut(&id)?;
        pc.transceivers[index].unreported = false;
        let pc = &*pc;
        let stream_n = stream.as_ref().map(|(n, _)| *n);
        // The element that shows this stream may already exist (audio arrived first), or arrive
        // later: either way the track has to be findable by the ids Teams was given.
        if let Some(handle) = &t.track {
            self.remote_tracks.insert(track_n, handle.0.clone());
            if let Some((sn, _)) = &stream {
                let tracks = self.remote_streams.entry(*sn).or_default();
                if !tracks.contains(&track_n) {
                    tracks.push(track_n);
                }
            }
        }
        let rec = &pc.transceivers[index];
        let mut receiver = json!({ "rpcObjectId": rec.receiver });
        if let Some(kind) = t.transceiver.kind {
            receiver["kind"] = json!(kind.as_str());
        }
        let mut args = Map::new();
        args.insert("receiver".into(), receiver);
        args.insert("sender".into(), json!({ "rpcObjectId": rec.sender }));
        args.insert(
            "transceiver".into(),
            json!({
                "rpcObjectId": rec.id,
                "direction": t.transceiver.direction.as_str(),
                "mid": mid,
            }),
        );
        args.insert(
            "track".into(),
            json!({ "rpcObjectId": track_n, "id": t.track_id }),
        );
        if let Some((n, sid)) = stream {
            args.insert("stream".into(), json!({ "rpcObjectId": n, "id": sid }));
        }
        args.insert("reuseReceiver".into(), json!(reuse));
        if let Some(n) = stream_n {
            self.refresh_elements(n);
        }
        Some(Value::Object(args))
    }
}

impl Drop for PeerConnections {
    fn drop(&mut self) {
        for pc in self.pcs.values() {
            pc.engine.close();
        }
        for stream in self.local_streams.values() {
            stream.tracks.iter().for_each(|t| t.stop());
        }
    }
}

pub(crate) fn object_id(req: &RpcRequest) -> Option<u64> {
    let v = req.object_id.as_ref()?;
    v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))
}

/// An error reply that echoes the call, with the message in `errorDetails`.
pub(crate) fn failure(req: &RpcRequest, hr: u32, details: &str) -> Outcome {
    Outcome {
        replies: envelope::error_response(req, hr, Some(details))
            .into_iter()
            .collect(),
        note: Some(details.to_owned()),
    }
}

/// A libwebrtc error of a synchronous call: `result.RtcError` / `RtcErrorDetails`, `E_FAIL`.
pub(crate) fn rtc_error(req: &RpcRequest, e: &EngineError) -> Outcome {
    Outcome {
        replies: envelope::error_with_result(
            req,
            hresult::E_FAIL,
            json!({ "RtcError": e.kind as i32, "RtcErrorDetails": e.message }),
        )
        .into_iter()
        .collect(),
        note: Some(e.to_string()),
    }
}

pub(crate) fn parse_direction(s: &str) -> Option<Direction> {
    Some(match s {
        "sendrecv" => Direction::SendRecv,
        "sendonly" => Direction::SendOnly,
        "recvonly" => Direction::RecvOnly,
        "inactive" => Direction::Inactive,
        _ => return None,
    })
}

/// Standard base64 with padding, for `message` events.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The engine's report text as JSON: an array. The empty string is an empty report.
fn parse_stats(text: &str) -> Result<Value, EngineError> {
    if text.trim().is_empty() {
        return Ok(json!([]));
    }
    serde_json::from_str(text)
        .map_err(|e| EngineError::new(ErrorKind::Internal, format!("unreadable stats report: {e}")))
}

/// The reply comes later, from the engine.
fn pending(req: &RpcRequest) -> Outcome {
    Outcome {
        replies: Vec::new(),
        note: Some(format!("{} pending", req.name)),
    }
}

/// `hr` of a failed synchronous-form reply.
fn error_hr(e: &EngineError) -> u32 {
    if e.kind == ErrorKind::SdpParse {
        hresult::ERROR_INVALID_DATA
    } else {
        hresult::E_FAIL
    }
}

/// How a failed asynchronous call is reported: a parse failure like the synchronous errors, a
/// libwebrtc error as `result.RtcError` / `RtcErrorDetails` with `E_FAIL` (§3c).
fn engine_failure(req: &RpcRequest, call_id: &Value, e: &EngineError) -> String {
    if e.kind == ErrorKind::SdpParse {
        envelope::error_response(req, hresult::ERROR_INVALID_DATA, Some(&e.message))
            .unwrap_or_default()
    } else {
        rtc_error_reply(call_id, e)
    }
}

fn rtc_error_reply(call_id: &Value, e: &EngineError) -> String {
    envelope::async_reply(
        call_id,
        hresult::E_FAIL,
        Some(json!({ "RtcError": e.kind as i32, "RtcErrorDetails": e.message })),
    )
}

/// The reply of `createOffer` / `createAnswer`. Empty for a notification.
fn description_reply(
    call_id: Option<&Value>,
    res: Result<SessionDescription, EngineError>,
) -> String {
    let Some(call_id) = call_id else {
        return String::new();
    };
    match res {
        Ok(d) => envelope::async_reply(call_id, 0, Some(json!({ "desc": description_json(&d) }))),
        Err(e) => rtc_error_reply(call_id, &e),
    }
}

fn description_json(d: &SessionDescription) -> Value {
    json!({ "type": d.kind.as_str(), "sdp": d.sdp })
}

fn parse_description(arg: Option<&Value>) -> Result<SessionDescription, (u32, &'static str)> {
    let Some(arg) = arg else {
        return Err((hresult::E_INVALIDARG, "missing desc"));
    };
    let desc = arg.get("desc").filter(|d| d.is_object()).unwrap_or(arg);
    let Some(kind) = desc["type"].as_str().and_then(SdpType::parse) else {
        return Err((hresult::E_INVALIDARG, "invalid type"));
    };
    let Some(sdp) = desc["sdp"].as_str() else {
        return Err((hresult::E_INVALIDARG, "missing desc"));
    };
    Ok(SessionDescription {
        kind,
        sdp: sdp.to_owned(),
    })
}

/// `rpcArgs[0]` of `createPeerConnection`. `None` for a configuration this client cannot honour:
/// only unified-plan SDP is supported. Policies other than `relay` are left to libwebrtc's
/// defaults (max-bundle, rtcp-mux required), which is what Teams asks for.
fn parse_config(arg: Option<&Value>) -> Option<PcConfig> {
    let Some(arg) = arg.filter(|a| a.is_object()) else {
        return Some(PcConfig::default());
    };
    if let Some(s) = arg["sdpSemantics"].as_str()
        && s != "unified-plan"
    {
        return None;
    }
    let ice_servers = arg["iceServers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| IceServer {
            urls: envelope::one_or_many(&s["urls"])
                .into_iter()
                .filter_map(|u| u.as_str().map(str::to_owned))
                .collect(),
            username: s["username"].as_str().unwrap_or("").to_owned(),
            credential: s["credential"].as_str().unwrap_or("").to_owned(),
        })
        .collect();
    Some(PcConfig {
        ice_servers,
        relay_only: arg["iceTransportPolicy"] == "relay",
    })
}

fn rtp_caps_json(c: &RtpCapabilities, advertise: bool) -> Value {
    if !advertise {
        return json!({ "codecs": [], "headerExtensions": [] });
    }
    let codecs: Vec<Value> = c
        .codecs
        .iter()
        .map(|k| {
            let mut m = Map::new();
            m.insert("mimeType".into(), json!(k.mime_type));
            if let Some(r) = k.clock_rate {
                m.insert("clockRate".into(), json!(r));
            }
            if let Some(n) = k.channels {
                m.insert("channels".into(), json!(n));
            }
            if let Some(f) = &k.sdp_fmtp_line {
                m.insert("sdpFmtpLine".into(), json!(f));
            }
            Value::Object(m)
        })
        .collect();
    let extensions: Vec<Value> = c
        .header_extensions
        .iter()
        .map(|uri| json!({ "uri": uri }))
        .collect();
    json!({ "codecs": codecs, "headerExtensions": extensions })
}

fn capabilities_json(c: &Capabilities, video: bool) -> Value {
    json!({
        "sendCapabilities": {
            "audio": rtp_caps_json(&c.send_audio, true),
            "video": rtp_caps_json(&c.send_video, video),
        },
        "recvCapabilities": {
            "audio": rtp_caps_json(&c.recv_audio, true),
            "video": rtp_caps_json(&c.recv_video, video),
        },
    })
}

/// `rpcEventArgs` of `icecandidate`: the local description and the candidate broken into fields
/// (§4). `None` if the line is not a candidate.
fn ice_candidate_args(c: &IceCandidateInit, local: Option<SessionDescription>) -> Option<Value> {
    let f = candidate::parse(&c.candidate)?;
    let mut cand = Map::new();
    cand.insert("sdp_mid".into(), json!(c.sdp_mid));
    cand.insert("sdp_mline_index".into(), json!(c.sdp_mline_index));
    cand.insert("foundation".into(), json!(f.foundation));
    cand.insert("component".into(), json!(f.component));
    cand.insert("protocol".into(), json!(f.protocol));
    if f.protocol == "tcp"
        && let Some(t) = &f.tcp_type
    {
        cand.insert("tcpType".into(), json!(t));
    }
    cand.insert("priority".into(), json!(f.priority));
    cand.insert("address".into(), json!(f.address));
    cand.insert("port".into(), json!(f.port));
    cand.insert("type".into(), json!(f.typ));
    if let Some(a) = &f.related_address {
        cand.insert("relatedAddress".into(), json!(a));
    }
    if let Some(p) = f.related_port {
        cand.insert("relatedPort".into(), json!(p));
    }
    cand.insert(
        "usernameFragment".into(),
        json!(f.username_fragment.unwrap_or_default()),
    );
    cand.insert("candidate".into(), json!(c.candidate));
    let mut args = Map::new();
    if let Some(d) = local {
        args.insert("desc".into(), description_json(&d));
    }
    args.insert("candidate".into(), Value::Object(cand));
    Some(Value::Object(args))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Receiver};

    use media::Kind;
    use media::fake::FakeEngine;

    use super::*;

    /// A dispatcher stand-in: deferred work queues up here and `pump` runs it.
    struct Rig {
        pcs: PeerConnections,
        rx: Receiver<Deferred>,
    }

    impl Rig {
        fn new(engine: FakeEngine) -> Rig {
            let (tx, rx) = mpsc::channel();
            let tx = Mutex::new(tx);
            let defer: Defer = Arc::new(move |d| {
                let _ = tx.lock().unwrap().send(d);
            });
            Rig {
                pcs: PeerConnections::new(
                    Arc::new(engine),
                    defer,
                    Arc::new(crate::video::NullSurface),
                    true,
                ),
                rx,
            }
        }

        /// Runs the queued work and returns what it would send.
        fn pump(&mut self) -> Vec<Value> {
            let mut out = Vec::new();
            while let Ok(d) = self.rx.try_recv() {
                out.extend(
                    d(&mut self.pcs)
                        .into_iter()
                        .map(|o| serde_json::from_str::<Value>(&o.json).unwrap()),
                );
            }
            out
        }

        fn call(&mut self, json: &str) -> (Vec<Value>, Option<String>) {
            let req = envelope::parse_request(json).unwrap();
            let o = self.pcs.handle(1, &req).expect("handled");
            let mut replies: Vec<Value> = o
                .replies
                .iter()
                .map(|r| serde_json::from_str(r).unwrap())
                .collect();
            replies.extend(self.pump());
            (replies, o.note)
        }
    }

    fn rpc(id: u64, call: u64, name: &str, args: &str) -> String {
        format!(
            r#"{{"rpcCallId":{call},"rpcObjectType":"RTCPeerConnection","rpcObjectId":{id},"rpcName":"{name}","rpcArgs":{args}}}"#
        )
    }

    fn created() -> Rig {
        let mut rig = Rig::new(FakeEngine::default());
        let (r, _) = rig.call(&rpc(
            3,
            1,
            "createPeerConnection",
            r#"[{"sdpSemantics":"unified-plan"},{}]"#,
        ));
        assert_eq!(r[0]["hr"], json!(0));
        rig
    }

    #[test]
    fn create_replies_with_capabilities_and_no_id() {
        let mut rig = Rig::new(FakeEngine::default());
        let (r, _) = rig.call(&rpc(
            3,
            42,
            "createPeerConnection",
            r#"[{"sdpSemantics":"unified-plan"},{}]"#,
        ));
        let reply = &r[0];
        assert_eq!(reply["rpcObjectType"], json!("RTCPeerConnection"));
        assert_eq!(reply["rpcObjectId"], json!(3));
        assert_eq!(reply["rpcCallId"], json!(42));
        let send = &reply["result"]["sendCapabilities"];
        assert_eq!(send["audio"]["codecs"][0]["mimeType"], json!("audio/opus"));
        assert_eq!(send["audio"]["codecs"][0]["channels"], json!(2));
        assert_eq!(
            send["audio"]["headerExtensions"][0],
            json!({"uri": "urn:ietf:params:rtp-hdrext:ssrc-audio-level"})
        );
        assert_eq!(send["video"], json!({"codecs": [], "headerExtensions": []}));
        assert!(reply["result"]["recvCapabilities"]["audio"]["codecs"].is_array());
        assert_eq!(rig.pcs.len(), 1);
    }

    #[test]
    fn create_errors() {
        let mut rig = created();
        let (dup, _) = rig.call(&rpc(3, 2, "createPeerConnection", "[{}]"));
        assert_eq!(dup[0]["hr"], json!(hresult::ERROR_ALREADY_EXISTS as i32));
        assert_eq!(
            dup[0]["errorDetails"],
            json!("RTCPeerConnection already exists")
        );
        let (planb, _) = rig.call(&rpc(
            4,
            3,
            "createPeerConnection",
            r#"[{"sdpSemantics":"plan-b"}]"#,
        ));
        assert_eq!(planb[0]["hr"], json!(hresult::E_INVALIDARG as i32));
        let (missing, _) = rig.call(
            r#"{"rpcCallId":4,"rpcObjectType":"RTCPeerConnection","rpcName":"createPeerConnection","rpcArgs":[{}]}"#,
        );
        assert_eq!(missing[0]["errorDetails"], json!("missing rpcObjectId"));
        let (unknown, _) = rig.call(&rpc(9, 5, "createOffer", "[]"));
        assert_eq!(
            unknown[0]["errorDetails"],
            json!("RTCPeerConnection not found")
        );
        assert_eq!(unknown[0]["hr"], json!(hresult::ERROR_NOT_FOUND as i32));
    }

    #[test]
    fn offer_reply_is_bare_and_comes_from_the_engine() {
        let mut rig = created();
        let req = envelope::parse_request(&rpc(3, 47, "createOffer", "[]")).unwrap();
        let o = rig.pcs.handle(1, &req).unwrap();
        assert!(o.replies.is_empty(), "async: nothing is sent at once");
        let r = rig.pump();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["rpcCallId"], json!(47));
        assert_eq!(r[0]["hr"], json!(0));
        assert_eq!(r[0]["result"]["desc"]["type"], json!("offer"));
        assert!(
            r[0]["result"]["desc"]["sdp"]
                .as_str()
                .unwrap()
                .starts_with("v=0")
        );
        assert!(r[0].get("rpcObjectType").is_none() && r[0].get("rpcName").is_none());
    }

    #[test]
    fn a_remote_offer_gives_a_track_event_first_and_then_plain_transceivers() {
        let mut rig = created();
        let offer = r#"[{"type":"offer","sdp":"v=0\r\nm=audio 9 X 111\r\na=mid:0\r\n"}]"#;
        let (r, _) = rig.call(&rpc(3, 10, "setRemoteDescription", offer));
        // The fake reports the track before completing, as libwebrtc does.
        let track = r
            .iter()
            .find(|m| m["rpcEventName"] == "track")
            .expect("track event");
        let a = &track["rpcEventArgs"];
        assert_eq!(
            track["rpcEventTarget"],
            json!({"rpcObjectType": "RTCPeerConnection", "rpcObjectId": 3})
        );
        assert_eq!(
            a["stream"],
            json!({"rpcObjectId": 4294967296u64, "id": "stream1"})
        );
        assert_eq!(a["track"]["rpcObjectId"], json!(4294967297u64));
        assert_eq!(
            a["transceiver"],
            json!({"rpcObjectId": 4294967298u64, "direction": "sendrecv", "mid": "0"})
        );
        assert_eq!(
            a["receiver"],
            json!({"rpcObjectId": 4294967299u64, "kind": "audio"})
        );
        assert_eq!(a["sender"], json!({"rpcObjectId": 4294967300u64}));
        assert_eq!(a["reuseReceiver"], json!(false));
        let reply = r.iter().find(|m| m["rpcCallId"] == 10).expect("reply");
        assert_eq!(reply["hr"], json!(0));
        assert_eq!(
            reply["result"]["transceivers"],
            json!([{"rpcObjectId": 4294967298u64, "direction": "sendrecv", "mid": "0", "kind": "audio"}])
        );
        assert!(reply.get("rpcName").is_none());
    }

    #[test]
    fn a_reply_that_allocates_first_reports_the_ids_once() {
        let mut rig = created();
        // A locally created transceiver gets its mid when the offer is set.
        let pc = rig.pcs.pcs.get(&3).unwrap().engine.clone();
        pc.add_transceiver(Kind::Audio, Direction::SendRecv)
            .unwrap();
        rig.pump();
        let (r, _) = rig.call(&rpc(
            3,
            11,
            "setLocalDescription",
            r#"[{"type":"offer","sdp":"v=0\r\n"}]"#,
        ));
        let reply = r.iter().find(|m| m["rpcCallId"] == 11).unwrap();
        let t = &reply["result"]["transceivers"][0];
        assert_eq!(t["addedFromSdp"], json!(true));
        assert_eq!(t["receiver"], json!({"rpcObjectId": 4294967297u64}));
        assert_eq!(t["sender"], json!({"rpcObjectId": 4294967298u64}));
        // The second report omits them.
        let (r, _) = rig.call(&rpc(
            3,
            12,
            "setLocalDescription",
            r#"[{"type":"offer","sdp":"v=0\r\n"}]"#,
        ));
        let t = &r.iter().find(|m| m["rpcCallId"] == 12).unwrap()["result"]["transceivers"][0];
        assert!(t.get("addedFromSdp").is_none() && t.get("receiver").is_none());
        // A later remote description for the same mid reuses the record.
        let pc = rig.pcs.pcs.get(&3).unwrap().engine.clone();
        pc.set_remote_description(
            SessionDescription {
                kind: SdpType::Offer,
                sdp: "v=0\r\nm=audio 9 X 111\r\na=mid:0\r\n".into(),
            },
            Box::new(|_| ()),
        );
        let events = rig.pump();
        let track = events
            .iter()
            .find(|m| m["rpcEventName"] == "track")
            .unwrap();
        assert_eq!(track["rpcEventArgs"]["reuseReceiver"], json!(true));
        assert_eq!(
            track["rpcEventArgs"]["transceiver"]["rpcObjectId"],
            json!(4294967296u64)
        );
    }

    #[test]
    fn description_arguments_may_be_wrapped() {
        let mut rig = created();
        let (r, _) = rig.call(&rpc(
            3,
            20,
            "setRemoteDescription",
            r#"[{"desc":{"type":"offer","sdp":"v=0\r\n"}}]"#,
        ));
        assert_eq!(
            r.iter().find(|m| m["rpcCallId"] == 20).unwrap()["hr"],
            json!(0)
        );
        let (bad, _) = rig.call(&rpc(
            3,
            21,
            "setRemoteDescription",
            r#"[{"type":"nonsense","sdp":"v=0"}]"#,
        ));
        assert_eq!(bad[0]["errorDetails"], json!("invalid type"));
        assert_eq!(bad[0]["hr"], json!(hresult::E_INVALIDARG as i32));
        let (none, _) = rig.call(&rpc(3, 22, "setLocalDescription", "[]"));
        assert_eq!(none[0]["errorDetails"], json!("missing desc"));
    }

    #[test]
    fn engine_failures_use_the_rtc_error_form() {
        let mut rig = Rig::new(FakeEngine::failing());
        rig.call(&rpc(3, 1, "createPeerConnection", "[{}]"));
        let (r, _) = rig.call(&rpc(3, 30, "createOffer", "[]"));
        assert_eq!(
            r[0],
            json!({
                "result": {"RtcError": 6, "RtcErrorDetails": "fake failure"},
                "rpcCallId": 30,
                "hr": hresult::E_FAIL as i32,
            })
        );
    }

    #[test]
    fn add_ice_candidate_replies_with_the_remote_description() {
        let mut rig = created();
        rig.call(&rpc(
            3,
            40,
            "setRemoteDescription",
            r#"[{"type":"offer","sdp":"v=0\r\nm=audio 9 X 111\r\na=mid:0\r\n"}]"#,
        ));
        let args = r#"[{"sdp_mid":"0","sdp_mlineindex":0,"sdp":"candidate:1 1 udp 2122260223 192.0.2.10 50000 typ host"}]"#;
        let (r, _) = rig.call(&rpc(3, 41, "addIceCandidate", args));
        assert_eq!(r[0]["rpcName"], json!("addIceCandidate"));
        assert_eq!(r[0]["rpcObjectId"], json!(3));
        assert_eq!(r[0]["result"]["desc"]["type"], json!("offer"));
        let (bad, _) = rig.call(&rpc(
            3,
            42,
            "addIceCandidate",
            r#"[{"sdp_mid":"0","sdp_mlineindex":0,"sdp":"nope"}]"#,
        ));
        assert_eq!(bad[0]["hr"], json!(hresult::ERROR_INVALID_DATA as i32));
        assert!(
            bad[0]["errorDetails"]
                .as_str()
                .unwrap()
                .starts_with("SDP parse failure")
        );
        let (missing, _) = rig.call(&rpc(3, 43, "addIceCandidate", "[]"));
        assert_eq!(missing[0]["errorDetails"], json!("missing rpcArgs"));
    }

    #[test]
    fn events_use_the_windows_shapes() {
        let mut rig = created();
        let out = |rig: &mut Rig, e: PcEvent| -> Value {
            let o = rig.pcs.on_event(3, e);
            serde_json::from_str(&o[0].json).unwrap()
        };
        let s = out(
            &mut rig,
            PcEvent::SignalingState(media::SignalingState::HaveRemoteOffer),
        );
        assert_eq!(s["rpcEventName"], json!("signalingstatechange"));
        assert_eq!(s["rpcEventArgs"], json!({"state": "have-remote-offer"}));
        assert_eq!(s["hr"], json!(0));
        let n = out(&mut rig, PcEvent::NegotiationNeeded);
        assert!(n.get("rpcEventArgs").is_none());
        let g = out(
            &mut rig,
            PcEvent::IceGatheringState(media::IceGatheringState::Complete),
        );
        assert_eq!(g["rpcEventArgs"], json!({"state": "complete"}));
        let c = out(&mut rig, PcEvent::IceCandidate(IceCandidateInit {
            sdp_mid: "0".into(),
            sdp_mline_index: 0,
            candidate: "candidate:1234567890 1 udp 2122260223 192.0.2.10 50000 typ host generation 0 ufrag abcd network-id 1".into(),
        }));
        let cand = &c["rpcEventArgs"]["candidate"];
        assert_eq!(cand["sdp_mline_index"], json!(0));
        assert_eq!(cand["component"], json!("rtp"));
        assert_eq!(cand["priority"], json!(2122260223u64));
        assert_eq!(cand["port"], json!(50000));
        assert_eq!(cand["type"], json!("host"));
        assert_eq!(cand["usernameFragment"], json!("abcd"));
        assert!(cand.get("relatedAddress").is_none() && cand.get("tcpType").is_none());
        // No local description yet, so no `desc`.
        assert!(c["rpcEventArgs"].get("desc").is_none());
        // An event for an unknown peer connection is dropped.
        assert!(rig.pcs.on_event(99, PcEvent::NegotiationNeeded).is_empty());
    }

    #[test]
    fn close_stats_and_set_configuration() {
        let mut rig = created();
        let (c, _) = rig.call(&rpc(3, 50, "close", "[{}]"));
        assert_eq!(
            c[0],
            json!({"rpcCallId": 50, "rpcObjectType": "RTCPeerConnection", "rpcObjectId": 3, "rpcName": "close", "hr": 0})
        );
        let (cfg, _) = rig.call(&rpc(3, 51, "setConfiguration", "[{}]"));
        assert_eq!(cfg[0]["hr"], json!(hresult::E_NOTIMPL as i32));
        assert!(cfg[0].get("errorDetails").is_none());
        let (st, _) = rig.call(&rpc(
            3,
            52,
            "getStats",
            r#"[{"selectorRpcObjectType":"","selectorRpcObjectId":0}]"#,
        ));
        assert_eq!(
            st[0],
            json!({"result": {"reports": [], "receivers": []}, "rpcCallId": 52, "hr": 0})
        );
        let call =
            |rig: &mut Rig, id: u64, n: u64, args: &str| rig.call(&rpc(id, n, "getStats", args)).0;
        let st = call(
            &mut rig,
            3,
            53,
            r#"[{"selectorRpcObjectType":"RTCPeerConnection"}]"#,
        );
        assert_eq!(st[0]["result"]["stats"][0]["type"], json!("codec"));
        assert_eq!(st[0]["result"]["receivers"], json!([]));
        assert_eq!(st[0]["rpcCallId"], json!(53));
        assert!(st[0].get("rpcName").is_none());
        let st = call(
            &mut rig,
            3,
            54,
            r#"[{"selectorRpcObjectType":"RTCRtpSender","selectorRpcObjectId":7}]"#,
        );
        assert_eq!(st[0]["result"], json!("{}"));
        let st = call(
            &mut rig,
            9,
            55,
            r#"[{"selectorRpcObjectType":"RTCPeerConnection"}]"#,
        );
        assert_eq!(st[0]["result"], json!("{}"));
        let st = call(&mut rig, 3, 56, r#"[{"selectorRpcObjectType":"nonsense"}]"#);
        assert_eq!(
            st[0]["errorDetails"],
            json!("unexpected selector rpcObjectType")
        );
    }

    #[test]
    fn remove_track_clears_the_sender() {
        let mut rig = created();
        rig_call(
            &mut rig,
            &rpc(
                3,
                1,
                "addTransceiver",
                r#"[{"kind":"audio","direction":"sendrecv","transceiverRpcObjectId":4,"senderRpcObjectId":5,"receiverRpcObjectId":6}]"#,
            ),
        );
        let r = rig_call(
            &mut rig,
            &rpc(3, 2, "removeTrack", r#"[{"senderRpcObjectId":5}]"#),
        );
        assert_eq!(r[0]["hr"], json!(0));
        assert!(r[0].get("result").is_none());
        let h = rig.pcs.pcs.get(&3).unwrap().transceivers[0].handle.clone();
        assert_eq!(h.info().direction, Direction::RecvOnly);
        let r = rig_call(
            &mut rig,
            &rpc(3, 3, "removeTrack", r#"[{"senderRpcObjectId":99}]"#),
        );
        assert_eq!(r[0]["errorDetails"], json!("Sender not found"));
        assert_eq!(r[0]["hr"], json!(hresult::ERROR_NOT_FOUND as i32));
        let r = rig_call(&mut rig, &rpc(3, 4, "removeTrack", "[{}]"));
        assert_eq!(r[0]["errorDetails"], json!("missing senderRpcObjectId"));
    }

    #[test]
    fn a_closed_connection_takes_its_peer_connections() {
        let mut rig = created();
        assert_eq!(rig.pcs.len(), 1);
        rig.pcs.conn_closed(2);
        assert_eq!(rig.pcs.len(), 1);
        rig.pcs.conn_closed(1);
        assert!(rig.pcs.is_empty());
    }

    fn rig_call(rig: &mut Rig, json: &str) -> Vec<Value> {
        rig.call(json).0
    }

    /// 2026-11-02, a 1:1 call: the service offers `audio_0` (it sends) and `video_1` (`recvonly`:
    /// it wants our video). Only the audio gets a `track` event, so the reply to the offer has to
    /// carry the ids of the video transceiver or Teams cannot attach the camera to it.
    #[test]
    fn a_receive_only_section_is_reported_with_its_ids_by_the_reply() {
        let mut rig = created();
        let sdp = "v=0\r\nm=audio 9 X 111\r\na=mid:audio_0\r\nm=video 9 X 96\r\na=mid:video_1\r\na=recvonly\r\n";
        let args = json!([{ "type": "offer", "sdp": sdp }]).to_string();
        let (r, _) = rig.call(&rpc(3, 10, "setRemoteDescription", &args));
        let events: Vec<&Value> = r.iter().filter(|m| m["rpcEventName"] == "track").collect();
        assert_eq!(events.len(), 1, "only the audio has a track");
        let audio = &events[0]["rpcEventArgs"];
        let reply = r.iter().find(|m| m["rpcCallId"] == 10).unwrap();
        let ts = reply["result"]["transceivers"].as_array().unwrap();
        assert_eq!(ts.len(), 2);
        // The audio was reported by its event, so the reply lists it plainly.
        assert!(ts[0].get("addedFromSdp").is_none());
        assert_eq!(ts[0]["rpcObjectId"], audio["transceiver"]["rpcObjectId"]);
        // The video was not, so the reply carries everything Teams needs to use it.
        let video = &ts[1];
        assert_eq!(video["mid"], json!("video_1"));
        assert_eq!(video["addedFromSdp"], json!(true));
        let (rid, sid) = (
            video["receiver"]["rpcObjectId"].as_u64().unwrap(),
            video["sender"]["rpcObjectId"].as_u64().unwrap(),
        );
        assert_eq!(sid, video["rpcObjectId"].as_u64().unwrap() + 2);
        assert_eq!(rid, video["rpcObjectId"].as_u64().unwrap() + 1);
        // Teams can now act on it: its sender id finds the transceiver.
        assert!(rig.pcs.sender_ids.contains_key(&sid));
        // And it is reported once.
        let (r, _) = rig.call(&rpc(
            3,
            11,
            "setLocalDescription",
            r#"[{"type":"answer","sdp":"v=0\r\n"}]"#,
        ));
        let reply = r.iter().find(|m| m["rpcCallId"] == 11).unwrap();
        assert!(
            reply["result"]["transceivers"][1]
                .get("addedFromSdp")
                .is_none()
        );
    }

    #[test]
    fn host_created_transceivers_keep_the_host_ids() {
        let mut rig = created();
        let args = |kind: &str, t: u64| {
            format!(
                r#"[{{"trackRpcObjectId":0,"kind":"{kind}","direction":"inactive","transceiverRpcObjectId":{t},"senderRpcObjectId":{},"receiverRpcObjectId":{}}}]"#,
                t + 1,
                t + 2
            )
        };
        let r = rig_call(&mut rig, &rpc(3, 60, "addTransceiver", &args("audio", 4)));
        assert_eq!(r[0]["hr"], json!(0));
        assert!(r[0].get("result").is_none());
        rig_call(&mut rig, &rpc(3, 61, "addTransceiver", &args("video", 7)));
        assert_eq!(rig.pcs.transceiver_ids.len(), 2);
        assert_eq!(rig.pcs.sender_ids.get(&5).unwrap().index, 0);
        assert_eq!(rig.pcs.sender_ids.get(&8).unwrap().index, 1);
        // Setting the offer reports them plainly: no addedFromSdp, no receiver/sender.
        let r = rig_call(
            &mut rig,
            &rpc(
                3,
                62,
                "setLocalDescription",
                r#"[{"type":"offer","sdp":"v=0\r\n"}]"#,
            ),
        );
        let reply = r.iter().find(|m| m["rpcCallId"] == 62).unwrap();
        let ts = reply["result"]["transceivers"].as_array().unwrap();
        assert_eq!(ts.len(), 2);
        assert_eq!(
            ts[0],
            json!({"rpcObjectId": 4, "direction": "inactive", "mid": "0", "kind": "audio"})
        );
        assert_eq!(ts[1]["rpcObjectId"], json!(7));
        // The host's transceiver gets a remote track: the record is reused.
        let pc = rig.pcs.pcs.get(&3).unwrap().engine.clone();
        pc.set_remote_description(
            SessionDescription {
                kind: SdpType::Answer,
                sdp: "v=0\r\n".into(),
            },
            Box::new(|_| ()),
        );
        rig.pump();
    }

    #[test]
    fn add_transceiver_errors() {
        let mut rig = created();
        let r = rig_call(
            &mut rig,
            &rpc(3, 1, "addTransceiver", r#"[{"direction":"inactive"}]"#),
        );
        assert_eq!(
            r[0]["errorDetails"],
            json!("missing kind for new transceiver")
        );
        let r = rig_call(
            &mut rig,
            &rpc(3, 2, "addTransceiver", r#"[{"kind":"data"}]"#),
        );
        assert_eq!(
            r[0]["errorDetails"],
            json!("transceiver.kind was not audio or video")
        );
        let r = rig_call(
            &mut rig,
            &rpc(3, 3, "addTransceiver", r#"[{"kind":"audio"}]"#),
        );
        assert_eq!(
            r[0]["errorDetails"],
            json!("missing rpcObjectId for transceiver, sender, and/or receiver")
        );
        let r = rig_call(
            &mut rig,
            &rpc(
                3,
                4,
                "addTransceiver",
                r#"[{"kind":"audio","trackRpcObjectId":9,"transceiverRpcObjectId":4,"senderRpcObjectId":5,"receiverRpcObjectId":6}]"#,
            ),
        );
        assert_eq!(r[0]["errorDetails"], json!("Track not found"));
        assert!(rig.pcs.transceiver_ids.is_empty());
    }

    #[test]
    fn a_data_channel_reports_open_messages_and_close() {
        let mut rig = created();
        let r = rig_call(
            &mut rig,
            &rpc(
                3,
                70,
                "createDataChannel",
                r#"[{"label":"main-channel","rpcObjectId":3}]"#,
            ),
        );
        let reply = r.iter().find(|m| m["rpcCallId"] == 70).unwrap();
        assert_eq!(reply["hr"], json!(0));
        assert!(reply.get("result").is_none());
        let open = r
            .iter()
            .find(|m| m["rpcEventName"] == "open")
            .expect("open event");
        assert_eq!(
            open["rpcEventTarget"],
            json!({"rpcObjectType": "RTCDataChannel", "rpcObjectId": 3})
        );
        assert_eq!(open["rpcEventArgs"], json!({"id": 1}));
        let out = rig
            .pcs
            .on_data_channel_event(3, DataChannelEvent::Message(b"hi!".to_vec()));
        let m: Value = serde_json::from_str(&out[0].json).unwrap();
        assert_eq!(
            m["rpcEventArgs"],
            json!({"id": 1, "base64MessageData": "aGkh"})
        );
        let dup = rig_call(
            &mut rig,
            &rpc(
                3,
                71,
                "createDataChannel",
                r#"[{"label":"x","rpcObjectId":3}]"#,
            ),
        );
        assert_eq!(dup[0]["hr"], json!(hresult::ERROR_ALREADY_EXISTS as i32));
        let none = rig_call(&mut rig, &rpc(3, 72, "createDataChannel", "[]"));
        assert_eq!(none[0]["errorDetails"], json!("missing rpcArgs"));
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    /// A rig whose surface records what would be drawn.
    fn video_rig() -> (Rig, Arc<crate::video::fake::RecordingSurface>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let defer: Defer = Arc::new(move |d| {
            let _ = tx.lock().unwrap().send(d);
        });
        let surface = Arc::new(crate::video::fake::RecordingSurface::default());
        let mut rig = Rig {
            pcs: PeerConnections::new(
                Arc::new(FakeEngine::default()),
                defer,
                surface.clone(),
                true,
            ),
            rx,
        };
        let (r, _) = rig.call(&rpc(3, 1, "createPeerConnection", "[{}]"));
        assert_eq!(r[0]["hr"], json!(0));
        (rig, surface)
    }

    fn element_call(rig: &mut Rig, object: &str, id: u64, name: &str, args: &str) -> Value {
        let req = envelope::parse_request(&format!(
            r#"{{"rpcCallId":9,"rpcObjectType":"{object}","rpcObjectId":{id},"rpcName":"{name}","rpcArgs":{args}}}"#
        ))
        .unwrap();
        let devices =
            crate::devices::fake::FakeDevices(Ok(vec![crate::devices::fake::camera("cam1")]));
        let o = rig.pcs.handle_object(1, &devices, &req).expect("handled");
        serde_json::from_str(&o.replies[0]).unwrap()
    }

    fn picture() -> media::VideoFrame {
        media::VideoFrame {
            width: 4,
            height: 2,
            y: vec![0; 8],
            u: vec![0; 2],
            v: vec![0; 2],
        }
    }

    fn remote_track(rig: &Rig, id: u64) -> Arc<dyn media::Track> {
        rig.pcs.remote_tracks[&id].clone()
    }

    #[test]
    fn a_remote_video_track_is_shown_in_the_elements_window() {
        let (mut rig, surface) = video_rig();
        let offer = r#"[{"type":"offer","sdp":"v=0\r\nm=audio 9 X 111\r\na=mid:0\r\nm=video 9 X 96\r\na=mid:1\r\n"}]"#;
        let (r, _) = rig.call(&rpc(3, 2, "setRemoteDescription", offer));
        let tracks: Vec<&Value> = r.iter().filter(|m| m["rpcEventName"] == "track").collect();
        assert_eq!(tracks.len(), 2);
        let stream = tracks[0]["rpcEventArgs"]["stream"]["rpcObjectId"]
            .as_u64()
            .unwrap();
        let video = tracks[1]["rpcEventArgs"]["track"]["rpcObjectId"]
            .as_u64()
            .unwrap();
        assert_eq!(rig.pcs.remote_streams[&stream].len(), 2);

        assert_eq!(
            element_call(
                &mut rig,
                "MediaElement",
                10,
                "createMediaElement",
                r#"["video",""]"#
            )["hr"],
            json!(0)
        );
        let attach =
            format!(r#"["srcObject",{{"rpcObjectType":"MediaStream","rpcObjectId":{stream}}}]"#);
        assert_eq!(
            element_call(&mut rig, "MediaElement", 10, "setAttribute", &attach)["hr"],
            json!(0)
        );
        // The decoder delivers a picture and it lands in window 10.
        let track = remote_track(&rig, video);
        let fake: &media::fake::FakeTrack = track.as_any().downcast_ref().unwrap();
        assert!(fake.has_sink());
        fake.deliver(&picture());
        assert_eq!(*surface.frames.lock().unwrap(), [(10, 4, 2)]);
        assert!(matches!(
            &surface.controls.lock().unwrap()[0],
            videoipc::Message::Title { window: 10, .. }
        ));

        // Mirror, fit and visibility reach the window; a bad fit is refused.
        element_call(
            &mut rig,
            "MediaElement",
            10,
            "notifyTransformChanged",
            r#"["scaleX(-1)"]"#,
        );
        element_call(
            &mut rig,
            "MediaElement",
            10,
            "notifyObjectFitChanged",
            r#"["cover"]"#,
        );
        element_call(
            &mut rig,
            "MediaElement",
            10,
            "notifyVisibilityChanged",
            "[false]",
        );
        let bad = element_call(
            &mut rig,
            "MediaElement",
            10,
            "notifyObjectFitChanged",
            r#"["bogus"]"#,
        );
        assert_eq!(
            bad["errorDetails"],
            json!("unsupported value for objectFit")
        );
        let controls = surface.controls.lock().unwrap().clone();
        assert!(controls.contains(&videoipc::Message::Mirror {
            window: 10,
            mirrored: true
        }));
        assert!(controls.contains(&videoipc::Message::Fit {
            window: 10,
            fit: videoipc::Fit::Cover
        }));
        assert!(controls.contains(&videoipc::Message::Visible {
            window: 10,
            visible: false
        }));
        drop(controls);

        // Shutting the element down stops the picture and closes the window.
        element_call(&mut rig, "MediaElement", 10, "shutdown", "[]");
        assert!(!fake.has_sink());
        assert!(
            surface
                .controls
                .lock()
                .unwrap()
                .contains(&videoipc::Message::Close { window: 10 })
        );
    }

    #[test]
    fn a_video_track_that_arrives_after_the_element_is_attached_is_picked_up() {
        let (mut rig, surface) = video_rig();
        let audio = r#"[{"type":"offer","sdp":"v=0\r\nm=audio 9 X 111\r\na=mid:0\r\n"}]"#;
        let (r, _) = rig.call(&rpc(3, 2, "setRemoteDescription", audio));
        let stream = r.iter().find(|m| m["rpcEventName"] == "track").unwrap()["rpcEventArgs"]["stream"]["rpcObjectId"].as_u64().unwrap();
        element_call(
            &mut rig,
            "MediaElement",
            10,
            "createMediaElement",
            r#"["video",""]"#,
        );
        let attach =
            format!(r#"["srcObject",{{"rpcObjectType":"MediaStream","rpcObjectId":{stream}}}]"#);
        element_call(&mut rig, "MediaElement", 10, "setAttribute", &attach);
        assert!(rig.pcs.elements[&10].bound.is_none(), "no video yet");
        let video = r#"[{"type":"offer","sdp":"v=0\r\nm=video 9 X 96\r\na=mid:1\r\n"}]"#;
        rig.call(&rpc(3, 3, "setRemoteDescription", video));
        assert!(
            rig.pcs.elements[&10].bound.is_some(),
            "the video track was not attached when it arrived"
        );
        let bound = rig.pcs.elements[&10].bound.clone().unwrap();
        let fake: &media::fake::FakeTrack = bound.as_any().downcast_ref().unwrap();
        fake.deliver(&picture());
        assert_eq!(surface.frames.lock().unwrap().len(), 1);
    }

    /// What the Teams device settings page does when a camera is picked (2026-11-01): it asks for
    /// the camera twice, then builds an empty stream whose video track is the camera, and shows
    /// that stream.
    #[test]
    fn the_settings_preview_wraps_the_camera_it_opened() {
        let (mut rig, surface) = video_rig();
        let video = r#"[{"id":"s","constraints":{"video":true}}]"#;
        for (stream, track) in [(82, 83), (84, 85)] {
            let c = element_call(&mut rig, "MediaStream", stream, "createMediaStream", video);
            assert_eq!(c["hr"], json!(0));
            let t = format!(
                r#"[{{"mediaStreamRpcObjectId":{stream},"id":"t","kind":"video","label":"Cam"}}]"#
            );
            let c = element_call(
                &mut rig,
                "MediaStreamTrack",
                track,
                "createMediaStreamTrack",
                &t,
            );
            assert_eq!(c["hr"], json!(0));
        }
        element_call(
            &mut rig,
            "MediaElement",
            81,
            "createMediaElement",
            r#"["video",""]"#,
        );
        let empty = element_call(
            &mut rig,
            "MediaStream",
            86,
            "createMediaStream",
            r#"[{"id":"e"}]"#,
        );
        assert_eq!(empty["hr"], json!(0));
        let wrap = r#"[{"mediaStreamRpcObjectId":86,"id":"w","kind":"video","label":"Cam"}]"#;
        let c = element_call(
            &mut rig,
            "MediaStreamTrack",
            87,
            "createMediaStreamTrack",
            wrap,
        );
        assert_eq!(c["hr"], json!(0), "{c}");
        let attach = r#"["srcObject",{"rpcObjectType":"MediaStream","rpcObjectId":86}]"#;
        element_call(&mut rig, "MediaElement", 81, "setAttribute", attach);
        // The newest camera is the one shown.
        let shown = rig.pcs.local_streams[&84].tracks[0].clone();
        let fake: &media::fake::FakeTrack = shown.as_any().downcast_ref().unwrap();
        fake.deliver(&picture());
        assert_eq!(*surface.frames.lock().unwrap(), [(81, 4, 2)]);
        // With nothing to wrap, the request still fails.
        element_call(
            &mut rig,
            "MediaStream",
            90,
            "createMediaStream",
            r#"[{"id":"x"}]"#,
        );
        let none = r#"[{"mediaStreamRpcObjectId":90,"id":"n","kind":"audio","label":"Mic"}]"#;
        let c = element_call(
            &mut rig,
            "MediaStreamTrack",
            91,
            "createMediaStreamTrack",
            none,
        );
        assert_ne!(c["hr"], json!(0));
    }

    #[test]
    fn the_local_camera_is_shown_in_a_self_view_and_audio_elements_need_no_stream() {
        let (mut rig, surface) = video_rig();
        let created = element_call(
            &mut rig,
            "MediaStream",
            34,
            "createMediaStream",
            r#"[{"id":"s","constraints":{"video":true}}]"#,
        );
        assert_eq!(created["hr"], json!(0));
        element_call(
            &mut rig,
            "MediaElement",
            11,
            "createMediaElement",
            r#"["video",""]"#,
        );
        let attach = r#"["srcObject",{"rpcObjectType":"MediaStream","rpcObjectId":34}]"#;
        assert_eq!(
            element_call(&mut rig, "MediaElement", 11, "setAttribute", attach)["hr"],
            json!(0)
        );
        let cam = rig.pcs.local_streams[&34].tracks[0].clone();
        let fake: &media::fake::FakeTrack = cam.as_any().downcast_ref().unwrap();
        fake.deliver(&picture());
        assert_eq!(*surface.frames.lock().unwrap(), [(11, 4, 2)]);
        // Detaching (srcObject null) stops the picture.
        element_call(
            &mut rig,
            "MediaElement",
            11,
            "setAttribute",
            r#"["srcObject",null]"#,
        );
        assert!(!fake.has_sink());
        // A video element needs the stream to exist; an audio element does not.
        let missing = r#"["srcObject",{"rpcObjectType":"MediaStream","rpcObjectId":999}]"#;
        let r = element_call(&mut rig, "MediaElement", 11, "setAttribute", missing);
        assert_eq!(r["errorDetails"], json!("MediaStream not found"));
        element_call(
            &mut rig,
            "MediaElement",
            12,
            "createMediaElement",
            r#"["audio",""]"#,
        );
        assert_eq!(
            element_call(&mut rig, "MediaElement", 12, "setAttribute", missing)["hr"],
            json!(0)
        );
        // A closed connection takes its windows with it.
        rig.pcs.conn_closed(1);
        assert!(
            surface
                .controls
                .lock()
                .unwrap()
                .contains(&videoipc::Message::Close { window: 11 })
        );
    }
}
