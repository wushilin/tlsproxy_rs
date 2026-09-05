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

# Named volumes keep the crate registry and the Zig cache alive between runs.
# Without them every run re-downloads the crates, and the freshly extracted
# sources get new mtimes, which invalidates cargo's fingerprints and forces a
# full rebuild.
CARGO_CACHE=tlsproxy-x64-musl-cargo
ZIG_CACHE=tlsproxy-x64-musl-zig

if ! command -v docker >/dev/null 2>&1; then
    echo "Docker is required on the build machine." >&2
    exit 1
fi

if ! docker info >/dev/null 2>&1; then
    echo "Cannot talk to the Docker daemon. Is it running, and is your user in" >&2
    echo "the 'docker' group? (sudo usermod -aG docker \"\$USER\", then re-login.)" >&2
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
    --volume "$CARGO_CACHE:/usr/local/cargo/registry" \
    --volume "$ZIG_CACHE:/root/.cache" \
    "$IMAGE" \
    sh -c 'cargo zigbuild --locked --release --target x86_64-unknown-linux-musl && chown -R "$HOST_UID:$HOST_GID" /build'

echo "Binary ready at: $BINARY"
if command -v file >/dev/null 2>&1; then
    file "$BINARY"
fi
