#!/usr/bin/env bash
# Builds DiskDrift (release by default). macOS only for now.
#
#   ./build.sh            release build -> target/release/diskdrift
#   ./build.sh --debug    debug build   -> target/debug/diskdrift
#   ./build.sh --test     release build, then all tests and clippy
set -euo pipefail
cd "$(dirname "$0")"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "build.sh: only macOS is supported for now" >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
    # rustup installs here; a shell started before the install may lack it in PATH.
    [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "build.sh: cargo not found; install Rust with:" >&2
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
    exit 1
fi

profile=release
run_tests=0
for arg in "$@"; do
    case "$arg" in
        --debug) profile=debug ;;
        --test) run_tests=1 ;;
        -h|--help) sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "build.sh: unknown option '$arg' (try --help)" >&2; exit 2 ;;
    esac
done

if [[ "$profile" == release ]]; then
    cargo build --release -p diskdrift-cli
else
    cargo build -p diskdrift-cli
fi

if [[ "$run_tests" == 1 ]]; then
    cargo test
    cargo clippy --all-targets -- -D warnings
fi

echo "Built: target/$profile/diskdrift"
