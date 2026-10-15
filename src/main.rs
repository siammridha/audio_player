mod capture;
mod events;
mod http;
mod library;
mod log;
mod output_device;
mod player;
mod sleep_timer;
mod state_store;

use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use player::alsa_backend::AlsaPlayer;
use player::mock_backend::MockPlayer;
use player::{Mode, OutputChoice, Player};
use tiny_http::Server;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const MANIFEST: &str = include_str!("../assets/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("../assets/sw.js");
const ICON_192: &[u8] = include_bytes!("../assets/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");
const WORKER_THREADS: usize = 4;
/// How often the status is checked for changes to send. Capture is faster so
/// the level meter moves smoothly.
const STATUS_TICK_PLAY: Duration = Duration::from_secs(1);
const STATUS_TICK_CAPTURE: Duration = Duration::from_millis(250);
const FILES_TICK: Duration = Duration::from_secs(1);
const EVENTS_PING: Duration = Duration::from_secs(15);

fn main() -> anyhow::Result<()> {
    let log_level = log::level_name();
    log::log_startup_banner(env!("CARGO_PKG_VERSION"));
    log::info(&format!("audio-player starting (log level: {log_level})"));

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let music_dir: PathBuf = env::var("MUSIC_DIR")
        .unwrap_or_else(|_| "/var/lib/audio-player/audio".to_string())
        .into();
    std::fs::create_dir_all(&music_dir)?;

    let state_file: PathBuf = env::var("STATE_FILE")
        .unwrap_or_else(|_| "/var/lib/audio-player/state.json".to_string())
        .into();
    // A persistence failure must never stop the player from starting, so
    // this is logged rather than propagated with `?`.
    if let Some(parent) = state_file.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        log::error(&format!(
            "audio-player: could not create {parent:?} for STATE_FILE, persistence disabled: {e}"
        ));
    }

    let output_device_file = state_file.with_file_name("output_device.json");
    let output_choice = output_device::load(&output_device_file);

    let use_mock = env::var("AUDIO_PLAYER_MOCK").is_ok_and(|v| v == "1");
    let player: Arc<dyn Player> = if use_mock {
        let mock = MockPlayer::new();
        mock.set_output(output_choice);
        Arc::new(mock)
    } else {
        let device = env::var("AUDIO_DEVICE")
            .unwrap_or_else(|_| player::alsa_backend::DEFAULT_DEVICE.to_string());
        let fallback_device = env::var("AUDIO_DEVICE_FALLBACK")
            .unwrap_or_else(|_| player::alsa_backend::DEFAULT_FALLBACK_DEVICE.to_string());
        AlsaPlayer::new(&device, &fallback_device, output_choice)?
    };

    let capture_device_file = state_file.with_file_name("capture_device.json");
    player.set_capture_device(match output_device::load(&capture_device_file) {
        OutputChoice::Card(id) => Some(id),
        OutputChoice::Auto => None,
    });

    if let Some(snapshot) = state_store::load(&state_file) {
        match library::resolve(&music_dir, &snapshot.file) {
            Some(path) => player.restore(&path, &snapshot),
            None => log::info(&format!(
                "audio-player: saved track {:?} no longer found, skipping restore",
                snapshot.file
            )),
        }
    }

    let sleep_timer_file = state_file.with_file_name("sleep_timer.json");
    let initial_sleep_timer = sleep_timer::load(&sleep_timer_file, sleep_timer::now_ms());

    let state = Arc::new(http::AppState {
        player,
        music_dir,
        state_file,
        sleep_timer_file,
        sleep_timer: Mutex::new(initial_sleep_timer),
        output_device_file,
        capture_device_file,
        events: events::Events::default(),
        index_html: INDEX_HTML,
        manifest: MANIFEST,
        service_worker: SERVICE_WORKER,
        icon_192: ICON_192,
        icon_512: ICON_512,
    });

    let server = Server::http(("0.0.0.0", port))
        .map_err(|e| anyhow::anyhow!("failed to bind port {port}: {e}"))?;
    let server = Arc::new(server);
    log::info(&format!(
        "audio-player: server started, listening on http://0.0.0.0:{port}"
    ));

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let status = state.player.status();
                if status.playing {
                    state_store::persist(&state.state_file, &status);
                }
            }
        });
    }

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                // Nothing to pause in Capture mode, and fading would quiet
                // the live sound.
                if state.player.status().mode == Mode::Capture {
                    continue;
                }
                let pending = state.sleep_timer.lock().unwrap().take();
                let Some(pending) = pending else { continue };
                let was_pending_at = pending.at;
                let updated =
                    sleep_timer::tick(sleep_timer::now_ms(), pending, state.player.as_ref());
                let fired = updated.at != was_pending_at;
                *state.sleep_timer.lock().unwrap() = Some(updated);
                if fired {
                    sleep_timer::persist(&state.sleep_timer_file, Some(&updated));
                    state_store::persist(&state.state_file, &state.player.status());
                }
            }
        });
    }

    {
        let state = Arc::clone(&state);
        player::device_watch::subscribe(move || http::publish_devices(&state));
    }
    http::publish_devices(&state);

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let mut files_checked: Option<Instant> = None;
            loop {
                http::publish_status(&state);
                // Nothing announces a file added outside the page (e.g.
                // copied in over SSH), so the list is compared on a timer
                // and only sent when it changes.
                if files_checked.is_none_or(|t| t.elapsed() >= FILES_TICK) {
                    http::publish_files(&state);
                    files_checked = Some(Instant::now());
                }
                std::thread::sleep(if state.player.status().mode == Mode::Capture {
                    STATUS_TICK_CAPTURE
                } else {
                    STATUS_TICK_PLAY
                });
            }
        });
    }

    let handles: Vec<_> = (0..WORKER_THREADS)
        .map(|_| {
            let server = Arc::clone(&server);
            let state = Arc::clone(&state);
            std::thread::spawn(move || worker(server, state))
        })
        .collect();

    for handle in handles {
        let _ = handle.join();
    }
    Ok(())
}

fn worker(server: Arc<Server>, state: Arc<http::AppState>) {
    for mut request in server.incoming_requests() {
        let method = match request.method() {
            tiny_http::Method::Get => "GET",
            tiny_http::Method::Post => "POST",
            _ => "OTHER",
        };
        let url = request.url().to_string();

        if method == "GET" && url.split('?').next() == Some("/api/events") {
            let events = state.events.subscribe();
            // A page holds this open for as long as it's showing, so it
            // gets its own thread rather than tying up a worker.
            std::thread::spawn(move || stream_events(request.into_writer(), events));
            continue;
        }

        let mut body = Vec::new();
        let _ = request.as_reader().read_to_end(&mut body);

        let response = http::handle(&state, method, &url, &body);

        let content_type =
            tiny_http::Header::from_bytes(&b"Content-Type"[..], response.content_type.as_bytes())
                .expect("static content-type header is always valid");
        let http_response = tiny_http::Response::from_data(response.body)
            .with_status_code(response.status)
            .with_header(content_type);
        let _ = request.respond(http_response);
    }
}

fn stream_events(mut out: Box<dyn Write + Send>, events: std::sync::mpsc::Receiver<String>) {
    // `retry` makes the browser reconnect after 1 s instead of its default
    // 3 s, so a page catches up quickly after a server restart.
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nretry: 1000\n\n";
    if out.write_all(head.as_bytes()).and_then(|()| out.flush()).is_err() {
        return;
    }
    loop {
        // The ping is what notices a page that went away while nothing
        // changed.
        let message = match events.recv_timeout(EVENTS_PING) {
            Ok(message) => message,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ": ping\n\n".to_string(),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if out.write_all(message.as_bytes()).and_then(|()| out.flush()).is_err() {
            return;
        }
    }
}
