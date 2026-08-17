# audio-player

A small web-controlled audio player for a Dell Wyse 3040 running Alpine
Linux. The program plays sound out loud through the device's own sound
card. A web page (dark background, orange accents) served on port 3000 is
the remote control: pick a file, play/pause, loop, start over.

## How it's built

- `src/player/` - a `Player` trait with two implementations:
  - `rodio_backend.rs` - real playback via [rodio](https://docs.rs/rodio),
    used on the actual device.
  - `mock_backend.rs` - in-memory only, no sound card needed. Used by tests.
- `src/http.rs` - the web page and a small JSON API (`/api/files`,
  `/api/status`, `/api/play`, `/api/toggle`, `/api/restart`, `/api/loop`).
- `assets/index.html` - the whole UI: one file, inline CSS/JS, no build step.
- `deploy/audio-player.initd` - an OpenRC service so it starts on boot.
- `.github/workflows/build.yml` - builds the release binary on a native
  x86_64 Alpine container, so it matches the Wyse 3040 exactly.

See [DEPLOY.md](DEPLOY.md) for how to get it running on the device.

## Development

This dev container is arm64 and has no sound card, so the real (`rodio`)
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

Supported audio file types: `.mp3`, `.wav`, `.flac`, `.ogg`.
