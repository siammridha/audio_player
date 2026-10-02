use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::player::Player;

/// How long the volume fades down for before the timer pauses playback.
pub const FADE_DURATION_MS: u64 = 2000;

/// The timer repeats daily at the same clock time until the user cancels it.
const ONE_DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Pushes `at` forward a day at a time until it's back in the future.
fn advance_to_future(mut at: u64, now: u64) -> u64 {
    while at <= now {
        at += ONE_DAY_MS;
    }
    at
}

/// Guards the write-then-rename in `persist` so two concurrent callers (an
/// HTTP worker thread and the tick thread) can't interleave writes to the
/// same temp file.
static PERSIST_LOCK: Mutex<()> = Mutex::new(());

/// A pending sleep timer: pause playback at `at` (Unix ms). `at` is computed
/// by the browser, which already knows the device's local time and
/// timezone, so the backend only ever compares plain timestamps.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TimerState {
    pub at: u64,
    /// Volume captured the moment the fade starts, so it can be restored
    /// after pausing and so the fade always ramps down from the same
    /// starting point even if `tick` is called many times during it. Not
    /// persisted - if the server restarts mid-fade, the fade just restarts
    /// from whatever the volume happens to be at that point.
    #[serde(skip)]
    fade_base_volume: Option<f32>,
}

impl TimerState {
    pub fn new(at: u64) -> Self {
        Self {
            at,
            fade_base_volume: None,
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Advances a pending timer by one tick. The timer repeats every day at the
/// same clock time - it only goes away when the user cancels it - so this
/// always returns the timer's next state, never `None`. Once it fires,
/// playback has already been paused (if it was playing), the volume
/// restored, and `at` pushed forward to the same time tomorrow.
pub fn tick(now: u64, state: TimerState, player: &dyn Player) -> TimerState {
    if now >= state.at {
        if player.status().playing {
            player.toggle_play_pause();
        }
        if let Some(base) = state.fade_base_volume {
            player.set_volume(base);
        }
        return TimerState {
            at: advance_to_future(state.at, now),
            fade_base_volume: None,
        };
    }

    let remaining = state.at - now;
    if remaining <= FADE_DURATION_MS {
        let base = state
            .fade_base_volume
            .unwrap_or_else(|| player.status().volume);
        let fraction = remaining as f32 / FADE_DURATION_MS as f32;
        player.set_volume(base * fraction);
        return TimerState {
            at: state.at,
            fade_base_volume: Some(base),
        };
    }

    state
}

/// Saves (or, if `state` is `None`, deletes) the sleep timer file. Mirrors
/// `state_store::persist`'s write-tmp-then-rename pattern. Any failure is
/// logged and otherwise ignored.
pub fn persist(path: &Path, state: Option<&TimerState>) {
    let _guard = PERSIST_LOCK.lock().unwrap();
    match state {
        Some(state) => {
            let bytes = match serde_json::to_vec(state) {
                Ok(bytes) => bytes,
                Err(e) => {
                    crate::log::error(&format!(
                        "audio-player: failed to serialize sleep timer: {e}"
                    ));
                    return;
                }
            };
            let tmp_path = path.with_extension("json.tmp");
            if let Err(e) = fs::write(&tmp_path, &bytes) {
                crate::log::error(&format!(
                    "audio-player: failed to write sleep timer file {tmp_path:?}: {e}"
                ));
                return;
            }
            if let Err(e) = fs::rename(&tmp_path, path) {
                crate::log::error(&format!(
                    "audio-player: failed to save sleep timer file {path:?}: {e}"
                ));
            }
        }
        None => {
            if let Err(e) = fs::remove_file(path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                crate::log::error(&format!(
                    "audio-player: failed to remove sleep timer file {path:?}: {e}"
                ));
            }
        }
    }
}

/// Loads a previously-saved sleep timer. Returns `None` if there isn't one or
/// it's corrupt. Since the timer repeats daily until cancelled, one whose
/// target time has already passed (e.g. the server was down over it) is kept
/// but rolled forward to the next occurrence, rather than firing late or
/// being dropped.
pub fn load(path: &Path, now: u64) -> Option<TimerState> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            crate::log::error(&format!(
                "audio-player: failed to read sleep timer file {path:?}: {e}"
            ));
            return None;
        }
    };
    let state: TimerState = match serde_json::from_slice(&bytes) {
        Ok(state) => state,
        Err(e) => {
            crate::log::error(&format!(
                "audio-player: sleep timer file {path:?} is corrupt, ignoring it: {e}"
            ));
            return None;
        }
    };
    if state.at <= now {
        let advanced = TimerState::new(advance_to_future(state.at, now));
        persist(path, Some(&advanced));
        return Some(advanced);
    }
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::mock_backend::MockPlayer;
    use std::path::Path;

    fn playing_player(volume: f32) -> MockPlayer {
        let player = MockPlayer::new();
        player.select(Path::new("song.mp3"), "song.mp3");
        player.set_volume(volume);
        player
    }

    #[test]
    fn before_fade_window_does_nothing() {
        let player = playing_player(0.8);
        let state = TimerState::new(10_000);
        let result = tick(0, state, &player);
        assert_eq!(result, state);
        assert_eq!(player.status().volume, 0.8);
        assert!(player.status().playing);
    }

    #[test]
    fn fade_window_ramps_volume_down_proportionally() {
        let player = playing_player(0.8);
        let state = TimerState::new(10_000);

        let state = tick(9_000, state, &player);
        assert!((player.status().volume - 0.4).abs() < 1e-6);

        tick(9_500, state, &player);
        assert!((player.status().volume - 0.2).abs() < 1e-6);
        assert!(player.status().playing);
    }

    #[test]
    fn firing_pauses_restores_volume_and_reschedules_for_tomorrow() {
        let player = playing_player(0.8);
        let state = TimerState::new(10_000);

        let state = tick(9_000, state, &player); // fade starts, base = 0.8
        let result = tick(10_000, state, &player);

        assert_eq!(result.at, 10_000 + ONE_DAY_MS);
        assert!(!player.status().playing);
        assert_eq!(player.status().volume, 0.8);
    }

    #[test]
    fn firing_while_already_paused_does_not_resume() {
        let player = playing_player(0.8);
        player.toggle_play_pause(); // pause
        let state = TimerState::new(10_000);

        let result = tick(10_000, state, &player);

        assert_eq!(result.at, 10_000 + ONE_DAY_MS);
        assert!(!player.status().playing);
    }

    #[test]
    fn persist_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sleep_timer.json");
        let state = TimerState::new(10_000);

        persist(&path, Some(&state));
        let loaded = load(&path, 0).unwrap();

        assert_eq!(loaded.at, 10_000);
    }

    #[test]
    fn persist_with_none_deletes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sleep_timer.json");
        persist(&path, Some(&TimerState::new(10_000)));

        persist(&path, None);

        assert!(!path.exists());
    }

    #[test]
    fn load_on_missing_path_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(load(&path, 0), None);
    }

    #[test]
    fn load_on_corrupt_bytes_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sleep_timer.json");
        fs::write(&path, b"not json").unwrap();
        assert_eq!(load(&path, 0), None);
    }

    #[test]
    fn load_advances_a_past_due_timer_to_the_next_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sleep_timer.json");
        persist(&path, Some(&TimerState::new(10_000)));

        let loaded = load(&path, 20_000).unwrap();

        assert_eq!(loaded.at, 10_000 + ONE_DAY_MS);
        let reloaded = load(&path, 20_000).unwrap();
        assert_eq!(reloaded.at, loaded.at);
    }
}
