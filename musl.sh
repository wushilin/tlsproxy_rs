#!/bin/sh
set -eu

# Build a statically linked musl binary on this machine, without Docker.
#
# The crate tree pulls in C and C++ dependencies (rocksdb, zstd, lz4, aws-lc-rs),
# so a musl build needs a cross C/C++ toolchain, not just the Rust musl target.
# Two toolchains are supported, in this order:
#
#   1. A musl cross GCC already in PATH (e.g. x86_64-linux-musl-gcc / -g++ from
#      musl-cross-make or musl.cc). Used with a plain `cargo build`.
#   2. Zig, driven by cargo-zigbuild. Zig ships musl libc and libc++, so it needs
#      no system packages. If Zig is missing it is fetched once into a virtualenv
#      under ~/.cache/tlsproxy-musl-build (pip install ziglang).
#
# Also required on the build host: libclang, for the bindgen run in
# librocksdb-sys (dnf install clang-devel / apt-get install libclang-dev).
#
# Zig cross-compiles, so any of the targets below can be built from any host --
# an arm64 binary on an x86_64 machine and, equally, the default x64 binary on an
# arm64 machine. The host architecture never needs to match. Nothing in this
# script assumes an x86_64 host. Run --help for the target list.
#
# The default target is x86_64 on EVERY host, including arm64 -- it is a fixed
# triple, never derived from `uname -m`. Building on an arm box with no arguments
# still produces an x64 binary. The architecture of the result is verified
# against the ELF header before the script reports success.
#
# Usage:
#   ./musl-build.sh                            # default: x64, on any host
#   ./musl-build.sh --target arm64             # short name or full triple
#   ./musl-build.sh --toolchain gcc            # force the musl cross GCC path
#   ./musl-build.sh -- --features foo          # extra args passed to cargo
#
# Environment:
#   TARGET                     same as --target
#   TOOLCHAIN                  force a backend: "zig" or "gcc"
#   NO_BOOTSTRAP=1             never download Zig; fail with instructions instead
#   TLSPROXY_BUILD_TOOLS_DIR   where to keep the bootstrapped Zig; must be
#                              outside the source tree (see the note below)

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

# triple|short names|status. The first row is the default target.
TARGET_TABLE='x86_64-unknown-linux-musl|x64 x86_64 amd64|tested
aarch64-unknown-linux-musl|arm64 aarch64|tested
armv7-unknown-linux-musleabihf|armv7 armhf|untested
i686-unknown-linux-musl|i686 x86|untested'

print_targets() {
    echo "Supported targets (--target accepts either column):"
    echo "$TARGET_TABLE" | while IFS='|' read -r triple names status; do
        printf '  %-32s %-16s %s\n' "$triple" "$names" "$status"
    done
    echo
    echo "Any other *-musl / *-musleabi* triple is accepted but untested."
}

# Expand a short name to a full triple; pass full triples through untouched.
resolve_target() {
    _want="$1"
    echo "$TARGET_TABLE" | while IFS='|' read -r triple names _status; do
        if [ "$_want" = "$triple" ]; then
            echo "$triple"
            break
        fi
        for name in $names; do
            if [ "$_want" = "$name" ]; then
                echo "$triple"
                break 2
            fi
        done
    done
}

TARGET="${TARGET:-x86_64-unknown-linux-musl}"
TOOLCHAIN="${TOOLCHAIN:-auto}"
NO_BOOTSTRAP="${NO_BOOTSTRAP:-0}"
# Deliberately outside the source tree: Zig reports its lib directory as a path
# relative to the current directory whenever the zig executable sits underneath
# it, and cargo-zigbuild passes that path to bindgen, which runs from a
# different directory and then cannot find clang's builtin headers.
TOOLS_DIR="${TLSPROXY_BUILD_TOOLS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/tlsproxy-musl-build}"

while [ $# -gt 0 ]; do
    case "$1" in
        --target)
            [ $# -ge 2 ] || { echo "--target needs a value" >&2; exit 2; }
            TARGET="$2"
            shift 2
            ;;
        --target=*)
            TARGET="${1#--target=}"
            shift
            ;;
        --toolchain)
            [ $# -ge 2 ] || { echo "--toolchain needs a value" >&2; exit 2; }
            TOOLCHAIN="$2"
            shift 2
            ;;
        --no-bootstrap)
            NO_BOOTSTRAP=1
            shift
            ;;
        -h|--help)
            awk 'NR>=4 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"
            echo
            print_targets
            exit 0
            ;;
        --)
            shift
            break
            ;;
        *)
            echo "Unknown option: $1 (use -- to pass extra cargo args)" >&2
            exit 2
            ;;
    esac
done

resolved=$(resolve_target "$TARGET")
if [ -n "$resolved" ]; then
    TARGET="$resolved"
fi

case "$TARGET" in
    *-musl|*-musleabi|*-musleabihf) ;;
    *)
        echo "Target '$TARGET' is not a musl target." >&2
        echo >&2
        print_targets >&2
        exit 2
        ;;
esac

echo "Checking embedded UI assets are present"
test -f "$SCRIPT_DIR/static/index.html"
test -f "$SCRIPT_DIR/static/app.js"
test -f "$SCRIPT_DIR/static/styles.css"

if ! command -v rustup >/dev/null 2>&1; then
    echo "rustup is required to install the '$TARGET' standard library." >&2
    exit 1
fi

if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "Installing Rust target '$TARGET'"
    rustup target add "$TARGET"
fi

# bindgen (used by librocksdb-sys) needs libclang on the build host. Only warn
# when it is nowhere to be found -- do not export LIBCLANG_PATH ourselves, since
# pinning it to a bare library directory breaks bindgen's discovery of clang's
# own builtin headers (stdbool.h and friends) on some distro layouts.
if [ -z "${LIBCLANG_PATH:-}" ]; then
    found_libclang=0
    # /usr/lib/<host-triple>-linux-gnu is glob'd rather than hardcoded so this
    # works on an arm64 build host as well as x86_64.
    for dir in /usr/lib64 /usr/lib /usr/lib/*-linux-gnu /usr/local/lib; do
        if ls "$dir"/libclang.so* >/dev/null 2>&1; then
            found_libclang=1
            break
        fi
    done
    if [ "$found_libclang" = 0 ]; then
        echo "libclang was not found; bindgen (librocksdb-sys) will fail." >&2
        echo "Install it with one of:" >&2
        echo "  dnf install clang-devel        # Fedora/RHEL" >&2
        echo "  apt-get install libclang-dev   # Debian/Ubuntu" >&2
        exit 1
    fi
fi

# Pick the C/C++ toolchain.
target_underscore=$(echo "$TARGET" | tr 'a-z-' 'A-Z_')
arch="${TARGET%%-*}"
# musleabihf targets use a matching toolchain prefix (armv7-linux-musleabihf).
gcc_prefix="$arch-linux-${TARGET##*-}"

use_gcc=0
if [ "$TOOLCHAIN" = "gcc" ]; then
    use_gcc=1
elif [ "$TOOLCHAIN" = "auto" ] \
        && command -v "$gcc_prefix-gcc" >/dev/null 2>&1 \
        && command -v "$gcc_prefix-g++" >/dev/null 2>&1; then
    use_gcc=1
elif [ "$TOOLCHAIN" != "auto" ] && [ "$TOOLCHAIN" != "zig" ]; then
    echo "Unknown toolchain '$TOOLCHAIN' (expected 'zig' or 'gcc')." >&2
    exit 2
fi

if [ "$use_gcc" = 1 ]; then
    for tool in "$gcc_prefix-gcc" "$gcc_prefix-g++" "$gcc_prefix-ar"; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            echo "Musl cross toolchain is incomplete: '$tool' not found in PATH." >&2
            echo "Either install a full $gcc_prefix- toolchain or run with --toolchain zig." >&2
            exit 1
        fi
    done

    echo "Using musl cross GCC: $(command -v "$gcc_prefix-gcc")"
    export "CC_$TARGET"="$gcc_prefix-gcc"
    export "CXX_$TARGET"="$gcc_prefix-g++"
    export "AR_$TARGET"="$gcc_prefix-ar"
    export "CARGO_TARGET_${target_underscore}_LINKER"="$gcc_prefix-gcc"
    # rocksdb links libstdc++; keep the result fully static.
    export "CARGO_TARGET_${target_underscore}_RUSTFLAGS"="-C target-feature=+crt-static"
    BUILD_CMD="build"
else
    # Zig backend. cargo-zigbuild drives `zig cc` / `zig c++` as the cross compiler.
    if ! command -v cargo-zigbuild >/dev/null 2>&1; then
        echo "Installing cargo-zigbuild"
        cargo install cargo-zigbuild --locked
    fi

    ZIG_BIN=""
    if command -v zig >/dev/null 2>&1; then
        ZIG_BIN=$(command -v zig)
    elif python3 -c 'import ziglang' >/dev/null 2>&1; then
        ZIG_BIN="python3 -m ziglang"
    elif [ -x "$TOOLS_DIR/zig-venv/bin/python" ] \
            && "$TOOLS_DIR/zig-venv/bin/python" -c 'import ziglang' >/dev/null 2>&1; then
        ZIG_BIN="$TOOLS_DIR/zig-venv/bin/python -m ziglang"
    elif [ "$NO_BOOTSTRAP" = 1 ]; then
        echo "Zig was not found and bootstrapping is disabled." >&2
        echo "Install it with one of:" >&2
        echo "  pip install ziglang" >&2
        echo "  <your package manager> install zig" >&2
        exit 1
    else
        echo "Zig not found; installing it into $TOOLS_DIR/zig-venv"
        command -v python3 >/dev/null 2>&1 || {
            echo "python3 is required to bootstrap Zig." >&2
            exit 1
        }
        mkdir -p "$TOOLS_DIR"
        python3 -m venv "$TOOLS_DIR/zig-venv"
        "$TOOLS_DIR/zig-venv/bin/pip" install --quiet --upgrade pip
        "$TOOLS_DIR/zig-venv/bin/pip" install --quiet ziglang
        ZIG_BIN="$TOOLS_DIR/zig-venv/bin/python -m ziglang"
    fi

    # cargo-zigbuild looks for `zig` on PATH, so wrap whatever we resolved.
    mkdir -p "$TOOLS_DIR/bin"
    printf '#!/bin/sh\nexec %s "$@"\n' "$ZIG_BIN" > "$TOOLS_DIR/bin/zig"
    chmod +x "$TOOLS_DIR/bin/zig"
    PATH="$TOOLS_DIR/bin:$PATH"
    export PATH

    # Guard against the relative-lib_dir trap described at TOOLS_DIR above: if
    # Zig resolves to a path under the current directory, bindgen will fail with
    # a confusing "'stdbool.h' file not found" later on.
    zig_lib=$(zig env 2>/dev/null | sed -n \
        -e 's/.*"lib_dir"[[:space:]]*:[[:space:]]*"\(.*\)".*/\1/p' \
        -e 's/^[[:space:]]*\.lib_dir[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' \
        | head -1)
    case "$zig_lib" in
        /*) ;;
        *)
            echo "Zig reports a relative lib directory ('$zig_lib')." >&2
            echo "That breaks bindgen. Install Zig outside $(pwd), or set" >&2
            echo "TLSPROXY_BUILD_TOOLS_DIR to a directory outside the source tree." >&2
            exit 1
            ;;
    esac

    echo "Using Zig $(zig version) via cargo-zigbuild"
    BUILD_CMD="zigbuild"
fi

echo "Building release binary for $TARGET"
cargo "$BUILD_CMD" --locked --release --target "$TARGET" "$@"

BINARY="${CARGO_TARGET_DIR:-$SCRIPT_DIR/target}/$TARGET/release/tlsproxy"

# Confirm the emitted binary really is for the requested architecture by reading
# ELF e_machine (offset 0x12). On an arm64 host the default target is still x64,
# and a silent fallback to a native build would otherwise be easy to miss.
expected_machine=""
case "$TARGET" in
    x86_64-*)      expected_machine=3e ;;
    aarch64-*)     expected_machine=b7 ;;
    armv7-*|arm-*) expected_machine=28 ;;
    i686-*|i586-*) expected_machine=03 ;;
esac
if [ -n "$expected_machine" ] && command -v od >/dev/null 2>&1; then
    actual_machine=$(od -An -tx1 -j18 -N1 "$BINARY" | tr -d ' \n')
    if [ "$actual_machine" != "$expected_machine" ]; then
        echo "Built binary is not for $TARGET:" >&2
        echo "  ELF e_machine is 0x$actual_machine, expected 0x$expected_machine." >&2
        exit 1
    fi
fi

echo "Binary ready at: $BINARY"
if command -v file >/dev/null 2>&1; then
    file "$BINARY"
fi
