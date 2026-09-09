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
STATE_FILE="$MUSIC_DIR/state.json"
touch "$MUSIC_DIR/song1.mp3" "$MUSIC_DIR/song2.wav"

cargo build --quiet
BIN="${CARGO_TARGET_DIR:-target}/debug/audio-player"

SERVER_PID=""
cleanup() {
	agent-browser close >/dev/null 2>&1 || true
	[ -n "$SERVER_PID" ] && kill "$SERVER_PID" >/dev/null 2>&1 || true
	rm -rf "$MUSIC_DIR"
}
trap cleanup EXIT

# Starts the server (same MUSIC_DIR/STATE_FILE/PORT every time, so restarting
# it mid-test is just stop_server + start_server) and waits until it answers.
start_server() {
	AUDIO_PLAYER_MOCK=1 MUSIC_DIR="$MUSIC_DIR" STATE_FILE="$STATE_FILE" PORT="$PORT" "$BIN" &
	SERVER_PID=$!
	for _ in $(seq 1 50); do
		if curl -s -o /dev/null "http://127.0.0.1:$PORT/"; then
			return
		fi
		sleep 0.1
	done
	echo "FAIL: server did not start"
	exit 1
}

# Kills the current server and waits for it to actually exit, so a following
# start_server can't collide with it over the port.
stop_server() {
	[ -n "$SERVER_PID" ] || return 0
	kill "$SERVER_PID" >/dev/null 2>&1 || true
	for _ in $(seq 1 50); do
		kill -0 "$SERVER_PID" >/dev/null 2>&1 || return 0
		sleep 0.1
	done
}

start_server

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
# The default headless viewport is shorter than the page, which puts the
# file list partly below the fold and makes clicks on it land off-screen.
agent-browser set viewport 1280 900 >/dev/null

files=$(browser eval "Array.from(document.querySelectorAll('#files button')).map(b => b.textContent).join(',')")
assert_eq "file list shows both fixture files" '"song1.mp3,song2.wav"' "$files"

browser click "#files button" >/dev/null
sleep 0.2
now_playing=$(browser eval "document.getElementById('now-playing').textContent")
assert_eq "selecting a file starts playing it" '"song1.mp3"' "$now_playing"
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

browser eval "(() => { const el = document.getElementById('volume-slider'); el.focus(); el.value = 65; el.dispatchEvent(new Event('input')); })()" >/dev/null
volume_icon_while_sliding=$(browser eval "document.getElementById('volume-icon').innerHTML")
echo "$volume_icon_while_sliding" | grep -q 'M17.5' && echo "ok: volume icon shows the high-volume glyph while sliding to 65%" || { echo "FAIL: volume icon did not update to the high-volume glyph"; exit 1; }

browser eval "(() => { const el = document.getElementById('volume-slider'); el.value = 0; el.dispatchEvent(new Event('input')); })()" >/dev/null
volume_icon_muted=$(browser eval "document.getElementById('volume-icon').innerHTML")
echo "$volume_icon_muted" | grep -q 'M16 9l5 6' && echo "ok: volume icon shows the muted glyph at 0%" || { echo "FAIL: volume icon did not switch to the muted glyph"; exit 1; }

browser eval "document.getElementById('volume-slider').dispatchEvent(new Event('pointerdown'))" >/dev/null
hud_visible=$(browser eval "document.getElementById('volume-hud').classList.contains('visible')")
assert_eq "volume overlay appears while sliding" "true" "$hud_visible"

browser eval "(() => { const el = document.getElementById('volume-slider'); el.value = 42; el.dispatchEvent(new Event('input')); })()" >/dev/null
hud_fill_width=$(browser eval "document.getElementById('volume-hud-fill').style.width")
assert_eq "volume overlay bar fills to match the live value while sliding" '"42%"' "$hud_fill_width"

sleep 1.3
volume_before_release=$(browser eval "document.getElementById('volume-slider').value")
assert_eq "sliding without releasing does not push volume to the server" '"42"' "$volume_before_release"

browser eval "(() => { const el = document.getElementById('volume-slider'); el.dispatchEvent(new Event('pointerup')); el.dispatchEvent(new Event('change')); })()" >/dev/null
sleep 1.3
hud_hidden=$(browser eval "document.getElementById('volume-hud').classList.contains('visible')")
assert_eq "volume overlay fades out after sliding stops" "false" "$hud_hidden"

volume_after_release=$(browser eval "document.getElementById('volume-slider').value")
assert_eq "releasing the slider round-trips the volume through the server" '"42"' "$volume_after_release"

manifest_href=$(browser eval "document.querySelector('link[rel=manifest]').getAttribute('href')")
assert_eq "page links a web app manifest" '"/manifest.webmanifest"' "$manifest_href"

manifest_status=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/manifest.webmanifest")
assert_eq "manifest is served" "200" "$manifest_status"

sw_status=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/sw.js")
assert_eq "service worker is served" "200" "$sw_status"

icon_status=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/icon-192.png")
assert_eq "app icon is served" "200" "$icon_status"

# --- Persistence across a restart ---
# At this point the server has song1.mp3 playing (from the "start over"
# click above), loop off, volume 42%. The browser tab stays open and keeps
# polling /api/status every second, exactly like a real restart underneath
# an already-open page - no manual reload.

position_before_restart=$(curl -s "http://127.0.0.1:$PORT/api/status" | jq '.position')
stop_server
start_server
sleep 1.3

now_playing=$(browser eval "document.getElementById('now-playing').textContent")
assert_eq "restart resumes the same track" '"song1.mp3"' "$now_playing"

play_pause=$(browser eval "document.getElementById('play-pause').textContent")
assert_eq "restart auto-resumes playing when it was playing before" '"Pause"' "$play_pause"

loop_after_restart=$(browser eval "document.getElementById('loop').classList.contains('on')")
assert_eq "restart restores the loop setting" "false" "$loop_after_restart"

volume_after_restart=$(browser eval "document.getElementById('volume-slider').value")
assert_eq "restart restores the volume" '"42"' "$volume_after_restart"

position_after_restart=$(curl -s "http://127.0.0.1:$PORT/api/status" | jq '.position')
position_advanced=$(awk -v a="$position_before_restart" -v b="$position_after_restart" 'BEGIN { print (b >= a) }')
assert_eq "restart resumes at the saved position, not from the start" "1" "$position_advanced"

# Now pause, let that persist, and confirm a restart while paused stays paused.
browser click "#play-pause" >/dev/null
sleep 0.2
play_pause=$(browser eval "document.getElementById('play-pause').textContent")
assert_eq "paused before the next restart" '"Play"' "$play_pause"

position_while_paused=$(curl -s "http://127.0.0.1:$PORT/api/status" | jq '.position')
stop_server
start_server
sleep 1.3

play_pause=$(browser eval "document.getElementById('play-pause').textContent")
assert_eq "restart stays paused when it was paused before" '"Play"' "$play_pause"

position_after_paused_restart=$(curl -s "http://127.0.0.1:$PORT/api/status" | jq '.position')
# Compared with a tolerance, not exact string equality: jq re-formats floats
# on the way through and can print the same double as a slightly different
# string (e.g. trailing "...0000004" noise), which isn't a real position drift.
position_unchanged=$(awk -v a="$position_while_paused" -v b="$position_after_paused_restart" 'BEGIN { d = a - b; if (d < 0) d = -d; print (d < 0.05) }')
assert_eq "position stays put across a restart while paused" "1" "$position_unchanged"

# Delete the saved track while the service is down: it must come back with
# nothing loaded instead of crashing or getting stuck.
rm "$MUSIC_DIR/song1.mp3"
stop_server
start_server
sleep 1.3

status_after_missing_file=$(curl -s "http://127.0.0.1:$PORT/api/status")
assert_eq "restart with the saved track missing starts with nothing loaded" "null" "$(echo "$status_after_missing_file" | jq '.file')"

now_playing=$(browser eval "document.getElementById('now-playing').textContent")
assert_eq "page reflects nothing loaded after the saved track disappears" '"Nothing selected"' "$now_playing"

echo "All browser checks passed."
