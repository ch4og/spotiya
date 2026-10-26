#!/bin/sh
# Runs the cross-built receiver under qemu-user to catch binaries that link but cannot start.
set -eu
SPOTIYA_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SPOTIYA_CACHE=${SPOTIYA_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/spotiya"}
SPOTIYA_TOOLCHAIN=${SPOTIYA_TOOLCHAIN:-"$SPOTIYA_CACHE/gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf"}
SPOTIYA_ALSA_SYSROOT=${SPOTIYA_ALSA_SYSROOT:-"$SPOTIYA_CACHE/alsa-sysroot-glibc223"}
SPOTIYA_QEMU=${SPOTIYA_QEMU:-qemu-arm}
SPOTIYA_BINARY=${1:-"$SPOTIYA_ROOT/${CARGO_TARGET_DIR:-target}/armv7-unknown-linux-gnueabihf/release/spotiya"}
SPOTIYA_OUTPUT=$(mktemp)
trap 'rm -f "$SPOTIYA_OUTPUT"' EXIT HUP INT TERM

run() {
    "$SPOTIYA_QEMU" -L "$SPOTIYA_TOOLCHAIN/arm-linux-gnueabihf/libc" \
        -E "LD_LIBRARY_PATH=$SPOTIYA_ALSA_SYSROOT/usr/lib" "$SPOTIYA_BINARY" "$@" >"$SPOTIYA_OUTPUT" 2>&1
}
expect() {
    if ! grep -Fq -- "$1" "$SPOTIYA_OUTPUT"; then
        echo "Smoke test: expected '$1' in output:" >&2
        cat "$SPOTIYA_OUTPUT" >&2
        exit 1
    fi
}

SPOTIYA_VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$SPOTIYA_ROOT/Cargo.toml" | head -n 1)
run --version
expect "spotiya $SPOTIYA_VERSION "
run --help
expect 'spotiya --play QUERY'
expect 'spotiya --uninstall [--yes]'
if run --uninstall --update; then
    echo 'Smoke test: uninstall was accepted together with update' >&2
    exit 1
fi
expect 'Choose one maintenance operation per command'
if run --yes; then
    echo 'Smoke test: --yes was accepted without uninstall' >&2
    exit 1
fi
expect '--yes requires --uninstall'
run --profiles
expect 'mini1 '
expect 'mini2 '
if run --no-such-option; then
    echo 'Smoke test: an unknown option was accepted' >&2
    exit 1
fi
expect 'Error: unknown option: --no-such-option'
echo "ARM smoke test passed: spotiya $SPOTIYA_VERSION starts under $SPOTIYA_QEMU"
