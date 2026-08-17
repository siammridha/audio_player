use std::fs;
use std::path::{Path, PathBuf};

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg"];

/// Makes sure the music folder exists, then lists the audio files in it
/// (top-level only), sorted by name.
pub fn list_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| has_audio_extension(name))
        .collect();

    names.sort();
    names
}

pub fn resolve(dir: &Path, name: &str) -> Option<PathBuf> {
    // Reject anything that isn't a bare file name, so a request can't escape
    // the music folder (e.g. "../../etc/passwd").
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == ".." {
        return None;
    }
    if !has_audio_extension(name) {
        return None;
    }
    let path = dir.join(name);
    path.is_file().then_some(path)
}

fn has_audio_extension(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}
