# JA→EN diagnosis

Running record for the Japanese pipeline. Two verdicts live here: the **Phase 0 ASR
verdict** (is the garbling an ASR problem or an MT problem?) and the **Phase 4 quantization
verdict** (is int8 costing us quality on ja-en?).

Both require listening to a real clip, so both are filled in by whoever runs one. The
instrumentation needed to answer them is in the tree now; the answers are not.

---

## Correction to the premise

The chunking plan that produced this work assumed Japanese was being translated "every 8
new words" off the growing Vosk partial, and that an 8-word prefix of an SOV sentence
rarely contains a predicate. That was true of the design at one point but **not of the code
as it stood**: `run_translated_pipeline` guarded the eager-chunking branch with `if !is_ja`,
so Japanese only ever translated a whole, stable Vosk `Final`, de-spaced, in one call. The
model is `staka/fugumt-ja-en`, not `opus-mt-ja-en`.

So short-chunk damage was not the cause of the garbling. Whatever was wrong was wrong about
**whole sentences** — which leaves ASR quality, the MT model itself, or its quantization.
That makes Phase 0 the load-bearing step rather than a formality, and it is why the Phase 2
chunker below is justified as a *latency* win (a long sentence no longer waits for a pause)
rather than as the fix for quality.

---

## Phase 0 — how to collect the data

```sh
VID_TRANSLATE_DEBUG_ASR=1 npm run tauri dev
```

Then run one 2–3 minute Japanese clip in JA + LOCAL mode. Three files appear in
`~/Library/Application Support/vid_translate/debug/` (macOS) or
`~/.local/share/vid_translate/debug/` (Linux):

| file | contents |
|---|---|
| `asr-ja.jsonl` | every Vosk callback, text exactly as emitted — no trimming, no de-spacing |
| `mt-ja.jsonl` | every translate call: the exact string passed to `translate_batch`, the output, and the latency |
| `chunks-ja.jsonl` | every chunk the chunker cut, and whether the cut was a real boundary |

The files append, so clear the directory between runs.

### Read `asr-ja.jsonl` and answer

1. **Is the Japanese transcript itself recognisable?** Score ~30 utterances good / partly
   wrong / unusable.
2. **Of the bad translations, how many had bad ASR input vs. good ASR input?** Join
   `mt-ja.jsonl` on `src` to answer this without guessing.

### The gate

> If more than ~40% of ASR output is unusable, stop: the chunker cannot help much and the
> real fix is the ASR model. The next step would be whisper.cpp with `-l ja`, which also
> gives the punctuation an `opus-mt`-family model was trained to expect.

### Verdict

*Not yet collected.* Fill in:

- utterances scored: __ good / __ partly wrong / __ unusable
- share of bad translations with bad ASR input: __%
- **verdict: ASR problem / MT problem / both**

### What the logs already settled

Three questions in the plan were answerable from the existing code and tests without a new
run, and the Japanese chunker is built on the answers (they are restated at the top of
`src-tauri/src/chunker/japanese.rs`):

- **Vosk JA emits spaces between morphemes, not words** — `こんにちは 、 元気 です か ？`.
  Those spaces are a recognizer artefact, so the chunker de-spaces before matching anything,
  or `です` would never match `で す`.
- **It does emit `、` and `。`**, but not reliably at real sentence ends, so punctuation is a
  bonus signal (Tier A) rather than the primary one.
- **JA partials are revised, not append-only** — kanji and word-boundary choices get
  re-ranked as more audio arrives. This is why the live line's id is keyed off the chunker's
  boundary index rather than a text prefix, and why `JapaneseChunker` counts revisions
  (`revisions`) instead of trusting its own consumed-prefix bookkeeping.

---

## Phase 4 — quantization A/B

The shipped model is int8. int8 degrades lower-resource pairs more than high-resource ones,
and ja-en is the weaker of the two pairs here, so an fp32 build is worth one measurement.

```sh
ct2-transformers-converter --model staka/fugumt-ja-en \
    --output_dir ct2-model-ja-fp32 --copy_files source.spm target.spm

# live app
VID_TRANSLATE_JA_MODEL_DIR=/abs/path/ct2-model-ja-fp32 npm run tauri dev
# or offline, scored
./eval/run.sh eval/out/int8
VID_TRANSLATE_JA_MODEL_DIR=/abs/path/ct2-model-ja-fp32 ./eval/run.sh eval/out/fp32
```

`VID_TRANSLATE_JA_MODEL_DIR` (and `VID_TRANSLATE_ES_MODEL_DIR`) override the load path
only — the download story is untouched.

### Verdict

*Not yet collected.* Fill in:

- int8 BLEU / chrF: __ / __
- fp32 BLEU / chrF: __ / __
- latency delta: __
- **verdict:** if fp32 is clearly better, ~300MB is a defensible cost for the pair that is
  currently broken. Do not change the default without a number here.

---

## Phase 5 — baseline

Record the pre-change baseline **before** merging further chunker changes. See
`eval/ja/README.md`.

| run | clips | BLEU | chrF | latency mean | latency p95 | forced cuts |
|---|---|---|---|---|---|---|
| *baseline (Final-only chunking)* | | | | | | n/a |
| *Phase 2 (clause chunking)* | | | | | | |
