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
curl -fsSL https://raw.githubusercontent.com/<owner>/<repo>/main/deploy/install.sh | sh
```

(Replace `<owner>/<repo>` with the actual GitHub repo, and make sure the
`REPO` line near the top of `deploy/install.sh` in the repo is also set to
that same `<owner>/<repo>` - that's what tells the script where to download
the binary from.)

This installs ALSA, downloads the latest release binary, sets up the OpenRC
service, sets it to start on every boot, and starts it right away. Nothing
else needs to be copied to the device first.

Then copy your audio files into `/var/lib/audio-player/audio` (e.g. with
`scp`). Files are scanned live, so no restart is needed after adding songs.

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

**Update the program:** push a new version tag, then re-run the same
`curl ... | sh` command on the device - it always grabs the latest release.
