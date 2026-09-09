use std::fs;
use std::path::Path;
use std::sync::Mutex;

use crate::player::{PersistedState, PlayerStatus};

/// Guards the write-then-rename in `persist` so two concurrent callers
/// (an HTTP worker thread and the periodic save thread) can't interleave
/// writes to the same temp file.
static PERSIST_LOCK: Mutex<()> = Mutex::new(());

/// Reads and parses a previously-saved state file. Returns `None` if it's
/// missing (expected on first-ever run) or unreadable/corrupt - either way
/// this is never fatal to startup.
pub fn load(path: &Path) -> Option<PersistedState> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::log::info("audio-player: no saved state file, starting fresh");
            return None;
        }
        Err(e) => {
            crate::log::error(&format!(
                "audio-player: failed to read state file {path:?}: {e}"
            ));
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(state) => Some(state),
        Err(e) => {
            crate::log::error(&format!(
                "audio-player: saved state file {path:?} is corrupt, ignoring it: {e}"
            ));
            None
        }
    }
}

/// Saves `status` to `path`, if there's a track loaded worth saving.
/// Writes to a sibling temp file and renames it into place, so a crash
/// mid-write can't leave a half-written state file behind. Any failure is
/// logged and otherwise ignored.
pub fn persist(path: &Path, status: &PlayerStatus) {
    let Some(file) = status.file.clone() else {
        return;
    };
    let snapshot = PersistedState {
        file,
        playing: status.playing,
        looping: status.looping,
        position: status.position,
        volume: status.volume,
    };
    let bytes = match serde_json::to_vec(&snapshot) {
        Ok(bytes) => bytes,
        Err(e) => {
            crate::log::error(&format!("audio-player: failed to serialize state: {e}"));
            return;
        }
    };

    let _guard = PERSIST_LOCK.lock().unwrap();
    let tmp_path = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp_path, &bytes) {
        crate::log::error(&format!(
            "audio-player: failed to write state file {tmp_path:?}: {e}"
        ));
        return;
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        crate::log::error(&format!(
            "audio-player: failed to save state file {path:?}: {e}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let status = PlayerStatus {
            file: Some("song.mp3".to_string()),
            playing: true,
            looping: false,
            position: 12.5,
            duration: Some(180.0),
            volume: 0.75,
            output: "mock",
        };

        persist(&path, &status);
        let loaded = load(&path).unwrap();

        assert_eq!(
            loaded,
            PersistedState {
                file: "song.mp3".to_string(),
                playing: true,
                looping: false,
                position: 12.5,
                volume: 0.75,
            }
        );
    }

    #[test]
    fn load_on_missing_path_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(load(&path), None);
    }

    #[test]
    fn load_on_corrupt_bytes_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, b"not json").unwrap();
        assert_eq!(load(&path), None);
    }

    #[test]
    fn persist_with_no_file_loaded_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let status = PlayerStatus {
            file: None,
            playing: false,
            looping: true,
            position: 0.0,
            duration: None,
            volume: 1.0,
            output: "mock",
        };

        persist(&path, &status);

        assert!(!path.exists());
    }
}
