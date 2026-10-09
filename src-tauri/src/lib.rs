mod audio;
pub mod chunker;
pub mod debug;
pub mod marian;
pub mod recognizer;

use serde::Serialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tauri::{Emitter, Manager};

#[derive(Serialize, Clone, Default)]
struct TranscriptionEvent {
    text: String,
    // JA mode only: the word currently being spoken, still in Japanese.
    // Empty in English mode and on final events.
    current: String,
    #[serde(rename = "type")]
    kind: String, // "partial" | "final" | "partial-chunk" | "final-chunk" | ...
    // Phase 3: identifies the in-progress line a "partial-chunk" belongs to, so the
    // frontend replaces that line in place instead of appending. Keyed off the chunker's
    // boundary index, not a text prefix, because Vosk revises JA partials (see
    // chunker::japanese). The matching "final-chunk" carries the same id, which is the
    // frontend's cue to promote the line to immutable history. 0 when not applicable.
    id: u64,
    // True when the chunk was cut by a length/time guard rather than a real clause
    // boundary — the frontend dims these to say "this may still change".
    provisional: bool,
}

impl TranscriptionEvent {
    fn new(kind: &str, text: impl Into<String>) -> Self {
        Self { text: text.into(), kind: kind.into(), ..Default::default() }
    }
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

/// Whisper replaced Vosk for Japanese: on multi-speaker audio Vosk lost about a third of
/// the speech outright (BLEU 5.03 vs 14.15 end-to-end — see docs/ja-diagnosis.md). One
/// ggml file rather than a directory, unlike the Vosk models.
///
/// Which size is a *user* choice, not a constant, because the right answer depends on the
/// machine. `small` runs ~6x realtime on an M3 and is the accuracy the Japanese numbers in
/// docs/ were measured at; on a slower x86 laptop a pass can overrun `STEP_MS`, at which
/// point the recognizer falls permanently behind the audio and latency grows without bound
/// (see `recognizer::whisper`). Dropping a size, or using the same size quantized, is the
/// only lever that moves inference cost materially — the decode is already greedy,
/// single-candidate, and threaded.
///
/// Ordered cheapest first; this is the order the picker shows.
pub const WHISPER_MODELS: &[(&str, &str)] = &[
    ("tiny-q5_1", "ggml-tiny-q5_1.bin"),
    ("tiny", "ggml-tiny.bin"),
    ("base-q5_1", "ggml-base-q5_1.bin"),
    ("base", "ggml-base.bin"),
    ("small-q5_1", "ggml-small-q5_1.bin"),
    ("small", "ggml-small.bin"),
    ("medium-q5_0", "ggml-medium-q5_0.bin"),
    ("medium", "ggml-medium.bin"),
];

/// What a machine that has not said otherwise gets: the size everything was tuned and
/// measured against.
pub const WHISPER_DEFAULT_MODEL: &str = "small";

pub const WHISPER_MODEL_REPO: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/";

/// Silero weights for the speech gate. ~865KB, and the thing that stops prominent
/// background music being captioned as "thank you for watching". Shared by every size.
pub const WHISPER_VAD_MODEL_URL: &str =
    "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin";

/// The ggml filename for a picker id, falling back to the default rather than erroring: an
/// id the frontend sends that this build does not know about is a version skew, and
/// captions at the default size beat no captions.
fn whisper_model_file(id: &str) -> &'static str {
    WHISPER_MODELS
        .iter()
        .find(|(k, _)| *k == id)
        .or_else(|| WHISPER_MODELS.iter().find(|(k, _)| *k == WHISPER_DEFAULT_MODEL))
        .map(|(_, f)| *f)
        .unwrap_or("ggml-small.bin")
}

fn whisper_model_id(id: Option<String>) -> String {
    id.filter(|s| !s.is_empty()).unwrap_or_else(|| WHISPER_DEFAULT_MODEL.to_string())
}

/// Where the models live. Sizes sit side by side, so switching back to one already fetched
/// costs no download.
fn whisper_dir() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vid_translate")
}

fn whisper_model_path(id: &str) -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("VID_TRANSLATE_WHISPER_MODEL") {
        // Still wins over the setting: it points at an arbitrary ggml file, which is how a
        // size outside the list above gets A/B'd without a build.
        return std::path::PathBuf::from(path);
    }
    whisper_dir().join(whisper_model_file(id))
}

fn vosk_es_model_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vid_translate")
        .join("vosk-model-es")
}

fn vosk_model_url_and_path(kind: &str) -> Result<(&'static str, std::path::PathBuf), String> {
    match kind {
        "en" => Ok((
            "https://alphacephei.com/vosk/models/vosk-model-small-en-us-0.15.zip",
            vosk_model_path(),
        )),
        "ja" => Ok((
            // The small model (48MB) has a high enough word-error-rate on real speech
            // (numbers, casual/fast talking) that it invents entirely different sentences —
            // no amount of chunking/translation-side tuning can recover meaning lost here.
            "https://alphacephei.com/vosk/models/vosk-model-ja-0.22.zip",
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

/// Downloads a quantized CTranslate2 translation model from Hugging Face (see
/// `marian::CT2_MODEL_REPO`) directly into place — no zip/extract step, just the fixed set
/// of files a model directory needs. Emits "ct2_download_progress" events for the setup UI,
/// same shape as the Vosk downloader.
#[tauri::command]
fn download_ct2_model(app: tauri::AppHandle, lang: String) {
    std::thread::spawn(move || {
        let emit = |status: &str, downloaded: Option<u64>, total: Option<u64>, error: Option<String>| {
            let _ = app.emit(
                "ct2_download_progress",
                ModelDownloadProgress {
                    kind: lang.clone(),
                    status: status.into(),
                    downloaded,
                    total,
                    error,
                },
            );
        };

        if lang != "ja" && lang != "es" {
            emit("error", None, None, Some(format!("unknown local model language: {lang}")));
            return;
        }

        emit("downloading", Some(0), None, None);

        // Fetch sizes upfront (cheap HEAD requests) so the progress bar has a real total
        // across all 5 files instead of resetting per-file.
        let mut file_sizes = Vec::with_capacity(marian::CT2_MODEL_FILES.len());
        for filename in marian::CT2_MODEL_FILES {
            let url = format!(
                "https://huggingface.co/{}/resolve/main/ct2-model-{lang}/{filename}",
                marian::CT2_MODEL_REPO
            );
            let size = match ureq::head(&url).call() {
                Ok(r) => r.header("Content-Length").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0),
                Err(e) => {
                    emit("error", None, None, Some(format!("could not reach model host for {filename}: {e}")));
                    return;
                }
            };
            file_sizes.push(size);
        }
        let total: u64 = file_sizes.iter().sum();

        let tmp_dir = std::env::temp_dir().join(format!("vid_translate_ct2_dl_{lang}"));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
            emit("error", None, None, Some(format!("cannot create temp dir: {e}")));
            return;
        }

        let mut downloaded: u64 = 0;
        for filename in marian::CT2_MODEL_FILES {
            let url = format!(
                "https://huggingface.co/{}/resolve/main/ct2-model-{lang}/{filename}",
                marian::CT2_MODEL_REPO
            );
            let resp = match ureq::get(&url).call() {
                Ok(r) => r,
                Err(e) => {
                    emit("error", None, None, Some(format!("download failed for {filename}: {e}")));
                    return;
                }
            };
            let mut reader = resp.into_reader();
            let mut file = match std::fs::File::create(tmp_dir.join(filename)) {
                Ok(f) => f,
                Err(e) => {
                    emit("error", None, None, Some(format!("cannot create temp file: {e}")));
                    return;
                }
            };
            let mut buf = [0u8; 65536];
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
                emit("downloading", Some(downloaded), Some(total), None);
            }
        }

        let target_dir = marian::model_path(&lang);
        if let Some(parent) = target_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_dir_all(&target_dir);
        if std::fs::rename(&tmp_dir, &target_dir).is_err() {
            if let Err(e) = copy_dir_recursive(&tmp_dir, &target_dir) {
                emit("error", None, None, Some(format!("cannot move model into place: {e}")));
                return;
            }
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }

        emit("done", None, None, None);
    });
}

/// Whether a local CTranslate2 model has already been downloaded, so the frontend can show
/// a download prompt instead of finding out mid-translation.
#[tauri::command]
fn local_model_exists(lang: String) -> bool {
    marian::is_model_downloaded(&lang)
}

#[tauri::command]
fn whisper_model_exists(model: Option<String>) -> bool {
    whisper_files(&whisper_model_id(model)).iter().all(|(_, p)| p.exists())
}

/// Loads the Whisper model into memory ahead of time, off the UI thread.
///
/// The frontend fires this when Japanese mode is selected, so the load has already happened
/// — or is already underway — by the time the user presses Start. It is the one part of
/// startup they would otherwise sit through with an empty caption bar, and it is wasted
/// waiting: nothing about it depends on the session having begun.
///
/// Safe to call repeatedly; after the first call it returns immediately.
#[tauri::command]
fn warm_whisper_model(model: Option<String>) {
    let id = whisper_model_id(model);
    std::thread::spawn(move || {
        let path = whisper_model_path(&id);
        if !path.exists() {
            return;
        }
        if let Err(e) = recognizer::whisper::preload(&path.to_string_lossy()) {
            // Not surfaced to the user: this is an optimisation, and if it failed the
            // session start will fail the same way with a message that has context.
            eprintln!("[whisper] preload failed: {e}");
        }
    });
}

/// The files the Japanese recognizer needs: the model itself, and the Silero weights for
/// the speech gate that stops Whisper captioning background music.
fn whisper_files(id: &str) -> Vec<(String, std::path::PathBuf)> {
    let model = whisper_model_path(id);
    let dir = model
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    vec![
        (format!("{WHISPER_MODEL_REPO}{}", whisper_model_file(id)), model),
        (WHISPER_VAD_MODEL_URL.to_string(), dir.join(recognizer::whisper::VAD_MODEL_FILE)),
    ]
}

/// Downloads the chosen Whisper model and the VAD weights. No archives to unpack, but two
/// files of very different sizes (32MB to 1.5GB for the model, ~865KB for the weights), so
/// the progress events are reported against their combined total rather than resetting per
/// file.
#[tauri::command]
fn download_whisper_model(app: tauri::AppHandle, model: Option<String>) {
    let id = whisper_model_id(model);
    std::thread::spawn(move || {
        let emit = |status: &str, downloaded: Option<u64>, total: Option<u64>, error: Option<String>| {
            let _ = app.emit(
                "whisper_download_progress",
                ModelDownloadProgress {
                    // Language-neutral: the same ggml file and the same VAD weights serve
                    // Japanese and Spanish, so the setup card for either listens to this.
                    kind: "whisper".into(),
                    status: status.into(),
                    downloaded,
                    total,
                    error,
                },
            );
        };

        let wanted: Vec<_> = whisper_files(&id).into_iter().filter(|(_, p)| !p.exists()).collect();
        if wanted.is_empty() {
            emit("done", None, None, None);
            return;
        }

        emit("downloading", Some(0), None, None);

        // Sizes up front, so the bar runs once across both files instead of twice.
        let mut total: u64 = 0;
        for (url, _) in &wanted {
            if let Ok(r) = ureq::head(url).call() {
                total += r
                    .header("Content-Length")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
            }
        }
        let total = (total > 0).then_some(total);

        let mut downloaded: u64 = 0;
        for (url, dest) in &wanted {
            if let Some(parent) = dest.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    emit("error", None, None, Some(format!("cannot create model dir: {e}")));
                    return;
                }
            }

            let resp = match ureq::get(url).call() {
                Ok(r) => r,
                Err(e) => {
                    emit("error", None, None, Some(format!("download failed: {e}")));
                    return;
                }
            };

            // Download to a temporary name and rename on success, so an interrupted
            // download cannot leave a half-written file that `whisper_model_exists` then
            // reports as present and the recognizer fails to load mid-session.
            let tmp = dest.with_extension("part");
            let mut file = match std::fs::File::create(&tmp) {
                Ok(f) => f,
                Err(e) => {
                    emit("error", None, None, Some(format!("cannot create {}: {e}", tmp.display())));
                    return;
                }
            };

            let mut reader = resp.into_reader();
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Err(e) = file.write_all(&buf[..n]) {
                            emit("error", None, None, Some(format!("write failed: {e}")));
                            let _ = std::fs::remove_file(&tmp);
                            return;
                        }
                        downloaded += n as u64;
                        emit("downloading", Some(downloaded), total, None);
                    }
                    Err(e) => {
                        emit("error", None, None, Some(format!("read failed: {e}")));
                        let _ = std::fs::remove_file(&tmp);
                        return;
                    }
                }
            }
            drop(file);

            if let Err(e) = std::fs::rename(&tmp, dest) {
                emit("error", None, None, Some(format!("could not finalise {}: {e}", dest.display())));
                let _ = std::fs::remove_file(&tmp);
                return;
            }
        }
        emit("done", Some(downloaded), total, None);
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
    use_local_translation: Option<bool>,
    prefer_microphone: Option<bool>,
    whisper_model: Option<String>,
) {
    // macOS only (a no-op elsewhere): set before the preflight below, since opting into
    // microphone capture is precisely what makes an unavailable system-audio tap acceptable.
    audio::set_prefer_microphone(prefer_microphone.unwrap_or(false));

    let mut pipeline = state.lock().unwrap();

    pipeline.stop_flag.store(true, Ordering::Relaxed);
    if let Some(h) = pipeline.thread.take() {
        drop(h);
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    pipeline.stop_flag = stop_flag.clone();

    let app_handle = app.clone();
    let mode = mode.unwrap_or_else(|| "vosk".into());
    let use_local = use_local_translation.unwrap_or(false);
    let whisper_model = whisper_model_id(whisper_model);

    let handle = std::thread::spawn(move || {
        // Refuse up front what is knowable up front (on macOS, the OS version), so an
        // unsupported machine gets a setup screen rather than a session that "runs" and
        // transcribes pure silence. Permission is deliberately *not* checked here: the tap
        // API reports success even when denied, so that verdict can only come from the
        // watchdog once audio should have been flowing.
        if let Err(fault) = audio::preflight() {
            let _ = app_handle.emit("status", StatusEvent { state: fault.status().into() });
            return;
        }
        match mode.as_str() {
            "vosk-ja" => run_vosk_ja_pipeline(
                app_handle,
                stop_flag,
                ollama_key,
                ollama_model,
                use_local,
                whisper_model,
            ),
            "vosk-es" => run_vosk_es_pipeline(
                app_handle,
                stop_flag,
                ollama_key,
                ollama_model,
                use_local,
                whisper_model,
            ),
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

    let _ = app_handle.emit("status", StatusEvent { state: "loading_model".into() });
    let rx = audio::start_capture(stop_flag.clone());
    spawn_capture_watchdog(app_handle.clone(), stop_flag.clone());
    let app_for_result = app_handle.clone();

    let app_for_ready = app_handle.clone();
    let result = recognizer::run(
        recognizer::Backend::Vosk,
        vosk_path.to_str().unwrap_or(""),
        recognizer::Pacing::Live,
        rx,
        move || {
            let _ = app_for_ready.emit("status", StatusEvent { state: "listening".into() });
        },
        move |result| {
            use recognizer::RecognitionResult::*;
            match result {
                Partial(text) => {
                    let _ = app_for_result.emit("transcription", TranscriptionEvent {
                        text,
    current: String::new(),
    kind: "partial".into(),
    ..Default::default()
                    });
                }
                Final(text) => {
                    let _ = app_for_result.emit("transcription", TranscriptionEvent {
                        text,
                        current: String::new(),
                        kind: "final".into(),
                        ..Default::default()
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
    use_local: bool,
    backend: recognizer::Backend,
) {
    let _ = app_handle.emit("status", StatusEvent { state: "loading".into() });

    // One unit of work for the translator thread.
    struct TranslateJob {
        text: String,
        /// True when this is the true tail end of a spoken utterance (a real Vosk `Final`)
        /// rather than an eagerly-flushed mid-utterance chunk. The frontend needs to tell
        /// these apart: a mid-utterance chunk shouldn't reset the live "currently speaking"
        /// caption, only a real utterance end should.
        is_utterance_end: bool,
        /// The chunker's boundary index this chunk closed. The in-progress live line
        /// carries the *next* index, so the `final-chunk` that lands here promotes exactly
        /// the line the user was watching. Keyed off the index and not a text prefix
        /// because Vosk revises JA partials.
        id: u64,
        /// The chunk was cut by a length/time guard, not a real clause boundary.
        provisional: bool,
    }

    let (tx_text, rx_text) = std::sync::mpsc::channel::<TranslateJob>();
    let app_for_translate = app_handle.clone();
    let stop_flag_worker = stop_flag.clone();

    // Buffering/merging fragments together before translating (predicate-suffix waiting,
    // debounce timeouts, etc.) was tried here and reverted: it sits upstream of the
    // use_local branch below, so it silently fed both Ollama and the local model mangled,
    // merged-together input — degrading a JA/Ollama pipeline that worked fine before any of
    // that was added (see git history at 60432d2). Each job received here is translated
    // immediately and independently, same as the original design: one chunk in, one
    // translate call out. Where the chunk *boundaries* go is `crate::chunker`'s job, and
    // only its job.
    std::thread::spawn(move || {
        for job in rx_text {
            if stop_flag_worker.load(Ordering::Relaxed) {
                break;
            }
            let TranslateJob { text, is_utterance_end, id, provisional } = job;
            if text.is_empty() {
                // Nothing left to translate — every word of this utterance was already
                // eagerly translated chunk-by-chunk. Still signal the end so the frontend
                // clears the live caption instead of leaving the last chunk stuck on screen.
                if is_utterance_end {
                    let _ = app_for_translate
                        .emit("transcription", TranscriptionEvent::new("utterance-end", ""));
                }
                continue;
            }
            let app_line = app_for_translate.clone();
            let stop_flag_line = stop_flag_worker.clone();
            let ollama_key = ollama_key.clone();
            let ollama_model = ollama_model.clone();
            let text_for_panic_msg = text.clone();

            // A panic inside translate (e.g. a tch/libtorch-level error) must not silently
            // kill this thread for the rest of the session — everything sent to tx_text
            // afterward would otherwise be dropped with zero user-visible feedback.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let on_update = |partial: &str| {
                    // JA suppresses the paced word-by-word reveal: its live line is the
                    // re-translated in-progress tail (`partial-chunk`), and feeding the
                    // paced queue at the same time would make the two fight over one line.
                    if source_lang == "ja" {
                        return;
                    }
                    let _ = app_line
                        .emit("transcription", TranscriptionEvent::new("streaming-en", partial));
                };
                let final_text = if use_local {
                    let marian_state = app_line.state::<marian::MarianState>();
                    marian::translate_local_blocking(
                        source_lang,
                        &text,
                        &stop_flag_line,
                        &marian_state,
                        on_update,
                    )
                } else {
                    translate_blocking(
                        &ollama_key,
                        &ollama_model,
                        source_lang,
                        &text,
                        &stop_flag_line,
                        on_update,
                    )
                };
                if !final_text.is_empty() {
                    let _ = app_line.emit(
                        "transcription",
                        TranscriptionEvent {
                            text: final_text,
                            current: String::new(),
                            kind: if is_utterance_end {
                                "final".into()
                            } else {
                                "final-chunk".into()
                            },
                            id,
                            provisional,
                        },
                    );
                }
            }));

            if let Err(e) = outcome {
                let msg = e
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".into());
                eprintln!("[translate] worker panicked on {text_for_panic_msg:?}: {msg}");
                let _ = app_for_translate.emit(
                    "transcription",
                    TranscriptionEvent::new(
                        "final",
                        format!("[translation error: internal panic: {msg}]"),
                    ),
                );
            }
        }
    });

    // Phase 3: re-translate the in-progress tail instead of locking in an append-only
    // guess. The old pipeline committed an English line the moment a chunk was emitted; a
    // Japanese clause that has not reached its predicate can only be guessed at, so the
    // guess has to be replaceable. This thread owns that one live line: it re-translates
    // the whole current tail from its start (not just the new characters) and emits
    // `partial-chunk { id, text }`, which the frontend renders in place. When the chunker
    // finds a real boundary the ordinary `final-chunk` arrives with the same id and the
    // frontend promotes the line to immutable history.
    //
    // Only wired for local JA: a ≤60-char tail through CT2 costs tens of milliseconds, so
    // one translation per 300ms is affordable, whereas against Ollama it would be an HTTP
    // round trip per partial. If CPU load ever becomes a problem, raise TAIL_DEBOUNCE_MS
    // before changing anything else.
    const TAIL_DEBOUNCE_MS: u64 = 300;
    let tx_tail = if use_local && source_lang == "ja" {
        let (tx, rx) = std::sync::mpsc::channel::<(u64, String)>();
        let app_tail = app_handle.clone();
        let stop_flag_tail = stop_flag.clone();
        std::thread::spawn(move || {
            while let Ok(mut latest) = rx.recv() {
                // Coalesce: while the debounce window is open, keep only the newest tail.
                // Partials arrive every ~250ms and each supersedes the last, so translating
                // every one of them would be pure waste.
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(TAIL_DEBOUNCE_MS);
                loop {
                    let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now())
                    else {
                        break;
                    };
                    match rx.recv_timeout(remaining) {
                        Ok(newer) => latest = newer,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                if stop_flag_tail.load(Ordering::Relaxed) {
                    return;
                }
                let (id, tail) = latest;
                let marian_state = app_tail.state::<marian::MarianState>();
                let english = marian::translate_local_blocking(
                    source_lang,
                    &tail,
                    &stop_flag_tail,
                    &marian_state,
                    |_| {},
                );
                if english.is_empty() || stop_flag_tail.load(Ordering::Relaxed) {
                    continue;
                }
                let _ = app_tail.emit(
                    "transcription",
                    TranscriptionEvent {
                        text: english,
                        current: String::new(),
                        kind: "partial-chunk".into(),
                        id,
                        // The tail is by definition an unfinished clause.
                        provisional: true,
                    },
                );
            }
        });
        Some(tx)
    } else {
        None
    };

    let rx = audio::start_capture(stop_flag.clone());
    spawn_capture_watchdog(app_handle.clone(), stop_flag.clone());
    // Deliberately NOT "listening" yet: the model load happens inside recognizer::run
    // below, it is the slowest part of starting a session, and audio is already queueing
    // behind it. Saying "listening" here claimed the app was working while it was blocked.
    let _ = app_handle.emit("status", StatusEvent { state: "loading_model".into() });

    let app_for_ready = app_handle.clone();
    let app_for_partial = app_handle.clone();
    let is_ja = source_lang == "ja";
    // Vosk only fires `Final` once it detects a pause, so a long sentence spoken in one
    // breath would otherwise sit untranslated until the speaker stops. Both languages
    // therefore watch the growing `Partial` and hand off finished pieces early — but where
    // a piece *finishes* is language-specific, and the two rules cut in opposite
    // directions. All of that now lives in `crate::chunker`; see its module docs.
    let mut chunker = chunker::for_language(source_lang, use_local);
    // Monotonic boundary counter. The in-progress live line is always `boundary_id + 1`.
    let mut boundary_id: u64 = 0;
    // Whisper's native translate task hands us English directly, which makes the chunker
    // and the translation model dead weight for Japanese.
    let native_en = backend.emits_english();
    let result = recognizer::run(
        backend,
        model_path.to_str().unwrap_or(""),
        recognizer::Pacing::Live,
        rx,
        move || {
            let _ = app_for_ready.emit("status", StatusEvent { state: "listening".into() });
        },
        move |ev| {
            use recognizer::RecognitionResult::*;

            // Native-translate path: the recognizer already produced English, so there is
            // nothing to chunk and nothing to translate. Text goes straight to the screen.
            //
            // Measured +6.67 BLEU / +4.76 chrF over transcribe-then-translate on the
            // 36-minute multi-speaker clip (19.88 / 57.82 vs 13.21 / 53.06). The whole
            // chunker and the JA half of `marian` are bypassed here — see the deletion
            // notes on `crate::chunker`.
            if native_en {
                match ev {
                    Partial(text) if !text.trim().is_empty() => {
                        debug::log_asr(source_lang, "partial-en", &text);
                        // The in-progress window, as the live (provisional) line.
                        let _ = app_for_partial.emit(
                            "transcription",
                            TranscriptionEvent {
                                text,
                                current: String::new(),
                                kind: "partial-chunk".into(),
                                id: boundary_id + 1,
                                provisional: true,
                            },
                        );
                    }
                    Final(text) if !text.trim().is_empty() => {
                        debug::log_asr(source_lang, "final-en", &text);
                        boundary_id += 1;
                        // Same id as the live line above, which is the frontend's cue to
                        // promote that line to history instead of leaving a duplicate.
                        let _ = app_for_partial.emit(
                            "transcription",
                            TranscriptionEvent {
                                text,
                                current: String::new(),
                                kind: "final-chunk".into(),
                                id: boundary_id,
                                provisional: false,
                            },
                        );
                    }
                    _ => {}
                }
                return;
            }

            match ev {
                Partial(text) => {
                    debug::log_asr(source_lang, "partial", &text);
                    for chunk in chunker.push_partial(&text) {
                        boundary_id += 1;
                        debug::log_chunk(source_lang, &chunk.text, chunk.boundary_confident);
                        let _ = tx_text.send(TranslateJob {
                            text: chunk.text,
                            is_utterance_end: false,
                            id: boundary_id,
                            provisional: !chunk.boundary_confident,
                        });
                    }
                    if let Some(tx_tail) = &tx_tail {
                        let tail = chunker.pending_tail();
                        if !tail.is_empty() {
                            let _ = tx_tail.send((boundary_id + 1, tail));
                        }
                    }
                    let _ = app_for_partial
                        .emit("transcription", TranscriptionEvent::new("partial", text));
                }
                Final(text) if !text.is_empty() => {
                    debug::log_asr(source_lang, "final", &text);
                    let chunks = chunker.flush(&text);
                    for chunk in &chunks {
                        debug::log_chunk(source_lang, &chunk.text, chunk.boundary_confident);
                    }
                    if chunks.is_empty() {
                        // Everything was already sent eagerly — still flag utterance end.
                        let _ = tx_text.send(TranslateJob {
                            text: String::new(),
                            is_utterance_end: true,
                            id: boundary_id,
                            provisional: false,
                        });
                    } else if is_ja && !is_japanese_text(&text) {
                        // JA mode picked up speech that isn't Japanese at all — show the
                        // recognizer's own text rather than running it through a ja→en model.
                        let joined: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
                        let _ = app_for_partial.emit(
                            "transcription",
                            TranscriptionEvent::new("final", joined.join(" ")),
                        );
                    } else {
                        let last = chunks.len() - 1;
                        for (i, chunk) in chunks.into_iter().enumerate() {
                            boundary_id += 1;
                            let _ = tx_text.send(TranslateJob {
                                text: chunk.text,
                                is_utterance_end: i == last,
                                id: boundary_id,
                                provisional: !chunk.boundary_confident,
                            });
                        }
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
    use_local: bool,
    whisper_model: String,
) {
    // Spanish recognises with Whisper, but *transcribes* rather than translating — the
    // opposite of the Japanese decision above, and measured on a 9.6-minute Spanish clip
    // against human subtitles:
    //
    //   Vosk -> Marian         30.68 BLEU / 62.88 chrF   (the path this replaces)
    //   Whisper native EN      33.56 BLEU / 65.15 chrF
    //   Whisper -> Marian      39.60 BLEU / 67.24 chrF
    //
    // Swapping only the recognizer and leaving the MT stage alone is worth +8.92 BLEU, and
    // the es->en Marian model beats Whisper's own translate task by a further +6.04. That
    // inverts the Japanese result for a reason: Spanish and English are close and
    // `es->en` is a strong, high-resource model, so nothing is lost handing it text —
    // whereas Japanese loses meaning in that same handoff.
    //
    // Keeping the MT stage also keeps the Spanish caption line. One Whisper pass yields
    // either the source text or English, never both, and this mode's whole layout is
    // source above translation.
    //
    // The clip was a single clear speaker, which is Vosk's best case (see eval/ja/README.md
    // on why that matters), so +8.92 is likely a floor rather than the typical gap.
    let es_path = whisper_model_path(&whisper_model);
    if !es_path.exists() {
        let _ = app_handle.emit("status", StatusEvent { state: "whisper_es_model_missing".into() });
        return;
    }
    // Unlike the Japanese path, this one always needs the translation model: the recognizer
    // hands over Spanish, not English.
    if use_local && !marian::is_model_downloaded("es") {
        let _ = app_handle.emit("status", StatusEvent { state: "ct2_es_model_missing".into() });
        return;
    }
    run_translated_pipeline(
        app_handle,
        stop_flag,
        ollama_key,
        ollama_model,
        es_path,
        "es",
        use_local,
        recognizer::Backend::Whisper { lang: "es", translate: false },
    );
}

fn run_vosk_ja_pipeline(
    app_handle: tauri::AppHandle,
    stop_flag: Arc<AtomicBool>,
    ollama_key: Option<String>,
    ollama_model: Option<String>,
    use_local: bool,
    whisper_model: String,
) {
    // Japanese recognises with Whisper, not Vosk. Vosk remains the recognizer for Spanish
    // and English, where it performs acceptably and its sub-second streaming is worth more
    // than the accuracy difference.
    let ja_path = whisper_model_path(&whisper_model);
    if !ja_path.exists() {
        let _ = app_handle.emit("status", StatusEvent { state: "whisper_ja_model_missing".into() });
        return;
    }
    // Whisper's own translate task, rather than transcribing to Japanese and handing that
    // to the Marian model. Measured +6.67 BLEU / +4.76 chrF on the 36-minute multi-speaker
    // clip while deleting a stage and a 240MB model — it wins because it heard the audio,
    // where the two-stage path loses meaning in the handoff through Japanese text.
    //
    // VID_TRANSLATE_JA_TWO_STAGE=1 restores the old transcribe-then-translate path, for
    // comparing the two by hand.
    let two_stage = std::env::var("VID_TRANSLATE_JA_TWO_STAGE").is_ok_and(|v| v == "1");
    // Only the two-stage path needs the Japanese translation model. Demanding it on the
    // native path would block a session on a 240MB download it never reads.
    if two_stage && use_local && !marian::is_model_downloaded("ja") {
        let _ = app_handle.emit("status", StatusEvent { state: "ct2_ja_model_missing".into() });
        return;
    }
    run_translated_pipeline(
        app_handle,
        stop_flag,
        ollama_key,
        ollama_model,
        ja_path,
        "ja",
        use_local,
        recognizer::Backend::Whisper { lang: "ja", translate: !two_stage },
    );
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

/// Watches for a capture fault and turns it into a setup screen.
///
/// Polled rather than pushed because the capture thread has no `AppHandle`, and because
/// every fault is terminal for the session: a tap that was denied permission will never
/// start working on its own. Stops the pipeline so the UI is not left showing "listening"
/// over a dead stream.
fn spawn_capture_watchdog(app: tauri::AppHandle, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Some(fault) = audio::capture_fault() {
                eprintln!("[audio] capture fault: {fault:?}");
                let _ = app.emit("status", StatusEvent { state: fault.status().into() });
                stop.store(true, Ordering::Relaxed);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
}

/// Opens System Settings at the pane holding the system-audio recording switch.
///
/// macOS ties this permission to the app's code signature, and release builds are ad-hoc
/// signed, so the grant is lost on every update while the stale entry still reads as
/// enabled — which makes "take me to the switch" a routine action rather than an edge case.
#[tauri::command]
fn open_audio_privacy_settings() {
    let url = "x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture";
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        eprintln!("[window] could not open privacy settings: {e}");
    }
}

/// Opts the widget into Mission Control and Spaces despite being an always-on-top window.
///
/// `alwaysOnTop` makes tao set `NSFloatingWindowLevel`, and AppKit's documented default for
/// any window above `NSNormalWindowLevel` is `NSWindowCollectionBehaviorTransient` — which
/// means "floats across Spaces, hides in Exposé". That default is why the widget was absent
/// from Mission Control, could not be sent to another desktop, and was awkward to find
/// again after Cmd-Tab.
///
/// The three behaviours in that group are mutually exclusive and only a *default* when none
/// is set, so asking for `Managed` explicitly ("participates in Spaces and Exposé") keeps
/// the floating level while restoring normal window management. `FullScreenAuxiliary` lets
/// it accompany a full-screen video rather than being left behind on the desktop Space,
/// which is the case it exists for.
#[cfg(target_os = "macos")]
fn make_window_mission_control_visible(window: &tauri::WebviewWindow) -> Result<(), String> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let ns_window = window.ns_window().map_err(|e| e.to_string())? as *mut AnyObject;
    if ns_window.is_null() {
        return Err("ns_window was null".into());
    }
    // Managed = 1 << 2, FullScreenAuxiliary = 1 << 8 (NSWindowCollectionBehavior).
    const MANAGED: usize = 1 << 2;
    const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
    unsafe {
        let _: () = msg_send![ns_window, setCollectionBehavior: MANAGED | FULL_SCREEN_AUXILIARY];
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Cosmetic-only: a failure here leaves the widget working, just missing from
            // Mission Control, so it is logged rather than aborting startup.
            #[cfg(target_os = "macos")]
            if let Some(window) = app.get_webview_window("main") {
                if let Err(e) = make_window_mission_control_visible(&window) {
                    eprintln!("[window] could not set collection behavior: {e}");
                }
            }
            let _ = app;
            Ok(())
        })
        .manage(Mutex::new(PipelineState::default()))
        .manage(marian::MarianState::default())
        .invoke_handler(tauri::generate_handler![
            start_listening,
            stop_listening,
            pull_model,
            download_vosk_model,
            download_ct2_model,
            download_whisper_model,
            local_model_exists,
            whisper_model_exists,
            warm_whisper_model,
            open_audio_privacy_settings,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // Cached Whisper models hold Metal resources, and ggml's global destructor
            // asserts that they have all been released by the time it runs. Dropping them
            // here rather than leaving it to process teardown is what stops a clean quit
            // from aborting — see recognizer::whisper::unload_all.
            if matches!(event, tauri::RunEvent::Exit) {
                recognizer::whisper::unload_all();
            }
        });
}
