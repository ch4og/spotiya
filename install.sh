#!/bin/sh
set -eu

fail() { echo "spotiya: $*" >&2; exit 1; }
usage() { echo 'Usage: sh install.sh [--install | --check | --uninstall [--yes]]'; }
SPOTIYA_MODE=menu
SPOTIYA_YES=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --install|--check|--uninstall)
            [ "$SPOTIYA_MODE" = menu ] || fail 'Choose one operation.'
            SPOTIYA_MODE=${1#--}
            ;;
        --yes) SPOTIYA_YES=--yes ;;
        --help|-h) usage; echo 'Without arguments, open the install/update, check and uninstall menu.'; exit 0 ;;
        *) usage >&2; exit 1 ;;
    esac
    shift
done
[ -z "$SPOTIYA_YES" ] || [ "$SPOTIYA_MODE" = uninstall ] || fail '--yes requires --uninstall.'
[ "$(id -u)" -eq 0 ] || fail 'Run this installer as root on the speaker.'
SPOTIYA_BASE=/data/opt/spotify-connect
if [ "$SPOTIYA_MODE" = menu ]; then
    echo 'Spotiya setup'
    echo '  1) Install or update to the latest stable release'
    echo '  2) Check download and compatibility without installing'
    echo '  3) Uninstall Spotiya and all its data'
    echo '  0) Exit'
    while :; do
        printf 'Choose [0]: '
        IFS= read -r SPOTIYA_CHOICE || SPOTIYA_CHOICE=0
        case "$SPOTIYA_CHOICE" in
            1) SPOTIYA_MODE=install; break ;;
            2) SPOTIYA_MODE=check; break ;;
            3) SPOTIYA_MODE=uninstall; break ;;
            0|'') echo 'Cancelled; nothing changed.'; exit 0 ;;
            *) echo 'Enter 0, 1, 2 or 3.' ;;
        esac
    done
fi
if [ "$SPOTIYA_MODE" = install ] && [ -x "$SPOTIYA_BASE/spotiya" ]; then
    echo 'Spotiya is installed; checking for updates...'
    exec "$SPOTIYA_BASE/spotiya" --update
fi
if [ "$SPOTIYA_MODE" = uninstall ]; then
    [ -d "$SPOTIYA_BASE" ] || { echo 'Spotiya is not installed.'; exit 0; }
    if [ -x "$SPOTIYA_BASE/spotiya" ] && "$SPOTIYA_BASE/spotiya" --help | grep -q -- '--uninstall'; then
        # The binary owns service shutdown, locking and removal. Do not duplicate it here.
        set -- --uninstall
        [ -z "$SPOTIYA_YES" ] || set -- "$@" --yes
        exec "$SPOTIYA_BASE/spotiya" "$@"
    fi
    echo 'Downloading a current uninstaller for this older installation...'
fi
for SPOTIYA_TOOL in uname od tr sha256sum gzip tar mktemp df awk grep mv; do
    command -v "$SPOTIYA_TOOL" >/dev/null 2>&1 || fail "Required command not found: $SPOTIYA_TOOL"
done
case "$(uname -m)" in
    armv7l|armv8l|aarch64) ;;
    *) fail 'This package requires ARMv7 Linux with a 32-bit userspace.' ;;
esac
if [ "$(od -An -tx1 -j4 -N2 /bin/sh | tr -d ' \n')" != 0101 ] ||
    [ "$(od -An -tx1 -j18 -N2 /bin/sh | tr -d ' \n')" != 2800 ]; then
    fail 'This package requires a little-endian ARM32 userspace.'
fi

SPOTIYA_MAIND=/system/vendor/quasar/maind
SPOTIYA_CA=/etc/ssl/certs/ca-certificates.crt
if [ ! -x "$SPOTIYA_MAIND" ] || [ ! -r "$SPOTIYA_CA" ]; then
    fail 'Factory maind and its CA bundle are required; use installation through a PC on other firmware.'
fi
SPOTIYA_FREE=$(df -Pk /tmp | awk 'END { print $4 }')
case "$SPOTIYA_FREE" in ''|*[!0-9]*) fail 'Cannot determine free space in /tmp.' ;; esac
[ "$SPOTIYA_FREE" -ge 16384 ] || fail 'At least 16 MiB of free temporary space is required.'
umask 077
SPOTIYA_WORK=$(mktemp -d /tmp/spotiya-install.XXXXXX)
trap 'rm -rf "$SPOTIYA_WORK"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

download() {
    LD_LIBRARY_PATH=/system/vendor/quasar/libs "$SPOTIYA_MAIND" --gst-launch \
        "souphttpsrc location=\"$1\" ssl-strict=true ssl-ca-file=\"$SPOTIYA_CA\" iradio-mode=false timeout=15 retries=1 ! filesink location=\"$2.part\"" \
        > "$2.log" 2>&1 || fail 'GitHub HTTPS download failed.'
    # This factory tool also exits successfully after pipeline errors.
    if ! grep -Fq 'EOS: end of stream' "$2.log" || grep -Fq 'ERROR from ' "$2.log" || [ ! -s "$2.part" ]; then
        fail 'GitHub HTTPS download did not complete; check Internet access, firmware CA certificates and system time.'
    fi
    mv "$2.part" "$2"
}

SPOTIYA_ASSET=spotiya-armv7-linux-gnueabihf.tar.gz
SPOTIYA_RELEASE=https://github.com/klockky/spotiya/releases/latest/download
echo 'Downloading the latest stable Spotiya from GitHub over HTTPS...'
download "$SPOTIYA_RELEASE/SHA256SUMS" "$SPOTIYA_WORK/SHA256SUMS"
download "$SPOTIYA_RELEASE/$SPOTIYA_ASSET" "$SPOTIYA_WORK/$SPOTIYA_ASSET"
awk -v asset="$SPOTIYA_ASSET" '
    NF == 2 && ($2 == asset || $2 == "*" asset) { count++; hash = $1 }
    END {
        if (count != 1 || length(hash) != 64 || hash ~ /[^0-9a-fA-F]/) exit 1
        print hash "  " asset
    }
' "$SPOTIYA_WORK/SHA256SUMS" > "$SPOTIYA_WORK/expected.sha256" || fail 'Invalid or duplicate release checksum.'
(cd "$SPOTIYA_WORK" && sha256sum -c expected.sha256) ||
    fail 'Release checksum mismatch; nothing installed. Retry if a new release was just published.'

gzip -dc "$SPOTIYA_WORK/$SPOTIYA_ASSET" > "$SPOTIYA_WORK/package.tar"
rm "$SPOTIYA_WORK/$SPOTIYA_ASSET"
tar -xf "$SPOTIYA_WORK/package.tar" -C "$SPOTIYA_WORK"
rm "$SPOTIYA_WORK/package.tar"
"$SPOTIYA_WORK/spotiya/spotiya" --version || fail 'The release binary cannot run on this firmware.'
if [ "$SPOTIYA_MODE" = check ]; then
    echo 'GitHub HTTPS download, checksum and ARM compatibility checks passed. Nothing installed.'
    exit 0
fi
if [ "$SPOTIYA_MODE" = uninstall ]; then
    set -- --uninstall
    [ -z "$SPOTIYA_YES" ] || set -- "$@" --yes
    "$SPOTIYA_WORK/spotiya/spotiya" "$@"
    exit 0
fi
sh "$SPOTIYA_WORK/spotiya/scripts/install.sh"
echo 'Spotiya is ready. Run: spotiya tui'
echo 'Optional add-ons: spotiya addons'
