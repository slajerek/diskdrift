#!/usr/bin/env bash
# Runs DiskDrift, rebuilding it first if the sources changed. macOS only for now.
#
#   ./run.sh                  full-screen TUI
#   ./run.sh <command> ...    any CLI command, e.g. ./run.sh scan / or ./run.sh diff
#   ./run.sh --help           CLI help
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"

# Only announce a build when the release binary may actually need rebuilding.
binary="$here/target/release/diskdrift"
needs_build=0
if [[ ! -x "$binary" ]]; then
    needs_build=1
else
    for input in "$here/Cargo.toml" "$here/Cargo.lock" "$here/crates" "$here/benches"; do
        if [[ -e "$input" ]] && [[ "$input" -nt "$binary" || ( -d "$input" && -n "$(find "$input" -type f -newer "$binary" -print -quit)" ) ]]; then
            needs_build=1
            break
        fi
    done
fi

if [[ "$needs_build" == 1 ]]; then
    printf '\033[1;36m==> Building...\033[0m\n'
fi

# Quiet incremental build: output shown on failure.
if ! out="$("$here/build.sh" 2>&1)"; then
    echo "$out" >&2
    echo "run.sh: build failed" >&2
    exit 1
fi

if [[ "$needs_build" == 1 ]]; then
    printf '\033[1;32m==> Build completed. Starting diskdrift.\033[0m\n'
fi

if [[ $# -eq 0 ]]; then
    set -- tui
fi
exec "$here/target/release/diskdrift" "$@"
