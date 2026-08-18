use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use super::{Player, PlayerStatus};
use crate::library;

struct State {
    file: Option<String>,
    looping: bool,
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
                looping: false,
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
        }
    }
}
