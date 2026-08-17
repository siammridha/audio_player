use std::fs;
use std::path::{Path, PathBuf};

use rodio::{Decoder, Source};

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg"];

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FileEntry {
    pub name: String,
    /// Length of the track in seconds, if it could be read from the file.
    pub duration: Option<f64>,
}

/// Makes sure the music folder exists, then lists the audio files in it
/// (top-level only), sorted by name, along with each one's length.
pub fn list_files(dir: &Path) -> Vec<FileEntry> {
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
        .into_iter()
        .map(|name| {
            let duration = probe_duration(&dir.join(&name));
            FileEntry { name, duration }
        })
        .collect()
}

/// Reads just enough of a file to find its length, without playing it.
pub fn probe_duration(path: &Path) -> Option<f64> {
    let file = std::fs::File::open(path).ok()?;
    let source = Decoder::try_from(file).ok()?;
    source.total_duration().map(|d| d.as_secs_f64())
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
