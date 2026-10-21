# Performance investigation — handoff

Scoped to one question: **why the app keeps up on an M3 Mac and
drops audio on Windows/Linux, and what to do about it.**

Written to be self-contained. A session that reads this and `docs/bench-whisper.md` has the
context; nothing important lives only in the chat it came from.

Everything below is either a verified fact (with how it was verified) or an explicitly
labelled expectation. The first measurements were taken on 2026-10-17 on Windows (see
**Results** directly below); the findings further down were written before them and are
annotated where the numbers overturned them.

---

## Results — 2026-10-17, Windows, ThinkBook (i7-1255U, Iris Xe)

**Outcome: Vulkan on the iGPU fixes the problem.** Shipped as the default for Windows and Linux
(`whisper-rs` features `["openmp", "vulkan"]`); macOS keeps Metal.

Setup: `ggml-small.bin`, `--translate` (JA mode), 10 s window, Silero VAD on, default threads
(10), 20 s warmup + 40 s measure, Intel driver 32.0.101.7092 (Vulkan 1.4.323), mains power.
Clip: 24 min Japanese family vlog, 16 kHz mono. Raw rows: `eval/ja/bench-2026-10-17.csv`
(git-ignored).

| Config | Load | Mean pass | p95 | `headroom_at_2s` | Overrun |
|---|---|---|---|---|---|
| CPU, 10 threads (previous release) | idle | 1988 ms | 2111 ms | 1.01 | 43% |
| CPU, 4 threads | idle | 2533 ms | 3504 ms | 0.79 | 100% |
| CPU, 10 threads | browser video | 2122 ms | 3141 ms | **0.94** | 32% |
| **iGPU (Vulkan)** | idle | **658 ms** | 695 ms | **3.04** | 0% |
| **iGPU (Vulkan)** | browser video | **667 ms** | 694 ms | **3.00** | 0% |
| iGPU + flash attention | idle | 1197 ms | 1231 ms | 1.67 | 0% |

What this settles:

- **The reported symptom is reproduced:** the CPU build drops below 1.0 headroom as soon as a
  video plays — it cannot keep up at any step.
- **The iGPU is ~3x faster and does not care about the browser** (658 → 667 ms). The
  compositor-contention worry under "Load contention" did not materialise for a 1080p YouTube
  video; a heavier GPU load (games, multiple streams) is untested.
- **Accuracy is unchanged.** `whisper_bench --transcribe 120` on GPU and `--no-gpu`: 11 of 12
  windows byte-identical; the twelfth differs because CPU fell into a repetition loop and GPU
  did not (float-order noise tipping greedy decoding — not a quality regression). Both share
  the same errors against the reference translation, which belong to `small`, not the backend.

How the toolchain had to be set up on Windows (all now handled — see README):

- Run Cargo from `src-tauri`, or `.cargo/config.toml`'s static-CRT setting is skipped → `LNK2038`.
- Vulkan SDK needed at build time (headers, `vulkan-1.lib`, `glslc`).
- `CARGO_BUILD_BUILD_DIR` must be short: ggml-vulkan's nested `vulkan-shaders-gen` CMake build
  hits MSBuild's 260-char path limit under `src-tauri\target` (266 chars here; 258 on GitHub
  runners — no margin). The symptom is a misleading *"No CMAKE_C_COMPILER could be found"*.

Not yet measured: Fedora (same silicon, Mesa ANV driver instead of Intel's), other models
(`base-q5_1`, `medium` — the headroom now leaves room to try a *bigger* one), battery power.

---

## Target hardware

Lenovo ThinkBook 14s Yoga G2 IAP, dual-booting Fedora 43 (GNOME/Wayland) and Windows.

| | |
|---|---|
| CPU | i7-1255U — Alder Lake-U, **10 cores / 12 threads: 2 P-cores (SMT) + 8 E-cores** |
| ISA | AVX2 + AVX-VNNI. **No AVX-512** (fused off on Alder Lake), no AMX |
| GPU | Iris Xe 96 EU @ up to 1.25 GHz — **integrated**, no dedicated VRAM, shares system RAM |
| RAM | 16 GB soldered LPDDR4x-4267 → **~68 GB/s total**, shared with the GPU and display |
| Power | **15 W base**, 55 W max turbo. 4.70 GHz is a single-core burst, never a sustained figure |

Confirmed against [Notebookcheck](https://www.notebookcheck.net/589628.0.html) and
[TechPowerUp](https://techpowerup.com/cpu-specs/core-i7-1255u.c2593).

**One machine, two OSes.** The Windows checkout at
`C:\Users\pager_dot\Desktop\vid_translate` is this same laptop dual-booted, confirmed by
the owner. So Fedora and Windows runs are directly comparable: identical silicon, identical
thermals, identical power envelope. A gap between them is an OS, driver or build-toolchain
difference, not a hardware one — which makes the cross-OS delta a useful signal in its own
right rather than noise to control for.

---

## The root cause, and it is not "the M3 CPU is faster"

`src-tauri/Cargo.toml` selects the ggml accelerator per platform:

- macOS → `whisper-rs` with `features = ["metal"]` — the encoder runs **on the GPU**
- everything else → `features = ["openmp"]` — **CPU only**

So the Mac is doing GPU inference and Windows/Linux are doing CPU inference. That is a build
configuration difference, not a hardware law. Compounding it: whisper inference is
**memory-bandwidth bound**, and an M3 has ~100+ GB/s of unified bandwidth against this
laptop's ~68 GB/s.

Worth stating plainly because it was a point of confusion: **Iris Xe is an integrated GPU.**
It has no dedicated video memory — that is what "integrated" means — but it is a real,
programmable, Vulkan-capable compute device. "No VRAM" and "not a GPU" are different claims.

---

## Findings, in priority order

### 1. The thread budget is oversubscribed — and worse on a hybrid CPU

- `recognizer/whisper.rs:328-330` — whisper asks for `available_parallelism() - 2`
- `marian.rs:88-89` — CTranslate2 asks for `cores - 4`, concurrently, in JA mode

`available_parallelism()` returns **logical** cores. On this machine that is 12, so whisper
takes 10 and Marian takes 8: **18 threads on 12 hardware threads.**

On a hybrid part this is worse than ordinary oversubscription. ggml splits each tensor op
into **equal** chunks per thread and waits for all of them, so a thread on an E-core paces
the entire pass while the P-core threads idle at the barrier. SMT siblings on the P-cores
additionally contend for the same vector units, which ggml already saturates.

**Expectation, unmeasured:** the optimum is well below `nproc - 2`, probably 4–6, and
pinning whisper to the 8 E-cores (homogeneous, no SMT, no straggler) may beat using all
cores. Free to test, possibly the largest easy win. **This is the first thing to measure.**

> **Measured 2026-10-17 — not borne out on Windows.** 4 threads was *slower* than 10
> (2533 vs 1988 ms). E-core pinning untested; moot now that whisper runs on the iGPU.

### 2. Model size is probably the biggest lever

Default is `ggml-small.bin` (`lib.rs:118`); quantized variants are already supported
(`lib.rs:88-95`). `base-q5_1` cuts compute, memory traffic and watts *simultaneously*, and on
a bandwidth-bound, power-limited machine those compound — so the win exceeds the
parameter-count ratio.

If the benchmark shows `small` below 1.0 headroom and `base-q5_1` above 1.25 under realistic
load, **that is the answer to the whole question** and no backend work is needed.

### 3. Flash attention is off and costs nothing to turn on

`recognizer/whisper.rs:281` loads contexts with `WhisperContextParameters::default()`.
**Verified:** `flash_attn(bool)` exists and compiles on whisper-rs 0.16. No correctness
tradeoff. The benchmark A/Bs it.

> **Measured 2026-10-17 — wrong on the iGPU.** Flash attention nearly *doubles* the Vulkan
> pass (658 → 1197 ms). Leave it off.

### 4. `GGML_AVX_VNNI` is unused

`.github/workflows/release.yml:15` sets `GGML_NATIVE: "OFF"` for a portable AVX2 baseline —
the right call for shipping, but it leaves Alder Lake's AVX-VNNI unused, which specifically
accelerates the int8 dot products in the q5/q8 models. Needs a separate build or runtime
dispatch; enabling it unconditionally would crash pre-2021 CPUs.

### 5. Vulkan — real but blocked, and smaller than advertised

> **Resolved 2026-10-17 — not blocked, and it is the fix.** The vendored whisper.cpp in
> `whisper-rs-sys` 0.15.0 is **1.8.3** (`project("whisper.cpp" VERSION 1.8.3)` in its
> `CMakeLists.txt`, and `GGML_BACKEND_DEVICE_TYPE_IGPU` accepted in `whisper.cpp`). On Iris Xe
> it logs `using Vulkan0 backend` and measures ~3x the CPU — see **Results**. The text below
> is the pre-measurement reasoning, kept for the record.

**Do not start this work before checking one thing.** whisper.cpp only began *accepting*
integrated GPUs as a backend in
[PR #3492](https://github.com/ggml-org/whisper.cpp/pull/3492), merged **2025-11-01**, shipped
in **1.8.3**. Before that an iGPU was enumerated and then silently ignored, falling back to
CPU. `Cargo.lock` pins `whisper-rs-sys` **0.15.0**, which near-certainly predates it —
**unverified**, because the vendored version could not be read from the cloud container.

Check it in `whisper-rs-sys-*/out/build/` after a build. If it predates 1.8.3, a Vulkan build
would cost the whole CI and shader-toolchain setup and still measure the CPU.

Also recalibrate the payoff: Phoronix ran a
[12x headline](https://www.phoronix.com/news/Whisper-cpp-1.8.3-12x-Perf), but the PR author's
own figure is **3–4x realtime factor**, and the 12x is unexplained in the thread. Benchmarked
hardware was a Radeon 680M and a Core Ultra 7 155H (Arc) — both **newer and stronger** than
Xe-LP. The same PR has an unresolved report from a user with Iris Xe whose GPU was not
recognised, plus an open SYCL segfault on Intel iGPUs (#4014). Treat it as an unproven 2–4x
worth a timeboxed spike **after** items 1–4 are banked.

---

## Load contention: three resources, failing differently

The app runs while the user watches video. This is not a detail, it reorders the priorities
above — and it is why an idle-machine benchmark is the *wrong* number, not a conservative one.

- **Video decode is mostly free.** Intel's media engine (QuickSync) is fixed-function with
  its own queue and clock, [independent of the compute EUs](https://www.intel.com/content/www/us/en/docs/oneapi/optimization-guide-gpu/2023-0/media-engine-hardware.html).
  *Caveat:* that page targets Flex Series "and some other Intel GPUs" and does not explicitly
  confirm Xe-LP.
- **Browser compositing is not free.** It runs on the EUs, continuously, while a video plays.
- **Bandwidth and the 15 W budget are the real squeeze.** Whisper is bandwidth-bound, and CPU
  and GPU draw from one power envelope — load one and the other clocks down.

**A conclusion that was reached and then retracted:** "put whisper on the iGPU and Marian on
the CPU, and the contention is solved" is **wrong**, because the browser is on that same
iGPU. It trades CPU contention for GPU *plus* bandwidth contention on a device the compositor
is actively using. Vulkan may still win; it does not win for that reason.

---

## Ruled out

- **NPU** — Alder Lake has none. Intel NPUs start with Meteor Lake. Do not look for one.
- **OpenVINO** — a genuinely good fit for this chip (Intel CPU/iGPU), but needs per-model
  conversion. Vulkan gets most of the way with one build flag and one code path for both
  Fedora and Windows. Revisit only if Vulkan disappoints.
- **Blaming the CPU generation** — see "root cause". The gap is a missing GPU backend plus
  memory bandwidth, not an inferior CPU.

---

## The harness

`docs/bench-whisper.md` is the full protocol. In brief:

```sh
# Fedora
powerprofilesctl set performance
sudo chmod +r /sys/class/powercap/intel-rapl:0/energy_uj
scripts/bench-whisper.sh --audio eval/ja/clip01.wav --label idle --preset quick
scripts/bench-whisper.sh --audio eval/ja/clip01.wav --label browser-video --preset quick
```

```powershell
# Windows — needs MSVC toolchain, CMake, VS Build Tools (C++); it builds whisper.cpp and
# CTranslate2 from source. Affinity masks are opt-in: Windows publishes no hybrid split.
# Build from inside src-tauri (its .cargo\config.toml sets the static CRT; --manifest-path from
# the root skips it and linking fails with LNK2038), with a short build-dir (the Vulkan shader
# generator's nested MSBuild paths otherwise exceed 260 chars). Needs the Vulkan SDK too.
# bench-whisper.ps1 does all of this itself when the binary is missing.
cd src-tauri; $env:CARGO_BUILD_BUILD_DIR = 'C:\vtb'
cargo build --release --features eval --bin whisper_bench; cd ..
powershell -ExecutionPolicy Bypass -File .\scripts\bench-whisper.ps1 `
    -Audio eval\ja\clip01.wav -Label idle -Preset quick -PCoreMask 0x00F -ECoreMask 0xFF0
```

The decision metric is **`headroom_at_2s`** = 2000 ms ÷ mean pass. `STEP_MIN_MS` is 2000, the
floor on the step the recognizer will accept, so **below 1.0 the config drops audio at any
step.** Sort by that column.

### State of the harness

- **Verified:** compiles (`cargo build --release --features eval` exits 0), 4/4 unit tests
  pass, CLI error paths exit 1 with readable messages, the WAV reader round-trips a real
  16 kHz mono PCM file, and the runner matrix is dry-run correct.
- **Not verified:** no inference cell has executed anywhere. `huggingface.co` was blocked by
  the cloud container's network policy, so no model weights were reachable. **The first real
  execution happens on the target hardware.**
- Built and tested only on a 4-core Xeon container with AVX-512/AMX, no SMT and no GPU —
  systematically unlike the target in every decisive respect, which is why no numbers were
  produced there rather than misleading ones.

---

## What this does not measure

**Accuracy.** Every number is latency. A smaller model that keeps up may transcribe worse,
and the harness will happily recommend it. Pair any model-size change with a `ja_eval` run
(BLEU/chrF, see `eval/ja/README.md`) before shipping. A config that keeps up but captions
badly is not an improvement.

---

## Next actions, in order

> **Updated 2026-10-17:** steps 3–5 are done on Windows and Vulkan shipped. Remaining: run the
> same matrix on Fedora (`scripts/bench-whisper.sh`), try a larger model now that there is
> headroom, and confirm the first CI release with the Vulkan SDK step goes green.

1. Run the two `--preset quick` passes on Fedora, idle and browser-video. ~5 min each.
2. Read the thread-count and model-size columns. Findings 1 and 2 above are likely settled
   by that alone.
3. Repeat on Windows for the same-silicon comparison. Verify the P/E logical-processor
   numbering in Task Manager first — the `0x00F` / `0xFF0` masks assume P-cores land on
   CPUs 0-3, which Windows does not guarantee the way Linux publishes it.
4. Check the vendored whisper.cpp version before any Vulkan work (finding 5).
5. Only then decide whether a backend change is worth building.
