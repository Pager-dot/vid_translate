// ============================================================================
// TODO(delete): this whole module is dead for Japanese.
//
// Japanese now uses Whisper's native translate task, which emits English directly — so
// there is no Japanese text to cut into clauses, and `run_translated_pipeline` bypasses
// everything here. Measured +6.67 BLEU / +4.76 chrF over transcribe-then-translate on the
// 36-minute multi-speaker clip.
//
// Kept for two reasons only:
//   1. Spanish still uses `SpanishChunker`, and that behaviour is required to stay
//      byte-identical. Deleting this module means moving the Spanish path out of it first.
//   2. VID_TRANSLATE_JA_TWO_STAGE=1 still routes Japanese through here, so the two
//      approaches can be compared by hand.
//
// Once the native-translate path is confirmed in real use: delete `japanese.rs` entirely
// (along with its ~25 tests and the measured guard table), delete `FinalOnlyChunker`, and
// fold whatever remains of `SpanishChunker` back into the ES path.
// ============================================================================

//! Where to cut a growing ASR partial into units worth translating.
//!
//! Vosk only fires `Final` once it hears a pause, so a long sentence spoken in one breath
//! would sit untranslated until the speaker stops. Both languages therefore watch the
//! growing `Partial` and hand off finished pieces early — but *where* a piece finishes is
//! language-specific enough that the two rules run in opposite directions. See
//! `spanish.rs` and `japanese.rs`.
//!
//! This module is a pure function of the partial text: no Tauri, no audio, no model. That
//! is what makes it unit-testable and what lets `bin/ja_eval.rs` drive it offline.

pub mod japanese;
pub mod spanish;

pub use japanese::JapaneseChunker;
pub use spanish::SpanishChunker;

/// One translatable unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    /// False when emitted by a forced timeout/length cap rather than a real clause
    /// boundary. The frontend renders these provisionally (reduced opacity) because the
    /// translation of a clause cut mid-predicate is a guess that may change.
    pub boundary_confident: bool,
}

impl Chunk {
    fn confident(text: impl Into<String>) -> Self {
        Self { text: text.into(), boundary_confident: true }
    }
    fn forced(text: impl Into<String>) -> Self {
        Self { text: text.into(), boundary_confident: false }
    }
}

/// Decides where to cut a growing ASR partial into translatable units.
pub trait ChunkStrategy: Send {
    /// Feed the latest full partial transcript. Returns zero or more chunks that are ready
    /// to translate now. Retains any unconsumed tail.
    fn push_partial(&mut self, partial: &str) -> Vec<Chunk>;

    /// ASR reported a final. Flush everything remaining.
    ///
    /// Takes the final text rather than being argument-less (as first sketched): Vosk's
    /// `Final` string is *not* guaranteed to equal the last partial — it is re-ranked once
    /// the full audio is in — so the strategy has to flush against the final text, not
    /// against whatever the last partial happened to say.
    fn flush(&mut self, final_text: &str) -> Vec<Chunk>;

    /// The still-unconsumed tail of the current utterance, as source-language text.
    ///
    /// Phase 3 re-translates this on every partial and *replaces* the live line, instead of
    /// locking in an append-only guess. Empty when everything has been emitted.
    fn pending_tail(&self) -> String;

    fn reset(&mut self);
}

/// Picks the strategy for a source language.
///
/// `use_local` matters: the Japanese clause chunker only makes sense paired with the
/// Phase 3 re-translating live line, and re-translating a growing tail every 300ms is only
/// affordable against the local CT2 model — against Ollama it would be an HTTP call per
/// partial. Ollama JA therefore keeps the old translate-on-`Final`-only behaviour.
pub fn for_language(source_lang: &str, use_local: bool) -> Box<dyn ChunkStrategy> {
    match (source_lang, use_local) {
        ("ja", true) => Box::new(JapaneseChunker::new()),
        ("ja", false) => Box::new(FinalOnlyChunker::default()),
        _ => Box::new(SpanishChunker::new()),
    }
}

/// Translate only on `Final`, never mid-utterance — the pre-existing Japanese behaviour,
/// kept for the Ollama path.
#[derive(Default)]
pub struct FinalOnlyChunker;

impl ChunkStrategy for FinalOnlyChunker {
    fn push_partial(&mut self, _partial: &str) -> Vec<Chunk> {
        Vec::new()
    }
    fn flush(&mut self, final_text: &str) -> Vec<Chunk> {
        if final_text.trim().is_empty() {
            Vec::new()
        } else {
            vec![Chunk::confident(final_text.trim())]
        }
    }
    fn pending_tail(&self) -> String {
        String::new()
    }
    fn reset(&mut self) {}
}
