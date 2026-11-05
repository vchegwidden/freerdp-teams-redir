//! JSONL message log. Every RPC in and out goes here (plan §2.6): the plaintext RPC was never
//! visible in the network captures, so this is where it can be read, and the replay tool turns
//! saved transcripts into regression tests.
//!
//! One line per message:
//! `{"t":<unix ms>,"dir":"in"|"out","ch":"webrtc"|"cpc","conn":N,"len":N,"msg":"<text>"}`.
//! `conn` numbers the connection, since the host opens several `webrtc.1` channels at once.
//! `msg` is the message text exactly as framed, without the trailing NUL.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
}

impl Dir {
    pub fn as_str(self) -> &'static str {
        match self {
            Dir::In => "in",
            Dir::Out => "out",
        }
    }
}

pub struct Transcript {
    out: BufWriter<File>,
    path: PathBuf,
}

impl Transcript {
    /// Creates `<dir>/<unix seconds>-<pid>.jsonl`, and the directory if needed.
    pub fn create(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = dir.join(format!("{secs}-{}.jsonl", std::process::id()));
        let out = BufWriter::new(File::create(&path)?);
        Ok(Transcript { out, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes one line and flushes it, since a crash is when the log is most wanted.
    pub fn record(&mut self, dir: Dir, channel: &str, conn: u32, data: &[u8]) -> io::Result<()> {
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        let msg = String::from_utf8_lossy(&data[..end]);
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let line = json!({ "t": t, "dir": dir.as_str(), "ch": channel, "conn": conn, "len": data.len(), "msg": msg });
        writeln!(self.out, "{line}")?;
        self.out.flush()
    }
}

/// One parsed transcript line, for the replay tool.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub t: u64,
    pub dir: Dir,
    pub channel: String,
    /// 0 in transcripts written before connections were numbered.
    pub conn: u32,
    pub msg: String,
}

pub fn parse_line(line: &str) -> Option<Entry> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let dir = match v.get("dir")?.as_str()? {
        "in" => Dir::In,
        "out" => Dir::Out,
        _ => return None,
    };
    Some(Entry {
        t: v.get("t").and_then(|t| t.as_u64()).unwrap_or(0),
        dir,
        channel: v.get("ch")?.as_str()?.to_owned(),
        conn: v
            .get("conn")
            .and_then(|c| c.as_u64())
            .and_then(|c| u32::try_from(c).ok())
            .unwrap_or(0),
        msg: v.get("msg")?.as_str()?.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_parses_back() {
        let dir =
            std::env::temp_dir().join(format!("teams-redir-transcript-{}", std::process::id()));
        let mut t = Transcript::create(&dir).unwrap();
        t.record(Dir::In, "webrtc", 3, b"{\"a\":1}\0").unwrap();
        t.record(Dir::Out, "cpc", 4, b"{\"b\":2}").unwrap();
        let text = fs::read_to_string(t.path()).unwrap();
        let entries: Vec<_> = text.lines().filter_map(parse_line).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            (
                entries[0].dir,
                entries[0].channel.as_str(),
                entries[0].msg.as_str()
            ),
            (Dir::In, "webrtc", "{\"a\":1}")
        );
        assert_eq!(
            (
                entries[1].dir,
                entries[1].channel.as_str(),
                entries[1].msg.as_str()
            ),
            (Dir::Out, "cpc", "{\"b\":2}")
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
