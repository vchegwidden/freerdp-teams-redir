//! `replay <transcript.jsonl>`: feeds the inbound lines of a saved transcript to a fresh core and
//! prints what it sends back. Golden-transcript comparison comes once reference transcripts exist
//! (plan §3.3); until then this shows how the current handlers answer a recorded session.
//!
//! Configuration is the same environment variables as the plugin. Transcripts are not written
//! unless `TEAMS_REDIR_TRANSCRIPTS` names a directory.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{env, fs};

use std::collections::BTreeSet;

use rpc::{Channel, Config, ConnId, Core, Host, LogLevel, SessionInfo};
use transcript::{Dir, parse_line};

#[derive(Default)]
struct PrintHost {
    closed: AtomicBool,
    /// The newest offer or answer this run produced, as the JSON text it was sent in.
    last_desc: Mutex<Option<String>>,
}

impl Host for PrintHost {
    fn write(&self, conn: ConnId, data: &[u8]) -> bool {
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        let text = String::from_utf8_lossy(&data[..end]);
        if text.contains("\"desc\"") && text.contains("a=fingerprint") {
            *self.last_desc.lock().unwrap() = Some(text.to_string());
        }
        println!("OUT conn {conn} {text}");
        true
    }

    fn close(&self, _: ConnId) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn log(&self, level: LogLevel, msg: &str) {
        if level >= LogLevel::Warn {
            eprintln!("log[{level:?}] {msg}");
        }
    }
}

/// The value after `prefix` in the JSON text of an SDP, up to its next (escaped) line end.
fn sdp_value<'a>(json: &'a str, prefix: &str) -> Option<&'a str> {
    let start = json.find(prefix)? + prefix.len();
    let len = json[start..].find("\\r")?;
    Some(&json[start..start + len])
}

/// A recorded `setLocalDescription` carries the DTLS fingerprint and ICE credentials of the run
/// that was recorded, and libwebrtc refuses them in this run. Swap in this run's.
fn adopt_identity(msg: &str, last_desc: &str) -> String {
    let mut out = msg.to_string();
    for prefix in ["a=fingerprint:sha-256 ", "a=ice-ufrag:", "a=ice-pwd:"] {
        if let (Some(old), Some(new)) = (sdp_value(msg, prefix), sdp_value(last_desc, prefix)) {
            out = out.replace(&format!("{prefix}{old}"), &format!("{prefix}{new}"));
        }
    }
    out
}

fn channel(name: &str) -> Option<Channel> {
    match name {
        "webrtc" => Some(Channel::Webrtc),
        "cpc" => Some(Channel::Cpc),
        _ => None,
    }
}

/// A transcript replay has no network, so the fake engine answers the calls, unless the real one
/// is asked for (and built in) to see what libwebrtc and the sound devices say.
fn engine() -> Arc<dyn media::MediaEngine> {
    #[cfg(feature = "libwebrtc")]
    if env::var("TEAMS_REDIR_REPLAY_ENGINE").as_deref() == Ok("libwebrtc") {
        return Arc::new(media::lk::LkEngine::new());
    }
    Arc::new(media::fake::FakeEngine::default())
}

fn main() -> ExitCode {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: replay <transcript.jsonl>");
        return ExitCode::from(2);
    };
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::from(2);
        }
    };

    let mut config = Config::from_env();
    if env::var_os("TEAMS_REDIR_TRANSCRIPTS").is_none() {
        config.transcript_dir = None;
    }
    let host = Arc::new(PrintHost::default());
    let session = SessionInfo {
        width: 1920,
        height: 1080,
        client_name: "replay".into(),
        client_version: "0".into(),
    };
    let core = Core::new(host.clone(), config, session, engine());

    let mut fed = 0usize;
    let mut opened = BTreeSet::new();
    for entry in text
        .lines()
        .filter_map(parse_line)
        .filter(|e| e.dir == Dir::In)
    {
        let Some(ch) = channel(&entry.channel) else {
            continue;
        };
        // Transcripts written before connections were numbered have conn 0 throughout; give each
        // channel its own id so they still replay.
        let conn = if entry.conn == 0 {
            u32::MAX - ch as u32
        } else {
            entry.conn
        };
        if opened.insert(conn) {
            core.channel_opened(conn, ch);
        }
        let msg = if entry.msg.contains("\"setLocalDescription\"") {
            // The offer being applied is still on its way out of the core.
            let mut desc = host.last_desc.lock().unwrap().clone();
            for _ in 0..100 {
                if desc.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                desc = host.last_desc.lock().unwrap().clone();
            }
            match desc {
                Some(desc) => adopt_identity(&entry.msg, &desc),
                None => entry.msg.clone(),
            }
        } else {
            entry.msg.clone()
        };
        println!("IN  conn {conn} {} {msg}", entry.channel);
        core.data(conn, msg.as_bytes());
        core.flush();
        fed += 1;
    }
    drop(core);

    eprintln!("replayed {fed} inbound messages");
    if host.closed.load(Ordering::SeqCst) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
