use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use rodio::{Decoder, Source};

use super::{Player, PlayerStatus};

const DEVICE: &str = "default";
const CHUNK_FRAMES: usize = 4096;

type BoxedSource = Box<dyn Source<Item = f32> + Send>;

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

/// Sent from `Player` trait methods (HTTP worker threads) to the feeder
/// thread, which is the sole owner of the ALSA device.
enum Command {
    Play(PathBuf, BoxedSource),
    TogglePlayPause,
}

/// Playback progress, written only by the feeder thread and read by
/// `status()`. Atomics rather than a mutex so status polling never blocks
/// behind a blocking ALSA write.
#[derive(Default)]
struct PlaybackState {
    sample_rate: AtomicU32,
    frames_written: AtomicU64,
    /// A track is loaded and hasn't finished playing (naturally or via error).
    active: AtomicBool,
    paused: AtomicBool,
}

/// Plays audio for real, through the machine's own sound card via ALSA.
///
/// Talks to ALSA directly instead of through rodio's `cpal`-based output:
/// on this device's SOF/rt5670 driver, cpal 0.17's hardware-timestamp check
/// fails on every period after the first, silently dropping all audio.
/// Raw ALSA calls don't do that check.
pub struct AlsaPlayer {
    inner: Arc<Mutex<Inner>>,
    playback: Arc<PlaybackState>,
    cmd_tx: mpsc::Sender<Command>,
}

impl AlsaPlayer {
    /// Opens the default ALSA playback device and starts the background
    /// feeder thread that owns it for the life of the process.
    pub fn new() -> anyhow::Result<Arc<Self>> {
        let pcm = PCM::new(DEVICE, Direction::Playback, false)
            .map_err(|e| anyhow::anyhow!("failed to open ALSA device {DEVICE:?}: {e}"))?;

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let playback = Arc::new(PlaybackState::default());
        let inner = Arc::new(Mutex::new(Inner {
            track: None,
            looping: false,
        }));

        let feeder_playback = Arc::clone(&playback);
        let feeder_inner = Arc::clone(&inner);
        thread::spawn(move || feeder_loop(pcm, cmd_rx, feeder_playback, feeder_inner));

        Ok(Arc::new(Self {
            inner,
            playback,
            cmd_tx,
        }))
    }

    /// Decodes `path` from scratch and hands it to the feeder thread to
    /// play, replacing whatever was playing before. Returns `None` (leaving
    /// the old track queued) if the file can't be opened or decoded,
    /// otherwise the track's length if it could be determined.
    fn load(&self, path: &Path) -> Option<Option<Duration>> {
        let (source, duration) = decode(path)?;
        self.cmd_tx
            .send(Command::Play(path.to_path_buf(), source))
            .ok()?;
        Some(duration)
    }
}

impl Player for AlsaPlayer {
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
        drop(inner);
        if self.playback.active.load(Ordering::Relaxed) {
            let _ = self.cmd_tx.send(Command::TogglePlayPause);
        } else {
            // The track finished on its own - "play" means start it over.
            let _ = self.load(&track.path);
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
        let playing = inner.track.is_some()
            && self.playback.active.load(Ordering::Relaxed)
            && !self.playback.paused.load(Ordering::Relaxed);
        let position = if inner.track.is_some() {
            let rate = self.playback.sample_rate.load(Ordering::Relaxed);
            let frames = self.playback.frames_written.load(Ordering::Relaxed);
            if rate > 0 {
                frames as f64 / rate as f64
            } else {
                0.0
            }
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

/// Opens and decodes `path` from scratch, the same way for a fresh
/// selection and for reloading a track that's looping.
fn decode(path: &Path) -> Option<(BoxedSource, Option<Duration>)> {
    let file = File::open(path).ok()?;
    // `Decoder::try_from(File)` (rather than wrapping it in a `BufReader`
    // ourselves) sets the byte length, which is what lets `total_duration`
    // work for formats like mp3 that don't carry timing info directly.
    let source = Decoder::try_from(file).ok()?;
    let duration = source.total_duration();
    Some((Box::new(source), duration))
}

/// Runs for the life of the process. This thread is the only thing that
/// ever touches `pcm`: it applies commands from `Player` trait methods,
/// pulls samples from the current track and writes them to the device, and
/// notices when a track ends so it can replay it if looping is on.
fn feeder_loop(
    pcm: PCM,
    cmd_rx: mpsc::Receiver<Command>,
    playback: Arc<PlaybackState>,
    inner: Arc<Mutex<Inner>>,
) {
    let mut current: Option<(PathBuf, BoxedSource)> = None;
    let mut configured: Option<(u32, u16)> = None;

    loop {
        let idle = current.is_none() || playback.paused.load(Ordering::Relaxed);
        let cmd = if idle {
            cmd_rx.recv_timeout(Duration::from_millis(300))
        } else {
            cmd_rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
            })
        };

        match cmd {
            Ok(Command::Play(path, source)) => {
                let rate = source.sample_rate().get();
                let channels = source.channels().get();
                if configured != Some((rate, channels)) {
                    if let Err(e) = configure(&pcm, rate, channels) {
                        eprintln!("audio-player: failed to configure ALSA device: {e}");
                        current = None;
                        playback.active.store(false, Ordering::Relaxed);
                        continue;
                    }
                    configured = Some((rate, channels));
                }
                playback.sample_rate.store(rate, Ordering::Relaxed);
                playback.frames_written.store(0, Ordering::Relaxed);
                playback.paused.store(false, Ordering::Relaxed);
                playback.active.store(true, Ordering::Relaxed);
                current = Some((path, source));
                continue;
            }
            Ok(Command::TogglePlayPause) => {
                if current.is_some() {
                    let was_paused = playback.paused.fetch_xor(true, Ordering::Relaxed);
                    if was_paused {
                        let _ = pcm.prepare();
                    }
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let Some((path, source)) = current.as_mut() else {
            continue;
        };
        if playback.paused.load(Ordering::Relaxed) {
            continue;
        }

        let channels = source.channels().get() as usize;
        let buf: Vec<i16> = source
            .by_ref()
            .take(CHUNK_FRAMES * channels)
            .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();

        if buf.is_empty() {
            let looping = inner.lock().unwrap().looping;
            let reloaded = looping.then(|| decode(path)).flatten();
            if let Some((next_source, _)) = reloaded {
                *source = next_source;
                playback.frames_written.store(0, Ordering::Relaxed);
            } else {
                current = None;
                playback.active.store(false, Ordering::Relaxed);
            }
            continue;
        }

        match write_i16(&pcm, &buf) {
            Ok(frames) => {
                playback
                    .frames_written
                    .fetch_add(frames as u64, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("audio-player: ALSA write error: {e}");
                current = None;
                playback.active.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// Negotiates hardware/software params for a track's rate and channel
/// count, and primes the device to start playing as soon as one period's
/// worth of samples has been written (rather than waiting to fill the
/// whole buffer, which would add startup latency).
fn configure(pcm: &PCM, rate: u32, channels: u16) -> alsa::Result<()> {
    // Ignore errors: this fails if the device was never started, which is
    // fine - there's nothing to drop yet.
    let _ = pcm.drop();

    let hwp = HwParams::any(pcm)?;
    hwp.set_access(Access::RWInterleaved)?;
    hwp.set_format(Format::s16())?;
    hwp.set_channels(channels as u32)?;
    hwp.set_rate(rate, ValueOr::Nearest)?;
    pcm.hw_params(&hwp)?;

    let period = pcm.hw_params_current()?.get_period_size()?;
    let swp = pcm.sw_params_current()?;
    swp.set_start_threshold(period)?;
    pcm.sw_params(&swp)?;

    pcm.prepare()
}

/// Writes one chunk of interleaved 16-bit samples, recovering once from a
/// buffer underrun or stream suspend before giving up.
fn write_i16(pcm: &PCM, buf: &[i16]) -> alsa::Result<usize> {
    let io = pcm.io_i16()?;
    match io.writei(buf) {
        Ok(frames) => Ok(frames),
        Err(e) => {
            pcm.recover(e.errno(), false)?;
            io.writei(buf)
        }
    }
}
