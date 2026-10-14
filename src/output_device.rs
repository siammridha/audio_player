use std::fs;
use std::path::Path;
use std::sync::Mutex;

use crate::player::OutputChoice;

/// Guards the write-then-rename in `persist` so two concurrent HTTP worker
/// threads can't interleave writes to the same temp file.
static PERSIST_LOCK: Mutex<()> = Mutex::new(());

#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    device: OutputChoice,
}

/// Saves the user's output choice. Mirrors `state_store::persist`'s
/// write-tmp-then-rename pattern. Any failure is logged and otherwise
/// ignored.
pub fn persist(path: &Path, choice: &OutputChoice) {
    let bytes = match serde_json::to_vec(&Saved {
        device: choice.clone(),
    }) {
        Ok(bytes) => bytes,
        Err(e) => {
            crate::log::error(&format!(
                "audio-player: failed to serialize output device: {e}"
            ));
            return;
        }
    };
    let _guard = PERSIST_LOCK.lock().unwrap();
    let tmp_path = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp_path, &bytes) {
        crate::log::error(&format!(
            "audio-player: failed to write output device file {tmp_path:?}: {e}"
        ));
        return;
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        crate::log::error(&format!(
            "audio-player: failed to save output device file {path:?}: {e}"
        ));
    }
}

/// Loads the saved output choice, or Automatic if there isn't one or it's
/// corrupt. The saved card doesn't have to be plugged in.
pub fn load(path: &Path) -> OutputChoice {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OutputChoice::Auto,
        Err(e) => {
            crate::log::error(&format!(
                "audio-player: failed to read output device file {path:?}: {e}"
            ));
            return OutputChoice::Auto;
        }
    };
    match serde_json::from_slice::<Saved>(&bytes) {
        Ok(saved) => saved.device,
        Err(e) => {
            crate::log::error(&format!(
                "audio-player: output device file {path:?} is corrupt, ignoring it: {e}"
            ));
            OutputChoice::Auto
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_card_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output_device.json");

        persist(&path, &OutputChoice::Card("Device".to_string()));

        assert_eq!(load(&path), OutputChoice::Card("Device".to_string()));
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"device":"Device"}"#);
    }

    #[test]
    fn round_trips_auto_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output_device.json");

        persist(&path, &OutputChoice::Auto);

        assert_eq!(load(&path), OutputChoice::Auto);
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"device":"auto"}"#);
    }

    #[test]
    fn load_on_missing_path_is_auto() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("nope.json")), OutputChoice::Auto);
    }

    #[test]
    fn load_on_corrupt_bytes_is_auto() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output_device.json");
        fs::write(&path, b"not json").unwrap();
        assert_eq!(load(&path), OutputChoice::Auto);
    }
}
