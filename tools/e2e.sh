#!/bin/bash
# Local end-to-end smoke test, headless and loopback-only: Xvfb plus FreeRDP's shadow server,
# with the SDL client loading the add-in. Checks that FreeRDP finds and initializes it. The
# shadow server does not open the Teams channels, so message handling is covered by
# build/shim_test and the Rust tests.
#
# usage: [E2E_CLIENT=<sdl-freerdp binary>] [NS_LIBDIR=<its prefix>/lib] tools/e2e.sh <libteams-redir-client.so> [client args]
#   defaults: the system sdl-freerdp3, /usr/lib, and the client args /dvc:teams-redir
# env:   TEAMS_REDIR_*  configuration passed through to the add-in (see crates/rpc/src/config.rs)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
module=$(realpath "${1:?usage: e2e.sh <module.so> [client args]}")
shift
[ $# -gt 0 ] || set -- /dvc:teams-redir

work=$(mktemp -d)
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; wait 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT

unset WAYLAND_DISPLAY
export XDG_CONFIG_HOME="$work/config"   # the shadow server writes its certificate here
export TEAMS_REDIR_STATE_DIR=${TEAMS_REDIR_STATE_DIR:-$work/state}
display=:${E2E_DISPLAY:-99}

Xvfb "$display" -screen 0 1280x720x24 >"$work/xvfb.log" 2>&1 & pids+=($!)
sleep 1
DISPLAY=$display freerdp-shadow-cli3 /port:3390 /bind-address:127.0.0.1 >"$work/shadow.log" 2>&1 & pids+=($!)
sleep 3

set +e
DISPLAY=$display SDL_VIDEODRIVER=x11 timeout "${E2E_SECONDS:-8}" "$here/ns-run.sh" "$module" \
  "${E2E_CLIENT:-sdl-freerdp3}" /v:127.0.0.1:3390 /cert:ignore /u:x /p:x /sec:tls "$@" 2>&1 | tee "$work/client.log" \
  | grep -E 'teams-redir|dvcman_load_addin|Failed to load' || true
set -e

grep -q 'com.microsoft.rdc.dvc.webrtc.1: listening' "$work/client.log" || { echo "FAIL: add-in did not initialize"; exit 1; }
echo "OK: add-in loaded and listening"
