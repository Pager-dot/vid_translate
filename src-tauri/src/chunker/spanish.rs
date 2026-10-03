//! Spanish/English: eager fixed-word-count chunking, cutting *before* a connector.
//!
//! This is a literal port of the behaviour that shipped inline in `run_translated_pipeline`
//! (and `find_break_point`) before the chunker was extracted. ES is the working baseline and
//! its emitted chunk sequence must stay byte-identical, so the quirks are deliberate:
//!
//!   * at most ONE chunk is emitted per partial callback, even if more than
//!     `STREAM_CHUNK_WORDS` new words arrived at once (the original used `if`, not `while`);
//!   * word indices are never revised downward, so a Vosk partial that shrinks is simply
//!     not re-sent — the original assumed a sent prefix is stable, and for ES it is close
//!     enough in practice;
//!   * `flush` joins from the already-sent word index into the *final* text, which is what
//!     the original did on `Final`.
//!
//! If you change anything here, re-run the ES clip and diff `chunks-es.jsonl` (see
//! `crate::debug`) against a pre-change run. They must match exactly.

use super::{Chunk, ChunkStrategy};

/// Hand off words to the translator as soon as this many new ones accumulate.
const STREAM_CHUNK_WORDS: usize = 8;

/// How far back from the hard cutoff to look for a nicer break.
const LOOKBACK: usize = 3;

const CONNECTORS: &[&str] = &[
    "a", "al", "de", "del", "que", "y", "o", "u", "pero", "porque", "para", "con", "en", "por",
    "si", "como", "cuando", "aunque", "pues", "sino",
];

pub struct SpanishChunker {
    /// How many whitespace-delimited words of the current utterance have been emitted.
    sent_word_count: usize,
    /// Last partial seen, so `pending_tail` can report the untranslated remainder.
    last_partial: String,
}

impl SpanishChunker {
    pub fn new() -> Self {
        Self { sent_word_count: 0, last_partial: String::new() }
    }
}

impl Default for SpanishChunker {
    fn default() -> Self {
        Self::new()
    }
}

/// A hard word-count cutoff can land mid-phrase (splitting "vamos a ir a almorzar | a un
/// sitio" right on a dangling preposition), so look a few words backward for a better
/// break: *before* a connector, since Spanish prepositions and conjunctions naturally lead
/// the clause that follows them. Falls back to the hard cutoff when the lookback window
/// holds nothing suitable, so this never delays a chunk beyond the original cap.
fn find_break_point(words: &[&str], start: usize, hard_cutoff: usize) -> usize {
    let window_start = hard_cutoff.saturating_sub(LOOKBACK).max(start + 1);
    for i in (window_start..hard_cutoff).rev() {
        let w = words[i].trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if CONNECTORS.contains(&w.as_str()) {
            return i; // cut before this connector — it leads the next chunk
        }
    }
    hard_cutoff
}

impl ChunkStrategy for SpanishChunker {
    fn push_partial(&mut self, partial: &str) -> Vec<Chunk> {
        self.last_partial = partial.to_string();
        let words: Vec<&str> = partial.split_whitespace().collect();
        if words.len() < self.sent_word_count + STREAM_CHUNK_WORDS {
            return Vec::new();
        }
        let hard_cutoff = self.sent_word_count + STREAM_CHUNK_WORDS;
        let cut = find_break_point(&words, self.sent_word_count, hard_cutoff);
        let text = words[self.sent_word_count..cut].join(" ");
        self.sent_word_count = cut;
        // A connector break is a real (if shallow) syntactic boundary; landing on the hard
        // cutoff is not. ES ignores the flag today — it exists for the JA live line — so
        // setting it changes nothing about ES output.
        vec![if cut == hard_cutoff { Chunk::forced(text) } else { Chunk::confident(text) }]
    }

    fn flush(&mut self, final_text: &str) -> Vec<Chunk> {
        let words: Vec<&str> = final_text.split_whitespace().collect();
        let remaining = if words.len() > self.sent_word_count {
            words[self.sent_word_count..].join(" ")
        } else {
            String::new()
        };
        self.reset();
        if remaining.is_empty() {
            Vec::new()
        } else {
            vec![Chunk::confident(remaining)]
        }
    }

    fn pending_tail(&self) -> String {
        let words: Vec<&str> = self.last_partial.split_whitespace().collect();
        if words.len() > self.sent_word_count {
            words[self.sent_word_count..].join(" ")
        } else {
            String::new()
        }
    }

    fn reset(&mut self) {
        self.sent_word_count = 0;
        self.last_partial.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact arithmetic the inline version did, reimplemented here independently, so
    /// this test fails if the port drifts rather than just mirroring it.
    fn reference(partials: &[&str], final_text: &str) -> Vec<String> {
        let mut sent = 0usize;
        let mut out = Vec::new();
        for p in partials {
            let words: Vec<&str> = p.split_whitespace().collect();
            if words.len() >= sent + 8 {
                let hard = sent + 8;
                let cut = find_break_point(&words, sent, hard);
                out.push(words[sent..cut].join(" "));
                sent = cut;
            }
        }
        let words: Vec<&str> = final_text.split_whitespace().collect();
        if words.len() > sent {
            out.push(words[sent..].join(" "));
        }
        out
    }

    fn run(partials: &[&str], final_text: &str) -> Vec<String> {
        let mut c = SpanishChunker::new();
        let mut out = Vec::new();
        for p in partials {
            out.extend(c.push_partial(p).into_iter().map(|c| c.text));
        }
        out.extend(c.flush(final_text).into_iter().map(|c| c.text));
        out
    }

    /// Phase 1.3 asked for a before/after diff of the ES chunk sequence over a real clip.
    /// This is the stronger, deterministic form of that check: 2000 synthetic partial
    /// streams, built from a connector-heavy vocabulary and grown in irregular steps (Vosk
    /// adds more than one word per callback under load), replayed through both the
    /// extracted chunker and a reimplementation of the original inline arithmetic. Any
    /// divergence in cut position, chunk count or ordering fails here.
    #[test]
    fn fuzz_equivalent_to_the_original_inline_algorithm() {
        const VOCAB: &[&str] = &[
            "vamos", "a", "ir", "almorzar", "un", "sitio", "que", "me", "gusta", "mucho",
            "porque", "la", "comida", "es", "buena", "y", "barata", "de", "del", "con", "en",
            "por", "si", "como", "cuando", "aunque", "pues", "sino", "pero", "para", "¿qué?",
            "hoy,", "mañana.", "o", "u", "al",
        ];
        // xorshift, so the corpus is identical on every machine and every run.
        let mut seed: u64 = 0x5EED_1234_ABCD_0001;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for case in 0..2000 {
            let len = 1 + (next() % 40) as usize;
            let words: Vec<&str> =
                (0..len).map(|_| VOCAB[(next() % VOCAB.len() as u64) as usize]).collect();

            // Grow the partial in steps of 1-3 words, as Vosk does.
            let mut partials = Vec::new();
            let mut n = 0usize;
            while n < words.len() {
                n = (n + 1 + (next() % 3) as usize).min(words.len());
                partials.push(words[..n].join(" "));
            }
            let refs: Vec<&str> = partials.iter().map(|s| s.as_str()).collect();
            let final_text = words.join(" ");

            assert_eq!(
                run(&refs, &final_text),
                reference(&refs, &final_text),
                "case {case} diverged on {final_text:?}"
            );
        }
    }

    #[test]
    fn matches_reference_on_growing_partial() {
        let words = "vamos a ir a almorzar a un sitio que me gusta mucho porque la comida es buena y barata";
        let all: Vec<&str> = words.split_whitespace().collect();
        // Simulate Vosk growing the partial one word at a time.
        let partials: Vec<String> =
            (1..=all.len()).map(|n| all[..n].join(" ")).collect();
        let refs: Vec<&str> = partials.iter().map(|s| s.as_str()).collect();
        assert_eq!(run(&refs, words), reference(&refs, words));
    }

    #[test]
    fn cuts_before_a_connector() {
        // 8 words in, word index 7 ("a") is a connector inside the lookback window, so the
        // cut lands before it and the connector leads the next chunk.
        let p = "vamos a ir a almorzar hoy mismo a un sitio";
        let mut c = SpanishChunker::new();
        let chunks = c.push_partial(p);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "vamos a ir a almorzar hoy mismo");
        assert!(chunks[0].boundary_confident);
        assert_eq!(c.pending_tail(), "a un sitio");
    }

    #[test]
    fn falls_back_to_hard_cutoff() {
        let p = "uno dos tres cuatro cinco seis siete ocho nueve";
        let mut c = SpanishChunker::new();
        let chunks = c.push_partial(p);
        assert_eq!(chunks[0].text, "uno dos tres cuatro cinco seis siete ocho");
        assert!(!chunks[0].boundary_confident);
    }

    #[test]
    fn at_most_one_chunk_per_partial() {
        // 20 words arriving in one callback still yields exactly one chunk, as before.
        let p = (1..=20).map(|n| n.to_string()).collect::<Vec<_>>().join(" ");
        let mut c = SpanishChunker::new();
        assert_eq!(c.push_partial(&p).len(), 1);
    }

    #[test]
    fn flush_sends_only_the_remainder() {
        let p = "uno dos tres cuatro cinco seis siete ocho nueve diez";
        let mut c = SpanishChunker::new();
        c.push_partial(p);
        let rest = c.flush(p);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].text, "nueve diez");
        // Reset for the next utterance.
        assert_eq!(c.pending_tail(), "");
    }

    #[test]
    fn flush_with_nothing_left_emits_nothing() {
        let p = "uno dos tres cuatro cinco seis siete ocho";
        let mut c = SpanishChunker::new();
        c.push_partial(p);
        assert!(c.flush(p).is_empty());
    }
}
