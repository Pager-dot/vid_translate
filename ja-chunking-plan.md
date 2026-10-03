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

## 4. Whisper: evaluated, and it wins — so this is now an integration problem

Measured, not predicted. `whisper.cpp` v1.9.4, `ggml-small`, `-l ja`, with its Japanese fed
through the *same* chunker and the *same* int8 model so the recognizer is the only variable:

| clip | Vosk → MT | Whisper → MT |
|---|---|---|
| vlog — 1 speaker, 7.7 min, quiet | BLEU 18.13 / chrF 53.40 | **26.35 / 58.57** |
| family — multi-speaker, 36 min, noisy | BLEU 5.03 / chrF 38.65 | **14.15 / 54.95** |
| CER vs reference Japanese (vlog) | 20.8% | **15.4%** |

The multi-speaker clip is the decisive one. Vosk does not merely do worse there, it
**collapses**: BLEU 5 is not a usable translation, and it emitted 2768 English words against
a 4244-word reference, silently dropping about a third of the speech. That is why no amount
of chunker or model work ever moved the needle. Whisper degrades gracefully instead.

Speed: 36 min of audio in 5:43 wall on Apple Silicon at `small` (~6.3x realtime). Real-time
feasible here; **unmeasured on Windows and Linux**, which is a shipping blocker, not a
detail.

> **Keep in consideration — `-mc 0` is mandatory.** Whisper's default context carry-over
> sends it into repetition loops: on the 36-minute clip, runs of 260 and 258 identical
> segments, 62% of all segments duplicated, BLEU 10.02. `-mc 0` fixes it (5.3%, BLEU 14.15).
> Ship without it and the caption bar chants one sentence for four minutes.

### 4.1 The design decision to make first

Whisper is **not streaming.** It emits finished, punctuated segments; Vosk emits a growing,
revised partial. The `ChunkStrategy` contract and the Phase 3 self-correcting live line are
both built on the latter. Pick one, deliberately, before writing code:

- **Sliding window, synthesised partials.** Keeps the live line and the self-correction.
  Costs: re-transcribing overlapping audio continuously, and partials that revise far more
  aggressively than Vosk's — which the `consumed`-never-rewinds rule in `chunker::japanese`
  already handles, but which would need re-measuring.
- **Segment-at-a-time.** Much simpler, and whisper's segments are already clean sentences.
  Costs ~1–2s added latency and **loses the live line you just built**.

A plausible third option: Whisper for the committed history line, Vosk kept alive purely to
drive the low-latency live line. Two recognizers running is more CPU, but it is the only
shape that keeps both the accuracy and the responsiveness.

### 4.2 What the clause chunker becomes

With punctuated input, most of `chunker::japanese` stops being load-bearing:

- Tier A (hard terminals) becomes the primary signal instead of incidental.
- Tiers B–E and the de-spacing exist to compensate for Vosk's unpunctuated,
  morpheme-spaced output. **Do not delete them while Vosk still serves ES/EN or the live
  line**, but they should stop being the thing that gets tuned.
- Sentence-merging Whisper's segments was a wash on quality (BLEU 14.15 → 14.31) but halved
  the MT calls (851 → 428). Worth doing for cost, not for quality.
- The guard table in section 1 was measured against Vosk output and **does not transfer.**
  Notably, Whisper showed none of the strong context-sensitivity Vosk did, because its
  segments already end at natural pauses rather than mid-clause. Re-sweep before assuming
  anything.

### 4.3 Model size and packaging

`ggml-small` is 487MB against Vosk JA's 48MB, on top of a download/bundling story that is
already known-bad. `medium`/`large-v3-turbo` were not tested — `small` already wins
decisively, so test larger models only if `small` proves inadequate in the app rather than
on principle. Keep Vosk for ES and EN; only JA has the error rate that justifies any of
this.

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

- No fine-tuning. Still deferred, and now clearly premature: the recognizer is the binding
  constraint — on multi-speaker audio it loses a third of the speech outright — so fine-tuning
  the translator optimises the wrong stage. Revisit only after Whisper lands.
- Do not change ES behaviour. It is byte-identical and fuzz-verified; keep it that way.
- Do not change the model download/bundling story as part of any of the above (known
  separate issue, which 4.3 will nonetheless collide with).
