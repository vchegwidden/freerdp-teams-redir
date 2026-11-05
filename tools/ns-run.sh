#!/bin/bash
# Run a command with the add-in visible at <libdir>/freerdp3/ without touching the real
# filesystem: a private user+mount namespace overlays <libdir> with a directory holding only the
# module. FreeRDP's add-in directory is its compile-time prefix (/usr/lib/freerdp3 for the Arch
# package), so a build cannot be loaded from anywhere else.
#
# usage: [NS_LIBDIR=/path/to/prefix/lib] tools/ns-run.sh <libteams-redir-client.so> <command> [args...]
#   NS_LIBDIR defaults to /usr/lib
set -euo pipefail

module=$(realpath "${1:?usage: ns-run.sh <module.so> <command> [args...]}")
shift
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/upper/freerdp3" "$work/work"
cp "$module" "$work/upper/freerdp3/libteams-redir-client.so"

libdir=${NS_LIBDIR:-/usr/lib}

unshare --user --map-root-user --mount bash -c '
  libdir=$1 work=$2
  shift 2
  mount -t overlay overlay -o "lowerdir=$libdir,upperdir=$work/upper,workdir=$work/work" "$libdir"
  exec "$@"' _ "$libdir" "$work" "$@"
