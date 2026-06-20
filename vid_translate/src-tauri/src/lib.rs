mod audio;
mod recognizer;
mod transcriber;

use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tauri::Emitter;

#[derive(Serialize, Clone)]
struct TranscriptionEvent {
    text: String,
    #[serde(rename = "type")]
    kind: String, // "partial" | "final"
}

#[derive(Serialize, Clone)]
struct StatusEvent {
    state: String,
}

struct PipelineState {
    stop_flag: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Default for PipelineState {
    fn default() -> Self {
        Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }
}

fn vosk_model_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vid_translate")
        .join("vosk-model")
}

fn whisper_model_path() -> std::path::PathBuf {
    let models_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vid_translate")
        .join("models");
    let tiny = models_dir.join("ggml-tiny.bin");
    if tiny.exists() { tiny } else { models_dir.join("ggml-base.bin") }
}

#[tauri::command]
fn get_model_path() -> String {
    vosk_model_path().to_string_lossy().to_string()
}

#[tauri::command]
fn start_listening(
    state: tauri::State<Mutex<PipelineState>>,
    app: tauri::AppHandle,
) {
    let mut pipeline = state.lock().unwrap();

    pipeline.stop_flag.store(true, Ordering::Relaxed);
    if let Some(h) = pipeline.thread.take() {
        drop(h);
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    pipeline.stop_flag = stop_flag.clone();

    let app_handle = app.clone();
    let vosk_path = vosk_model_path();

    let handle = std::thread::spawn(move || {
        if !vosk_path.exists() {
            let _ = app_handle.emit("status", StatusEvent { state: "model_missing".into() });
            return;
        }

        let _ = app_handle.emit("status", StatusEvent { state: "loading".into() });

        let rx = audio::start_capture(stop_flag.clone());
        let app_for_result = app_handle.clone();

        let _ = app_handle.emit("status", StatusEvent { state: "listening".into() });

        let result = recognizer::run(
            vosk_path.to_str().unwrap_or(""),
            rx,
            move |result| {
                use recognizer::RecognitionResult::*;
                match result {
                    Partial(text) => {
                        let _ = app_for_result.emit("transcription", TranscriptionEvent {
                            text,
                            kind: "partial".into(),
                        });
                    }
                    Final(text) => {
                        let _ = app_for_result.emit("transcription", TranscriptionEvent {
                            text,
                            kind: "final".into(),
                        });
                    }
                    Silent => {}
                }
            },
        );

        if let Err(e) = result {
            eprintln!("[lib] Recognizer error: {}", e);
            let _ = app_handle.emit("status", StatusEvent { state: "error".into() });
        } else {
            let _ = app_handle.emit("status", StatusEvent { state: "idle".into() });
        }
    });

    pipeline.thread = Some(handle);
}

#[tauri::command]
fn stop_listening(state: tauri::State<Mutex<PipelineState>>) {
    let pipeline = state.lock().unwrap();
    pipeline.stop_flag.store(true, Ordering::Relaxed);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(Mutex::new(PipelineState::default()))
        .invoke_handler(tauri::generate_handler![
            start_listening,
            stop_listening,
            get_model_path,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
