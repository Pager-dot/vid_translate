// ============================================================================
// TODO(delete): this file is dead on the shipping Japanese path.
//
// It exists to compensate for Vosk's unpunctuated, morpheme-spaced Japanese. Japanese now
// uses Whisper's native translate task and never produces Japanese text at all, so none of
// the tier tables, the de-spacing, the overlap rules or the measured guard values below are
// reached. Reachable only via VID_TRANSLATE_JA_TWO_STAGE=1.
//
// Worth saying plainly for whoever deletes it: the guard table in
// `DEFAULT_MIN_CHUNK_CHARS` was measured, and the three bug fixes here were each found on
// real audio rather than reasoned out. None of that was wrong — it was just aimed at a
// stage that the native-translate path removes. Delete it without ceremony.
// ============================================================================

//! Japanese: cut on clause boundaries, *after* the marker.
//!
//! # Phase 0 findings this module is built on
//!
//! * **Vosk JA emits spaces between morphemes, not words.** `こんにちは 、 元気 です か ？`
//!   is a representative partial. Those spaces are an artefact of the recognizer, not
//!   Japanese orthography, so this module de-spaces first and then works on the raw `char`
//!   stream. Matching `です` would otherwise fail against `で す`.
//! * **Vosk JA does emit `、` and `。`** — but only sometimes, and not reliably at real
//!   sentence ends, so punctuation is treated as a bonus signal (Tier A) rather than the
//!   primary one.
//! * **JA partials are revised, not append-only.** Kanji and word-boundary choices get
//!   re-ranked as more audio arrives. That is why Phase 3 keys the live line's id off the
//!   boundary index rather than a string prefix, and why `push_partial` below clamps rather
//!   than trusting its own consumed-prefix bookkeeping.
//!
//! # The cut direction is the opposite of Spanish — do not "unify" these
//!
//! Spanish cuts *before* a connector, because a Spanish connector leads the clause that
//! follows it: `...vi la película` | `porque me gusta...`.
//!
//! Japanese cuts *after* the marker, because a Japanese boundary marker **closes** the
//! clause it sits in: `映画を見たから` | `楽しかった`.
//!
//! Japanese is SOV: negation, tense, politeness and the main verb all land at the end of a
//! clause. Cutting before the marker would hand the MT model a subject-and-object with no
//! predicate, which it can only resolve by inventing one. Applying the Spanish direction
//! here makes the output actively worse.

use std::time::{Duration, Instant};

use super::{Chunk, ChunkStrategy};

/// The shortest clause that can be read as closing: 6 chars is roughly the shortest
/// standalone JA clause that still carries a predicate (`わかりました` is exactly 6). This
/// is a linguistic floor used when deciding whether an *ambiguous* marker is clause-final,
/// and is independent of how short a chunk is allowed to be — that is
/// `DEFAULT_MIN_CHUNK_CHARS`, which is much larger and was measured, not reasoned about.
pub const MIN_CHUNK_CHARS: usize = 6;

/// Never emit a sliver: a boundary inside this many characters is ignored and accumulation
/// continues.
///
/// **This value was measured, and it is far larger than it looks like it should be.** Every
/// clause cut costs translation quality, because each chunk is translated with no knowledge
/// of its neighbours. Swept against a 7.7-minute Japanese vlog with a human reference
/// (BLEU / chrF, document-level):
///
/// | min chars | chunks | BLEU | chrF |
/// |---|---|---|---|
/// | 6 | 165 | 13.31 | 51.59 |
/// | 12 | 134 | 15.75 | 52.73 |
/// | 20 | 107 | 16.57 | 52.14 |
/// | 30 | 88 | 17.13 | 53.03 |
/// | 40 | 80 | 17.78 | 53.00 |
/// | 50 | 73 | **18.40** | **53.80** |
/// | never cut (translate on `Final` only) | 65 | 18.11 | 52.76 |
///
/// The trend is monotonic: fewer cuts, better output. 50 is where quality reaches parity
/// with never cutting at all, so it is the largest latency win available for free. Lowering
/// it trades measurable quality for responsiveness; 6 — the value this started with — costs
/// about 5 BLEU, which is most of the quality of the pair.
pub const DEFAULT_MIN_CHUNK_CHARS: usize = 50;

/// Hard ceiling before a forced cut, raised alongside the minimum so a forced cut stays a
/// genuine last resort rather than the common case.
pub const MAX_CHUNK_CHARS: usize = 120;

/// Time-based forced flush, measured from the last emit. This is what preserves
/// responsiveness on long unbroken speech — the original reason the 8-word rule existed.
pub const MAX_WAIT_MS: u64 = 4000;

/// Tier A — hard terminals. Cut immediately after.
const TIER_A: &[char] = &['。', '！', '？', '．', '!', '?'];

/// Tier B — polite / copula endings. Cut after. Checked *before* Tier D, since several
/// contain `で`. Longest-match first.
const TIER_B: &[&str] = &[
    "ましょう", "でしょう", "でした", "ました", "ません", "だった", "である", "です", "ます",
];

/// Tier C — conjunctive particles that close a clause. Cut after. Longest-match first.
const TIER_C: &[&str] = &[
    "けれども", "けれど", "ですが", "だけど", "ながら", "けど", "ので", "から", "のに", "たら",
    "なら", "し", "が",
];

/// Single-character Tier C markers that are ambiguous enough to need extra evidence.
///
/// `が` is both the nominative subject marker and a clause-final "but"; `し` is both the
/// `〜し` conjunctive and the tail of extremely common words (`わたし`, `少し`, `話し`).
/// Either one treated naively shreds every subject phrase, so both additionally require at
/// least one following token *and* a tail already at `MIN_CHUNK_CHARS`.
const TIER_C_AMBIGUOUS: &[&str] = &["が", "し"];

/// Tier D — te-form. Cut after a `て`/`で`, except when what follows is a continuing
/// auxiliary (`〜ている`, `〜ておく`, `〜てしまう`, `〜てみる`, `〜てある`) — there the
/// predicate has not landed yet.
/// `き` covers both `できる`/`できます` (where the `で` is not a te-form at all) and the
/// `〜てきます` auxiliary; `も` and `は` cover `〜ても`/`〜では`, which continue the clause.
/// Observed on real input: `見ることができます` was cut `...ことがで` | `きます`, and
/// `飛んでも...` was cut `とんで` | `も...`.
const TE_FORM_CONTINUES: &[char] = &['い', 'く', 'お', 'み', 'し', 'あ', 'き', 'も', 'は'];

/// Words that merely *end* in `て`/`で` while being clause-**initial** connectives. They
/// lead the clause that follows them, Spanish-style, so cutting after one strands it at the
/// end of the previous chunk (observed: `ございますよだって` | `ねマート…`).
const TE_FORM_FALSE_POSITIVES: &[&str] =
    &["だって", "でも", "そして", "それで", "なので", "ところで", "まで", "ので"];

/// Tier E — sentence-final particles. Recognised only as the final character of the tail
/// **and only on a real ASR final**, never mid-partial.
///
/// The "final character of the tail" test is worthless during a partial: a Vosk partial's
/// tail ends at an arbitrary point mid-word every ~250ms, so every one of these matches the
/// interior of an ordinary word sooner or later. Observed on real input: `ますか` matched
/// the first half of `...住めますから`, cutting `か|ら`; and `か`/`な`/`わ` are the tails of
/// `かもしれない`, `なる`/`など` and `わたし`/`わかりました`. Mid-partial Tier E shreds
/// words, and anything it would legitimately have caught is caught on the `Final` that
/// Vosk fires at the pause those particles precede.
const TIER_E_MULTI: &[&str] = &["ですね", "ますか", "かな", "よね"];
const TIER_E_SINGLE: &[char] = &['よ', 'ね', 'ぞ', 'ぜ', 'か', 'な', 'わ'];

/// Characters that begin a Tier B, C or E marker, and so can extend a clause that looked
/// finished one character ago. `思います` + `が` is still one clause; cutting at `ます`
/// would strand the "but". When one of these follows a candidate cut, the cut is skipped
/// and the scan continues to the longer boundary.
///
/// `ま`, `で` and `だ` are here for the Tier B endings (`ます`, `ましょう`, `です`,
/// `だった`): without them the ambiguous `し` cut `過ごし|ましょう`, stranding the polite
/// predicate ending in its own chunk.
const CONTINUATIONS: &[char] = &[
    'が', 'け', 'の', 'か', 'た', 'な', 'し', 'よ', 'ね', 'わ', 'ぞ', 'ぜ', 'ま', 'で', 'だ',
];

/// Characters that cannot begin a chunk, because they are not standalone: small kana
/// (`ょ` in `ましょう`), the geminate `っ`, the long-vowel mark `ー`, and syllabic `ん`.
///
/// A partial grows one character at a time, so a marker is routinely the *last* character
/// of an incomplete mora: `過ごしましょう` passes through `過ごしまし`, where the ambiguous
/// Tier C `し` is followed by `ょ`. Cutting there produced `過ごしまし` | `ょう`. No cut may
/// land immediately before one of these, whatever tier claims the position.
const DEPENDENT_CHARS: &[char] = &[
    'ゃ', 'ゅ', 'ょ', 'ゎ', 'ぁ', 'ぃ', 'ぅ', 'ぇ', 'ぉ', 'っ', 'ん',
    'ャ', 'ュ', 'ョ', 'ヮ', 'ァ', 'ィ', 'ゥ', 'ェ', 'ォ', 'ッ', 'ン',
    'ー', '々', '〜', '゛', '゜',
];

/// Punctuation that belongs to the clause it follows, pulled into the chunk being cut so
/// the next chunk does not start with a dangling `、`.
const TRAILING_PUNCT: &[char] = &['、', '，', ',', '・', '。', '．'];

fn ends_with(tail: &[char], marker: &str) -> bool {
    let m: Vec<char> = marker.chars().collect();
    tail.len() >= m.len() && tail[tail.len() - m.len()..] == m[..]
}

fn starts_with(rest: &[char], marker: &str) -> bool {
    let m: Vec<char> = marker.chars().collect();
    rest.len() >= m.len() && rest[..m.len()] == m[..]
}

/// What a scan position turned out to be.
enum Verdict {
    /// Cut here (exclusive index into the tail).
    Cut(usize),
    /// A marker is sitting at the very end of the tail and one more token is needed to tell
    /// whether it closes the clause (`ます` vs `ますが`, `て` vs `ている`). Wait for the next
    /// partial. Nothing later in the tail can match either, so scanning stops.
    NeedLookahead,
    /// Not a boundary; keep scanning.
    No,
}

/// Pulls trailing clause punctuation into the chunk ending at `i`.
fn absorb_punct(tail: &[char], mut i: usize) -> usize {
    while i < tail.len() && TRAILING_PUNCT.contains(&tail[i]) {
        i += 1;
    }
    i
}

/// Decides whether a cut belongs immediately after `tail[i - 1]`.
///
/// `at_final` relaxes the lookahead rules: on a real ASR final there is no "next partial"
/// to wait for, so a marker at the end of the tail is as much evidence as we will ever get.
fn verdict_at(tail: &[char], i: usize, at_final: bool) -> Verdict {
    debug_assert!(i > 0 && i <= tail.len());
    let head = &tail[..i];
    let rest = &tail[i..];
    let prev = tail[i - 1];

    // A chunk may never begin with a character that is not standalone — the position is
    // inside a mora, not between two of them.
    if rest.first().is_some_and(|c| DEPENDENT_CHARS.contains(c)) {
        return Verdict::No;
    }

    // Tier A — unambiguous. Punctuation never needs lookahead.
    if TIER_A.contains(&prev) {
        return Verdict::Cut(absorb_punct(tail, i));
    }

    // Tier B — polite / copula endings. Before Tier D, since `です`/`でした`/`でしょう`
    // all contain `で`.
    for marker in TIER_B {
        if ends_with(head, marker) {
            return match classify_following(rest, at_final) {
                Following::Continues => Verdict::No,
                Following::Unknown => Verdict::NeedLookahead,
                Following::Ends => Verdict::Cut(absorb_punct(tail, i)),
            };
        }
    }

    // Tier C — conjunctive particles.
    for marker in TIER_C {
        if !ends_with(head, marker) {
            continue;
        }
        if TIER_C_AMBIGUOUS.contains(marker) {
            // Needs a following token to be a clause-final "but"/"and" rather than a case
            // marker or a word ending, and needs the clause so far to be substantial.
            if rest.is_empty() {
                return if at_final { Verdict::No } else { Verdict::NeedLookahead };
            }
            // Kept as the constant, not the tunable guard: this is a linguistic floor on
            // how much clause has to precede an ambiguous marker before it can be read as
            // clause-final, which is a separate question from how short a chunk may be.
            if i < MIN_CHUNK_CHARS {
                return Verdict::No;
            }
        }
        return match classify_following(rest, at_final) {
            Following::Continues => Verdict::No,
            Following::Unknown => Verdict::NeedLookahead,
            Following::Ends => Verdict::Cut(absorb_punct(tail, i)),
        };
    }

    // Tier D — te-form. Needs exactly one token of lookahead.
    if prev == 'て' || prev == 'で' {
        if rest.is_empty() {
            // Can't tell `見て` (clause end) from `見ている` (predicate still coming).
            return if at_final { Verdict::No } else { Verdict::NeedLookahead };
        }
        if TE_FORM_CONTINUES.contains(&rest[0]) {
            return Verdict::No;
        }
        if TE_FORM_FALSE_POSITIVES.iter().any(|w| ends_with(head, w)) {
            return Verdict::No;
        }
        // `で` that opens a Tier B ending (`です`, `でした`, `でしょう`, `である`) is not a
        // te-form at all — let the Tier B check at the later position make the cut.
        let from_prev = &tail[i - 1..];
        if TIER_B.iter().any(|m| starts_with(from_prev, m)) {
            return Verdict::No;
        }
        return Verdict::Cut(absorb_punct(tail, i));
    }

    // Tier E — sentence-final particles, only as the last character of the tail, and only
    // on a real final (see TIER_E_MULTI for why mid-partial is unsafe).
    if at_final && rest.is_empty() {
        if TIER_E_MULTI.iter().any(|m| ends_with(head, m)) || TIER_E_SINGLE.contains(&prev) {
            return Verdict::Cut(i);
        }
    }

    Verdict::No
}

enum Following {
    /// A character that can extend the clause follows — this is not the boundary.
    Continues,
    /// Nothing follows yet and more audio may still arrive.
    Unknown,
    /// Something follows that cannot extend the clause, so the clause ended here.
    Ends,
}

fn classify_following(rest: &[char], at_final: bool) -> Following {
    match rest.first() {
        None if at_final => Following::Ends,
        None => Following::Unknown,
        Some(c) if CONTINUATIONS.contains(c) => Following::Continues,
        Some(_) => Following::Ends,
    }
}

/// The earliest real clause boundary at or after `MIN_CHUNK_CHARS`.
fn find_boundary(tail: &[char], at_final: bool, min_chunk_chars: usize) -> Option<usize> {
    for i in min_chunk_chars..=tail.len() {
        match verdict_at(tail, i, at_final) {
            Verdict::Cut(cut) => return Some(cut),
            Verdict::NeedLookahead => return None,
            Verdict::No => {}
        }
    }
    None
}

/// Where to cut when a guard (length ceiling or wait timeout) forces one: the last marker
/// of any tier within `limit`, falling back to `limit` itself. Lookahead rules are ignored
/// here — we are out of time either way, and ending on a marker still beats ending
/// mid-predicate.
fn find_forced_cut(tail: &[char], limit: usize, min_chunk_chars: usize) -> usize {
    let limit = limit.min(tail.len());
    for i in (min_chunk_chars..=limit).rev() {
        if let Verdict::Cut(cut) = verdict_at(&tail[..limit], i, true) {
            return cut;
        }
    }
    limit
}

/// The three tuning guards, as values rather than constants so a sweep can measure them
/// instead of arguing about them. Production uses `Guards::default()`, which is exactly the
/// constants above.
#[derive(Debug, Clone, Copy)]
pub struct Guards {
    pub min_chunk_chars: usize,
    pub max_chunk_chars: usize,
    pub max_wait_ms: u64,
}

impl Default for Guards {
    fn default() -> Self {
        Self {
            min_chunk_chars: DEFAULT_MIN_CHUNK_CHARS,
            max_chunk_chars: MAX_CHUNK_CHARS,
            max_wait_ms: MAX_WAIT_MS,
        }
    }
}

pub struct JapaneseChunker {
    guards: Guards,
    /// The de-spaced text of the current utterance, as chars.
    text: Vec<char>,
    /// How many of `text` have been emitted.
    consumed: usize,
    /// When the last chunk was emitted — the clock for `MAX_WAIT_MS`.
    last_emit: Instant,
    /// Counts partials where Vosk revised text we had already emitted. Nothing can be
    /// un-emitted; Phase 3's re-translating live line is what actually covers this, so this
    /// is here to be observable, not acted on.
    pub revisions: usize,
}

impl JapaneseChunker {
    pub fn new() -> Self {
        Self::with_guards(Guards::default())
    }

    pub fn with_guards(guards: Guards) -> Self {
        Self { guards, text: Vec::new(), consumed: 0, last_emit: Instant::now(), revisions: 0 }
    }

    /// The unconsumed tail, empty when a revised partial came back shorter than what has
    /// already been emitted.
    fn tail_slice(&self) -> &[char] {
        self.text.get(self.consumed..).unwrap_or(&[])
    }

    /// `push_partial` with an injectable clock, so the `MAX_WAIT_MS` guard is testable.
    pub fn push_partial_at(&mut self, partial: &str, now: Instant) -> Vec<Chunk> {
        let despaced: Vec<char> = partial.split_whitespace().flat_map(|w| w.chars()).collect();

        // Vosk re-ranks partials, so the new text may not extend what we already emitted.
        // `consumed` is therefore treated as "this many characters have already been shown
        // to the user", never as a prefix that is still believed to match: it is only ever
        // advanced by an emit. Re-deriving it from the text would re-emit words the user
        // has already read, which is a worse failure than leaving a revised prefix
        // uncorrected — and the live line (which re-translates the whole tail) is what
        // actually keeps the visible text honest.
        if despaced.len() < self.consumed
            || despaced[..self.consumed.min(despaced.len())]
                != self.text[..self.consumed.min(self.text.len())]
        {
            if !self.text.is_empty() {
                self.revisions += 1;
            }
        }
        self.text = despaced;

        let mut out = Vec::new();
        loop {
            let tail = self.tail_slice();
            if tail.len() < self.guards.min_chunk_chars {
                break;
            }
            if let Some(cut) = find_boundary(tail, false, self.guards.min_chunk_chars) {
                out.push(Chunk::confident(tail[..cut].iter().collect::<String>()));
                self.consumed += cut;
                self.last_emit = now;
                continue;
            }
            // No real boundary. Fall through to the guards.
            if tail.len() >= self.guards.max_chunk_chars {
                let cut = find_forced_cut(tail, self.guards.max_chunk_chars, self.guards.min_chunk_chars);
                out.push(Chunk::forced(tail[..cut].iter().collect::<String>()));
                self.consumed += cut;
                self.last_emit = now;
                continue;
            }
            if now.duration_since(self.last_emit) >= Duration::from_millis(self.guards.max_wait_ms) {
                let cut = find_forced_cut(tail, tail.len(), self.guards.min_chunk_chars);
                out.push(Chunk::forced(tail[..cut].iter().collect::<String>()));
                self.consumed += cut;
                self.last_emit = now;
                continue;
            }
            break;
        }
        out
    }
}

impl Default for JapaneseChunker {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkStrategy for JapaneseChunker {
    fn push_partial(&mut self, partial: &str) -> Vec<Chunk> {
        self.push_partial_at(partial, Instant::now())
    }

    /// Splits the final text on every boundary it can find — a `Final` can hold several
    /// sentences — and emits whatever is left as the last chunk.
    fn flush(&mut self, final_text: &str) -> Vec<Chunk> {
        let despaced: Vec<char> =
            final_text.split_whitespace().flat_map(|w| w.chars()).collect();
        // Skip exactly as many characters as have already been emitted. Checking whether
        // the final's prefix still matches and resetting to 0 when it does not — which is
        // what this did first — re-emitted the whole utterance every time Vosk re-ranked a
        // single character of it. On a 7.7-minute sample that produced 224 chunks for ~125
        // utterances, with whole sentences translated and shown twice.
        let consumed = self.consumed.min(despaced.len());
        if despaced[..consumed] != self.text[..consumed.min(self.text.len())] {
            self.revisions += 1;
        }
        let mut consumed = consumed;

        let mut out = Vec::new();
        while consumed < despaced.len() {
            let tail = &despaced[consumed..];
            match find_boundary(tail, true, self.guards.min_chunk_chars) {
                Some(cut) if cut < tail.len() => {
                    out.push(Chunk::confident(tail[..cut].iter().collect::<String>()));
                    consumed += cut;
                }
                _ => {
                    out.push(Chunk::confident(tail.iter().collect::<String>()));
                    break;
                }
            }
        }
        self.reset();
        out
    }

    fn pending_tail(&self) -> String {
        self.tail_slice().iter().collect()
    }

    fn reset(&mut self) {
        self.text.clear();
        self.consumed = 0;
        self.last_emit = Instant::now();
        // `revisions` deliberately survives a reset — it is a session-level diagnostic.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guards the *rule* tests run under. Boundary-rule behaviour is a separate
    /// question from how short a chunk may be, and the shipped minimum (50) is far longer
    /// than the textbook sentences those rules are stated in terms of, so pinning the
    /// minimum to the linguistic floor keeps each test about one thing.
    fn rule_guards() -> Guards {
        Guards { min_chunk_chars: MIN_CHUNK_CHARS, ..Guards::default() }
    }

    /// Feeds the whole string as one partial, then flushes — the shape most of the table
    /// cases care about.
    fn chunks(input: &str) -> Vec<Chunk> {
        let mut c = JapaneseChunker::with_guards(rule_guards());
        let mut out = c.push_partial(input);
        out.extend(c.flush(input));
        out
    }

    #[test]
    fn shipped_guards_are_the_measured_ones() {
        // Guards against a well-meaning "6 chars is surely enough" edit. The table in the
        // DEFAULT_MIN_CHUNK_CHARS docs is why these are what they are.
        let g = Guards::default();
        assert_eq!((g.min_chunk_chars, g.max_chunk_chars, g.max_wait_ms), (50, 120, 4000));
    }

    fn texts(input: &str) -> Vec<String> {
        chunks(input).into_iter().map(|c| c.text).collect()
    }

    /// Feeds the string one character at a time, the way Vosk grows a partial.
    fn chunks_streamed(input: &str) -> Vec<Chunk> {
        let all: Vec<char> = input.chars().collect();
        let mut c = JapaneseChunker::with_guards(rule_guards());
        let mut out = Vec::new();
        for n in 1..=all.len() {
            let partial: String = all[..n].iter().collect();
            out.extend(c.push_partial(&partial));
        }
        out.extend(c.flush(input));
        out
    }

    #[test]
    fn whole_sentence_with_terminal_is_one_chunk() {
        assert_eq!(texts("昨日友達と映画を見に行こうと思った。"), vec!["昨日友達と映画を見に行こうと思った。"]);
    }

    #[test]
    fn cuts_after_kara() {
        assert_eq!(texts("映画を見たから楽しかった"), vec!["映画を見たから", "楽しかった"]);
    }

    #[test]
    fn no_cut_at_te_before_iru() {
        // `見ている` — the predicate has not landed, so `て` is not a boundary. The only cut
        // is after the closing `です`.
        assert_eq!(texts("今テレビを見ているところです"), vec!["今テレビを見ているところです"]);
    }

    #[test]
    fn no_cut_at_bare_ga_below_min_chars() {
        assert_eq!(texts("彼が来た"), vec!["彼が来た"]);
    }

    #[test]
    fn cuts_after_clause_final_ga() {
        assert_eq!(
            texts("それは面白いと思いますが、やめておきます"),
            vec!["それは面白いと思いますが、", "やめておきます"]
        );
    }

    #[test]
    fn never_cuts_before_a_dependent_character() {
        // `過ごしましょう` passes through `過ごしまし` as the partial grows, where the
        // ambiguous `し` is followed by the small kana `ょ`.
        assert_eq!(
            texts("健康 で 元気 に 仲良く 過ごし ましょう"),
            vec!["健康で元気に仲良く過ごしましょう"]
        );
        // Same for the geminate `っ` and syllabic `ん`.
        assert_eq!(texts("家賃がすごく安かった"), vec!["家賃がすごく安かった"]);
    }

    #[test]
    fn cuts_after_mashita() {
        assert_eq!(texts("わかりました"), vec!["わかりました"]);
    }

    #[test]
    fn no_cut_after_a_clause_initial_connective() {
        // `だって` ends in `て` but introduces the clause after it, so it must not be left
        // dangling at the end of the previous chunk.
        assert_eq!(texts("ございますよだってねそうですよね").len(), 1);
    }

    #[test]
    fn te_form_cut_when_predicate_landed() {
        // `食べて` followed by `家` — a real te-form clause break.
        assert_eq!(texts("朝ご飯を食べて家を出ました"), vec!["朝ご飯を食べて", "家を出ました"]);
    }

    #[test]
    fn forced_cut_at_max_chars_is_not_confident() {
        // A long run of characters with no marker anywhere.
        let long: String = "ア".repeat(MAX_CHUNK_CHARS * 2);
        let mut c = JapaneseChunker::with_guards(rule_guards());
        let out = c.push_partial(&long);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|ch| !ch.boundary_confident));
        assert_eq!(out[0].text.chars().count(), MAX_CHUNK_CHARS);
    }

    #[test]
    fn wait_timeout_forces_a_flush() {
        let mut c = JapaneseChunker::with_guards(rule_guards());
        let t0 = Instant::now();
        // Short, no boundary, well under the char ceiling.
        assert!(c.push_partial_at("そのあたりのこと", t0).is_empty());
        let out = c.push_partial_at(
            "そのあたりのこと",
            t0 + Duration::from_millis(MAX_WAIT_MS + 1),
        );
        assert_eq!(out.len(), 1);
        assert!(!out[0].boundary_confident);
        assert_eq!(out[0].text, "そのあたりのこと");
    }

    #[test]
    fn despaces_vosk_morpheme_spacing() {
        // Exactly the shape Vosk JA emits: morpheme spaces that are not Japanese
        // orthography. `で す` must still match the `です` ending, and `か ？` the terminal.
        // `、` is deliberately not a hard terminal, so the greeting stays attached.
        assert_eq!(texts("こんにちは 、 元気 です か ？"), vec!["こんにちは、元気ですか？"]);
        // Same string unspaced must chunk identically.
        assert_eq!(texts("こんにちは、元気ですか？"), texts("こんにちは 、 元気 です か ？"));
    }

    #[test]
    fn streaming_one_char_at_a_time_matches_whole_input() {
        for input in [
            "映画を見たから楽しかった",
            "今テレビを見ているところです",
            "それは面白いと思いますが、やめておきます",
            "朝ご飯を食べて家を出ました",
        ] {
            let streamed: Vec<String> =
                chunks_streamed(input).into_iter().map(|c| c.text).collect();
            assert_eq!(streamed.concat(), input.to_string(), "lossy on {input}");
            assert_eq!(streamed, texts(input), "stream/batch disagree on {input}");
        }
    }

    #[test]
    fn flush_splits_a_multi_sentence_final() {
        assert_eq!(
            texts("おはようございます。今日はいい天気ですね"),
            vec!["おはようございます。", "今日はいい天気ですね"]
        );
    }

    #[test]
    fn pending_tail_is_the_unconsumed_remainder() {
        let mut c = JapaneseChunker::with_guards(rule_guards());
        c.push_partial("映画を見たから楽しか");
        assert_eq!(c.pending_tail(), "楽しか");
    }

    #[test]
    fn revised_partial_is_counted_and_does_not_panic() {
        let mut c = JapaneseChunker::with_guards(rule_guards());
        c.push_partial("映画を見たから楽しかった");
        // Vosk re-ranks the prefix it already gave us.
        c.push_partial("映画を見てから楽しかった");
        assert_eq!(c.revisions, 1);
    }

    #[test]
    fn a_revised_final_never_re_emits_what_was_already_shown() {
        // The failure this guards against, measured on a 7.7-minute sample: Vosk re-ranks
        // one character of the utterance on `Final`, the prefix no longer matches, and the
        // entire utterance is emitted a second time — translated twice and shown twice.
        let mut c = JapaneseChunker::with_guards(rule_guards());
        let emitted = c.push_partial("映画を見たから楽しかったです");
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].text, "映画を見たから");

        // `見た` -> `見て`: same length, different characters, inside the part already sent.
        let rest = c.flush("映画を見てから楽しかったです");
        assert_eq!(
            rest.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["楽しかったです"],
            "flush re-emitted text the user had already read"
        );
    }

    #[test]
    fn a_shorter_revised_partial_emits_nothing_new() {
        let mut c = JapaneseChunker::with_guards(rule_guards());
        assert_eq!(c.push_partial("映画を見たから楽しかった").len(), 1);
        // Vosk shrinks the partial below what has already been emitted.
        assert!(c.push_partial("映画を").is_empty());
        assert_eq!(c.pending_tail(), "");
    }

    #[test]
    fn de_inside_dekimasu_is_not_a_te_form() {
        // `見ることができます` was cut `...ことがで` | `きます`.
        assert_eq!(
            texts("上から横浜の街を見ることができます"),
            vec!["上から横浜の街を見ることができます"]
        );
        // `〜ても` / `〜では` continue the clause too.
        assert_eq!(texts("飛んではいないですね"), vec!["飛んではいないですね"]);
    }

    #[test]
    fn nothing_is_ever_lost() {
        // Whatever the cut points, the chunks must reassemble into the de-spaced input.
        for input in [
            "ございます よ だって ね マート そう です よ ね ちょっと 地方 に 行っ たら 家賃 も 本当 に 安く なる し 大きい アパート に 住め ます から ね 本当 に そう です ね",
            "健康 で 元気 に 仲良く 過ごし ましょう",
            "今年 の 抱負 は 何 です か",
            "小 学校 です",
        ] {
            let despaced: String = input.split_whitespace().collect();
            assert_eq!(texts(input).concat(), despaced, "lossy on {input}");
        }
    }
}

#[cfg(test)]
mod sample_dump {
    use super::tests_support::*;

    /// Prints the chunking of real Vosk-shaped JA partials. Not an assertion — run with
    /// `cargo test -- --ignored --nocapture sample_dump` when tuning the three constants.
    #[test]
    #[ignore]
    fn dump() {
        for input in SAMPLES {
            println!("IN : {input}");
            for c in chunk_all(input) {
                println!("  -> {:?} confident={}", c.text, c.boundary_confident);
            }
        }
    }
}

#[cfg(test)]
pub mod tests_support {
    use super::{Chunk, ChunkStrategy, JapaneseChunker};

    pub const SAMPLES: &[&str] = &[
        "ございます よ だって ね マート そう です よ ね ちょっと 地方 に 行っ たら 家賃 も 本当 に 安く なる し 大きい アパート に 住め ます から ね 本当 に そう です ね",
        "健康 で 元気 に 仲良く 過ごし ましょう",
        "今年 の 抱負 は 何 です か",
        "そんな 残念 だ なぁ と",
        "昨日 友達 と 映画 を 見 に 行こ う と 思っ た 。",
    ];

    /// Streams the input one character at a time (as Vosk grows a partial) then flushes.
    pub fn chunk_all(input: &str) -> Vec<Chunk> {
        let despaced: String = input.split_whitespace().collect();
        let all: Vec<char> = despaced.chars().collect();
        let mut c = JapaneseChunker::with_guards(super::Guards {
            min_chunk_chars: super::MIN_CHUNK_CHARS,
            ..Default::default()
        });
        let mut out = Vec::new();
        for n in 1..=all.len() {
            let partial: String = all[..n].iter().collect();
            out.extend(c.push_partial(&partial));
        }
        out.extend(c.flush(&despaced));
        out
    }
}

