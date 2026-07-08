mod audio;
mod recognizer;

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

fn vosk_es_model_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vid_translate")
        .join("vosk-model-es")
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
fn get_vosk_es_model_path() -> String {
    vosk_es_model_path().to_string_lossy().to_string()
}

/// Picks whichever Python binary name is actually on PATH: "python3" on Linux/macOS,
/// "python" on most Windows installs. Resolved once and cached.
fn python_command() -> &'static str {
    use std::sync::OnceLock;
    static PYTHON_BIN: OnceLock<&'static str> = OnceLock::new();
    *PYTHON_BIN.get_or_init(|| {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
        {
            "python3"
        } else {
            "python"
        }
    })
}

#[tauri::command]
fn get_platform() -> String {
    std::env::consts::OS.to_string()
}

fn translate_server_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vid_translate")
        .join("translate_server.py")
}

/// Spawns the translate_server.py process and returns (stdin_writer, stdout_reader).
/// The server stays alive for the lifetime of the pipeline — no per-call startup cost.
/// `source_lang` is an ISO-639-1 code ("ja" or "es") telling the script which
/// source language's system prompt/filler handling to use.
fn spawn_translate_server(
    ollama_key: Option<String>,
    ollama_model: Option<String>,
    source_lang: &str,
) -> Result<
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

    let mut cmd = std::process::Command::new(python_command());
    cmd.arg(&script)
        .env("SOURCE_LANG", source_lang)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    if let Some(key) = ollama_key.filter(|k| !k.is_empty()) {
        cmd.env("OLLAMA_KEY", key);
    }
    if let Some(model) = ollama_model.filter(|m| !m.is_empty()) {
        cmd.env("OLLAMA_MODEL", model);
    }

    let mut child = cmd
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
    ollama_key: Option<String>,
    ollama_model: Option<String>,
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
            "vosk-ja" => run_vosk_ja_pipeline(app_handle, stop_flag, ollama_key, ollama_model),
            "vosk-es" => run_vosk_es_pipeline(app_handle, stop_flag, ollama_key, ollama_model),
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

fn run_vosk_es_pipeline(
    app_handle: tauri::AppHandle,
    stop_flag: Arc<AtomicBool>,
    ollama_key: Option<String>,
    ollama_model: Option<String>,
) {
    let es_path = vosk_es_model_path();
    if !es_path.exists() {
        let _ = app_handle.emit("status", StatusEvent { state: "vosk_es_model_missing".into() });
        return;
    }

    let _ = app_handle.emit("status", StatusEvent { state: "loading".into() });

    let (tx_stdin, rx_stdout) = match spawn_translate_server(ollama_key, ollama_model, "es") {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[translate] {}", e);
            let _ = app_handle.emit("status", StatusEvent { state: "error".into() });
            return;
        }
    };

    let tx_stdin = Arc::new(Mutex::new(tx_stdin));
    let tx_stdin_cb = tx_stdin.clone();

    let rx = audio::start_capture(stop_flag.clone());
    let _ = app_handle.emit("status", StatusEvent { state: "listening".into() });

    let app_for_translate = app_handle.clone();

    // Reader thread: receives translated lines from the translate server, emits events
    std::thread::spawn(move || {
        let mut stdout = rx_stdout;
        loop {
            let mut last_text = String::new();
            loop {
                let mut line = String::new();
                if stdout.read_line(&mut line).unwrap_or(0) == 0 { return; }
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

    let app_for_partial = app_handle.clone();
    let result = recognizer::run(
        es_path.to_str().unwrap_or(""),
        rx,
        move |ev| {
            use recognizer::RecognitionResult::*;
            match ev {
                Partial(text) => {
                    let _ = app_for_partial.emit("transcription", TranscriptionEvent {
                        text,
                        current: String::new(),
                        kind: "partial".into(),
                    });
                }
                Final(text) if !text.is_empty() => {
                    eprintln!("[vosk→rust] sending: {:?}", text);
                    let payload = serde_json::json!({"text": text});
                    let mut msg = serde_json::to_string(&payload).unwrap();
                    msg.push('\n');
                    let mut stdin = tx_stdin_cb.lock().unwrap();
                    let _ = stdin.write_all(msg.as_bytes());
                    let _ = stdin.flush();
                }
                _ => {}
            }
        },
    );

    if let Err(e) = result {
        eprintln!("[lib] ES recognizer error: {}", e);
        let _ = app_handle.emit("status", StatusEvent { state: "error".into() });
    } else {
        let _ = app_handle.emit("status", StatusEvent { state: "idle".into() });
    }
}

fn is_japanese_text(text: &str) -> bool {
    text.chars().any(|c| {
        ('\u{3040}'..='\u{309F}').contains(&c) || // hiragana
        ('\u{30A0}'..='\u{30FF}').contains(&c) || // katakana
        ('\u{4E00}'..='\u{9FFF}').contains(&c) || // CJK unified
        ('\u{3400}'..='\u{4DBF}').contains(&c)    // CJK extension A
    })
}

fn run_vosk_ja_pipeline(
    app_handle: tauri::AppHandle,
    stop_flag: Arc<AtomicBool>,
    ollama_key: Option<String>,
    ollama_model: Option<String>,
) {
    let ja_path = vosk_ja_model_path();
    if !ja_path.exists() {
        let _ = app_handle.emit("status", StatusEvent { state: "vosk_ja_model_missing".into() });
        return;
    }

    let _ = app_handle.emit("status", StatusEvent { state: "loading".into() });

    let (tx_stdin, rx_stdout) = match spawn_translate_server(ollama_key, ollama_model, "ja") {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[translate] {}", e);
            let _ = app_handle.emit("status", StatusEvent { state: "error".into() });
            return;
        }
    };

    // Wrap stdin in Mutex so the Vosk callback (sync) can write to it
    let tx_stdin = Arc::new(Mutex::new(tx_stdin));
    let tx_stdin_cb = tx_stdin.clone();

    let rx = audio::start_capture(stop_flag.clone());
    let _ = app_handle.emit("status", StatusEvent { state: "listening".into() });

    let app_for_translate = app_handle.clone();

    // Reader thread: receives translated lines from the translate server, emits events
    std::thread::spawn(move || {
        let mut stdout = rx_stdout;
        loop {
            let mut last_text = String::new();
            loop {
                let mut line = String::new();
                if stdout.read_line(&mut line).unwrap_or(0) == 0 { return; }
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

    let app_for_partial = app_handle.clone();
    let result = recognizer::run(
        ja_path.to_str().unwrap_or(""),
        rx,
        move |ev| {
            use recognizer::RecognitionResult::*;
            match ev {
                Partial(text) => {
                    let _ = app_for_partial.emit("transcription", TranscriptionEvent {
                        text,
                        current: String::new(),
                        kind: "partial".into(),
                    });
                }
                Final(text) if !text.is_empty() => {
                    if is_japanese_text(&text) {
                        eprintln!("[vosk→rust] sending: {:?}", text);
                        let payload = serde_json::json!({"text": text});
                        let mut msg = serde_json::to_string(&payload).unwrap();
                        msg.push('\n');
                        let mut stdin = tx_stdin_cb.lock().unwrap();
                        let _ = stdin.write_all(msg.as_bytes());
                        let _ = stdin.flush();
                    } else {
                        eprintln!("[vosk→rust] english passthrough: {:?}", text);
                        let _ = app_for_partial.emit("transcription", TranscriptionEvent {
                            text,
                            current: String::new(),
                            kind: "final".into(),
                        });
                    }
                }
                _ => {}
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

#[tauri::command]
fn stop_listening(state: tauri::State<Mutex<PipelineState>>) {
    let pipeline = state.lock().unwrap();
    pipeline.stop_flag.store(true, Ordering::Relaxed);
}

#[derive(Serialize, Clone)]
struct PullProgressEvent {
    status: String,
    completed: Option<u64>,
    total: Option<u64>,
}

/// Calls local Ollama pull API and streams progress as "pull_progress" events.
/// Model name is passed as sys.argv[1] to avoid shell injection.
#[tauri::command]
fn pull_model(app: tauri::AppHandle, model: String) {
    std::thread::spawn(move || {
        let python_code = r#"
import urllib.request, json, sys
model = sys.argv[1]
url = "http://localhost:11434/api/pull"
payload = json.dumps({"model": model, "stream": True}).encode()
req = urllib.request.Request(url, data=payload, headers={"Content-Type": "application/json"})
try:
    with urllib.request.urlopen(req, timeout=600) as r:
        for line in r:
            line = line.strip()
            if line:
                print(line.decode(), flush=True)
except Exception as e:
    print(json.dumps({"status": "error", "error": str(e)}), flush=True)
"#;
        let mut child = match std::process::Command::new(python_command())
            .arg("-c").arg(python_code)
            .arg(&model)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[pull] spawn failed: {}", e);
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                if let Ok(line) = line {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                        let event = PullProgressEvent {
                            status: val.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                            completed: val.get("completed").and_then(|v| v.as_u64()),
                            total: val.get("total").and_then(|v| v.as_u64()),
                        };
                        let _ = app.emit("pull_progress", &event);
                    }
                }
            }
        }
        child.wait().ok();
    });
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
            get_vosk_es_model_path,
            get_platform,
            pull_model,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
