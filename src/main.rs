mod http;
mod library;
mod log;
mod player;
mod state_store;

use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use player::Player;
use player::alsa_backend::AlsaPlayer;
use player::mock_backend::MockPlayer;
use tiny_http::Server;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const MANIFEST: &str = include_str!("../assets/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("../assets/sw.js");
const ICON_192: &[u8] = include_bytes!("../assets/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");
const WORKER_THREADS: usize = 4;

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

    let use_mock = env::var("AUDIO_PLAYER_MOCK").is_ok_and(|v| v == "1");
    let player: Arc<dyn Player> = if use_mock {
        Arc::new(MockPlayer::new())
    } else {
        let device = env::var("AUDIO_DEVICE")
            .unwrap_or_else(|_| player::alsa_backend::DEFAULT_DEVICE.to_string());
        let fallback_device = env::var("AUDIO_DEVICE_FALLBACK")
            .unwrap_or_else(|_| player::alsa_backend::DEFAULT_FALLBACK_DEVICE.to_string());
        AlsaPlayer::new(
            &device,
            &fallback_device,
            player::alsa_backend::PRIMARY_CARD_NAME,
        )?
    };

    if let Some(snapshot) = state_store::load(&state_file) {
        match library::resolve(&music_dir, &snapshot.file) {
            Some(path) => player.restore(&path, &snapshot),
            None => log::info(&format!(
                "audio-player: saved track {:?} no longer found, skipping restore",
                snapshot.file
            )),
        }
    }

    let state = Arc::new(http::AppState {
        player,
        music_dir,
        state_file,
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
