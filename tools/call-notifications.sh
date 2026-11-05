#!/bin/bash
# Switch the desktop notification for incoming Teams calls, without restarting anything.
#   tools/call-notifications.sh on|off|toggle    set it
#   tools/call-notifications.sh status           show it (default)
#   tools/call-notifications.sh test             show a sample notification to check the daemon
# The add-in reads the file at every ring. Without the file, TEAMS_REDIR_CALL_NOTIFICATIONS=off in
# the environment of the FreeRDP process decides, and the default is on.
set -euo pipefail

state=${TEAMS_REDIR_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/teams-redir}
file=$state/call-notifications

current() {
  if [ -f "$file" ]; then
    case "$(tr '[:upper:]' '[:lower:]' < "$file" | tr -d '[:space:]')" in
      off|0|false|no) echo off ;;
      *) echo on ;;
    esac
  else
    echo "default (on unless TEAMS_REDIR_CALL_NOTIFICATIONS=off)"
  fi
}

case "${1:-status}" in
  on|off) mkdir -p "$state"; echo "$1" > "$file"; echo "call notifications: $1" ;;
  toggle) mkdir -p "$state"
          if [ "$(current)" = off ]; then echo on > "$file"; else echo off > "$file"; fi
          echo "call notifications: $(current)" ;;
  status) echo "call notifications: $(current)" ;;
  test)   notify-send --app-name=Teams --icon=call-start --expire-time=5000 \
            "Incoming Teams call" "This is how a ringing call will look." ;;
  *)      sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
