# audio-player

A small web-controlled audio player for a Dell Wyse 3040 running Alpine
Linux. The program plays sound out loud through the device's own sound
card. A web page (dark background, orange accents) served on port 3000 is
the remote control: pick a file, play/pause, loop (on by default), start
over, adjust volume. It's a PWA, so it can be installed to a phone's home
screen ("Add to Home Screen" in the browser menu) and launches full-screen
like an app.

## How it's built

- `src/player/` - a `Player` trait with two implementations:
  - `alsa_backend.rs` - real playback, used on the actual device: decodes
    files with [rodio](https://docs.rs/rodio)'s decoder, then writes the
    audio straight to ALSA via the [alsa](https://docs.rs/alsa) crate
    (rather than through rodio's own output/`cpal` layer, which has a bug
    on this device's sound driver that silently drops audio).
  - `mock_backend.rs` - in-memory only, no sound card needed. Used by tests.
- `src/http.rs` - the web page and a small JSON API (`/api/files`,
  `/api/status`, `/api/play`, `/api/toggle`, `/api/restart`, `/api/loop`,
  `/api/volume`), plus the PWA files (`/manifest.webmanifest`, `/sw.js`,
  `/icon-192.png`, `/icon-512.png`).
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
enough to develop and test the web page and API.

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
| `AUDIO_PLAYER_MOCK` | unset                           | set to `1` to skip real playback  |
| `AUDIO_DEVICE`      | `plughw:CARD=Device,DEV=0`      | ALSA device sound is played through (see `aplay -l` for card names) |

Supported audio file types: `.mp3`, `.wav`, `.flac`, `.ogg`.
