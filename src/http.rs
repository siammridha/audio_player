use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::json;

use crate::capture;
use crate::events::Events;
use crate::library;
use crate::output_device;
use crate::player::{Mode, OutputChoice, Player};
use crate::sleep_timer::{self, TimerState};
use crate::state_store;

pub struct AppState {
    pub player: Arc<dyn Player>,
    pub music_dir: PathBuf,
    pub state_file: PathBuf,
    pub sleep_timer_file: PathBuf,
    pub sleep_timer: Mutex<Option<TimerState>>,
    pub output_device_file: PathBuf,
    pub capture_device_file: PathBuf,
    pub events: Events,
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

    fn conflict(msg: &str) -> Self {
        Self::json(409, json!({ "error": msg }))
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

#[derive(Deserialize)]
struct SleepTimerRequest {
    at: u64,
}

#[derive(Deserialize)]
struct OutputDeviceRequest {
    device: OutputChoice,
}

#[derive(Deserialize)]
struct ModeRequest {
    mode: Mode,
}

#[derive(Deserialize)]
struct CaptureDeviceRequest {
    device: String,
}

/// Routes one request to a handler. Kept as a plain function of its inputs
/// (no tiny_http types) so it can be tested without a real socket.
pub fn handle(state: &AppState, method: &str, path: &str, body: &[u8]) -> Response {
    let response = route(state, method, path, body);
    // Other open pages see the change now, not on the next tick.
    if method == "POST" && response.status == 200 {
        publish_status(state);
        publish_files(state);
    }
    response
}

fn route(state: &AppState, method: &str, path: &str, body: &[u8]) -> Response {
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

        ("GET", "/api/files") => Response::json(200, files_json(state)),

        ("GET", "/api/status") => status_response(state),

        ("POST", "/api/play" | "/api/toggle" | "/api/restart")
            if state.player.status().mode == Mode::Capture =>
        {
            Response::conflict("not available in Capture mode")
        }

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

        ("POST", "/api/sleep-timer") => {
            let Ok(req) = serde_json::from_slice::<SleepTimerRequest>(body) else {
                return Response::bad_request("expected { \"at\": <unix ms, in the future> }");
            };
            if req.at <= sleep_timer::now_ms() {
                return Response::bad_request("at must be in the future");
            }
            let timer = TimerState::new(req.at);
            *state.sleep_timer.lock().unwrap() = Some(timer);
            sleep_timer::persist(&state.sleep_timer_file, Some(&timer));
            status_response(state)
        }

        ("POST", "/api/sleep-timer/cancel") => {
            *state.sleep_timer.lock().unwrap() = None;
            sleep_timer::persist(&state.sleep_timer_file, None);
            status_response(state)
        }

        ("GET", "/api/devices") => Response::json(200, devices_json(state)),

        ("POST", "/api/output-device") => {
            let Ok(req) = serde_json::from_slice::<OutputDeviceRequest>(body) else {
                return Response::bad_request("expected { \"device\": \"auto\" | \"<card id>\" }");
            };
            if recording_file(state).is_some() {
                return Response::conflict("stop recording first");
            }
            if let OutputChoice::Card(id) = &req.device
                && !state.player.output_devices().iter().any(|d| &d.id == id)
            {
                return Response::bad_request("unknown device");
            }
            state.player.set_output(req.device.clone());
            output_device::persist(&state.output_device_file, &req.device);
            status_response(state)
        }

        ("POST", "/api/mode") => {
            let Ok(req) = serde_json::from_slice::<ModeRequest>(body) else {
                return Response::bad_request("expected { \"mode\": \"play\" | \"capture\" }");
            };
            if req.mode == Mode::Play && recording_file(state).is_some() {
                return Response::conflict("stop recording first");
            }
            state.player.set_mode(req.mode);
            save_status(state);
            status_response(state)
        }

        ("GET", "/api/capture-devices") => Response::json(200, capture_devices_json(state)),

        ("POST", "/api/capture-device") => {
            let Ok(req) = serde_json::from_slice::<CaptureDeviceRequest>(body) else {
                return Response::bad_request("expected { \"device\": \"<card id>\" }");
            };
            if !state
                .player
                .capture_devices()
                .iter()
                .any(|d| d.id == req.device)
            {
                return Response::bad_request("unknown device");
            }
            if recording_file(state).is_some() {
                return Response::conflict("stop recording first");
            }
            state.player.set_capture_device(Some(req.device.clone()));
            // Stored the same way as the output choice.
            output_device::persist(&state.capture_device_file, &OutputChoice::Card(req.device));
            status_response(state)
        }

        ("POST", "/api/record/start") => {
            let now = (sleep_timer::now_ms() / 1000) as i64;
            let path = capture::new_recording_path(&state.music_dir, now);
            match state.player.start_recording(&path) {
                Ok(()) => status_response(state),
                Err(e) => Response::conflict(&e.to_string()),
            }
        }

        ("POST", "/api/record/stop") => {
            state.player.stop_recording();
            status_response(state)
        }

        ("POST", "/api/delete") => {
            let Ok(req) = serde_json::from_slice::<PlayRequest>(body) else {
                return Response::bad_request("expected { \"file\": \"...\" }");
            };
            if !capture::is_recording_name(&req.file) {
                return Response::bad_request("only recordings can be deleted");
            }
            if recording_file(state).as_ref() == Some(&req.file) {
                return Response::conflict("still recording");
            }
            let Some(path) = library::resolve(&state.music_dir, &req.file) else {
                return Response::bad_request("unknown file");
            };
            if state.player.status().file.as_ref() == Some(&req.file) {
                state.player.unload();
            }
            if let Err(e) = std::fs::remove_file(&path) {
                crate::log::error(&format!("audio-player: failed to delete {}: {e}", req.file));
                return Response::json(500, json!({ "error": e.to_string() }));
            }
            crate::log::info(&format!("audio-player: deleted {}", req.file));
            status_response(state)
        }

        _ => Response::not_found(),
    }
}

fn status_response(state: &AppState) -> Response {
    Response::json(200, status_json(state))
}

fn status_json(state: &AppState) -> serde_json::Value {
    let mut value = serde_json::to_value(state.player.status()).unwrap();
    value["sleepTimer"] = match *state.sleep_timer.lock().unwrap() {
        Some(timer) => json!({ "at": timer.at }),
        None => serde_json::Value::Null,
    };
    let disk = capture::disk_space(&state.music_dir);
    value["capture"]["freeBytes"] = json!(disk.map(|(free, _)| free));
    value["capture"]["totalBytes"] = json!(disk.map(|(_, total)| total));
    value
}

fn files_json(state: &AppState) -> serde_json::Value {
    // The page shows the file being recorded on its own, from status.
    let recording = recording_file(state);
    let files: Vec<_> = library::list_files(&state.music_dir)
        .into_iter()
        .filter(|f| Some(&f.name) != recording.as_ref())
        .collect();
    serde_json::to_value(files).unwrap()
}

fn devices_json(state: &AppState) -> serde_json::Value {
    serde_json::to_value(state.player.output_devices()).unwrap()
}

fn capture_devices_json(state: &AppState) -> serde_json::Value {
    serde_json::to_value(state.player.capture_devices()).unwrap()
}

pub fn publish_status(state: &AppState) {
    state.events.publish("status", &status_json(state));
}

pub fn publish_files(state: &AppState) {
    state.events.publish("files", &files_json(state));
}

pub fn publish_devices(state: &AppState) {
    state.events.publish("devices", &devices_json(state));
    state.events.publish("capture-devices", &capture_devices_json(state));
}

fn recording_file(state: &AppState) -> Option<String> {
    state.player.status().capture.file
}

/// Called by the mutating endpoints after they change the player, not by the
/// plain `GET /api/status` or the status events (which would write every
/// second).
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
            sleep_timer_file: dir.path().join("sleep_timer.json"),
            sleep_timer: Mutex::new(None),
            output_device_file: dir.path().join("output_device.json"),
            capture_device_file: dir.path().join("capture_device.json"),
            events: Events::default(),
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
    fn status_has_no_sleep_timer_by_default() {
        let (state, _dir) = test_state();
        let status = handle(&state, "GET", "/api/status", b"");
        assert_eq!(json_body(&status)["sleepTimer"], serde_json::Value::Null);
    }

    #[test]
    fn setting_a_sleep_timer_shows_up_in_status() {
        let (state, _dir) = test_state();
        let at = sleep_timer::now_ms() + 60_000;
        let resp = handle(
            &state,
            "POST",
            "/api/sleep-timer",
            format!(r#"{{"at":{at}}}"#).as_bytes(),
        );
        assert_eq!(json_body(&resp)["sleepTimer"]["at"], at);

        let status = handle(&state, "GET", "/api/status", b"");
        assert_eq!(json_body(&status)["sleepTimer"]["at"], at);
    }

    #[test]
    fn sleep_timer_rejects_a_time_that_is_not_in_the_future() {
        let (state, _dir) = test_state();
        let at = sleep_timer::now_ms();
        let resp = handle(
            &state,
            "POST",
            "/api/sleep-timer",
            format!(r#"{{"at":{at}}}"#).as_bytes(),
        );
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn sleep_timer_rejects_bad_body() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "POST", "/api/sleep-timer", b"not json");
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn cancelling_a_sleep_timer_clears_it() {
        let (state, _dir) = test_state();
        let at = sleep_timer::now_ms() + 60_000;
        handle(
            &state,
            "POST",
            "/api/sleep-timer",
            format!(r#"{{"at":{at}}}"#).as_bytes(),
        );

        let resp = handle(&state, "POST", "/api/sleep-timer/cancel", b"");
        assert_eq!(json_body(&resp)["sleepTimer"], serde_json::Value::Null);

        let status = handle(&state, "GET", "/api/status", b"");
        assert_eq!(json_body(&status)["sleepTimer"], serde_json::Value::Null);
    }

    #[test]
    fn setting_a_sleep_timer_persists_it_to_disk() {
        let (state, _dir) = test_state();
        let at = sleep_timer::now_ms() + 60_000;
        handle(
            &state,
            "POST",
            "/api/sleep-timer",
            format!(r#"{{"at":{at}}}"#).as_bytes(),
        );

        let saved = sleep_timer::load(&state.sleep_timer_file, 0).unwrap();
        assert_eq!(saved.at, at);
    }

    #[test]
    fn lists_output_devices() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "GET", "/api/devices", b"");
        assert_eq!(resp.status, 200);
        assert_eq!(
            json_body(&resp)[0],
            json!({ "id": "Device", "name": "USB PnP Sound Device" })
        );
    }

    #[test]
    fn output_choice_defaults_to_auto() {
        let (state, _dir) = test_state();
        let status = handle(&state, "GET", "/api/status", b"");
        assert_eq!(json_body(&status)["outputChoice"], "auto");
    }

    #[test]
    fn choosing_an_output_device_shows_up_in_status_and_is_saved() {
        let (state, _dir) = test_state();
        let resp = handle(
            &state,
            "POST",
            "/api/output-device",
            br#"{"device":"rt5672"}"#,
        );
        assert_eq!(json_body(&resp)["outputChoice"], "rt5672");

        let status = handle(&state, "GET", "/api/status", b"");
        assert_eq!(json_body(&status)["outputChoice"], "rt5672");
        assert_eq!(
            output_device::load(&state.output_device_file),
            OutputChoice::Card("rt5672".to_string())
        );

        let resp = handle(
            &state,
            "POST",
            "/api/output-device",
            br#"{"device":"auto"}"#,
        );
        assert_eq!(json_body(&resp)["outputChoice"], "auto");
        assert_eq!(
            output_device::load(&state.output_device_file),
            OutputChoice::Auto
        );
    }

    #[test]
    fn output_device_rejects_an_unknown_card() {
        let (state, _dir) = test_state();
        let resp = handle(
            &state,
            "POST",
            "/api/output-device",
            br#"{"device":"nope"}"#,
        );
        assert_eq!(resp.status, 400);
        assert!(!state.output_device_file.exists());
    }

    #[test]
    fn output_device_rejects_bad_body() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "POST", "/api/output-device", b"not json");
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
    fn a_change_is_sent_to_open_pages() {
        let (state, _dir) = test_state();
        let events = state.events.subscribe();
        post(&state, "/api/loop", br#"{"loop":false}"#);
        let sent: Vec<String> = events.try_iter().collect();
        assert!(sent.iter().any(|e| e.starts_with("event: status\n") && e.contains(r#""loop":false"#)));
        assert!(sent.iter().any(|e| e.starts_with("event: files\n")));
    }

    fn post(state: &AppState, path: &str, body: &[u8]) -> Response {
        handle(state, "POST", path, body)
    }

    #[test]
    fn play_mode_still_names_the_capture_card() {
        let (state, _dir) = test_state();
        let status = json_body(&handle(&state, "GET", "/api/status", b""));
        assert_eq!(status["mode"], "play");
        assert_eq!(status["capture"]["device"], "Device_1");
        assert_eq!(status["capture"]["recording"], false);
    }

    #[test]
    fn status_shows_the_music_folder_disk_space() {
        let (state, _dir) = test_state();
        let status = json_body(&handle(&state, "GET", "/api/status", b""));
        let free = status["capture"]["freeBytes"].as_u64().unwrap();
        let total = status["capture"]["totalBytes"].as_u64().unwrap();
        assert!(free > 0);
        assert!(total >= free);
    }

    #[test]
    fn capture_mode_pauses_playback_and_shows_the_newest_card() {
        let (state, _dir) = test_state();
        post(&state, "/api/play", br#"{"file":"song.mp3"}"#);

        let body = json_body(&post(&state, "/api/mode", br#"{"mode":"capture"}"#));
        assert_eq!(body["mode"], "capture");
        assert_eq!(body["playing"], false);
        assert_eq!(body["capture"]["device"], "Device_1");
        assert_eq!(body["capture"]["recording"], false);

        let body = json_body(&post(&state, "/api/mode", br#"{"mode":"play"}"#));
        assert_eq!(body["mode"], "play");
        assert_eq!(body["playing"], false);
    }

    #[test]
    fn mode_rejects_bad_body() {
        let (state, _dir) = test_state();
        assert_eq!(post(&state, "/api/mode", br#"{"mode":"nope"}"#).status, 400);
    }

    #[test]
    fn playback_controls_are_refused_in_capture_mode() {
        let (state, _dir) = test_state();
        post(&state, "/api/mode", br#"{"mode":"capture"}"#);
        assert_eq!(
            post(&state, "/api/play", br#"{"file":"song.mp3"}"#).status,
            409
        );
        assert_eq!(post(&state, "/api/toggle", b"").status, 409);
        assert_eq!(post(&state, "/api/restart", b"").status, 409);
    }

    #[test]
    fn lists_capture_devices() {
        let (state, _dir) = test_state();
        let resp = handle(&state, "GET", "/api/capture-devices", b"");
        assert_eq!(resp.status, 200);
        assert_eq!(
            json_body(&resp)[1],
            json!({ "id": "Device_1", "name": "BlueTrm USB PnP Audio Device" })
        );
    }

    #[test]
    fn choosing_a_capture_device_shows_up_in_status_and_is_saved() {
        let (state, _dir) = test_state();
        post(&state, "/api/mode", br#"{"mode":"capture"}"#);
        let body = json_body(&post(
            &state,
            "/api/capture-device",
            br#"{"device":"rt5672"}"#,
        ));
        assert_eq!(body["capture"]["device"], "rt5672");
        assert_eq!(body["capture"]["deviceChoice"], "rt5672");
        assert_eq!(
            output_device::load(&state.capture_device_file),
            OutputChoice::Card("rt5672".to_string())
        );
    }

    #[test]
    fn capture_device_rejects_an_unknown_card() {
        let (state, _dir) = test_state();
        let resp = post(&state, "/api/capture-device", br#"{"device":"nope"}"#);
        assert_eq!(resp.status, 400);
        assert!(!state.capture_device_file.exists());
    }

    #[test]
    fn recording_is_refused_in_play_mode() {
        let (state, _dir) = test_state();
        assert_eq!(post(&state, "/api/record/start", b"").status, 409);
    }

    #[test]
    fn recording_saves_a_wav_in_the_music_folder() {
        let (state, _dir) = test_state();
        post(&state, "/api/mode", br#"{"mode":"capture"}"#);

        let body = json_body(&post(&state, "/api/record/start", b""));
        assert_eq!(body["capture"]["recording"], true);
        let file = body["capture"]["file"].as_str().unwrap().to_string();
        assert!(file.starts_with("capture-") && file.ends_with(".wav"));

        // While recording, the file isn't in the list and nothing that would
        // interrupt it is allowed.
        let files = json_body(&handle(&state, "GET", "/api/files", b""));
        assert!(files.as_array().unwrap().iter().all(|f| f["name"] != file));
        assert_eq!(post(&state, "/api/record/start", b"").status, 409);
        assert_eq!(post(&state, "/api/mode", br#"{"mode":"play"}"#).status, 409);
        assert_eq!(
            post(&state, "/api/output-device", br#"{"device":"rt5672"}"#).status,
            409
        );
        assert_eq!(
            post(&state, "/api/capture-device", br#"{"device":"rt5672"}"#).status,
            409
        );
        let delete = format!(r#"{{"file":"{file}"}}"#);
        assert_eq!(post(&state, "/api/delete", delete.as_bytes()).status, 409);

        std::thread::sleep(std::time::Duration::from_millis(50));
        let body = json_body(&post(&state, "/api/record/stop", b""));
        assert_eq!(body["capture"]["recording"], false);

        let files = json_body(&handle(&state, "GET", "/api/files", b""));
        let saved = files
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == file)
            .expect("recording is in the file list");
        assert!(saved["duration"].is_number());
    }

    #[test]
    fn deletes_a_recording() {
        let (state, dir) = test_state();
        let path = dir.path().join("capture-2026-10-14-14-03.wav");
        fs::write(&path, b"fake audio").unwrap();

        let resp = post(
            &state,
            "/api/delete",
            br#"{"file":"capture-2026-10-14-14-03.wav"}"#,
        );
        assert_eq!(resp.status, 200);
        assert!(!path.exists());
    }

    #[test]
    fn delete_refuses_anything_but_a_recording() {
        let (state, dir) = test_state();
        for body in [
            &br#"{"file":"song.mp3"}"#[..],
            br#"{"file":"capture-missing.wav"}"#,
            br#"{"file":"capture-../song.mp3"}"#,
            br#"{"file":"capture-notes.txt"}"#,
            b"not json",
        ] {
            assert_eq!(post(&state, "/api/delete", body).status, 400);
        }
        assert!(dir.path().join("song.mp3").exists());
    }

    #[test]
    fn deleting_the_loaded_track_unloads_it() {
        let (state, dir) = test_state();
        fs::write(dir.path().join("capture-a.wav"), b"fake audio").unwrap();
        post(&state, "/api/play", br#"{"file":"capture-a.wav"}"#);

        let body = json_body(&post(&state, "/api/delete", br#"{"file":"capture-a.wav"}"#));
        assert!(body["file"].is_null());
        assert_eq!(body["playing"], false);
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
