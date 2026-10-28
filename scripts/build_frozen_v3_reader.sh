#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
source="$root/crates/diskdrift-storage/tests/fixtures/v3-reader"
out="$root/target/v3-reader"

cd "$source"
shasum -a 256 -c checksums.sha256 >&2
mkdir -p "$out/cargo"
cargo build --release --locked --manifest-path Cargo.toml --target-dir "$out/cargo"
cp "$out/cargo/release/diskdrift-frozen-v3-reader" "$out/diskdrift-v3-reader"
chmod 755 "$out/diskdrift-v3-reader"
test -x "$out/diskdrift-v3-reader"
printf '%s\n' "$out/diskdrift-v3-reader"
