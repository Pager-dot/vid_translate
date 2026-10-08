//! Speech recognition backends.
//!
//! Both backends present the same contract — a stream of `RecognitionResult` driven by
//! 16 kHz mono `i16` blocks — so everything downstream (`crate::chunker`, the translator
//! threads, the frontend's event handling) is identical whichever one is running. That is
//! what let Japanese move from Vosk to Whisper without touching the pipeline.
//!
//! They are not equivalent, though, and the difference is the interesting part:
//!
//! * **Vosk** is natively streaming. It emits a growing partial as audio arrives and a
//!   `Final` when it hears a pause. Cheap, and sub-second to first text.
//! * **Whisper** is not. It transcribes a *window* of finished audio, so `whisper.rs`
//!   synthesises the streaming contract by re-transcribing a sliding window every couple of
//!   seconds. More accurate by a wide margin (on multi-speaker audio, nearly 3x the BLEU
//!   end-to-end — see `docs/ja-diagnosis.md`), at the cost of a couple of seconds before
//!   anything appears at all.

pub mod vosk;
pub mod whisper;

pub enum RecognitionResult {
    /// Words being spoken right now (unstable, updates rapidly)
    Partial(String),
    /// Completed utterance (stable, ready to display)
    Final(String),
    /// Silence or noise
    Silent,
}

/// Which engine to recognise with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Vosk,
    Whisper {
        /// The language to decode as (`"ja"`). Whisper will happily auto-detect, but on
        /// short windows of a conversation it guesses wrong often enough to matter, and
        /// every window here is short.
        lang: &'static str,
        /// Emit English directly instead of the source language.
        ///
        /// Whisper has a native translate task, and on the 36-minute multi-speaker clip it
        /// beat transcribing-then-translating by **+6.67 BLEU / +4.76 chrF** (19.88 / 57.82
        /// against 13.21 / 53.06) while deleting a stage and a 240MB model. It wins because
        /// it heard the *audio*: forcing everything through Japanese text loses meaning in
        /// the handoff, of which the `みなとみらい` → `港未来` → "the future of Yokohama's
        /// port" failure was one instance.
        ///
        /// The cost is that one pass gives you either the source text or the English, never
        /// both — so this path has no Japanese caption line, and `crate::chunker` has
        /// nothing to chunk.
        translate: bool,
    },
}

impl Backend {
    /// True when the recognizer already emits English, so nothing downstream should
    /// translate again.
    pub fn emits_english(&self) -> bool {
        matches!(self, Backend::Whisper { translate: true, .. })
    }
}

/// Runs the chosen backend over `rx`, calling `on_result` for every update.
///
/// `model_path` is a directory for Vosk and a single `ggml-*.bin` file for Whisper.
///
/// `on_ready` fires once the model is loaded and the next block of audio will actually be
/// decoded. The caller needs this to tell the user the truth: loading is the slowest part of
/// starting a session (487MB for Whisper), it happens inside this call, and audio is already
/// queueing up behind it. Reporting "listening" before it — which is what the pipeline used
/// to do — claims the app is working while it is still blocked.
pub fn run<F>(
    backend: Backend,
    model_path: &str,
    rx: std::sync::mpsc::Receiver<Vec<i16>>,
    on_ready: impl FnOnce(),
    on_result: F,
) -> Result<(), String>
where
    F: FnMut(RecognitionResult),
{
    match backend {
        Backend::Vosk => vosk::run(model_path, rx, on_ready, on_result),
        Backend::Whisper { lang, translate } => {
            whisper::run(model_path, lang, translate, rx, on_ready, on_result)
        }
    }
}
