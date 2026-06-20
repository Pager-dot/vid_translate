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

fn vosk_ja_model_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vid_translate")
        .join("vosk-model-ja")
}

fn whisper_model_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vid_translate")
        .join("models")
        .join("ggml-base.bin")
}

#[tauri::command]
fn get_model_path() -> String {
    vosk_model_path().to_string_lossy().to_string()
}

#[tauri::command]
fn get_vosk_ja_model_path() -> String {
    vosk_ja_model_path().to_string_lossy().to_string()
}

#[tauri::command]
fn get_whisper_model_path() -> String {
    whisper_model_path().to_string_lossy().to_string()
}

/// Translate Japanese text to English via MyMemory (free, no API key needed).
/// Blocks for ~200–500ms — called only on finals, not on every partial.
fn translate_ja_en(text: &str) -> String {
    let q = urlencoding::encode(text);
    let url = format!(
        "https://api.mymemory.translated.net/get?q={}&langpair=ja|en",
        q
    );
    ureq::get(&url)
        .call()
        .ok()
        .and_then(|r| r.into_json::<serde_json::Value>().ok())
        .and_then(|j| {
            j["responseData"]["translatedText"]
                .as_str()
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

#[tauri::command]
fn start_listening(
    state: tauri::State<Mutex<PipelineState>>,
    app: tauri::AppHandle,
    mode: Option<String>,
) {
    let mut pipeline = state.lock().unwrap();

    pipeline.stop_flag.store(true, Ordering::Relaxed);
    if let Some(h) = pipeline.thread.take() {
        drop(h);
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    pipeline.stop_flag = stop_flag.clone();

    let app_handle = app.clone();
    let mode = mode.unwrap_or_else(|| "vosk".into());

    let handle = std::thread::spawn(move || {
        match mode.as_str() {
            "vosk-ja" => run_vosk_ja_pipeline(app_handle, stop_flag),
            "whisper" => run_whisper_pipeline(app_handle, stop_flag),
            _ => run_vosk_pipeline(app_handle, stop_flag),
        }
    });

    pipeline.thread = Some(handle);
}

fn run_vosk_pipeline(app_handle: tauri::AppHandle, stop_flag: Arc<AtomicBool>) {
    let vosk_path = vosk_model_path();
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
}

fn run_vosk_ja_pipeline(app_handle: tauri::AppHandle, stop_flag: Arc<AtomicBool>) {
    let ja_path = vosk_ja_model_path();
    if !ja_path.exists() {
        let _ = app_handle.emit("status", StatusEvent { state: "vosk_ja_model_missing".into() });
        return;
    }

    let _ = app_handle.emit("status", StatusEvent { state: "loading".into() });
    let rx = audio::start_capture(stop_flag.clone());
    let app_for_result = app_handle.clone();
    let _ = app_handle.emit("status", StatusEvent { state: "listening".into() });

    let result = recognizer::run(
        ja_path.to_str().unwrap_or(""),
        rx,
        move |result| {
            use recognizer::RecognitionResult::*;
            match result {
                // Partials stream live as Japanese — same real-time feel as English mode
                Partial(text) => {
                    let _ = app_for_result.emit("transcription", TranscriptionEvent {
                        text,
                        kind: "partial".into(),
                    });
                }
                // Finals are translated to English; this blocks ~200-500ms but the
                // next sentence is already being spoken, so the delay is imperceptible
                Final(text) => {
                    let english = translate_ja_en(&text);
                    if !english.is_empty() {
                        let _ = app_for_result.emit("transcription", TranscriptionEvent {
                            text: english,
                            kind: "final".into(),
                        });
                    }
                }
                Silent => {}
            }
        },
    );

    if let Err(e) = result {
        eprintln!("[lib] JA recognizer error: {}", e);
        let _ = app_handle.emit("status", StatusEvent { state: "error".into() });
    } else {
        let _ = app_handle.emit("status", StatusEvent { state: "idle".into() });
    }
}

fn run_whisper_pipeline(app_handle: tauri::AppHandle, stop_flag: Arc<AtomicBool>) {
    let model_path = whisper_model_path();
    if !model_path.exists() {
        let _ = app_handle.emit("status", StatusEvent { state: "whisper_model_missing".into() });
        return;
    }

    let _ = app_handle.emit("status", StatusEvent { state: "loading".into() });

    let transcriber = match transcriber::Transcriber::new(model_path.to_str().unwrap_or("")) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[lib] Whisper load error: {}", e);
            let _ = app_handle.emit("status", StatusEvent { state: "error".into() });
            return;
        }
    };

    let rx = audio::start_capture(stop_flag.clone());
    let _ = app_handle.emit("status", StatusEvent { state: "listening".into() });

    // Accumulate 6 seconds of audio before each Whisper inference pass.
    // 6s gives enough lyric content per window without too much latency.
    const WINDOW_SAMPLES: usize = 6 * audio::SAMPLE_RATE as usize;
    let mut buffer: Vec<f32> = Vec::with_capacity(WINDOW_SAMPLES);

    for chunk in rx {
        // Convert S16LE i16 samples → f32 in [-1.0, 1.0] for Whisper
        for sample in &chunk {
            buffer.push(*sample as f32 / 32768.0);
        }

        if buffer.len() >= WINDOW_SAMPLES {
            let _ = app_handle.emit("status", StatusEvent { state: "processing".into() });

            match transcriber.transcribe(&buffer, "ja") {
                Ok(text) if !text.is_empty() => {
                    let _ = app_handle.emit("transcription", TranscriptionEvent {
                        text,
                        kind: "final".into(),
                    });
                }
                Err(e) => eprintln!("[whisper] transcribe error: {}", e),
                _ => {}
            }

            buffer.clear();
            let _ = app_handle.emit("status", StatusEvent { state: "listening".into() });
        }
    }

    let _ = app_handle.emit("status", StatusEvent { state: "idle".into() });
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
            get_vosk_ja_model_path,
            get_whisper_model_path,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
