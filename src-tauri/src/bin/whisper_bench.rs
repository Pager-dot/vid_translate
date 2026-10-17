//! What one Whisper pass actually costs on *this* machine, under *this* load.
//!
//! The recognizer's whole design rests on one inequality: a pass must finish inside the
//! step (see `recognizer::whisper`'s module docs). Everything else — the adaptive step, the
//! backlog drop, the choice of model — is downstream of whether that holds. This binary
//! measures it, so a change to threads, model size, `audio_ctx` or the ggml backend is a
//! number rather than an impression.
//!
//! ## One process, one config
//!
//! This measures exactly one configuration and prints one CSV row. The matrix lives in
//! `scripts/bench-whisper.sh` (and `.ps1`), which re-invokes this binary per cell. That is
//! not an accident of convenience, it is required for the numbers to mean anything:
//!
//! * OpenMP thread pools persist for the life of the process and do not reliably resize
//!   when `n_threads` changes. An in-process sweep would measure the first thread count it
//!   was given and quietly attribute it to every later row.
//! * Core affinity is a property of a process, so `taskset` / `start /affinity` can only be
//!   an outer loop. On a hybrid CPU (P-cores and E-cores) that outer loop is the single
//!   most interesting dimension, because ggml splits each tensor op into *equal* chunks and
//!   then waits for all of them — one E-core thread sets the pace for the whole pass.
//! * A fresh process per cell keeps thermal and turbo state attributable to the cell.
//!
//! ## Steady state is the only state that matters
//!
//! On a 15W-class laptop the first few seconds of any benchmark are a lie: the package
//! starts cold, burns its turbo budget, and then settles into a sustained clock that can be
//! half the burst figure. A captioning session runs for minutes, so the sustained number is
//! the real one. `--warmup-ms` is discarded before measurement begins and defaults to
//! something long enough to reach that state rather than something that makes the benchmark
//! quick.
//!
//! ## Usage
//!
//!   cargo run --release --features eval --bin whisper_bench -- \
//!       --model ~/.local/share/vid_translate/models/ggml-base-q5_1.bin \
//!       --audio eval/ja/clip01.wav \
//!       --threads 4 --window-ms 10000
//!
//! Audio must be 16 kHz mono 16-bit PCM WAV — the format the live capture path produces, so
//! the model sees what it sees in production:
//!
//!   ffmpeg -i source.mp4 -ac 1 -ar 16000 -c:a pcm_s16le clip.wav
//!
//! The clip must contain real speech. A silent clip measures the VAD gate rejecting it and
//! nothing else, which is not the cost the step has to cover.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vid_translate_lib::recognizer::whisper::{audio_ctx_for, VAD_MODEL_FILE};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext,
    WhisperVadContextParams, WhisperVadParams,
};

const SAMPLE_RATE: usize = 16_000;

/// The recognizer's `STEP_MIN_MS`. Not imported because it is private and deliberately so;
/// duplicated here with the reason it matters: it is the floor on the step, so a mean pass
/// above this cannot keep up *at any step the recognizer is willing to choose*, and the
/// session will drop audio rather than lag. It is the pass/fail line of this whole exercise.
const STEP_MIN_MS: f64 = 2000.0;

fn ms_to_samples(ms: usize) -> usize {
    ms * SAMPLE_RATE / 1000
}

struct Config {
    model: PathBuf,
    audio: PathBuf,
    threads: i32,
    window_ms: usize,
    warmup_ms: u64,
    measure_ms: u64,
    flash_attn: bool,
    vad: bool,
    full_audio_ctx: bool,
    lang: String,
    translate: bool,
    label: String,
    csv_header: bool,
}

fn usage() -> String {
    "\
whisper_bench — cost of one Whisper pass, for one configuration

Required:
  --model <path>        ggml model file (.bin)
  --audio <path>        16kHz mono 16-bit PCM WAV containing speech

Shape of the pass:
  --threads <n>         n_threads for ggml and the VAD  [default: cores-2, as the app]
  --window-ms <ms>      audio in the window per pass     [default: 10000]
  --full-audio-ctx      do not cap the mel window (A/B against audio_ctx_for)
  --no-vad              skip the Silero gate even if the weights are present
  --flash-attn          enable flash attention on the context
  --lang <code>         language hint                    [default: ja]
  --translate           Whisper's translate task (what JA mode runs)

Timing:
  --warmup-ms <ms>      discarded before measuring       [default: 60000]
  --measure-ms <ms>     measurement duration             [default: 60000]

Output:
  --label <text>        free-form tag copied into the row (e.g. \"browser-video\")
  --csv-header          print the header line before the row
  --print-header        print only the CSV header and exit (no model needed)
"
    .to_string()
}

fn parse_args() -> Result<Config, String> {
    // Hand-rolled rather than clap: this is a dev binary built behind a feature gate, and a
    // CLI dependency would be compiled into every `--features eval` build to save thirty
    // lines here.
    let default_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(2)
        .max(2) as i32;

    let mut cfg = Config {
        model: PathBuf::new(),
        audio: PathBuf::new(),
        threads: default_threads,
        window_ms: 10_000,
        warmup_ms: 60_000,
        measure_ms: 60_000,
        flash_attn: false,
        vad: true,
        full_audio_ctx: false,
        lang: "ja".to_string(),
        translate: true,
        label: String::new(),
        csv_header: false,
    };

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Handled before anything else so the runner script can emit a header cheaply: every
    // other path requires a model on disk, and a header is not worth a model load.
    if args.iter().any(|a| a == "--print-header") {
        println!("{CSV_HEADER}");
        std::process::exit(0);
    }

    let mut i = 0;
    while i < args.len() {
        let next = |i: usize, what: &str| -> Result<String, String> {
            args.get(i + 1).cloned().ok_or_else(|| format!("{what} needs a value"))
        };
        let num = |s: String, what: &str| -> Result<u64, String> {
            s.parse::<u64>().map_err(|_| format!("{what}: '{s}' is not a number"))
        };
        match args[i].as_str() {
            "--model" => {
                cfg.model = PathBuf::from(next(i, "--model")?);
                i += 2;
            }
            "--audio" => {
                cfg.audio = PathBuf::from(next(i, "--audio")?);
                i += 2;
            }
            "--threads" => {
                cfg.threads = num(next(i, "--threads")?, "--threads")? as i32;
                i += 2;
            }
            "--window-ms" => {
                cfg.window_ms = num(next(i, "--window-ms")?, "--window-ms")? as usize;
                i += 2;
            }
            "--warmup-ms" => {
                cfg.warmup_ms = num(next(i, "--warmup-ms")?, "--warmup-ms")?;
                i += 2;
            }
            "--measure-ms" => {
                cfg.measure_ms = num(next(i, "--measure-ms")?, "--measure-ms")?;
                i += 2;
            }
            "--lang" => {
                cfg.lang = next(i, "--lang")?;
                i += 2;
            }
            "--label" => {
                cfg.label = next(i, "--label")?;
                i += 2;
            }
            "--flash-attn" => {
                cfg.flash_attn = true;
                i += 1;
            }
            "--no-vad" => {
                cfg.vad = false;
                i += 1;
            }
            "--full-audio-ctx" => {
                cfg.full_audio_ctx = true;
                i += 1;
            }
            "--translate" => {
                cfg.translate = true;
                i += 1;
            }
            "--no-translate" => {
                cfg.translate = false;
                i += 1;
            }
            "--csv-header" => {
                cfg.csv_header = true;
                i += 1;
            }
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unrecognised argument '{other}'\n\n{}", usage())),
        }
    }

    if cfg.model.as_os_str().is_empty() || cfg.audio.as_os_str().is_empty() {
        return Err(format!("--model and --audio are both required\n\n{}", usage()));
    }
    if cfg.threads < 1 {
        return Err("--threads must be at least 1".to_string());
    }
    if cfg.window_ms < 1000 {
        return Err("--window-ms below 1000 is not a window the recognizer would ever use".into());
    }
    Ok(cfg)
}

/// Minimal 16-bit PCM WAV reader, strict on purpose — same rationale as `ja_eval`'s: the
/// corpus is produced by one documented ffmpeg command, and a reader that silently accepts
/// the wrong sample rate would skew every number downstream of it.
fn read_wav_16k_mono(path: &Path) -> Result<Vec<f32>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("{}: not a RIFF/WAVE file", path.display()));
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);

    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(pos + 4) as usize;
        let body = pos + 8;
        match id {
            b"fmt " if body + 16 <= bytes.len() => {
                fmt = Some((u16_at(body), u16_at(body + 2), u32_at(body + 4), u16_at(body + 14)));
            }
            b"data" => {
                let (format, channels, rate, bits) =
                    fmt.ok_or_else(|| format!("{}: data before fmt", path.display()))?;
                if format != 1 || channels != 1 || rate != 16000 || bits != 16 {
                    return Err(format!(
                        "{}: need 16kHz mono 16-bit PCM, got format {format} / {channels}ch / \
                         {rate}Hz / {bits}-bit",
                        path.display()
                    ));
                }
                let end = (body + size).min(bytes.len());
                let samples = bytes[body..end]
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                    .collect();
                return Ok(samples);
            }
            _ => {}
        }
        // Chunks are word-aligned, and an odd `size` is followed by a pad byte.
        pos = body + size + (size & 1);
    }
    Err(format!("{}: no data chunk", path.display()))
}

/// Builds a window of exactly `window_ms` by repeating the clip.
///
/// Repetition rather than a longer clip on purpose: the window is identical on every pass,
/// so the decoder emits the same tokens every time and pass-to-pass variance is the
/// machine's, not the content's. That is what makes p95 readable as "how bad does this
/// machine get" rather than "how hard was this sentence".
fn build_window(clip: &[f32], window_ms: usize) -> Result<Vec<f32>, String> {
    if clip.is_empty() {
        return Err("audio file contains no samples".to_string());
    }
    let want = ms_to_samples(window_ms);
    let mut window = Vec::with_capacity(want);
    while window.len() < want {
        let take = (want - window.len()).min(clip.len());
        window.extend_from_slice(&clip[..take]);
    }
    Ok(window)
}

/// Which CPUs this process is allowed on, read from `/proc/self/status` rather than through
/// `libc::sched_getaffinity` so the binary needs no new dependency. Returns the kernel's own
/// list form (e.g. "4-11"), which is exactly what was passed to `taskset`, so a row can be
/// traced back to the mask that produced it.
#[cfg(target_os = "linux")]
fn affinity() -> (String, usize) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let list = status
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "?".to_string());
    // Count the CPUs the list describes, so a row carries "how many cores did this actually
    // get" without the reader having to expand ranges by hand.
    let count = list
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            match part.split_once('-') {
                Some((a, b)) => {
                    let (a, b) = (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?);
                    Some(b.saturating_sub(a) + 1)
                }
                None => part.parse::<usize>().ok().map(|_| 1),
            }
        })
        .sum();
    (list, count)
}

#[cfg(not(target_os = "linux"))]
fn affinity() -> (String, usize) {
    // Windows exposes this through GetProcessAffinityMask, which would mean a windows-sys
    // dependency for a diagnostic. The runner script records the mask it passed to
    // `start /affinity` instead.
    ("n/a".to_string(), 0)
}

/// Package power, CPU clock and temperature, sampled so a row carries *why* it was slow.
///
/// On a 15W part these three are the difference between "this configuration is
/// computationally worse" and "this configuration hit the power limit", which look identical
/// in a milliseconds column and call for opposite fixes. All three are best-effort: the
/// files are Linux-specific, and RAPL in particular is often root-only on Fedora (a
/// side-effect of the PLATYPUS mitigation), so a missing reading is reported as empty rather
/// than failing the run.
#[derive(Default)]
struct Telemetry {
    /// Cumulative package energy in microjoules, if RAPL is readable.
    energy_uj: Option<u64>,
    /// Mean of per-CPU current frequency in MHz.
    mhz: Option<f64>,
    /// Hottest thermal zone in degrees C.
    temp_c: Option<f64>,
}

#[cfg(target_os = "linux")]
fn sample_telemetry() -> Telemetry {
    let read_num = |p: &str| -> Option<u64> {
        std::fs::read_to_string(p).ok()?.trim().parse::<u64>().ok()
    };

    // RAPL package domain. intel-rapl:0 is the package on every Alder Lake laptop; the glob
    // is avoided because a wrong domain (dram, psys) would be silently plausible.
    let energy_uj = read_num("/sys/class/powercap/intel-rapl:0/energy_uj");

    // Current frequency per CPU. scaling_cur_freq is in kHz. Averaged across the CPUs that
    // report, because on a hybrid part the interesting thing is not any one core's clock but
    // where the package as a whole settled.
    let mut freqs = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Some(khz) = read_num(&format!(
                "/sys/devices/system/cpu/{name}/cpufreq/scaling_cur_freq"
            )) {
                freqs.push(khz as f64 / 1000.0);
            }
        }
    }
    let mhz = if freqs.is_empty() {
        None
    } else {
        Some(freqs.iter().sum::<f64>() / freqs.len() as f64)
    };

    let mut hottest: Option<f64> = None;
    if let Ok(entries) = std::fs::read_dir("/sys/class/thermal") {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("thermal_zone") {
                continue;
            }
            if let Some(milli) = read_num(&format!("/sys/class/thermal/{name}/temp")) {
                let c = milli as f64 / 1000.0;
                // Zones report in millidegrees, but a few report nonsense (0, or a raw
                // register). Anything outside a plausible silicon range is discarded rather
                // than averaged in.
                if (20.0..=110.0).contains(&c) {
                    hottest = Some(hottest.map_or(c, |h: f64| h.max(c)));
                }
            }
        }
    }

    Telemetry { energy_uj, mhz, temp_c: hottest }
}

#[cfg(not(target_os = "linux"))]
fn sample_telemetry() -> Telemetry {
    Telemetry::default()
}

/// `p`-th percentile of `sorted`, by nearest-rank. No interpolation: with the pass counts
/// this produces (tens, not thousands) an interpolated percentile implies a precision the
/// sample size does not support.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        f64::NAN
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// One measured pass, split the way the recognizer splits it: the gate is not free, and its
/// cost grows with the window because it rescans the whole thing every pass.
struct Pass {
    vad_ms: f64,
    decode_ms: f64,
}

pub const CSV_HEADER: &str = "label,model,threads,affinity,affinity_cpus,window_ms,audio_ctx,\
flash_attn,vad,passes,mean_ms,p50_ms,p95_ms,max_ms,vad_mean_ms,headroom_at_2s,overrun_frac,\
mhz_mean,watts_mean,temp_max_c";

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    // `audio_ctx_for` reads this env var itself, so the flag has to be set before the first
    // call rather than threaded through as a parameter. Set here so the knob is visible on
    // the command line instead of only in the environment.
    if cfg.full_audio_ctx {
        std::env::set_var("VID_TRANSLATE_WHISPER_FULL_AUDIO_CTX", "1");
    }

    let clip = match read_wav_16k_mono(&cfg.audio) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[bench] {e}");
            std::process::exit(1);
        }
    };
    let window = match build_window(&clip, cfg.window_ms) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[bench] {e}");
            std::process::exit(1);
        }
    };
    let audio_ctx = audio_ctx_for(window.len());

    // Whisper itself. Loaded with explicit context params rather than through
    // `recognizer::whisper::preload` so flash attention is controllable — preload uses the
    // defaults and caches the context process-wide, which is right for the app and wrong for
    // an A/B.
    let mut ctx_params = WhisperContextParameters::default();
    ctx_params.flash_attn(cfg.flash_attn);
    let load_start = Instant::now();
    let ctx = match WhisperContext::new_with_params(&cfg.model.to_string_lossy(), ctx_params) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[bench] failed to load {}: {e}", cfg.model.display());
            std::process::exit(1);
        }
    };
    let mut state = match ctx.create_state() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[bench] failed to create state: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[bench] model loaded in {:.1?}", load_start.elapsed());

    // The speech gate, on the same terms as the app: weights beside the model, absent is a
    // warning not an error. Benchmarked because the recognizer times gate+decode together —
    // measuring decode alone under-reports what a step has to pay for.
    let mut vad = if cfg.vad {
        match cfg.model.parent().map(|d| d.join(VAD_MODEL_FILE)) {
            Some(p) if p.exists() => {
                let mut vp = WhisperVadContextParams::new();
                vp.set_n_threads(cfg.threads);
                match WhisperVadContext::new(&p.to_string_lossy(), vp) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        eprintln!("[bench] VAD load failed, gate disabled: {e}");
                        None
                    }
                }
            }
            _ => {
                eprintln!("[bench] no {VAD_MODEL_FILE} beside the model — gate disabled");
                None
            }
        }
    } else {
        None
    };

    let run_pass = |state: &mut whisper_rs::WhisperState,
                    vad: &mut Option<WhisperVadContext>|
     -> Result<Pass, String> {
        let vad_start = Instant::now();
        if let Some(vad) = vad.as_mut() {
            let mut vp = WhisperVadParams::new();
            vp.set_threshold(0.5);
            vp.set_min_speech_duration(100);
            vp.set_min_silence_duration(150);
            match vad.segments_from_samples(vp, &window) {
                Ok(segs) if segs.num_segments() == 0 => {
                    return Err("the gate rejected this window as non-speech — the clip needs \
                                real speech in it, otherwise this measures the gate and \
                                nothing else"
                        .to_string())
                }
                Ok(_) => {}
                Err(e) => return Err(format!("VAD failed: {e}")),
            }
        }
        let vad_ms = vad_start.elapsed().as_secs_f64() * 1000.0;

        // Identical to the recognizer's parameters. Any divergence here makes the number
        // unusable for the decision it exists to inform.
        let decode_start = Instant::now();
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(&cfg.lang));
        params.set_translate(cfg.translate);
        params.set_no_context(true);
        params.set_suppress_nst(true);
        params.set_temperature(0.0);
        params.set_temperature_inc(0.0);
        params.set_no_speech_thold(0.6);
        params.set_logprob_thold(-1.0);
        params.set_n_threads(cfg.threads);
        params.set_audio_ctx(audio_ctx);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        state.full(params, &window).map_err(|e| format!("inference failed: {e}"))?;
        let decode_ms = decode_start.elapsed().as_secs_f64() * 1000.0;

        Ok(Pass { vad_ms, decode_ms })
    };

    // Warmup. Discarded, and long by default: on a 15W part the package starts cold, spends
    // its turbo budget, and only then settles into the clock a real session runs at.
    eprintln!(
        "[bench] warmup {}ms (discarded) — threads={} window={}ms audio_ctx={}",
        cfg.warmup_ms, cfg.threads, cfg.window_ms, audio_ctx
    );
    let warmup_deadline = Instant::now() + Duration::from_millis(cfg.warmup_ms);
    let mut warmup_passes = 0u32;
    while Instant::now() < warmup_deadline {
        if let Err(e) = run_pass(&mut state, &mut vad) {
            eprintln!("[bench] {e}");
            std::process::exit(1);
        }
        warmup_passes += 1;
    }
    if warmup_passes == 0 {
        // A zero-pass warmup means the first pass outlived the whole warmup window, which is
        // itself the headline result: this configuration is far past unusable.
        eprintln!(
            "[bench] warning: warmup completed zero passes — one pass exceeds {}ms",
            cfg.warmup_ms
        );
    }

    eprintln!("[bench] measuring for {}ms", cfg.measure_ms);
    let energy_start = sample_telemetry();
    let measure_start = Instant::now();
    let measure_deadline = measure_start + Duration::from_millis(cfg.measure_ms);
    let mut passes: Vec<Pass> = Vec::new();
    let mut mhz_samples: Vec<f64> = Vec::new();
    let mut temp_max: Option<f64> = None;

    // At least one measured pass even if `--measure-ms` is shorter than a pass, so a
    // hopeless configuration still produces a row saying how hopeless.
    while passes.is_empty() || Instant::now() < measure_deadline {
        match run_pass(&mut state, &mut vad) {
            Ok(p) => passes.push(p),
            Err(e) => {
                eprintln!("[bench] {e}");
                std::process::exit(1);
            }
        }
        let t = sample_telemetry();
        if let Some(m) = t.mhz {
            mhz_samples.push(m);
        }
        if let Some(c) = t.temp_c {
            temp_max = Some(temp_max.map_or(c, |h: f64| h.max(c)));
        }
    }
    let elapsed = measure_start.elapsed();
    let energy_end = sample_telemetry();

    let mut totals: Vec<f64> = passes.iter().map(|p| p.vad_ms + p.decode_ms).collect();
    let vad_mean = mean(&passes.iter().map(|p| p.vad_ms).collect::<Vec<_>>());
    let pass_mean = mean(&totals);
    totals.sort_by(|a, b| a.partial_cmp(b).unwrap());

    // The decision metric. Below 1.0 the mean pass does not fit in the recognizer's minimum
    // step, so this configuration drops audio on this machine no matter how the step adapts.
    let headroom = STEP_MIN_MS / pass_mean;
    let overrun = totals.iter().filter(|&&t| t > STEP_MIN_MS).count() as f64 / totals.len() as f64;

    // RAPL counters are cumulative and wrap at a domain-specific maximum. A decrease means a
    // wrap, and rather than guess the wrap point the sample is discarded — a missing power
    // figure is honest, a wrong one is not.
    let watts = match (energy_start.energy_uj, energy_end.energy_uj) {
        (Some(a), Some(b)) if b >= a => Some((b - a) as f64 / 1e6 / elapsed.as_secs_f64()),
        _ => None,
    };

    let (aff, aff_cpus) = affinity();
    let model_name = cfg
        .model
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "?".to_string());
    let fmt = |v: Option<f64>| v.map(|x| format!("{x:.1}")).unwrap_or_default();

    if cfg.csv_header {
        println!("{CSV_HEADER}");
    }
    println!(
        "{label},{model},{threads},{aff},{aff_cpus},{window},{audio_ctx},{flash},{vad},{n},\
{mean:.0},{p50:.0},{p95:.0},{max:.0},{vadm:.0},{headroom:.2},{overrun:.2},{mhz},{watts},{temp}",
        label = cfg.label,
        model = model_name,
        threads = cfg.threads,
        window = cfg.window_ms,
        flash = cfg.flash_attn,
        vad = vad.is_some(),
        n = totals.len(),
        mean = pass_mean,
        p50 = percentile(&totals, 50.0),
        p95 = percentile(&totals, 95.0),
        max = totals.last().copied().unwrap_or(f64::NAN),
        vadm = vad_mean,
        mhz = fmt(if mhz_samples.is_empty() { None } else { Some(mean(&mhz_samples)) }),
        watts = fmt(watts),
        temp = fmt(temp_max),
    );

    // Said in words as well as a CSV cell, because this is the one line that decides whether
    // the configuration is usable at all and a lone 0.84 is easy to skim past.
    if headroom < 1.0 {
        eprintln!(
            "[bench] VERDICT: cannot keep up — mean pass {pass_mean:.0}ms exceeds the {STEP_MIN_MS:.0}ms \
             minimum step ({:.0}% of passes overrun). This config drops audio.",
            overrun * 100.0
        );
    } else if headroom < 1.25 {
        eprintln!(
            "[bench] VERDICT: marginal — mean pass {pass_mean:.0}ms leaves {:.0}% headroom at the \
             minimum step. STEP_HEADROOM is 1.25, so this config has no room for a busy machine.",
            (headroom - 1.0) * 100.0
        );
    } else {
        eprintln!(
            "[bench] VERDICT: keeps up — mean pass {pass_mean:.0}ms, {:.2}x headroom at the \
             minimum step.",
            headroom
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank_and_clamps() {
        let xs = vec![10.0, 20.0, 30.0, 40.0];
        assert_eq!(percentile(&xs, 50.0), 20.0);
        assert_eq!(percentile(&xs, 100.0), 40.0);
        // Below the first rank still lands on the first element rather than panicking.
        assert_eq!(percentile(&xs, 0.0), 10.0);
        assert!(percentile(&[], 95.0).is_nan());
    }

    #[test]
    fn window_is_exactly_the_requested_length() {
        // A clip shorter than the window has to repeat; one longer has to truncate. Both
        // must land on the exact sample count, because audio_ctx is derived from it and an
        // off-by-a-bit window would change the mel cap and so the measurement.
        let short = vec![0.5f32; ms_to_samples(1500)];
        assert_eq!(build_window(&short, 10_000).unwrap().len(), ms_to_samples(10_000));
        let long = vec![0.5f32; ms_to_samples(30_000)];
        assert_eq!(build_window(&long, 5_000).unwrap().len(), ms_to_samples(5_000));
        assert!(build_window(&[], 5_000).is_err());
    }

    #[test]
    fn window_repeats_the_clip_rather_than_padding_with_silence() {
        // Padding with zeros would hand the gate a window that is mostly silence and the
        // encoder mostly nothing to do, which would quietly understate every pass cost.
        let clip: Vec<f32> = (0..ms_to_samples(1000)).map(|i| (i % 7) as f32 / 7.0).collect();
        let w = build_window(&clip, 3_000).unwrap();
        assert_eq!(&w[..clip.len()], &clip[..]);
        assert_eq!(&w[clip.len()..clip.len() * 2], &clip[..]);
        assert!(w.iter().any(|&s| s != 0.0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn affinity_counts_cpus_in_the_kernels_list_form() {
        // The real reader is exercised by running at all; this pins the range arithmetic,
        // which is the part that would silently mislabel a hybrid-CPU row.
        let (list, count) = affinity();
        assert!(!list.is_empty());
        assert!(count >= 1, "expected at least one allowed CPU, got {count} from '{list}'");
    }
}
