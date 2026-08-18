#!/bin/sh
# Run this on the Wyse 3040 itself, as root:
#
#   wget -qO- https://raw.githubusercontent.com/siammridha/audio_player/master/deploy/install.sh | sh
#
# Downloads the latest release binary from GitHub, installs it as an OpenRC
# boot service, and starts it. No other files need to be copied to the
# device first.
set -eu

REPO="siammridha/audio_player"

if [ "$(id -u)" -ne 0 ]; then
	echo "Run this as root." >&2
	exit 1
fi

apk add --no-cache alsa-lib libgcc jq sof-firmware alsa-utils

echo "Looking up the latest release of $REPO..."
DOWNLOAD_URL=$(wget -qO- "https://api.github.com/repos/$REPO/releases/latest" \
	| jq -r '.assets[] | select(.name == "audio-player") | .browser_download_url')

if [ -z "$DOWNLOAD_URL" ]; then
	echo "Could not find an 'audio-player' asset in the latest release of $REPO." >&2
	exit 1
fi

echo "Downloading $DOWNLOAD_URL..."
# Download to a temp file and rename it into place, rather than overwriting
# /usr/local/bin/audio-player directly: if the service is already running
# from a previous install, writing straight to that path fails with "Text
# file busy". A rename works because it swaps the directory entry instead
# of touching the file the running process has open.
wget -qO /usr/local/bin/audio-player.new "$DOWNLOAD_URL"
chmod 755 /usr/local/bin/audio-player.new
mv /usr/local/bin/audio-player.new /usr/local/bin/audio-player

# The Wyse 3040's headphone jack (rt5670 codec) boots with its internal
# output routing switched off - nothing plays until these are turned on.
# This has to happen on every boot, so it's saved and restored by the
# service below rather than just set once here.
echo "Enabling headphone output routing..."
amixer sset "DAC1 MIXL DAC1" on >/dev/null 2>&1 || true
amixer sset "DAC1 MIXR DAC1" on >/dev/null 2>&1 || true
amixer sset "HPOVOL MIXL DAC1" on >/dev/null 2>&1 || true
amixer sset "HPOVOL MIXR DAC1" on >/dev/null 2>&1 || true
amixer sset "HPO MIX DAC1" on >/dev/null 2>&1 || true
amixer sset "HPO MIX HPVOL" on >/dev/null 2>&1 || true
amixer sset HP 100% >/dev/null 2>&1 || true
alsactl store 2>/dev/null || true

cat > /etc/init.d/audio-player <<'EOF'
#!/sbin/openrc-run

name="audio-player"
description="Web-controlled audio player"

command="/usr/local/bin/audio-player"
command_background="yes"
pidfile="/run/${RC_SVCNAME}.pid"
output_log="/var/log/audio-player.log"
error_log="/var/log/audio-player.log"

: "${PORT:=3000}"
: "${MUSIC_DIR:=/var/lib/audio-player/audio}"
export PORT MUSIC_DIR

depend() {
	need net
}

start_pre() {
	alsactl restore 2>/dev/null || true
}
EOF
chmod 755 /etc/init.d/audio-player

mkdir -p /var/lib/audio-player/audio

rc-update add audio-player default
rc-service audio-player restart

echo "audio-player installed and running. Status:"
rc-service audio-player status

echo
echo "If this is the first install on this device, reboot now so the sound"
echo "chip's firmware loads: reboot"
