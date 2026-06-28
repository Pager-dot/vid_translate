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
        .stderr(std::process::Stdio::inherit())
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

    // tx_phrase sends raw audio chunks (Vec<i16>) to the Whisper translate thread.
    let (tx_phrase, rx_phrase) = std::sync::mpsc::channel::<Vec<i16>>();
    let app_for_translate = app_handle.clone();

    // Audio accumulator: filled by the forwarding thread, drained on each Vosk Final.
    let audio_acc: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let audio_acc_fwd = audio_acc.clone();
    let audio_acc_cb  = audio_acc.clone();

    // Forwarding thread: tees the single-consumer audio channel into
    //   (a) the Vosk recognizer channel  (b) the shared accumulator
    let (tx_to_vosk, rx_to_vosk) = std::sync::mpsc::channel::<Vec<i16>>();
    std::thread::spawn(move || {
        for chunk in rx {
            audio_acc_fwd.lock().unwrap().extend_from_slice(&chunk);
            let _ = tx_to_vosk.send(chunk);
        }
    });

    // Translation thread: receives audio Vec<i16>, sends binary to Faster Whisper
    // over stdin, reads streaming text back, emits events.
    std::thread::spawn(move || {
        let mut stdout = rx_stdout;
        'outer: while let Ok(audio) = rx_phrase.recv() {
            // Drain stale — only translate the most recent chunk
            let latest: Vec<i16> = std::iter::once(audio)
                .chain(rx_phrase.try_iter())
                .last()
                .unwrap();

            // Write binary: 4-byte LE length then raw i16 samples
            let bytes: Vec<u8> = latest.iter()
                .flat_map(|&s| s.to_le_bytes())
                .collect();
            let n_bytes = bytes.len() as u32;
            if tx_stdin.write_all(&n_bytes.to_le_bytes()).is_err() { break; }
            if tx_stdin.write_all(&bytes).is_err() { break; }
            if tx_stdin.flush().is_err() { break; }

            // Read streaming text lines until "---" done marker
            let mut last_text = String::new();
            loop {
                let mut line = String::new();
                if stdout.read_line(&mut line).is_err() { break 'outer; }
                let line = line.trim().to_string();

                if line == "---" {
                    if !last_text.is_empty() {
                        let _ = app_for_translate.emit("transcription", TranscriptionEvent {
                            text: last_text,
                            current: String::new(),
                            kind: "final".into(),
                        });
                    }
                    break;
                } else if !line.is_empty() {
                    last_text = line.clone();
                    let _ = app_for_translate.emit("transcription", TranscriptionEvent {
                        text: line,
                        current: String::new(),
                        kind: "streaming-en".into(),
                    });
                }
            }
        }
    });

    // Force-flush accumulated audio if no sentence boundary for FLUSH_SECS.
    const FLUSH_SECS: u64 = 8;
    const MIN_SAMPLES: usize = 8_000; // 0.5 s at 16 kHz — skip misfires
    let mut last_sent_at = std::time::Instant::now();

    let app_for_partial = app_handle.clone();
    let result = recognizer::run(
        ja_path.to_str().unwrap_or(""),
        rx_to_vosk,
        move |ev| {
            use recognizer::RecognitionResult::*;
            match ev {
                Partial(text) => {
                    let _ = app_for_partial.emit("transcription", TranscriptionEvent {
                        text: text.clone(),
                        current: String::new(),
                        kind: "partial".into(),
                    });
                    // 8-second flush: drain audio and translate
                    if !text.is_empty() && last_sent_at.elapsed().as_secs() >= FLUSH_SECS {
                        let audio = std::mem::take(&mut *audio_acc_cb.lock().unwrap());
                        if audio.len() >= MIN_SAMPLES {
                            let _ = tx_phrase.send(audio);
                            last_sent_at = std::time::Instant::now();
                        }
                    }
                }
                Final(_) => {
                    last_sent_at = std::time::Instant::now();
                    // Drain accumulated audio and send to Faster Whisper
                    let audio = std::mem::take(&mut *audio_acc_cb.lock().unwrap());
                    if audio.len() >= MIN_SAMPLES {
                        let _ = tx_phrase.send(audio);
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
