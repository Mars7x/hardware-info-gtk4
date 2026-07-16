#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

host="$(rustc -vV | sed -n 's/^host: //p')"
case "$host" in
  aarch64-unknown-linux-gnu|armv7-unknown-linux-gnueabihf|x86_64-unknown-linux-gnu)
    native_flags='-C target-cpu=native'
    ;;
  *)
    native_flags=''
    ;;
esac

if [ -n "$native_flags" ]; then
  export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$native_flags"
fi

cargo build --release
printf 'Built target/release/hardware-info-gtk4 for %s\n' "$host"
