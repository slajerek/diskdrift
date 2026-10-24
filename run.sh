#!/usr/bin/env bash
# Runs DiskDrift, rebuilding it first if the sources changed. macOS only for now.
#
#   ./run.sh                  full-screen TUI
#   ./run.sh <command> ...    any CLI command, e.g. ./run.sh scan / or ./run.sh diff
#   ./run.sh --help           CLI help
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"

# Quiet incremental build: a no-op when nothing changed; output shown on failure.
if ! out="$("$here/build.sh" 2>&1)"; then
    echo "$out" >&2
    echo "run.sh: build failed" >&2
    exit 1
fi

if [[ $# -eq 0 ]]; then
    set -- tui
fi
exec "$here/target/release/diskdrift" "$@"
