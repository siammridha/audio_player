use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use super::{
    CaptureStatus, Mode, OutputChoice, OutputDevice, PersistedState, Player, PlayerStatus,
};
use crate::library;

/// The format the mock pretends to record in.
const MOCK_RATE: u32 = 48_000;
const MOCK_CHANNELS: u16 = 2;

struct State {
    file: Option<String>,
    looping: bool,
    volume: f32,
    duration: Option<f64>,
    /// Seconds accumulated from previous play segments (before the current
    /// one, if any).
    elapsed_base: f64,
    /// Set while "playing"; position is elapsed_base plus time since this.
    running_since: Option<Instant>,
    output_choice: OutputChoice,
    mode: Mode,
    capture_choice: Option<String>,
    /// The file being "recorded" and when it started.
    recording: Option<(PathBuf, Instant)>,
}

/// In-memory stand-in for `AlsaPlayer`, used wherever there's no sound card
/// to talk to: unit tests, and the browser e2e test. Position advances with
/// the wall clock instead of real playback.
pub struct MockPlayer {
    state: Mutex<State>,
}

impl MockPlayer {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                file: None,
                looping: true,
                volume: 1.0,
                duration: None,
                elapsed_base: 0.0,
                running_since: None,
                output_choice: OutputChoice::Auto,
                mode: Mode::Play,
                capture_choice: None,
                recording: None,
            }),
        }
    }
}

/// Stand-ins for the real device's BlueTrm USB input and built-in chip.
fn mock_capture_devices() -> Vec<OutputDevice> {
    vec![
        OutputDevice {
            id: "rt5672".to_string(),
            name: "sof-rt5672".to_string(),
        },
        OutputDevice {
            id: "Device_1".to_string(),
            name: "BlueTrm USB PnP Audio Device".to_string(),
        },
    ]
}

/// Writes a real silent WAV, so the recording shows up in the file list
/// with a length.
fn write_silent_wav(path: &Path, seconds: f64) {
    let spec = hound::WavSpec {
        channels: MOCK_CHANNELS,
        sample_rate: MOCK_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let Ok(mut writer) = hound::WavWriter::create(path, spec) else {
        return;
    };
    let samples = (seconds * MOCK_RATE as f64) as usize * MOCK_CHANNELS as usize;
    for _ in 0..samples {
        let _ = writer.write_sample(0i16);
    }
    let _ = writer.finalize();
}

impl Player for MockPlayer {
    fn select(&self, path: &Path, display_name: &str) {
        let duration = library::probe_duration(path);
        let mut state = self.state.lock().unwrap();
        state.file = Some(display_name.to_string());
        state.duration = duration;
        state.elapsed_base = 0.0;
        state.running_since = Some(Instant::now());
    }

    fn toggle_play_pause(&self) {
        let mut state = self.state.lock().unwrap();
        if state.file.is_none() {
            return;
        }
        if let Some(since) = state.running_since.take() {
            state.elapsed_base += since.elapsed().as_secs_f64();
        } else {
            state.running_since = Some(Instant::now());
        }
    }

    fn restart(&self) {
        let mut state = self.state.lock().unwrap();
        if state.file.is_some() {
            state.elapsed_base = 0.0;
            state.running_since = Some(Instant::now());
        }
    }

    fn set_loop(&self, looping: bool) {
        self.state.lock().unwrap().looping = looping;
    }

    fn set_volume(&self, volume: f32) {
        self.state.lock().unwrap().volume = volume.clamp(0.0, 1.0);
    }

    fn restore(&self, path: &Path, snapshot: &PersistedState) {
        let duration = library::probe_duration(path);
        let mut state = self.state.lock().unwrap();
        state.file = Some(snapshot.file.clone());
        state.duration = duration;
        state.looping = snapshot.looping;
        state.volume = snapshot.volume.clamp(0.0, 1.0);
        state.elapsed_base = snapshot.position.max(0.0);
        state.running_since = snapshot.playing.then(Instant::now);
    }

    /// Stand-ins for the real device's USB card and built-in sound chip.
    fn unload(&self) {
        let mut state = self.state.lock().unwrap();
        state.file = None;
        state.duration = None;
        state.elapsed_base = 0.0;
        state.running_since = None;
    }

    fn output_devices(&self) -> Vec<OutputDevice> {
        vec![
            OutputDevice {
                id: "Device".to_string(),
                name: "USB PnP Sound Device".to_string(),
            },
            OutputDevice {
                id: "rt5672".to_string(),
                name: "sof-rt5672".to_string(),
            },
        ]
    }

    fn set_output(&self, choice: OutputChoice) {
        self.state.lock().unwrap().output_choice = choice;
    }

    fn set_mode(&self, mode: Mode) {
        if mode == Mode::Play {
            self.stop_recording();
        }
        let mut state = self.state.lock().unwrap();
        if mode == Mode::Capture
            && let Some(since) = state.running_since.take()
        {
            state.elapsed_base += since.elapsed().as_secs_f64();
        }
        state.mode = mode;
    }

    fn capture_devices(&self) -> Vec<OutputDevice> {
        mock_capture_devices()
    }

    fn set_capture_device(&self, id: Option<String>) {
        self.state.lock().unwrap().capture_choice = id;
    }

    fn start_recording(&self, path: &Path) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.mode != Mode::Capture {
            anyhow::bail!("not in Capture mode");
        }
        if state.recording.is_some() {
            anyhow::bail!("already recording");
        }
        state.recording = Some((path.to_path_buf(), Instant::now()));
        Ok(())
    }

    fn stop_recording(&self) {
        let recording = self.state.lock().unwrap().recording.take();
        if let Some((path, since)) = recording {
            write_silent_wav(&path, since.elapsed().as_secs_f64());
        }
    }

    fn status(&self) -> PlayerStatus {
        let state = self.state.lock().unwrap();
        let position = state.elapsed_base
            + state
                .running_since
                .map(|t| t.elapsed().as_secs_f64())
                .unwrap_or(0.0);
        PlayerStatus {
            file: state.file.clone(),
            playing: state.running_since.is_some(),
            looping: state.looping,
            position,
            duration: state.duration,
            volume: state.volume,
            output: "mock".to_string(),
            output_choice: state.output_choice.clone(),
            mode: state.mode,
            capture: capture_status(&state),
        }
    }
}

fn capture_status(state: &State) -> CaptureStatus {
    let devices = mock_capture_devices();
    let card = state
        .capture_choice
        .as_ref()
        .and_then(|id| devices.iter().find(|d| &d.id == id))
        .or(devices.last())
        .cloned();
    let elapsed = state
        .recording
        .as_ref()
        .map(|(_, since)| since.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    let bytes = if state.recording.is_some() {
        44 + (elapsed * MOCK_RATE as f64) as u64 * MOCK_CHANNELS as u64 * 2
    } else {
        0
    };
    // Moves on its own, so the level meter can be seen working.
    let wobble = (crate::sleep_timer::now_ms() as f64 / 300.0).sin() as f32;
    CaptureStatus {
        device: card.as_ref().map(|c| c.id.clone()),
        device_name: card.map(|c| c.name),
        device_choice: state.capture_choice.clone(),
        recording: state.recording.is_some(),
        file: state
            .recording
            .as_ref()
            .and_then(|(path, _)| path.file_name().map(|n| n.to_string_lossy().into_owned())),
        elapsed,
        bytes,
        level: if state.mode == Mode::Capture {
            0.4 + 0.2 * wobble
        } else {
            0.0
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.mp3");
        fs::write(&path, b"fake audio").unwrap();
        (dir, path)
    }

    #[test]
    fn restore_while_playing() {
        let (_dir, path) = fixture();
        let player = MockPlayer::new();
        player.restore(
            &path,
            &PersistedState {
                file: "song.mp3".to_string(),
                playing: true,
                looping: false,
                position: 42.0,
                volume: 0.4,
            },
        );

        let status = player.status();
        assert_eq!(status.file.as_deref(), Some("song.mp3"));
        assert!(status.playing);
        assert!(!status.looping);
        assert_eq!(status.volume, 0.4);
        assert!(status.position >= 42.0);
    }

    #[test]
    fn restore_while_paused() {
        let (_dir, path) = fixture();
        let player = MockPlayer::new();
        player.restore(
            &path,
            &PersistedState {
                file: "song.mp3".to_string(),
                playing: false,
                looping: true,
                position: 17.5,
                volume: 0.9,
            },
        );

        let status = player.status();
        assert!(!status.playing);
        assert!(status.looping);
        assert_eq!(status.volume, 0.9);
        assert_eq!(status.position, 17.5);

        // Position stays frozen while paused - no wall-clock drift.
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(player.status().position, 17.5);
    }
}
