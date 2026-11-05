Patches for finding what the host checks (plan §3.2 runs C and D). Each file is a JSON merge patch
(RFC 7386: objects merge, `null` removes a key). Two kinds, applied together if both are set:

- `session/*.json` patch the `rpcEventArgs` of the `sessioninfo` event (`webrtc.1`).
- `clienthello/*.json` patch the `ClientHello` (`cpc.1`).

`tools/experiment.sh <name>` activates one (names starting `c` are ClientHello patches, the rest are
session patches). The add-in re-reads them on every handshake, so **restart Teams in the session** to
try a new one; no reconnect. `tools/experiment.sh off` removes both.

**Found (2026-10-29, from the cloud PC's Teams web logs):** after our `sessioninfo` the shim logged
`endpoint microphone access is disabled`, Teams emitted `vdiClientDisconnected` with reason
`endpointUnsupported` ("Unsupported Endpoint") and switched to the local stack, which is the "Not
optimized" message. Every earlier experiment left `features.microphone` at `"disabled"`. The
`ClientHello` was accepted and logged field by field, so the `cpc.1` side was never the problem.

Run `s4-microphone` first. Everything below it is only needed if that does not change the message.

Suggested order:

0. `s4-microphone`: `features.microphone = enabled`, on top of the honest defaults. Nothing else.
1. `s3-kitchen-sink`: every session change at once. If Teams' message changes, bisect with `s1`,
   `s2` and the `e*` files. If not, the `sessioninfo` content is not the gate.
2. `c1-store-errc`, then `c2-macstore-full`, each on its own and then with `s3` active: the
   `ClientHello` is what tells the host whether to fall back to `webrtc.1`.

| Name | Changes |
|---|---|
| `e1-datachannel` | `datachannel = enabled` |
| `e2-windows-versions` | e1, plus the Windows `webRTC` / `webRTCRedirector` strings (win-findings §4, [O]) |
| `e3-windows-features` | e2, plus the other features Windows enables |
| `s1-os-and-dpi` | adds `display.dpi` and `version.clientOS` (both reference clients send `clientOS`) |
| `s2-windows-extra-keys` | s1, plus Windows-only `clientOSName`, `clientOSVersionDetail`, `oobClient` (values guessed) |
| `s3-kitchen-sink` | s2 + e3 |
| `s4-microphone` | `microphone = enabled`; the host rejects the endpoint while it is `disabled` |
| `v1-clip-region` | `clipRectMode = clipRegion`: tells Teams the client composites video over its window. Video is shown in separate windows, so this is a diagnostic for what Teams does with the setting (it then sends `notifyClipRectChanged`) |
| `v2-mirror-self` | `mirrormyselfvideo = enabled`: Teams mirrors the self view; the video window honours `scaleX(-1)` |
| `v3-video-send` | `maxOutgoingResolution = 720` and `datachannel = enabled`. The 2026-11-02 call: Teams sent a video offer and the service rejected the send section (port 0); a Windows client reports both of these and this client reports neither |
| `c1-store-errc` | `ClientHello` errors become `client_app_store_build` (16066), the macOS Store decline |
| `c2-macstore-full` | c1 plus `platform = mac-arm64` and the macOS plugin version |

Everything except `s1`, `s4` and `c1` **claims things this client does not have or is not** (libwebrtc
M133, screen share, a macOS platform). They are diagnostics to learn where the host draws the line,
not a configuration to leave on: a call routed to this client would fail. A shipped client reports
what it really is.
