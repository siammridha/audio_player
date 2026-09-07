//! Timestamped, leveled logging. The OpenRC service sends both stdout and
//! stderr to the same file (`/var/log/audio-player.log`), so a UTC
//! timestamp on every line is what makes it possible to line log events up
//! against `dmesg`/hardware timing when diagnosing why playback isn't
//! working.
//!
//! The level shown is controlled by the `RUST_LOG` env var (`error`,
//! `info`, or `debug`; case-insensitive), read once at first use and
//! defaulting to `info` if unset or unrecognized. Errors are always shown -
//! `RUST_LOG` only controls how much extra detail is shown above that
//! floor.

use std::sync::OnceLock;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Info,
    Debug,
}

impl Level {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }
}

fn configured_level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        std::env::var("RUST_LOG")
            .ok()
            .and_then(|v| Level::parse(&v))
            .unwrap_or(Level::Info)
    })
}

/// The effective level (after applying `RUST_LOG` and defaulting), as shown
/// in log lines - useful to announce once at startup.
pub fn level_name() -> &'static str {
    match configured_level() {
        Level::Error => "error",
        Level::Info => "info",
        Level::Debug => "debug",
    }
}

fn timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "????-??-??T??:??:??Z".to_string())
}

pub fn info(msg: &str) {
    if configured_level() >= Level::Info {
        println!("{} INFO {msg}", timestamp());
    }
}

pub fn debug(msg: &str) {
    if configured_level() >= Level::Debug {
        println!("{} DEBUG {msg}", timestamp());
    }
}

pub fn error(msg: &str) {
    eprintln!("{} ERROR {msg}", timestamp());
}

pub fn log_startup_banner(version: &str) {
    let line = format!("audio-player v{version}");
    let bar = "=".repeat(line.chars().count() + 4);
    println!("\x1b[1;32m{bar}\x1b[0m");
    println!("\x1b[1;32m  {line}\x1b[0m");
    println!("\x1b[1;32m{bar}\x1b[0m");
}

#[cfg(test)]
mod tests {
    use super::Level;

    #[test]
    fn parses_known_levels_case_insensitively() {
        assert_eq!(Level::parse("error"), Some(Level::Error));
        assert_eq!(Level::parse("INFO"), Some(Level::Info));
        assert_eq!(Level::parse("Debug"), Some(Level::Debug));
    }

    #[test]
    fn rejects_unknown_levels() {
        assert_eq!(Level::parse("verbose"), None);
        assert_eq!(Level::parse(""), None);
    }

    #[test]
    fn ordering_gates_info_and_debug_correctly() {
        // Error is the always-shown floor; Info and Debug add detail on
        // top of it in that order.
        assert!(Level::Error < Level::Info);
        assert!(Level::Info < Level::Debug);
    }
}
