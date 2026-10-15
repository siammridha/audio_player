use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use rodio::{Decoder, Source};

use super::{
    CaptureStatus, Mode, OutputChoice, OutputDevice, PersistedState, Player, PlayerStatus,
    device_watch,
};
use crate::capture;

/// ALSA device used in Automatic mode: the external USB sound card the
/// player is meant to play through, addressed by its ALSA card name
/// ("Device", visible in `aplay -l`) rather than a card number, since the
/// number can shift depending on what else is plugged in. Override with the
/// `AUDIO_DEVICE` env var.
///
/// `plughw`, not `hw`/`default`: only using `plughw` gets ALSA's software
/// rate conversion, needed for files whose sample rate doesn't match what
/// the card runs at natively.
pub const DEFAULT_DEVICE: &str = "plughw:CARD=Device,DEV=0";

/// The USB card's ALSA short name (the same "Device" baked into
/// `DEFAULT_DEVICE`), watched for appearing/disappearing in Automatic mode.
/// Not read from `AUDIO_DEVICE` at runtime, since that env var can be
/// overridden to something else entirely.
const PRIMARY_CARD_NAME: &str = "Device";

/// Fallback ALSA device, used whenever the wanted card isn't present: the
/// Wyse 3040's own built-in sound chip, which is always there once its
/// firmware has loaded. Override with the `AUDIO_DEVICE_FALLBACK` env var.
pub const DEFAULT_FALLBACK_DEVICE: &str = "plughw:CARD=rt5672,DEV=0";

const CHUNK_FRAMES: usize = 4096;
/// Smaller chunks and a short device buffer for live sound, so it's heard
/// with little delay.
const LIVE_CHUNK_FRAMES: usize = 1024;
const LIVE_BUFFER_US: u32 = 100_000;
const LIVE_PERIOD_US: u32 = 20_000;
const LIVE_ERROR_PAUSE: Duration = Duration::from_millis(500);

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
    output_choice: OutputChoice,
}

/// The card the feeder plays through whenever it's present, and the ALSA
/// device string to open it with.
struct Target {
    card: String,
    device: String,
}

impl Target {
    fn for_choice(choice: &OutputChoice, auto_device: &str) -> Self {
        match choice {
            OutputChoice::Auto => Self {
                card: PRIMARY_CARD_NAME.to_string(),
                device: auto_device.to_string(),
            },
            OutputChoice::Card(id) => Self {
                card: id.clone(),
                device: format!("plughw:CARD={id},DEV=0"),
            },
        }
    }
}

/// Sent from `Player` trait methods (HTTP worker threads) to the feeder
/// thread, which is the sole owner of the ALSA device.
enum Command {
    /// The `u64` is the initial `frames_written` value to report - 0 for a
    /// fresh play, or a computed offset when resuming mid-track (see
    /// `AlsaPlayer::load_at`).
    Play(PathBuf, BoxedSource, u64),
    TogglePlayPause,
    /// A confirmed flip in the watched card's presence, from `device_watch`.
    /// Carries no flag: it may be about a card that was the target before a
    /// `SetOutput`, so the feeder re-checks reality instead.
    CardChanged,
    SetOutput(OutputChoice),
    Unload,
    /// Capture mode's live sound, played instead of the track while set.
    Live(Option<BoxedSource>),
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
    /// The card id the feeder is currently playing through, or `None` for
    /// the built-in fallback. Only locked briefly, never across an ALSA
    /// write.
    active_card: Mutex<Option<String>>,
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
    /// Set while in Capture mode.
    capture: Mutex<Option<capture::Session>>,
    capture_choice: Arc<Mutex<Option<String>>>,
}

impl AlsaPlayer {
    /// Opens `fallback_device` for playback right away (this has to
    /// succeed - it's the Wyse 3040's own sound chip, expected to always be
    /// there) and starts the background feeder thread that owns the active
    /// PCM handle for the life of the process. Also starts watching the card
    /// `output_choice` wants for hotplug; the feeder switches over to it once
    /// it's confirmed present, and back to `fallback_device` if it's later
    /// removed. `auto_device` is what Automatic mode plays through.
    pub fn new(
        auto_device: &str,
        fallback_device: &str,
        output_choice: OutputChoice,
    ) -> anyhow::Result<Arc<Self>> {
        let pcm = PCM::new(fallback_device, Direction::Playback, false).map_err(|e| {
            anyhow::anyhow!("failed to open ALSA fallback device {fallback_device:?}: {e}")
        })?;
        let target = Target::for_choice(&output_choice, auto_device);
        crate::log::info(&format!(
            "audio-player: starting on the built-in speaker ({fallback_device}), checking for the {} sound card...",
            target.card
        ));

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let playback = Arc::new(PlaybackState::default());
        playback
            .volume_bits
            .store(1.0f32.to_bits(), Ordering::Relaxed);
        let inner = Arc::new(Mutex::new(Inner {
            track: None,
            looping: true,
            output_choice,
        }));
        let watched_card = Arc::new(Mutex::new(target.card.clone()));

        let output = Output {
            pcm,
            device: fallback_device.to_string(),
            configured: None,
        };
        let feeder = Feeder {
            output,
            target,
            auto_device: auto_device.to_string(),
            fallback_device: fallback_device.to_string(),
            watched_card: Arc::clone(&watched_card),
            playback: Arc::clone(&playback),
            inner: Arc::clone(&inner),
        };
        thread::spawn(move || feeder_loop(feeder, cmd_rx));

        let watch_tx = cmd_tx.clone();
        device_watch::watch(watched_card, move |_| {
            let _ = watch_tx.send(Command::CardChanged);
        });

        Ok(Arc::new(Self {
            inner,
            playback,
            cmd_tx,
            capture: Mutex::new(None),
            capture_choice: Arc::new(Mutex::new(None)),
        }))
    }

    /// Decodes `path` from scratch and hands it to the feeder thread to
    /// play, replacing whatever was playing before. Returns `None` (leaving
    /// the old track queued) if the file can't be opened or decoded,
    /// otherwise the track's length if it could be determined.
    fn load(&self, path: &Path) -> Option<Option<Duration>> {
        self.load_at(path, Duration::ZERO)
    }

    /// Like `load`, but seeks the freshly-decoded source to `start` before
    /// handing it to the feeder thread - used to resume mid-track after a
    /// restore. Seeking is best-effort and permanently approximate for
    /// compressed formats (mp3 seeks to the nearest frame), so the
    /// reported position reflects the requested `start`, not necessarily
    /// the exact landed position; if the seek fails outright, playback
    /// just starts from the beginning instead of failing the load.
    fn load_at(&self, path: &Path, start: Duration) -> Option<Option<Duration>> {
        let (mut source, duration) = decode(path)?;
        let initial_frames = if start > Duration::ZERO {
            match source.try_seek(start) {
                Ok(()) => (start.as_secs_f64() * source.sample_rate().get() as f64).round() as u64,
                Err(e) => {
                    crate::log::error(&format!(
                        "audio-player: seek to {start:?} failed ({e}), starting {path:?} from the beginning"
                    ));
                    0
                }
            }
        } else {
            0
        };
        self.cmd_tx
            .send(Command::Play(path.to_path_buf(), source, initial_frames))
            .ok()?;
        Some(duration)
    }

    fn is_playing(&self) -> bool {
        self.inner.lock().unwrap().track.is_some()
            && self.playback.active.load(Ordering::Relaxed)
            && !self.playback.paused.load(Ordering::Relaxed)
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
        crate::log::debug(&format!(
            "audio-player: volume set to {:.0}%",
            clamped * 100.0
        ));
    }

    fn restore(&self, path: &Path, snapshot: &PersistedState) {
        let mut inner = self.inner.lock().unwrap();
        let start = Duration::from_secs_f64(snapshot.position.max(0.0));
        if let Some(duration) = self.load_at(path, start) {
            inner.track = Some(Track {
                path: path.to_path_buf(),
                display_name: snapshot.file.clone(),
                duration,
            });
            inner.looping = snapshot.looping;
        }
        drop(inner);
        self.set_volume(snapshot.volume);
        // Play always resets `paused` to false before this is processed, and
        // both sends are on the same Sender from the same thread, so mpsc's
        // FIFO ordering guarantees TogglePlayPause lands after Play and
        // flips paused to true. (A CardChanged message could interleave
        // between them from the hotplug-watch thread, but it never touches
        // `paused`/`active`/`frames_written`, so it's harmless here.)
        if !snapshot.playing {
            let _ = self.cmd_tx.send(Command::TogglePlayPause);
        }
    }

    fn unload(&self) {
        self.inner.lock().unwrap().track = None;
        let _ = self.cmd_tx.send(Command::Unload);
    }

    fn output_devices(&self) -> Vec<OutputDevice> {
        device_watch::cards()
    }

    fn set_output(&self, choice: OutputChoice) {
        self.inner.lock().unwrap().output_choice = choice.clone();
        let _ = self.cmd_tx.send(Command::SetOutput(choice));
    }

    fn set_mode(&self, mode: Mode) {
        let mut session = self.capture.lock().unwrap();
        match mode {
            Mode::Capture if session.is_none() => {
                // Set here rather than sent to the feeder, so the status
                // returned (and saved) right after already says paused.
                if self.is_playing() {
                    self.playback.paused.store(true, Ordering::Relaxed);
                    crate::log::info("audio-player: paused playback");
                }
                let live_tx = self.cmd_tx.clone();
                *session = Some(capture::Session::start(
                    Arc::clone(&self.capture_choice),
                    move |source| {
                        let _ = live_tx.send(Command::Live(Some(Box::new(source))));
                    },
                ));
                crate::log::info("audio-player: switched to Capture mode");
            }
            Mode::Play if session.is_some() => {
                // Dropping the session stops capturing and finishes any
                // recording.
                *session = None;
                let _ = self.cmd_tx.send(Command::Live(None));
                crate::log::info("audio-player: switched to Play mode");
            }
            _ => {}
        }
    }

    fn capture_devices(&self) -> Vec<OutputDevice> {
        device_watch::capture_cards()
    }

    fn set_capture_device(&self, id: Option<String>) {
        *self.capture_choice.lock().unwrap() = id;
        if let Some(session) = self.capture.lock().unwrap().as_ref() {
            session.reopen();
        }
    }

    fn start_recording(&self, path: &Path) -> anyhow::Result<()> {
        match self.capture.lock().unwrap().as_ref() {
            Some(session) => session.start_recording(path),
            None => anyhow::bail!("not in Capture mode"),
        }
    }

    fn stop_recording(&self) {
        if let Some(session) = self.capture.lock().unwrap().as_ref() {
            session.stop_recording();
        }
    }

    fn status(&self) -> PlayerStatus {
        let device_choice = self.capture_choice.lock().unwrap().clone();
        let session = self.capture.lock().unwrap();
        let in_capture = session.is_some();
        let capture = match session.as_ref() {
            Some(session) => {
                let snap = session.snapshot();
                // The card takes a moment to open after a switch. Name the
                // one being opened meanwhile, so the page doesn't flash its
                // no-device layout.
                let card = snap.card.or_else(|| capture::pick_card(&device_choice));
                CaptureStatus {
                    device: card.as_ref().map(|c| c.id.clone()),
                    device_name: card.map(|c| c.name),
                    device_choice,
                    recording: snap.recording.is_some(),
                    file: snap.recording,
                    elapsed: snap.elapsed,
                    bytes: snap.bytes,
                    level: snap.level,
                }
            }
            // Not opened, so the page can name the card before switching.
            None => {
                let card = capture::pick_card(&device_choice);
                CaptureStatus {
                    device: card.as_ref().map(|c| c.id.clone()),
                    device_name: card.map(|c| c.name),
                    device_choice,
                    ..CaptureStatus::default()
                }
            }
        };
        drop(session);
        let playing = self.is_playing();
        let inner = self.inner.lock().unwrap();
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
            output: self
                .playback
                .active_card
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "built-in".to_string()),
            output_choice: inner.output_choice.clone(),
            mode: if in_capture {
                Mode::Capture
            } else {
                Mode::Play
            },
            capture,
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

/// The open ALSA device the feeder is writing to.
struct Output {
    pcm: PCM,
    /// The device string `pcm` was opened with.
    device: String,
    configured: Option<Setup>,
}

/// What the device is set up for: rate, channel count, and whether it's
/// tuned for live sound.
type Setup = (u32, u16, bool);

/// The source the feeder plays from: the live sound when set, otherwise the
/// track. The flag says which.
fn active<'a>(
    live: &'a Option<BoxedSource>,
    current: &'a Option<(PathBuf, BoxedSource)>,
) -> Option<(&'a BoxedSource, bool)> {
    live.as_ref()
        .map(|s| (s, true))
        .or_else(|| current.as_ref().map(|(_, s)| (s, false)))
}

/// Everything the feeder thread owns besides the current track.
struct Feeder {
    output: Output,
    target: Target,
    auto_device: String,
    fallback_device: String,
    /// Shared with `device_watch`, so it watches whichever card `target`
    /// names.
    watched_card: Arc<Mutex<String>>,
    playback: Arc<PlaybackState>,
    inner: Arc<Mutex<Inner>>,
}

impl Feeder {
    /// Plays through the target card if it's present, otherwise the
    /// built-in fallback.
    fn follow_target(&mut self, current: Option<(&BoxedSource, bool)>) {
        // Checked right before opening: closes the gap between the
        // watcher's last confirmation and this exact moment.
        if device_watch::card_present(&self.target.card) {
            let device = self.target.device.clone();
            let card = self.target.card.clone();
            self.switch_to(&device, Some(card), current);
        } else {
            let device = self.fallback_device.clone();
            self.switch_to(&device, None, current);
        }
    }

    /// Makes `device` the active output (`card` is `None` for the built-in
    /// fallback) and carries on with the current source mid-way. Doesn't
    /// reopen a device that's already open, since a `hw` device can only be
    /// opened once at a time.
    fn switch_to(
        &mut self,
        device: &str,
        card: Option<String>,
        current: Option<(&BoxedSource, bool)>,
    ) {
        if self.output.device != device {
            match PCM::new(device, Direction::Playback, false) {
                Ok(pcm) => {
                    self.output.pcm = pcm;
                    self.output.device = device.to_string();
                    self.output.configured = None;
                    reconfigure_for_current(&mut self.output, current, &self.playback);
                }
                Err(e) => {
                    crate::log::error(&format!(
                        "audio-player: failed to open {device} ({e}), staying on {}",
                        self.output.device
                    ));
                    return;
                }
            }
        }
        let mut active_card = self.playback.active_card.lock().unwrap();
        if *active_card != card {
            match &card {
                Some(id) => {
                    crate::log::info(&format!("audio-player: switching to the {id} sound card"))
                }
                None => crate::log::info("audio-player: falling back to the built-in speaker"),
            }
            *active_card = card;
        }
    }
}

/// Runs for the life of the process. This thread is the only thing that
/// ever touches the ALSA device: it applies commands from `Player` trait
/// methods, pulls samples from the current track (or Capture mode's live
/// sound) and writes them to the device, and notices when a track ends so it
/// can replay it if looping is on.
fn feeder_loop(mut feeder: Feeder, cmd_rx: mpsc::Receiver<Command>) {
    let playback = Arc::clone(&feeder.playback);
    let inner = Arc::clone(&feeder.inner);
    let mut current: Option<(PathBuf, BoxedSource)> = None;
    let mut live: Option<BoxedSource> = None;

    loop {
        let idle = live.is_none() && (current.is_none() || playback.paused.load(Ordering::Relaxed));
        let cmd = if idle {
            cmd_rx.recv_timeout(Duration::from_millis(300))
        } else {
            cmd_rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
            })
        };

        match cmd {
            Ok(Command::Play(path, source, initial_frames)) => {
                if let Err(e) = ensure_configured(&mut feeder.output, &source, false) {
                    crate::log::error(&format!(
                        "audio-player: failed to configure ALSA device: {e}"
                    ));
                    current = None;
                    playback.active.store(false, Ordering::Relaxed);
                    continue;
                }
                playback
                    .sample_rate
                    .store(source.sample_rate().get(), Ordering::Relaxed);
                playback
                    .frames_written
                    .store(initial_frames, Ordering::Relaxed);
                playback.paused.store(false, Ordering::Relaxed);
                playback.active.store(true, Ordering::Relaxed);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                crate::log::info(&format!("audio-player: playing {name}"));
                current = Some((path, source));
                continue;
            }
            Ok(Command::TogglePlayPause) => {
                if let Some((_, source)) = &current {
                    let was_paused = playback.paused.fetch_xor(true, Ordering::Relaxed);
                    if was_paused {
                        // The device may still be set up for live sound.
                        if let Err(e) = ensure_configured(&mut feeder.output, source, false) {
                            crate::log::error(&format!(
                                "audio-player: failed to configure ALSA device: {e}"
                            ));
                        }
                        let _ = feeder.output.pcm.prepare();
                        crate::log::info("audio-player: resumed playback");
                    } else {
                        crate::log::info("audio-player: paused playback");
                    }
                }
                continue;
            }
            Ok(Command::CardChanged) => {
                feeder.follow_target(active(&live, &current));
                continue;
            }
            Ok(Command::SetOutput(choice)) => {
                feeder.target = Target::for_choice(&choice, &feeder.auto_device);
                *feeder.watched_card.lock().unwrap() = feeder.target.card.clone();
                feeder.follow_target(active(&live, &current));
                continue;
            }
            Ok(Command::Unload) => {
                current = None;
                playback.active.store(false, Ordering::Relaxed);
                continue;
            }
            Ok(Command::Live(source)) => {
                if let Some(source) = &source
                    && let Err(e) = ensure_configured(&mut feeder.output, source, true)
                {
                    crate::log::error(&format!(
                        "audio-player: failed to configure ALSA device for live sound: {e}"
                    ));
                }
                live = source;
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let volume = f32::from_bits(playback.volume_bits.load(Ordering::Relaxed));

        if let Some(source) = live.as_mut() {
            let channels = source.channels().get() as usize;
            let buf: Vec<i16> = source
                .by_ref()
                .take(LIVE_CHUNK_FRAMES * channels)
                .map(|s| to_i16(s, volume))
                .collect();
            if buf.is_empty() {
                live = None;
            } else if let Err(e) = write_i16(&feeder.output.pcm, &buf) {
                crate::log::error(&format!("audio-player: ALSA write error: {e}"));
                thread::sleep(LIVE_ERROR_PAUSE);
            }
            continue;
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
            .map(|s| to_i16(s, volume))
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

        match write_i16(&feeder.output.pcm, &buf) {
            Ok(frames) => {
                playback
                    .frames_written
                    .fetch_add(frames as u64, Ordering::Relaxed);
            }
            Err(e) => {
                crate::log::error(&format!("audio-player: ALSA write error: {e}"));
                current = None;
                playback.active.store(false, Ordering::Relaxed);
            }
        }
    }
}

fn to_i16(sample: f32, volume: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * volume * i16::MAX as f32) as i16
}

/// Sets the device up for `source` unless it already is.
fn ensure_configured(output: &mut Output, source: &BoxedSource, live: bool) -> alsa::Result<()> {
    let setup = (source.sample_rate().get(), source.channels().get(), live);
    if output.configured != Some(setup) {
        configure(&output.pcm, setup)?;
        output.configured = Some(setup);
    }
    Ok(())
}

/// Negotiates hardware/software params for a source's rate and channel
/// count, and primes the device to start playing as soon as one period's
/// worth of samples has been written (rather than waiting to fill the
/// whole buffer, which would add startup latency). Live sound also gets a
/// short buffer, since the buffer's length is how late it's heard.
fn configure(pcm: &PCM, (rate, channels, live): Setup) -> alsa::Result<()> {
    // Ignore errors: this fails if the device was never started, which is
    // fine - there's nothing to drop yet.
    let _ = pcm.drop();

    let hwp = HwParams::any(pcm)?;
    hwp.set_access(Access::RWInterleaved)?;
    hwp.set_format(Format::s16())?;
    hwp.set_channels(channels as u32)?;
    hwp.set_rate(rate, ValueOr::Nearest)?;
    if live {
        hwp.set_buffer_time_near(LIVE_BUFFER_US, ValueOr::Nearest)?;
        hwp.set_period_time_near(LIVE_PERIOD_US, ValueOr::Nearest)?;
    }
    pcm.hw_params(&hwp)?;

    let period = pcm.hw_params_current()?.get_period_size()?;
    let swp = pcm.sw_params_current()?;
    swp.set_start_threshold(period)?;
    pcm.sw_params(&swp)?;

    pcm.prepare()
}

/// If something is playing, negotiates hw params for it on a freshly-opened
/// handle after a device switch, and primes it to keep playing mid-way.
/// Leaves `frames_written` alone, so the reported position stays continuous
/// across the switch instead of jumping back to zero.
fn reconfigure_for_current(
    output: &mut Output,
    current: Option<(&BoxedSource, bool)>,
    playback: &PlaybackState,
) {
    let Some((source, live)) = current else {
        return;
    };
    match ensure_configured(output, source, live) {
        Ok(()) if !live => {
            playback
                .sample_rate
                .store(source.sample_rate().get(), Ordering::Relaxed);
        }
        Ok(()) => {}
        Err(e) => {
            crate::log::error(&format!(
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
