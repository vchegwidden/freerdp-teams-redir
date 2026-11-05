#!/bin/bash
# Select sessioninfo / ClientHello patches for the next Teams restart (no reconnect needed).
#   tools/experiment.sh s3-kitchen-sink    activate experiments/session/s3-kitchen-sink.json
#   tools/experiment.sh c1-store-errc      activate experiments/clienthello/c1-store-errc.json
#   tools/experiment.sh off                remove both patches
#   tools/experiment.sh                    show what is active
# One session patch and one ClientHello patch can be active at the same time.
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
state=${TEAMS_REDIR_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/teams-redir}
session=$state/sessioninfo-patch.json
hello=$state/clienthello-patch.json

show() { [ -f "$2" ] && { echo "$1:"; cat "$2"; } || echo "$1: none"; }

case "${1:-}" in
  "")  show "sessioninfo patch" "$session"; show "clienthello patch" "$hello" ;;
  off) rm -f "$session" "$hello"; echo "patches removed" ;;
  *)   name=${1%.json}
       src=$(ls "$here"/experiments/session/"$name".json "$here"/experiments/clienthello/"$name".json 2>/dev/null | head -1 || true)
       if [ -z "$src" ]; then
         echo "no such experiment: $1"; ls "$here"/experiments/session "$here"/experiments/clienthello | grep json | sed 's/\.json$//'; exit 1
       fi
       mkdir -p "$state"
       case "$src" in
         */clienthello/*) cp "$src" "$hello"; show "clienthello patch" "$hello" ;;
         *)               cp "$src" "$session"; show "sessioninfo patch" "$session" ;;
       esac ;;
esac
