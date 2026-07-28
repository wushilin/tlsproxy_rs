#!/bin/sh
set -eu

# Run a native build container and use Zig to cross-link an x86_64 musl binary.
# This works on ARM without a VM or QEMU. The target machine only receives the
# finished binary and does not need Docker, Zig, or Rust.
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
TARGET=x86_64-unknown-linux-musl
IMAGE=tlsproxy-x64-musl-builder
TARGET_ROOT="$SCRIPT_DIR/target"
BINARY="$TARGET_ROOT/$TARGET/release/tlsproxy"

if ! command -v docker >/dev/null 2>&1; then
    echo "Docker is required on the build machine." >&2
    exit 1
fi

mkdir -p "$TARGET_ROOT"

echo "Building the amd64 musl build environment"
docker build \
    --file "$SCRIPT_DIR/Dockerfile.x64-musl" \
    --tag "$IMAGE" \
    "$SCRIPT_DIR"

echo "Building release binary for $TARGET"
docker run --rm \
    --env CARGO_TARGET_DIR=/build \
    --env HOST_UID="$(id -u)" \
    --env HOST_GID="$(id -g)" \
    --volume "$SCRIPT_DIR:/src:ro" \
    --volume "$TARGET_ROOT:/build" \
    "$IMAGE" \
    sh -c 'cargo zigbuild --locked --release --target x86_64-unknown-linux-musl && chown -R "$HOST_UID:$HOST_GID" /build'

echo "Binary ready at: $BINARY"
if command -v file >/dev/null 2>&1; then
    file "$BINARY"
fi
