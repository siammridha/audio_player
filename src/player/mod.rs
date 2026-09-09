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
    /// Which sound output is currently in use: "usb", "built-in", or
    /// "mock".
    pub output: &'static str,
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

    fn status(&self) -> PlayerStatus;
}
