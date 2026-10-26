#!/bin/sh
# Cross-compile the optional YouTube add-on for the station and package it the
# way the receiver's add-on installer expects: spotiya-youtube-armv7.tar.gz plus
# YOUTUBE_SHA256SUMS, with a manifest pinned to this Spotiya version.
set -eu
SPOTIYA_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SPOTIYA_CACHE=${SPOTIYA_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/spotiya"}
SPOTIYA_TOOLCHAIN=${SPOTIYA_TOOLCHAIN:-"$SPOTIYA_CACHE/gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf"}
SPOTIYA_ALSA_SYSROOT=${SPOTIYA_ALSA_SYSROOT:-"$SPOTIYA_CACHE/alsa-sysroot-glibc223"}
TARGET=armv7-unknown-linux-gnueabihf
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$SPOTIYA_ROOT/Cargo.toml" | head -n1)

if [ ! -x "$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc" ] || [ ! -f "$SPOTIYA_ALSA_SYSROOT/.spotiya-alsa-glibc223-no-versioned" ]; then
    echo 'Run scripts/bootstrap-arm.sh first' >&2
    exit 1
fi

C="$SPOTIYA_TOOLCHAIN/bin"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="$C/arm-linux-gnueabihf-gcc"
export CC_armv7_unknown_linux_gnueabihf="$C/arm-linux-gnueabihf-gcc"
export CXX_armv7_unknown_linux_gnueabihf="$C/arm-linux-gnueabihf-g++"
export AR_armv7_unknown_linux_gnueabihf="$C/arm-linux-gnueabihf-ar"
# rquickjs builds QuickJS C, opus builds libopus via cmake (older policy), both need NEON.
export CMAKE_POLICY_VERSION_MINIMUM=3.5
export CFLAGS_armv7_unknown_linux_gnueabihf="-mcpu=${SPOTIYA_TARGET_CPU:-cortex-a53} -mfpu=neon-fp-armv8 -mfloat-abi=hard"
export CXXFLAGS_armv7_unknown_linux_gnueabihf="$CFLAGS_armv7_unknown_linux_gnueabihf"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$SPOTIYA_ALSA_SYSROOT"
export PKG_CONFIG_LIBDIR="$SPOTIYA_ALSA_SYSROOT/usr/lib/pkgconfig"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS="-C target-cpu=${SPOTIYA_TARGET_CPU:-cortex-a53} -C link-arg=-Wl,--gc-sections -C link-arg=-Wl,-z,max-page-size=4096 -L $SPOTIYA_ALSA_SYSROOT/usr/lib"

( cd "$SPOTIYA_ROOT/youtube" && cargo build --release --target "$TARGET" )
BIN="$SPOTIYA_ROOT/youtube/target/$TARGET/release/spotiya-youtube"
"$C/arm-linux-gnueabihf-strip" "$BIN" 2>/dev/null || true

mkdir -p "$SPOTIYA_ROOT/dist"
python3 "$SPOTIYA_ROOT/youtube/package.py" "$BIN" "$VERSION"
