# Development notes

Working notes from building the add-in: what a real AVD host sends, how the wire format was
established, experiments that were run, and where the code departs from the original plan. They are
kept as written. Where they cite `msrdc-reply-structures.md`, the implementation plan or other
analysis documents, those are private working files that are not part of this repository.

The user-facing documentation is in the top-level [README](../README.md).

---

## Layout

| Path | What |
|---|---|
| `shim/shim.c` | The FreeRDP entry point `DVCPluginEntry`; listeners for `webrtc.1` and `cpc.1`; copies messages to Rust and writes replies back. |
| `crates/proto` | NUL framing, the `webrtc.1` envelope, `cpc.1` `VMHello` parser and `ClientHello` builder, error-code encoding. |
| `crates/rpc` | The dispatcher thread, configuration, the `setVersionInfo` / `sessioninfo` handler, devices, and `RTCPeerConnection` (`peerconnection.rs`). |
| `crates/media` | The `MediaEngine` / `PeerConnection` traits; `lk` (libwebrtc through LiveKit's bindings, feature `libwebrtc`) and `fake` (tests, replay). |
| `crates/transcript` | JSONL log of every message in and out. |
| `crates/ffi` | The `extern "C"` interface (`tr_core_*`); `cbindgen` generates `tr_core.h`. |
| `crates/replay` | Feeds a saved transcript to a fresh core and prints the replies. |
| `tools/` | `ns-run.sh` and `e2e.sh` (below). |
| `golden/` | Reference transcripts for the replay tests (none yet, plan §3.3). |

## Build and test

```
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build
cargo test --workspace        # protocol, dispatcher, C interface
ctest --test-dir build        # the built .so, driven through FreeRDP's DVC interface with a mock manager
tools/e2e.sh build/libteams-redir-client.so   # real sdl-freerdp3 + local shadow server, headless
```

Needs `freerdp` 3.x with headers, `rust`, `cbindgen`, `cmake`, `clang` (Arch: `pacman -S freerdp rust cbindgen`).

The add-in links libwebrtc statically (LiveKit's prebuilt `webrtc-a545de4`, 148 MB, about 700 MB
unpacked). Cargo downloads it into each target directory on the first build, which is slow and
wasteful with several build trees. To share one copy, unpack the release
`webrtc-linux-x64-release.zip` from `github.com/livekit/rust-sdks` once and point
`LK_CUSTOM_WEBRTC` at the directory that holds `include/` and `lib/` before running cmake or cargo.
`cargo build -p teams-redir-ffi --no-default-features` builds with a fake engine and downloads
nothing, for protocol work. libwebrtc brings its own BoringSSL; the version script and
`--exclude-libs` keep it from being seen by FreeRDP's OpenSSL, and `shim_test` runs a real peer
connection in the same process as FreeRDP.
The module exports only `DVCPluginEntry` (checked with `nm -D --defined-only`).

## Running it

`/dvc:teams-redir` makes FreeRDP load `libteams-redir-client.so` from `<prefix>/lib/freerdp3`. The
prefix is compiled in (`/usr` on Arch), so a development build cannot be loaded from elsewhere.
Install with `sudo install -D build/libteams-redir-client.so /usr/lib/freerdp3/`, or run through
`tools/ns-run.sh <module> <command>`, which overlays `/usr/lib` in a private user namespace and
changes nothing on disk.

```
sdl-freerdp3 /v:<host> ... /dvc:teams-redir
```

Logs go to wLog under `com.freerdp.channels.teams-redir`; raise it with `WLOG_LEVEL=DEBUG`. Message
transcripts go to `~/.local/state/teams-redir/transcripts/`.

## Using it with avd_feed_connect

`avd_feed_connect` (`~/Dev/avd_feed_connect`) launches `$AVD_SDL_FREERDP`, else `sdl-freerdp` on
`PATH`, else `~/opt/freerdp-sdl3-cam/bin/sdl-freerdp` (FreeRDP 3.32.1, prefix
`~/opt/freerdp-sdl3-cam`). It copies its environment to the child and appends
`AVD_EXTRA_ARGS` (or the per-resource "extra args" setting) to the command line. The Flatpak build
bundles its own FreeRDP and is not covered.

```
# build against that FreeRDP, and install into its add-in directory
PKG_CONFIG_PATH=~/opt/freerdp-sdl3-cam/lib/pkgconfig cmake -S . -B build-cam -DCMAKE_BUILD_TYPE=Release
cmake --build build-cam && ctest --test-dir build-cam
install -D build-cam/libteams-redir-client.so ~/opt/freerdp-sdl3-cam/lib/freerdp3/libteams-redir-client.so

# launch the app with the add-in on, from a shell that has the experiment variables
AVD_EXTRA_ARGS="/dvc:teams-redir" TEAMS_REDIR_CPC=honest ~/Dev/avd_feed_connect/avd-feed-connect
```

Without installing, `E2E_CLIENT=~/opt/freerdp-sdl3-cam/bin/sdl-freerdp NS_LIBDIR=~/opt/freerdp-sdl3-cam/lib tools/e2e.sh build-cam/libteams-redir-client.so`
checks the load against that exact client. Rebuild the add-in whenever that FreeRDP is rebuilt.

## Configuration (environment)

| Variable | Values |
|---|---|
| `TEAMS_REDIR_DISABLE` | `1`: listen on neither channel |
| `TEAMS_REDIR_CPC` | `honest` (default, plan run B), `macstore` (run C), `off` (run A: no `cpc.1` listener) |
| `TEAMS_REDIR_CLIENT_PLATFORM`, `_APP_NAME`, `_APP_VERSION`, `_FLAVOR`, `_WEBRTC`, `_WEBRTC_REDIRECTOR` | the `sessioninfo.version` strings (run D) |
| `TEAMS_REDIR_STATE_DIR` | endpoint id and transcripts; default `$XDG_STATE_HOME/teams-redir` |
| `TEAMS_REDIR_TRANSCRIPTS` | a directory, or `off` |

## Finding what the host checks

Teams reports "Not optimized" and the host sends nothing after our `sessioninfo`, whatever we put in
`datachannel`, the version strings or the feature flags. To bisect without a rebuild or a reconnect,
both `sessioninfo` and the `ClientHello` can be changed by a JSON merge patch that is re-read at every
handshake: `tools/experiment.sh <name>`, then restart Teams in the session. See `experiments/README.md`
for the patches, what each one asks, and the order to run them.

## Where this differs from the plan

- `tr_host_api` has a `close` callback (the panic path closes every connection), and channels are
  plain `int`s in the C interface.
- Messages are addressed by **connection id**, not channel. The first real host opened three
  `webrtc.1` connections within 3 ms at Teams start-up, so the shim accepts up to 32 per channel and
  the core keeps a separate conversation (and, later, object table) for each.
- `tr_core_wants_channel` lets the shim skip a listener, for run A and the off switch.
- The replay tool is its own crate, because the transcript crate cannot depend on `rpc` while `rpc`
  depends on it.
- `engine` and `media-linux` do not exist yet (phases 1.5 and 2).
- The shim holds a recursive lock across `IWTSVirtualChannel::Write`. Writes from the dispatcher
  thread have worked against a real host (the `ClientHello` reached it), which answers the plan's
  §2.3 question for the SDL client.

## What a real host sends (first contact, 2026-10-29)

From `golden/2026-10-29-avd-first-contact.jsonl`; host `websocketService` 1.53.2408.19001, `shim`
1.1.2602.23001. These corrected the plan:

- `setVersionInfo` is a **notification** (no `rpcCallId`, no `rpcObjectId`, no `rpcArgs`): `version`,
  `session` and `features` are top-level keys. The reply is the `sessioninfo` event only.
- `rpcCallId` is optional in general; a request without one gets no reply.
- `rpcArgs` is an array for some calls (`rtcSession`, `playNotifyAudio`) and an object for others
  (`setTeamsAppVolume`).
- `rtcSession` (a call; args: a hex id, a ms timestamp, a 64-digit hex value, the shim version) was
  not in any earlier analysis. What it means is unknown.
- `playNotifyAudio` / `pauseNotifyAudio` (client plays a notification mp3 from a URL) arrive as soon as
  Teams runs, which suggests Teams already treats the redirector as present.
- The `VMHello` URL list includes a **`linux-x64`** SlimCore package, which the plan assumed did not
  exist. Not acted on; see the plan's §0.

## Reply format (from the Windows add-in)

`msrdc-reply-structures.md` (decompiled `MsRdcWebRTCAddIn.dll`) fixed the wire format:

- A reply is `{result?, errorDetails?, rpcObjectType, rpcObjectId?, rpcName, rpcCallId, hr}`. `hr` is
  the HRESULT as a **signed decimal number** (`0x80070490` is `-2147023728`). Our earlier
  `hresult` key (and a missing `hr` on success) is why Teams ignored the `enumerateDevices` list.
- `result` is only present when the method has content. `errorDetails` is the message of a failure.
- An unknown method gets `hr` -2147024846 (`0x80070032`) and nothing else. That is also the right
  reply to `rtcSession`, which the Windows client does not implement.
- `setVersionInfo` and `setTeamsCallInfo` get no RPC reply; `setVersionInfo` is answered by the
  `sessioninfo` event. Events carry `hr: 0`.
- `MediaDevices.enumerateDevices`: `result` is a flat array of `{deviceId, groupId, kind, label}`,
  starting with `communications` and `default` entries for output and input.
- `HidManager`: `startWithDeviceFilter` / `stop` reply `"RPC succeeded."`, `enumerateDevices` `[]`.
- `sessioninfo.features.microphone` is **left out** when a capture device exists and `"disabled"`
  when none does.

## Phase 2 progress

- Audio devices come from `pactl` (`crates/rpc/src/devices.rs`; PipeWire and PulseAudio both serve
  it), behind the `DeviceSource` trait. It is a stand-in for native PipeWire code.
- `RTCPeerConnection` (`crates/rpc/src/peerconnection.rs`): `createPeerConnection` (replies with the
  engine's real audio capabilities; video is deliberately advertised empty until there is a video
  path), `createOffer`, `createAnswer`, `setLocalDescription`, `setRemoteDescription` (replies with
  the transceiver ids this side allocates, from 2^32), `addIceCandidate`, `close`, a legacy
  `getStats` with no reports, and `setConfiguration` refused like the shipped client. Events:
  `signalingstatechange`, `iceconnectionstatechange`, `connectionstatechange`,
  `icegatheringstatechange`, `negotiationneeded`, `icecandidate`, `track`.
- Audio ports that `pactl` reports as `not available` (an unplugged headset, HDMI without a
  monitor) are left out of the device list, unless they are the default.
- What Teams does when a call starts (`golden/2026-10-31-avd-test-call-requests.jsonl`, a real
  attempt with credentials redacted) and how each call is handled, in `media_objects.rs` and
  `peerconnection.rs`: `MediaElement.createMediaElement` (bookkeeping; remote audio plays through
  libwebrtc's audio device module), `createDataChannel` (and `RTCDataChannel.close` and the
  `open` / `closing` / `close` / `message` events), ten `addTransceiver` calls with host-chosen ids
  followed by `RTCRtpTransceiver.setDirection`, `MediaStream.createMediaStream` with
  `audio.deviceId` (our own `pactl` node names, which become capture tracks on the engine's
  recording device), `MediaStreamTrack.createMediaStreamTrack` / `onEnabledAttributeChanged` /
  `stop`, and `RTCRtpSender.replaceTrack`. The `the_test_call_requests_are_answered` test replays
  that file and fails if a call that should work gets the unknown-method reply.
- **The first working call** (`golden/2026-11-01-avd-test-call-echo-works-requests.jsonl`): after the
  offer fix below, the service answered `sendrecv` and Teams got ten `track` events (audio plus nine
  receive-only video), then called `MediaElement.setSinkId`, `setAttribute("srcObject", {stream id
  we allocated})` and `play`. Our client-allocated stream and track ids (from 2^32) were accepted
  as they are. The only non-success replies were `rtcSession` (the Windows client answers it the
  same way) and a `removeTrack` sent after the connection had closed.
- `vendor/libwebrtc` is LiveKit's crate with three changes (`vendor/README.md`): `set_direction`,
  the raw stats JSON, and offers that no longer strip the receive bit from every transceiver (they
  used to turn `sendrecv` into `sendonly`, so a call could never receive audio).
- Known gaps: the bindings expose only *current* descriptions, so the `desc` in `icecandidate`
  events is the description last set, without the gathered candidates; `createPeerConnection`
  ignores bundle/rtcp-mux policies and port ranges; recording and playout devices are
  process-wide, so two streams on different microphones would share the last one selected.
- Second test call (`golden/2026-10-31-avd-test-call-2-requests.jsonl`, SDP secrets redacted): the
  call connected (ICE and DTLS, data channel open) and Teams then used `RTCDataChannel.send` (`data`
  is an array of byte values; it opens with a `syn` handshake), polled `getStats` with the
  `RTCPeerConnection` selector every second, and ended with `removeTrack`. All three are handled
  now: stats are libwebrtc's own JSON report (`vendor/`), `getStats` for an unknown object answers
  the string `"{}"`, and a sender or receiver selector gets the connection-wide report.
- `playNotifyAudio` / `pauseNotifyAudio` (`crates/rpc/src/notify.rs`): the ringing and call-ended
  tones. For now they play **local files only**: the URL Teams sends is never fetched, only its file
  name is used to pick `<state dir>/notify-audio/<name>.mp3` (`TEAMS_REDIR_NOTIFY_DIR` moves the
  directory, `TEAMS_REDIR_NOTIFY_FILE` is one fallback file for every sound). The mp3 is decoded
  in Rust and played with `paplay` on the sink Teams names, looping while `loop` is true.
  Without a local file the call fails with "not found" and the tone is skipped. The two Teams
  tones are `Teams_Call_Ringing.mp3` and `Teams_Call_Ended.mp3`.
- Incoming-call notification (`crates/rpc/src/callalert.rs`): the add-in is not told that a call is
  incoming; it sees Teams ask for a *looping* sound (`playNotifyAudio` with `loop: true`), which is
  how a ring arrives, and the stop (`pauseNotifyAudio`, same `audioId`) closes the notification. The
  caller's own ringback (`Teams_Call_Ringing.mp3`, seen on an outgoing call) does not alert, and
  `TEAMS_REDIR_CALL_ALERT_SOUNDS=name1.mp3,name2.mp3` limits it to named sounds once the real
  ringtone's name is known. **No incoming call has been seen yet**, so this rule is a best guess;
  Teams sends no caller name to the client, so the text is generic. It uses the freedesktop
  notification service (mako, dunst, GNOME, KDE, …) through `notify-send`, or `gdbus` without it;
  on macOS `osascript` (untested). A missing tool only means no notification.
  Switch it with `tools/call-notifications.sh on|off|toggle|status|test`: that writes
  `<state dir>/call-notifications`, which is read at every ring, so nothing restarts.
  `TEAMS_REDIR_CALL_NOTIFICATIONS=off` sets the default when the file does not exist.
- Not yet: `addTrack`, video, `applyConstraints`; a safe download of the tones. Unhandled calls get the unknown-method reply and a log
  line `unhandled <type>.<method>`: those lines in a test call are the to-do list.

## Guesses to correct from further transcripts

All are isolated so they can be fixed in one place:

- The `sessioninfo` **feature values** (`enabled` / `disabled` strings, numeric `maxSimulcastLayers`)
  in `crates/rpc/src/redirector.rs`, and the event's addressing (no `rpcObjectId`, from the macOS
  writer, which omits a zero id).
- The `Pong` body, `vdiVendor = 5` (what macOS sends), and not sending `telemetry`, `node_id` and
  the `system*` fields in `ClientHello`.
