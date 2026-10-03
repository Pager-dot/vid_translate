# JA eval corpus

20–30 short Japanese clips with hand-written English references. Small on purpose: the job
is to catch regressions in chunking and model changes, not to produce a publishable score.

## Adding a clip

1. Cut 5–15 seconds of Japanese speech — one or two sentences, the length a live caption
   actually has to handle.
2. Convert to the exact format the live capture path feeds Vosk, so the ASR sees what it
   sees in production:

   ```sh
   ffmpeg -i source.mp4 -ac 1 -ar 16000 -c:a pcm_s16le eval/ja/clip01.wav
   # no ffmpeg? afconvert ships with macOS and does the same job:
   afconvert -f WAVE -d LEI16@16000 -c 1 source.m4a eval/ja/clip01.wav
   ```

   `ja_eval` rejects anything that is not 16 kHz mono 16-bit PCM rather than resampling it,
   so a clip that slips through in the wrong format cannot quietly skew a score.

3. Write the reference English in `clip01.txt` — one line, natural English, the way a human
   interpreter would say it. Not a gloss, and not a translation of the ASR output: a
   translation of what was *said*, so ASR errors count against the pipeline as they should.
4. Optionally record the Japanese you actually hear in `clip01.ja.txt`. `run.sh` ignores it,
   but it is what makes a Phase 0-style "was this bad ASR or bad MT?" judgement possible
   later without re-listening to everything.

Clip order is alphabetical by filename, and `hyp.txt` / `ref.txt` are written in that order,
so renaming clips mid-corpus invalidates comparisons against older runs.

## Running

```sh
./eval/run.sh                   # timestamped directory under eval/out/
./eval/run.sh eval/out/baseline # named run, for before/after comparison
```

Record the pre-change baseline **before** merging a chunker change.

Clips and their references are not committed — they are someone's audio, and the corpus is
expected to be local to whoever is measuring. Only this README and `run.sh` are in git.

## Comparing a different recognizer

`ja_eval` also accepts a `.txt` of Japanese already transcribed by something else, one ASR
final per line. Everything downstream — chunker, model — is the shipped code, so the score
difference is the recognizer's:

```sh
whisper-cli -m ggml-small.bin -l ja -mc 0 -f clip.wav -oj -of out
python3 -c "import json,sys;print('\n'.join(s['text'].strip() for s in json.load(open('out.json'))['transcription'] if s['text'].strip()))" > out.ja.txt
cargo run --release --bin ja_eval -- out.ja.txt
```

`-mc 0` is not optional: Whisper's default context carry-over produces repetition loops
hundreds of segments long. See docs/ja-diagnosis.md.

## A note on what to measure on

The first corpus clip was a single speaker in a quiet room, and it flattered Vosk badly —
20.8% CER there versus a near-total collapse on multi-speaker kitchen audio. **Any clip set
used to make a decision needs noisy, multi-speaker, overlapping-speech material in it**, or
it will report that whichever recognizer you already have is fine.
