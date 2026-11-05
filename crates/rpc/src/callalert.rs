//! A desktop notification when a call comes in.
//!
//! The add-in is never told "a call is incoming": what it sees is Teams asking it to play a sound
//! (`playNotifyAudio`), and an incoming call rings through that same path, with `loop: true`. So
//! a looping sound starts an alert and `pauseNotifyAudio` for the same `audioId` closes it. The
//! caller's own ringback (`Teams_Call_Ringing.mp3`, seen on an outgoing test call) is looping too
//! and must not alert; `TEAMS_REDIR_CALL_ALERT_SOUNDS` (comma-separated file names) turns the
//! rule around to "only these sounds", for when the name of the incoming ringtone is known.
//! Teams sends no caller name or number to the client, so the text is generic.
//!
//! On or off: `TEAMS_REDIR_CALL_NOTIFICATIONS=off` sets the default, and the file
//! `<state dir>/call-notifications` (`on` or `off`) overrides it and is read at every ring, so
//! `tools/call-notifications.sh` switches it without restarting anything.
//!
//! Backends: on Linux and the BSDs the freedesktop notification service, which mako, dunst,
//! GNOME, KDE and the others implement, through `notify-send` or, without it, `gdbus`; on macOS
//! `osascript`. Nothing is linked and a missing tool only means no notification.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use proto::envelope::RpcRequest;

use crate::config::Config;
use crate::notify::{PAUSE, PLAY};

/// The file whose content (`on` / `off`) overrides the configured default.
pub const SWITCH_FILE: &str = "call-notifications";

/// Sounds known to be the caller's side of a call, never an incoming one.
const OUTGOING_SOUNDS: &[&str] = &["Teams_Call_Ringing.mp3"];

/// How long the notification stays when nothing closes it: Teams rings for about 30 seconds.
const TIMEOUT_MS: u32 = 40_000;

const TITLE: &str = "Incoming Teams call";
const BODY: &str = "Teams in your remote desktop session is ringing.";

pub trait AlertBackend: Send + Sync {
    /// Shows a notification and returns its id if the service gave one. Blocks while the tool
    /// runs, so it is called from a thread.
    fn show(&self, title: &str, body: &str, timeout_ms: u32) -> Result<Option<u32>, String>;
    fn close(&self, id: u32);
}

/// What one ring has shown.
struct Alert {
    id: Mutex<Option<u32>>,
    /// Set when the ring stopped before the service had answered with an id.
    closed: AtomicBool,
}

pub struct CallAlerts {
    default_on: bool,
    sounds: Vec<String>,
    switch: PathBuf,
    backend: Arc<dyn AlertBackend>,
    active: HashMap<String, Arc<Alert>>,
}

impl CallAlerts {
    pub fn new(config: &Config) -> CallAlerts {
        CallAlerts::with_backend(config, default_backend())
    }

    pub fn with_backend(config: &Config, backend: Arc<dyn AlertBackend>) -> CallAlerts {
        CallAlerts {
            default_on: config.call_alerts,
            sounds: config.call_alert_sounds.clone(),
            switch: config.state_dir.join(SWITCH_FILE),
            backend,
            active: HashMap::new(),
        }
    }

    /// Looks at a `playNotifyAudio` or `pauseNotifyAudio` request. Returns a line for the log
    /// when it did something.
    pub fn observe(&mut self, req: &RpcRequest) -> Option<String> {
        let args = req.args_array().first()?;
        let audio_id = args["audioId"].as_str()?;
        match req.name.as_str() {
            PLAY => self.on_play(audio_id, args),
            PAUSE => self.on_pause(audio_id),
            _ => None,
        }
    }

    /// The switch file wins over the configured default. Anything but a clear "off" is on.
    pub fn enabled(&self) -> bool {
        match std::fs::read_to_string(&self.switch) {
            Ok(text) => !matches!(
                text.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no"
            ),
            Err(_) => self.default_on,
        }
    }

    fn is_incoming_ring(&self, args: &serde_json::Value) -> bool {
        if args["loop"].as_bool() != Some(true) {
            return false;
        }
        let name = args["uri"]
            .as_str()
            .and_then(|u| u.split(['?', '#']).next())
            .and_then(|u| u.rsplit('/').next())
            .unwrap_or("");
        if self.sounds.is_empty() {
            !OUTGOING_SOUNDS.contains(&name)
        } else {
            self.sounds.iter().any(|s| s == name)
        }
    }

    fn on_play(&mut self, audio_id: &str, args: &serde_json::Value) -> Option<String> {
        if !self.is_incoming_ring(args) {
            return None;
        }
        if !self.enabled() {
            return Some("call alert skipped: call notifications are off".into());
        }
        let alert = Arc::new(Alert {
            id: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        if let Some(old) = self.active.insert(audio_id.to_owned(), alert.clone()) {
            close(&*self.backend, &old);
        }
        let backend = self.backend.clone();
        // The tools take a moment, and the dispatcher must not wait for them.
        let spawned = std::thread::Builder::new()
            .name("teams-redir-alert".into())
            .spawn(move || {
                if let Ok(id) = backend.show(TITLE, BODY, TIMEOUT_MS) {
                    *alert.id.lock().unwrap() = id;
                    // The ring may already be over by the time the service answered.
                    if alert.closed.load(Ordering::SeqCst)
                        && let Some(id) = id
                    {
                        backend.close(id);
                    }
                }
            });
        match spawned {
            Ok(_) => Some(format!("call alert shown for {audio_id}")),
            Err(e) => Some(format!("call alert not shown: {e}")),
        }
    }

    fn on_pause(&mut self, audio_id: &str) -> Option<String> {
        let alert = self.active.remove(audio_id)?;
        close(&*self.backend, &alert);
        Some(format!("call alert closed for {audio_id}"))
    }
}

fn close(backend: &dyn AlertBackend, alert: &Alert) {
    alert.closed.store(true, Ordering::SeqCst);
    let id = *alert.id.lock().unwrap();
    if let Some(id) = id {
        backend.close(id);
    }
}

#[cfg(target_os = "macos")]
fn default_backend() -> Arc<dyn AlertBackend> {
    Arc::new(MacBackend)
}

#[cfg(not(target_os = "macos"))]
fn default_backend() -> Arc<dyn AlertBackend> {
    Arc::new(FreedesktopBackend)
}

/// The freedesktop notification service, which every Linux desktop's notification daemon speaks.
pub struct FreedesktopBackend;

impl AlertBackend for FreedesktopBackend {
    fn show(&self, title: &str, body: &str, timeout_ms: u32) -> Result<Option<u32>, String> {
        // `notify-send -p` prints the notification id, which is what closing needs.
        let sent = Command::new("notify-send")
            .args([
                "--print-id",
                "--app-name=Teams",
                "--icon=call-start",
                "--category=im.received",
                &format!("--expire-time={timeout_ms}"),
                title,
                body,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match sent {
            Ok(out) if out.status.success() => {
                return Ok(String::from_utf8_lossy(&out.stdout).trim().parse().ok());
            }
            // An old libnotify has no `--print-id`: show it without, and it just cannot be closed.
            Ok(_) => {
                let plain = Command::new("notify-send")
                    .args([
                        "--app-name=Teams",
                        &format!("--expire-time={timeout_ms}"),
                        title,
                        body,
                    ])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                if plain.is_ok_and(|s| s.success()) {
                    return Ok(None);
                }
            }
            Err(_) => {}
        }
        gdbus_notify(title, body, timeout_ms)
    }

    fn close(&self, id: u32) {
        let _ = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest=org.freedesktop.Notifications",
                "--object-path=/org/freedesktop/Notifications",
                "--method=org.freedesktop.Notifications.CloseNotification",
                &id.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// `Notify` called by hand, for systems without `notify-send`. `gdbus` prints `(uint32 7,)`.
fn gdbus_notify(title: &str, body: &str, timeout_ms: u32) -> Result<Option<u32>, String> {
    let out = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest=org.freedesktop.Notifications",
            "--object-path=/org/freedesktop/Notifications",
            "--method=org.freedesktop.Notifications.Notify",
            "Teams",
            "0",
            "call-start",
            title,
            body,
            "[]",
            "{}",
            &timeout_ms.to_string(),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("no notify-send and no gdbus: {e}"))?;
    if !out.status.success() {
        return Err("the notification service did not accept the notification".into());
    }
    Ok(parse_gdbus_id(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_gdbus_id(text: &str) -> Option<u32> {
    text.trim()
        .strip_prefix("(uint32 ")?
        .split([',', ')'])
        .next()?
        .trim()
        .parse()
        .ok()
}

/// macOS Notification Center through `osascript`. It has no way to close one again.
#[cfg(target_os = "macos")]
pub struct MacBackend;

#[cfg(target_os = "macos")]
impl AlertBackend for MacBackend {
    fn show(&self, title: &str, body: &str, _timeout_ms: u32) -> Result<Option<u32>, String> {
        let script = format!("display notification {body:?} with title {title:?}");
        Command::new("osascript")
            .args(["-e", &script])
            .stdin(Stdio::null())
            .status()
            .map_err(|e| e.to_string())?;
        Ok(None)
    }

    fn close(&self, _id: u32) {}
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use proto::envelope;

    use super::*;

    #[derive(Default)]
    struct FakeBackend {
        shown: Mutex<Vec<(String, String, u32)>>,
        closed: Mutex<Vec<u32>>,
        next: Mutex<u32>,
        /// Delay before `show` answers, to race a quick pause against it.
        delay_ms: u64,
    }

    impl AlertBackend for FakeBackend {
        fn show(&self, title: &str, body: &str, timeout_ms: u32) -> Result<Option<u32>, String> {
            std::thread::sleep(Duration::from_millis(self.delay_ms));
            self.shown
                .lock()
                .unwrap()
                .push((title.into(), body.into(), timeout_ms));
            let mut n = self.next.lock().unwrap();
            *n += 1;
            Ok(Some(*n))
        }

        fn close(&self, id: u32) {
            self.closed.lock().unwrap().push(id);
        }
    }

    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timed out: {what}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct Rig {
        alerts: CallAlerts,
        backend: Arc<FakeBackend>,
        dir: PathBuf,
    }

    fn rig(name: &str, env: &[(&str, &str)], delay_ms: u64) -> Rig {
        let dir =
            std::env::temp_dir().join(format!("teams-redir-alert-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.to_string_lossy().into_owned();
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let config = Config::from_lookup(move |k| {
            if k == "TEAMS_REDIR_STATE_DIR" {
                return Some(state.clone());
            }
            env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        });
        let backend = Arc::new(FakeBackend {
            delay_ms,
            ..FakeBackend::default()
        });
        Rig {
            alerts: CallAlerts::with_backend(&config, backend.clone()),
            backend,
            dir,
        }
    }

    fn play(file: &str, looped: bool) -> RpcRequest {
        envelope::parse_request(&format!(
            r#"{{"rpcCallId":1,"rpcObjectType":"RDWebRTCRedirector","rpcName":"playNotifyAudio","rpcArgs":[{{"audioId":"a1","uri":"https://h/audio/{file}","sinkId":"s","loop":{looped}}}]}}"#
        ))
        .unwrap()
    }

    fn pause() -> RpcRequest {
        envelope::parse_request(
            r#"{"rpcCallId":2,"rpcObjectType":"RDWebRTCRedirector","rpcName":"pauseNotifyAudio","rpcArgs":[{"audioId":"a1"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_looping_ringtone_alerts_and_stopping_it_closes_the_alert() {
        let mut r = rig("ring", &[], 0);
        assert!(
            r.alerts
                .observe(&play("Teams_Call_Incoming.mp3", true))
                .is_some()
        );
        wait_for("shown", || r.backend.shown.lock().unwrap().len() == 1);
        assert_eq!(r.backend.shown.lock().unwrap()[0].0, "Incoming Teams call");
        assert!(r.alerts.observe(&pause()).unwrap().contains("closed"));
        assert_eq!(*r.backend.closed.lock().unwrap(), [1]);
        // A second pause has nothing to close.
        assert!(r.alerts.observe(&pause()).is_none());
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn the_callers_own_ringback_and_one_shot_sounds_do_not_alert() {
        let mut r = rig("quiet", &[], 0);
        assert!(
            r.alerts
                .observe(&play("Teams_Call_Ringing.mp3", true))
                .is_none()
        );
        assert!(
            r.alerts
                .observe(&play("Teams_Call_Ended.mp3", false))
                .is_none()
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(r.backend.shown.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn a_named_sound_list_turns_the_rule_around() {
        let mut r = rig(
            "named",
            &[("TEAMS_REDIR_CALL_ALERT_SOUNDS", "Ring_Me.mp3, Other.mp3")],
            0,
        );
        assert!(
            r.alerts
                .observe(&play("Teams_Call_Incoming.mp3", true))
                .is_none()
        );
        assert!(r.alerts.observe(&play("Ring_Me.mp3", true)).is_some());
        wait_for("shown", || r.backend.shown.lock().unwrap().len() == 1);
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn the_switch_file_beats_the_default_and_is_read_every_time() {
        let mut r = rig("switch", &[("TEAMS_REDIR_CALL_NOTIFICATIONS", "off")], 0);
        assert!(!r.alerts.enabled());
        assert!(
            r.alerts
                .observe(&play("Ring.mp3", true))
                .unwrap()
                .contains("off")
        );
        std::fs::write(r.dir.join(SWITCH_FILE), "on\n").unwrap();
        assert!(r.alerts.enabled());
        assert!(
            r.alerts
                .observe(&play("Ring.mp3", true))
                .unwrap()
                .contains("shown")
        );
        wait_for("shown", || r.backend.shown.lock().unwrap().len() == 1);
        std::fs::write(r.dir.join(SWITCH_FILE), "OFF").unwrap();
        assert!(!r.alerts.enabled());
        std::fs::remove_file(r.dir.join(SWITCH_FILE)).unwrap();
        assert!(!r.alerts.enabled(), "back to the configured default, off");
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn a_ring_that_stops_before_the_service_answers_is_closed_afterwards() {
        let mut r = rig("race", &[], 150);
        r.alerts.observe(&play("Ring.mp3", true));
        r.alerts.observe(&pause());
        wait_for("closed after show", || {
            r.backend.closed.lock().unwrap().len() == 1
        });
        assert_eq!(*r.backend.closed.lock().unwrap(), [1]);
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn gdbus_answers_are_parsed() {
        assert_eq!(parse_gdbus_id("(uint32 7,)\n"), Some(7));
        assert_eq!(parse_gdbus_id("(uint32 1234,)"), Some(1234));
        assert_eq!(parse_gdbus_id("nonsense"), None);
    }

    /// Shows one notification on this desktop, then closes it. Run by hand:
    /// `cargo test -p teams-redir-rpc shows_on_this_desktop -- --ignored --nocapture`.
    #[test]
    #[ignore = "shows a notification"]
    fn shows_on_this_desktop() {
        let backend = FreedesktopBackend;
        let id = backend.show(TITLE, BODY, 10_000).unwrap();
        println!("notification id: {id:?}");
        assert!(id.is_some(), "the notification service gave no id");
        std::thread::sleep(Duration::from_millis(2500));
        backend.close(id.unwrap());
    }
}
