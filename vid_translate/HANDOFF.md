# VidTranslate — Handoff Document

Real-time subtitle overlay widget that captures system audio and transcribes it live.  
Built on **Tauri v2 + React 19 (Vite)** with a **Rust** backend.

---

## What It Does

- Sits as a frameless, always-on-top, transparent bar over the desktop
- Captures whatever is playing through the speakers (system loopback audio)
- Transcribes speech in real-time using Vosk (offline, no internet needed)
- Shows spoken words in gray as they're being said, highlights the current word in white
- Snaps to full white when a sentence finalises, then clears after 2.5 seconds
- Window is freely resizable; font scales with window width

---

## Project Structure

```
vid_translate/
├── src/                        # React frontend
│   ├── App.jsx                 # Subtitle widget UI
│   ├── App.css                 # Overlay styling
│   └── main.jsx                # React entry point
├── src-tauri/
│   ├── src/
│   │   ├── main.rs             # Tauri entry point (unchanged)
│   │   ├── lib.rs              # Commands, events, pipeline state
│   │   ├── audio.rs            # System audio capture via parec
│   │   ├── recognizer.rs       # Vosk streaming recognizer
│   │   └── transcriber.rs      # Whisper wrapper (kept, unused for now)
│   ├── Cargo.toml              # Rust dependencies
│   ├── tauri.conf.json         # Window config
│   └── capabilities/
│       └── default.json        # Tauri permissions
├── index.html
├── package.json
└── vite.config.js
```

---

## System Dependencies (must be installed)

```bash
# PulseAudio/PipeWire development library (needed to build)
sudo dnf install pulseaudio-libs-devel

# Vosk speech recognition shared library
sudo dnf install vosk-api-devel

# parec — used at runtime to capture loopback audio
# Already included with pulseaudio-utils on Fedora
```

---

## Data / Model Files (not in repo)

| File | Purpose | Size |
|------|---------|------|
| `~/.local/share/vid_translate/vosk-model/` | Vosk English model directory | ~40 MB |

### Download the Vosk model

```bash
mkdir -p ~/.local/share/vid_translate/
curl -L https://alphacephei.com/vosk/models/vosk-model-small-en-us-0.15.zip \
     -o /tmp/vosk-model.zip
unzip /tmp/vosk-model.zip -d /tmp/
mv /tmp/vosk-model-small-en-us-0.15 ~/.local/share/vid_translate/vosk-model
```

> A `whisper-rs` dependency is also in `Cargo.toml` with `ggml-tiny.bin` /
> `ggml-base.bin` models at `~/.local/share/vid_translate/models/` — these
> were used in an earlier batch-transcription approach and are currently unused.
> They can be removed if disk space is a concern.

---

## Running the App

```bash
cd ~/Desktop/trans_vid/vid_translate
npm run tauri dev      # development (hot-reload frontend)
npm run tauri build    # production build
```

First compile takes 5–15 minutes (compiles whisper.cpp + vosk bindings from source).  
Subsequent builds are fast.

---

## Architecture & Data Flow

```
parec (subprocess)
  --device alsa_output.<sink>.monitor   ← loopback of speakers
  --format s16le --rate 16000 --channels 1
        │
        │  250ms chunks of i16 samples
        ▼
audio::start_capture()          [src-tauri/src/audio.rs]
  — spawns parec, reads stdout in a loop
  — sends Vec<i16> via mpsc channel
        │
        ▼
recognizer::run()               [src-tauri/src/recognizer.rs]
  — Vosk Model + Recognizer (16 kHz)
  — accept_waveform() every 250ms
  — DecodingState::Running  → emit "transcription" { type: "partial" }
  — DecodingState::Finalized → emit "transcription" { type: "final" }
        │
        │  Tauri events
        ▼
App.jsx                         [src/App.jsx]
  — listen("transcription")
  — partial → slideWindow(last 10 words), highlight last word
  — final   → show all white, clear after 2500ms
```

---

## Key Files Explained

### `src-tauri/src/audio.rs`
Spawns `parec` as a child process targeting the PulseAudio monitor source
(loopback of whatever is playing). Finds the device by running
`pactl get-default-sink` and appending `.monitor`. Reads 250 ms chunks of
S16LE mono 16 kHz audio and sends them over an `mpsc` channel.

Using `parec` as a subprocess (rather than Rust libpulse bindings directly)
proved more reliable for PipeWire's PulseAudio compatibility layer on Fedora.

### `src-tauri/src/recognizer.rs`
Wraps the Vosk `Model` + `Recognizer`. Processes each audio chunk
synchronously — Vosk takes < 10 ms per 250 ms chunk, so it keeps up in
real-time. Returns `Partial`, `Final`, or `Silent` per chunk.

### `src-tauri/src/lib.rs`
Manages the pipeline with `Mutex<PipelineState>` (holds an `Arc<AtomicBool>`
stop flag + thread handle). Exposes three Tauri commands:

| Command | Description |
|---------|-------------|
| `start_listening` | Starts audio capture + recognizer thread |
| `stop_listening` | Sets stop flag; thread exits on next chunk |
| `get_model_path` | Returns expected Vosk model path for setup UI |

Emits two Tauri events to the frontend:

| Event | Payload |
|-------|---------|
| `transcription` | `{ text: string, type: "partial" \| "final" }` |
| `status` | `{ state: "loading" \| "listening" \| "idle" \| "error" \| "model_missing" }` |

### `src/App.jsx`
Single caption state — one array of words + a boolean `isPartial`.  
No separate "finals array" to avoid duplication bugs.

- **Partial**: `slideWindow()` keeps last 10 words. All words gray except
  the last (current word) which is white with a soft glow.
- **Final**: All words white. Cleared after `FINAL_LINGER_MS` (2500 ms).
- A single `clearTimer` ref handles the expiry; it resets on every new event.

### `src/App.css`
- `html/body/#root`: transparent background (required for frameless overlay)
- `.bar`: `rgba(0,0,0,0.78)` with `backdrop-filter: blur(8px)`, `border-radius: 10px`
- Font: `clamp(14px, 2.2vw, 32px)` — scales proportionally with window width
- `data-tauri-drag-region` on bar + transcript area makes the whole surface draggable

### `src-tauri/tauri.conf.json`
Window: `decorations: false`, `alwaysOnTop: true`, `transparent: true`,
`resizable: true`, `minWidth: 400`, `minHeight: 60`, starts at `1200×90`
positioned near the bottom of a 1080p screen (`y: 950`).

---

## Tunable Constants

| File | Constant | Default | Effect |
|------|----------|---------|--------|
| `App.jsx` | `MAX_WORDS` | `10` | Words visible at once in sliding window |
| `App.jsx` | `FINAL_LINGER_MS` | `2500` | Ms a finalised sentence stays on screen |
| `App.css` | `font-size` clamp | `2.2vw` | Font size relative to window width |
| `audio.rs` | `CHUNK_SAMPLES` | `RATE/4` = 4000 | Audio chunk size (250 ms) |

---

## Why Vosk Instead of Whisper

Whisper (even the tiny model) is a **batch encoder-decoder** — it always
processes a 30-second internal window, taking 2–4 seconds per call on CPU.
This creates unavoidable multi-second lag.

Vosk is a **streaming CTC model** — it processes 250 ms chunks in < 10 ms,
outputting partial words as they are spoken (~100–200 ms end-to-end latency).
This gives the YouTube-captions feel the project requires.

Whisper code is kept in `transcriber.rs` / `Cargo.toml` for a future
"translate to English" mode for non-English audio, which would require
accepting higher latency.

---

## Known Limitations & Future Work

| Issue | Notes |
|-------|-------|
| English only | Vosk model is English. Non-English audio won't transcribe correctly. To add language support, download the appropriate Vosk language model and let user select. |
| No translation | For non-English → English, Whisper (`transcriber.rs`) is already wired up; needs a UI toggle and mode switch in `lib.rs`. |
| Model loads twice on rapid start/stop | `drop(h)` doesn't wait for the thread; rapid toggle can start a new Vosk load before the old one exits. Fix: `h.join()` with a timeout, or a proper cancellation token. |
| Vosk logs to stderr | `LOG (VoskAPI:...)` lines appear in the terminal. Suppress by redirecting stderr in the Vosk init, or setting `VOSK_LOG_LEVEL=0` env var. |
| Position not persisted | Window always starts at `x:0, y:950`. Save/restore position using Tauri's `window.outerPosition()` + app state file. |
