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
speaker otherwise - no restart needed either way. To check the switch is
working, watch the log while you plug/unplug the USB card:

```sh
tail -f /var/log/audio-player.log
```

Every line is timestamped (UTC) and tagged with a level (`ERROR`, `INFO`,
or `DEBUG`), so it can be lined up against `dmesg` and `aplay -l` when
something looks wrong. What to expect at the default `INFO` level:

- On startup: `starting on the built-in speaker ... checking for the USB
  sound card`.
- Plugging the USB card in: `sound card detected`, then about 5 seconds
  later (a settle delay before it's touched) `switching to the USB sound
  card`.
- Unplugging it: `sound card disconnected`, then right away `falling back
  to the built-in speaker`.
- Using the web page: `playing <file>` and `paused playback`/`resumed
  playback`, so you can tell from the log alone whether a button press on
  the page actually reached the player.

If audio isn't playing, check which device it's actually using (a line
like `switching to the USB sound card` with nothing after it means it's
still there) - sound is likely just coming out of the *other* output than
the one you're listening on. The web page itself also shows this, as a
small line under the title ("Playing through USB sound card" / "Playing
through built-in speaker").

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
