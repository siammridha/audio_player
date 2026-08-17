# Getting this running on the Wyse 3040

## 1. Get the binary built

The binary is built by GitHub Actions, in a container that matches the
Wyse 3040 exactly (x86_64 Alpine) - no cross-compiling.

1. Push this repo to GitHub, with Actions enabled.
2. Open the repo's **Actions** tab, wait for the `build` workflow to finish
   (or run it by hand with **Run workflow** if it didn't trigger).
3. Open the finished run and download the `audio-player-x86_64-alpine`
   artifact. It's a zip containing one file: `audio-player`.
4. Copy that `audio-player` file to the Wyse 3040, e.g.:
   ```sh
   scp audio-player root@<device-ip>:/tmp/audio-player
   ```

## 2. Set it up on the device

Run these on the Wyse 3040 itself (as root):

```sh
# Runtime dependency: the program links against ALSA at runtime.
apk add alsa-lib

# Install the binary.
install -m 755 /tmp/audio-player /usr/local/bin/audio-player

# Folder the player scans for audio files (.mp3, .wav, .flac, .ogg).
mkdir -p /var/lib/audio-player/audio
# Now copy your audio files into /var/lib/audio-player/audio, e.g. with scp.
```

Get the OpenRC service file onto the device (e.g. `scp deploy/audio-player.initd
root@<device-ip>:/tmp/`), then:

```sh
install -m 755 /tmp/audio-player.initd /etc/init.d/audio-player
rc-update add audio-player default   # start on every boot
rc-service audio-player start
```

Check it's running:

```sh
rc-service audio-player status
cat /var/log/audio-player.log
```

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

**Update the program:** download a fresh artifact from a new Actions run,
then on the device:

```sh
rc-service audio-player stop
install -m 755 /tmp/audio-player /usr/local/bin/audio-player
rc-service audio-player start
```
