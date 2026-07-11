mod audio;
mod recognizer;

use serde::Serialize;
use std::io::{BufRead, BufReader, Read, Write};
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

fn vosk_model_url_and_path(kind: &str) -> Result<(&'static str, std::path::PathBuf), String> {
    match kind {
        "en" => Ok((
            "https://alphacephei.com/vosk/models/vosk-model-small-en-us-0.15.zip",
            vosk_model_path(),
        )),
        "ja" => Ok((
            "https://alphacephei.com/vosk/models/vosk-model-small-ja-0.22.zip",
            vosk_ja_model_path(),
        )),
        "es" => Ok((
            "https://alphacephei.com/vosk/models/vosk-model-small-es-0.42.zip",
            vosk_es_model_path(),
        )),
        other => Err(format!("unknown model kind: {other}")),
    }
}

#[derive(Serialize, Clone)]
struct ModelDownloadProgress {
    kind: String,
    status: String, // "downloading" | "extracting" | "done" | "error"
    downloaded: Option<u64>,
    total: Option<u64>,
    error: Option<String>,
}

/// Recursively copies a directory tree. Used as a fallback when `rename` fails with
/// EXDEV (source and destination on different filesystems/mount points, e.g. /tmp
/// being tmpfs while the data dir is on disk).
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Downloads and extracts a Vosk model zip directly — no external tools (curl, PowerShell,
/// Python) required. Emits "vosk_download_progress" events for the settings/setup UI.
#[tauri::command]
fn download_vosk_model(app: tauri::AppHandle, kind: String) {
    std::thread::spawn(move || {
        let emit = |status: &str, downloaded: Option<u64>, total: Option<u64>, error: Option<String>| {
            let _ = app.emit(
                "vosk_download_progress",
                ModelDownloadProgress {
                    kind: kind.clone(),
                    status: status.into(),
                    downloaded,
                    total,
                    error,
                },
            );
        };

        let (url, target_dir) = match vosk_model_url_and_path(&kind) {
            Ok(v) => v,
            Err(e) => {
                emit("error", None, None, Some(e));
                return;
            }
        };

        emit("downloading", Some(0), None, None);

        let resp = match ureq::get(url).call() {
            Ok(r) => r,
            Err(e) => {
                emit("error", None, None, Some(format!("download failed: {e}")));
                return;
            }
        };
        let total = resp
            .header("Content-Length")
            .and_then(|s| s.parse::<u64>().ok());

        let tmp_dir = std::env::temp_dir().join(format!("vid_translate_dl_{kind}"));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
            emit("error", None, None, Some(format!("cannot create temp dir: {e}")));
            return;
        }
        let zip_path = tmp_dir.join("model.zip");

        let mut reader = resp.into_reader();
        let mut file = match std::fs::File::create(&zip_path) {
            Ok(f) => f,
            Err(e) => {
                emit("error", None, None, Some(format!("cannot create temp file: {e}")));
                return;
            }
        };

        let mut buf = [0u8; 65536];
        let mut downloaded: u64 = 0;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    emit("error", None, None, Some(format!("download error: {e}")));
                    return;
                }
            };
            if n == 0 {
                break;
            }
            if let Err(e) = file.write_all(&buf[..n]) {
                emit("error", None, None, Some(format!("write error: {e}")));
                return;
            }
            downloaded += n as u64;
            emit("downloading", Some(downloaded), total, None);
        }
        drop(file);

        emit("extracting", None, None, None);

        let extract_dir = tmp_dir.join("extracted");
        if let Err(e) = std::fs::create_dir_all(&extract_dir) {
            emit("error", None, None, Some(format!("cannot create extract dir: {e}")));
            return;
        }

        let zip_file = match std::fs::File::open(&zip_path) {
            Ok(f) => f,
            Err(e) => {
                emit("error", None, None, Some(format!("cannot open zip: {e}")));
                return;
            }
        };
        let mut archive = match zip::ZipArchive::new(zip_file) {
            Ok(a) => a,
            Err(e) => {
                emit("error", None, None, Some(format!("bad zip file: {e}")));
                return;
            }
        };

        for i in 0..archive.len() {
            let mut entry = match archive.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let outpath = match entry.enclosed_name() {
                Some(p) => extract_dir.join(p),
                None => continue,
            };
            if entry.is_dir() {
                let _ = std::fs::create_dir_all(&outpath);
            } else {
                if let Some(p) = outpath.parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                let outfile = match std::fs::File::create(&outpath) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let mut outfile = outfile;
                let _ = std::io::copy(&mut entry, &mut outfile);
            }
        }

        // The zip contains a single top-level folder (e.g. vosk-model-small-en-us-0.15/) —
        // find it and move it into place under the name lib.rs expects.
        let top_level = std::fs::read_dir(&extract_dir)
            .ok()
            .and_then(|d| d.filter_map(|e| e.ok()).find(|e| e.path().is_dir()))
            .map(|e| e.path());

        let top_level = match top_level {
            Some(p) => p,
            None => {
                emit("error", None, None, Some("unexpected zip layout".into()));
                return;
            }
        };

        if let Some(parent) = target_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_dir_all(&target_dir);
        // rename() fails with EXDEV when the temp dir and the target dir are on
        // different filesystems (e.g. /tmp is tmpfs on many distros). Fall back
        // to a recursive copy in that case.
        if std::fs::rename(&top_level, &target_dir).is_err() {
            if let Err(e) = copy_dir_recursive(&top_level, &target_dir) {
                emit("error", None, None, Some(format!("cannot move model into place: {e}")));
                return;
            }
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
        emit("done", None, None, None);
    });
}

/// Streams a single-shot translation from Ollama (cloud if a key is given, else local
/// http://localhost:11434). Calls `on_update` with the accumulated translation as tokens
/// arrive, checking `stop_flag` between chunks so a mid-request Stop feels instant.
/// Returns the final accumulated text (empty if stopped before anything came back).
fn translate_blocking(
    ollama_key: &Option<String>,
    ollama_model: &Option<String>,
    source_lang: &str,
    text: &str,
    stop_flag: &Arc<AtomicBool>,
    mut on_update: impl FnMut(&str),
) -> String {
    let key = ollama_key.as_ref().filter(|k| !k.is_empty());
    let base_url = if key.is_some() {
        "https://ollama.com"
    } else {
        "http://localhost:11434"
    };
    let model = ollama_model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "gemma3:27b".to_string());
    let lang_name = match source_lang {
        "ja" => "Japanese",
        "es" => "Spanish",
        other => other,
    };
    let system_prompt = format!(
        "You are a real-time speech translator. You receive short, possibly imperfect \
         {lang_name} speech-to-text fragments and must translate them into natural, fluent \
         English. Output ONLY the English translation with no notes, quotes, or explanations. \
         If the fragment is just filler noise or has nothing translatable, output nothing."
    );

    let url = format!("{base_url}/api/generate");
    let body = serde_json::json!({
        "model": model,
        "system": system_prompt,
        "prompt": text,
        "stream": true,
    });

    let mut req = ureq::post(&url).set("Content-Type", "application/json");
    if let Some(k) = key {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }

    let resp = match req.send_string(&body.to_string()) {
        Ok(r) => r,
        Err(e) => return format!("[translation error: {e}]"),
    };

    let mut reader = BufReader::new(resp.into_reader());
    let mut accumulated = String::new();
    let mut line = String::new();
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let chunk: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(piece) = chunk.get("response").and_then(|v| v.as_str()) {
            if !piece.is_empty() {
                accumulated.push_str(piece);
                on_update(&accumulated);
            }
        }
        if chunk.get("done").and_then(|v| v.as_bool()).unwrap_or(false) {
            break;
        }
    }
    accumulated
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

    let handle = std::thread::spawn(move || match mode.as_str() {
        "vosk-ja" => run_vosk_ja_pipeline(app_handle, stop_flag, ollama_key, ollama_model),
        "vosk-es" => run_vosk_es_pipeline(app_handle, stop_flag, ollama_key, ollama_model),
        _ => run_vosk_pipeline(app_handle, stop_flag),
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

fn is_japanese_text(text: &str) -> bool {
    text.chars().any(|c| {
        ('\u{3040}'..='\u{309F}').contains(&c) || // hiragana
        ('\u{30A0}'..='\u{30FF}').contains(&c) || // katakana
        ('\u{4E00}'..='\u{9FFF}').contains(&c) || // CJK unified
        ('\u{3400}'..='\u{4DBF}').contains(&c)    // CJK extension A
    })
}

/// Shared JA/ES pipeline: Vosk recognizes the source language, finalized sentences are
/// handed off to a worker thread that translates them via Ollama (native Rust HTTP calls,
/// no subprocess) so the audio thread never blocks on network I/O.
fn run_translated_pipeline(
    app_handle: tauri::AppHandle,
    stop_flag: Arc<AtomicBool>,
    ollama_key: Option<String>,
    ollama_model: Option<String>,
    model_path: std::path::PathBuf,
    source_lang: &'static str,
) {
    let _ = app_handle.emit("status", StatusEvent { state: "loading".into() });

    let (tx_text, rx_text) = std::sync::mpsc::channel::<String>();
    let app_for_translate = app_handle.clone();
    let stop_flag_worker = stop_flag.clone();

    std::thread::spawn(move || {
        for text in rx_text {
            if stop_flag_worker.load(Ordering::Relaxed) {
                break;
            }
            let app_line = app_for_translate.clone();
            let final_text = translate_blocking(
                &ollama_key,
                &ollama_model,
                source_lang,
                &text,
                &stop_flag_worker,
                |partial| {
                    let _ = app_line.emit("transcription", TranscriptionEvent {
                        text: partial.to_string(),
                        current: String::new(),
                        kind: "streaming-en".into(),
                    });
                },
            );
            if !final_text.is_empty() {
                let _ = app_line.emit("transcription", TranscriptionEvent {
                    text: final_text,
                    current: String::new(),
                    kind: "final".into(),
                });
            }
        }
    });

    let rx = audio::start_capture(stop_flag.clone());
    let _ = app_handle.emit("status", StatusEvent { state: "listening".into() });

    let app_for_partial = app_handle.clone();
    let is_ja = source_lang == "ja";
    let result = recognizer::run(
        model_path.to_str().unwrap_or(""),
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
                    if is_ja && !is_japanese_text(&text) {
                        let _ = app_for_partial.emit("transcription", TranscriptionEvent {
                            text,
                            current: String::new(),
                            kind: "final".into(),
                        });
                    } else {
                        let _ = tx_text.send(text);
                    }
                }
                _ => {}
            }
        },
    );

    if let Err(e) = result {
        eprintln!("[lib] {} recognizer error: {}", source_lang, e);
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
    run_translated_pipeline(app_handle, stop_flag, ollama_key, ollama_model, es_path, "es");
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
    run_translated_pipeline(app_handle, stop_flag, ollama_key, ollama_model, ja_path, "ja");
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

/// Calls local Ollama's pull API and streams progress as "pull_progress" events.
#[tauri::command]
fn pull_model(app: tauri::AppHandle, model: String) {
    std::thread::spawn(move || {
        let body = serde_json::json!({ "model": model, "stream": true });
        let resp = match ureq::post("http://localhost:11434/api/pull")
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
        {
            Ok(r) => r,
            Err(e) => {
                let _ = app.emit("pull_progress", PullProgressEvent {
                    status: "error".into(),
                    completed: None,
                    total: None,
                });
                eprintln!("[pull] request failed: {e}");
                return;
            }
        };

        let reader = BufReader::new(resp.into_reader());
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                let event = PullProgressEvent {
                    status: val.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                    completed: val.get("completed").and_then(|v| v.as_u64()),
                    total: val.get("total").and_then(|v| v.as_u64()),
                };
                let _ = app.emit("pull_progress", &event);
            }
        }
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
            pull_model,
            download_vosk_model,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
