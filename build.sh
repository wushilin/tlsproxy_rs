#!/bin/sh
set -eu

TARGET="${TARGET:-x86_64-unknown-linux-musl}"

echo "Checking embedded dependency-free UI"
test -f static/index.html
test -f static/app.js
test -f static/styles.css
if grep -REn 'https?://' static/index.html static/app.js static/styles.css; then
  echo "External runtime URL found in embedded UI" >&2
  exit 1
fi

if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "Rust target '$TARGET' is not installed." >&2
  echo "Install it with: rustup target add $TARGET" >&2
  exit 1
fi

echo "Building release binary for $TARGET"
cargo build --locked --release --target "$TARGET"
echo "Done: target/$TARGET/release/tlsproxy"
