use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use super::{PersistedState, Player, PlayerStatus};
use crate::library;

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
            }),
        }
    }
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
            output: "mock",
        }
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
