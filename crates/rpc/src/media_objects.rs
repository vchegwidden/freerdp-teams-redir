//! The local media objects the host creates around a call, and the calls on transceivers and
//! senders it made or negotiated (`teams-redir-call-trace` of 2026-10-31 for the order Teams uses):
//!
//! - `MediaStream.createMediaStream`: `rpcObjectId` is the stream, `rpcArgs[0]` is `{id, constraints}`.
//!   `constraints.audio.deviceId` names a microphone (the same ids `enumerateDevices` returned), which
//!   is captured. No constraints make a stream that only holds tracks.
//! - `MediaStreamTrack.createMediaStreamTrack`: `{mediaStreamRpcObjectId, id, kind, label}`; the
//!   track is the stream's capture track. `onEnabledAttributeChanged([bool])` mutes it, `stop` ends it.
//! - `MediaElement`: the host's stand-in for an `<audio>` tag. Remote audio plays through the
//!   engine's audio device module by itself, so these calls are bookkeeping, and `setSinkId` picks
//!   the output device.
//! - `RTCRtpTransceiver.setDirection`, `RTCRtpSender.replaceTrack`, `RTCDataChannel.close` and
//!   `send`. `send` carries `rpcArgs[0].data` as an array of byte values (seen 2026-10-31); a string
//!   is sent as text.
//!
//! Replies carry no `result` (`msrdc-reply-structures.md` §3d, §3e) [O].

use std::sync::Arc;

use media::{AudioDevice, Kind, Track, VideoConstraints};
use proto::envelope::{self, RpcRequest};
use proto::hresult;
use serde_json::Value;

use crate::devices::{DeviceKind, DeviceSource, resolve};
use crate::peerconnection::{
    Element, PeerConnections, failure, object_id, parse_direction, rtc_error,
};
use crate::redirector::Outcome;

pub const MEDIA_STREAM: &str = "MediaStream";
pub const MEDIA_STREAM_TRACK: &str = "MediaStreamTrack";
pub const MEDIA_ELEMENT: &str = "MediaElement";
pub const TRANSCEIVER: &str = "RTCRtpTransceiver";
pub const SENDER: &str = "RTCRtpSender";
pub const DATA_CHANNEL: &str = "RTCDataChannel";

/// True for the object types this module handles.
pub fn handles(object_type: &str) -> bool {
    matches!(
        object_type,
        MEDIA_STREAM | MEDIA_STREAM_TRACK | MEDIA_ELEMENT | TRANSCEIVER | SENDER | DATA_CHANNEL
    )
}

fn ok(req: &RpcRequest, note: impl Into<String>) -> Outcome {
    Outcome {
        replies: envelope::ok_empty(req).into_iter().collect(),
        note: Some(note.into()),
    }
}

impl PeerConnections {
    /// `None` for methods not handled here.
    pub fn handle_object(
        &mut self,
        conn: crate::host::ConnId,
        devices: &dyn DeviceSource,
        req: &RpcRequest,
    ) -> Option<Outcome> {
        Some(match (req.object_type.as_str(), req.name.as_str()) {
            (MEDIA_STREAM, "createMediaStream") => self.create_media_stream(conn, devices, req),
            (MEDIA_STREAM_TRACK, "createMediaStreamTrack") => self.create_track(req),
            (MEDIA_STREAM_TRACK, "onEnabledAttributeChanged") => self.track_enabled(req),
            (MEDIA_STREAM_TRACK, "stop") => self.track_stop(req),
            (MEDIA_ELEMENT, "createMediaElement") => self.create_element(conn, req),
            (MEDIA_ELEMENT, "shutdown") => self.shutdown_element(req),
            (MEDIA_ELEMENT, "setAttribute") => self.set_attribute(req),
            (MEDIA_ELEMENT, "notifyVisibilityChanged") => self.element_visibility(req),
            (MEDIA_ELEMENT, "notifyObjectFitChanged") => self.element_fit(req),
            (MEDIA_ELEMENT, "notifyTransformChanged") => self.element_transform(req),
            (MEDIA_ELEMENT, "play" | "pause") => self.element_call(req),
            (MEDIA_ELEMENT, "setSinkId") => self.set_sink_id(devices, req),
            (TRANSCEIVER, "setDirection") => self.set_direction(req),
            (TRANSCEIVER, "setCodecPreferences") => self.set_codec_preferences(req),
            (SENDER, "replaceTrack") => self.replace_track(req),
            (DATA_CHANNEL, "close") => self.close_channel(req),
            (DATA_CHANNEL, "send") => self.send_on_channel(req),
            _ => return None,
        })
    }

    fn create_media_stream(
        &mut self,
        conn: crate::host::ConnId,
        devices: &dyn DeviceSource,
        req: &RpcRequest,
    ) -> Outcome {
        let Some(id) = object_id(req) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found");
        };
        if self.local_streams.contains_key(&id) {
            return failure(
                req,
                hresult::ERROR_ALREADY_EXISTS,
                "MediaStream already exists",
            );
        }
        let Some(args) = req.args_array().first() else {
            return failure(req, hresult::E_INVALIDARG, "missing rpcArgs");
        };
        if args["id"].as_str().is_none() {
            return failure(req, hresult::E_INVALIDARG, "missing id");
        }
        let constraints = &args["constraints"];
        let mut tracks = Vec::new();
        let mut note = format!("MediaStream {id}");
        if let Some(audio) = constraints.get("audio").filter(|a| wanted(a)) {
            let device_id = device_id_of(audio);
            let Some(device) = devices
                .list()
                .ok()
                .and_then(|list| resolve(&list, DeviceKind::AudioInput, &device_id))
            else {
                return failure(
                    req,
                    hresult::ERROR_NOT_FOUND,
                    &format!("no microphone \"{device_id}\""),
                );
            };
            let device = AudioDevice {
                id: device.id,
                label: device.label,
            };
            match self.engine.create_audio_capture(&device) {
                Ok(t) => {
                    note = format!("{note}: microphone \"{}\"", device.label);
                    tracks.push(t);
                }
                Err(e) => return rtc_error(req, &e),
            }
        }
        if let Some(video) = constraints.get("video").filter(|v| wanted(v)) {
            let (source, wish) = video_request(video);
            // Teams asks for the screen the same way, with a source id such as `display`.
            if matches!(source.as_str(), "display" | "screen" | "window") {
                tracks.iter().for_each(|t| t.stop());
                return failure(req, hresult::E_NOTIMPL, "screen capture is not supported");
            }
            let Some(camera) = devices
                .list()
                .ok()
                .and_then(|list| resolve(&list, DeviceKind::VideoInput, &source))
            else {
                tracks.iter().for_each(|t| t.stop());
                return failure(
                    req,
                    hresult::ERROR_NOT_FOUND,
                    &format!("no camera \"{source}\""),
                );
            };
            match self.engine.create_video_capture(&camera.id, wish) {
                Ok(t) => {
                    note = format!("{note}: camera \"{}\" {wish:?}", camera.label);
                    tracks.push(t);
                }
                Err(e) => {
                    tracks.iter().for_each(|t| t.stop());
                    return rtc_error(req, &e);
                }
            }
        }
        self.local_streams
            .insert(id, crate::peerconnection::LocalStream { conn, tracks });
        ok(req, note)
    }

    fn create_track(&mut self, req: &RpcRequest) -> Outcome {
        let Some(id) = object_id(req) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found");
        };
        if self.local_tracks.contains_key(&id) {
            return failure(
                req,
                hresult::ERROR_ALREADY_EXISTS,
                "MediaStreamTrack already exists",
            );
        }
        let Some(args) = req.args_array().first() else {
            return failure(req, hresult::E_INVALIDARG, "missing rpcArgs");
        };
        let Some(stream_id) = args["mediaStreamRpcObjectId"].as_u64() else {
            return failure(req, hresult::ERROR_NOT_FOUND, "MediaStream not found");
        };
        let Some(stream) = self.local_streams.get(&stream_id) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "MediaStream not found");
        };
        // `kind` says which one when the stream has both a microphone and a camera.
        let kind = match args["kind"].as_str() {
            Some("audio") => Some(Kind::Audio),
            Some("video") => Some(Kind::Video),
            _ => None,
        };
        let own = stream
            .tracks
            .iter()
            .find(|t| kind.is_none_or(|k| t.kind() == k))
            .cloned();
        let track = match own {
            Some(t) => t,
            None => {
                // A stream Teams builds for a preview holds no capture of its own: its track is
                // the camera (or microphone) Teams opened last.
                let donor = kind.and_then(|k| {
                    self.local_streams
                        .iter()
                        .filter(|(sid, _)| **sid != stream_id)
                        .filter_map(|(sid, s)| {
                            s.tracks.iter().find(|t| t.kind() == k).map(|t| (*sid, t))
                        })
                        .max_by_key(|(sid, _)| *sid)
                        .map(|(_, t)| t.clone())
                });
                let Some(track) = donor else {
                    return failure(
                        req,
                        hresult::E_NOTIMPL,
                        "the stream has no local track of that kind to wrap",
                    );
                };
                if let Some(s) = self.local_streams.get_mut(&stream_id) {
                    s.tracks.push(track.clone());
                }
                self.refresh_elements(stream_id);
                track
            }
        };
        self.local_tracks
            .insert(id, crate::peerconnection::LocalTrack { track });
        ok(req, format!("MediaStreamTrack {id} of stream {stream_id}"))
    }

    fn local_track(&self, req: &RpcRequest) -> Result<(u64, std::sync::Arc<dyn Track>), Outcome> {
        let id = object_id(req)
            .ok_or_else(|| failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found"))?;
        match self.local_tracks.get(&id) {
            Some(t) => Ok((id, t.track.clone())),
            None => Err(failure(req, hresult::ERROR_NOT_FOUND, "Track not found")),
        }
    }

    fn track_enabled(&mut self, req: &RpcRequest) -> Outcome {
        let (id, track) = match self.local_track(req) {
            Ok(t) => t,
            Err(o) => return o,
        };
        let Some(enabled) = req.args_array().first().and_then(Value::as_bool) else {
            return failure(
                req,
                hresult::E_INVALIDARG,
                "failed to change track enabled state",
            );
        };
        track.set_enabled(enabled);
        ok(req, format!("MediaStreamTrack {id} enabled={enabled}"))
    }

    /// Teams stops a track more than once; the second call is not an error.
    fn track_stop(&mut self, req: &RpcRequest) -> Outcome {
        let (id, track) = match self.local_track(req) {
            Ok(t) => t,
            Err(o) => return o,
        };
        track.stop();
        ok(req, format!("MediaStreamTrack {id} stopped"))
    }

    fn create_element(&mut self, conn: crate::host::ConnId, req: &RpcRequest) -> Outcome {
        let Some(id) = object_id(req) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found");
        };
        if self.elements.contains_key(&id) {
            return failure(
                req,
                hresult::ERROR_ALREADY_EXISTS,
                "MediaElement already exists",
            );
        }
        // `["audio", ""]` or `["video", ""]`.
        let kind = match req.args_array().first().and_then(Value::as_str) {
            Some("video") => Kind::Video,
            _ => Kind::Audio,
        };
        self.elements.insert(
            id,
            Element {
                conn,
                kind,
                source: None,
                bound: None,
            },
        );
        ok(req, format!("MediaElement {id} {}", req.args))
    }

    fn element(&self, req: &RpcRequest) -> Result<u64, Outcome> {
        let id = object_id(req)
            .ok_or_else(|| failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found"))?;
        if self.elements.contains_key(&id) {
            Ok(id)
        } else {
            Err(failure(
                req,
                hresult::ERROR_NOT_FOUND,
                "MediaElement not found",
            ))
        }
    }

    /// Forgets an element: its picture stops and its window closes.
    pub(crate) fn drop_element(&mut self, id: u64) {
        self.unbind(id);
        if let Some(e) = self.elements.remove(&id)
            && e.kind == Kind::Video
        {
            self.surface
                .control(videoipc::Message::Close { window: id as u32 });
        }
    }

    fn shutdown_element(&mut self, req: &RpcRequest) -> Outcome {
        match self.element(req) {
            Ok(id) => {
                self.drop_element(id);
                ok(req, format!("MediaElement {id} shut down"))
            }
            Err(o) => o,
        }
    }

    /// Detaches an element's picture; the window stays, showing its last frame, until it closes.
    fn unbind(&mut self, id: u64) {
        if let Some(e) = self.elements.get_mut(&id) {
            e.source = None;
            if let Some(track) = e.bound.take() {
                track.set_video_sink(None);
            }
        }
    }

    /// `setAttribute(name, value)`. Only `srcObject` does anything: `{rpcObjectType: "MediaStream",
    /// rpcObjectId}` for a stream, or null to detach. The others (`muted`, `autoplay`, `loop`,
    /// `data-vdi_videoid`) are logged.
    fn set_attribute(&mut self, req: &RpcRequest) -> Outcome {
        let id = match self.element(req) {
            Ok(id) => id,
            Err(o) => return o,
        };
        let args = req.args_array();
        if args.first().and_then(Value::as_str) != Some("srcObject") {
            return ok(req, format!("MediaElement {id} setAttribute {}", req.args));
        }
        self.unbind(id);
        let Some(source) = args.get(1).filter(|v| v.is_object()) else {
            return ok(req, format!("MediaElement {id} srcObject cleared"));
        };
        let (Some("MediaStream"), Some(stream)) = (
            source["rpcObjectType"].as_str(),
            source["rpcObjectId"].as_u64(),
        ) else {
            return failure(
                req,
                hresult::E_INVALIDARG,
                "expecting rpcObjectType=MediaStream",
            );
        };
        // Audio plays through the engine whatever is attached, so only a picture needs the stream.
        let known =
            self.local_streams.contains_key(&stream) || self.remote_streams.contains_key(&stream);
        if !known && self.is_video(id) {
            return failure(req, hresult::ERROR_NOT_FOUND, "MediaStream not found");
        }
        if let Some(e) = self.elements.get_mut(&id) {
            e.source = Some(stream);
        }
        let note = self.bind_video(id);
        ok(
            req,
            format!("MediaElement {id} srcObject = stream {stream}{note}"),
        )
    }

    /// Shows the video track of the element's stream, if it has one and none is showing. A
    /// remote stream can get its video track after the element was attached, so this also runs
    /// when a track appears.
    fn bind_video(&mut self, id: u64) -> String {
        let Some(e) = self.elements.get(&id) else {
            return String::new();
        };
        let (Some(stream), Kind::Video, None) = (e.source, e.kind, &e.bound) else {
            return String::new();
        };
        let (tracks, local): (Vec<Arc<dyn Track>>, bool) = match self.local_streams.get(&stream) {
            Some(s) => (s.tracks.clone(), true),
            None => (
                self.remote_streams
                    .get(&stream)
                    .into_iter()
                    .flatten()
                    .filter_map(|t| self.remote_tracks.get(t).cloned())
                    .collect(),
                false,
            ),
        };
        let Some(track) = tracks.into_iter().find(|t| t.kind() == Kind::Video) else {
            return ", no video track yet".into();
        };
        let window = id as u32;
        let surface = self.surface.clone();
        track.set_video_sink(Some(Arc::new(move |f| surface.frame(window, f))));
        self.surface.control(videoipc::Message::Title {
            window,
            title: if local {
                "Teams: your camera".into()
            } else {
                "Teams call video".into()
            },
        });
        if let Some(e) = self.elements.get_mut(&id) {
            e.bound = Some(track);
        }
        ", showing its video".into()
    }

    /// A video track arrived for `stream`: elements that were waiting for one can show it now.
    pub(crate) fn refresh_elements(&mut self, stream: u64) {
        let waiting: Vec<u64> = self
            .elements
            .iter()
            .filter(|(_, e)| e.source == Some(stream) && e.bound.is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in waiting {
            self.bind_video(id);
        }
    }

    /// `notifyVisibilityChanged`: whether the video is on screen in the host's window. The
    /// argument is a boolean, or an object or string that says so; anything else is ignored.
    fn element_visibility(&mut self, req: &RpcRequest) -> Outcome {
        let id = match self.element(req) {
            Ok(id) => id,
            Err(o) => return o,
        };
        let first = req.args_array().first().unwrap_or(&req.args);
        let visible = match first {
            Value::Bool(b) => Some(*b),
            Value::String(s) => match s.as_str() {
                "visible" | "true" => Some(true),
                "hidden" | "false" => Some(false),
                _ => None,
            },
            Value::Object(o) => ["visible", "isVisible"]
                .iter()
                .find_map(|k| o.get(*k).and_then(Value::as_bool)),
            _ => None,
        };
        if let (Some(visible), true) = (visible, self.is_video(id)) {
            self.surface.control(videoipc::Message::Visible {
                window: id as u32,
                visible,
            });
        }
        ok(
            req,
            format!("MediaElement {id} visibility {} -> {visible:?}", req.args),
        )
    }

    /// `notifyObjectFitChanged`: `fill`, `contain`, `cover` or `scale-down`.
    fn element_fit(&mut self, req: &RpcRequest) -> Outcome {
        let id = match self.element(req) {
            Ok(id) => id,
            Err(o) => return o,
        };
        let name = req
            .args_array()
            .first()
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(fit) = videoipc::Fit::from_css(name) else {
            return failure(
                req,
                hresult::E_INVALIDARG,
                "unsupported value for objectFit",
            );
        };
        if self.is_video(id) {
            self.surface.control(videoipc::Message::Fit {
                window: id as u32,
                fit,
            });
        }
        ok(req, format!("MediaElement {id} object-fit {name}"))
    }

    /// `notifyTransformChanged`: a CSS transform string. Only the mirror (`scaleX(-1)`) is used.
    fn element_transform(&mut self, req: &RpcRequest) -> Outcome {
        let id = match self.element(req) {
            Ok(id) => id,
            Err(o) => return o,
        };
        let transform = req
            .args_array()
            .first()
            .and_then(Value::as_str)
            .unwrap_or("");
        let mirrored = transform.replace(' ', "").contains("scaleX(-1)");
        if self.is_video(id) {
            self.surface.control(videoipc::Message::Mirror {
                window: id as u32,
                mirrored,
            });
        }
        ok(req, format!("MediaElement {id} transform {transform:?}"))
    }

    fn is_video(&self, id: u64) -> bool {
        self.elements
            .get(&id)
            .is_some_and(|e| e.kind == Kind::Video)
    }

    /// `play` and `pause` need nothing: a picture shows as soon as it is bound.
    fn element_call(&mut self, req: &RpcRequest) -> Outcome {
        match self.element(req) {
            Ok(id) => ok(req, format!("MediaElement {id} {} {}", req.name, req.args)),
            Err(o) => o,
        }
    }

    /// The argument layout was never seen, so the output device is the first string in the
    /// arguments that names one; anything else is logged and ignored.
    fn set_sink_id(&mut self, devices: &dyn DeviceSource, req: &RpcRequest) -> Outcome {
        let id = match self.element(req) {
            Ok(id) => id,
            Err(o) => return o,
        };
        let list = devices.list().unwrap_or_default();
        let device = strings(&req.args).into_iter().find_map(|s| {
            list.iter()
                .find(|d| d.kind == DeviceKind::AudioOutput && d.id == s)
        });
        let Some(device) = device else {
            return ok(
                req,
                format!(
                    "MediaElement {id} setSinkId {}: not an output device, ignored",
                    req.args
                ),
            );
        };
        let target = AudioDevice {
            id: device.id.clone(),
            label: device.label.clone(),
        };
        match self.engine.set_playout_device(&target) {
            Ok(()) => ok(
                req,
                format!("MediaElement {id} plays on \"{}\"", target.label),
            ),
            Err(e) => rtc_error(req, &e),
        }
    }

    fn set_direction(&mut self, req: &RpcRequest) -> Outcome {
        let Some(id) = object_id(req) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found");
        };
        let Some(at) = self.transceiver_ids.get(&id).copied() else {
            return failure(req, hresult::ERROR_NOT_FOUND, "Transceiver not found");
        };
        let direction = req
            .args_array()
            .first()
            .and_then(|a| a["direction"].as_str())
            .and_then(parse_direction);
        let handle = self
            .pcs
            .get(&at.pc)
            .and_then(|pc| pc.transceivers.get(at.index))
            .map(|r| r.handle.clone());
        let (Some(direction), Some(handle)) = (direction, handle) else {
            return failure(
                req,
                hresult::E_INVALIDARG,
                "failed to set direction on transceiver",
            );
        };
        match handle.set_direction(direction) {
            Ok(()) => ok(req, format!("transceiver {id} {}", direction.as_str())),
            Err(_) => failure(
                req,
                hresult::E_FAIL,
                "failed to set direction on transceiver",
            ),
        }
    }

    /// `rpcArgs[0].codecs`: `{mimeType, clockRate, sdpFmtpLine, channels}` entries, taken from the
    /// capabilities this client reported.
    fn set_codec_preferences(&mut self, req: &RpcRequest) -> Outcome {
        let Some(id) = object_id(req) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found");
        };
        let Some(at) = self.transceiver_ids.get(&id).copied() else {
            return failure(req, hresult::ERROR_NOT_FOUND, "Transceiver not found");
        };
        let codecs: Vec<media::CodecCapability> = req
            .args_array()
            .first()
            .and_then(|a| a["codecs"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|c| {
                Some(media::CodecCapability {
                    mime_type: c["mimeType"].as_str()?.to_owned(),
                    clock_rate: c["clockRate"].as_u64(),
                    channels: c["channels"].as_u64().map(|n| n as u16),
                    sdp_fmtp_line: c["sdpFmtpLine"].as_str().map(str::to_owned),
                })
            })
            .collect();
        let Some(handle) = self
            .pcs
            .get(&at.pc)
            .and_then(|pc| pc.transceivers.get(at.index))
            .map(|r| r.handle.clone())
        else {
            return failure(req, hresult::ERROR_NOT_FOUND, "Transceiver not found");
        };
        let count = codecs.len();
        match handle.set_codec_preferences(codecs) {
            Ok(()) => ok(req, format!("transceiver {id}: {count} codec preferences")),
            Err(e) => rtc_error(req, &e),
        }
    }

    fn replace_track(&mut self, req: &RpcRequest) -> Outcome {
        let Some(id) = object_id(req) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found");
        };
        let Some(at) = self.sender_ids.get(&id).copied() else {
            return failure(req, hresult::ERROR_NOT_FOUND, "Sender not found");
        };
        let track = match req
            .args_array()
            .first()
            .and_then(|a| a["trackRpcObjectId"].as_u64())
            .filter(|n| *n != 0)
        {
            None => None,
            Some(n) => match self.local_tracks.get(&n) {
                Some(t) => Some(t.track.clone()),
                None => return failure(req, hresult::ERROR_NOT_FOUND, "Track not found"),
            },
        };
        let Some(handle) = self
            .pcs
            .get(&at.pc)
            .and_then(|pc| pc.transceivers.get(at.index))
            .map(|r| r.handle.clone())
        else {
            return failure(req, hresult::ERROR_NOT_FOUND, "Sender not found");
        };
        let note = match &track {
            Some(t) => format!("sender {id} now sends {}", t.id()),
            None => format!("sender {id} cleared"),
        };
        match handle.set_sender_track(track) {
            Ok(()) => ok(req, note),
            Err(e) => rtc_error(req, &e),
        }
    }

    fn send_on_channel(&mut self, req: &RpcRequest) -> Outcome {
        let Some(id) = object_id(req) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found");
        };
        let Some(channel) = self.channels.get(&id).map(|c| c.channel.clone()) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "data channel not found");
        };
        let data = req.args_array().first().map(|a| &a["data"]);
        let (bytes, binary) = match data {
            Some(Value::Array(items)) => {
                let bytes: Option<Vec<u8>> = items
                    .iter()
                    .map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
                    .collect();
                match bytes {
                    Some(b) => (b, true),
                    None => {
                        return failure(
                            req,
                            hresult::E_INVALIDARG,
                            "No data found in data channel send method",
                        );
                    }
                }
            }
            Some(Value::String(text)) => (text.clone().into_bytes(), false),
            _ => {
                return failure(
                    req,
                    hresult::E_INVALIDARG,
                    "No data found in data channel send method",
                );
            }
        };
        match channel.send(&bytes, binary) {
            Ok(()) => ok(
                req,
                format!("RTCDataChannel {id}: sent {} bytes", bytes.len()),
            ),
            Err(e) => rtc_error(req, &e),
        }
    }

    fn close_channel(&mut self, req: &RpcRequest) -> Outcome {
        let Some(id) = object_id(req) else {
            return failure(req, hresult::ERROR_NOT_FOUND, "rpcObjectId field not found");
        };
        match self.channels.get(&id) {
            Some(c) => {
                c.channel.close();
                ok(req, format!("RTCDataChannel {id} closed"))
            }
            None => failure(req, hresult::ERROR_NOT_FOUND, "data channel not found"),
        }
    }
}

/// A constraint that asks for something: not absent, `null` or `false`.
fn wanted(v: &Value) -> bool {
    !v.is_null() && *v != Value::Bool(false)
}

/// The device a constraint names, as the string it is in `deviceId` / `sourceId`, or in
/// `{exact: ..}` / `{ideal: ..}` form. Empty for none.
fn id_in(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => ["exact", "ideal"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_str))
            .unwrap_or("")
            .to_owned(),
        _ => String::new(),
    }
}

fn device_id_of(constraint: &Value) -> String {
    ["deviceId", "sourceId"]
        .iter()
        .map(|k| id_in(&constraint[*k]))
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

/// A number in a constraint: a plain number, or `{exact | ideal | max | min}`, in that order of
/// preference.
fn number_in(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Object(o) => ["exact", "ideal", "max", "min"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_f64)),
        _ => None,
    }
}

/// The camera and mode a `video` constraint asks for. Two shapes seen in the Windows add-in
/// (`teams-optimization-windows-rpc-handlers.md`): `video.mandatory.{minWidth, maxWidth, minHeight,
/// maxHeight, maxFrameRate, sourceId}` and `video.{deviceId, width, height, frameRate}`.
fn video_request(video: &Value) -> (String, VideoConstraints) {
    let mandatory = &video["mandatory"];
    let source = [device_id_of(video), device_id_of(mandatory)]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let pick = |plain: &str, max: &str, min: &str| {
        number_in(&video[plain])
            .or_else(|| number_in(&mandatory[max]))
            .or_else(|| number_in(&mandatory[min]))
    };
    let wish = VideoConstraints {
        width: pick("width", "maxWidth", "minWidth").map(|n| n as u32),
        height: pick("height", "maxHeight", "minHeight").map(|n| n as u32),
        frame_rate: number_in(&video["frameRate"])
            .or_else(|| number_in(&mandatory["maxFrameRate"])),
    };
    (source, wish)
}

/// Every string in a JSON value, depth first.
fn strings(v: &Value) -> Vec<&str> {
    match v {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().flat_map(strings).collect(),
        Value::Object(o) => o.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use media::fake::{FakeEngine, FakeTrack};
    use serde_json::json;

    use super::*;
    use crate::devices::fake::*;
    use crate::peerconnection::PeerConnections;

    fn rig() -> (PeerConnections, FakeDevices) {
        let defer: crate::peerconnection::Defer = Arc::new(|_| ());
        (
            PeerConnections::new(
                Arc::new(FakeEngine::default()),
                defer,
                Arc::new(crate::video::NullSurface),
                true,
            ),
            FakeDevices(Ok(vec![mic("m", true), speaker("s", true), camera("cam1")])),
        )
    }

    fn call(
        rig: &mut (PeerConnections, FakeDevices),
        object: &str,
        id: u64,
        name: &str,
        args: &str,
    ) -> Value {
        let req = proto::envelope::parse_request(&format!(
            r#"{{"rpcCallId":1,"rpcObjectType":"{object}","rpcObjectId":{id},"rpcName":"{name}","rpcArgs":{args}}}"#
        ))
        .unwrap();
        let o = rig.0.handle_object(1, &rig.1, &req).expect("handled");
        serde_json::from_str(&o.replies[0]).unwrap()
    }

    fn fake(t: &Arc<dyn Track>) -> &FakeTrack {
        t.as_any().downcast_ref::<FakeTrack>().unwrap()
    }

    #[test]
    fn a_stream_with_a_microphone_and_a_camera_has_both_and_tracks_pick_by_kind() {
        let mut r = rig();
        let args = r#"[{"id":"s","constraints":{"audio":{"deviceId":"m"},"video":{"width":{"ideal":1280},"height":{"ideal":720},"frameRate":{"max":30}}}}]"#;
        assert_eq!(
            call(&mut r, "MediaStream", 34, "createMediaStream", args)["hr"],
            json!(0)
        );
        assert_eq!(r.0.local_streams[&34].tracks.len(), 2);
        let video = call(
            &mut r,
            "MediaStreamTrack",
            35,
            "createMediaStreamTrack",
            r#"[{"mediaStreamRpcObjectId":34,"id":"t","kind":"video"}]"#,
        );
        let audio = call(
            &mut r,
            "MediaStreamTrack",
            36,
            "createMediaStreamTrack",
            r#"[{"mediaStreamRpcObjectId":34,"id":"u","kind":"audio"}]"#,
        );
        assert_eq!(
            (video["hr"].clone(), audio["hr"].clone()),
            (json!(0), json!(0))
        );
        assert_eq!(r.0.local_tracks[&35].track.kind(), Kind::Video);
        assert_eq!(r.0.local_tracks[&36].track.kind(), Kind::Audio);
        // Disabling the video track reaches the camera, and stopping ends it.
        call(
            &mut r,
            "MediaStreamTrack",
            35,
            "onEnabledAttributeChanged",
            "[false]",
        );
        assert!(!fake(&r.0.local_tracks[&35].track).enabled());
        assert!(fake(&r.0.local_tracks[&36].track).enabled());
        call(&mut r, "MediaStreamTrack", 35, "stop", "[]");
        assert!(fake(&r.0.local_tracks[&35].track).stopped());
    }

    #[test]
    fn video_requests_name_cameras_in_either_shape() {
        let mut r = rig();
        let a = call(
            &mut r,
            "MediaStream",
            1,
            "createMediaStream",
            r#"[{"id":"a","constraints":{"video":{"deviceId":{"exact":"cam1"}}}}]"#,
        );
        let b = call(
            &mut r,
            "MediaStream",
            2,
            "createMediaStream",
            r#"[{"id":"b","constraints":{"video":{"mandatory":{"sourceId":"cam1","maxWidth":640,"maxHeight":360,"maxFrameRate":15}}}}]"#,
        );
        let c = call(
            &mut r,
            "MediaStream",
            3,
            "createMediaStream",
            r#"[{"id":"c","constraints":{"video":true}}]"#,
        );
        assert_eq!(
            [a["hr"].clone(), b["hr"].clone(), c["hr"].clone()],
            [json!(0), json!(0), json!(0)]
        );
        let t = &r.0.local_streams[&1].tracks[0];
        assert_eq!(t.id(), "camera-cam1");
        let missing = call(
            &mut r,
            "MediaStream",
            4,
            "createMediaStream",
            r#"[{"id":"d","constraints":{"video":{"deviceId":"nope"}}}]"#,
        );
        assert_eq!(missing["errorDetails"], json!("no camera \"nope\""));
        assert_eq!(missing["hr"], json!(hresult::ERROR_NOT_FOUND as i32));
        // The screen is asked for like a camera with a source id of `display`.
        let screen = call(
            &mut r,
            "MediaStream",
            5,
            "createMediaStream",
            r#"[{"id":"e","constraints":{"video":{"mandatory":{"sourceId":"display"}}}}]"#,
        );
        assert_eq!(
            screen["errorDetails"],
            json!("screen capture is not supported")
        );
        assert!(!r.0.local_streams.contains_key(&5));
    }

    #[test]
    fn constraint_parsing() {
        let (src, w) = video_request(
            &json!({"deviceId":{"ideal":"x"},"width":{"min":320,"ideal":1280},"height":720,"frameRate":{"max":24}}),
        );
        assert_eq!(src, "x");
        assert_eq!(
            (w.width, w.height, w.frame_rate),
            (Some(1280), Some(720), Some(24.0))
        );
        let (src, w) = video_request(
            &json!({"mandatory":{"sourceId":"y","minWidth":640,"maxWidth":1920,"maxFrameRate":30}}),
        );
        assert_eq!(src, "y");
        assert_eq!(
            (w.width, w.height, w.frame_rate),
            (Some(1920), None, Some(30.0))
        );
        let (src, w) = video_request(&json!(true));
        assert_eq!(src, "");
        assert_eq!(w, VideoConstraints::default());
        assert!(
            wanted(&json!({}))
                && wanted(&json!(true))
                && !wanted(&json!(false))
                && !wanted(&Value::Null)
        );
    }

    #[test]
    fn codec_preferences_reach_the_transceiver() {
        let mut r = rig();
        let pc = proto::envelope::parse_request(
            r#"{"rpcCallId":1,"rpcObjectType":"RTCPeerConnection","rpcObjectId":3,"rpcName":"createPeerConnection","rpcArgs":[{}]}"#,
        )
        .unwrap();
        r.0.handle(1, &pc).unwrap();
        let add = proto::envelope::parse_request(
            r#"{"rpcCallId":2,"rpcObjectType":"RTCPeerConnection","rpcObjectId":3,"rpcName":"addTransceiver","rpcArgs":[{"kind":"video","transceiverRpcObjectId":7,"senderRpcObjectId":8,"receiverRpcObjectId":9}]}"#,
        )
        .unwrap();
        r.0.handle(1, &add).unwrap();
        let ok = call(
            &mut r,
            "RTCRtpTransceiver",
            7,
            "setCodecPreferences",
            r#"[{"codecs":[{"mimeType":"video/H264","clockRate":90000,"sdpFmtpLine":"packetization-mode=1"},{"mimeType":"video/rtx","clockRate":90000}]}]"#,
        );
        assert_eq!(ok["hr"], json!(0));
        let bad = call(
            &mut r,
            "RTCRtpTransceiver",
            7,
            "setCodecPreferences",
            r#"[{"codecs":[{"mimeType":"audio/nonsense"}]}]"#,
        );
        assert_eq!(bad["hr"], json!(hresult::E_FAIL as i32));
        let unknown = call(
            &mut r,
            "RTCRtpTransceiver",
            99,
            "setCodecPreferences",
            r#"[{"codecs":[]}]"#,
        );
        assert_eq!(unknown["errorDetails"], json!("Transceiver not found"));
    }
}
