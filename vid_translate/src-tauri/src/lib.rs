mod audio;
mod recognizer;
mod transcriber;

use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tauri::Emitter;

#[derive(Serialize, Clone)]
struct TranscriptionEvent {
    text: String,
    // JA mode only: the word currently being spoken, still in Japanese.
    // Empty in English mode and on final events.
    current: String,
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

fn translate_server_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vid_translate")
        .join("translate_server.py")
}

/// Spawns the argostranslate Python server and returns (stdin_writer, stdout_reader).
/// The server stays alive for the lifetime of the pipeline — no per-call startup cost.
fn spawn_translate_server() -> Result<
    (
        std::io::BufWriter<std::process::ChildStdin>,
        BufReader<std::process::ChildStdout>,
    ),
    String,
> {
    let script = translate_server_path();
    if !script.exists() {
        return Err(format!(
            "translate_server.py not found at {}",
            script.display()
        ));
    }

    let mut child = std::process::Command::new("python3")
        .arg(&script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn translate_server.py: {}", e))?;

    let stdin = std::io::BufWriter::new(child.stdin.take().unwrap());
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // Wait for "READY" line before returning
    let mut ready = String::new();
    stdout
        .read_line(&mut ready)
        .map_err(|e| format!("translate server did not send READY: {}", e))?;

    // Leak the child so it stays alive; it exits when stdin is dropped
    std::mem::forget(child);

    Ok((stdin, stdout))
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
                        current: String::new(),
                        kind: "partial".into(),
                    });
                }
                Final(text) => {
                    let _ = app_for_result.emit("transcription", TranscriptionEvent {
                        text,
                        current: String::new(),
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

    let (mut tx_stdin, rx_stdout) = match spawn_translate_server() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[translate] {}", e);
            let _ = app_handle.emit("status", StatusEvent { state: "error".into() });
            return;
        }
    };

    let rx = audio::start_capture(stop_flag.clone());
    let _ = app_handle.emit("status", StatusEvent { state: "listening".into() });

    let (tx_phrase, rx_phrase) = std::sync::mpsc::channel::<String>();
    let app_for_translate = app_handle.clone();

    // Translation thread owns the subprocess pipes and emits English finals
    // directly as soon as each translation completes — no polling needed.
    std::thread::spawn(move || {
        let mut stdout = rx_stdout;
        while let Ok(phrase) = rx_phrase.recv() {
            // If requests piled up while we were translating, only do the latest
            let latest = std::iter::once(phrase)
                .chain(rx_phrase.try_iter())
                .last()
                .unwrap();

            if writeln!(tx_stdin, "{}", latest).is_err() { break; }
            if tx_stdin.flush().is_err() { break; }

            let mut english = String::new();
            if stdout.read_line(&mut english).is_err() { break; }
            let english = english.trim().to_string();

            if !english.is_empty() {
                let _ = app_for_translate.emit("transcription", TranscriptionEvent {
                    text: english,
                    current: String::new(),
                    kind: "final".into(),
                });
            }
        }
    });

    // Force-translate if no sentence boundary for FLUSH_SECS (long unbroken speech).
    const FLUSH_SECS: u64 = 8;
    let mut last_sent_at = std::time::Instant::now();
    let mut last_phrase_sent = String::new();

    let app_for_partial = app_handle.clone();
    let result = recognizer::run(
        ja_path.to_str().unwrap_or(""),
        rx,
        move |ev| {
            use recognizer::RecognitionResult::*;
            match ev {
                Partial(text) => {
                    let _ = app_for_partial.emit("transcription", TranscriptionEvent {
                        text: text.clone(),
                        current: String::new(),
                        kind: "partial".into(),
                    });

                    if !text.is_empty()
                        && last_sent_at.elapsed().as_secs() >= FLUSH_SECS
                        && text != last_phrase_sent
                    {
                        let _ = tx_phrase.send(text.clone());
                        last_phrase_sent = text;
                        last_sent_at = std::time::Instant::now();
                    }
                }
                Final(text) => {
                    last_sent_at = std::time::Instant::now();
                    // Skip if we just flushed the exact same partial a moment ago
                    if !text.is_empty() && text != last_phrase_sent {
                        last_phrase_sent = text.clone();
                        let _ = tx_phrase.send(text);
                    }
                }
                Silent => {
                    last_sent_at = std::time::Instant::now();
                }
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

    const WINDOW_SAMPLES: usize = 6 * audio::SAMPLE_RATE as usize;
    let mut buffer: Vec<f32> = Vec::with_capacity(WINDOW_SAMPLES);

    for chunk in rx {
        for sample in &chunk {
            buffer.push(*sample as f32 / 32768.0);
        }

        if buffer.len() >= WINDOW_SAMPLES {
            let _ = app_handle.emit("status", StatusEvent { state: "processing".into() });

            match transcriber.transcribe(&buffer, "ja") {
                Ok(text) if !text.is_empty() => {
                    let _ = app_handle.emit("transcription", TranscriptionEvent {
                        text,
                        current: String::new(),
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
