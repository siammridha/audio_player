//! Watches for an ALSA sound card appearing/disappearing (hotplug), without
//! udev: this listens straight to the kernel's own netlink broadcast, since
//! the device this runs on (Alpine, mdev) has no udev daemon to rely on.
//!
//! Design, matched to real hardware behavior:
//! - An event is only ever a hint to go recheck reality (`card_present`),
//!   never trusted for its own content beyond which subsystem it's about.
//! - Only a flip in known presence is reported; repeated/duplicate events
//!   are ignored.
//! - A newly-appeared card gets a settle delay before anything acts on it
//!   (the card needs a moment after showing up before it's safe to open),
//!   re-checked after the wait - if it's gone again, that wait is discarded
//!   rather than confirming a card that's no longer there.
//! - A card disappearing is reported immediately, no delay.
//! - No polling fallback: presence is learned only from netlink events (plus
//!   the one check done at startup, so a caller need not wait for an event
//!   to learn the current state).

use std::os::unix::io::RawFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use alsa::ctl::Ctl;

const SETTLE_DELAY: Duration = Duration::from_secs(5);
const SOCKET_OPEN_RETRIES: u32 = 5;
const SOCKET_RETRY_DELAY: Duration = Duration::from_secs(1);
const RECV_BUF_LEN: usize = 8192;

/// True if an ALSA card with this short id (as shown by `aplay -l` and used
/// in `CARD=` device strings, e.g. "Device") currently exists.
///
/// This is `snd_ctl_card_info_get_id`, not `snd_card_get_name` - the name is
/// a longer descriptive string (e.g. "USB PnP Sound Device"), a different
/// field that never matches the short id.
pub(crate) fn card_present(name: &str) -> bool {
    alsa::card::Iter::new()
        .filter_map(|c| c.ok())
        .filter_map(|c| Ctl::from_card(&c, false).ok())
        .filter_map(|ctl| ctl.card_info().ok())
        .filter_map(|info| info.get_id().ok().map(|s| s.to_string()))
        .any(|n| n == name)
}

/// Starts watching `card_name` for hotplug changes in the background.
/// `on_change` is called with the confirmed presence state on every real
/// flip (after the settle delay for an appearance, immediately for a
/// disappearance). It never fires for the current state at startup - call
/// `card_present` yourself first if you need that.
pub fn watch(card_name: &'static str, on_change: impl Fn(bool) + Send + 'static) {
    let (recheck_tx, recheck_rx) = mpsc::channel();
    thread::spawn(move || uevent_listener(recheck_tx));
    thread::spawn(move || {
        run_presence_loop(
            move || card_present(card_name),
            move |present| {
                if present {
                    crate::log::info(&format!("device_watch: {card_name} sound card detected"));
                } else {
                    crate::log::info(&format!(
                        "device_watch: {card_name} sound card disconnected"
                    ));
                }
                on_change(present);
            },
            recheck_rx,
            SETTLE_DELAY,
        )
    });
}

/// The presence state machine, decoupled from real ALSA/netlink so it can be
/// unit-tested with a fake `check` and a short `settle`.
fn run_presence_loop(
    check: impl Fn() -> bool,
    on_change: impl Fn(bool),
    recheck: Receiver<()>,
    settle: Duration,
) {
    let mut known_present = false;
    if check() {
        known_present = settle_and_confirm(&check, settle, &on_change);
    }

    while recheck.recv().is_ok() {
        let now_present = check();
        if now_present == known_present {
            continue;
        }
        if now_present {
            known_present = settle_and_confirm(&check, settle, &on_change);
        } else {
            known_present = false;
            on_change(false);
        }
    }
}

/// Waits out the settle delay, then re-checks. Returns whether the card is
/// still there (and, if so, has already reported it via `on_change(true)`).
fn settle_and_confirm(
    check: &impl Fn() -> bool,
    settle: Duration,
    on_change: &impl Fn(bool),
) -> bool {
    crate::log::debug(&format!(
        "device_watch: sound card noticed, waiting {settle:?} before using it"
    ));
    thread::sleep(settle);
    if check() {
        on_change(true);
        true
    } else {
        crate::log::debug(
            "device_watch: sound card vanished again during the settle wait, ignoring",
        );
        false
    }
}

/// Opens the kernel uevent netlink socket and, for every event about the
/// "sound" subsystem, pings `recheck` so the presence loop goes and checks
/// reality. Gives up (and stops watching) if the socket can't be opened.
fn uevent_listener(recheck: Sender<()>) {
    let Some(fd) = open_socket_with_retries() else {
        crate::log::error(&format!(
            "device_watch: failed to open netlink socket after {SOCKET_OPEN_RETRIES} attempts, giving up on hotplug detection"
        ));
        return;
    };

    let mut buf = [0u8; RECV_BUF_LEN];
    loop {
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n <= 0 {
            break;
        }
        let msg = &buf[..n as usize];
        let is_sound_event = msg
            .split(|&b| b == 0)
            .any(|field| field == b"SUBSYSTEM=sound");
        if is_sound_event && recheck.send(()).is_err() {
            break;
        }
    }
    unsafe { libc::close(fd) };
}

fn open_socket_with_retries() -> Option<RawFd> {
    for attempt in 0..SOCKET_OPEN_RETRIES {
        if let Some(fd) = open_uevent_socket() {
            return Some(fd);
        }
        if attempt + 1 < SOCKET_OPEN_RETRIES {
            thread::sleep(SOCKET_RETRY_DELAY);
        }
    }
    None
}

/// Opens and binds a raw netlink socket to the kernel's kobject-uevent
/// broadcast group, the same broadcast `mdev`/`udev` themselves listen to.
fn open_uevent_socket() -> Option<RawFd> {
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_KOBJECT_UEVENT,
        );
        if fd < 0 {
            return None;
        }

        let mut addr: libc::sockaddr_nl = std::mem::zeroed();
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_pid = 0;
        addr.nl_groups = 1; // kernel kobject-uevent multicast group

        let ret = libc::bind(
            fd,
            (&addr as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        if ret < 0 {
            libc::close(fd);
            return None;
        }

        Some(fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A `check` fake driven by a scripted sequence of results, one per
    /// call; the last value repeats once the script runs out.
    fn scripted_check(script: Vec<bool>) -> impl Fn() -> bool {
        let script = Arc::new(Mutex::new(script));
        move || {
            let mut script = script.lock().unwrap();
            if script.len() > 1 {
                script.remove(0)
            } else {
                *script.first().unwrap()
            }
        }
    }

    fn recording_on_change() -> (impl Fn(bool), Arc<Mutex<Vec<bool>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&calls);
        (move |present| recorder.lock().unwrap().push(present), calls)
    }

    const NO_SETTLE: Duration = Duration::from_millis(1);

    #[test]
    fn absent_at_start_reports_nothing() {
        let (on_change, calls) = recording_on_change();
        let (tx, rx) = mpsc::channel();
        drop(tx);
        run_presence_loop(|| false, on_change, rx, NO_SETTLE);
        assert_eq!(*calls.lock().unwrap(), Vec::<bool>::new());
    }

    #[test]
    fn present_at_start_confirms_after_settling() {
        let (on_change, calls) = recording_on_change();
        let (tx, rx) = mpsc::channel();
        drop(tx);
        run_presence_loop(|| true, on_change, rx, NO_SETTLE);
        assert_eq!(*calls.lock().unwrap(), vec![true]);
    }

    #[test]
    fn appears_after_start_and_confirms() {
        let (on_change, calls) = recording_on_change();
        let (tx, rx) = mpsc::channel();
        let check = scripted_check(vec![false, true, true]);
        tx.send(()).unwrap();
        drop(tx);
        run_presence_loop(check, on_change, rx, NO_SETTLE);
        assert_eq!(*calls.lock().unwrap(), vec![true]);
    }

    #[test]
    fn disappears_immediately_with_no_settle_wait() {
        let (on_change, calls) = recording_on_change();
        let (tx, rx) = mpsc::channel();
        // Present at start (confirms), then an event finds it gone. The
        // disappear branch in run_presence_loop never calls
        // settle_and_confirm, so this doesn't wait out `settle` at all -
        // that's checked directly, not by timing.
        let check = scripted_check(vec![true, true, false]);
        tx.send(()).unwrap();
        drop(tx);
        run_presence_loop(check, on_change, rx, NO_SETTLE);
        assert_eq!(*calls.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn gone_during_settle_wait_is_not_confirmed() {
        let (on_change, calls) = recording_on_change();
        let (tx, rx) = mpsc::channel();
        // Appears, but by the time the settle wait ends it's gone again.
        let check = scripted_check(vec![false, true, false]);
        tx.send(()).unwrap();
        drop(tx);
        run_presence_loop(check, on_change, rx, NO_SETTLE);
        assert_eq!(*calls.lock().unwrap(), Vec::<bool>::new());
    }

    #[test]
    fn a_gone_and_confirmed_appearance_is_treated_as_fresh_next_time() {
        let (on_change, calls) = recording_on_change();
        let (tx, rx) = mpsc::channel();
        // Appears, vanishes during settle (not confirmed), then appears
        // again for real and should confirm normally.
        let check = scripted_check(vec![false, true, false, true, true]);
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        drop(tx);
        run_presence_loop(check, on_change, rx, NO_SETTLE);
        assert_eq!(*calls.lock().unwrap(), vec![true]);
    }

    #[test]
    fn duplicate_events_with_no_flip_are_ignored() {
        let (on_change, calls) = recording_on_change();
        let (tx, rx) = mpsc::channel();
        let check = scripted_check(vec![true, true, true, true]);
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        drop(tx);
        run_presence_loop(check, on_change, rx, NO_SETTLE);
        // Only the initial confirmation fires; the repeated "still present"
        // events after it don't flip anything, so no extra calls.
        assert_eq!(*calls.lock().unwrap(), vec![true]);
    }
}
