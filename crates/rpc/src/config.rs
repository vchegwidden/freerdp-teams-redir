//! Configuration comes from environment variables, so the phase 1 experiments (plan §3.2) can
//! be switched without a rebuild:
//!
//! | Variable | Values |
//! |---|---|
//! | `TEAMS_REDIR_DISABLE` | `1`: listen on neither channel (the runtime off switch, plan §9) |
//! | `TEAMS_REDIR_CPC` | `honest` (default, run B), `macstore` (run C), `off` (run A: no listener) |
//! | `TEAMS_REDIR_STATE_DIR` | default `$XDG_STATE_HOME/teams-redir`, else `~/.local/state/teams-redir` |
//! | `TEAMS_REDIR_TRANSCRIPTS` | a directory, or `off`; default `<state dir>/transcripts` |
//! | `TEAMS_REDIR_CLIENT_PLATFORM`, `_APP_NAME`, `_APP_VERSION`, `_FLAVOR`, `_WEBRTC`, `_WEBRTC_REDIRECTOR` | `sessioninfo.version` strings (run D) |

use std::fs;
use std::path::{Path, PathBuf};

use proto::errc;
use serde_json::Value;

use crate::host::Channel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpcMode {
    /// Do not listen on `cpc.1` at all.
    Off,
    /// `ClientHello` with Linux values and `no_plugin`.
    Honest,
    /// `ClientHello` with the macOS Store values (`mac-arm64`, `client_app_store_build`).
    MacStore,
}

/// The strings sent in `sessioninfo.version` and the `ClientHello`.
#[derive(Debug, Clone)]
pub struct Identity {
    pub client_platform: String,
    pub client_app_name: String,
    pub client_app_version: String,
    pub client_flavor: String,
    pub web_rtc: String,
    pub web_rtc_redirector: String,
    pub plugin_version: String,
    /// `ClientHello.platform`.
    pub cpc_platform: String,
    pub vdi_vendor: i32,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub cpc: CpcMode,
    pub state_dir: PathBuf,
    pub transcript_dir: Option<PathBuf>,
    /// Where `playNotifyAudio` looks for a sound by the file name of its URL.
    pub notify_dir: PathBuf,
    /// One sound for every notification, when none is found in `notify_dir`.
    pub notify_file: Option<PathBuf>,
    /// Whether the sounds Teams names get a tone made here when no file for them exists
    /// (`TEAMS_REDIR_BUILTIN_TONES=off` turns it off).
    pub builtin_tones: bool,
    /// Whether a ringing call shows a desktop notification when nothing in the state directory
    /// says otherwise (`call-notifications`, see `callalert`).
    pub call_alerts: bool,
    /// File names of the sounds that mean an incoming call; empty means any looping sound that is
    /// not known to be the caller's own ringback.
    pub call_alert_sounds: Vec<String>,
    /// Whether video is offered at all (`TEAMS_REDIR_VIDEO=off` turns it off).
    pub video: bool,
    /// The video window helper, if named; else it is looked for next to this module and in `PATH`.
    pub video_helper: Option<PathBuf>,
    pub identity: Identity,
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn cpc_platform() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "linux-x64",
        "aarch64" => "linux-arm64",
        _ => "linux",
    }
}

impl Config {
    pub fn from_env() -> Config {
        Config::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Config {
        let var = |k: &str| get(k).filter(|v| !v.is_empty());
        let cpc = match var("TEAMS_REDIR_CPC").as_deref() {
            Some("off") => CpcMode::Off,
            Some("macstore") => CpcMode::MacStore,
            _ => CpcMode::Honest,
        };
        let state_dir = var("TEAMS_REDIR_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if let Some(x) = var("XDG_STATE_HOME") {
                    PathBuf::from(x).join("teams-redir")
                } else if let Some(h) = var("HOME") {
                    PathBuf::from(h).join(".local/state/teams-redir")
                } else {
                    PathBuf::from("/tmp/teams-redir")
                }
            });
        let transcript_dir = match var("TEAMS_REDIR_TRANSCRIPTS").as_deref() {
            Some("off") => None,
            Some(dir) => Some(PathBuf::from(dir)),
            None => Some(state_dir.join("transcripts")),
        };
        let notify_dir = var("TEAMS_REDIR_NOTIFY_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.join("notify-audio"));
        let notify_file = var("TEAMS_REDIR_NOTIFY_FILE").map(PathBuf::from);
        let builtin_tones = !matches!(
            var("TEAMS_REDIR_BUILTIN_TONES").as_deref(),
            Some("off" | "0" | "false" | "no")
        );
        let call_alerts = !matches!(
            var("TEAMS_REDIR_CALL_NOTIFICATIONS").as_deref(),
            Some("off" | "0" | "false" | "no")
        );
        let call_alert_sounds = var("TEAMS_REDIR_CALL_ALERT_SOUNDS")
            .map(|v| {
                v.split(',')
                    .map(|n| n.trim().to_owned())
                    .filter(|n| !n.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let video = !matches!(
            var("TEAMS_REDIR_VIDEO").as_deref(),
            Some("off" | "0" | "false" | "no")
        );
        let video_helper = var("TEAMS_REDIR_VIDEO_HELPER").map(PathBuf::from);
        let mac = cpc == CpcMode::MacStore;
        let pick = |k: &str, default: &str| var(k).unwrap_or_else(|| default.to_owned());
        let identity = Identity {
            client_platform: pick("TEAMS_REDIR_CLIENT_PLATFORM", "linux"),
            client_app_name: pick("TEAMS_REDIR_APP_NAME", "teams-redir"),
            client_app_version: pick("TEAMS_REDIR_APP_VERSION", VERSION),
            client_flavor: pick("TEAMS_REDIR_FLAVOR", "freerdp"),
            web_rtc: pick("TEAMS_REDIR_WEBRTC", "none"),
            web_rtc_redirector: pick("TEAMS_REDIR_WEBRTC_REDIRECTOR", VERSION),
            plugin_version: if mac {
                "2026.23.01.1".into()
            } else {
                VERSION.into()
            },
            cpc_platform: if mac {
                "mac-arm64".into()
            } else {
                cpc_platform().into()
            },
            vdi_vendor: 5,
        };
        Config {
            enabled: var("TEAMS_REDIR_DISABLE").as_deref() != Some("1"),
            cpc,
            state_dir,
            transcript_dir,
            notify_dir,
            notify_file,
            builtin_tones,
            call_alerts,
            call_alert_sounds,
            video,
            video_helper,
            identity,
        }
    }

    /// The `errc` sent as `load_errc` and `install_errc` for this mode.
    pub fn cpc_errc(&self) -> i32 {
        match self.cpc {
            CpcMode::MacStore => errc::CLIENT_APP_STORE_BUILD,
            _ => errc::NO_PLUGIN,
        }
    }

    pub fn wants_channel(&self, channel: Channel) -> bool {
        self.enabled
            && match channel {
                Channel::Webrtc => true,
                Channel::Cpc => self.cpc != CpcMode::Off,
            }
    }

    /// A stable UUID per install, created on first use. Falls back to a fresh one per run if the
    /// state directory is not writable.
    pub fn endpoint_id(&self) -> String {
        let path = self.state_dir.join("endpoint-id");
        if let Ok(s) = fs::read_to_string(&path) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_owned();
            }
        }
        let id = new_uuid();
        if fs::create_dir_all(&self.state_dir).is_ok() {
            let _ = fs::write(&path, &id);
        }
        id
    }
}

fn new_uuid() -> String {
    fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "00000000-0000-4000-8000-000000000000".to_owned())
}

/// What the shim reads from FreeRDP's settings. Passed as JSON:
/// `{"display":{"width":W,"height":H},"client":{"name":"FreeRDP","version":"3.x"}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfo {
    pub width: u32,
    pub height: u32,
    pub client_name: String,
    pub client_version: String,
}

impl SessionInfo {
    pub fn from_json(text: &str) -> SessionInfo {
        let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let num = |p: &str| {
            v.pointer(p)
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0)
        };
        let s = |p: &str| {
            v.pointer(p)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        SessionInfo {
            width: num("/display/width"),
            height: num("/display/height"),
            client_name: s("/client/name"),
            client_version: s("/client/version"),
        }
    }
}

/// `/proc/self/exe` basename, for `rdClientProcessName`.
pub fn process_name() -> Option<String> {
    let exe = fs::read_link("/proc/self/exe").ok()?;
    Path::new(&exe)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// Kernel release, for `clientOsVersion`.
pub fn os_version() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Flatpak and Snap both sandbox the client; `rdClientSandboxed` reports it.
pub fn is_sandboxed() -> bool {
    Path::new("/.flatpak-info").exists() || std::env::var_os("SNAP").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn defaults_are_honest_linux() {
        let c = cfg(&[("HOME", "/home/x")]);
        assert!(c.enabled);
        assert_eq!(c.cpc, CpcMode::Honest);
        assert_eq!(c.cpc_errc(), errc::NO_PLUGIN);
        assert_eq!(
            c.state_dir,
            PathBuf::from("/home/x/.local/state/teams-redir")
        );
        assert_eq!(
            c.transcript_dir,
            Some(PathBuf::from(
                "/home/x/.local/state/teams-redir/transcripts"
            ))
        );
        assert_eq!(c.identity.client_platform, "linux");
    }

    #[test]
    fn experiment_switches() {
        let c = cfg(&[("TEAMS_REDIR_CPC", "off")]);
        assert!(!c.wants_channel(Channel::Cpc));
        assert!(c.wants_channel(Channel::Webrtc));

        let c = cfg(&[("TEAMS_REDIR_CPC", "macstore")]);
        assert_eq!(c.cpc_errc(), errc::CLIENT_APP_STORE_BUILD);
        assert_eq!(c.identity.cpc_platform, "mac-arm64");

        let c = cfg(&[("TEAMS_REDIR_DISABLE", "1")]);
        assert!(!c.wants_channel(Channel::Webrtc));
        assert!(!c.wants_channel(Channel::Cpc));

        let c = cfg(&[
            ("TEAMS_REDIR_TRANSCRIPTS", "off"),
            ("TEAMS_REDIR_WEBRTC", "133.0"),
        ]);
        assert_eq!(c.transcript_dir, None);
        assert_eq!(c.identity.web_rtc, "133.0");
    }

    #[test]
    fn session_info_parses_and_tolerates_junk() {
        let s = SessionInfo::from_json(
            r#"{"display":{"width":1920,"height":1080},"client":{"name":"FreeRDP","version":"3.32.2"}}"#,
        );
        assert_eq!(
            (s.width, s.height, s.client_name.as_str()),
            (1920, 1080, "FreeRDP")
        );
        assert_eq!(SessionInfo::from_json("garbage"), SessionInfo::default());
    }
}
