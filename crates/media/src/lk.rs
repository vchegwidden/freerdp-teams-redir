//! libwebrtc through LiveKit's `libwebrtc` crate (a prebuilt M1xx static library, linked in).
//!
//! The crate's API is async for the calls that complete on libwebrtc's signaling thread, so they
//! run on a small tokio runtime that exists only to wait for them. Everything is created lazily:
//! loading the add-in into a FreeRDP session that never starts a Teams call costs nothing.
//!
//! Audio goes through libwebrtc's platform audio device module (PulseAudio, which PipeWire serves):
//! remote audio plays on the selected output, and a "device track" captures the selected input.
//! The module knows devices by index and description, so devices are found by their label.
//!
//! Known gaps of the bindings, to be closed with the media work:
//! - `RtcConfiguration` has no bundle policy, rtcp-mux policy or port ranges; libwebrtc's defaults
//!   (max-bundle, require) are what the Windows host asks for anyway.
//! - Only the *current* local and remote descriptions are readable, not the pending ones, so
//!   `local_description` falls back to the description last set while an offer is outstanding.

use std::sync::{Arc, Mutex, OnceLock};

use libwebrtc::MediaType;
use libwebrtc::data_channel::{DataChannelInit, DataChannelState};
use libwebrtc::ice_candidate::IceCandidate;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::peer_connection as lw_pc;
use libwebrtc::peer_connection::{AnswerOptions, OfferOptions};
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use libwebrtc::peer_connection_factory::{
    IceServer as LwIceServer, IceTransportsType, PeerConnectionFactory, RtcConfiguration,
};
use libwebrtc::rtp_parameters::RtpCapabilities as LwCaps;
use libwebrtc::rtp_transceiver::{RtpTransceiver, RtpTransceiverDirection, RtpTransceiverInit};
use libwebrtc::session_description::{SdpType as LwSdpType, SessionDescription as LwSdp};
use libwebrtc::video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation};
use libwebrtc::video_source::VideoResolution;
use libwebrtc::video_source::native::NativeVideoSource;
use libwebrtc::video_stream::native::NativeVideoStream;
use libwebrtc::{RtcError, RtcErrorType};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio_stream::StreamExt;

use crate::*;

struct Inner {
    factory: PeerConnectionFactory,
    /// Kept alive for `handle`.
    _rt: Runtime,
    handle: Handle,
}

#[derive(Default)]
pub struct LkEngine {
    inner: OnceLock<Result<Inner, String>>,
    /// Whether the platform audio device module started; decided once.
    adm: OnceLock<bool>,
}

impl LkEngine {
    pub fn new() -> LkEngine {
        LkEngine::default()
    }

    fn inner(&self) -> Result<&Inner, EngineError> {
        self.inner
            .get_or_init(|| {
                let rt = Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("teams-redir-rtc")
                    .enable_time()
                    .build()
                    .map_err(|e| format!("cannot start the media runtime: {e}"))?;
                Ok(Inner {
                    factory: PeerConnectionFactory::default(),
                    handle: rt.handle().clone(),
                    _rt: rt,
                })
            })
            .as_ref()
            .map_err(|e| EngineError::new(ErrorKind::Internal, e.clone()))
    }

    /// Starts the audio device module (remote audio then plays on the speakers).
    fn audio(&self) -> Result<&PeerConnectionFactory, EngineError> {
        let factory = &self.inner()?.factory;
        let ready = *self.adm.get_or_init(|| {
            let ok = factory.acquire_platform_adm();
            if ok {
                // Both default to off in the bindings (they are built for apps that feed their own
                // audio): without recording a microphone track yields no samples at all.
                factory.set_adm_playout_enabled(true);
                factory.set_adm_recording_enabled(true);
            }
            ok
        });
        if ready {
            Ok(factory)
        } else {
            Err(EngineError::new(
                ErrorKind::Internal,
                "the audio device module did not start",
            ))
        }
    }
}

/// The index of the device whose description is `label`. The module's first entry of each kind is
/// `default: <description>`, which is skipped: the real entry follows it.
fn find_device(count: i16, name: impl Fn(u16) -> String, label: &str) -> Option<u16> {
    (0..count.max(0) as u16).find(|&i| {
        let n = name(i);
        !n.starts_with("default: ") && n == label
    })
}

impl MediaEngine for LkEngine {
    fn capabilities(&self) -> Capabilities {
        let Ok(inner) = self.inner() else {
            return Capabilities::default();
        };
        let f = &inner.factory;
        Capabilities {
            send_audio: caps(f.get_rtp_sender_capabilities(MediaType::Audio)),
            send_video: caps(f.get_rtp_sender_capabilities(MediaType::Video)),
            recv_audio: caps(f.get_rtp_receiver_capabilities(MediaType::Audio)),
            recv_video: caps(f.get_rtp_receiver_capabilities(MediaType::Video)),
        }
    }

    fn create_audio_capture(&self, device: &AudioDevice) -> Result<Arc<dyn Track>, EngineError> {
        let f = self.audio()?;
        let index = find_device(
            f.recording_devices(),
            |i| f.recording_device_name(i),
            &device.label,
        )
        .ok_or_else(|| {
            EngineError::new(
                ErrorKind::InvalidParameter,
                format!("no recording device \"{}\"", device.label),
            )
        })?;
        let running = f.recording_is_initialized();
        if running {
            f.stop_recording();
        }
        if !f.set_recording_device(index) {
            return Err(EngineError::new(
                ErrorKind::Internal,
                format!("cannot select recording device {index}"),
            ));
        }
        if running {
            f.init_recording();
            f.start_recording();
        }
        let track = f.create_device_audio_track(&format!("mic-{}", device.id));
        Ok(Arc::new(LkTrack {
            track: MediaStreamTrack::Audio(track),
            camera: None,
            rt: None,
            sink_task: Mutex::new(None),
        }))
    }

    fn create_video_capture(
        &self,
        device_id: &str,
        constraints: VideoConstraints,
    ) -> Result<Arc<dyn Track>, EngineError> {
        let inner = self.inner()?;
        let cameras = camera::list();
        let default = matches!(device_id, "" | "default" | "communications");
        let cam = if default {
            cameras.first()
        } else {
            cameras.iter().find(|c| c.id == device_id)
        }
        .ok_or_else(|| {
            EngineError::new(
                ErrorKind::InvalidParameter,
                format!("no camera \"{device_id}\""),
            )
        })?;
        let want = camera::Constraints {
            width: constraints.width,
            height: constraints.height,
            frame_rate: constraints.frame_rate,
        };
        // The source starts a task of its own, so it has to be made inside the runtime.
        let _runtime = inner.handle.enter();
        let source = NativeVideoSource::new(
            VideoResolution {
                width: want.width.unwrap_or(1280),
                height: want.height.unwrap_or(720),
            },
            false,
        );
        let track = inner
            .factory
            .create_video_track(&format!("cam-{}", cam.id), source.clone());
        let feed = CameraFeed {
            path: cam.path.clone(),
            want,
            preview: Arc::new(Mutex::new(None)),
            source,
            state: Mutex::new(FeedState {
                capture: None,
                enabled: true,
                stopped: false,
            }),
        };
        feed.start()
            .map_err(|e| EngineError::new(ErrorKind::Internal, e))?;
        Ok(Arc::new(LkTrack {
            track: MediaStreamTrack::Video(track),
            camera: Some(feed),
            rt: None,
            sink_task: Mutex::new(None),
        }))
    }

    fn set_playout_device(&self, device: &AudioDevice) -> Result<(), EngineError> {
        let f = self.audio()?;
        let index = find_device(
            f.playout_devices(),
            |i| f.playout_device_name(i),
            &device.label,
        )
        .ok_or_else(|| {
            EngineError::new(
                ErrorKind::InvalidParameter,
                format!("no playout device \"{}\"", device.label),
            )
        })?;
        let running = f.playout_is_initialized();
        if running {
            f.stop_playout();
        }
        let ok = f.set_playout_device(index);
        if running {
            f.init_playout();
            f.start_playout();
        }
        if ok {
            Ok(())
        } else {
            Err(EngineError::new(
                ErrorKind::Internal,
                format!("cannot select playout device {index}"),
            ))
        }
    }

    fn create_peer_connection(
        &self,
        config: PcConfig,
        events: EventSink,
    ) -> Result<Box<dyn PeerConnection>, EngineError> {
        // Remote audio needs the device module running before the first track arrives. A machine
        // without sound still gets a peer connection.
        let _ = self.audio();
        let inner = self.inner()?;
        let mut cfg = RtcConfiguration::default();
        cfg.ice_servers = config
            .ice_servers
            .into_iter()
            .map(|s| LwIceServer {
                urls: s.urls,
                username: s.username,
                password: s.credential,
            })
            .collect();
        if config.relay_only {
            cfg.ice_transport_type = IceTransportsType::Relay;
        }
        let pc = inner.factory.create_peer_connection(cfg).map_err(error)?;
        let sink = |events: &EventSink| {
            let events = events.clone();
            move |e: PcEvent| events(e)
        };

        let emit = sink(&events);
        pc.on_signaling_state_change(Some(Box::new(move |s| {
            emit(PcEvent::SignalingState(match s {
                lw_pc::SignalingState::Stable => SignalingState::Stable,
                lw_pc::SignalingState::HaveLocalOffer => SignalingState::HaveLocalOffer,
                lw_pc::SignalingState::HaveLocalPrAnswer => SignalingState::HaveLocalPrAnswer,
                lw_pc::SignalingState::HaveRemoteOffer => SignalingState::HaveRemoteOffer,
                lw_pc::SignalingState::HaveRemotePrAnswer => SignalingState::HaveRemotePrAnswer,
                lw_pc::SignalingState::Closed => SignalingState::Closed,
            }))
        })));
        let emit = sink(&events);
        pc.on_ice_connection_state_change(Some(Box::new(move |s| {
            emit(PcEvent::IceConnectionState(match s {
                lw_pc::IceConnectionState::New | lw_pc::IceConnectionState::Max => {
                    IceConnectionState::New
                }
                lw_pc::IceConnectionState::Checking => IceConnectionState::Checking,
                lw_pc::IceConnectionState::Connected => IceConnectionState::Connected,
                lw_pc::IceConnectionState::Completed => IceConnectionState::Completed,
                lw_pc::IceConnectionState::Failed => IceConnectionState::Failed,
                lw_pc::IceConnectionState::Disconnected => IceConnectionState::Disconnected,
                lw_pc::IceConnectionState::Closed => IceConnectionState::Closed,
            }))
        })));
        let emit = sink(&events);
        pc.on_connection_state_change(Some(Box::new(move |s| {
            emit(PcEvent::ConnectionState(match s {
                lw_pc::PeerConnectionState::New => ConnectionState::New,
                lw_pc::PeerConnectionState::Connecting => ConnectionState::Connecting,
                lw_pc::PeerConnectionState::Connected => ConnectionState::Connected,
                lw_pc::PeerConnectionState::Disconnected => ConnectionState::Disconnected,
                lw_pc::PeerConnectionState::Failed => ConnectionState::Failed,
                lw_pc::PeerConnectionState::Closed => ConnectionState::Closed,
            }))
        })));
        let emit = sink(&events);
        pc.on_ice_gathering_state_change(Some(Box::new(move |s| {
            emit(PcEvent::IceGatheringState(match s {
                lw_pc::IceGatheringState::New => IceGatheringState::New,
                lw_pc::IceGatheringState::Gathering => IceGatheringState::Gathering,
                lw_pc::IceGatheringState::Complete => IceGatheringState::Complete,
            }))
        })));
        let emit = sink(&events);
        pc.on_ice_candidate(Some(Box::new(move |c: IceCandidate| {
            emit(PcEvent::IceCandidate(IceCandidateInit {
                sdp_mid: c.sdp_mid(),
                sdp_mline_index: c.sdp_mline_index(),
                candidate: c.candidate(),
            }))
        })));
        let emit = sink(&events);
        pc.on_negotiation_needed(Some(Box::new(move |_| emit(PcEvent::NegotiationNeeded))));
        let emit = sink(&events);
        let rt = inner.handle.clone();
        pc.on_track(Some(Box::new(move |t: lw_pc::TrackEvent| {
            let kind = match t.track {
                MediaStreamTrack::Audio(_) => Kind::Audio,
                MediaStreamTrack::Video(_) => Kind::Video,
            };
            emit(PcEvent::Track(TrackInfo {
                transceiver: transceiver_info(&t.transceiver, Some(kind)),
                track_id: t.track.id(),
                stream_ids: t.streams.iter().map(|s| s.id()).collect(),
                track: Some(TrackHandle(Arc::new(LkTrack {
                    track: t.track.clone(),
                    camera: None,
                    rt: Some(rt.clone()),
                    sink_task: Mutex::new(None),
                }))),
            }))
        })));

        Ok(Box::new(LkPc {
            pc,
            handle: inner.handle.clone(),
            last_local: Mutex::new(None),
            last_remote: Mutex::new(None),
        }))
    }
}

struct LkPc {
    pc: lw_pc::PeerConnection,
    handle: Handle,
    /// See the module comment: the current descriptions are all the bindings show.
    last_local: Mutex<Option<SessionDescription>>,
    last_remote: Mutex<Option<SessionDescription>>,
}

impl PeerConnection for LkPc {
    fn create_offer(&self, done: Done<SessionDescription>) {
        let pc = self.pc.clone();
        self.handle.spawn(async move {
            done(
                pc.create_offer(OfferOptions::default())
                    .await
                    .map(from_lk_sdp)
                    .map_err(error),
            )
        });
    }

    fn create_answer(&self, done: Done<SessionDescription>) {
        let pc = self.pc.clone();
        self.handle.spawn(async move {
            done(
                pc.create_answer(AnswerOptions::default())
                    .await
                    .map(from_lk_sdp)
                    .map_err(error),
            )
        });
    }

    fn set_local_description(&self, desc: SessionDescription, done: Done<()>) {
        let lw = match to_lk_sdp(&desc) {
            Ok(lw) => lw,
            Err(e) => return done(Err(e)),
        };
        let pc = self.pc.clone();
        self.handle.spawn(async move {
            done(pc.set_local_description(lw).await.map_err(error));
        });
        *self.last_local.lock().unwrap() = Some(desc);
    }

    fn set_remote_description(&self, desc: SessionDescription, done: Done<()>) {
        let lw = match to_lk_sdp(&desc) {
            Ok(lw) => lw,
            Err(e) => return done(Err(e)),
        };
        let pc = self.pc.clone();
        self.handle.spawn(async move {
            done(pc.set_remote_description(lw).await.map_err(error));
        });
        *self.last_remote.lock().unwrap() = Some(desc);
    }

    fn add_ice_candidate(&self, candidate: IceCandidateInit, done: Done<()>) {
        let parsed = IceCandidate::parse(
            &candidate.sdp_mid,
            candidate.sdp_mline_index,
            &candidate.candidate,
        )
        .map_err(|e| {
            EngineError::new(
                ErrorKind::SdpParse,
                format!("SDP parse failure - line=[{}] - {}", e.line, e.description),
            )
        });
        let parsed = match parsed {
            Ok(p) => p,
            Err(e) => return done(Err(e)),
        };
        let pc = self.pc.clone();
        self.handle
            .spawn(async move { done(pc.add_ice_candidate(parsed).await.map_err(error)) });
    }

    fn local_description(&self) -> Option<SessionDescription> {
        self.pc
            .current_local_description()
            .map(from_lk_sdp)
            .or_else(|| self.last_local.lock().unwrap().clone())
    }

    fn remote_description(&self) -> Option<SessionDescription> {
        self.pc
            .current_remote_description()
            .map(from_lk_sdp)
            .or_else(|| self.last_remote.lock().unwrap().clone())
    }

    fn transceivers(&self) -> Vec<Arc<dyn Transceiver>> {
        self.pc
            .transceivers()
            .into_iter()
            .map(|t| {
                Arc::new(LkTransceiver {
                    pc: self.pc.clone(),
                    t,
                    kind: None,
                }) as Arc<dyn Transceiver>
            })
            .collect()
    }

    fn add_transceiver(
        &self,
        kind: Kind,
        direction: Direction,
    ) -> Result<Arc<dyn Transceiver>, EngineError> {
        let init = RtpTransceiverInit {
            direction: to_lk_direction(direction),
            stream_ids: Vec::new(),
            send_encodings: Vec::new(),
        };
        let media = match kind {
            Kind::Audio => MediaType::Audio,
            Kind::Video => MediaType::Video,
        };
        let t = self
            .pc
            .add_transceiver_for_media(media, init)
            .map_err(error)?;
        Ok(Arc::new(LkTransceiver {
            pc: self.pc.clone(),
            t,
            kind: Some(kind),
        }))
    }

    fn create_data_channel(
        &self,
        label: &str,
        events: DataChannelSink,
    ) -> Result<Arc<dyn DataChannel>, EngineError> {
        let dc = self
            .pc
            .create_data_channel(label, DataChannelInit::default())
            .map_err(error)?;
        let sink = events.clone();
        dc.on_state_change(Some(Box::new(move |s| match s {
            DataChannelState::Connecting => {}
            DataChannelState::Open => sink(DataChannelEvent::Open),
            DataChannelState::Closing => sink(DataChannelEvent::Closing),
            DataChannelState::Closed => sink(DataChannelEvent::Closed),
        })));
        dc.on_message(Some(Box::new(move |m| {
            events(DataChannelEvent::Message(m.data.to_vec()))
        })));
        Ok(Arc::new(LkDataChannel { dc }))
    }

    fn get_stats(&self, done: Done<String>) {
        let pc = self.pc.clone();
        self.handle
            .spawn(async move { done(pc.get_stats_json().await.map_err(error)) });
    }

    fn close(&self) {
        self.pc.close();
    }
}

struct LkTrack {
    track: MediaStreamTrack,
    /// Set for a camera track.
    camera: Option<CameraFeed>,
    /// For a remote track, the runtime that drives its frame stream.
    rt: Option<Handle>,
    /// The task that hands a remote video track's frames to the sink.
    sink_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for LkTrack {
    fn drop(&mut self) {
        if let Some(task) = self.sink_task.lock().unwrap().take() {
            task.abort();
        }
    }
}

/// A decoded frame as tightly packed I420.
fn tight_i420(buffer: &dyn VideoBuffer) -> Option<camera::Frame> {
    let i420 = buffer.to_i420();
    let (w, h) = (i420.width() as usize, i420.height() as usize);
    let (cw, ch) = (i420.chroma_width() as usize, i420.chroma_height() as usize);
    let (sy, su, sv) = i420.strides();
    let (y, u, v) = i420.data();
    let copy = |src: &[u8], stride: usize, width: usize, rows: usize| -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(width * rows);
        for r in 0..rows {
            out.extend_from_slice(src.get(r * stride..r * stride + width)?);
        }
        Some(out)
    };
    Some(camera::Frame {
        width: w as u32,
        height: h as u32,
        y: copy(y, sy as usize, w, h)?,
        u: copy(u, su as usize, cw, ch)?,
        v: copy(v, sv as usize, cw, ch)?,
    })
}

/// Where a camera's frames go: one entry for each track on it.
type FrameSinks = Arc<Mutex<Vec<(u64, Box<dyn FnMut(&camera::Frame) + Send>)>>>;

/// A camera that is open. A V4L2 device has one reader, and Teams makes several tracks on the same
/// camera at once (the settings page asks for the stream twice, then wraps it for the preview), so
/// the tracks share it. It closes when the last lease is dropped.
struct OpenCamera {
    sinks: FrameSinks,
    _capture: camera::Capture,
}

/// A track's claim on an open camera.
struct CameraLease {
    camera: Arc<OpenCamera>,
    id: u64,
}

impl CameraLease {
    fn acquire(
        path: &std::path::Path,
        want: camera::Constraints,
        sink: Box<dyn FnMut(&camera::Frame) + Send>,
    ) -> Result<CameraLease, String> {
        static OPEN: Mutex<Vec<(std::path::PathBuf, std::sync::Weak<OpenCamera>)>> =
            Mutex::new(Vec::new());
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut open = OPEN.lock().unwrap();
        open.retain(|(_, c)| c.strong_count() > 0);
        let shared = open
            .iter()
            .find(|(p, _)| p == path)
            .and_then(|(_, c)| c.upgrade());
        let camera = match shared {
            Some(c) => c,
            None => {
                let sinks: FrameSinks = Arc::new(Mutex::new(Vec::new()));
                let fan_out = sinks.clone();
                let capture = camera::Capture::start(
                    path,
                    want,
                    Box::new(move |frame| {
                        for (_, sink) in fan_out.lock().unwrap().iter_mut() {
                            sink(&frame);
                        }
                    }),
                )?;
                let c = Arc::new(OpenCamera {
                    sinks,
                    _capture: capture,
                });
                open.push((path.to_path_buf(), Arc::downgrade(&c)));
                c
            }
        };
        camera.sinks.lock().unwrap().push((id, sink));
        Ok(CameraLease { camera, id })
    }
}

impl Drop for CameraLease {
    fn drop(&mut self) {
        self.camera
            .sinks
            .lock()
            .unwrap()
            .retain(|(id, _)| *id != self.id);
    }
}

/// The camera behind a video track. It runs only while the track is enabled and not stopped, so
/// a track Teams has muted leaves the camera, and its light, off.
struct CameraFeed {
    path: std::path::PathBuf,
    want: camera::Constraints,
    /// Where the frames also go for a self view.
    preview: Arc<Mutex<Option<VideoSink>>>,
    source: NativeVideoSource,
    state: Mutex<FeedState>,
}

struct FeedState {
    capture: Option<CameraLease>,
    enabled: bool,
    stopped: bool,
}

impl CameraFeed {
    /// Opens the camera if it should be running and is not.
    fn start(&self) -> Result<(), String> {
        let mut st = self.state.lock().unwrap();
        if st.stopped || !st.enabled || st.capture.is_some() {
            return Ok(());
        }
        let source = self.source.clone();
        let preview = self.preview.clone();
        st.capture = Some(CameraLease::acquire(
            &self.path,
            self.want,
            Box::new(move |frame| {
                push_frame(&source, frame);
                let sink = preview.lock().unwrap().clone();
                if let Some(sink) = sink {
                    sink(frame);
                }
            }),
        )?);
        Ok(())
    }

    fn set_enabled(&self, enabled: bool) {
        {
            let mut st = self.state.lock().unwrap();
            st.enabled = enabled;
            if !enabled {
                st.capture = None;
            }
        }
        // A camera that cannot be opened again leaves a black track: nobody is told here.
        let _ = self.start();
    }

    fn stop(&self) {
        let mut st = self.state.lock().unwrap();
        st.stopped = true;
        st.capture = None;
    }
}

/// Hands a camera frame to libwebrtc.
fn push_frame(source: &NativeVideoSource, f: &camera::Frame) {
    let (w, h) = (f.width, f.height);
    let mut buf = I420Buffer::with_strides(w, h, w, w.div_ceil(2), w.div_ceil(2));
    let (y, u, v) = buf.data_mut();
    if y.len() == f.y.len() && u.len() == f.u.len() && v.len() == f.v.len() {
        y.copy_from_slice(&f.y);
        u.copy_from_slice(&f.u);
        v.copy_from_slice(&f.v);
        source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, buf));
    }
}

impl Track for LkTrack {
    fn id(&self) -> String {
        self.track.id()
    }

    fn kind(&self) -> Kind {
        match self.track {
            MediaStreamTrack::Audio(_) => Kind::Audio,
            MediaStreamTrack::Video(_) => Kind::Video,
        }
    }

    fn set_enabled(&self, enabled: bool) {
        self.track.set_enabled(enabled);
        if let Some(cam) = &self.camera {
            cam.set_enabled(enabled);
        }
    }

    /// The bindings cannot end a track, so it is silenced; it is gone when its last handle is. A
    /// camera is closed at once.
    fn stop(&self) {
        self.track.set_enabled(false);
        if let Some(cam) = &self.camera {
            cam.stop();
        }
    }

    fn set_video_sink(&self, sink: Option<VideoSink>) {
        if let Some(cam) = &self.camera {
            *cam.preview.lock().unwrap() = sink;
            return;
        }
        let (MediaStreamTrack::Video(video), Some(rt)) = (&self.track, &self.rt) else {
            return;
        };
        let mut task = self.sink_task.lock().unwrap();
        if let Some(old) = task.take() {
            old.abort();
        }
        let Some(sink) = sink else {
            return;
        };
        let _runtime = rt.enter();
        let mut stream = NativeVideoStream::new(video.clone());
        *task = Some(rt.spawn(async move {
            while let Some(frame) = stream.next().await {
                if let Some(f) = tight_i420(frame.buffer.as_ref()) {
                    sink(&f);
                }
            }
        }));
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct LkTransceiver {
    pc: lw_pc::PeerConnection,
    t: RtpTransceiver,
    /// Known when this side created the transceiver.
    kind: Option<Kind>,
}

impl Transceiver for LkTransceiver {
    fn id(&self) -> u64 {
        self.t.id() as u64
    }

    fn info(&self) -> TransceiverInfo {
        transceiver_info(&self.t, self.kind)
    }

    fn set_direction(&self, direction: Direction) -> Result<(), EngineError> {
        self.t
            .set_direction(to_lk_direction(direction))
            .map_err(error)
    }

    fn set_sender_track(&self, track: Option<Arc<dyn Track>>) -> Result<(), EngineError> {
        let track = match track {
            None => None,
            Some(t) => match t.as_any().downcast_ref::<LkTrack>() {
                Some(t) => Some(t.track.clone()),
                None => {
                    return Err(EngineError::new(
                        ErrorKind::InvalidParameter,
                        "not a libwebrtc track",
                    ));
                }
            },
        };
        self.t.sender().set_track(track).map_err(error)
    }

    fn sender_track_id(&self) -> Option<String> {
        self.t.sender().track().map(|t| t.id())
    }

    fn set_codec_preferences(&self, codecs: Vec<CodecCapability>) -> Result<(), EngineError> {
        let codecs = codecs
            .into_iter()
            .map(|c| libwebrtc::rtp_parameters::RtpCodecCapability {
                channels: c.channels,
                clock_rate: c.clock_rate,
                mime_type: c.mime_type,
                sdp_fmtp_line: c.sdp_fmtp_line,
            })
            .collect();
        self.t.set_codec_preferences(codecs).map_err(error)
    }

    fn stop(&self) -> Result<(), EngineError> {
        self.t.stop().map_err(error)
    }

    fn remove_track(&self) -> Result<(), EngineError> {
        self.pc.remove_track(self.t.sender()).map_err(error)
    }
}

struct LkDataChannel {
    dc: libwebrtc::data_channel::DataChannel,
}

impl DataChannel for LkDataChannel {
    fn id(&self) -> i32 {
        self.dc.id()
    }

    fn send(&self, data: &[u8], binary: bool) -> Result<(), EngineError> {
        self.dc
            .send(data, binary)
            .map_err(|e| EngineError::new(ErrorKind::InvalidState, e.to_string()))
    }

    fn close(&self) {
        self.dc.close();
    }
}

fn caps(c: LwCaps) -> RtpCapabilities {
    RtpCapabilities {
        codecs: c
            .codecs
            .into_iter()
            .map(|k| CodecCapability {
                mime_type: k.mime_type,
                clock_rate: k.clock_rate,
                channels: k.channels,
                sdp_fmtp_line: k.sdp_fmtp_line,
            })
            .collect(),
        header_extensions: c.header_extensions.into_iter().map(|h| h.uri).collect(),
    }
}

fn error(e: RtcError) -> EngineError {
    let kind = match e.error_type {
        RtcErrorType::InvalidSdp => ErrorKind::InvalidParameter,
        RtcErrorType::InvalidState => ErrorKind::InvalidState,
        RtcErrorType::Internal => ErrorKind::Internal,
    };
    EngineError::new(kind, e.message)
}

fn from_lk_sdp(d: LwSdp) -> SessionDescription {
    SessionDescription {
        kind: match d.sdp_type() {
            LwSdpType::Offer => SdpType::Offer,
            LwSdpType::PrAnswer => SdpType::PrAnswer,
            LwSdpType::Answer => SdpType::Answer,
            LwSdpType::Rollback => SdpType::Rollback,
        },
        sdp: d.to_string(),
    }
}

fn to_lk_sdp(d: &SessionDescription) -> Result<LwSdp, EngineError> {
    let kind = match d.kind {
        SdpType::Offer => LwSdpType::Offer,
        SdpType::PrAnswer => LwSdpType::PrAnswer,
        SdpType::Answer => LwSdpType::Answer,
        SdpType::Rollback => LwSdpType::Rollback,
    };
    LwSdp::parse(&d.sdp, kind).map_err(|e| {
        EngineError::new(
            ErrorKind::SdpParse,
            format!("SDP parse failure - line=[{}] - {}", e.line, e.description),
        )
    })
}

fn to_lk_direction(d: Direction) -> RtpTransceiverDirection {
    match d {
        Direction::SendRecv => RtpTransceiverDirection::SendRecv,
        Direction::SendOnly => RtpTransceiverDirection::SendOnly,
        Direction::RecvOnly => RtpTransceiverDirection::RecvOnly,
        Direction::Inactive => RtpTransceiverDirection::Inactive,
        Direction::Stopped => RtpTransceiverDirection::Stopped,
    }
}

fn transceiver_info(t: &RtpTransceiver, kind: Option<Kind>) -> TransceiverInfo {
    let kind = kind.or_else(|| match t.receiver().track() {
        Some(MediaStreamTrack::Audio(_)) => Some(Kind::Audio),
        Some(MediaStreamTrack::Video(_)) => Some(Kind::Video),
        None => None,
    });
    TransceiverInfo {
        mid: t.mid(),
        kind,
        direction: match t.direction() {
            RtpTransceiverDirection::SendRecv => Direction::SendRecv,
            RtpTransceiverDirection::SendOnly => Direction::SendOnly,
            RtpTransceiverDirection::RecvOnly => Direction::RecvOnly,
            RtpTransceiverDirection::Inactive => Direction::Inactive,
            RtpTransceiverDirection::Stopped => Direction::Stopped,
        },
    }
}

#[allow(dead_code)]
fn _assert_send_sync(e: &LkEngine) -> &dyn MediaEngine {
    let _: Option<Arc<dyn MediaEngine>> = None;
    e
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn wait<T: Send + 'static>(f: impl FnOnce(Done<T>)) -> Result<T, EngineError> {
        let (tx, rx) = mpsc::channel();
        f(Box::new(move |r| {
            let _ = tx.send(r);
        }));
        rx.recv_timeout(Duration::from_secs(10))
            .expect("engine call timed out")
    }

    #[test]
    fn audio_capabilities_include_opus() {
        let caps = LkEngine::new().capabilities();
        assert!(
            caps.send_audio
                .codecs
                .iter()
                .any(|c| c.mime_type == "audio/opus")
        );
        assert!(
            caps.recv_audio
                .codecs
                .iter()
                .any(|c| c.mime_type == "audio/opus")
        );
        assert!(!caps.send_audio.header_extensions.is_empty());
    }

    /// Two peer connections in one process negotiate an audio session over loopback ICE.
    #[test]
    fn two_peers_negotiate_over_loopback() {
        let engine = LkEngine::new();
        let (sink_a, log_a) = fake::collecting_sink();
        let (sink_b, log_b) = fake::collecting_sink();
        let a = engine
            .create_peer_connection(PcConfig::default(), sink_a)
            .unwrap();
        let b = engine
            .create_peer_connection(PcConfig::default(), sink_b)
            .unwrap();
        a.add_transceiver(Kind::Audio, Direction::SendRecv).unwrap();
        let channel_log = Arc::new(Mutex::new(Vec::new()));
        let l = channel_log.clone();
        let _channel = a
            .create_data_channel("main-channel", Arc::new(move |e| l.lock().unwrap().push(e)))
            .unwrap();

        let offer = wait(|d| a.create_offer(d)).unwrap();
        assert_eq!(offer.kind, SdpType::Offer);
        assert!(offer.sdp.contains("m=audio"), "{}", offer.sdp);
        wait(|d| a.set_local_description(offer.clone(), d)).unwrap();
        wait(|d| b.set_remote_description(offer, d)).unwrap();
        let answer = wait(|d| b.create_answer(d)).unwrap();
        assert_eq!(answer.kind, SdpType::Answer);
        wait(|d| b.set_local_description(answer.clone(), d)).unwrap();
        wait(|d| a.set_remote_description(answer, d)).unwrap();

        // B saw the remote track, and the peers gather candidates and trade them.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            log_b
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, PcEvent::Track(t) if t.transceiver.kind == Some(Kind::Audio))),
            "{:?}",
            log_b.lock().unwrap()
        );
        let candidates = |log: &Arc<Mutex<Vec<PcEvent>>>| -> Vec<IceCandidateInit> {
            log.lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    PcEvent::IceCandidate(c) => Some(c.clone()),
                    _ => None,
                })
                .collect()
        };
        let (ca, cb) = (candidates(&log_a), candidates(&log_b));
        assert!(!ca.is_empty() && !cb.is_empty());
        assert!(
            candidate::parse(&ca[0].candidate).is_some(),
            "{}",
            ca[0].candidate
        );
        for c in ca {
            wait(|d| b.add_ice_candidate(c, d)).unwrap();
        }
        for c in cb {
            wait(|d| a.add_ice_candidate(c, d)).unwrap();
        }
        std::thread::sleep(Duration::from_secs(3));
        let connected = |log: &Arc<Mutex<Vec<PcEvent>>>| {
            log.lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, PcEvent::ConnectionState(ConnectionState::Connected)))
        };
        assert!(
            connected(&log_a) && connected(&log_b),
            "A {:?}\nB {:?}",
            log_a.lock().unwrap(),
            log_b.lock().unwrap()
        );
        assert_eq!(a.transceivers()[0].info().mid.as_deref(), Some("0"));
        assert!(
            channel_log
                .lock()
                .unwrap()
                .contains(&DataChannelEvent::Open),
            "data channel never opened: {:?}",
            channel_log.lock().unwrap()
        );
        // The report is libwebrtc's own JSON array, with the transport and the data channel in it.
        let report = wait(|d| a.get_stats(d)).unwrap();
        let stats: serde_json::Value = serde_json::from_str(&report).unwrap();
        let types: Vec<&str> = stats
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["type"].as_str())
            .collect();
        assert!(
            types.contains(&"transport") && types.contains(&"data-channel"),
            "{types:?}"
        );
        a.close();
        b.close();
    }

    /// The shape of the call Teams sets up (2026-10-31): a data channel, an audio transceiver and
    /// nine receive-only video ones, negotiated with a second peer that answers.
    #[test]
    fn a_teams_shaped_call_negotiates() {
        let engine = LkEngine::new();
        let (sink_a, log_a) = fake::collecting_sink();
        let (sink_b, _) = fake::collecting_sink();
        let a = engine
            .create_peer_connection(PcConfig::default(), sink_a)
            .unwrap();
        let b = engine
            .create_peer_connection(PcConfig::default(), sink_b)
            .unwrap();
        let (dc_sink, dc_log) = {
            let log = Arc::new(Mutex::new(Vec::new()));
            let l = log.clone();
            let sink: DataChannelSink = Arc::new(move |e| l.lock().unwrap().push(e));
            (sink, log)
        };
        let dc = a.create_data_channel("main-channel", dc_sink).unwrap();
        let audio = a.add_transceiver(Kind::Audio, Direction::Inactive).unwrap();
        for _ in 0..9 {
            a.add_transceiver(Kind::Video, Direction::Inactive).unwrap();
        }
        audio.set_direction(Direction::SendRecv).unwrap();
        for t in a.transceivers().iter().skip(1) {
            t.set_direction(Direction::RecvOnly).unwrap();
        }
        assert_eq!(a.transceivers().len(), 10);
        assert_eq!(audio.info().direction, Direction::SendRecv);

        let offer = wait(|d| a.create_offer(d)).unwrap();
        let directions: Vec<&str> = offer
            .sdp
            .lines()
            .map(str::trim)
            .filter(|l| ["a=sendrecv", "a=sendonly", "a=recvonly", "a=inactive"].contains(l))
            .collect();
        assert_eq!(directions[0], "a=sendrecv", "{directions:?}");
        assert!(
            directions[1..].iter().all(|d| *d == "a=recvonly"),
            "{directions:?}"
        );
        assert_eq!(offer.sdp.matches("m=audio").count(), 1, "{}", offer.sdp);
        assert_eq!(offer.sdp.matches("m=video").count(), 9);
        assert!(offer.sdp.contains("m=application"), "{}", offer.sdp);
        wait(|d| a.set_local_description(offer.clone(), d)).unwrap();
        assert!(a.local_description().is_some());
        wait(|d| b.set_remote_description(offer, d)).unwrap();
        let answer = wait(|d| b.create_answer(d)).unwrap();
        wait(|d| b.set_local_description(answer.clone(), d)).unwrap();
        wait(|d| a.set_remote_description(answer, d)).unwrap();
        // Mids are assigned in transceiver order, and the data channel comes after them.
        let mids: Vec<_> = a.transceivers().iter().map(|t| t.info().mid).collect();
        assert_eq!(mids[0].as_deref(), Some("0"));
        assert_eq!(mids[9].as_deref(), Some("9"));
        assert!(
            log_a
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, PcEvent::IceCandidate(_))),
            "no candidates gathered"
        );
        let _ = (dc, dc_log);
    }

    /// Needs a microphone: captures from the machine's default input, sends it to a second peer
    /// over loopback and checks from the sender's stats that audio packets left. Nothing is kept.
    /// `cargo test -p teams-redir-media --features libwebrtc microphone_audio_is_sent -- --ignored`
    #[test]
    #[ignore = "uses the microphone"]
    fn microphone_audio_is_sent() {
        let label = std::process::Command::new("sh")
            .arg("-c")
            .arg("pactl -f json list sources | python3 -c \"import json,sys,subprocess; d=subprocess.check_output(['pactl','get-default-source'],text=True).strip(); print(next(s['description'] for s in json.load(sys.stdin) if s['name']==d))\"")
            .output()
            .expect("pactl");
        let label = String::from_utf8(label.stdout).unwrap().trim().to_owned();
        assert!(!label.is_empty(), "no default microphone");
        let engine = LkEngine::new();
        let (sink_a, log_a) = fake::collecting_sink();
        let (sink_b, log_b) = fake::collecting_sink();
        let a = engine
            .create_peer_connection(PcConfig::default(), sink_a)
            .unwrap();
        let b = engine
            .create_peer_connection(PcConfig::default(), sink_b)
            .unwrap();
        let mic = engine
            .create_audio_capture(&AudioDevice {
                id: "default".into(),
                label,
            })
            .unwrap();
        let t = a.add_transceiver(Kind::Audio, Direction::SendOnly).unwrap();
        t.set_sender_track(Some(mic)).unwrap();
        let offer = wait(|d| a.create_offer(d)).unwrap();
        wait(|d| a.set_local_description(offer.clone(), d)).unwrap();
        wait(|d| b.set_remote_description(offer, d)).unwrap();
        let answer = wait(|d| b.create_answer(d)).unwrap();
        wait(|d| b.set_local_description(answer.clone(), d)).unwrap();
        wait(|d| a.set_remote_description(answer, d)).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let candidates = |log: &Arc<Mutex<Vec<PcEvent>>>| -> Vec<IceCandidateInit> {
            log.lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    PcEvent::IceCandidate(c) => Some(c.clone()),
                    _ => None,
                })
                .collect()
        };
        for c in candidates(&log_a) {
            let _ = wait(|d| b.add_ice_candidate(c, d));
        }
        for c in candidates(&log_b) {
            let _ = wait(|d| a.add_ice_candidate(c, d));
        }
        std::thread::sleep(Duration::from_secs(3));
        if let Ok(f) = engine.audio() {
            println!(
                "recording initialized: {}, playout initialized: {}",
                f.recording_is_initialized(),
                f.playout_is_initialized()
            );
        }
        let report = wait(|d| a.get_stats(d)).unwrap();
        let stats: serde_json::Value = serde_json::from_str(&report).unwrap();
        let source = stats
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["type"] == "media-source")
            .expect("no media-source stat");
        let seconds = source["totalSamplesDuration"].as_f64().unwrap_or(0.0);
        println!("captured {seconds} s of audio");
        assert!(
            seconds > 1.0,
            "the microphone produced no samples: {source}"
        );
    }

    /// `createOffer` must keep the directions the caller set. (libwebrtc strips the receive bit
    /// when told not to offer to receive, which the bindings did by default: see vendor/README.md.)
    #[test]
    fn offers_keep_the_transceiver_directions() {
        let engine = LkEngine::new();
        let offered = |sdp: &str| -> Vec<String> {
            sdp.lines()
                .map(str::trim)
                .filter(|l| ["a=sendrecv", "a=sendonly", "a=recvonly", "a=inactive"].contains(l))
                .map(|l| l[2..].to_owned())
                .collect()
        };
        for (kind, init, expected) in [
            (Kind::Audio, Direction::SendRecv, "sendrecv"),
            (Kind::Audio, Direction::SendOnly, "sendonly"),
            (Kind::Audio, Direction::RecvOnly, "recvonly"),
            (Kind::Audio, Direction::Inactive, "inactive"),
            (Kind::Video, Direction::SendRecv, "sendrecv"),
            (Kind::Video, Direction::RecvOnly, "recvonly"),
        ] {
            let (sink, _) = fake::collecting_sink();
            let pc = engine
                .create_peer_connection(PcConfig::default(), sink)
                .unwrap();
            let t = pc.add_transceiver(kind, init).unwrap();
            let offer = wait(|d| pc.create_offer(d)).unwrap();
            assert_eq!(offered(&offer.sdp), [expected], "{kind:?} {init:?}");
            assert_eq!(
                t.info().direction,
                init,
                "the offer changed the transceiver"
            );
        }
        // Set afterwards, as Teams does.
        let (sink, _) = fake::collecting_sink();
        let pc = engine
            .create_peer_connection(PcConfig::default(), sink)
            .unwrap();
        let t = pc
            .add_transceiver(Kind::Audio, Direction::Inactive)
            .unwrap();
        t.set_direction(Direction::SendRecv).unwrap();
        let offer = wait(|d| pc.create_offer(d)).unwrap();
        assert_eq!(offered(&offer.sdp), ["sendrecv"]);
    }

    /// `replaceTrack(null)` reaches the engine as `set_sender_track(None)`; it used to
    /// dereference a null track and kill the whole client.
    #[test]
    fn clearing_a_sender_track_does_not_crash() {
        let engine = LkEngine::new();
        let (sink, _log) = fake::collecting_sink();
        let pc = engine
            .create_peer_connection(PcConfig::default(), sink)
            .unwrap();
        let t = pc
            .add_transceiver(Kind::Audio, Direction::SendRecv)
            .unwrap();
        t.set_sender_track(None).unwrap();
        assert_eq!(t.sender_track_id(), None);
    }

    /// Needs a webcam. Sends it to a second peer over loopback and checks, from the receiver's
    /// stats alone, that video frames arrived and decoded. No image is read or stored.
    /// `cargo test --release -p teams-redir-media --features libwebrtc camera_video_reaches -- --ignored --nocapture`
    #[test]
    #[ignore = "turns the camera on"]
    fn camera_video_reaches_a_peer() {
        let engine = LkEngine::new();
        let (sink_a, log_a) = fake::collecting_sink();
        let (sink_b, log_b) = fake::collecting_sink();
        let a = engine
            .create_peer_connection(PcConfig::default(), sink_a)
            .unwrap();
        let b = engine
            .create_peer_connection(PcConfig::default(), sink_b)
            .unwrap();
        let cam = engine
            .create_video_capture("default", VideoConstraints::default())
            .unwrap();
        assert_eq!(cam.kind(), Kind::Video);
        let t = a.add_transceiver(Kind::Video, Direction::SendOnly).unwrap();
        t.set_sender_track(Some(cam.clone())).unwrap();
        let offer = wait(|d| a.create_offer(d)).unwrap();
        assert!(offer.sdp.contains("m=video"), "{}", offer.sdp);
        wait(|d| a.set_local_description(offer.clone(), d)).unwrap();
        wait(|d| b.set_remote_description(offer, d)).unwrap();
        let answer = wait(|d| b.create_answer(d)).unwrap();
        wait(|d| b.set_local_description(answer.clone(), d)).unwrap();
        wait(|d| a.set_remote_description(answer, d)).unwrap();
        // Frame sinks: the remote peer's decoded video, and the local camera as a self view.
        let remote = log_b
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match e {
                PcEvent::Track(t) if t.transceiver.kind == Some(Kind::Video) => t.track.clone(),
                _ => None,
            })
            .expect("no remote video track event");
        let counts = Arc::new(Mutex::new((0u32, 0u32, 0u32, 0u32)));
        let c = counts.clone();
        remote
            .0
            .set_video_sink(Some(Arc::new(move |f: &camera::Frame| {
                let mut c = c.lock().unwrap();
                c.0 += 1;
                c.1 = f.width;
                c.2 = f.height;
                assert_eq!(f.y.len(), (f.width * f.height) as usize);
                assert_eq!(f.u.len(), f.y.len() / 4);
            })));
        let c = counts.clone();
        cam.set_video_sink(Some(Arc::new(move |_f: &camera::Frame| {
            c.lock().unwrap().3 += 1;
        })));
        std::thread::sleep(Duration::from_millis(800));
        let candidates = |log: &Arc<Mutex<Vec<PcEvent>>>| -> Vec<IceCandidateInit> {
            log.lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    PcEvent::IceCandidate(c) => Some(c.clone()),
                    _ => None,
                })
                .collect()
        };
        for c in candidates(&log_a) {
            let _ = wait(|d| b.add_ice_candidate(c, d));
        }
        for c in candidates(&log_b) {
            let _ = wait(|d| a.add_ice_candidate(c, d));
        }
        std::thread::sleep(Duration::from_secs(5));
        let stats = |pc: &dyn PeerConnection| -> Vec<serde_json::Value> {
            let text = wait(|d| pc.get_stats(d)).unwrap();
            serde_json::from_str::<Vec<serde_json::Value>>(&text).unwrap()
        };
        let pick = |all: &[serde_json::Value], ty: &str| -> Option<serde_json::Value> {
            all.iter()
                .find(|s| s["type"] == ty && s["kind"] == "video")
                .cloned()
        };
        let sent = pick(&stats(a.as_ref()), "outbound-rtp").expect("no outbound video");
        let got = pick(&stats(b.as_ref()), "inbound-rtp").expect("no inbound video");
        println!(
            "sender  : {} frames encoded, {}x{}, {}",
            sent["framesEncoded"],
            sent["frameWidth"],
            sent["frameHeight"],
            sent["encoderImplementation"]
        );
        println!(
            "receiver: {} frames decoded, {}x{}, {}",
            got["framesDecoded"],
            got["frameWidth"],
            got["frameHeight"],
            got["decoderImplementation"]
        );
        let (remote_frames, w, h, preview_frames) = *counts.lock().unwrap();
        println!(
            "sinks   : remote {remote_frames} frames of {w}x{h}, camera preview {preview_frames} frames"
        );
        assert!(remote_frames > 10, "remote sink saw {remote_frames} frames");
        assert!(
            preview_frames > 10,
            "preview sink saw {preview_frames} frames"
        );
        assert!(got["framesDecoded"].as_u64().unwrap_or(0) > 10, "{got}");
        assert!(got["frameWidth"].as_u64().unwrap_or(0) >= 320, "{got}");
        // Disabling the track closes the camera: the sender stops getting new frames.
        cam.set_enabled(false);
        std::thread::sleep(Duration::from_millis(500));
        let before = pick(&stats(a.as_ref()), "outbound-rtp").unwrap()["framesEncoded"]
            .as_u64()
            .unwrap();
        std::thread::sleep(Duration::from_secs(2));
        let after = pick(&stats(a.as_ref()), "outbound-rtp").unwrap()["framesEncoded"]
            .as_u64()
            .unwrap();
        println!("while disabled: {before} -> {after} frames encoded");
        assert!(after <= before + 5, "frames still flowing while disabled");
        cam.stop();
    }

    /// Needs a webcam. Teams makes several tracks on one camera at once; they must share it, and
    /// the camera must close with the last of them.
    #[test]
    #[ignore = "turns the camera on"]
    fn tracks_on_one_camera_share_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let engine = LkEngine::new();
        let a = engine
            .create_video_capture("default", VideoConstraints::default())
            .unwrap();
        let b = engine
            .create_video_capture("default", VideoConstraints::default())
            .expect("the second track could not open the camera");
        let counts = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
        for (track, n) in [&a, &b].into_iter().zip(&counts) {
            let n = n.clone();
            track.set_video_sink(Some(Arc::new(move |_f: &camera::Frame| {
                n.fetch_add(1, Ordering::Relaxed);
            })));
        }
        std::thread::sleep(Duration::from_secs(2));
        let seen: Vec<usize> = counts.iter().map(|n| n.load(Ordering::Relaxed)).collect();
        println!("frames: {seen:?}");
        assert!(seen.iter().all(|&n| n > 10), "{seen:?}");
        // One track can go while the other keeps the camera.
        a.stop();
        let before = counts[1].load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(1));
        assert!(counts[1].load(Ordering::Relaxed) > before);
        b.stop();
        std::thread::sleep(Duration::from_millis(300));
        let after = counts[1].load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            counts[1].load(Ordering::Relaxed),
            after,
            "the camera is still running"
        );
    }

    /// Needs a webcam. The same as `camera_video_reaches_a_peer`, but H.264 only, the codec the
    /// Teams service answers with.
    #[test]
    #[ignore = "turns the camera on"]
    fn h264_camera_video_reaches_a_peer() {
        let engine = LkEngine::new();
        let (sink_a, log_a) = fake::collecting_sink();
        let (sink_b, log_b) = fake::collecting_sink();
        let a = engine
            .create_peer_connection(PcConfig::default(), sink_a)
            .unwrap();
        let b = engine
            .create_peer_connection(PcConfig::default(), sink_b)
            .unwrap();
        let cam = engine
            .create_video_capture("default", VideoConstraints::default())
            .unwrap();
        let t = a.add_transceiver(Kind::Video, Direction::SendOnly).unwrap();
        t.set_sender_track(Some(cam.clone())).unwrap();
        let h264: Vec<CodecCapability> = engine
            .capabilities()
            .send_video
            .codecs
            .into_iter()
            .filter(|c| c.mime_type == "video/H264")
            .collect();
        assert!(!h264.is_empty());
        t.set_codec_preferences(h264).unwrap();
        let offer = wait(|d| a.create_offer(d)).unwrap();
        assert!(
            offer.sdp.contains("H264/90000") && !offer.sdp.contains("VP8/90000"),
            "{}",
            offer.sdp
        );
        wait(|d| a.set_local_description(offer.clone(), d)).unwrap();
        wait(|d| b.set_remote_description(offer, d)).unwrap();
        let answer = wait(|d| b.create_answer(d)).unwrap();
        wait(|d| b.set_local_description(answer.clone(), d)).unwrap();
        wait(|d| a.set_remote_description(answer, d)).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let candidates = |log: &Arc<Mutex<Vec<PcEvent>>>| -> Vec<IceCandidateInit> {
            log.lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    PcEvent::IceCandidate(c) => Some(c.clone()),
                    _ => None,
                })
                .collect()
        };
        for c in candidates(&log_a) {
            let _ = wait(|d| b.add_ice_candidate(c, d));
        }
        for c in candidates(&log_b) {
            let _ = wait(|d| a.add_ice_candidate(c, d));
        }
        std::thread::sleep(Duration::from_secs(5));
        let video = |pc: &dyn PeerConnection, ty: &str| -> serde_json::Value {
            let text = wait(|d| pc.get_stats(d)).unwrap();
            serde_json::from_str::<Vec<serde_json::Value>>(&text)
                .unwrap()
                .into_iter()
                .find(|s| s["type"] == ty && s["kind"] == "video")
                .unwrap_or_else(|| panic!("no {ty} video stats"))
        };
        let sent = video(a.as_ref(), "outbound-rtp");
        let got = video(b.as_ref(), "inbound-rtp");
        println!(
            "sender  : {} frames, {}x{}, encoder {}",
            sent["framesEncoded"],
            sent["frameWidth"],
            sent["frameHeight"],
            sent["encoderImplementation"]
        );
        println!(
            "receiver: {} frames, {}x{}, decoder {}",
            got["framesDecoded"],
            got["frameWidth"],
            got["frameHeight"],
            got["decoderImplementation"]
        );
        assert!(got["framesDecoded"].as_u64().unwrap_or(0) > 10, "{got}");
        let encoder = sent["encoderImplementation"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();
        assert!(!encoder.contains("vpx"), "not H.264: {encoder}");
        cam.stop();
    }

    #[test]
    fn bad_sdp_is_a_parse_failure() {
        let engine = LkEngine::new();
        let (sink, _) = fake::collecting_sink();
        let pc = engine
            .create_peer_connection(PcConfig::default(), sink)
            .unwrap();
        let err = wait(|d| {
            pc.set_remote_description(
                SessionDescription {
                    kind: SdpType::Offer,
                    sdp: "garbage".into(),
                },
                d,
            )
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::SdpParse);
        assert!(
            err.message.starts_with("SDP parse failure"),
            "{}",
            err.message
        );
    }
}
