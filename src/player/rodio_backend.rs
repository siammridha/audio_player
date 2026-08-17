use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player as RodioSink, Source};

use super::{Player, PlayerStatus};

#[derive(Clone)]
struct Track {
    path: PathBuf,
    display_name: String,
    duration: Option<Duration>,
}

struct Inner {
    track: Option<Track>,
    looping: bool,
}

/// Plays audio for real, through the machine's own sound card via ALSA.
pub struct RodioPlayer {
    // Held only to keep the output device open for the life of the process;
    // dropping it would silence audio.
    _device_sink: MixerDeviceSink,
    sink: RodioSink,
    inner: Mutex<Inner>,
}

impl RodioPlayer {
    /// Opens the default audio output device and starts the background
    /// thread that restarts a track when it ends and looping is on.
    pub fn new() -> anyhow::Result<Arc<Self>> {
        let device_sink = DeviceSinkBuilder::open_default_sink()?;
        let sink = RodioSink::connect_new(device_sink.mixer());
        let player = Arc::new(Self {
            _device_sink: device_sink,
            sink,
            inner: Mutex::new(Inner {
                track: None,
                looping: false,
            }),
        });

        let watcher = Arc::clone(&player);
        thread::spawn(move || watcher.watch_for_track_end());

        Ok(player)
    }

    /// Replaces whatever is queued with `path`, decoded from scratch, and
    /// starts it playing. Returns `None` (leaving the old track queued) if
    /// the file can't be opened or decoded, otherwise the track's length if
    /// it could be determined.
    fn load(&self, path: &Path) -> Option<Option<Duration>> {
        let file = File::open(path).ok()?;
        // `Decoder::try_from(File)` (rather than wrapping it in a `BufReader`
        // ourselves) sets the byte length, which is what lets `total_duration`
        // work for formats like mp3 that don't carry timing info directly.
        let source = Decoder::try_from(file).ok()?;
        let duration = source.total_duration();
        self.sink.clear();
        self.sink.append(source);
        self.sink.play();
        Some(duration)
    }

    /// Runs for the life of the process: notices when a track has finished
    /// playing on its own, and replays it if looping is on.
    fn watch_for_track_end(&self) {
        loop {
            thread::sleep(Duration::from_millis(300));
            let inner = self.inner.lock().unwrap();
            if !self.sink.empty() {
                continue;
            }
            let (Some(track), true) = (inner.track.clone(), inner.looping) else {
                continue;
            };
            drop(inner);
            let _ = self.load(&track.path);
        }
    }
}

impl Player for RodioPlayer {
    fn select(&self, path: &Path, display_name: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(duration) = self.load(path) {
            inner.track = Some(Track {
                path: path.to_path_buf(),
                display_name: display_name.to_string(),
                duration,
            });
        }
    }

    fn toggle_play_pause(&self) {
        let inner = self.inner.lock().unwrap();
        let Some(track) = inner.track.clone() else {
            return;
        };
        if self.sink.empty() {
            // The track finished on its own - "play" means start it over.
            drop(inner);
            let _ = self.load(&track.path);
        } else if self.sink.is_paused() {
            self.sink.play();
        } else {
            self.sink.pause();
        }
    }

    fn restart(&self) {
        let inner = self.inner.lock().unwrap();
        if let Some(track) = inner.track.clone() {
            drop(inner);
            let _ = self.load(&track.path);
        }
    }

    fn set_loop(&self, looping: bool) {
        self.inner.lock().unwrap().looping = looping;
    }

    fn status(&self) -> PlayerStatus {
        let inner = self.inner.lock().unwrap();
        let playing = inner.track.is_some() && !self.sink.empty() && !self.sink.is_paused();
        let position = if inner.track.is_some() {
            self.sink.get_pos().as_secs_f64()
        } else {
            0.0
        };
        let duration = inner
            .track
            .as_ref()
            .and_then(|t| t.duration)
            .map(|d| d.as_secs_f64());
        PlayerStatus {
            file: inner.track.as_ref().map(|t| t.display_name.clone()),
            playing,
            looping: inner.looping,
            position,
            duration,
        }
    }
}
