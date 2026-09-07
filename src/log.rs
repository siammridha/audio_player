//! Timestamped logging. The OpenRC service sends both stdout and stderr to
//! the same file (`/var/log/audio-player.log`), so a UTC timestamp on every
//! line is what makes it possible to line log events up against `dmesg`/
//! hardware timing when diagnosing why playback isn't working.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "????-??-??T??:??:??Z".to_string())
}

pub fn print_line(msg: &str) {
    println!("{} {msg}", timestamp());
}

pub fn eprint_line(msg: &str) {
    eprintln!("{} {msg}", timestamp());
}
