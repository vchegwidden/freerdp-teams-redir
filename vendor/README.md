# vendor

`libwebrtc/` is `libwebrtc` 0.3.55 from crates.io (Copyright LiveKit, Inc., Apache-2.0: see
`libwebrtc/LICENSE-APACHE`; the source files keep their license headers) with five changes, each
marked `teams-redir patch`:

- `RtpTransceiver::set_direction`, which the C++ layer has and the Rust bindings leave out. Teams
  calls it on every transceiver of a call.
- `PeerConnection::get_stats_json`, the stats report as libwebrtc serialises it. The crate's own
  `get_stats` parses it into typed structs that drop fields, and the Windows add-in forwards the
  libwebrtc JSON as it is.

- `RtpTransceiver::id`, the address of the libwebrtc transceiver (added to `webrtc-sys` too). The
  list of transceivers shrinks when a section both sides rejected is dropped and grows when a later
  offer reuses it, so the add-in matches its records to transceivers by this id, not by position.

- `OfferOptions { offer_to_receive_audio: false, .. }` now means "unset" instead of 0. libwebrtc
  reads 0 as "do not offer to receive" and strips the receive bit from every transceiver when it
  creates an offer, so a `sendrecv` audio transceiver was offered `sendonly`. Teams calls
  `createOffer` with no options, and so did the Windows add-in.

- `PeerConnection::pending_local_description` / `pending_remote_description` (added to
  `webrtc-sys` too). Teams forwards the local description from each `icecandidate` event to the
  other client of a 1:1 call; the current one has no candidates while our offer is outstanding, so
  calls placed from this side failed ICE.

Drop this directory and the `[patch.crates-io]` entry in `../Cargo.toml` when upstream has both.

`webrtc-sys/` is `webrtc-sys` 0.3.52 from crates.io (Copyright LiveKit, Inc., Apache-2.0) without its
`test/` directory, with six changes, marked `teams-redir patch`: `RtpTransceiver::id` and the pending descriptions (see above), `RtpSender::set_track`
accepting a null track (`replaceTrack(null)` used to dereference it and crash the client), max-bundle
and rtcp-mux "require" on every peer connection (libwebrtc defaults to "balanced", which gave each
section its own ICE transport, and a meeting rejects a camera re-offer whose video sections are not
bundled: `MultiplexingRequiredForMediaWithSameLabel`), the PipeWire screen capturer on any Wayland session
(`XDG_SESSION_TYPE=wayland`) even without `WAYLAND_DISPLAY`, which a client on XWayland drops and
libwebrtc otherwise requires (the portal needs only D-Bus and PipeWire), and in
`src/video_encoder_factory.cpp`:

- The VAAPI H.264 encoder is only offered when `TEAMS_REDIR_VAAPI=1`. libwebrtc otherwise picks it
  whenever libva opens a display, and on Intel (iHD driver) it segfaulted inside `vaRenderPicture`
  on the first frame of a call. Software OpenH264 is used instead.
