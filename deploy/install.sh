#!/bin/sh
# Run this on the Wyse 3040 itself, as root:
#
#   curl -fsSL https://raw.githubusercontent.com/jdoe/audio-player/main/deploy/install.sh | sh
#
# Downloads the latest release binary from GitHub, installs it as an OpenRC
# boot service, and starts it. No other files need to be copied to the
# device first.
set -eu

REPO="jdoe/audio-player" # change this to your GitHub owner/repo

if [ "$(id -u)" -ne 0 ]; then
	echo "Run this as root." >&2
	exit 1
fi

apk add --no-cache alsa-lib curl jq

echo "Looking up the latest release of $REPO..."
DOWNLOAD_URL=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
	| jq -r '.assets[] | select(.name == "audio-player") | .browser_download_url')

if [ -z "$DOWNLOAD_URL" ]; then
	echo "Could not find an 'audio-player' asset in the latest release of $REPO." >&2
	exit 1
fi

echo "Downloading $DOWNLOAD_URL..."
curl -fsSL -o /usr/local/bin/audio-player "$DOWNLOAD_URL"
chmod 755 /usr/local/bin/audio-player

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
EOF
chmod 755 /etc/init.d/audio-player

mkdir -p /var/lib/audio-player/audio

rc-update add audio-player default
rc-service audio-player restart

echo "audio-player installed and running. Status:"
rc-service audio-player status
