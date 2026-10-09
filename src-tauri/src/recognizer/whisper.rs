//! Whisper, made to look streaming.
//!
//! Whisper transcribes a *finished* window of audio; it has no notion of a growing partial.
//! This module manufactures the streaming contract the rest of the pipeline expects by
//! holding a window of recent audio, re-transcribing all of it every step, and
//! deciding which of the segments that come back are finished:
//!
//! * a segment with enough audio after it has stopped changing — the speaker moved on — so
//!   it is emitted as `Final` and its audio is dropped from the window;
//! * whatever is left is emitted as `Partial`, and will be re-transcribed (and possibly
//!   revised) on the next pass.
//!
//! Re-transcribing the window is not as wasteful as it sounds: Whisper's encoder runs on a
//! zero-padded 30-second window whatever you feed it, so a pass over 3 seconds of audio
//! costs about the same as a pass over 15 (measured: ~400ms encode either way). The cost is
//! therefore per *pass*, not per second of audio, which is why the step size sets both the
//! latency and the CPU load and why there is nothing to gain from a smaller window.
//!
//! `audio_ctx` claws back part of that padding cost — see `audio_ctx_for` — but the shape of
//! the problem is unchanged: cost per pass.
//!
//! ## Keeping up is not optional
//!
//! A pass must finish inside the step, or the audio queued behind it grows and the captions
//! describe steadily older audio — fluent, plausible, and further out of sync every minute,
//! with no mechanism that ever recovers. Two things hold the line, and both matter more than
//! raw speed: the step is measured from real pass durations rather than fixed, and a backlog
//! past `BACKLOG_STEPS` is *discarded* instead of transcribed late. On a machine too slow
//! for the chosen model the result is captions with holes rather than captions that lag,
//! which is the right failure: a late caption is read against the wrong picture.
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

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext,
    WhisperVadContextParams, WhisperVadParams,
};

use super::{Pacing, RecognitionResult};

/// How much new audio to accumulate between passes, as a range rather than a constant.
///
/// The step is the dominant term in user-visible latency: a word spoken just after a pass
/// waits almost a full step before the next one sees it. It is also what sets the CPU duty
/// cycle, and those pull in opposite directions — which is why a single number cannot be
/// right for both an M3 (a pass costs ~0.4s, so a 2s step wastes 1.6s of latency doing
/// nothing) and a slower x86 laptop (a pass costs more than 2s, so a 2s step asks for more
/// passes per second than the machine can run).
///
/// So it is measured instead: the step tracks recent cycle durations (see `STEP_HEADROOM`),
/// clamped to this range.
///
/// **The floor is 2s, which is where this started, so the step only ever stretches.** A
/// machine fast enough for 2s steps was never the problem, and tightening below that is a
/// different change with its own costs — double the passes means double the duty cycle and
/// twice as many chances to revise a partial line under the reader, on battery. Apple
/// Silicon therefore keeps exactly the cadence it was tuned and measured with, and only a
/// machine that cannot hold 2s sees any of this move.
const STEP_MIN_MS: usize = 2000;
const STEP_MAX_MS: usize = 4000;

/// Step as a multiple of the recent mean pass. The margin above 1.0 is what keeps the
/// recognizer from spending every available cycle on inference: at 1.25 the decode occupies
/// ~80% of one core's worth of work per step, leaving the rest for the VAD, the translator
/// threads and the UI.
const STEP_HEADROOM: f32 = 1.25;

/// Where the step starts before any cycle has been timed — the same as the floor, so until
/// a machine proves it is too slow nothing about its behaviour differs from before.
const STEP_START_MS: usize = STEP_MIN_MS;

/// Un-transcribed audio that means the recognizer has fallen behind for real, as a multiple
/// of the current step. Below this, a slow pass is absorbed by the next one being skipped;
/// above it, the audio is arriving faster than it can be decoded and the backlog will grow
/// without bound unless something is thrown away.
const BACKLOG_STEPS: usize = 2;


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

/// The Silero VAD weights, expected next to the main model. ~865KB.
///
/// Optional: without it the speech gate below is skipped and Whisper decides for itself
/// what is speech, which it is bad at.
pub const VAD_MODEL_FILE: &str = "ggml-silero-v5.1.2.bin";

/// Audio kept when the gate rejects a window, in case speech starts right at its edge and
/// the VAD missed the onset.
const VAD_KEEP_TAIL_MS: usize = 500;

/// Phrases Whisper emits over music and silence rather than from anything that was said.
///
/// These are artefacts of its training data — it saw an enormous number of videos that end
/// with someone thanking the viewer over outro music, so music alone is enough to produce
/// them. Observed in this project: a 36-minute clip whose closing music became
/// `ご視聴ありがとうございました`, and prominent background music producing "thank you for
/// watching" out of nothing.
///
/// Matched against the whole segment only. A segment that genuinely *is* someone thanking
/// their viewers is a real caption, and this must not eat it — which is why the test below
/// requires the phrase to be essentially the entire segment.
/// Only phrases that are both distinctive to video outros and implausible as spontaneous
/// speech. Bare "thank you", "thanks", "bye", `ありがとうございました` and `おやすみなさい`
/// were on this list and were removed: people say all of those for real, and silently
/// eating someone's goodbye is a worse bug than the one being fixed. The VAD gate is the
/// primary defence; this is only the net under it.
const HALLUCINATED_PHRASES: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "thank you very much for watching",
    "thank you for watching this video",
    "please subscribe",
    "subscribe to my channel",
    "like and subscribe",
    "ご視聴ありがとうございました",
    "ご視聴ありがとうございます",
    "チャンネル登録お願いします",
];

/// Strips what no sane caption needs, for comparison against the phrase list.
fn canonical(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && !c.is_ascii_punctuation())
        .filter(|c| !matches!(c, '、' | '。' | '！' | '？' | '・' | '…' | '「' | '」'))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// True when a segment is one of Whisper's stock phrases and nothing else.
///
/// Deliberately exact rather than substring: "thank you for watching, and now the weather"
/// is a real sentence, and dropping it would be worse than the hallucination this prevents.
fn is_hallucinated(text: &str) -> bool {
    let c = canonical(text);
    if c.is_empty() {
        return true;
    }
    HALLUCINATED_PHRASES.iter().any(|p| canonical(p) == c)
}

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
/// Encoder context tokens for 30s of audio — the full mel window, and the value Whisper
/// uses if told nothing.
const AUDIO_CTX_FULL: i32 = 1500;

/// Encoder tokens to allow for a window of `samples`.
///
/// This is the one place the "a pass over 3s costs the same as a pass over 15s" property in
/// the module docs can be attacked rather than worked around. That property is not a law of
/// nature: it is Whisper zero-padding every input to 30 seconds and then running its
/// encoder over all of it, padding included. `audio_ctx` caps how much of that mel window
/// the encoder actually walks, so a 6-second window can be encoded as 6 seconds of work
/// instead of 30.
///
/// Scaled with a deliberate margin above the true length: the conv front-end needs context
/// past the audio it is describing, and cutting too close truncates the last word or sends
/// the decoder looking for text in padding it cannot see. `clamp` to `AUDIO_CTX_FULL` means
/// a full-length window is simply the old behaviour.
fn audio_ctx_for(samples: usize) -> i32 {
    // An escape hatch, and the knob the A/B above was measured with:
    // VID_TRANSLATE_WHISPER_FULL_AUDIO_CTX=1 restores the full mel window, so a quality
    // regression blamed on this can be confirmed or cleared in one run rather than argued
    // about.
    static FULL: OnceLock<bool> = OnceLock::new();
    if *FULL.get_or_init(|| {
        std::env::var("VID_TRANSLATE_WHISPER_FULL_AUDIO_CTX").is_ok_and(|v| v == "1")
    }) {
        return AUDIO_CTX_FULL;
    }
    let secs = samples as f32 / SAMPLE_RATE as f32;
    // 1500 tokens per 30s, plus 2s of headroom, rounded up to a multiple of 32 — whisper's
    // encoder works in blocks and an awkward value buys nothing.
    let tokens = ((secs + 2.0) * (AUDIO_CTX_FULL as f32 / 30.0)).ceil() as i32;
    let tokens = (tokens + 31) / 32 * 32;
    tokens.clamp(320, AUDIO_CTX_FULL)
}

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
    // One model at a time. The user can switch size between sessions, and keeping the old
    // one cached would hold its weights — up to 1.5GB — for a path nothing will ask for
    // again.
    guard.clear();
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
    translate: bool,
    pacing: Pacing,
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

    // Speech gate. Whisper will caption background music rather than admit it heard
    // nothing — "thank you for watching" over an outro is its single most recognisable
    // failure, and `no_speech_probability` does not catch it because the model is
    // *confident*. Silero answers the narrower question of whether this is speech at all,
    // and a window it rejects never reaches Whisper, which also saves the inference.
    //
    // Optional by design: the weights sit beside the main model, and if they are absent the
    // gate is skipped with a warning rather than failing the session.
    let mut vad = match std::path::Path::new(model_path).parent().map(|d| d.join(VAD_MODEL_FILE)) {
        Some(p) if p.exists() => {
            let mut cp = WhisperVadContextParams::new();
            cp.set_n_threads(threads);
            match WhisperVadContext::new(&p.to_string_lossy(), cp) {
                Ok(v) => {
                    eprintln!("[whisper] speech gate active ({})", p.display());
                    Some(v)
                }
                Err(e) => {
                    eprintln!("[whisper] VAD load failed, gate disabled: {e}");
                    None
                }
            }
        }
        _ => {
            eprintln!(
                "[whisper] no {VAD_MODEL_FILE} beside the model — speech gate disabled, \
                 expect captions over music"
            );
            None
        }
    };

    // The window of audio not yet committed, as the f32 mono Whisper wants.
    let mut window: Vec<f32> = Vec::with_capacity(ms_to_samples(MAX_WINDOW_MS));
    let mut since_pass = 0usize;
    let mut passes = 0u32;
    // Cycle durations — speech gate plus inference, i.e. everything a step must pay for.
    // This checks the one invariant that matters: a cycle must finish inside the step. If it
    // does not, the recognizer falls behind the audio and, without the catch-up below,
    // latency grows without bound rather than settling.
    let mut pass_ms_all: Vec<u128> = Vec::new();
    // The current step, and the mean pass it is derived from. Both move; see STEP_MIN_MS.
    let mut step_ms = STEP_START_MS;
    let mut pass_ema_ms = 0f32;
    // Audio thrown away to catch up, and how many times. Reported at the end, because a
    // session that dropped audio produced captions with holes in them and the log should
    // say so rather than leaving it to be guessed from the transcript.
    let mut dropped_ms = 0usize;
    let mut catch_ups = 0u32;
    let mut overruns = 0u32;
    // Audio pulled from the channel, against wall-clock time, to measure how far behind
    // live the recognizer is running. See the catch-up block in the loop.
    let mut consumed_samples = 0usize;
    let mut clock: Option<std::time::Instant> = None;
    let mut last_partial = String::new();
    // Tail of what has already been emitted as `Final`, for overlap stripping.
    let mut committed_tail = String::new();

    for chunk in rx.iter() {
        let clock = *clock.get_or_insert_with(std::time::Instant::now);
        consumed_samples += chunk.len();

        // How far behind the audio this recognizer is running.
        //
        // The channel from the capture thread is unbounded, and that is the hazard this
        // block exists to handle. A live capture pushes one second of audio per second, so
        // if a cycle costs more than the audio it covers, blocks queue up and the
        // recognizer keeps transcribing audio from ever further in the past: captions stay
        // fluent and drift steadily out of sync, and nothing ever recovers, not even in
        // silence, because nothing shrinks the queue.
        //
        // Audio consumed against the wall clock measures exactly that. Note what it does
        // *not* measure: whether the producer is live. A decoder slower than real time
        // falls behind the clock whether it is fed by a microphone or by a file, which is
        // why the answer comes from `pacing` and not from this number. See `Pacing`.
        let consumed_ms = (consumed_samples * 1000 / SAMPLE_RATE) as i64;
        let lag_ms = clock.elapsed().as_millis() as i64 - consumed_ms;

        // Past a couple of steps the backlog is growing rather than fluctuating, and the
        // only way back to live is to give up on some audio: a caption twenty seconds late
        // is worse than a missing one, because the viewer reads it against the wrong
        // picture.
        if pacing == Pacing::Live && lag_ms > (step_ms * BACKLOG_STEPS) as i64 {
            // Fast-forward: take everything queued and keep only the newest block. Dropping
            // the *oldest* audio is what makes this a catch-up rather than a stutter.
            let mut newest = chunk;
            let mut skipped = 0usize;
            while let Ok(next) = rx.try_recv() {
                skipped += newest.len();
                newest = next;
            }
            consumed_samples += skipped;
            dropped_ms += skipped * 1000 / SAMPLE_RATE;
            catch_ups += 1;
            // The window holds audio from before the gap. Keeping it would splice speech
            // across a hole and invite a confabulated bridge between the two halves.
            window.clear();
            since_pass = 0;
            last_partial.clear();
            // `committed_tail` deliberately stays: it is what the viewer has already read,
            // and overlap stripping across a gap is harmless, where clearing it would let a
            // repeat through at exactly the moment the transcript is already damaged.
            window.extend(newest.iter().map(|s| *s as f32 / 32768.0));
            since_pass += newest.len();
            continue;
        }

        window.extend(chunk.iter().map(|s| *s as f32 / 32768.0));
        since_pass += chunk.len();

        // The first pass runs as soon as there is enough audio to decode at all, rather
        // than after a full step. It costs one extra pass per session and takes roughly a
        // second off the wait before anything appears — the part of the delay a user
        // actually notices, because it is the only one they sit through with a blank bar.
        let step_samples = if passes == 0 {
            ms_to_samples(MIN_INFER_MS)
        } else {
            ms_to_samples(step_ms)
        };
        if since_pass < step_samples || window.len() < ms_to_samples(MIN_INFER_MS) {
            continue;
        }
        since_pass = 0;

        // Everything from here to the end of inference is what one step has to pay for, so
        // it is all timed. The gate is not free and its cost grows with the window — it
        // re-scans the whole thing each pass — so timing inference alone (which is what
        // this used to do) under-reports the real duty cycle by 10-15% and tunes the step
        // too tight.
        let cycle_start = std::time::Instant::now();

        // The gate: is any of this speech? If not, discard the window instead of handing
        // music to Whisper and letting it invent a sentence.
        if let Some(vad) = vad.as_mut() {
            let mut vp = WhisperVadParams::new();
            vp.set_threshold(0.5);
            vp.set_min_speech_duration(100);
            vp.set_min_silence_duration(150);
            let speech = match vad.segments_from_samples(vp, &window) {
                Ok(segs) => segs.num_segments() > 0,
                // Fail open: a broken gate should degrade to the old behaviour, not stop
                // captions entirely.
                Err(e) => {
                    eprintln!("[whisper] VAD failed on this window, passing it through: {e}");
                    true
                }
            };
            if !speech {
                // Keep a short tail in case speech begins right at the window's edge and
                // the VAD missed its onset.
                let keep = ms_to_samples(VAD_KEEP_TAIL_MS).min(window.len());
                let drop_to = window.len() - keep;
                window.drain(..drop_to);
                last_partial.clear();
                on_result(RecognitionResult::Silent);
                continue;
            }
        }

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(lang));
        params.set_translate(translate);
        // See the module docs: without this, long sessions degenerate into repetition.
        params.set_no_context(true);
        // Anti-confabulation, all cheap and all aimed at the same failure: Whisper would
        // rather produce a confident sentence than nothing.
        //   - suppress_nst: drop non-speech tokens outright.
        //   - temperature 0 with no increment: disable the fallback sampling that turns a
        //     low-confidence decode into a creative one.
        //   - no_speech / logprob thresholds: discard a segment the model itself doubts.
        params.set_suppress_nst(true);
        params.set_temperature(0.0);
        params.set_temperature_inc(0.0);
        params.set_no_speech_thold(0.6);
        params.set_logprob_thold(-1.0);
        params.set_n_threads(threads);
        // Encode the audio that is there, not the 30s of padding around it.
        params.set_audio_ctx(audio_ctx_for(window.len()));
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        if let Err(e) = state.full(params, &window) {
            eprintln!("[whisper] inference failed: {e}");
            continue;
        }
        passes += 1;
        let pass_ms = cycle_start.elapsed().as_millis();
        pass_ms_all.push(pass_ms);
        if pass_ms > step_ms as u128 {
            overruns += 1;
        }

        // Retune the step. An EMA rather than the last pass alone: pass cost varies with
        // how much speech is in the window, and chasing each sample would make the step —
        // and so the caption cadence — visibly jittery. The first pass is excluded because
        // it also pays for pipeline setup, which never recurs.
        if passes == 1 {
            pass_ema_ms = pass_ms as f32;
        } else {
            pass_ema_ms = 0.7 * pass_ema_ms + 0.3 * pass_ms as f32;
        }
        let tuned = (pass_ema_ms * STEP_HEADROOM) as usize;
        let tuned = tuned.clamp(STEP_MIN_MS, STEP_MAX_MS);
        if tuned != step_ms && (tuned.abs_diff(step_ms) > 150 || passes < 4) {
            if passes > 2 {
                eprintln!("[whisper] step {step_ms}ms → {tuned}ms (mean pass {:.0}ms)", pass_ema_ms);
            }
            step_ms = tuned;
        }

        // The first pass pays for pipeline setup on top of inference, so it is reported
        // separately rather than being averaged into the steady-state figure.
        if passes <= 2 || pass_ms > 2 * step_ms as u128 {
            eprintln!(
                "[whisper] pass {passes}: {pass_ms}ms (gate+decode) over {:.1}s of audio \
                 (audio_ctx {}){}",
                window.len() as f32 / SAMPLE_RATE as f32,
                audio_ctx_for(window.len()),
                if pass_ms > step_ms as u128 {
                    "  (slower than the step — dropping audio to stay live)"
                } else {
                    ""
                }
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
            // The net under the speech gate: a stock outro phrase that got through anyway.
            if is_hallucinated(&text) {
                eprintln!("[whisper] dropped stock phrase: {text:?}");
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
        eprintln!(
            "[whisper] {} passes: mean {}ms, p95 {}ms, max {}ms; {overruns} exceeded the step \
             (final step {step_ms}ms)",
            pass_ms_all.len(),
            pass_ms_all.iter().sum::<u128>() / pass_ms_all.len() as u128,
            sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)],
            sorted[sorted.len() - 1],
        );
        if catch_ups > 0 {
            // Worth saying plainly: this machine could not keep up, and the transcript has
            // holes in it where the audio was dropped to stay in sync. A smaller model is
            // the fix.
            eprintln!(
                "[whisper] fell behind {catch_ups}x — dropped {:.1}s of audio to stay live; \
                 try a smaller model",
                dropped_ms as f32 / 1000.0
            );
        }
    }

    // Capture stopped. Anything still in the window was really said, so flush it.
    let flush_tail = committed_tail.clone();
    if window.len() >= ms_to_samples(MIN_INFER_MS) {
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(lang));
        params.set_translate(translate);
        params.set_no_context(true);
        params.set_n_threads(threads);
        params.set_audio_ctx(audio_ctx_for(window.len()));
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
                    if !t.is_empty() && !is_hallucinated(t) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_ctx_scales_with_the_window_and_never_exceeds_the_mel_window() {
        // A full-length window must ask for the whole thing, or this optimisation would be
        // silently truncating audio Whisper was given.
        assert_eq!(audio_ctx_for(ms_to_samples(30_000)), AUDIO_CTX_FULL);
        assert_eq!(audio_ctx_for(ms_to_samples(60_000)), AUDIO_CTX_FULL);
        // Typical windows cost a fraction of it...
        assert!(audio_ctx_for(ms_to_samples(4_000)) < AUDIO_CTX_FULL / 3);
        // ...but always with headroom past the audio itself, which is what keeps the last
        // word from being cut off.
        for ms in [1_000, 2_000, 5_000, 10_000, 20_000] {
            let needed = (ms as f32 / 1000.0 * (AUDIO_CTX_FULL as f32 / 30.0)) as i32;
            assert!(
                audio_ctx_for(ms_to_samples(ms)) > needed,
                "{ms}ms: {} leaves no headroom over {needed}",
                audio_ctx_for(ms_to_samples(ms))
            );
        }
    }

    /// What a pass actually costs on *this* machine, with and without the `audio_ctx` cap.
    ///
    /// The question this answers is the only one that matters for latency: is a pass faster
    /// than the step it has to fit inside? Run it on any machine where captions lag:
    ///
    /// ```text
    /// cargo test --release -p vid_translate audio_ctx_pass_cost -- --ignored --nocapture
    /// ```
    ///
    /// `#[ignore]`d because it needs a downloaded model and takes a few seconds. Noise
    /// rather than speech, so the figures are encode-dominated — which is the part
    /// `audio_ctx` changes, and the part that does not depend on what was said.
    #[test]
    #[ignore]
    fn audio_ctx_pass_cost() {
        let path = std::env::var("VID_TRANSLATE_WHISPER_MODEL").unwrap_or_else(|_| {
            dirs::data_local_dir()
                .unwrap_or_else(|| ".".into())
                .join("vid_translate")
                .join("ggml-small.bin")
                .to_string_lossy()
                .into_owned()
        });
        let Ok(ctx) = preload(&path) else {
            eprintln!("no model at {path} — skipping");
            return;
        };
        let mut state = ctx.create_state().expect("state");
        let threads =
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_sub(2).max(2) as i32;

        // Something with broadband content, so the encoder does real work.
        let make = |ms: usize| -> Vec<f32> {
            (0..ms_to_samples(ms))
                .map(|i| {
                    let t = i as f32 / SAMPLE_RATE as f32;
                    0.3 * ((t * 220.0 * 6.283).sin() + (t * 700.0 * 6.283).sin() * 0.5)
                })
                .collect()
        };

        eprintln!("model: {path}  threads: {threads}");
        for ms in [2_000usize, 6_000, 12_000] {
            let audio = make(ms);
            for ctx_tokens in [AUDIO_CTX_FULL, audio_ctx_for(audio.len())] {
                let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                p.set_language(Some("ja"));
                p.set_translate(true);
                p.set_no_context(true);
                p.set_temperature(0.0);
                p.set_temperature_inc(0.0);
                p.set_n_threads(threads);
                p.set_audio_ctx(ctx_tokens);
                p.set_print_special(false);
                p.set_print_progress(false);
                p.set_print_realtime(false);
                p.set_print_timestamps(false);
                let t = std::time::Instant::now();
                state.full(p, &audio).expect("inference");
                let took = t.elapsed();
                eprintln!(
                    "  {:>5}ms window, audio_ctx {ctx_tokens:>4} → {:>6.0}ms/pass",
                    ms,
                    took.as_secs_f32() * 1000.0
                );
            }
        }
        unload_all();
    }

    #[test]
    fn drops_whispers_stock_outro_phrases() {
        for s in [
            "Thank you for watching!",
            "thank you for watching",
            "Thanks for watching.",
            "Please subscribe!",
            "ご視聴ありがとうございました。",
            "ご視聴ありがとうございます",
            "チャンネル登録お願いします！",
        ] {
            assert!(is_hallucinated(s), "should have been dropped: {s:?}");
        }
    }

    #[test]
    fn keeps_real_speech_that_merely_resembles_them() {
        // The failure mode that matters more than the one being fixed: silently eating
        // something the speaker actually said. Short pleasantries are real speech.
        for s in [
            "Thank you.",
            "Thanks!",
            "Bye",
            "ありがとうございました",
            "おやすみなさい",
            "Thank you for watching, and now let's look at the menu.",
            "I want to thank you for watching over my bag.",
            "Subscribe to a newspaper, she said.",
            "横浜に来ました",
        ] {
            assert!(!is_hallucinated(s), "should have been kept: {s:?}");
        }
    }

    #[test]
    fn empty_and_punctuation_only_segments_are_dropped() {
        for s in ["", "   ", ".", "。", "、、、", "!?"] {
            assert!(is_hallucinated(s), "should have been dropped: {s:?}");
        }
    }
}
