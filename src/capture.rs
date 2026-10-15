//! Capture mode: reads sound from a capture card, hands it to the playback
//! thread so it can be heard live, and writes it to a WAV file while
//! recording.

use std::ffi::CString;
use std::fs::File;
use std::io::BufWriter;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use rodio::{ChannelCount, SampleRate, Source};

use crate::player::{OutputDevice, device_watch};

const RATE: u32 = 48_000;
const PERIOD_FRAMES: usize = 1024;
/// Recording stops on its own once the music folder's disk has less free
/// space than this, so the system never runs completely out.
const MIN_FREE_BYTES: u64 = 50 * 1024 * 1024;
const RETRY_DELAY: Duration = Duration::from_secs(1);
/// How many periods may wait for the playback thread before new ones are
/// dropped. Keeps the live delay short if the two cards' clocks drift.
const LIVE_QUEUE_PERIODS: usize = 8;
const LIVE_WAIT: Duration = Duration::from_millis(20);
const SILENCE_FRAMES: usize = 256;
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// hound writes a plain 44-byte header for 16-bit PCM with 1 or 2 channels.
const WAV_HEADER_BYTES: u64 = 44;

type Writer = hound::WavWriter<BufWriter<File>>;

struct Recording {
    writer: Writer,
    dir: PathBuf,
}

enum Cmd {
    Record(Recording),
    /// The sender is told once the file is complete on disk.
    Stop(mpsc::Sender<()>),
    /// Re-picks the card (the user changed their choice).
    Reopen,
    Quit,
}

#[derive(Default)]
struct Shared {
    card: Mutex<Option<OutputDevice>>,
    /// Rate and channel count of the open card.
    format: Mutex<Option<(u32, u16)>>,
    level_bits: AtomicU32,
    /// Name of the file being recorded.
    recording: Mutex<Option<String>>,
    frames_recorded: AtomicU64,
}

/// What a `Session` is doing right now.
pub struct Snapshot {
    pub card: Option<OutputDevice>,
    pub recording: Option<String>,
    pub elapsed: f64,
    pub bytes: u64,
    pub level: f32,
}

/// One stay in Capture mode. Capturing starts when it's created and stops,
/// finishing any recording, when it's dropped.
pub struct Session {
    shared: Arc<Shared>,
    cmd_tx: mpsc::Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// `choice` is the card the user picked (read each time a card is
    /// opened). `on_live` is called with the live sound each time a card is
    /// opened; the source ends when that card is closed.
    pub fn start(
        choice: Arc<Mutex<Option<String>>>,
        on_live: impl Fn(LiveSource) + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let thread = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || run(&shared, &choice, &cmd_rx, &on_live))
        };
        Self {
            shared,
            cmd_tx,
            thread: Some(thread),
        }
    }

    pub fn reopen(&self) {
        let _ = self.cmd_tx.send(Cmd::Reopen);
    }

    pub fn start_recording(&self, path: &Path) -> anyhow::Result<()> {
        let mut recording = self.shared.recording.lock().unwrap();
        if recording.is_some() {
            anyhow::bail!("already recording");
        }
        let Some((rate, channels)) = *self.shared.format.lock().unwrap() else {
            anyhow::bail!("no capture device");
        };
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(path, spec)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        self.shared.frames_recorded.store(0, Ordering::Relaxed);
        *recording = Some(name.clone());
        let _ = self.cmd_tx.send(Cmd::Record(Recording { writer, dir }));
        crate::log::info(&format!("audio-player: recording to {name}"));
        Ok(())
    }

    pub fn stop_recording(&self) {
        if self.shared.recording.lock().unwrap().is_none() {
            return;
        }
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.cmd_tx.send(Cmd::Stop(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(STOP_TIMEOUT);
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let format = *self.shared.format.lock().unwrap();
        let recording = self.shared.recording.lock().unwrap().clone();
        let frames = self.shared.frames_recorded.load(Ordering::Relaxed);
        let (elapsed, bytes) = match (&recording, format) {
            (Some(_), Some((rate, channels))) => (
                frames as f64 / rate as f64,
                WAV_HEADER_BYTES + frames * channels as u64 * 2,
            ),
            _ => (0.0, 0),
        };
        Snapshot {
            card: self.shared.card.lock().unwrap().clone(),
            recording,
            elapsed,
            bytes,
            level: f32::from_bits(self.shared.level_bits.load(Ordering::Relaxed)),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Why `read_loop` returned.
enum Exit {
    Quit,
    Reopen,
    Lost,
}

fn run(
    shared: &Shared,
    choice: &Mutex<Option<String>>,
    cmd_rx: &mpsc::Receiver<Cmd>,
    on_live: &dyn Fn(LiveSource),
) {
    let mut recording: Option<Recording> = None;
    loop {
        let opened = pick_card(&choice.lock().unwrap()).and_then(|card| match open(&card.id) {
            Ok((pcm, rate, channels)) => Some((card, pcm, rate, channels)),
            Err(e) => {
                crate::log::error(&format!(
                    "audio-player: failed to open capture card {} ({e})",
                    card.id
                ));
                None
            }
        });
        let Some((card, pcm, rate, channels)) = opened else {
            // Nothing to capture from: wait for a card, still answering
            // commands.
            match cmd_rx.recv_timeout(RETRY_DELAY) {
                Ok(Cmd::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Ok(Cmd::Record(r)) => finish(shared, Some(r)),
                Ok(Cmd::Stop(ack)) => {
                    let _ = ack.send(());
                }
                Ok(Cmd::Reopen) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            continue;
        };

        crate::log::info(&format!(
            "audio-player: capturing from the {} sound card ({rate} Hz, {channels} ch)",
            card.id
        ));
        let (live_tx, live_rx) = mpsc::sync_channel(LIVE_QUEUE_PERIODS);
        *shared.card.lock().unwrap() = Some(card.clone());
        *shared.format.lock().unwrap() = Some((rate, channels));
        on_live(LiveSource::new(live_rx, rate, channels));

        let exit = read_loop(
            shared,
            choice,
            cmd_rx,
            &pcm,
            &card,
            (rate, channels),
            &live_tx,
            &mut recording,
        );

        *shared.card.lock().unwrap() = None;
        *shared.format.lock().unwrap() = None;
        shared.level_bits.store(0, Ordering::Relaxed);
        finish(shared, recording.take());
        match exit {
            Exit::Quit => return,
            Exit::Reopen => {}
            Exit::Lost => {
                crate::log::info(&format!(
                    "audio-player: the {} capture card stopped",
                    card.id
                ));
                thread::sleep(RETRY_DELAY);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn read_loop(
    shared: &Shared,
    choice: &Mutex<Option<String>>,
    cmd_rx: &mpsc::Receiver<Cmd>,
    pcm: &PCM,
    card: &OutputDevice,
    (rate, channels): (u32, u16),
    live_tx: &mpsc::SyncSender<Vec<i16>>,
    recording: &mut Option<Recording>,
) -> Exit {
    let Ok(io) = pcm.io_i16() else {
        return Exit::Lost;
    };
    let channels = channels as usize;
    let mut buf = vec![0i16; PERIOD_FRAMES * channels];
    let mut frames_since_check = 0usize;

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Cmd::Quit => return Exit::Quit,
                Cmd::Reopen => return Exit::Reopen,
                Cmd::Record(r) => *recording = Some(r),
                Cmd::Stop(ack) => {
                    finish(shared, recording.take());
                    let _ = ack.send(());
                }
            }
        }

        let frames = match io.readi(&mut buf) {
            Ok(frames) => frames,
            Err(e) => {
                if pcm.recover(e.errno(), true).is_ok() {
                    continue;
                }
                crate::log::error(&format!("audio-player: capture read error: {e}"));
                return Exit::Lost;
            }
        };
        let samples = &buf[..frames * channels];

        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        let level = peak as f32 / 32768.0;
        shared.level_bits.store(level.to_bits(), Ordering::Relaxed);

        let _ = live_tx.try_send(samples.to_vec());

        if let Some(r) = recording.as_mut() {
            let written = samples.iter().try_for_each(|&s| r.writer.write_sample(s));
            if let Err(e) = written {
                crate::log::error(&format!("audio-player: failed to write recording: {e}"));
                finish(shared, recording.take());
            } else {
                shared
                    .frames_recorded
                    .fetch_add(frames as u64, Ordering::Relaxed);
            }
        }

        frames_since_check += frames;
        if frames_since_check >= rate as usize {
            frames_since_check = 0;
            if let Some(r) = recording.as_mut() {
                // Rewrites the header, so the file stays playable if the
                // power goes out mid-recording.
                let _ = r.writer.flush();
                if low_on_space(disk_space(&r.dir).map(|(free, _)| free)) {
                    crate::log::error("audio-player: disk nearly full, stopping the recording");
                    finish(shared, recording.take());
                }
            } else if pick_card(&choice.lock().unwrap()).is_some_and(|c| c.id != card.id) {
                return Exit::Reopen;
            }
        }
    }
}

/// Finishes the file and clears the recording from the status.
fn finish(shared: &Shared, recording: Option<Recording>) {
    let name = shared.recording.lock().unwrap().take();
    shared.frames_recorded.store(0, Ordering::Relaxed);
    let Some(recording) = recording else {
        return;
    };
    match recording.writer.finalize() {
        Ok(()) => crate::log::info(&format!(
            "audio-player: saved recording {}",
            name.unwrap_or_default()
        )),
        Err(e) => crate::log::error(&format!("audio-player: failed to finish recording: {e}")),
    }
}

/// The card the user picked if it's ready, otherwise the newest ready card
/// that can record.
pub fn pick_card(choice: &Option<String>) -> Option<OutputDevice> {
    let cards = device_watch::ready_capture_cards();
    choice
        .as_ref()
        .and_then(|id| cards.iter().find(|c| &c.id == id).cloned())
        .or_else(|| cards.last().cloned())
}

/// Opens `card` for capture at 48 kHz, 16-bit, with the card's own channel
/// count (1 or 2), so a mono input isn't stored as stereo.
fn open(card: &str) -> alsa::Result<(PCM, u32, u16)> {
    let channels = native_channels(card).unwrap_or(2);
    let pcm = PCM::new(
        &format!("plughw:CARD={card},DEV=0"),
        Direction::Capture,
        false,
    )?;
    let rate = {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::s16())?;
        hwp.set_channels(channels as u32)?;
        hwp.set_rate(RATE, ValueOr::Nearest)?;
        hwp.set_period_size_near(PERIOD_FRAMES as alsa::pcm::Frames, ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
        pcm.hw_params_current()?.get_rate()?
    };
    pcm.prepare()?;
    Ok((pcm, rate, channels))
}

fn native_channels(card: &str) -> Option<u16> {
    let pcm = PCM::new(&format!("hw:CARD={card},DEV=0"), Direction::Capture, false).ok()?;
    let hwp = HwParams::any(&pcm).ok()?;
    Some(hwp.get_channels_min().ok()?.clamp(1, 2) as u16)
}

/// Free and total bytes on the disk that holds `dir`.
pub fn disk_space(dir: &Path) -> Option<(u64, u64)> {
    let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid C string and `stat` is a plain struct that
    // statvfs fills in.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    Some((stat.f_bavail as u64 * block, stat.f_blocks as u64 * block))
}

fn low_on_space(free: Option<u64>) -> bool {
    free.is_some_and(|free| free < MIN_FREE_BYTES)
}

/// A path in `dir` for a new recording named after the local time, with
/// "-2", "-3", ... added if a file with that name already exists.
const FILE_PREFIX: &str = "capture-";

/// Whether `name` is a file Capture mode made.
pub fn is_recording_name(name: &str) -> bool {
    name.starts_with(FILE_PREFIX)
}

pub fn new_recording_path(dir: &Path, unix_secs: i64) -> PathBuf {
    let base = base_name(&local_time(unix_secs));
    let mut path = dir.join(format!("{base}.wav"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.wav"));
        n += 1;
    }
    path
}

fn local_time(unix_secs: i64) -> libc::tm {
    // SAFETY: localtime_r only writes into `tm`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&(unix_secs as _), &mut tm) };
    tm
}

fn base_name(tm: &libc::tm) -> String {
    format!(
        "{FILE_PREFIX}{:04}-{:02}-{:02}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// The captured sound as a rodio source, for the playback thread. Gives
/// short runs of silence when no samples arrive in time, so a slow input
/// never stalls playback. Ends when the card is closed.
pub struct LiveSource {
    rx: mpsc::Receiver<Vec<i16>>,
    buf: Vec<i16>,
    pos: usize,
    rate: SampleRate,
    channels: ChannelCount,
}

impl LiveSource {
    fn new(rx: mpsc::Receiver<Vec<i16>>, rate: u32, channels: u16) -> Self {
        Self {
            rx,
            buf: Vec::new(),
            pos: 0,
            rate: SampleRate::new(rate).unwrap_or(SampleRate::new(RATE).unwrap()),
            channels: ChannelCount::new(channels).unwrap_or(ChannelCount::MIN),
        }
    }
}

impl Iterator for LiveSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.pos >= self.buf.len() {
            self.buf = match self.rx.recv_timeout(LIVE_WAIT) {
                Ok(samples) if !samples.is_empty() => samples,
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {
                    vec![0; SILENCE_FRAMES * self.channels.get() as usize]
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            };
            self.pos = 0;
        }
        let sample = self.buf[self.pos];
        self.pos += 1;
        Some(sample as f32 / 32768.0)
    }
}

impl Source for LiveSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_a_recording_after_the_local_time() {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = 126;
        tm.tm_mon = 9;
        tm.tm_mday = 4;
        tm.tm_hour = 9;
        tm.tm_min = 5;
        assert_eq!(base_name(&tm), "capture-2026-10-04-09-05");
    }

    #[test]
    fn new_recording_path_does_not_reuse_an_existing_name() {
        let dir = tempfile::tempdir().unwrap();
        let first = new_recording_path(dir.path(), 0);
        assert!(first.to_string_lossy().ends_with(".wav"));
        std::fs::write(&first, b"").unwrap();

        let second = new_recording_path(dir.path(), 0);
        assert_ne!(first, second);
        assert!(second.to_string_lossy().ends_with("-2.wav"));
    }

    #[test]
    fn low_on_space_only_below_the_limit() {
        assert!(low_on_space(Some(MIN_FREE_BYTES - 1)));
        assert!(!low_on_space(Some(MIN_FREE_BYTES)));
        // Unknown free space never stops a recording.
        assert!(!low_on_space(None));
    }

    #[test]
    fn disk_space_reads_a_real_folder() {
        let dir = tempfile::tempdir().unwrap();
        let (free, total) = disk_space(dir.path()).unwrap();
        assert!(total >= free);
    }

    #[test]
    fn live_source_plays_samples_then_silence_then_ends() {
        let (tx, rx) = mpsc::sync_channel(4);
        let mut source = LiveSource::new(rx, 48_000, 1);
        tx.send(vec![16384, -16384]).unwrap();
        assert_eq!(source.next(), Some(0.5));
        assert_eq!(source.next(), Some(-0.5));
        // Nothing queued: a run of silence instead of waiting.
        assert_eq!(source.next(), Some(0.0));
        drop(tx);
        let rest: Vec<f32> = source.by_ref().collect();
        assert_eq!(rest.len(), SILENCE_FRAMES - 1);
    }
}
