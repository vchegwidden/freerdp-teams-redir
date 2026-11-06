//! The media engine behind the redirected WebRTC calls (plan §4.1).
//!
//! The protocol code in `teams-redir-rpc` talks to these traits and never to libwebrtc, so it
//! can be tested with [`fake::FakeEngine`] and the engine can be swapped. [`lk::LkEngine`] is the
//! real one (feature `libwebrtc`): the same libwebrtc family the Windows add-in bundles, so
//! the SDP, the codec lists and the candidate lines look the way Teams expects.
//!
//! Everything asynchronous reports through callbacks that run on engine threads. Callers must
//! hand the result to their own thread and not block in a callback.

use std::fmt;
use std::sync::Arc;

pub mod candidate;
/// One decoded or captured video frame: I420, tightly packed.
pub use camera::Frame as VideoFrame;
pub mod fake;
#[cfg(feature = "libwebrtc")]
pub mod lk;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Audio,
    Video,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Audio => "audio",
            Kind::Video => "video",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecCapability {
    /// `audio/opus`.
    pub mime_type: String,
    pub clock_rate: Option<u64>,
    pub channels: Option<u16>,
    pub sdp_fmtp_line: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RtpCapabilities {
    pub codecs: Vec<CodecCapability>,
    /// URIs only: the host's reply has no ids or directions.
    pub header_extensions: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub send_audio: RtpCapabilities,
    pub send_video: RtpCapabilities,
    pub recv_audio: RtpCapabilities,
    pub recv_video: RtpCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpType {
    Offer,
    PrAnswer,
    Answer,
    Rollback,
}

impl SdpType {
    pub fn as_str(self) -> &'static str {
        match self {
            SdpType::Offer => "offer",
            SdpType::PrAnswer => "pranswer",
            SdpType::Answer => "answer",
            SdpType::Rollback => "rollback",
        }
    }

    pub fn parse(s: &str) -> Option<SdpType> {
        Some(match s {
            "offer" => SdpType::Offer,
            "pranswer" => SdpType::PrAnswer,
            "answer" => SdpType::Answer,
            "rollback" => SdpType::Rollback,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDescription {
    pub kind: SdpType,
    pub sdp: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PcConfig {
    pub ice_servers: Vec<IceServer>,
    /// `iceTransportPolicy: "relay"`.
    pub relay_only: bool,
}

/// What `addIceCandidate` carries, and what a gathered candidate is reported as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceCandidateInit {
    pub sdp_mid: String,
    pub sdp_mline_index: i32,
    /// The attribute line, `candidate:1 1 udp ...`.
    pub candidate: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
    Stopped,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::SendRecv => "sendrecv",
            Direction::SendOnly => "sendonly",
            Direction::RecvOnly => "recvonly",
            Direction::Inactive => "inactive",
            Direction::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransceiverInfo {
    /// `None` until negotiation assigns one.
    pub mid: Option<String>,
    pub kind: Option<Kind>,
    pub direction: Direction,
}

/// A shared handle to a [`Track`], comparable and printable so that events carrying one can be.
#[derive(Clone)]
pub struct TrackHandle(pub Arc<dyn Track>);

impl fmt::Debug for TrackHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TrackHandle({:?})", self.0.id())
    }
}

impl PartialEq for TrackHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for TrackHandle {}

/// A remote track that appeared (`track` event). `stream_ids` are the `msid` stream labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    pub transceiver: TransceiverInfo,
    pub track_id: String,
    pub stream_ids: Vec<String>,
    /// The track itself, to attach a video sink to.
    pub track: Option<TrackHandle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingState {
    Stable,
    HaveLocalOffer,
    HaveLocalPrAnswer,
    HaveRemoteOffer,
    HaveRemotePrAnswer,
    Closed,
}

impl SignalingState {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalingState::Stable => "stable",
            SignalingState::HaveLocalOffer => "have-local-offer",
            SignalingState::HaveLocalPrAnswer => "have-local-pranswer",
            SignalingState::HaveRemoteOffer => "have-remote-offer",
            SignalingState::HaveRemotePrAnswer => "have-remote-pranswer",
            SignalingState::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceConnectionState {
    New,
    Checking,
    Connected,
    Completed,
    Failed,
    Disconnected,
    Closed,
}

impl IceConnectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            IceConnectionState::New => "new",
            IceConnectionState::Checking => "checking",
            IceConnectionState::Connected => "connected",
            IceConnectionState::Completed => "completed",
            IceConnectionState::Failed => "failed",
            IceConnectionState::Disconnected => "disconnected",
            IceConnectionState::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

impl ConnectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionState::New => "new",
            ConnectionState::Connecting => "connecting",
            ConnectionState::Connected => "connected",
            ConnectionState::Disconnected => "disconnected",
            ConnectionState::Failed => "failed",
            ConnectionState::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceGatheringState {
    New,
    Gathering,
    Complete,
}

impl IceGatheringState {
    pub fn as_str(self) -> &'static str {
        match self {
            IceGatheringState::New => "new",
            IceGatheringState::Gathering => "gathering",
            IceGatheringState::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcEvent {
    SignalingState(SignalingState),
    IceConnectionState(IceConnectionState),
    ConnectionState(ConnectionState),
    IceGatheringState(IceGatheringState),
    IceCandidate(IceCandidateInit),
    NegotiationNeeded,
    Track(TrackInfo),
}

/// The numbers are libwebrtc's `RTCErrorType`, which the Windows add-in puts in
/// `result.RtcError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidParameter = 3,
    /// The SDP or candidate line did not parse. The host reports this one differently from a
    /// libwebrtc error.
    SdpParse = 5,
    InvalidState = 6,
    Internal = 10,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    pub kind: ErrorKind,
    pub message: String,
}

impl EngineError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> EngineError {
        EngineError {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for EngineError {}

/// Completion of an asynchronous call. Runs on an engine thread.
pub type Done<T> = Box<dyn FnOnce(Result<T, EngineError>) + Send>;

/// Receives a peer connection's events on engine threads.
pub type EventSink = Arc<dyn Fn(PcEvent) + Send + Sync>;

/// A sound device as the engine needs to find it. Ids are the sound server's node names; the
/// engine's own device manager knows devices by their description, so the label travels along.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: String,
    pub label: String,
}

/// What the caller would like from a camera; each field is a preference (see `camera`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VideoConstraints {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
}

/// Receives video frames on an engine thread. It must return quickly: it runs between frames.
pub type VideoSink = Arc<dyn Fn(&VideoFrame) + Send + Sync>;

/// A media track: captured here, or received from the other side.
pub trait Track: Send + Sync {
    fn id(&self) -> String;
    fn kind(&self) -> Kind;
    fn set_enabled(&self, enabled: bool);
    /// Ends the track; a stopped track cannot be started again.
    fn stop(&self);
    /// Delivers the frames of a video track (the picture a remote peer sends, or the camera, for a
    /// self view) to `sink`, until it is replaced or `None` is set. Audio tracks ignore it.
    fn set_video_sink(&self, sink: Option<VideoSink>);
    /// For the engine to recover its own type when a track is handed back to it.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// One m-line's worth of sender and receiver. Handles stay valid for the life of the connection.
pub trait Transceiver: Send + Sync {
    /// Tells transceivers apart: two handles of one transceiver have the same id, and no two
    /// transceivers that exist at the same time share one. The engine's list of transceivers
    /// shrinks when a rejected section is dropped and grows when one is recycled, so a position
    /// in it says nothing.
    fn id(&self) -> u64;
    fn info(&self) -> TransceiverInfo;
    fn set_direction(&self, direction: Direction) -> Result<(), EngineError>;
    /// `replaceTrack` on the sender; `None` clears it. The id of the track now sent, if any.
    fn set_sender_track(&self, track: Option<Arc<dyn Track>>) -> Result<(), EngineError>;
    fn sender_track_id(&self) -> Option<String>;
    /// `setCodecPreferences`: the codecs this transceiver may negotiate, best first. They must come
    /// from the engine's own capabilities.
    fn set_codec_preferences(&self, codecs: Vec<CodecCapability>) -> Result<(), EngineError>;
    /// `removeTrack`: stops sending and lets the next negotiation drop the sender's stream.
    fn remove_track(&self) -> Result<(), EngineError>;
    /// Stops the transceiver for good: the next negotiation rejects its section.
    fn stop(&self) -> Result<(), EngineError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataChannelEvent {
    Open,
    Closing,
    Closed,
    Message(Vec<u8>),
}

pub type DataChannelSink = Arc<dyn Fn(DataChannelEvent) + Send + Sync>;

pub trait DataChannel: Send + Sync {
    /// The SCTP stream id; -1 until one is assigned.
    fn id(&self) -> i32;
    fn send(&self, data: &[u8], binary: bool) -> Result<(), EngineError>;
    fn close(&self);
}

pub trait MediaEngine: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    /// A local microphone track. Capture starts when the track is sent.
    fn create_audio_capture(&self, device: &AudioDevice) -> Result<Arc<dyn Track>, EngineError>;

    /// A local camera track. `device_id` is a camera id from the device list; empty or `default`
    /// is the first camera. The camera is on only while the track is enabled.
    fn create_video_capture(
        &self,
        device_id: &str,
        constraints: VideoConstraints,
    ) -> Result<Arc<dyn Track>, EngineError>;

    /// Where remote audio plays (`setSinkId`).
    fn set_playout_device(&self, device: &AudioDevice) -> Result<(), EngineError>;

    fn create_peer_connection(
        &self,
        config: PcConfig,
        events: EventSink,
    ) -> Result<Box<dyn PeerConnection>, EngineError>;
}

pub trait PeerConnection: Send + Sync {
    fn create_offer(&self, done: Done<SessionDescription>);
    fn create_answer(&self, done: Done<SessionDescription>);
    fn set_local_description(&self, desc: SessionDescription, done: Done<()>);
    fn set_remote_description(&self, desc: SessionDescription, done: Done<()>);
    fn add_ice_candidate(&self, candidate: IceCandidateInit, done: Done<()>);
    fn local_description(&self) -> Option<SessionDescription>;
    fn remote_description(&self) -> Option<SessionDescription>;
    /// Every transceiver, in creation order. The order is stable: transceivers are only appended.
    fn transceivers(&self) -> Vec<Arc<dyn Transceiver>>;
    /// Adds a transceiver with no track, as `addTransceiver` does.
    fn add_transceiver(
        &self,
        kind: Kind,
        direction: Direction,
    ) -> Result<Arc<dyn Transceiver>, EngineError>;
    fn create_data_channel(
        &self,
        label: &str,
        events: DataChannelSink,
    ) -> Result<Arc<dyn DataChannel>, EngineError>;
    /// The stats report as the engine serialises it: a JSON array of objects with `type`, `id`
    /// and `timestamp`, the W3C `RTCStatsReport` shape. An empty report may be `""`.
    fn get_stats(&self, done: Done<String>);
    fn close(&self);
}
