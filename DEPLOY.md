# Getting this running on the Wyse 3040

## 1. Build a release

The binary is built by GitHub Actions, in a container that matches the
Wyse 3040 exactly (x86_64 Alpine) - no cross-compiling.

1. Push this repo to GitHub, with Actions enabled.
2. Push a version tag, e.g.:
   ```sh
   git tag v1.0.0
   git push origin v1.0.0
   ```
   This runs the `build` workflow and publishes a GitHub Release with the
   `audio-player` binary attached.

## 2. Install it on the device

Run this on the Wyse 3040 itself, as root:

```sh
wget -qO- https://raw.githubusercontent.com/siammridha/audio_player/master/deploy/install.sh | sh
```

This installs ALSA, downloads the latest release binary, sets up the OpenRC
service, sets it to start on every boot, and starts it right away. Nothing
else needs to be copied to the device first.

**On the first install on a given device, reboot once afterwards** (`reboot`)
so the sound chip's firmware loads. Without this, the device falls back to
HDMI-only audio and the headphone jack stays silent.

Then copy your audio files into `/var/lib/audio-player/audio` (e.g. with
`scp`). Files are scanned live, so no restart is needed after adding songs.

Check it's running:

```sh
rc-service audio-player status
cat /var/log/audio-player.log
```

**USB sound card, with a fallback:** the player plays through an external
USB sound card when it's plugged in, and through the device's own built-in
speaker otherwise - no restart needed either way. That's the default
(Automatic) mode. Tapping the "Playing through ..." pill under the page
title and picking a specific card instead makes that card the one it
switches to and from (the choice is saved in
`/var/lib/audio-player/output_device.json`; delete that file and restart the
service to go back to Automatic). To check the switch is
working, watch the log while you plug/unplug the card:

```sh
tail -f /var/log/audio-player.log
```

Every line is timestamped (UTC) and tagged with a level (`ERROR`, `INFO`,
or `DEBUG`), so it can be lined up against `dmesg` and `aplay -l` when
something looks wrong. What to expect at the default `INFO` level:

- On startup: `starting on the built-in speaker ... checking for the
  Device sound card` (`Device` is the USB card's ALSA id; with a card
  picked on the page, its id shows here instead).
- Plugging the card in: `Device sound card detected`, then about 5 seconds
  later (a settle delay before it's touched) `switching to the Device sound
  card`.
- Unplugging it: `Device sound card disconnected`, then right away `falling
  back to the built-in speaker`.
- Picking a card on the page: `switching to the <id> sound card` right
  away if it's plugged in (no settle delay), otherwise `falling back to the
  built-in speaker`.
- Using the web page: `playing <file>` and `paused playback`/`resumed
  playback`, so you can tell from the log alone whether a button press on
  the page actually reached the player.

If audio isn't playing, check which device it's actually using (a line
like `switching to the Device sound card` with nothing after it means it's
still there) - sound is likely just coming out of the *other* output than
the one you're listening on. The web page itself also shows this, in the
pill under the title ("Playing through USB PnP Sound Device" / "Playing
through built-in speaker").

**Capture mode:** swipe the page left to record from a capture card into
`/var/lib/audio-player/audio`. Recordings are plain WAV and take about
11.5 MB per minute in stereo (half that from a mono card). The device's
disk has under 1 GB free, so that's roughly 80 minutes; a recording stops
on its own when less than 50 MB is left. The Capture card shows the free
space, so you can see how much room is left. The picked capture card is saved
in `/var/lib/audio-player/capture_device.json`. At the `INFO` level the log
shows `switched to Capture mode`, `capturing from the <id> sound card
(48000 Hz, <n> ch)`, `recording to <file>` and `saved recording <file>`. Recordings can be deleted from the page (swipe
the row left); that logs `deleted <file>`.

**Log level:** a normal install (`deploy/install.sh`) sets the service to
`RUST_LOG=error`, so only failures are logged - the lines above stay
silent unless you turn it up. To see them (or `DEBUG`-level detail, like
volume changes and the settle-wait timing around hotplug), edit
`/etc/conf.d/audio-player` on the device:

```sh
echo 'export RUST_LOG=info' > /etc/conf.d/audio-player   # or debug
rc-service audio-player restart
```

`./test-on-device.sh` always sets this to `debug` for you, since that's
the point of using it.

## 3. Use it

Find the device's IP (`ip addr` on the device), then from any phone or
laptop on the same network, open:

```
http://<device-ip>:3000
```

## Updating things later

**Add or remove songs:** just copy files in/out of
`/var/lib/audio-player/audio` on the device (e.g. with `scp`). Reload the
web page afterwards to see the updated list.

**Update the program:** push a new version tag, then re-run the same
`wget ... | sh` command on the device - it always grabs the latest release.
