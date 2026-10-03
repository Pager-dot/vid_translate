# JA→EN diagnosis

Measured on a 7.7-minute Japanese vlog (Yokohama / Minato Mirai walkaround) with a
timestamped human transcript carrying both the Japanese and a reference English
translation — 125 utterances, 2115 reference characters, 977 reference English words. Run
offline through the real pipeline with `ja_eval`, so Vosk, the chunker and the int8
CTranslate2 model are all the shipped ones.

---

## Correction to the premise

The plan behind this work assumed Japanese was being translated "every 8 new words" off the
growing Vosk partial, and that an 8-word prefix of an SOV sentence rarely contains a
predicate. That was true of the design at one point but **not of the code as it stood**:
`run_translated_pipeline` guarded the eager-chunking branch with `if !is_ja`, so Japanese
only ever translated a whole, stable Vosk `Final`, de-spaced, in one call. The model is
`staka/fugumt-ja-en`, not `opus-mt-ja-en`.

Short-chunk damage was therefore not the cause of the garbling. The measurements below say
what is.

---

## Phase 0 verdict: ASR is the dominant problem — but it clears the gate

Reference Japanese and recognizer output were aligned as two character streams, then scored
per reference utterance.

| | |
|---|---|
| **Stream-level character error rate** | **20.8%** |
| good (CER ≤ 15%) | 66 / 125 — 52.8% |
| partly wrong (15–45%) | 34 / 125 — 27.2% |
| unusable (> 45%) | 25 / 125 — 20.0% |

**Verdict: both, with ASR dominant. The gate is passed** — the threshold for stopping was
~40% unusable and this is 20%, some of which is alignment artefact rather than
misrecognition (a reference utterance whose aligned span bleeds into its neighbour scores
badly without the ASR being wrong). So the chunker work was worth finishing. But at 20.8%
CER, roughly one character in five is wrong before the translator ever sees the text, and no
amount of chunking or model work recovers meaning that was never recognised.

What the failures look like:

| reference | what Vosk heard | resulting English |
|---|---|---|
| はい、皆さんこんにちは。 | パイ皆さん今日は我がも飽きて参っちゃうねよ | "pie, i'm getting tired of you today" |
| いらっしゃいませ。画面のボタンをお選びください。 | なぁ | — |
| で、クレジットカード、現金、 | クレジットカド現金ICカドありますけれども | (long vowels dropped) |
| すごい、これ初めて見た。 | ２５これ初めて見た | "25 for the first time" |
| あそこから、下が見えるんですね。 | 見たアソコから | "from the sight of a scorpion" |

These are not chunking failures and not model failures. `はい` → `パイ` and `すごい` → `２５`
are the recognizer. The honest next step for quality is **evaluating whisper.cpp with
`-l ja`**, which would also supply the sentence punctuation this family of models was
trained to expect — and which would make Tier A of the chunker load-bearing instead of
incidental.

Where the Japanese *was* recognised correctly, the model is largely fine:
`横浜のみなとみらいをぶらぶらしたいと思います` → "I want to hang out in Yokohama",
`観覧車とかジェットコースターもあります` → "and the Ferris wheel and the roller coaster".

---

## The chunking finding: cutting Japanese into clauses makes the translation worse

This is the opposite of what the plan predicted, and it is the most important result here.
Document-level BLEU and chrF against the reference English, sweeping the minimum chunk size:

| min chars | chunks | BLEU | chrF |
|---|---|---|---|
| 6 (the plan's value) | 165 | 13.31 | 51.59 |
| 12 | 134 | 15.75 | 52.73 |
| 20 | 107 | 16.57 | 52.14 |
| 30 | 88 | 17.13 | 53.03 |
| 40 | 80 | 17.78 | 53.00 |
| **50 (shipped)** | **73** | **18.40** | **53.80** |
| never cut — translate on `Final` only | 65 | 18.11 | 52.76 |

Monotonic. Every cut costs quality, because each chunk is translated with no knowledge of
its neighbours, and the model's fluency comes from having the whole sentence. Clause
boundaries are better places to cut than word counts — that part of the plan holds — but the
best place to cut is nowhere.

So the guards ship at **50 / 120 / 4000** (`DEFAULT_MIN_CHUNK_CHARS`, `MAX_CHUNK_CHARS`,
`MAX_WAIT_MS`), where quality reaches parity with never cutting. That preserves the one
thing the chunker is actually for — a long sentence spoken in one breath no longer waits for
the speaker to pause — and gives up the rest. On this sample it fires 8 times more than
`Final`-only chunking does.

**Phase 2 is therefore a latency feature, not a quality feature.** It should not be defended
as the latter.

### Two bugs the real audio exposed

Neither was reachable from the textbook sentences in the unit tests:

* **A revised `Final` re-emitted the whole utterance.** Vosk re-ranks a character or two of
  almost every utterance once the full audio is in; `flush` saw the prefix mismatch and
  reset its consumed count to 0. 224 chunks for 125 utterances, with whole sentences
  translated and shown **twice** — 23 near-duplicate re-emits in 7.7 minutes. This is the
  single largest contributor to what "the translations were wrong" looked like on screen.
* **`で` inside `できます` was cut as a te-form.** `見ることができます` became `...ことがで` |
  `きます`, two meaningless fragments, instead of "you can see the city of Yokohama".

### Latency

Per-chunk translate time: **mean 75ms, p95 164ms** at the shipped guards. The plan's
acceptance criterion was p95 added latency under ~1s, and translation is nowhere near being
the constraint. End-to-end latency is not measured here and cannot be: `ja_eval` feeds the
whole clip to the recognizer at once, so a cut-to-shown number measures the backlog of that
burst, not anything a user experiences. Measuring it properly needs the audio paced in real
time.

### Reproducing

```sh
afconvert -f WAVE -d LEI16@16000 -c 1 clip.m4a clip.wav
cargo run --release --bin ja_eval -- clip.wav                        # shipped guards
VID_TRANSLATE_EVAL_CHUNKER=final-only cargo run --release --bin ja_eval -- clip.wav
VID_TRANSLATE_EVAL_GUARDS=20,80,2500  cargo run --release --bin ja_eval -- clip.wav
```

Chunk boundaries are deterministic; Vosk's own output varies slightly between runs on
identical input, worth about ±0.3 BLEU. Do not read a swing that size as signal.

---

## What the ASR logs settled

Three questions the plan asked about Vosk's behaviour, now answered from real output. The
Japanese chunker is built on these and restates them at the top of
`src-tauri/src/chunker/japanese.rs`:

* **Vosk JA emits spaces between morphemes, not words** — `こんにちは 、 元気 です か ？`.
  A recognizer artefact, so the chunker de-spaces before matching anything, or `です` would
  never match `で す`.
* **It emits `、` and `。`, but unreliably** and not dependably at real sentence ends, so
  punctuation is a bonus signal (Tier A) rather than the primary one. Whisper would change
  this.
* **Partials are revised, not append-only.** Confirmed in volume: nearly every utterance is
  re-ranked on `Final`. This is why the live line's id is keyed off the chunker's boundary
  index rather than a text prefix, and why `consumed` is never re-derived from the text.

---

## Phase 4 — quantization A/B

Still open, and now the most promising remaining lever on quality after ASR.

```sh
ct2-transformers-converter --model staka/fugumt-ja-en \
    --output_dir ct2-model-ja-fp32 --copy_files source.spm target.spm

VID_TRANSLATE_JA_MODEL_DIR=/abs/path/ct2-model-ja-fp32 ./eval/run.sh eval/out/fp32
./eval/run.sh eval/out/int8
```

int8 degrades lower-resource pairs more than high-resource ones, and ja-en is the weaker of
the two pairs here. Baseline to beat: **BLEU 18.13, chrF 53.40** at the shipped guards.

### Verdict

*Not yet collected.* If fp32 is clearly better, ~300MB is a defensible cost for the pair
that is currently broken. Do not change the default without a number here.

---

## Where this leaves the work

| | |
|---|---|
| ES→EN | unchanged, verified by a 2000-case equivalence fuzz against the original algorithm |
| JA chunking | correct and clause-aligned, tuned to where it costs no quality; a latency win |
| JA re-translating live line | works — the line visibly self-corrects as a sentence completes |
| JA quality | **still limited by ASR at 20.8% CER.** Next lever is whisper.cpp `-l ja`, then fp32 |
