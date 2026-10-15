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
//! - The card lists (`cards`, `capture_cards`) hold a new card back for the
//!   same settle delay, so the page, the output picker and Capture mode all
//!   see it at the same time as the output switches to it.
//! - No polling fallback: presence is learned only from netlink events (plus
//!   the one check done at startup, so a caller need not wait for an event
//!   to learn the current state).

use std::os::unix::io::RawFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alsa::ctl::Ctl;

use super::OutputDevice;

const SETTLE_DELAY: Duration = Duration::from_secs(5);
const SOCKET_OPEN_RETRIES: u32 = 5;
const SOCKET_RETRY_DELAY: Duration = Duration::from_secs(1);
const RECV_BUF_LEN: usize = 8192;

/// When each card in a list was first seen, so it's only listed once it has
/// been present for `SETTLE_DELAY`. One per list, since each list only
/// knows about its own cards.
static SEEN_CARDS: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());
static SEEN_CAPTURE_CARDS: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());

/// Every ALSA card that has been present for the settle delay.
pub fn cards() -> Vec<OutputDevice> {
    settled(&SEEN_CARDS, present_cards(), Instant::now(), SETTLE_DELAY)
}

/// Every ALSA card that has been present for the settle delay and has a
/// capture stream on device 0, in card-number order (so a card plugged in
/// later comes last).
pub fn capture_cards() -> Vec<OutputDevice> {
    settled(&SEEN_CAPTURE_CARDS, present_capture_cards(), Instant::now(), SETTLE_DELAY)
}

/// Keeps `seen` up to date with `present`, and returns the cards in
/// `present` first seen at least `delay` before `now`.
fn settled(
    seen: &Mutex<Vec<(String, Instant)>>,
    present: Vec<OutputDevice>,
    now: Instant,
    delay: Duration,
) -> Vec<OutputDevice> {
    let mut seen = seen.lock().unwrap();
    seen.retain(|(id, _)| present.iter().any(|c| &c.id == id));
    for card in &present {
        if !seen.iter().any(|(id, _)| id == &card.id) {
            seen.push((card.id.clone(), now));
        }
    }
    present
        .into_iter()
        .filter(|c| {
            seen.iter()
                .any(|(id, since)| id == &c.id && now.duration_since(*since) >= delay)
        })
        .collect()
}

/// Every ALSA card currently present.
///
/// `id` is `snd_ctl_card_info_get_id` - the short id shown by `aplay -l` and
/// used in `CARD=` device strings (e.g. "Device"). `name` is the longer
/// descriptive string (e.g. "USB PnP Sound Device"), a different field that
/// never matches the short id.
fn present_cards() -> Vec<OutputDevice> {
    alsa::card::Iter::new()
        .filter_map(|c| c.ok())
        .filter_map(|c| Ctl::from_card(&c, false).ok())
        .filter_map(|ctl| ctl.card_info().ok())
        .filter_map(|info| {
            Some(OutputDevice {
                id: info.get_id().ok()?.to_string(),
                name: info.get_name().ok()?.to_string(),
            })
        })
        .collect()
}

/// Every ALSA card currently present that has a capture stream on device 0.
fn present_capture_cards() -> Vec<OutputDevice> {
    alsa::card::Iter::new()
        .filter_map(|c| c.ok())
        .filter_map(|c| Ctl::from_card(&c, false).ok())
        .filter(|ctl| ctl.pcm_info(0, 0, alsa::Direction::Capture).is_ok())
        .filter_map(|ctl| ctl.card_info().ok())
        .filter_map(|info| {
            Some(OutputDevice {
                id: info.get_id().ok()?.to_string(),
                name: info.get_name().ok()?.to_string(),
            })
        })
        .collect()
}

/// True if an ALSA card with this short id currently exists. Not held back
/// by the settle delay, since the watcher does its own wait.
pub(crate) fn card_present(id: &str) -> bool {
    present_cards().iter().any(|c| c.id == id)
}

/// Starts watching for hotplug changes in the background, for whichever
/// card `card_name` holds at the time (the caller may change it while
/// running). `on_change` is called with the confirmed presence state on
/// every real flip (after the settle delay for an appearance, immediately
/// for a disappearance). It never fires for the current state at startup -
/// call `card_present` yourself first if you need that.
pub fn watch(card_name: Arc<Mutex<String>>, on_change: impl Fn(bool) + Send + 'static) {
    let (recheck_tx, recheck_rx) = mpsc::channel();
    thread::spawn(move || uevent_listener(recheck_tx));
    thread::spawn(move || {
        let check_name = Arc::clone(&card_name);
        run_presence_loop(
            move || card_present(&check_name.lock().unwrap()),
            move |present| {
                let name = card_name.lock().unwrap().clone();
                if present {
                    crate::log::info(&format!("device_watch: {name} sound card detected"));
                } else {
                    crate::log::info(&format!("device_watch: {name} sound card disconnected"));
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

    fn card(id: &str) -> OutputDevice {
        OutputDevice {
            id: id.to_string(),
            name: id.to_string(),
        }
    }

    fn ids(cards: Vec<OutputDevice>) -> Vec<String> {
        cards.into_iter().map(|c| c.id).collect()
    }

    #[test]
    fn a_new_card_is_listed_only_after_the_settle_delay() {
        let seen = Mutex::new(Vec::new());
        let start = Instant::now();
        let delay = Duration::from_secs(5);
        assert!(settled(&seen, vec![card("A")], start, delay).is_empty());
        assert!(settled(&seen, vec![card("A")], start + Duration::from_secs(4), delay).is_empty());
        assert_eq!(ids(settled(&seen, vec![card("A")], start + delay, delay)), ["A"]);
    }

    #[test]
    fn a_removed_card_drops_out_at_once_and_waits_again_when_back() {
        let seen = Mutex::new(Vec::new());
        let start = Instant::now();
        let delay = Duration::from_secs(5);
        settled(&seen, vec![card("A"), card("B")], start, delay);
        let later = start + delay;
        assert_eq!(ids(settled(&seen, vec![card("A")], later, delay)), ["A"]);
        let back = later + Duration::from_secs(1);
        assert_eq!(ids(settled(&seen, vec![card("A"), card("B")], back, delay)), ["A"]);
    }

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
