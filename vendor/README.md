# vendor

`libwebrtc/` is `libwebrtc` 0.3.55 from crates.io (Copyright LiveKit, Inc., Apache-2.0: see
`libwebrtc/LICENSE-APACHE`; the source files keep their license headers) with four changes, each
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

Drop this directory and the `[patch.crates-io]` entry in `../Cargo.toml` when upstream has both.

`webrtc-sys/` is `webrtc-sys` 0.3.52 from crates.io (Copyright LiveKit, Inc., Apache-2.0) without its
`test/` directory, with two changes, marked `teams-redir patch`: `RtpTransceiver::id` (see above), and in
`src/video_encoder_factory.cpp`:

- The VAAPI H.264 encoder is only offered when `TEAMS_REDIR_VAAPI=1`. libwebrtc otherwise picks it
  whenever libva opens a display, and on Intel (iHD driver) it segfaulted inside `vaRenderPicture`
  on the first frame of a call. Software OpenH264 is used instead.
