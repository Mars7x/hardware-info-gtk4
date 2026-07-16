#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export XDG_DATA_DIRS="$PWD/data:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"

host="$(rustc -vV | sed -n 's/^host: //p')"
case "$host" in
  aarch64-unknown-linux-gnu|armv7-unknown-linux-gnueabihf|x86_64-unknown-linux-gnu)
    export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-cpu=native"
    ;;
esac

cargo run --release
