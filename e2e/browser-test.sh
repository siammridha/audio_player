#!/bin/sh
# Drives the running web UI through a real browser (system Chromium via
# agent-browser). This only checks the page/API layer using the mock audio
# backend - it can't verify real sound coming out of a speaker, since this
# container has no sound card. That only adds a browser layer; it does not
# replace `cargo nextest run`.
set -eu

cd "$(dirname "$0")/.."

PORT=3900
MUSIC_DIR=$(mktemp -d)
touch "$MUSIC_DIR/song1.mp3" "$MUSIC_DIR/song2.wav"

cargo build --quiet

SERVER_PID=""
cleanup() {
	agent-browser close >/dev/null 2>&1 || true
	[ -n "$SERVER_PID" ] && kill "$SERVER_PID" >/dev/null 2>&1 || true
	rm -rf "$MUSIC_DIR"
}
trap cleanup EXIT

AUDIO_PLAYER_MOCK=1 MUSIC_DIR="$MUSIC_DIR" PORT="$PORT" ./target/debug/audio-player &
SERVER_PID=$!

for _ in $(seq 1 50); do
	if curl -s -o /dev/null "http://127.0.0.1:$PORT/"; then
		break
	fi
	sleep 0.1
done

agent-browser close >/dev/null 2>&1 || true

browser() {
	agent-browser "$@"
}

assert_eq() {
	label=$1
	expected=$2
	actual=$3
	if [ "$actual" != "$expected" ]; then
		echo "FAIL: $label - expected [$expected], got [$actual]"
		exit 1
	fi
	echo "ok: $label"
}

agent-browser --executable-path /usr/bin/chromium --args "--no-sandbox" open "http://127.0.0.1:$PORT" >/dev/null

files=$(browser eval "Array.from(document.querySelectorAll('#files button')).map(b => b.textContent).join(',')")
assert_eq "file list shows both fixture files" '"song1.mp3,song2.wav"' "$files"

browser click "#files button" >/dev/null
sleep 0.2
now_playing=$(browser eval "document.getElementById('now-playing').textContent")
assert_eq "selecting a file starts playing it" '"Playing: song1.mp3"' "$now_playing"
play_pause=$(browser eval "document.getElementById('play-pause').textContent")
assert_eq "play/pause button reads Pause while playing" '"Pause"' "$play_pause"

browser click "#play-pause" >/dev/null
sleep 0.2
play_pause=$(browser eval "document.getElementById('play-pause').textContent")
assert_eq "play/pause button reads Play once paused" '"Play"' "$play_pause"

loop_on_by_default=$(browser eval "document.getElementById('loop').classList.contains('on')")
assert_eq "loop is on by default" "true" "$loop_on_by_default"

browser click "#loop" >/dev/null
loop_off=$(browser eval "document.getElementById('loop').classList.contains('on')")
assert_eq "loop button turns off" "false" "$loop_off"

browser click "#restart" >/dev/null
sleep 0.2
play_pause=$(browser eval "document.getElementById('play-pause').textContent")
assert_eq "start over resumes playing" '"Pause"' "$play_pause"

browser eval "const el = document.getElementById('volume-slider'); el.value = 42; el.dispatchEvent(new Event('input'));" >/dev/null
sleep 1.3
volume_after_roundtrip=$(browser eval "document.getElementById('volume-slider').value")
assert_eq "volume slider change round-trips through the server" '"42"' "$volume_after_roundtrip"

manifest_href=$(browser eval "document.querySelector('link[rel=manifest]').getAttribute('href')")
assert_eq "page links a web app manifest" '"/manifest.webmanifest"' "$manifest_href"

manifest_status=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/manifest.webmanifest")
assert_eq "manifest is served" "200" "$manifest_status"

sw_status=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/sw.js")
assert_eq "service worker is served" "200" "$sw_status"

icon_status=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/icon-192.png")
assert_eq "app icon is served" "200" "$icon_status"

echo "All browser checks passed."
