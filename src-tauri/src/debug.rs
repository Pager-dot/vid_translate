//! Phase 0 diagnostics: raw ASR and MT input/output dumps, behind an env var.
//!
//! Enabled with `VID_TRANSLATE_DEBUG_ASR=1`. Writes newline-delimited JSON to
//! `<data_local>/vid_translate/debug/`:
//!
//!   asr-<lang>.jsonl  {"t_ms":12840,"kind":"partial","text":"..."}
//!   mt-<lang>.jsonl   {"t_ms":12840,"src":"...","tgt":"...","ms":41}
//!
//! The ASR text is logged *exactly* as Vosk emits it — no trimming, no de-spacing, no
//! normalisation — because the whole point is to find out what Vosk actually produces
//! (spaces between morphemes? punctuation? monotonically growing partials?).
//!
//! Off by default and cheap when off: one atomic-ish `OnceLock` read per call.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

static ENABLED: OnceLock<bool> = OnceLock::new();
static CLOCK: OnceLock<Instant> = OnceLock::new();

/// True when `VID_TRANSLATE_DEBUG_ASR` is set to anything other than "0"/"".
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| match std::env::var("VID_TRANSLATE_DEBUG_ASR") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    })
}

/// Milliseconds since the first logged event of this process — a session-relative clock, so
/// two runs of the same clip line up when diffed.
fn t_ms() -> u128 {
    CLOCK.get_or_init(Instant::now).elapsed().as_millis()
}

fn debug_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("vid_translate")
        .join("debug")
}

fn append(file: &str, line: &str) {
    let dir = debug_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(dir.join(file)) {
        let _ = writeln!(f, "{line}");
    }
}

/// One Vosk callback. `kind` is "partial" | "final".
pub fn log_asr(lang: &str, kind: &str, text: &str) {
    if !enabled() {
        return;
    }
    let line = serde_json::json!({ "t_ms": t_ms(), "kind": kind, "text": text });
    append(&format!("asr-{lang}.jsonl"), &line.to_string());
}

/// One translate call: the exact string handed to the model and what came back.
pub fn log_mt(lang: &str, src: &str, tgt: &str, ms: u128) {
    if !enabled() {
        return;
    }
    let line = serde_json::json!({ "t_ms": t_ms(), "src": src, "tgt": tgt, "ms": ms });
    append(&format!("mt-{lang}.jsonl"), &line.to_string());
}

/// One emitted chunk, for the Phase 1.3 before/after refactor diff. Separate file so it can
/// be diffed on its own without MT latency noise.
pub fn log_chunk(lang: &str, text: &str, boundary_confident: bool) {
    if !enabled() {
        return;
    }
    let line = serde_json::json!({ "text": text, "boundary_confident": boundary_confident });
    append(&format!("chunks-{lang}.jsonl"), &line.to_string());
}
