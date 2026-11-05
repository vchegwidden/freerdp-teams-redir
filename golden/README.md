Reference transcripts (plan §3.3) go here as `*.jsonl`, in the format written by `crates/transcript`.
They are the replay regression inputs: `cargo run -p teams-redir-replay -- golden/<file>`.

- `2026-10-29-avd-first-contact.jsonl`: a real AVD host's first contact with this add-in (before it
  answered `setVersionInfo`, so it holds only inbound traffic plus our early, wrong replies).

All captures are scrubbed: TURN credentials, `callID`, `selfParticipantID`, ICE ufrag/pwd, DTLS
fingerprints and SDP cnames read `REDACTED`, and the `cloudId`, `rtcSession` ids and `endpointId` are
replaced with zeros of the same shape.

The hand-written fixture in `tests/fixtures/` is synthetic and must not be mistaken for a capture.
