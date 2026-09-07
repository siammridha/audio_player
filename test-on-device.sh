#!/bin/sh
# Build a release binary and push it straight to the real device, for
# testing changes without waiting on a tagged release + GitHub Actions +
# deploy/install.sh. Run from the repo root:
#
#   ./test-on-device.sh
#
# The device accepts root login with no key and no password (PermitEmptyPasswords),
# so no credentials are needed here.
#
# Set DEVICE_IP to your device's LAN address before running, e.g.:
#
#   DEVICE_IP=192.168.1.50 ./test-on-device.sh
set -eu

[ -f .env ] && export $(cat .env | xargs)

: "${DEVICE_IP:?Set DEVICE_IP to your device's LAN address, e.g. DEVICE_IP=192.168.1.50 ./test-on-device.sh}"

if ! command -v cargo-zigbuild >/dev/null 2>&1; then
	echo "Installing cargo-zigbuild..."
	cargo install cargo-zigbuild
fi
rustup target add x86_64-unknown-linux-musl >/dev/null

SSH="ssh -o StrictHostKeyChecking=accept-new root@$DEVICE_IP"
SCP="scp -o StrictHostKeyChecking=accept-new"

# The `alsa-sys` crate's build script links against `libasound` via
# pkg-config (see its build.rs) - on a build host whose own arch differs
# from the target's (e.g. this aarch64 devcontainer cross-compiling to
# x86_64), the host's own alsa-lib-dev package is the wrong arch to link
# against, and pkg-config refuses to probe a foreign arch without extra
# cross-compile setup this devcontainer doesn't have.
#
# The device itself only has the alsa-lib runtime package (no headers, no
# .pc file - deploy/install.sh never installs alsa-lib-dev), so unlike a
# project that can fetch a ready-made sysroot straight off the device, this
# one fetches the matching x86_64 alsa-lib/alsa-lib-dev .apk packages
# directly from Alpine's own CDN instead - the same alpine:3.23 release the
# `build` workflow compiles in (see .github/workflows/build.yml), so the
# headers/.so match what a real release build links against. An .apk file
# is just a gzipped tar, so this only needs curl + tar, no apk/QEMU/Docker
# needed. Cached under ~/.cache so a repeat run doesn't re-fetch.
ALPINE_VERSION="v3.23"
ALSA_VERSION="1.2.14-r2"
ALSA_SYSROOT="$HOME/.cache/audio_player-alsa-x86_64-linux-musl"
if [ ! -f "$ALSA_SYSROOT/usr/lib/pkgconfig/alsa.pc" ]; then
	echo "Fetching x86_64 alsa-lib headers/library from Alpine $ALPINE_VERSION (cached at $ALSA_SYSROOT)..."
	rm -rf "$ALSA_SYSROOT"
	mkdir -p "$ALSA_SYSROOT"
	TMP_APK_DIR=$(mktemp -d)
	trap 'rm -rf "$TMP_APK_DIR"' EXIT
	for pkg in alsa-lib alsa-lib-dev; do
		curl -fsSL -o "$TMP_APK_DIR/$pkg.apk" \
			"https://dl-cdn.alpinelinux.org/alpine/$ALPINE_VERSION/main/x86_64/$pkg-$ALSA_VERSION.apk"
		tar xzf "$TMP_APK_DIR/$pkg.apk" -C "$ALSA_SYSROOT"
	done
	rm -rf "$TMP_APK_DIR"
	trap - EXIT
fi
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$ALSA_SYSROOT"
export PKG_CONFIG_PATH="$ALSA_SYSROOT/usr/lib/pkgconfig"

echo "Building release binary for x86_64-unknown-linux-musl..."
RUSTC_BOOTSTRAP=1 cargo zigbuild --release --target x86_64-unknown-linux-musl

BINARY="${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/audio-player"

echo "Copying $BINARY to $DEVICE_IP..."
$SCP "$BINARY" "root@$DEVICE_IP:/usr/local/bin/audio-player.new"

echo "Installing and restarting audio-player on the device..."
# mv instead of overwriting the running binary directly, to avoid a
# "Text file busy" error (same reason as deploy/install.sh). Also turns on
# debug-level logging for this test run, regardless of what a real install
# was set to.
$SSH "echo 'export RUST_LOG=debug' > /etc/conf.d/audio-player && chmod 755 /usr/local/bin/audio-player.new && mv /usr/local/bin/audio-player.new /usr/local/bin/audio-player && rc-service audio-player restart"

echo "Done. Status:"
$SSH "rc-service audio-player status"

echo
echo "Open http://$DEVICE_IP:3000 in a browser to test."
echo "Logs: ssh root@$DEVICE_IP tail -f /var/log/audio-player.log"
