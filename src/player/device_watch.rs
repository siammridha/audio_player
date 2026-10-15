//! Keeps the list of ALSA sound cards up to date, and says when each card
//! can be used. Works without udev: this listens straight to the kernel's
//! own netlink broadcast, since the device this runs on (Alpine, mdev) has
//! no udev daemon to rely on.
//!
//! - A kernel event is only a hint to re-read `/proc/asound/cards`. Reading
//!   `/proc` never opens a card, so keeping the lists never touches the
//!   hardware.
//! - A new card is listed at once, but only marked ready (safe to open)
//!   once it has stayed for its settle delay (`settle_delay`). Some USB
//!   cards misbehave when opened right after they appear.
//! - If a card goes away during that wait, it's not marked ready, and its
//!   next plug-in waits again from the start.
//! - A card that goes away is dropped at once.
//! - No polling fallback: the lists change only on kernel events (plus the
//!   one read in `start`).

use std::fs;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use super::OutputDevice;

const SETTLE_DELAY: Duration = Duration::from_secs(5);
/// Shorter than `SETTLE_DELAY` because the BlueTrm capture card resets
/// itself about 4.8 s after connecting unless something records from it.
const CAPTURE_SETTLE_DELAY: Duration = Duration::from_secs(3);
const SOCKET_OPEN_RETRIES: u32 = 5;
const SOCKET_RETRY_DELAY: Duration = Duration::from_secs(1);
const RECV_BUF_LEN: usize = 8192;
const CARDS_FILE: &str = "/proc/asound/cards";

static CARDS: Mutex<Cards> = Mutex::new(Cards::new());
static LISTENERS: Mutex<Vec<Box<dyn Fn() + Send>>> = Mutex::new(Vec::new());

struct Cards {
    /// In card-number order, so a card plugged in later comes last.
    list: Vec<Card>,
    next_detection: u64,
}

impl Cards {
    const fn new() -> Self {
        Self {
            list: Vec::new(),
            next_detection: 0,
        }
    }
}

struct Card {
    number: u32,
    device: OutputDevice,
    capture: bool,
    ready: bool,
    /// Different for every plug-in, so a wait only marks ready the plug-in
    /// it was started for.
    detection: u64,
}

/// A card as read from `/proc`.
struct Found {
    number: u32,
    device: OutputDevice,
    capture: bool,
}

/// Reads the cards that are already there, then keeps the lists up to date
/// in the background. Call once.
pub fn start() {
    rescan();
    thread::spawn(|| uevent_listener(rescan));
}

/// Calls `on_change` every time a card is added, becomes ready or goes away.
pub fn subscribe(on_change: impl Fn() + Send + 'static) {
    LISTENERS.lock().unwrap().push(Box::new(on_change));
}

/// Every card, ready or not.
pub fn cards() -> Vec<OutputDevice> {
    listed(|_| true)
}

/// Every card with a capture stream on device 0, ready or not.
pub fn capture_cards() -> Vec<OutputDevice> {
    listed(|c| c.capture)
}

/// The capture cards that are safe to open.
pub fn ready_capture_cards() -> Vec<OutputDevice> {
    listed(|c| c.capture && c.ready)
}

/// True if the card with this short id is there and safe to open.
pub(crate) fn card_ready(id: &str) -> bool {
    CARDS
        .lock()
        .unwrap()
        .list
        .iter()
        .any(|c| c.device.id == id && c.ready)
}

fn listed(keep: impl Fn(&Card) -> bool) -> Vec<OutputDevice> {
    CARDS
        .lock()
        .unwrap()
        .list
        .iter()
        .filter(|c| keep(c))
        .map(|c| c.device.clone())
        .collect()
}

fn notify() {
    for on_change in LISTENERS.lock().unwrap().iter() {
        on_change();
    }
}

fn rescan() {
    let (changed, new) = update(&mut CARDS.lock().unwrap(), read_cards());
    for (id, detection, delay) in new {
        thread::spawn(move || wait_then_mark_ready(id, detection, delay));
    }
    if changed {
        notify();
    }
}

/// How long a new card must stay before it's marked ready.
fn settle_delay(capture: bool) -> Duration {
    if capture {
        CAPTURE_SETTLE_DELAY
    } else {
        SETTLE_DELAY
    }
}

fn wait_then_mark_ready(id: String, detection: u64, delay: Duration) {
    crate::log::debug(&format!(
        "device_watch: waiting {delay:?} before using the {id} sound card"
    ));
    thread::sleep(delay);
    rescan();
    if mark_ready(&mut CARDS.lock().unwrap(), detection) {
        crate::log::info(&format!("device_watch: {id} sound card ready"));
        notify();
    } else {
        crate::log::debug(&format!(
            "device_watch: {id} sound card went away during the {delay:?} wait, not using it"
        ));
    }
}

/// Brings `cards` in line with `found`. Returns whether anything changed,
/// and the id, detection and settle delay of each new card, to wait on.
fn update(cards: &mut Cards, found: Vec<Found>) -> (bool, Vec<(String, u64, Duration)>) {
    let before = cards.list.len();
    cards.list.retain(|card| {
        let still_there = found.iter().any(|f| f.device.id == card.device.id);
        if !still_there {
            crate::log::info(&format!(
                "device_watch: {} sound card disconnected",
                card.device.id
            ));
        }
        still_there
    });
    let removed = cards.list.len() != before;

    let mut new = Vec::new();
    for f in found {
        if cards.list.iter().any(|c| c.device.id == f.device.id) {
            continue;
        }
        let detection = cards.next_detection;
        cards.next_detection += 1;
        crate::log::info(&format!(
            "device_watch: {} sound card detected",
            f.device.id
        ));
        new.push((f.device.id.clone(), detection, settle_delay(f.capture)));
        cards.list.push(Card {
            number: f.number,
            device: f.device,
            capture: f.capture,
            ready: false,
            detection,
        });
    }
    cards.list.sort_by_key(|c| c.number);
    (removed || !new.is_empty(), new)
}

/// Marks the card ready if this same plug-in is still there.
fn mark_ready(cards: &mut Cards, detection: u64) -> bool {
    match cards.list.iter_mut().find(|c| c.detection == detection) {
        Some(card) => {
            card.ready = true;
            true
        }
        None => false,
    }
}

fn read_cards() -> Vec<Found> {
    let text = match fs::read_to_string(CARDS_FILE) {
        Ok(text) => text,
        Err(e) => {
            crate::log::error(&format!("device_watch: failed to read {CARDS_FILE}: {e}"));
            String::new()
        }
    };
    parse_cards(&text)
        .into_iter()
        .map(|(number, device)| Found {
            number,
            device,
            capture: Path::new(&format!("/sys/class/sound/pcmC{number}D0c")).exists(),
        })
        .collect()
}

/// Parses `/proc/asound/cards`. Each card's first line looks like
/// ` 1 [Device         ]: USB-Audio - USB PnP Sound Device`: the number,
/// the short id (used in `CARD=` device strings), the driver, then the
/// name. The second line of each card has no number and is skipped.
fn parse_cards(text: &str) -> Vec<(u32, OutputDevice)> {
    text.lines()
        .filter_map(|line| {
            let (number, rest) = line.trim_start().split_once(" [")?;
            let number = number.parse().ok()?;
            let (id, rest) = rest.split_once("]: ")?;
            let (_driver, name) = rest.split_once(" - ")?;
            Some((
                number,
                OutputDevice {
                    id: id.trim().to_string(),
                    name: name.trim().to_string(),
                },
            ))
        })
        .collect()
}

/// Opens the kernel uevent netlink socket and calls `on_sound_event` for
/// every event about the "sound" subsystem. Gives up (and stops watching)
/// if the socket can't be opened.
fn uevent_listener(on_sound_event: impl Fn()) {
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
        if is_sound_event {
            on_sound_event();
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

    fn found(number: u32, id: &str) -> Found {
        Found {
            number,
            device: OutputDevice {
                id: id.to_string(),
                name: id.to_string(),
            },
            capture: true,
        }
    }

    fn ids(cards: &Cards) -> Vec<&str> {
        cards.list.iter().map(|c| c.device.id.as_str()).collect()
    }

    fn ready(cards: &Cards, id: &str) -> bool {
        cards.list.iter().any(|c| c.device.id == id && c.ready)
    }

    #[test]
    fn parses_the_proc_card_list() {
        let text = " 0 [rt5670         ]: cht-bsw-rt5670 - cht-bsw-rt5670\n\
                    \x20                     Dell Inc.-Wyse 3040\n\
                    \x201 [Device         ]: USB-Audio - USB PnP Sound Device\n\
                    \x20                     C-Media Electronics Inc. USB PnP Sound Device at usb-0000:00:14.0-1, full speed\n";
        assert_eq!(
            parse_cards(text),
            vec![
                (
                    0,
                    OutputDevice {
                        id: "rt5670".to_string(),
                        name: "cht-bsw-rt5670".to_string(),
                    }
                ),
                (
                    1,
                    OutputDevice {
                        id: "Device".to_string(),
                        name: "USB PnP Sound Device".to_string(),
                    }
                ),
            ]
        );
    }

    #[test]
    fn parses_no_cards() {
        assert!(parse_cards("").is_empty());
        assert!(parse_cards("--- no soundcards ---\n").is_empty());
    }

    #[test]
    fn a_new_card_is_listed_at_once_but_not_ready() {
        let mut cards = Cards::new();
        let (changed, new) = update(&mut cards, vec![found(1, "Device")]);
        assert!(changed);
        assert_eq!(new, vec![("Device".to_string(), 0, CAPTURE_SETTLE_DELAY)]);
        assert_eq!(ids(&cards), ["Device"]);
        assert!(!ready(&cards, "Device"));
    }

    #[test]
    fn a_card_that_cannot_record_waits_longer() {
        let mut cards = Cards::new();
        let playback_only = Found {
            capture: false,
            ..found(1, "Audio")
        };
        let (_, new) = update(&mut cards, vec![playback_only]);
        assert_eq!(new, vec![("Audio".to_string(), 0, SETTLE_DELAY)]);
    }

    #[test]
    fn the_same_cards_again_change_nothing() {
        let mut cards = Cards::new();
        update(&mut cards, vec![found(0, "A"), found(1, "B")]);
        let (changed, new) = update(&mut cards, vec![found(0, "A"), found(1, "B")]);
        assert!(!changed);
        assert!(new.is_empty());
    }

    #[test]
    fn a_removed_card_drops_out_at_once() {
        let mut cards = Cards::new();
        update(&mut cards, vec![found(0, "A"), found(1, "B")]);
        let (changed, new) = update(&mut cards, vec![found(0, "A")]);
        assert!(changed);
        assert!(new.is_empty());
        assert_eq!(ids(&cards), ["A"]);
    }

    #[test]
    fn cards_stay_in_number_order() {
        let mut cards = Cards::new();
        update(&mut cards, vec![found(2, "C")]);
        update(&mut cards, vec![found(0, "A"), found(2, "C")]);
        assert_eq!(ids(&cards), ["A", "C"]);
    }

    #[test]
    fn a_card_still_there_after_the_wait_is_ready() {
        let mut cards = Cards::new();
        let (_, new) = update(&mut cards, vec![found(1, "Device")]);
        assert!(mark_ready(&mut cards, new[0].1));
        assert!(ready(&cards, "Device"));
    }

    #[test]
    fn a_card_gone_during_the_wait_is_not_ready() {
        let mut cards = Cards::new();
        let (_, new) = update(&mut cards, vec![found(1, "Device")]);
        update(&mut cards, vec![]);
        assert!(!mark_ready(&mut cards, new[0].1));
        assert!(cards.list.is_empty());
    }

    #[test]
    fn a_card_plugged_back_in_waits_again() {
        let mut cards = Cards::new();
        let (_, first) = update(&mut cards, vec![found(1, "Device")]);
        update(&mut cards, vec![]);
        let (_, second) = update(&mut cards, vec![found(1, "Device")]);
        assert!(!mark_ready(&mut cards, first[0].1));
        assert!(!ready(&cards, "Device"));
        assert!(mark_ready(&mut cards, second[0].1));
        assert!(ready(&cards, "Device"));
    }
}
