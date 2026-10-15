use std::path::Path;

pub mod alsa_backend;
pub mod device_watch;
pub mod mock_backend;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PlayerStatus {
    pub file: Option<String>,
    pub playing: bool,
    #[serde(rename = "loop")]
    pub looping: bool,
    /// Seconds into the current track. 0 if nothing is loaded.
    pub position: f64,
    /// Total length of the current track in seconds, if known.
    pub duration: Option<f64>,
    /// Playback volume, from 0.0 (silent) to 1.0 (full).
    pub volume: f32,
    /// Which sound output is currently in use: the ALSA card id being
    /// played through, "built-in" for the fallback device, or "mock".
    pub output: String,
    /// Which output the user asked for.
    #[serde(rename = "outputChoice")]
    pub output_choice: OutputChoice,
    pub mode: Mode,
    pub capture: CaptureStatus,
}

/// Which card the web page shows: playing files, or capturing an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Play,
    Capture,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct CaptureStatus {
    /// The capture card in use (in Play mode, the one Capture mode would
    /// use), or `None` while no card that can record is present.
    pub device: Option<String>,
    #[serde(rename = "deviceName")]
    pub device_name: Option<String>,
    /// The capture card the user picked, if any.
    #[serde(rename = "deviceChoice")]
    pub device_choice: Option<String>,
    pub recording: bool,
    /// Name of the file being recorded, inside the music folder.
    pub file: Option<String>,
    /// Seconds recorded so far.
    pub elapsed: f64,
    /// Size of the file being recorded.
    pub bytes: u64,
    /// Peak input level of the latest samples, 0.0..=1.0.
    pub level: f32,
}

/// The user's pick of where sound should go.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(from = "String", into = "String")]
pub enum OutputChoice {
    /// The USB card when it's plugged in, otherwise the built-in speaker.
    Auto,
    /// A specific ALSA card, by its short id (e.g. "Device"). The built-in
    /// speaker is still used while that card isn't plugged in.
    Card(String),
}

impl From<String> for OutputChoice {
    fn from(s: String) -> Self {
        if s == "auto" {
            Self::Auto
        } else {
            Self::Card(s)
        }
    }
}

impl From<OutputChoice> for String {
    fn from(choice: OutputChoice) -> Self {
        match choice {
            OutputChoice::Auto => "auto".to_string(),
            OutputChoice::Card(id) => id,
        }
    }
}

/// A sound card that can be picked as the output.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OutputDevice {
    /// ALSA short id, as used in `CARD=` device strings (e.g. "Device").
    pub id: String,
    /// Longer descriptive name (e.g. "USB PnP Sound Device").
    pub name: String,
}

/// A snapshot of playback state saved to disk so it survives a service
/// restart. Mirrors the persistable subset of `PlayerStatus` (not
/// `duration`/`output`, which are re-derived on load).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedState {
    pub file: String,
    pub playing: bool,
    #[serde(rename = "loop")]
    pub looping: bool,
    pub position: f64,
    pub volume: f32,
}

/// A single audio output. All methods act on "the currently loaded track" and
/// are safe to call with no track loaded (they're just no-ops in that case,
/// except `select`).
pub trait Player: Send + Sync {
    /// Load `path` and start playing it immediately, replacing whatever was
    /// playing before.
    fn select(&self, path: &Path, display_name: &str);

    /// Flip between playing and paused. No-op if nothing is loaded.
    fn toggle_play_pause(&self);

    /// Seek the current track back to the start and play. No-op if nothing
    /// is loaded.
    fn restart(&self);

    fn set_loop(&self, looping: bool);

    /// Sets playback volume. `volume` is clamped to 0.0..=1.0.
    fn set_volume(&self, volume: f32);

    /// Restores previously-saved state. Called once at startup, before
    /// the server starts. `path` has already been resolved and confirmed
    /// to exist. Best-effort: must not panic on a stale/bad snapshot.
    fn restore(&self, path: &Path, snapshot: &PersistedState);

    /// Stops and forgets the current track, so nothing is loaded.
    fn unload(&self);

    /// Sound cards currently present that can be picked as the output.
    fn output_devices(&self) -> Vec<OutputDevice>;

    /// Switches where sound goes, keeping the current track playing.
    fn set_output(&self, choice: OutputChoice);

    /// Entering Capture pauses the track and starts listening to the
    /// capture card. Leaving it stops capturing (and any recording).
    fn set_mode(&self, mode: Mode);

    /// Sound cards currently present that can record.
    fn capture_devices(&self) -> Vec<OutputDevice>;

    /// Picks the capture card by its short id. `None`, or a card that isn't
    /// present, means the newest card that can record.
    fn set_capture_device(&self, id: Option<String>);

    /// Starts writing the captured sound to a new WAV file at `path`. Fails
    /// outside Capture mode or while no capture card is open.
    fn start_recording(&self, path: &Path) -> anyhow::Result<()>;

    /// Stops recording and finishes the file. Returns once the file is
    /// complete on disk.
    fn stop_recording(&self);

    fn status(&self) -> PlayerStatus;
}
