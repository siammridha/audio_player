use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use rodio::{Decoder, Source};

use super::{Player, PlayerStatus, device_watch};

/// Default ALSA device: the external USB sound card the player is meant to
/// play through, addressed by its ALSA card name ("Device", visible in
/// `aplay -l`) rather than a card number, since the number can shift
/// depending on what else is plugged in. Override with the `AUDIO_DEVICE`
/// env var.
///
/// `plughw`, not `hw`/`default`: only using `plughw` gets ALSA's software
/// rate conversion, needed for files whose sample rate doesn't match what
/// the card runs at natively.
pub const DEFAULT_DEVICE: &str = "plughw:CARD=Device,DEV=0";

/// The USB card's ALSA short name (the same "Device" baked into
/// `DEFAULT_DEVICE`), used to watch for it appearing/disappearing. Not
/// read from `AUDIO_DEVICE` at runtime, since that env var can be
/// overridden to something else entirely.
pub const PRIMARY_CARD_NAME: &str = "Device";

/// Fallback ALSA device, used whenever the USB card isn't present: the Wyse
/// 3040's own built-in sound chip, which is always there once its firmware
/// has loaded. Override with the `AUDIO_DEVICE_FALLBACK` env var.
pub const DEFAULT_FALLBACK_DEVICE: &str = "plughw:CARD=rt5672,DEV=0";

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
    /// A confirmed flip in the USB card's presence, from `device_watch`.
    DevicePresence(bool),
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
    /// Volume, 0.0..=1.0, stored as `f32::to_bits` since there's no `AtomicF32`.
    /// Read by the feeder thread on every chunk, so an atomic rather than a
    /// mutex to keep that hot path lock-free.
    volume_bits: AtomicU32,
    /// Whether the feeder is currently playing through the USB card
    /// (`primary_device`) rather than the built-in fallback.
    using_primary: AtomicBool,
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
    /// Opens `fallback_device` for playback right away (this has to
    /// succeed - it's the Wyse 3040's own sound chip, expected to always be
    /// there) and starts the background feeder thread that owns the active
    /// PCM handle for the life of the process. Also starts watching
    /// `primary_card_name` for hotplug; the feeder switches over to
    /// `primary_device` once that card is confirmed present, and back to
    /// `fallback_device` if it's later removed.
    pub fn new(
        primary_device: &str,
        fallback_device: &str,
        primary_card_name: &'static str,
    ) -> anyhow::Result<Arc<Self>> {
        let pcm = PCM::new(fallback_device, Direction::Playback, false).map_err(|e| {
            anyhow::anyhow!("failed to open ALSA fallback device {fallback_device:?}: {e}")
        })?;
        crate::log::print_line(&format!(
            "audio-player: starting on the built-in speaker ({fallback_device}), checking for the USB sound card ({primary_card_name})..."
        ));

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let playback = Arc::new(PlaybackState::default());
        playback
            .volume_bits
            .store(1.0f32.to_bits(), Ordering::Relaxed);
        let inner = Arc::new(Mutex::new(Inner {
            track: None,
            looping: true,
        }));

        let feeder_playback = Arc::clone(&playback);
        let feeder_inner = Arc::clone(&inner);
        let primary_device = primary_device.to_string();
        let fallback_device = fallback_device.to_string();
        thread::spawn(move || {
            feeder_loop(
                pcm,
                primary_device,
                fallback_device,
                primary_card_name,
                cmd_rx,
                feeder_playback,
                feeder_inner,
            )
        });

        let watch_tx = cmd_tx.clone();
        device_watch::watch(primary_card_name, move |present| {
            let _ = watch_tx.send(Command::DevicePresence(present));
        });

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

    fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        self.playback
            .volume_bits
            .store(clamped.to_bits(), Ordering::Relaxed);
        crate::log::print_line(&format!(
            "audio-player: volume set to {:.0}%",
            clamped * 100.0
        ));
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
            volume: f32::from_bits(self.playback.volume_bits.load(Ordering::Relaxed)),
            output: if self.playback.using_primary.load(Ordering::Relaxed) {
                "usb"
            } else {
                "built-in"
            },
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
    initial_pcm: PCM,
    primary_device: String,
    fallback_device: String,
    primary_card_name: &'static str,
    cmd_rx: mpsc::Receiver<Command>,
    playback: Arc<PlaybackState>,
    inner: Arc<Mutex<Inner>>,
) {
    let mut pcm = initial_pcm;
    let mut using_primary = false;
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
                        crate::log::eprint_line(&format!(
                            "audio-player: failed to configure ALSA device: {e}"
                        ));
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
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                crate::log::print_line(&format!("audio-player: playing {name}"));
                current = Some((path, source));
                continue;
            }
            Ok(Command::TogglePlayPause) => {
                if current.is_some() {
                    let was_paused = playback.paused.fetch_xor(true, Ordering::Relaxed);
                    if was_paused {
                        let _ = pcm.prepare();
                        crate::log::print_line("audio-player: resumed playback");
                    } else {
                        crate::log::print_line("audio-player: paused playback");
                    }
                }
                continue;
            }
            Ok(Command::DevicePresence(true)) => {
                // Re-check right before opening: closes the gap between the
                // watcher's last confirmation and this exact moment.
                if !using_primary && device_watch::card_present(primary_card_name) {
                    match PCM::new(&primary_device, Direction::Playback, false) {
                        Ok(new_pcm) => {
                            crate::log::print_line("audio-player: switching to the USB sound card");
                            pcm = new_pcm;
                            using_primary = true;
                            playback.using_primary.store(true, Ordering::Relaxed);
                            configured = None;
                            reconfigure_for_current(&pcm, &current, &mut configured, &playback);
                        }
                        Err(e) => {
                            crate::log::eprint_line(&format!(
                                "audio-player: USB sound card reported present but failed to open ({e}), staying on the fallback device"
                            ));
                        }
                    }
                }
                continue;
            }
            Ok(Command::DevicePresence(false)) => {
                if using_primary {
                    match PCM::new(&fallback_device, Direction::Playback, false) {
                        Ok(new_pcm) => {
                            crate::log::print_line(
                                "audio-player: falling back to the built-in speaker",
                            );
                            pcm = new_pcm;
                            using_primary = false;
                            playback.using_primary.store(false, Ordering::Relaxed);
                            configured = None;
                            reconfigure_for_current(&pcm, &current, &mut configured, &playback);
                        }
                        Err(e) => {
                            crate::log::eprint_line(&format!(
                                "audio-player: USB sound card disconnected but failed to open the fallback device: {e}"
                            ));
                        }
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
        let volume = f32::from_bits(playback.volume_bits.load(Ordering::Relaxed));
        let buf: Vec<i16> = source
            .by_ref()
            .take(CHUNK_FRAMES * channels)
            .map(|s| (s.clamp(-1.0, 1.0) * volume * i16::MAX as f32) as i16)
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
                crate::log::eprint_line(&format!("audio-player: ALSA write error: {e}"));
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

/// If a track is currently loaded, negotiates hw params for it on `pcm` - a
/// freshly-opened handle after a device switch - and primes it to keep
/// playing mid-track. Leaves `frames_written` alone, so the reported
/// position stays continuous across the switch instead of jumping back to
/// zero.
fn reconfigure_for_current(
    pcm: &PCM,
    current: &Option<(PathBuf, BoxedSource)>,
    configured: &mut Option<(u32, u16)>,
    playback: &PlaybackState,
) {
    let Some((_, source)) = current else {
        return;
    };
    let rate = source.sample_rate().get();
    let channels = source.channels().get();
    match configure(pcm, rate, channels) {
        Ok(()) => {
            *configured = Some((rate, channels));
            playback.sample_rate.store(rate, Ordering::Relaxed);
        }
        Err(e) => {
            crate::log::eprint_line(&format!(
                "audio-player: failed to configure ALSA device after switch: {e}"
            ));
        }
    }
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
