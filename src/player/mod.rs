use std::path::Path;

pub mod mock_backend;
pub mod rodio_backend;

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

    fn status(&self) -> PlayerStatus;
}
