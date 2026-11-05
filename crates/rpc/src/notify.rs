//! `playNotifyAudio` and `pauseNotifyAudio`: the ringing and call-ended sounds. Teams has the
//! Windows client play them from a URL (`https://…/Teams_Call_Ringing.mp3`) on the output device
//! it names, and `pauseNotifyAudio` stops one by its `audioId`:
//!
//! ```json
//! {"audioId":"vdi_web_audio_0","uri":"https://…/Teams_Call_Ringing.mp3","sinkId":"alsa_output…","loop":true}
//! ```
//!
//! This client does not fetch the URL (the host is not trusted to name what the client downloads).
//! It plays a local file instead: `<notify dir>/<file name of the URL>` if it exists, else the file
//! in `TEAMS_REDIR_NOTIFY_FILE`. The notify dir is `TEAMS_REDIR_NOTIFY_DIR`, or `notify-audio` in
//! the state directory. With neither (and `TEAMS_REDIR_BUILTIN_TONES` not `off`), Teams' three sounds (`ring.mp3`, `Teams_Call_Ringing.mp3` and
//! `Teams_Call_Ended.mp3`) get a plain tone made here (`tones`), and any other call fails with "not
//! found" and the sound is skipped.
//!
//! The mp3 is decoded here (symphonia) and played by `paplay`, which PipeWire and PulseAudio both
//! serve, so nothing else is linked. The reply carries no `result`.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use proto::envelope::{self, RpcRequest};
use proto::hresult;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::config::Config;
use crate::devices::{DeviceKind, DeviceSource, resolve};
use crate::peerconnection::failure;
use crate::redirector::Outcome;

pub const PLAY: &str = "playNotifyAudio";
pub const PAUSE: &str = "pauseNotifyAudio";

/// Decoded audio, interleaved signed 16-bit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pcm {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

pub trait Player: Send {
    fn stop(&mut self);
}

pub trait Playback: Send + Sync {
    /// Starts playing on `sink` (the default output when `None`), repeating while `looped`.
    fn start(
        &self,
        pcm: Arc<Pcm>,
        sink: Option<&str>,
        looped: bool,
    ) -> Result<Box<dyn Player>, String>;
}

pub struct Notifier {
    dir: PathBuf,
    file: Option<PathBuf>,
    builtin_tones: bool,
    playback: Arc<dyn Playback>,
    cache: HashMap<PathBuf, Arc<Pcm>>,
    /// Sounds that were started, by `audioId`.
    playing: HashMap<String, Box<dyn Player>>,
}

impl Notifier {
    pub fn new(config: &Config) -> Notifier {
        Notifier::with_playback(config, Arc::new(PaplayPlayback))
    }

    pub fn with_playback(config: &Config, playback: Arc<dyn Playback>) -> Notifier {
        Notifier {
            dir: config.notify_dir.clone(),
            file: config.notify_file.clone(),
            builtin_tones: config.builtin_tones,
            playback,
            cache: HashMap::new(),
            playing: HashMap::new(),
        }
    }

    pub fn handle(&mut self, devices: &dyn DeviceSource, req: &RpcRequest) -> Option<Outcome> {
        match req.name.as_str() {
            PLAY => Some(self.play(devices, req)),
            PAUSE => Some(self.pause(req)),
            _ => None,
        }
    }

    fn play(&mut self, devices: &dyn DeviceSource, req: &RpcRequest) -> Outcome {
        let args = req.args_array().first().unwrap_or(&serde_json::Value::Null);
        let (Some(audio_id), Some(uri)) = (args["audioId"].as_str(), args["uri"].as_str()) else {
            return failure(req, hresult::E_INVALIDARG, "missing audioId or uri");
        };
        let looped = args["loop"].as_bool().unwrap_or(false);
        // Playing an id again replaces it.
        if let Some(mut old) = self.playing.remove(audio_id) {
            old.stop();
        }
        let (pcm, source) = match self.find(uri) {
            Some(path) => match self.load(&path) {
                Ok(p) => (p, path.display().to_string()),
                Err(e) => {
                    return failure(req, hresult::E_FAIL, &format!("{}: {e}", path.display()));
                }
            },
            None => match file_name(uri)
                .filter(|_| self.builtin_tones)
                .and_then(crate::tones::builtin)
            {
                Some(p) => (Arc::new(p), "the built-in tone".to_owned()),
                None => {
                    return failure(
                        req,
                        hresult::ERROR_NOT_FOUND,
                        &format!(
                            "no local sound for {} (put it in {} or set TEAMS_REDIR_NOTIFY_FILE)",
                            file_name(uri).unwrap_or("?"),
                            self.dir.display()
                        ),
                    );
                }
            },
        };
        let sink = args["sinkId"].as_str().and_then(|id| {
            let list = devices.list().ok()?;
            resolve(&list, DeviceKind::AudioOutput, id).map(|d| d.id)
        });
        match self.playback.start(pcm, sink.as_deref(), looped) {
            Ok(player) => {
                self.playing.insert(audio_id.to_owned(), player);
                Outcome {
                    replies: envelope::ok_empty(req).into_iter().collect(),
                    note: Some(format!(
                        "playing {source} as {audio_id} on {}{}",
                        sink.as_deref().unwrap_or("the default output"),
                        if looped { ", looping" } else { "" }
                    )),
                }
            }
            Err(e) => failure(req, hresult::E_FAIL, &format!("cannot play: {e}")),
        }
    }

    /// Stopping a sound that already ended, or never started, is not an error.
    fn pause(&mut self, req: &RpcRequest) -> Outcome {
        let audio_id = req
            .args_array()
            .first()
            .and_then(|a| a["audioId"].as_str())
            .unwrap_or("");
        let note = match self.playing.remove(audio_id) {
            Some(mut p) => {
                p.stop();
                format!("stopped {audio_id}")
            }
            None => format!("{audio_id} was not playing"),
        };
        Outcome {
            replies: envelope::ok_empty(req).into_iter().collect(),
            note: Some(note),
        }
    }

    fn find(&self, uri: &str) -> Option<PathBuf> {
        file_name(uri)
            .map(|n| self.dir.join(n))
            .filter(|p| p.is_file())
            .or_else(|| self.file.clone().filter(|p| p.is_file()))
    }

    fn load(&mut self, path: &Path) -> Result<Arc<Pcm>, String> {
        if let Some(p) = self.cache.get(path) {
            return Ok(p.clone());
        }
        let pcm = Arc::new(decode_mp3(path)?);
        self.cache.insert(path.to_owned(), pcm.clone());
        Ok(pcm)
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        for p in self.playing.values_mut() {
            p.stop();
        }
    }
}

/// The last path segment of a URL, without query or fragment, if it is a plain file name. Nothing
/// that could leave the notify dir is accepted.
fn file_name(uri: &str) -> Option<&str> {
    let path = uri.split(['?', '#']).next()?;
    let name = path.rsplit('/').next()?;
    let plain = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    plain.then_some(name)
}

pub fn decode_mp3(path: &Path) -> Result<Pcm, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("not an mp3: {e}"))?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;
    let (mut rate, mut channels) = (0, 0);
    let mut samples = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // The end of the stream is an I/O error of kind `UnexpectedEof`.
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = *buf.spec();
                rate = spec.rate;
                channels = spec.channels.count() as u16;
                let mut copy = SampleBuffer::<i16>::new(buf.capacity() as u64, spec);
                copy.copy_interleaved_ref(buf);
                samples.extend_from_slice(copy.samples());
            }
            // A damaged frame is skipped, as players do.
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    if samples.is_empty() || rate == 0 || channels == 0 {
        return Err("no audio decoded".into());
    }
    Ok(Pcm {
        rate,
        channels,
        samples,
    })
}

/// Plays through `paplay`, fed from a thread.
pub struct PaplayPlayback;

impl Playback for PaplayPlayback {
    fn start(
        &self,
        pcm: Arc<Pcm>,
        sink: Option<&str>,
        looped: bool,
    ) -> Result<Box<dyn Player>, String> {
        let mut cmd = Command::new("paplay");
        cmd.args([
            "--raw",
            "--format=s16le",
            &format!("--rate={}", pcm.rate),
            &format!("--channels={}", pcm.channels),
            "--client-name=teams-redir",
            "--stream-name=Teams notification",
        ]);
        if let Some(sink) = sink {
            cmd.arg(format!("--device={sink}"));
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run paplay: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("no paplay stdin")?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let feeder = std::thread::Builder::new()
            .name("teams-redir-notify".into())
            .spawn(move || {
                let bytes: Vec<u8> = pcm.samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                'play: loop {
                    for chunk in bytes.chunks(16 * 1024) {
                        if flag.load(Ordering::SeqCst) || stdin.write_all(chunk).is_err() {
                            break 'play;
                        }
                    }
                    if !looped {
                        break;
                    }
                }
                // Closing stdin lets paplay finish what it has buffered.
            })
            .map_err(|e| e.to_string())?;
        Ok(Box::new(PaplayPlayer {
            child,
            stop,
            feeder: Some(feeder),
        }))
    }
}

struct PaplayPlayer {
    child: Child,
    stop: Arc<AtomicBool>,
    feeder: Option<JoinHandle<()>>,
}

impl Player for PaplayPlayer {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(f) = self.feeder.take() {
            let _ = f.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::devices::fake::*;

    const TONE: &[u8] = include_bytes!("../tests/data/tone-0.3s.mp3");

    #[derive(Default)]
    struct FakePlayback {
        started: Mutex<Vec<(usize, Option<String>, bool)>>,
        stopped: Arc<Mutex<Vec<usize>>>,
    }

    struct FakePlayer(usize, Arc<Mutex<Vec<usize>>>);

    impl Player for FakePlayer {
        fn stop(&mut self) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    impl Playback for FakePlayback {
        fn start(
            &self,
            pcm: Arc<Pcm>,
            sink: Option<&str>,
            looped: bool,
        ) -> Result<Box<dyn Player>, String> {
            let mut started = self.started.lock().unwrap();
            started.push((pcm.samples.len(), sink.map(str::to_owned), looped));
            Ok(Box::new(FakePlayer(started.len(), self.stopped.clone())))
        }
    }

    struct Rig {
        notifier: Notifier,
        playback: Arc<FakePlayback>,
        dir: PathBuf,
        devices: FakeDevices,
    }

    fn rig(name: &str, with_file: bool) -> Rig {
        let dir =
            std::env::temp_dir().join(format!("teams-redir-notify-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if with_file {
            std::fs::write(dir.join("Teams_Call_Ringing.mp3"), TONE).unwrap();
        }
        let cfg_dir = dir.clone();
        let config = Config::from_lookup(move |k| {
            (k == "TEAMS_REDIR_NOTIFY_DIR").then(|| cfg_dir.to_string_lossy().into_owned())
        });
        let playback = Arc::new(FakePlayback::default());
        Rig {
            notifier: Notifier::with_playback(&config, playback.clone()),
            playback,
            dir,
            devices: FakeDevices(Ok(vec![speaker("sink1", true), speaker("sink2", false)])),
        }
    }

    fn call(r: &mut Rig, json: &str) -> serde_json::Value {
        let req = envelope::parse_request(json).unwrap();
        let o = r.notifier.handle(&r.devices, &req).unwrap();
        serde_json::from_str(&o.replies[0]).unwrap()
    }

    fn play(id: &str, uri: &str, sink: &str, looped: bool) -> String {
        format!(
            r#"{{"rpcCallId":1,"rpcObjectType":"RDWebRTCRedirector","rpcName":"playNotifyAudio","rpcArgs":[{{"audioId":"{id}","uri":"{uri}","sinkId":"{sink}","loop":{looped}}}]}}"#
        )
    }

    fn pause(id: &str) -> String {
        format!(
            r#"{{"rpcCallId":2,"rpcObjectType":"RDWebRTCRedirector","rpcName":"pauseNotifyAudio","rpcArgs":[{{"audioId":"{id}"}}]}}"#
        )
    }

    #[test]
    fn the_mp3_decodes() {
        let path =
            std::env::temp_dir().join(format!("teams-redir-tone-{}.mp3", std::process::id()));
        std::fs::write(&path, TONE).unwrap();
        let pcm = decode_mp3(&path).unwrap();
        assert_eq!((pcm.rate, pcm.channels), (22050, 1));
        assert!(pcm.samples.len() > 5000, "{} samples", pcm.samples.len());
        assert!(pcm.samples.iter().any(|s| s.abs() > 1000), "silent");
        std::fs::write(&path, b"not an mp3 at all").unwrap();
        assert!(decode_mp3(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_sound_is_found_by_the_name_in_the_url_and_stopped_by_id() {
        let mut r = rig("play", true);
        let uri = "https://teams.public.onecdn.static.microsoft/evergreen-assets/audio/Teams_Call_Ringing.mp3?v=1";
        let reply = call(&mut r, &play("vdi_web_audio_0", uri, "sink2", true));
        assert_eq!(reply["hr"], json!(0));
        assert!(reply.get("result").is_none());
        let started = r.playback.started.lock().unwrap().clone();
        assert_eq!(started.len(), 1);
        assert_eq!(
            (started[0].1.as_deref(), started[0].2),
            (Some("sink2"), true)
        );
        let reply = call(&mut r, &pause("vdi_web_audio_0"));
        assert_eq!(reply["hr"], json!(0));
        assert_eq!(*r.playback.stopped.lock().unwrap(), [1]);
        // Pausing it again, or something unknown, is fine and stops nothing more.
        assert_eq!(call(&mut r, &pause("vdi_web_audio_0"))["hr"], json!(0));
        assert_eq!(r.playback.stopped.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn playing_an_id_again_replaces_it_and_an_unknown_sink_uses_the_default() {
        let mut r = rig("again", true);
        let uri = "https://x.example/Teams_Call_Ringing.mp3";
        call(&mut r, &play("a", uri, "sink1", false));
        call(&mut r, &play("a", uri, "no-such-sink", false));
        assert_eq!(*r.playback.stopped.lock().unwrap(), [1]);
        assert_eq!(r.playback.started.lock().unwrap()[1].1, None);
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn a_missing_sound_is_not_found_unless_a_fallback_file_is_set() {
        let mut r = rig("missing", false);
        let reply = call(
            &mut r,
            &play("a", "https://x.example/Chime.mp3", "sink1", false),
        );
        assert_eq!(reply["hr"], json!(hresult::ERROR_NOT_FOUND as i32));
        assert!(
            reply["errorDetails"]
                .as_str()
                .unwrap()
                .contains("Chime.mp3")
        );
        assert!(r.playback.started.lock().unwrap().is_empty());
        // The fallback plays whatever Teams asks for.
        let fallback = r.dir.join("any.mp3");
        std::fs::write(&fallback, TONE).unwrap();
        r.notifier.file = Some(fallback);
        let reply = call(
            &mut r,
            &play("a", "https://x.example/Chime.mp3", "sink1", false),
        );
        assert_eq!(reply["hr"], json!(0));
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn the_sounds_teams_names_have_a_built_in_tone_when_there_is_no_file() {
        let mut r = rig("builtin", false);
        let uri = "https://x.example/evergreen-assets/audio/ring.mp3";
        assert_eq!(
            call(&mut r, &play("ring", uri, "sink1", true))["hr"],
            json!(0)
        );
        let started = r.playback.started.lock().unwrap();
        assert_eq!(started.len(), 1);
        assert!(started[0].2, "the ring loops");
        drop(started);
        // A file in the notify dir wins.
        std::fs::write(r.dir.join("ring.mp3"), TONE).unwrap();
        call(&mut r, &play("ring2", uri, "sink1", true));
        assert_eq!(r.playback.started.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn a_bad_file_or_arguments_fail_without_playing() {
        let mut r = rig("bad", true);
        std::fs::write(r.dir.join("Teams_Call_Ringing.mp3"), b"junk").unwrap();
        let reply = call(
            &mut r,
            &play(
                "a",
                "https://x.example/Teams_Call_Ringing.mp3",
                "sink1",
                false,
            ),
        );
        assert_eq!(reply["hr"], json!(hresult::E_FAIL as i32));
        let reply = call(
            &mut r,
            r#"{"rpcCallId":3,"rpcObjectType":"RDWebRTCRedirector","rpcName":"playNotifyAudio","rpcArgs":[{}]}"#,
        );
        assert_eq!(reply["errorDetails"], json!("missing audioId or uri"));
        assert!(r.playback.started.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn only_plain_file_names_are_accepted() {
        assert_eq!(
            file_name("https://h/a/Teams_Call_Ringing.mp3?x=1#y"),
            Some("Teams_Call_Ringing.mp3")
        );
        assert_eq!(file_name("https://h/a/.."), None);
        assert_eq!(file_name("https://h/a/"), None);
        assert_eq!(file_name("https://h/a/b%2F..%2Fc.mp3"), None);
        assert_eq!(file_name("../../etc/passwd"), Some("passwd"));
        assert_eq!(file_name("file name.mp3"), None);
    }

    /// Plays a real file on this machine's default output for a moment. Run it by hand:
    /// `TEAMS_REDIR_TEST_MP3=file.mp3 cargo test -p teams-redir-rpc plays_a_real_file -- --ignored`.
    #[test]
    #[ignore = "makes sound"]
    fn plays_a_real_file() {
        let path = std::env::var("TEAMS_REDIR_TEST_MP3").expect("TEAMS_REDIR_TEST_MP3");
        let pcm = Arc::new(decode_mp3(Path::new(&path)).unwrap());
        println!(
            "{} Hz, {} channels, {} samples",
            pcm.rate,
            pcm.channels,
            pcm.samples.len()
        );
        let mut player = PaplayPlayback.start(pcm, None, true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        player.stop();
    }
}
