//! An engine with no network and no libwebrtc, for tests of the protocol code and the replay tool.
//! It negotiates nothing: offers list the transceivers it has, a remote offer creates one
//! transceiver per `m=` section (and reports it as a track), and every call completes at once on
//! the calling thread.

use std::any::Any;
use std::sync::{Arc, Mutex};

use crate::*;

#[derive(Default)]
pub struct FakeEngine {
    /// Makes every asynchronous call fail with `InvalidState`.
    pub failing: bool,
}

impl FakeEngine {
    pub fn failing() -> FakeEngine {
        FakeEngine { failing: true }
    }
}

fn opus() -> CodecCapability {
    CodecCapability {
        mime_type: "audio/opus".into(),
        clock_rate: Some(48000),
        channels: Some(2),
        sdp_fmtp_line: Some("minptime=10;useinbandfec=1".into()),
    }
}

fn audio_caps() -> RtpCapabilities {
    RtpCapabilities {
        codecs: vec![
            opus(),
            CodecCapability {
                mime_type: "audio/PCMU".into(),
                clock_rate: Some(8000),
                channels: Some(1),
                sdp_fmtp_line: None,
            },
        ],
        header_extensions: vec!["urn:ietf:params:rtp-hdrext:ssrc-audio-level".into()],
    }
}

impl MediaEngine for FakeEngine {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            send_audio: audio_caps(),
            recv_audio: audio_caps(),
            ..Capabilities::default()
        }
    }

    fn create_audio_capture(&self, device: &AudioDevice) -> Result<Arc<dyn Track>, EngineError> {
        if device.id.is_empty() || device.id == "missing" {
            return Err(EngineError::new(
                ErrorKind::InvalidParameter,
                "no such device",
            ));
        }
        Ok(Arc::new(FakeTrack {
            id: format!("capture-{}", device.id),
            kind: Kind::Audio,
            sink: Mutex::new(None),
            state: Mutex::new((true, false)),
        }))
    }

    fn create_video_capture(
        &self,
        device_id: &str,
        _constraints: VideoConstraints,
    ) -> Result<Arc<dyn Track>, EngineError> {
        if device_id == "missing" {
            return Err(EngineError::new(
                ErrorKind::InvalidParameter,
                "no such camera",
            ));
        }
        Ok(Arc::new(FakeTrack {
            id: format!("camera-{device_id}"),
            kind: Kind::Video,
            sink: Mutex::new(None),
            state: Mutex::new((true, false)),
        }))
    }

    fn set_playout_device(&self, device: &AudioDevice) -> Result<(), EngineError> {
        if device.id == "missing" {
            return Err(EngineError::new(
                ErrorKind::InvalidParameter,
                "no such device",
            ));
        }
        Ok(())
    }

    fn create_peer_connection(
        &self,
        _config: PcConfig,
        events: EventSink,
    ) -> Result<Box<dyn PeerConnection>, EngineError> {
        Ok(Box::new(FakePc {
            failing: self.failing,
            events,
            state: Mutex::new(PcState::default()),
        }))
    }
}

pub struct FakeTrack {
    id: String,
    kind: Kind,
    sink: Mutex<Option<VideoSink>>,
    /// `(enabled, stopped)`.
    state: Mutex<(bool, bool)>,
}

impl FakeTrack {
    pub fn enabled(&self) -> bool {
        self.state.lock().unwrap().0
    }

    pub fn stopped(&self) -> bool {
        self.state.lock().unwrap().1
    }

    /// A remote track as the fake reports it.
    pub fn remote(id: &str, kind: Kind) -> Arc<FakeTrack> {
        Arc::new(FakeTrack {
            id: id.to_owned(),
            kind,
            sink: Mutex::new(None),
            state: Mutex::new((true, false)),
        })
    }

    /// Whether a sink is attached.
    pub fn has_sink(&self) -> bool {
        self.sink.lock().unwrap().is_some()
    }

    /// Pushes a frame to the attached sink, as the decoder would.
    pub fn deliver(&self, frame: &VideoFrame) {
        let sink = self.sink.lock().unwrap().clone();
        if let Some(sink) = sink {
            sink(frame);
        }
    }
}

impl Track for FakeTrack {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn set_enabled(&self, enabled: bool) {
        self.state.lock().unwrap().0 = enabled;
    }

    fn stop(&self) {
        self.state.lock().unwrap().1 = true;
    }

    fn set_video_sink(&self, sink: Option<VideoSink>) {
        *self.sink.lock().unwrap() = sink;
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct FakeTransceiver {
    /// The mime types last given to `set_codec_preferences`.
    pub codecs: Mutex<Vec<String>>,
    info: Mutex<TransceiverInfo>,
    sender_track: Mutex<Option<String>>,
}

impl FakeTransceiver {
    fn new(mid: Option<String>, kind: Kind, direction: Direction) -> Arc<FakeTransceiver> {
        Arc::new(FakeTransceiver {
            codecs: Mutex::new(Vec::new()),
            info: Mutex::new(TransceiverInfo {
                mid,
                kind: Some(kind),
                direction,
            }),
            sender_track: Mutex::new(None),
        })
    }
}

impl Transceiver for FakeTransceiver {
    fn info(&self) -> TransceiverInfo {
        self.info.lock().unwrap().clone()
    }

    fn set_direction(&self, direction: Direction) -> Result<(), EngineError> {
        self.info.lock().unwrap().direction = direction;
        Ok(())
    }

    fn set_sender_track(&self, track: Option<Arc<dyn Track>>) -> Result<(), EngineError> {
        *self.sender_track.lock().unwrap() = track.map(|t| t.id());
        Ok(())
    }

    fn sender_track_id(&self) -> Option<String> {
        self.sender_track.lock().unwrap().clone()
    }

    fn set_codec_preferences(&self, codecs: Vec<CodecCapability>) -> Result<(), EngineError> {
        if codecs.iter().any(|c| c.mime_type == "audio/nonsense") {
            return Err(EngineError::new(
                ErrorKind::InvalidParameter,
                "unsupported codec",
            ));
        }
        *self.codecs.lock().unwrap() = codecs.iter().map(|c| c.mime_type.clone()).collect();
        Ok(())
    }

    fn remove_track(&self) -> Result<(), EngineError> {
        *self.sender_track.lock().unwrap() = None;
        self.info.lock().unwrap().direction = Direction::RecvOnly;
        Ok(())
    }
}

pub struct FakeDataChannel {
    sink: DataChannelSink,
    pub sent: Mutex<Vec<Vec<u8>>>,
}

impl FakeDataChannel {
    /// Delivers a message as if the remote peer sent it.
    pub fn receive(&self, data: &[u8]) {
        (self.sink)(DataChannelEvent::Message(data.to_vec()));
    }
}

impl DataChannel for FakeDataChannel {
    fn id(&self) -> i32 {
        1
    }

    fn send(&self, data: &[u8], _binary: bool) -> Result<(), EngineError> {
        self.sent.lock().unwrap().push(data.to_vec());
        Ok(())
    }

    fn close(&self) {
        (self.sink)(DataChannelEvent::Closing);
        (self.sink)(DataChannelEvent::Closed);
    }
}

#[derive(Default)]
struct PcState {
    transceivers: Vec<Arc<FakeTransceiver>>,
    local: Option<SessionDescription>,
    remote: Option<SessionDescription>,
    closed: bool,
}

struct FakePc {
    failing: bool,
    events: EventSink,
    state: Mutex<PcState>,
}

impl FakePc {
    fn failure(&self) -> Option<EngineError> {
        self.failing
            .then(|| EngineError::new(ErrorKind::InvalidState, "fake failure"))
    }

    fn sdp(&self, st: &PcState) -> String {
        let mut s = String::from("v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n");
        for t in &st.transceivers {
            let info = t.info();
            s.push_str(&format!(
                "m={} 9 UDP/TLS/RTP/SAVPF 111\r\n",
                info.kind.map_or("audio", Kind::as_str)
            ));
            if let Some(mid) = &info.mid {
                s.push_str(&format!("a=mid:{mid}\r\n"));
            }
            s.push_str(&format!("a={}\r\n", info.direction.as_str()));
        }
        s
    }
}

impl PeerConnection for FakePc {
    fn create_offer(&self, done: Done<SessionDescription>) {
        if let Some(e) = self.failure() {
            return done(Err(e));
        }
        let st = self.state.lock().unwrap();
        done(Ok(SessionDescription {
            kind: SdpType::Offer,
            sdp: self.sdp(&st),
        }));
    }

    fn create_answer(&self, done: Done<SessionDescription>) {
        let st = self.state.lock().unwrap();
        if self.failing || st.remote.is_none() {
            return done(Err(EngineError::new(
                ErrorKind::InvalidState,
                "no remote offer",
            )));
        }
        done(Ok(SessionDescription {
            kind: SdpType::Answer,
            sdp: self.sdp(&st),
        }));
    }

    fn set_local_description(&self, desc: SessionDescription, done: Done<()>) {
        if let Some(e) = self.failure() {
            return done(Err(e));
        }
        let mut st = self.state.lock().unwrap();
        // Setting an offer assigns the mids the fake would negotiate.
        for (i, t) in st.transceivers.iter().enumerate() {
            t.info
                .lock()
                .unwrap()
                .mid
                .get_or_insert_with(|| i.to_string());
        }
        st.local = Some(desc);
        drop(st);
        (self.events)(PcEvent::SignalingState(SignalingState::HaveLocalOffer));
        done(Ok(()));
    }

    fn set_remote_description(&self, desc: SessionDescription, done: Done<()>) {
        if let Some(e) = self.failure() {
            return done(Err(e));
        }
        let mut tracks = Vec::new();
        {
            let mut st = self.state.lock().unwrap();
            // Sections as (kind, mid, whether the remote sends), in the order of the `m=` lines.
            let mut sections: Vec<(Kind, Option<String>, bool)> = Vec::new();
            for line in desc.sdp.trim().lines() {
                let line = line.trim();
                if line.starts_with("m=audio") {
                    sections.push((Kind::Audio, None, true));
                } else if line.starts_with("m=video") {
                    sections.push((Kind::Video, None, true));
                } else if let Some(last) = sections.last_mut() {
                    if let Some(v) = line.strip_prefix("a=mid:") {
                        last.1 = Some(v.trim().to_owned());
                    } else if matches!(line, "a=recvonly" | "a=inactive") {
                        last.2 = false;
                    }
                }
            }
            if desc.kind == SdpType::Offer {
                for (kind, mid, remote_sends) in sections {
                    // A section the remote only receives makes a transceiver and no track.
                    let direction = if remote_sends {
                        Direction::SendRecv
                    } else {
                        Direction::RecvOnly
                    };
                    let t = FakeTransceiver::new(mid, kind, direction);
                    st.transceivers.push(t.clone());
                    if !remote_sends {
                        continue;
                    }
                    let track_id = format!("track{}", st.transceivers.len());
                    tracks.push(TrackInfo {
                        transceiver: t.info(),
                        track: Some(TrackHandle(FakeTrack::remote(&track_id, kind))),
                        track_id,
                        stream_ids: vec!["stream1".into()],
                    });
                }
            }
            st.remote = Some(desc);
        }
        for t in tracks {
            (self.events)(PcEvent::Track(t));
        }
        (self.events)(PcEvent::SignalingState(SignalingState::HaveRemoteOffer));
        done(Ok(()));
    }

    fn add_ice_candidate(&self, candidate: IceCandidateInit, done: Done<()>) {
        if candidate::parse(&candidate.candidate).is_none() {
            return done(Err(EngineError::new(
                ErrorKind::SdpParse,
                "SDP parse failure - line=[bad] - not a candidate",
            )));
        }
        done(Ok(()));
    }

    fn local_description(&self) -> Option<SessionDescription> {
        self.state.lock().unwrap().local.clone()
    }

    fn remote_description(&self) -> Option<SessionDescription> {
        self.state.lock().unwrap().remote.clone()
    }

    fn transceivers(&self) -> Vec<Arc<dyn Transceiver>> {
        let st = self.state.lock().unwrap();
        st.transceivers
            .iter()
            .map(|t| t.clone() as Arc<dyn Transceiver>)
            .collect()
    }

    fn add_transceiver(
        &self,
        kind: Kind,
        direction: Direction,
    ) -> Result<Arc<dyn Transceiver>, EngineError> {
        let t = FakeTransceiver::new(None, kind, direction);
        self.state.lock().unwrap().transceivers.push(t.clone());
        (self.events)(PcEvent::NegotiationNeeded);
        Ok(t)
    }

    fn create_data_channel(
        &self,
        label: &str,
        events: DataChannelSink,
    ) -> Result<Arc<dyn DataChannel>, EngineError> {
        if label.is_empty() {
            return Err(EngineError::new(ErrorKind::InvalidParameter, "no label"));
        }
        events(DataChannelEvent::Open);
        Ok(Arc::new(FakeDataChannel {
            sink: events,
            sent: Mutex::new(Vec::new()),
        }))
    }

    fn get_stats(&self, done: Done<String>) {
        if let Some(e) = self.failure() {
            return done(Err(e));
        }
        done(Ok(
            r#"[{"type":"codec","id":"RTCCodec_0","timestamp":1730300000000,"payloadType":111,"mimeType":"audio/opus","clockRate":48000}]"#
                .into(),
        ));
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        (self.events)(PcEvent::SignalingState(SignalingState::Closed));
    }
}

/// An [`EventSink`] that collects events, for tests.
pub fn collecting_sink() -> (EventSink, Arc<Mutex<Vec<PcEvent>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    (Arc::new(move |e| sink.lock().unwrap().push(e)), log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_lists_the_transceivers_and_a_remote_offer_makes_tracks() {
        let engine = FakeEngine::default();
        let (sink, log) = collecting_sink();
        let pc = engine
            .create_peer_connection(PcConfig::default(), sink)
            .unwrap();
        pc.add_transceiver(Kind::Audio, Direction::SendRecv)
            .unwrap();
        let got = Arc::new(Mutex::new(None));
        let g = got.clone();
        pc.create_offer(Box::new(move |r| *g.lock().unwrap() = Some(r)));
        let offer = got.lock().unwrap().take().unwrap().unwrap();
        assert!(offer.sdp.contains("m=audio"));

        pc.set_remote_description(
            SessionDescription {
                kind: SdpType::Offer,
                sdp: "v=0\r\nm=audio 9 X 111\r\na=mid:0\r\n".into(),
            },
            Box::new(|r| assert!(r.is_ok())),
        );
        let events = log.lock().unwrap();
        assert!(
            events.iter().any(
                |e| matches!(e, PcEvent::Track(t) if t.transceiver.mid.as_deref() == Some("0"))
            )
        );
    }

    #[test]
    fn a_sender_track_can_be_replaced() {
        let engine = FakeEngine::default();
        let (sink, _) = collecting_sink();
        let pc = engine
            .create_peer_connection(PcConfig::default(), sink)
            .unwrap();
        let t = pc
            .add_transceiver(Kind::Audio, Direction::Inactive)
            .unwrap();
        let mic = engine
            .create_audio_capture(&AudioDevice {
                id: "m".into(),
                label: "Mic".into(),
            })
            .unwrap();
        t.set_sender_track(Some(mic)).unwrap();
        assert_eq!(t.sender_track_id().as_deref(), Some("capture-m"));
        t.set_sender_track(None).unwrap();
        assert_eq!(t.sender_track_id(), None);
    }
}
