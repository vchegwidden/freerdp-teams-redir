//! `replay <transcript.jsonl>`: feeds the inbound lines of a saved transcript to a fresh core and
//! prints what it sends back. Golden-transcript comparison comes once reference transcripts exist
//! (plan §3.3); until then this shows how the current handlers answer a recorded session.
//!
//! Configuration is the same environment variables as the plugin. Transcripts are not written
//! unless `TEAMS_REDIR_TRANSCRIPTS` names a directory.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{env, fs};

use std::collections::BTreeSet;

use rpc::{Channel, Config, ConnId, Core, Host, LogLevel, SessionInfo};
use transcript::{Dir, parse_line};

#[derive(Default)]
struct PrintHost {
    closed: AtomicBool,
}

impl Host for PrintHost {
    fn write(&self, conn: ConnId, data: &[u8]) -> bool {
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        println!("OUT conn {conn} {}", String::from_utf8_lossy(&data[..end]));
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
        println!("IN  conn {conn} {} {}", entry.channel, entry.msg);
        core.data(conn, entry.msg.as_bytes());
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
