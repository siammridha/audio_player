use std::path::Path;
use std::sync::Mutex;

use super::{Player, PlayerStatus};

/// In-memory stand-in for `RodioPlayer`, used wherever there's no sound card
/// to talk to: unit tests, and the browser e2e test.
pub struct MockPlayer {
    state: Mutex<PlayerStatus>,
}

impl MockPlayer {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PlayerStatus {
                file: None,
                playing: false,
                looping: false,
            }),
        }
    }
}

impl Player for MockPlayer {
    fn select(&self, _path: &Path, display_name: &str) {
        let mut state = self.state.lock().unwrap();
        state.file = Some(display_name.to_string());
        state.playing = true;
    }

    fn toggle_play_pause(&self) {
        let mut state = self.state.lock().unwrap();
        if state.file.is_some() {
            state.playing = !state.playing;
        }
    }

    fn restart(&self) {
        let mut state = self.state.lock().unwrap();
        if state.file.is_some() {
            state.playing = true;
        }
    }

    fn set_loop(&self, looping: bool) {
        self.state.lock().unwrap().looping = looping;
    }

    fn status(&self) -> PlayerStatus {
        self.state.lock().unwrap().clone()
    }
}
