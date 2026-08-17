mod http;
mod library;
mod player;

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use player::Player;
use player::mock_backend::MockPlayer;
use player::rodio_backend::RodioPlayer;
use tiny_http::Server;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const WORKER_THREADS: usize = 4;

fn main() -> anyhow::Result<()> {
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let music_dir: PathBuf = env::var("MUSIC_DIR")
        .unwrap_or_else(|_| "/var/lib/audio-player/audio".to_string())
        .into();
    std::fs::create_dir_all(&music_dir)?;

    let use_mock = env::var("AUDIO_PLAYER_MOCK").is_ok_and(|v| v == "1");
    let player: Arc<dyn Player> = if use_mock {
        Arc::new(MockPlayer::new())
    } else {
        RodioPlayer::new()?
    };

    let state = Arc::new(http::AppState {
        player,
        music_dir,
        index_html: INDEX_HTML,
    });

    let server = Server::http(("0.0.0.0", port))
        .map_err(|e| anyhow::anyhow!("failed to bind port {port}: {e}"))?;
    let server = Arc::new(server);
    println!("audio-player listening on http://0.0.0.0:{port}");

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
