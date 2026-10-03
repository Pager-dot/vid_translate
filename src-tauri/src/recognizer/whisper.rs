//! Whisper, made to look streaming.
//!
//! Whisper transcribes a *finished* window of audio; it has no notion of a growing partial.
//! This module manufactures the streaming contract the rest of the pipeline expects by
//! holding a window of recent audio, re-transcribing all of it every `STEP_MS`, and
//! deciding which of the segments that come back are finished:
//!
//! * a segment with enough audio after it has stopped changing — the speaker moved on — so
//!   it is emitted as `Final` and its audio is dropped from the window;
//! * whatever is left is emitted as `Partial`, and will be re-transcribed (and possibly
//!   revised) on the next pass.
//!
//! Re-transcribing the window is not as wasteful as it sounds: Whisper's encoder always runs
//! on a zero-padded 30-second window whatever you feed it, so a pass over 3 seconds of audio
//! costs about the same as a pass over 15 (measured: ~400ms encode either way). The cost is
//! therefore per *pass*, not per second of audio, which is why the step size sets both the
//! latency and the CPU load and why there is nothing to gain from a smaller window.
//!
//! ## `set_no_context(true)` is not optional
//!
//! Whisper's default behaviour feeds the previous window's text back in as context, and on
//! long audio that sends it into repetition loops. Measured on a 36-minute clip: runs of 260
//! identical segments, 62% of all segments duplicated, and end-to-end BLEU down from 14.15
//! to 10.02. With context disabled: 5.3% duplicates. A caption bar that chants one sentence
//! for four minutes is the failure mode this one line prevents.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, OnceLock};

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::RecognitionResult;

/// How much new audio to accumulate between passes. This is the dominant term in
/// user-visible latency: a word spoken just after a pass waits almost a full step before
/// the next one sees it. 2s against ~0.5s of inference also keeps the duty cycle near 25%
/// of one core, which leaves room for the translator threads.
const STEP_MS: usize = 2000;

/// Whisper needs roughly a second of audio to say anything useful; below this a pass is
/// wasted work that tends to return nothing or a hallucinated fragment.
const MIN_INFER_MS: usize = 1000;

/// Never hold more than this. Reached only when someone talks continuously without the
/// trailing-silence rule ever firing; everything is then committed so the window cannot
/// grow without bound (and so inference cost stays flat).
const MAX_WINDOW_MS: usize = 20_000;

/// A segment is treated as finished once this much audio follows it. Whisper's own segment
/// end timestamps are what make this possible without a separate VAD.
const TRAIL_SILENCE_MS: i64 = 700;

/// Segments Whisper itself thinks are probably not speech are dropped. Whisper is
/// notorious for producing confident text over silence, music and room noise.
const NO_SPEECH_THRESHOLD: f32 = 0.6;

const SAMPLE_RATE: usize = 16_000;

/// How far back to look for text already emitted, when stripping an overlap.
const MAX_OVERLAP_CHARS: usize = 16;

/// Removes from `text` any leading run that repeats the end of `already_emitted`.
///
/// Trimming the audio window by Whisper's segment timestamps is not enough on its own:
/// the timestamps run slightly short of the audio they describe, and each pass re-segments
/// the window from scratch, so the same syllables routinely come back at the start of the
/// next pass. Left alone that put `暗くならないうちに` on screen followed by
/// `うちにあれに乗りたいと思います`. Comparing the text is the only check that actually
/// holds, because it does not depend on the timestamps being right.
fn strip_overlap(already_emitted: &str, text: &str) -> String {
    let prev: Vec<char> = already_emitted.chars().collect();
    let next: Vec<char> = text.chars().collect();
    let limit = MAX_OVERLAP_CHARS.min(prev.len()).min(next.len());
    // Longest first, so `うちに` wins over `う`.
    for len in (1..=limit).rev() {
        if prev[prev.len() - len..] == next[..len] {
            return trim_lead_punct(&next[len..].iter().collect::<String>());
        }
    }
    text.to_string()
}

/// Removing an overlap can leave the remainder starting on punctuation that belonged to the
/// part already shown — `飛んでいますね` committed, `、というか…` left over.
fn trim_lead_punct(text: &str) -> String {
    text.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '、' | '。' | '，' | '．' | ',' | '.' | '・' | '!' | '?' | '！' | '？')
    })
    .to_string()
}

fn ms_to_samples(ms: usize) -> usize {
    ms * SAMPLE_RATE / 1000
}

/// Whisper reports segment times in centiseconds.
fn cs_to_samples(cs: i64) -> usize {
    (cs.max(0) as usize) * SAMPLE_RATE / 100
}

/// Keeps only the last `MAX_OVERLAP_CHARS` — all `strip_overlap` ever looks at.
fn remember_tail(tail: &mut String, emitted: &str) {
    tail.push_str(emitted);
    let n = tail.chars().count();
    if n > MAX_OVERLAP_CHARS {
        *tail = tail.chars().skip(n - MAX_OVERLAP_CHARS).collect();
    }
}

/// Loaded models, kept for the life of the process and keyed by path.
///
/// Without this, every Start/Stop toggle re-read 487MB from disk and re-initialised the GPU
/// pipeline. `MarianState` already caches the translation models across toggles for exactly
/// this reason; the recognizer had no equivalent because the Vosk model it replaced was
/// 48MB and the cost did not show.
static LOADED: OnceLock<Mutex<HashMap<String, Arc<WhisperContext>>>> = OnceLock::new();

/// Returns the model for `path`, loading it only the first time.
///
/// Also callable before a session starts, to get the load out of the way while the user is
/// still choosing settings — the load is the one part of startup they sit through with
/// nothing on screen.
pub fn preload(path: &str) -> Result<Arc<WhisperContext>, String> {
    let cache = LOADED.get_or_init(|| Mutex::new(HashMap::new()));
    // Held across the load on purpose: two threads racing to load the same 487MB model
    // would double the memory and the wait.
    let mut guard = cache.lock().map_err(|e| format!("whisper cache poisoned: {e}"))?;
    if let Some(ctx) = guard.get(path) {
        return Ok(ctx.clone());
    }
    let load_start = std::time::Instant::now();
    let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
        .map_err(|e| format!("failed to load Whisper model at {path}: {e}"))?;
    eprintln!("[whisper] model loaded in {:.1?}", load_start.elapsed());
    let ctx = Arc::new(ctx);
    guard.insert(path.to_string(), ctx.clone());
    Ok(ctx)
}

/// Releases every cached model.
///
/// **Must be called before the process exits.** Rust statics are never dropped, so without
/// this the cached contexts still hold Metal resources when ggml's own global destructor
/// runs, and it aborts:
///
/// ```text
/// ggml-metal-device.m:608: GGML_ASSERT([rsets->data count] == 0) failed
/// ```
///
/// The work is already finished by then, so it presents as a crash on quit — which looks
/// exactly like the app falling over, and would have shipped that way.
pub fn unload_all() {
    if let Some(cache) = LOADED.get() {
        if let Ok(mut guard) = cache.lock() {
            guard.clear();
        }
    }
}

pub fn run<F>(
    model_path: &str,
    lang: &'static str,
    rx: Receiver<Vec<i16>>,
    on_ready: impl FnOnce(),
    mut on_result: F,
) -> Result<(), String>
where
    F: FnMut(RecognitionResult),
{
    // Free after the first session: see `preload`.
    let ctx = preload(model_path)?;
    let mut state = ctx
        .create_state()
        .map_err(|e| format!("failed to create Whisper state: {e}"))?;
    on_ready();

    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    // Hold a couple of cores back so the translator threads and the UI never starve.
    let threads = threads.saturating_sub(2).max(2) as i32;

    // The window of audio not yet committed, as the f32 mono Whisper wants.
    let mut window: Vec<f32> = Vec::with_capacity(ms_to_samples(MAX_WINDOW_MS));
    let mut since_pass = 0usize;
    let mut passes = 0u32;
    // Pass durations, to check the one invariant that matters: a pass must finish inside
    // STEP_MS. If it does not, the recognizer falls behind the audio permanently and
    // latency grows without bound rather than settling.
    let mut pass_ms_all: Vec<u128> = Vec::new();
    let mut last_partial = String::new();
    // Tail of what has already been emitted as `Final`, for overlap stripping.
    let mut committed_tail = String::new();

    for chunk in rx {
        window.extend(chunk.iter().map(|s| *s as f32 / 32768.0));
        since_pass += chunk.len();

        // The first pass runs as soon as there is enough audio to decode at all, rather
        // than after a full step. It costs one extra pass per session and takes roughly a
        // second off the wait before anything appears — the part of the delay a user
        // actually notices, because it is the only one they sit through with a blank bar.
        let step_samples = if passes == 0 {
            ms_to_samples(MIN_INFER_MS)
        } else {
            ms_to_samples(STEP_MS)
        };
        if since_pass < step_samples || window.len() < ms_to_samples(MIN_INFER_MS) {
            continue;
        }
        since_pass = 0;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(lang));
        params.set_translate(false);
        // See the module docs: without this, long sessions degenerate into repetition.
        params.set_no_context(true);
        params.set_n_threads(threads);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        let pass_start = std::time::Instant::now();
        if let Err(e) = state.full(params, &window) {
            eprintln!("[whisper] inference failed: {e}");
            continue;
        }
        passes += 1;
        let pass_ms = pass_start.elapsed().as_millis();
        pass_ms_all.push(pass_ms);
        // The first pass pays for GPU pipeline setup on top of inference, so it is reported
        // separately rather than being averaged into the steady-state figure.
        if passes <= 2 || pass_ms > 2 * STEP_MS as u128 {
            eprintln!(
                "[whisper] pass {passes}: {pass_ms}ms over {:.1}s of audio{}",
                window.len() as f32 / SAMPLE_RATE as f32,
                if pass_ms > STEP_MS as u128 { "  (slower than the step — falling behind)" } else { "" }
            );
        }

        let n = state.full_n_segments();
        let window_end_cs = (window.len() * 100 / SAMPLE_RATE) as i64;

        // Walk the segments in order, committing the ones the speaker has moved past.
        let force_commit = window.len() >= ms_to_samples(MAX_WINDOW_MS);
        let mut commit_to_samples = 0usize;
        let mut pending = String::new();
        // Where the first segment we are *keeping* begins. The window is trimmed to here
        // rather than to the end of the last committed segment: Whisper's segment end
        // timestamps run slightly short of the audio they describe, so trimming to an end
        // left the tail of committed speech in the window, where the next pass transcribed
        // it again. That produced visibly duplicated text across the boundary —
        // `暗くならないうちに` followed by `うちにあれに乗りたいと思います`.
        let mut keep_from_samples: Option<usize> = None;

        for i in 0..n {
            let Some(seg) = state.get_segment(i) else { continue };
            let Ok(text) = seg.to_str_lossy() else { continue };
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            // Whisper invents text over silence and music. Its own no-speech probability is
            // the cheapest filter for that, and dropping these is what keeps a quiet room
            // from producing captions.
            if seg.no_speech_probability() > NO_SPEECH_THRESHOLD {
                continue;
            }
            let end_cs = seg.end_timestamp();
            let settled = window_end_cs - end_cs >= TRAIL_SILENCE_MS / 10;

            if settled || (force_commit && i + 1 < n) {
                let text = strip_overlap(&committed_tail, &text);
                if text.is_empty() {
                    // Entirely a repeat of what the user has already read.
                    commit_to_samples = cs_to_samples(end_cs);
                    continue;
                }
                remember_tail(&mut committed_tail, &text);
                on_result(RecognitionResult::Final(text));
                commit_to_samples = cs_to_samples(end_cs);
            } else {
                if keep_from_samples.is_none() {
                    keep_from_samples = Some(cs_to_samples(seg.start_timestamp()));
                }
                if !pending.is_empty() {
                    pending.push(' ');
                }
                pending.push_str(&text);
            }
        }

        if force_commit && !pending.is_empty() {
            // Continuous speech with no gap anywhere in the window. Commit the remainder
            // rather than letting the window — and the latency — grow without end.
            let text = strip_overlap(&committed_tail, &std::mem::take(&mut pending));
            if !text.is_empty() {
                remember_tail(&mut committed_tail, &text);
                on_result(RecognitionResult::Final(text));
            }
            commit_to_samples = window.len();
        }

        if commit_to_samples > 0 {
            // Prefer the start of the first kept segment; fall back to the committed end
            // when everything was committed.
            let trim = keep_from_samples
                .unwrap_or(commit_to_samples)
                .max(commit_to_samples)
                .min(window.len());
            window.drain(..trim);
            last_partial.clear();
        }

        let pending = strip_overlap(&committed_tail, &pending);
        if pending.is_empty() {
            on_result(RecognitionResult::Silent);
        } else if pending != last_partial {
            // Only when it actually changed: an unchanged partial would make the pipeline
            // re-translate the same tail every step for no reason.
            last_partial = pending.clone();
            on_result(RecognitionResult::Partial(pending));
        }
    }

    if !pass_ms_all.is_empty() {
        let mut sorted = pass_ms_all.clone();
        sorted.sort_unstable();
        let over = pass_ms_all.iter().filter(|m| **m > STEP_MS as u128).count();
        eprintln!(
            "[whisper] {} passes: mean {}ms, p95 {}ms, max {}ms; {over} exceeded the {}ms step",
            pass_ms_all.len(),
            pass_ms_all.iter().sum::<u128>() / pass_ms_all.len() as u128,
            sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)],
            sorted[sorted.len() - 1],
            STEP_MS,
        );
    }

    // Capture stopped. Anything still in the window was really said, so flush it.
    let flush_tail = committed_tail.clone();
    if window.len() >= ms_to_samples(MIN_INFER_MS) {
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(lang));
        params.set_translate(false);
        params.set_no_context(true);
        params.set_n_threads(threads);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        if state.full(params, &window).is_ok() {
            let mut tail = String::new();
            for i in 0..state.full_n_segments() {
                let Some(seg) = state.get_segment(i) else { continue };
                if seg.no_speech_probability() > NO_SPEECH_THRESHOLD {
                    continue;
                }
                if let Ok(t) = seg.to_str_lossy() {
                    let t = t.trim();
                    if !t.is_empty() {
                        if !tail.is_empty() {
                            tail.push(' ');
                        }
                        tail.push_str(t);
                    }
                }
            }
            let tail = strip_overlap(&flush_tail, &tail);
            if !tail.is_empty() {
                on_result(RecognitionResult::Final(tail));
            }
        }
    }

    Ok(())
}
