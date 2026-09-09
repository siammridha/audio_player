use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use crate::library;
use crate::player::Player;
use crate::state_store;

pub struct AppState {
    pub player: Arc<dyn Player>,
    pub music_dir: PathBuf,
    pub state_file: PathBuf,
    pub index_html: &'static str,
    pub manifest: &'static str,
    pub service_worker: &'static str,
    pub icon_192: &'static [u8],
    pub icon_512: &'static [u8],
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(&value).unwrap(),
        }
    }

    fn html(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }

    fn raw(content_type: &'static str, body: &[u8]) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.to_vec(),
        }
    }

    fn not_found() -> Self {
        Self::json(404, json!({ "error": "not found" }))
    }

    fn bad_request(msg: &str) -> Self {
        Self::json(400, json!({ "error": msg }))
    }
}

#[derive(Deserialize)]
struct PlayRequest {
    file: String,
}

#[derive(Deserialize)]
struct LoopRequest {
    #[serde(rename = "loop")]
    looping: bool,
}

#[derive(Deserialize)]
struct VolumeRequest {
    volume: f32,
}

/// Routes one request to a handler. Kept as a plain function of its inputs
/// (no tiny_http types) so it can be tested without a real socket.
pub fn handle(state: &AppState, method: &str, path: &str, body: &[u8]) -> Response {
    // Requests never carry a query string in this app, but strip one off
    // defensively so an accidental "?foo=bar" doesn't 404.
    let path = path.split('?').next().unwrap_or(path);

    match (method, path) {
        ("GET", "/") => Response::html(state.index_html),
        ("GET", "/manifest.webmanifest") => {
            Response::raw("application/manifest+json", state.manifest.as_bytes())
        }
        ("GET", "/sw.js") => {
            Response::raw("application/javascript", state.service_worker.as_bytes())
        }
        ("GET", "/icon-192.png") => Response::raw("image/png", state.icon_192),
        ("GET", "/icon-512.png") => Response::raw("image/png", state.icon_512),

        ("GET", "/api/files") => {
            let files = library::list_files(&state.music_dir);
            Response::json(200, serde_json::to_value(files).unwrap())
        }

        ("GET", "/api/status") => status_response(state),

        ("POST", "/api/play") => {
            let Ok(req) = serde_json::from_slice::<PlayRequest>(body) else {
                return Response::bad_request("expected { \"file\": \"...\" }");
            };
            match library::resolve(&state.music_dir, &req.file) {
                Some(path) => {
                    state.player.select(&path, &req.file);
                    save_status(state);
                    status_response(state)
                }
                None => Response::bad_request("unknown file"),
            }
        }

        ("POST", "/api/toggle") => {
            state.player.toggle_play_pause();
            save_status(state);
            status_response(state)
        }

        ("POST", "/api/restart") => {
            state.player.restart();
            save_status(state);
            status_response(state)
        }

        ("POST", "/api/loop") => {
            let Ok(req) = serde_json::from_slice::<LoopRequest>(body) else {
                return Response::bad_request("expected { \"loop\": true|false }");
            };
            state.player.set_loop(req.looping);
            save_status(state);
            status_response(state)
        }

        ("POST", "/api/volume") => {
            let Ok(req) = serde_json::from_slice::<VolumeRequest>(body) else {
                return Response::bad_request("expected { \"volume\": 0.0..=1.0 }");
            };
            state.player.set_volume(req.volume);
            save_status(state);
            status_response(state)
        }

        _ => Response::not_found(),
    }
}

fn status_response(state: &AppState) -> Response {
    Response::json(200, serde_json::to_value(state.player.status()).unwrap())
}

/// Called by the mutating endpoints after they change the player, not by the
/// plain `GET /api/status` (which the frontend polls every second and
/// shouldn't trigger a write each time).
fn save_status(state: &AppState) {
    state_store::persist(&state.state_file, &state.player.status());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::mock_backend::MockPlayer;
    use std::fs;
    use std::sync::Arc;

    fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("song.mp3"), b"fake audio").unwrap();
        fs::write(dir.path().join("notes.txt"), b"not audio").unwrap();
        let state = AppState {
            player: Arc::new(MockPlayer::new()),
            music_dir: dir.path().to_path_buf(),
            state_file: dir.path().join("state.json"),
            index_html: "<html></html>",
            manifest: "{}",
            service_worker: "",
            icon_192: b"fake png",
            icon_512: b"fake png",
        };
        (state, dir)
    }

    fn json_body(resp: &Response) -> serde_json::Value {
        serde_json::from_slice(&resp.body).unwrap()
    }

    #[test]
    fn serves_index_page() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "GET", "/", b"");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "text/html; charset=utf-8");
    }

    #[test]
    fn lists_only_audio_files() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "GET", "/api/files", b"");
        // The fixture file isn't real audio, so its duration can't be read.
        assert_eq!(
            json_body(&resp),
            json!([{ "name": "song.mp3", "duration": null }])
        );
    }

    #[test]
    fn status_includes_position_and_duration() {
        let (state, _dir) = test_state();
        handle(&state, "POST", "/api/play", br#"{"file":"song.mp3"}"#);
        let resp = handle(&state, "GET", "/api/status", b"");
        let body = json_body(&resp);
        assert!(body["position"].is_number());
        assert!(body["duration"].is_null() || body["duration"].is_number());
    }

    #[test]
    fn unknown_route_is_404() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "GET", "/nope", b"");
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn play_then_status_reflects_playing() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "POST", "/api/play", br#"{"file":"song.mp3"}"#);
        assert_eq!(resp.status, 200);
        assert_eq!(json_body(&resp)["file"], "song.mp3");
        assert_eq!(json_body(&resp)["playing"], true);
    }

    #[test]
    fn play_rejects_unknown_file() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "POST", "/api/play", br#"{"file":"missing.mp3"}"#);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn play_rejects_path_traversal() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "POST", "/api/play", br#"{"file":"../song.mp3"}"#);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn toggle_pauses_and_resumes() {
        let (state, _dir) = test_state();
        handle(&state, "POST", "/api/play", br#"{"file":"song.mp3"}"#);
        let paused = handle(&state, "POST", "/api/toggle", b"");
        assert_eq!(json_body(&paused)["playing"], false);
        let resumed = handle(&state, "POST", "/api/toggle", b"");
        assert_eq!(json_body(&resumed)["playing"], true);
    }

    #[test]
    fn restart_resumes_playing() {
        let (state, _dir) = test_state();
        handle(&state, "POST", "/api/play", br#"{"file":"song.mp3"}"#);
        handle(&state, "POST", "/api/toggle", b""); // pause
        let resp = handle(&state, "POST", "/api/restart", b"");
        assert_eq!(json_body(&resp)["playing"], true);
    }

    #[test]
    fn loop_flag_persists_in_status() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "POST", "/api/loop", br#"{"loop":true}"#);
        assert_eq!(json_body(&resp)["loop"], true);
        let status = handle(&state, "GET", "/api/status", b"");
        assert_eq!(json_body(&status)["loop"], true);
    }

    #[test]
    fn loop_defaults_to_on() {
        let (state, _dir) = test_state();
        let status = handle(&state, "GET", "/api/status", b"");
        assert_eq!(json_body(&status)["loop"], true);
    }

    #[test]
    fn volume_defaults_to_full_and_persists_in_status() {
        let (state, _dir) = test_state();
        let status = handle(&state, "GET", "/api/status", b"");
        assert_eq!(json_body(&status)["volume"], 1.0);

        let resp = handle(&state, "POST", "/api/volume", br#"{"volume":0.5}"#);
        assert_eq!(json_body(&resp)["volume"], 0.5);
        let status = handle(&state, "GET", "/api/status", b"");
        assert_eq!(json_body(&status)["volume"], 0.5);
    }

    #[test]
    fn volume_rejects_bad_body() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "POST", "/api/volume", b"not json");
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn mutating_call_persists_state_to_disk() {
        let (state, _dir) = test_state();
        assert!(!state.state_file.exists());

        handle(&state, "POST", "/api/play", br#"{"file":"song.mp3"}"#);

        let saved = state_store::load(&state.state_file).unwrap();
        assert_eq!(saved.file, "song.mp3");
        assert!(saved.playing);
    }

    #[test]
    fn plain_status_poll_does_not_persist() {
        let (state, _dir) = test_state();
        handle(&state, "GET", "/api/status", b"");
        assert!(!state.state_file.exists());
    }

    #[test]
    fn serves_pwa_assets() {
        let (state, _dir) = test_state();

        let manifest = handle(&state, "GET", "/manifest.webmanifest", b"");
        assert_eq!(manifest.status, 200);
        assert_eq!(manifest.content_type, "application/manifest+json");

        let sw = handle(&state, "GET", "/sw.js", b"");
        assert_eq!(sw.status, 200);
        assert_eq!(sw.content_type, "application/javascript");

        let icon = handle(&state, "GET", "/icon-192.png", b"");
        assert_eq!(icon.status, 200);
        assert_eq!(icon.content_type, "image/png");
    }
}
