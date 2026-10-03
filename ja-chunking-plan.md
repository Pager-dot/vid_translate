# vid_translate — Japanese pipeline: status and next plan

Supersedes the original "Fix Japanese chunking (no fine-tuning)" plan. That plan's phases
0–5 are done; its central hypothesis turned out to be wrong, and the measurements that
disproved it are what this document is mostly about. Full numbers live in
`docs/ja-diagnosis.md`; this is the decision-level summary and what to do next.

---

## 1. The original hypothesis was wrong, twice over

**It assumed JA was being translated every 8 words off the growing Vosk partial**, and that
an 8-word prefix of an SOV sentence rarely contains a predicate. The code did not do that:
`run_translated_pipeline` guarded the eager-chunking branch with `if !is_ja`, so Japanese
only ever translated a whole, stable Vosk `Final`, de-spaced, in one call. (The model is
also `staka/fugumt-ja-en`, not `opus-mt-ja-en`.) Short-chunk damage was never happening.

**It then assumed clause-aware chunking would improve quality.** Measured on a 7.7-minute
Japanese vlog against a human transcript, the opposite is true — quality degrades
monotonically with the number of cuts:

| min chunk chars | chunks | BLEU | chrF |
|---|---|---|---|
| 6 (the plan's value) | 165 | 13.31 | 51.59 |
| 20 | 107 | 16.57 | 52.14 |
| **50 (shipped)** | **73** | **18.40** | **53.80** |
| never cut (translate on `Final` only) | 65 | 18.11 | 52.76 |

Each chunk is translated with no knowledge of its neighbours, and the model's fluency comes
from seeing a whole sentence. Clause boundaries beat word counts — that part of the plan
held — but **the best place to cut is nowhere.** The guards therefore ship at the point where
quality reaches parity with not cutting at all (50 / 120 / 4000).

> **Keep in consideration:** Phase 2 is a **latency** feature. It stops a long sentence
> spoken in one breath from waiting for a pause, and nothing more. It is not a quality fix
> and must not be defended as one. Anyone lowering `DEFAULT_MIN_CHUNK_CHARS` on intuition is
> trading measurable quality for responsiveness; the table above is the exchange rate, and
> `shipped_guards_are_the_measured_ones` is the test that will complain.

---

## 2. Where the quality is actually lost

Phase 0's real answer: **ASR dominates.**

| | |
|---|---|
| Vosk JA character error rate vs. reference | **20.8%** |
| utterances good (CER ≤ 15%) | 52.8% |
| partly wrong | 27.2% |
| unusable | 20.0% |

Representative failures — none of them chunking or model problems:

| reference | Vosk heard | output |
|---|---|---|
| はい、皆さんこんにちは。 | パイ皆さん今日は我がも飽きて参っちゃうねよ | "pie, i'm getting tired of you today" |
| すごい、これ初めて見た。 | ２５これ初めて見た | "25 for the first time" |
| あそこから、下が見えるんですね。 | 見たアソコから | "from the sight of a scorpion" |

Where the Japanese is recognised correctly the model is largely fine. One character in five
is wrong before translation begins, and nothing downstream recovers it.

---

## 3. What shipped

Twelve commits, each independently revertable.

| | |
|---|---|
| `debug` | ASR / MT / chunk JSONL dumps behind `VID_TRANSLATE_DEBUG_ASR=1` |
| `chunker` | `ChunkStrategy` trait; ES ported verbatim, proven by a 2000-case equivalence fuzz against the original inline algorithm |
| `chunker::japanese` | clause-boundary strategy, Tiers A–E, cutting *after* the marker |
| `mt` | re-translating in-progress tail; the live line replaces itself and self-corrects |
| `mt` | `VID_TRANSLATE_{JA,ES}_MODEL_DIR` override for model A/B |
| `eval` | `ja_eval` offline runner + `eval/run.sh` + BLEU/chrF/translate-time scoring |

Three bugs were found only by running real audio, none reachable from the textbook sentences
the unit tests were written against:

1. **Every revised `Final` re-emitted the whole utterance** — 224 chunks for 125 utterances,
   sentences translated and displayed twice, 23 duplicates in 7.7 minutes. This was the
   largest visible contributor to "the translations are wrong".
2. **`で` inside `できます` was cut as a te-form** — `見ることができます` became two
   meaningless fragments.
3. **`ja_eval` broke `tauri dev` entirely** (ambiguous `cargo run`), and the harness reported
   ~20s of offline queue backlog as if it were user-facing latency.

> **Keep in consideration:** the unit tests were all written from invented sentences and all
> passed while three real bugs sat in the code. Any further chunker work should be validated
> against `eval/out` on real audio before it is believed.

---

## 4. Next: whisper.cpp evaluation (the only lever that matters now)

ASR is the ceiling, so this is the highest-value work remaining. It is a bigger change than
everything above combined, and it is a *decision*, not just an implementation.

### 4.1 Measure before committing to anything

Do not swap the recognizer first. Transcribe the same `eval/ja` corpus offline with
`whisper.cpp -l ja` at a few model sizes and score the Japanese against the reference:

```sh
./whisper-cli -m models/ggml-small.bin -l ja -f clip.wav -otxt
# then the same CER alignment used in docs/ja-diagnosis.md
```

Target to beat: **20.8% CER**. Expect `small` or `medium` to roughly halve it; `tiny` may not
beat Vosk at all. If the win is under ~5 points absolute, stop — it will not justify the
cost in 4.3.

### 4.2 The second prize: punctuation

Whisper emits `。`, `、`, `？`. The chunker's Tier A (hard terminals) is currently incidental
because Vosk's punctuation is unreliable; with Whisper it becomes the primary signal, and
`fugumt`/`opus-mt` were trained on punctuated text. Two consequences worth planning for:

- The guard table in section 1 **must be re-swept.** It was measured against unpunctuated,
  morpheme-spaced Vosk output. Real sentence boundaries may well move the optimum back down,
  which would make clause chunking a quality win after all — the one way this plan's original
  hypothesis could still come true.
- The de-spacing in `chunker::japanese` is a Vosk artefact workaround. With Whisper it
  becomes a no-op, not a bug, but say so in the module docs rather than deleting it while
  both recognizers are in play.

### 4.3 What the swap actually costs

Be honest about this before starting, because it is where the work is:

- **Not streaming.** Whisper processes windows, not a growing partial. Vosk's `Partial` /
  `Final` contract is what `ChunkStrategy` and the Phase 3 live line are both built on.
  Either run Whisper on a sliding window and synthesise partials, or accept ~1–2s of added
  latency and lose the self-correcting live line. **This is the main design decision, and it
  should be made before any code is written.**
- **Model size.** `small` is ~500MB vs Vosk JA's 48MB, against an existing known-bad
  download/bundling story.
- **CPU.** Real-time on Apple Silicon at `small`; needs measuring on the Windows/Linux
  targets before it can ship.
- Keep Vosk for ES and EN. Only JA has the error rate that justifies this.

---

## 5. Also open

- **Phase 4 — fp32 A/B.** Cheap, wired (`VID_TRANSLATE_JA_MODEL_DIR`), unrun. int8 degrades
  lower-resource pairs more, and ja-en is the weaker of the two. Baseline to beat: **BLEU
  18.13 / chrF 53.40**. Needs `ct2-transformers-converter`. Do this before the Whisper work —
  it is an afternoon, not a week.
- **Real end-to-end latency is still unmeasured.** `ja_eval` feeds the clip to the recognizer
  all at once, so cut-to-shown there measures the burst backlog, not experience. Per-chunk
  translate time is p95 164ms, so translation is not the constraint; the number that matters
  needs the audio paced in real time. Add a `--realtime` mode before claiming any latency
  figure.
- **Eval corpus is one clip.** The vlog is a single speaker in a quiet-ish setting. BLEU on
  it is a regression tripwire with a ±0.3 noise floor (Vosk itself is not quite deterministic
  run to run), not a leaderboard. Add 10–20 more clips, including noisy and multi-speaker,
  before trusting any comparison finer than a few points.
- **Ollama JA still uses `FinalOnlyChunker`.** The clause chunker and the re-translating live
  line are local-only, because re-translating a tail every 300ms over HTTP is not affordable.
  Fine as-is; just do not assume the two paths behave alike when debugging.

---

## 6. Non-goals, unchanged

- No fine-tuning. Still deferred, and now clearly premature: with 20.8% ASR CER, fine-tuning
  the translator optimises the wrong stage.
- Do not change ES behaviour. It is byte-identical and fuzz-verified; keep it that way.
- Do not change the model download/bundling story as part of any of the above (known
  separate issue, which 4.3 will nonetheless collide with).
