# Measuring what a Whisper pass costs

The recognizer's design rests on one inequality: **a pass must finish inside the step.**
Everything else — the adaptive step, the backlog drop, the choice of model, whether to add a
GPU backend — is downstream of whether that holds on a given machine. This is how to find
out, so those decisions are made against numbers instead of impressions.

- `src-tauri/src/bin/whisper_bench.rs` — measures one configuration, prints one CSV row.
- `scripts/bench-whisper.sh` — drives the matrix on Linux.
- `scripts/bench-whisper.ps1` — same matrix on Windows.

## The one number that decides things

`headroom_at_2s` = 2000ms ÷ mean pass ms.

`STEP_MIN_MS` is 2000, the floor on the step the recognizer will choose. So:

| headroom | meaning |
|---|---|
| **< 1.0** | The mean pass does not fit the minimum step. This config **drops audio** on this machine, at any step. Captions get holes. |
| **1.0 – 1.25** | Fits, but with less margin than `STEP_HEADROOM` (1.25) assumes. Will fail the moment the machine is busy. |
| **> 1.25** | Keeps up with room to spare. |

`overrun_frac` is the fraction of individual passes over 2000ms, and it is the honest
companion to the mean: a config with 1.3x mean headroom but 20% overruns is dropping audio
in bursts, which reads to a user as captions that stutter rather than lag.

Read `p95_ms` over `mean_ms` whenever the two disagree. A dropped window is a permanent hole
in the transcript, so the tail is what the user experiences.

## The protocol, and why each part is non-negotiable

### 1. Run it twice: idle, and with a browser playing video

```sh
scripts/bench-whisper.sh --audio eval/ja/clip01.wav --label idle
scripts/bench-whisper.sh --audio eval/ja/clip01.wav --label browser-video
```

The app runs while the user watches something. On a 15W laptop with integrated graphics that
competes for three resources at once, and they fail differently:

- **Video decode is mostly free.** Intel's media engine (QuickSync) is fixed-function with
  its own queue and clock, independent of the compute EUs. H.264/AV1 decode does not eat the
  GPU's shader array.
- **Browser compositing is not free.** It runs on the EUs, continuously, while a video
  plays. Directly relevant if a Vulkan backend is ever added — whisper would be sharing the
  device with the compositor.
- **Memory bandwidth and the power budget are the real squeeze.** Whisper inference is
  bandwidth-bound, and on a U-series part the CPU and GPU draw from one envelope. Load one
  and the other clocks down.

An idle-machine number is therefore not a conservative estimate, it is the wrong number. The
delta between the two labels is the most useful thing this harness produces.

### 2. Mains power, performance profile

```sh
powerprofilesctl set performance
```

The script warns if either is wrong, because both silently halve sustained clocks rather than
failing. A battery run and a mains run are not comparable, full stop.

### 3. Let warmup finish

Default 60s discarded per cell (20s under `--preset quick`). On a 15W part the first seconds
of any benchmark are a lie: the package starts cold, spends its turbo budget, then settles
into the clock a real session actually runs at. The spec-sheet 4.70 GHz on an i7-1255U is a
single-core burst you will never see during captioning; sustained all-core is a different
number entirely, and it is the only one that matters here.

### 4. Make RAPL readable

```sh
sudo chmod +r /sys/class/powercap/intel-rapl:0/energy_uj
```

Without it the `watts_mean` column is empty, and a cell that hit the power limit is
indistinguishable from a cell that is computationally slow. Those two call for opposite
fixes — fewer threads versus a smaller model — so the distinction is worth a chmod.

(It is root-only by default on Fedora as a side-effect of the PLATYPUS mitigation. Only
relevant if untrusted local users share the machine.)

### 5. Use a clip with real speech in it

16 kHz mono 16-bit PCM, same as the `ja_eval` corpus:

```sh
ffmpeg -i source.mp4 -ac 1 -ar 16000 -c:a pcm_s16le eval/ja/clip01.wav
```

A silent or music-only clip gets rejected by the Silero gate, and the harness exits with an
error rather than reporting the cost of the gate saying no. The window is built by repeating
the clip to the requested length, so every pass decodes identical audio — pass-to-pass
variance is then the machine's, not the content's, which is what makes `p95` readable.

## What the sweep is looking for

### Thread count — expect a surprise

The app currently asks for `available_parallelism() - 2` threads
(`recognizer/whisper.rs:328-330`), and `marian.rs:88-89` asks for `cores - 4` concurrently.
On a 12-thread machine that is **18 threads on 12 hardware threads**.

On a hybrid CPU that is worse than ordinary oversubscription. ggml splits each tensor op into
**equal** chunks per thread and waits for all of them, so a thread on an E-core paces the
whole pass while the P-core threads idle at the barrier. Expect the optimum to be well below
`nproc - 2`, and expect `ecores` (8 homogeneous cores, no SMT, no straggler) to compete with
or beat `all`.

The script detects the P/E split from `/sys/devices/system/cpu/types/` rather than
hardcoding it. The Windows script **cannot** — Windows publishes no equivalent, so the masks
there are hardcoded for the Alder Lake-U layout and are wrong on any other machine. Edit
`$affinitySets` before trusting a Windows run on different hardware.

### Model size — probably the biggest lever

`base-q5_1` over `small` cuts compute, memory traffic and watts simultaneously. On a
bandwidth-bound, power-limited machine that compounds, so the win is larger than the
parameter-count ratio suggests. If the `browser-video` label shows `small` below 1.0
headroom and `base-q5_1` above 1.25, that is the answer to the whole performance question
and no backend work is needed.

### Flash attention — free if it works

The app loads contexts with `WhisperContextParameters::default()`
(`recognizer/whisper.rs:281`), so it is currently off. The sweep A/Bs it. No correctness
tradeoff, so a win here is free.

### `audio_ctx` — already won, worth confirming

`audio_ctx_for` caps the mel window so a 6s window is encoded as ~6s of work rather than
whisper's zero-padded 30s. `--full-audio-ctx` restores the old behaviour for an A/B. This
should show a large regression; if it does not, the cap is not doing what it claims on this
build and that is worth knowing.

## What this harness deliberately does not measure

**Accuracy.** Every number here is latency. A smaller model that keeps up may transcribe
worse, and this tool will happily recommend it. Pair any model-size change with a `ja_eval`
run (BLEU/chrF) before shipping it — `eval/ja/README.md`. A config that keeps up but
captions badly is not an improvement.

**The GPU path.** Nothing here tests Vulkan, because the current tree cannot: whisper.cpp
only began *accepting* integrated GPUs as a backend in
[PR #3492](https://github.com/ggml-org/whisper.cpp/pull/3492), merged 2025-11-01 and shipped
in 1.8.3. Before that an iGPU was enumerated and then ignored, falling back to CPU silently.
`Cargo.lock` pins `whisper-rs-sys` 0.15.0, which near-certainly predates it. **Confirm the
vendored whisper.cpp version before spending any time on a Vulkan build** — otherwise you
pay the CI and shader-toolchain cost and still measure the CPU.

Note also that the 3492 thread contains an unresolved report from a user with Iris Xe
specifically whose GPU was not recognised, and that the PR author's own measurement is 3-4x
realtime-factor improvement — not the 12x in the headlines, which is unexplained. Treat
Vulkan as an unproven 2-4x, worth a timeboxed spike after the free wins above are banked.
