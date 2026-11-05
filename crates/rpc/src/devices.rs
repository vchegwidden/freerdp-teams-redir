//! Audio devices on the Linux client, as the host's `MediaDevices.enumerateDevices` wants them.
//!
//! [`PactlDevices`] asks the sound server through `pactl`, which both PipeWire (pipewire-pulse) and
//! PulseAudio answer. It is a stand-in for the native PipeWire code that `media-linux` will bring
//! (plan §4.1): the [`DeviceSource`] trait is what stays. Ids are the server's node names, which
//! survive restarts, so they are stable across calls as the plan asks (§5.1).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    AudioInput,
    AudioOutput,
    VideoInput,
}

impl DeviceKind {
    /// The W3C `MediaDeviceInfo.kind` string.
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceKind::AudioInput => "audioinput",
            DeviceKind::AudioOutput => "audiooutput",
            DeviceKind::VideoInput => "videoinput",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub kind: DeviceKind,
    pub label: String,
    pub group_id: String,
    pub is_default: bool,
}

pub trait DeviceSource: Send + Sync {
    /// Capture and playback devices. `Err` carries a message for the log.
    fn list(&self) -> Result<Vec<Device>, String>;
}

/// The `result` array of `MediaDevices.enumerateDevices`, in the order the Windows add-in builds it
/// (`msrdc-reply-structures.md` §2) [O]: a `communications` and a `default` entry for output and
/// for input, then every real audio device. Cameras follow, each once: video has no virtual
/// entries. An entry is `{deviceId, groupId, kind, label}`, nothing
/// more, and a kind with no default device has no virtual entries.
///
/// Linux has no separate communications role, so `communications` repeats the default device.
pub fn media_device_infos(devices: &[Device]) -> Value {
    let default_of = |kind| devices.iter().find(|d| d.kind == kind && d.is_default);
    let mut out = Vec::new();
    for (id, prefix) in [("communications", "Communications"), ("default", "Default")] {
        for kind in [DeviceKind::AudioOutput, DeviceKind::AudioInput] {
            if let Some(d) = default_of(kind) {
                out.push(info(
                    id,
                    kind,
                    &format!("{prefix} - {}", d.label),
                    &d.group_id,
                ));
            }
        }
    }
    for kind in [DeviceKind::AudioOutput, DeviceKind::AudioInput] {
        for d in devices.iter().filter(|d| d.kind == kind) {
            out.push(info(&d.id, kind, &d.label, &d.group_id));
        }
    }
    for d in devices.iter().filter(|d| d.kind == DeviceKind::VideoInput) {
        out.push(info(&d.id, d.kind, &d.label, &d.group_id));
    }
    Value::Array(out)
}

/// The device a `deviceId` names: a node name, or `default` / `communications` (and no id at all)
/// for the default one.
pub fn resolve(devices: &[Device], kind: DeviceKind, id: &str) -> Option<Device> {
    let of_kind = || devices.iter().filter(|d| d.kind == kind);
    if id.is_empty() || id == "default" || id == "communications" {
        return of_kind()
            .find(|d| d.is_default)
            .or_else(|| of_kind().next())
            .cloned();
    }
    of_kind().find(|d| d.id == id).cloned()
}

fn info(id: &str, kind: DeviceKind, label: &str, group_id: &str) -> Value {
    json!({
        "deviceId": id,
        "groupId": group_guid(group_id),
        "kind": kind.as_str(),
        "label": label,
    })
}

/// Windows reports the device's container id, a braced upper-case GUID, as `groupId`. Nothing
/// parses it, but it should look like one, so the sound server's group name is hashed into a
/// stable GUID-shaped string (FNV-1a over two lanes).
fn group_guid(group: &str) -> String {
    let mut lanes = [0xcbf2_9ce4_8422_2325u64, 0x8422_2325_cbf2_9ce4u64];
    for (i, b) in group.bytes().enumerate() {
        let lane = &mut lanes[i % 2];
        *lane = (*lane ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let (a, b) = (lanes[0], lanes[1]);
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a as u16,
        (b >> 48) as u16,
        b & 0xffff_ffff_ffff
    )
}

/// What this machine has: sound devices from the sound server and cameras from V4L2. A machine
/// with no sound server still lists its cameras and the other way round.
#[derive(Default)]
pub struct SystemDevices;

impl DeviceSource for SystemDevices {
    fn list(&self) -> Result<Vec<Device>, String> {
        let cameras = camera::list().into_iter().map(|c| Device {
            id: c.id,
            kind: DeviceKind::VideoInput,
            label: c.label,
            group_id: c.group,
            is_default: false,
        });
        match PactlDevices.list() {
            Ok(mut devices) => {
                devices.extend(cameras);
                Ok(devices)
            }
            Err(e) => {
                let cameras: Vec<Device> = cameras.collect();
                if cameras.is_empty() {
                    Err(e)
                } else {
                    Ok(cameras)
                }
            }
        }
    }
}

#[derive(Default)]
pub struct PactlDevices;

impl DeviceSource for PactlDevices {
    fn list(&self) -> Result<Vec<Device>, String> {
        let mut devices = Vec::new();
        for (kind, what, default_cmd) in [
            (DeviceKind::AudioInput, "sources", "get-default-source"),
            (DeviceKind::AudioOutput, "sinks", "get-default-sink"),
        ] {
            let json = pactl(&["-f", "json", "list", what])?;
            // A missing default is not an error; the list is still useful.
            let default = pactl(&[default_cmd]).unwrap_or_default();
            devices.extend(parse_pactl(kind, &json, default.trim())?);
        }
        Ok(devices)
    }
}

/// Parses `pactl -f json list sources|sinks`. Monitors of outputs are not microphones and are
/// left out.
pub fn parse_pactl(
    kind: DeviceKind,
    json: &str,
    default_name: &str,
) -> Result<Vec<Device>, String> {
    let list: Value = serde_json::from_str(json).map_err(|e| format!("pactl output: {e}"))?;
    let entries = list.as_array().ok_or("pactl output is not a list")?;
    let mut out = Vec::new();
    for e in entries {
        let Some(name) = e["name"].as_str() else {
            continue;
        };
        let is_monitor =
            e["monitor_of_sink"].is_string() || e["properties"]["device.class"] == "monitor";
        if kind == DeviceKind::AudioInput && is_monitor {
            continue;
        }
        // An unplugged jack or an HDMI output without a monitor. The default is kept so that the
        // `default` entries always point at a listed device.
        if name != default_name && port_unavailable(e) {
            continue;
        }
        let label = e["description"].as_str().unwrap_or(name);
        let group_id = e["properties"]["device.id"]
            .as_str()
            .or(e["properties"]["alsa.card"].as_str())
            .unwrap_or(name);
        out.push(Device {
            id: name.to_string(),
            kind,
            label: label.to_string(),
            group_id: group_id.to_string(),
            is_default: name == default_name,
        });
    }
    Ok(out)
}

/// True when the device's active port is reported as `not available`. `availability unknown` (a
/// built-in speaker, say) and a missing port list count as available.
fn port_unavailable(entry: &Value) -> bool {
    let Some(active) = entry["active_port"].as_str() else {
        return false;
    };
    entry["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|p| p["name"] == active && p["availability"] == "not available")
}

/// Runs `pactl` with a deadline: a sound server that is gone must not stall the dispatcher.
fn pactl(args: &[&str]) -> Result<String, String> {
    const DEADLINE: Duration = Duration::from_secs(3);
    let mut child = Command::new("pactl")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run pactl: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no pactl stdout")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < DEADLINE => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("pactl {} timed out", args.join(" ")));
            }
            Err(e) => return Err(format!("pactl: {e}")),
        }
    };
    let text = reader.join().unwrap_or_default();
    if status.success() {
        Ok(text)
    } else {
        Err(format!("pactl {} failed: {status}", args.join(" ")))
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;

    /// A fixed device list, or a failure.
    pub struct FakeDevices(pub Result<Vec<Device>, String>);

    impl DeviceSource for FakeDevices {
        fn list(&self) -> Result<Vec<Device>, String> {
            self.0.clone()
        }
    }

    pub fn mic(id: &str, default: bool) -> Device {
        Device {
            id: id.into(),
            kind: DeviceKind::AudioInput,
            label: format!("Mic {id}"),
            group_id: "g1".into(),
            is_default: default,
        }
    }

    pub fn camera(id: &str) -> Device {
        Device {
            id: id.into(),
            kind: DeviceKind::VideoInput,
            label: format!("Camera {id}"),
            group_id: format!("cam-{id}"),
            is_default: false,
        }
    }

    pub fn speaker(id: &str, default: bool) -> Device {
        Device {
            kind: DeviceKind::AudioOutput,
            label: format!("Speaker {id}"),
            ..mic(id, default)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    const SOURCES: &str = r#"[
      {"name":"out.monitor","description":"Monitor of Speaker","monitor_of_sink":"out","properties":{"device.class":"monitor"}},
      {"name":"in.mic","description":"Built-in mic","monitor_of_sink":null,"properties":{"device.id":"48"}},
      {"name":"in.headset","description":"Headset","properties":{"alsa.card":"1"}}
    ]"#;

    #[test]
    fn monitors_are_not_inputs_and_the_default_is_marked() {
        let d = parse_pactl(DeviceKind::AudioInput, SOURCES, "in.headset").unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].id, "in.mic");
        assert_eq!(d[0].group_id, "48");
        assert!(!d[0].is_default);
        assert_eq!(d[1].group_id, "1");
        assert!(d[1].is_default);
    }

    #[test]
    fn outputs_keep_their_monitors_out_of_the_way_only_for_inputs() {
        let sinks = r#"[{"name":"out","description":"Speaker","properties":{}}]"#;
        let d = parse_pactl(DeviceKind::AudioOutput, sinks, "").unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].label, "Speaker");
        assert_eq!(d[0].group_id, "out");
    }

    #[test]
    fn unavailable_ports_are_hidden_unless_they_are_the_default() {
        let sinks = r#"[
          {"name":"spk","description":"Speaker","active_port":"s","ports":[{"name":"s","availability":"availability unknown"}]},
          {"name":"hp","description":"Headphones","active_port":"h","ports":[{"name":"h","availability":"not available"}]},
          {"name":"hdmi","description":"HDMI","active_port":"d","ports":[{"name":"d","availability":"not available"}]},
          {"name":"plain","description":"No ports","properties":{}}
        ]"#;
        let ids = |default: &str| -> Vec<String> {
            parse_pactl(DeviceKind::AudioOutput, sinks, default)
                .unwrap()
                .into_iter()
                .map(|d| d.id)
                .collect()
        };
        assert_eq!(ids("spk"), ["spk", "plain"]);
        assert_eq!(ids("hdmi"), ["spk", "hdmi", "plain"]);
    }

    #[test]
    fn bad_pactl_output_is_an_error() {
        assert!(parse_pactl(DeviceKind::AudioInput, "nope", "").is_err());
        assert!(parse_pactl(DeviceKind::AudioInput, "{}", "").is_err());
    }

    #[test]
    fn infos_follow_the_windows_order() {
        let list = vec![
            mic("a", false),
            mic("b", true),
            speaker("s", true),
            speaker("t", false),
        ];
        let v = media_device_infos(&list);
        let ids: Vec<(&str, &str)> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["kind"].as_str().unwrap(), e["deviceId"].as_str().unwrap()))
            .collect();
        assert_eq!(
            ids,
            [
                ("audiooutput", "communications"),
                ("audioinput", "communications"),
                ("audiooutput", "default"),
                ("audioinput", "default"),
                ("audiooutput", "s"),
                ("audiooutput", "t"),
                ("audioinput", "a"),
                ("audioinput", "b"),
            ]
        );
        assert_eq!(v[0]["label"], json!("Communications - Speaker s"));
        assert_eq!(v[3]["label"], json!("Default - Mic b"));
    }

    #[test]
    fn entries_have_exactly_the_four_windows_keys() {
        let v = media_device_infos(&[mic("a", true)]);
        let mut keys: Vec<&str> = v[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["deviceId", "groupId", "kind", "label"]);
    }

    #[test]
    fn group_ids_are_stable_guid_shaped_and_shared_within_a_group() {
        let g = group_guid("48");
        assert_eq!(g, group_guid("48"));
        assert_ne!(g, group_guid("49"));
        assert_eq!(g.len(), 38);
        assert!(g.starts_with('{') && g.ends_with('}'));
        let parts: Vec<usize> = g[1..37].split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
        assert!(
            g[1..37]
                .chars()
                .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
        );
    }

    #[test]
    fn device_ids_resolve_to_devices() {
        let list = vec![mic("a", false), mic("b", true), speaker("s", true)];
        assert_eq!(resolve(&list, DeviceKind::AudioInput, "a").unwrap().id, "a");
        assert_eq!(
            resolve(&list, DeviceKind::AudioInput, "default")
                .unwrap()
                .id,
            "b"
        );
        assert_eq!(resolve(&list, DeviceKind::AudioInput, "").unwrap().id, "b");
        assert_eq!(
            resolve(&list, DeviceKind::AudioOutput, "communications")
                .unwrap()
                .id,
            "s"
        );
        assert!(resolve(&list, DeviceKind::AudioInput, "s").is_none());
        assert!(resolve(&list, DeviceKind::AudioInput, "nope").is_none());
    }

    #[test]
    fn cameras_come_after_the_audio_devices_without_virtual_entries() {
        let list = vec![
            camera("c1"),
            mic("m", true),
            speaker("s", true),
            camera("c2"),
        ];
        let v = media_device_infos(&list);
        let all: Vec<(&str, &str)> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["kind"].as_str().unwrap(), e["deviceId"].as_str().unwrap()))
            .collect();
        assert_eq!(
            &all[all.len() - 2..],
            [("videoinput", "c1"), ("videoinput", "c2")]
        );
        assert_eq!(all.iter().filter(|(k, _)| *k == "videoinput").count(), 2);
        assert_eq!(v[all.len() - 2]["label"], json!("Camera c1"));
        // The first camera is what "default" means.
        assert_eq!(
            resolve(&list, DeviceKind::VideoInput, "default")
                .unwrap()
                .id,
            "c1"
        );
        assert_eq!(
            resolve(&list, DeviceKind::VideoInput, "c2").unwrap().id,
            "c2"
        );
    }

    #[test]
    fn no_default_means_no_virtual_entries() {
        let v = media_device_infos(&[mic("a", false)]);
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["deviceId"], json!("a"));
    }
}
