# Performance investigation — handoff

Companion to `HANDOFF.md`, scoped to one question: **why the app keeps up on an M3 Mac and
drops audio on Windows/Linux, and what to do about it.**

Written to be self-contained. A session that reads this and `docs/bench-whisper.md` has the
context; nothing important lives only in the chat it came from.

Everything below is either a verified fact (with how it was verified) or an explicitly
labelled expectation. **No performance measurement has been taken yet on any machine.**
That is the next step and it cannot be done from a cloud container.

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

**There may be a second machine.** A Windows desktop at
`C:\Users\pager_dot\Desktop\vid_translate` was mentioned and it is **not confirmed** to be
this laptop booted into Windows. If it is a separate desktop PC, none of the analysis here
transfers — different topology, different bandwidth, likely no 15 W ceiling, possibly a
discrete GPU, which would change the Vulkan question entirely. **Establish which machine any
benchmark ran on before comparing numbers.**

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

### 4. `GGML_AVX_VNNI` is unused

`.github/workflows/release.yml:15` sets `GGML_NATIVE: "OFF"` for a portable AVX2 baseline —
the right call for shipping, but it leaves Alder Lake's AVX-VNNI unused, which specifically
accelerates the int8 dot products in the q5/q8 models. Needs a separate build or runtime
dispatch; enabling it unconditionally would crash pre-2021 CPUs.

### 5. Vulkan — real but blocked, and smaller than advertised

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
cargo build --release --features eval --bin whisper_bench --manifest-path src-tauri\Cargo.toml
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

1. Confirm which machine the Windows path refers to (laptop dual-boot vs separate desktop).
2. Run the two `--preset quick` passes on Fedora, idle and browser-video. ~5 min each.
3. Read the thread-count and model-size columns. Items 1 and 2 above are likely settled by
   that alone.
4. Check the vendored whisper.cpp version before any Vulkan work (item 5).
5. Only then decide whether a backend change is worth building.
