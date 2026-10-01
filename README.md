# audio-player

A small web-controlled audio player for a Dell Wyse 3040 running Alpine
Linux. The program plays sound out loud through an external USB sound card
when it's plugged in, falling back to the device's own built-in speaker
otherwise. A web page (dark background, orange accents) served on port 3000 is
the remote control: pick a file, play/pause, loop (on by default), start
over, adjust volume, and set a sleep timer. It's a PWA, so it can be
installed to a phone's home screen ("Add to Home Screen" in the browser
menu) and launches full-screen like an app. Playback state (track,
position, volume, loop, play/pause) is saved to disk as it changes, so a
service restart or crash comes back exactly where it left off. A pending
sleep timer is saved the same way, so it still fires even across a restart.

The sleep timer pauses playback at a chosen clock time instead of sounding
an alarm: pick a time, and the volume fades out over the last 2 seconds
before pausing. It's driven by the server, not the browser tab, so it
still fires even if the phone is asleep or the page is closed.

## How it's built

- `src/player/` - a `Player` trait with two implementations:
  - `alsa_backend.rs` - real playback, used on the actual device: decodes
    files with [rodio](https://docs.rs/rodio)'s decoder, then writes the
    audio straight to ALSA via the [alsa](https://docs.rs/alsa) crate
    (rather than through rodio's own output/`cpal` layer, which has a bug
    on this device's sound driver that silently drops audio). Plays
    through the external USB sound card when it's plugged in, and falls
    back to the Wyse 3040's built-in speaker otherwise - see
    `device_watch.rs` below for how that's detected.
  - `device_watch.rs` - watches for the USB sound card appearing/
    disappearing. Listens straight to the kernel's netlink hotplug
    broadcast (no udev - this device runs mdev), rechecking the actual ALSA
    card list on every event rather than trusting the event's contents, and
    only acts on a real change in presence. A newly-appeared card gets a
    5-second settle delay (re-checked after the wait) before anything opens
    it; a disappearing card is acted on immediately.
  - `mock_backend.rs` - in-memory only, no sound card needed. Used by tests.
- `src/state_store.rs` - saves/loads the `STATE_FILE` JSON snapshot (track,
  position, volume, loop, play/pause) used to restore playback after a
  restart. Saved on every play/toggle/restart/loop/volume change, and every
  5 seconds while playing; failures are logged and never block startup or a
  request.
- `src/sleep_timer.rs` - the sleep timer: a target Unix-ms timestamp (sent
  by the browser, which already knows the device's local time, so the
  backend never has to deal with timezones), ticked every 100ms by a
  background thread in `main.rs`. Fades the volume down over the last 2
  seconds, then pauses (only if still playing) and restores the volume.
  Saved to a `sleep_timer.json` file next to `STATE_FILE` so it survives a
  restart; a saved timer whose time has already passed is discarded on
  load instead of firing late.
- `src/http.rs` - the web page and a small JSON API (`/api/files`,
  `/api/status`, `/api/play`, `/api/toggle`, `/api/restart`, `/api/loop`,
  `/api/volume`, `/api/sleep-timer`, `/api/sleep-timer/cancel`), plus the
  PWA files (`/manifest.webmanifest`, `/sw.js`, `/icon-192.png`,
  `/icon-512.png`).
- `assets/index.html` - the whole UI: one file, inline CSS/JS, no build step.
- `assets/manifest.webmanifest`, `assets/sw.js`, `assets/icon-*.png` - what
  makes the page a PWA: an app manifest, a service worker that caches the
  page shell for fast/offline loading (never the `/api/*` calls, which
  always need the live server), and the app icon.
  The device is only reachable over plain `http://`, not `https://`, so
  browsers won't show an automatic "install this app" prompt - use "Add to
  Home Screen" from the browser's menu instead, which works the same way.
- `deploy/install.sh` - run on the device; downloads the latest release
  binary from GitHub and sets it up as an OpenRC service that starts on boot.
- `.github/workflows/build.yml` - on a pushed version tag, builds the release
  binary on a native x86_64 Alpine container (matching the Wyse 3040 exactly)
  and publishes it as a GitHub Release.

See [DEPLOY.md](DEPLOY.md) for how to get it running on the device.

## Development

This dev container is arm64 and has no sound card, so the real (ALSA)
backend can't be run or tested here - only the mock backend can. That's
enough to develop and test the web page and API. `device_watch.rs`'s
presence-detection logic (the settle delay, only-report-real-changes rule,
etc.) is decoupled from real ALSA/netlink and has its own unit tests, which
do run here.

```sh
cargo build
cargo nextest run          # unit tests, against the mock backend
./e2e/browser-test.sh      # drives the real UI in a browser, mock backend
```

To try the UI locally by hand:

```sh
AUDIO_PLAYER_MOCK=1 MUSIC_DIR=/tmp/music cargo run
```

then open http://127.0.0.1:3000.

Environment variables the program reads:

| Variable            | Default                        | Meaning                          |
|---------------------|---------------------------------|-----------------------------------|
| `PORT`              | `3000`                          | web server port                   |
| `MUSIC_DIR`         | `/var/lib/audio-player/audio`   | folder scanned for audio files    |
| `STATE_FILE`        | `/var/lib/audio-player/state.json` | file where playback state is saved so it survives a restart |
| `AUDIO_PLAYER_MOCK` | unset                           | set to `1` to skip real playback  |
| `AUDIO_DEVICE`      | `plughw:CARD=Device,DEV=0`      | ALSA device sound is played through when the USB sound card is present (see `aplay -l` for card names) |
| `AUDIO_DEVICE_FALLBACK` | `plughw:CARD=rt5672,DEV=0`  | ALSA device used instead when the USB sound card isn't plugged in (the Wyse 3040's built-in speaker) |

Supported audio file types: `.mp3`, `.wav`, `.flac`, `.ogg`.
