# VidTranslate — Handoff Document

Real-time subtitle overlay widget that captures system audio and transcribes it live.  
Built on **Tauri v2 + React 19 (Vite)** with a **Rust** backend.

---

## What It Does

- Sits as a frameless, always-on-top, transparent bar over the desktop
- Captures whatever is playing through the speakers (the system audio mix) on
  Linux, Windows and macOS
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
│   │   ├── audio/              # System audio capture (linux.rs / windows.rs / macos.rs)
│   │   ├── marian.rs           # Offline CTranslate2 translation (TEST LOCAL)
│   │   └── recognizer.rs       # Vosk streaming recognizer
│   ├── Cargo.toml              # Rust dependencies
│   ├── tauri.conf.json         # Window config (shared)
│   ├── tauri.linux.conf.json   # ─┐
│   ├── tauri.windows.conf.json #  ├ per-platform bundle overlays
│   ├── tauri.macos.conf.json   # ─┘
│   ├── Info.plist              # macOS: merged into the bundle (mic usage string)
│   ├── entitlements.plist      # macOS: only used when signing with a real identity
│   ├── vendor/
│   │   ├── linux-x86_64/       # libvosk.so (committed)
│   │   └── macos/              # libvosk.dylib (fetched, gitignored)
│   └── capabilities/
│       └── default.json        # Tauri permissions
├── scripts/
│   └── fetch-libvosk-macos.sh  # Downloads the universal2 libvosk for macOS builds
├── index.html
├── package.json
└── vite.config.js
```

---

## System Dependencies (must be installed)

`libvosk` is no longer a system package on any platform — Linux and Windows vendor it in
`src-tauri/`, macOS fetches it (see below).

### Linux (Fedora)

```bash
# PulseAudio/PipeWire development library (needed to build)
sudo dnf install pulseaudio-libs-devel

# parec — used at runtime to capture loopback audio
# Already included with pulseaudio-utils on Fedora
```

### macOS

```bash
xcode-select --install          # Command Line Tools
brew install cmake              # CTranslate2 is CMake-built by ct2rs

bash scripts/fetch-libvosk-macos.sh   # once — puts libvosk.dylib in src-tauri/vendor/macos/
```

Plus one **audio-recording permission** at runtime, which macOS prompts for — see "macOS specifics" below.

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

---

## Running the App

```bash
cd ~/Desktop/trans_vid/vid_translate
npm run tauri dev      # development (hot-reload frontend)
npm run tauri build    # production build
```

First compile takes 5–15 minutes (compiles vosk bindings from source).  
Subsequent builds are fast.

---

## Architecture & Data Flow

```
Platform capture backend                [src-tauri/src/audio/]
  Linux    parec --device <sink>.monitor --format s16le --rate 16000 --channels 1
  Windows  WASAPI loopback on the default render device (autoconvert to 16k mono)
  macOS    Core Audio process tap on the system mix, downmixed + resampled to 16k
        │
        │  250ms chunks of i16 samples — identical contract on all three
        ▼
audio::start_capture()          [src-tauri/src/audio/mod.rs]
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

### `src-tauri/src/audio/`
`mod.rs` cfg-switches between three backends that all satisfy the same contract:
a `Receiver<Vec<i16>>` yielding 250 ms chunks of 16 kHz mono audio. Nothing
downstream knows which platform captured it.

**`linux.rs`** — spawns `parec` as a child process targeting the PulseAudio monitor
source (loopback of whatever is playing), found via `pactl get-default-sink` + `.monitor`.
Using `parec` as a subprocess rather than Rust libpulse bindings proved more reliable
for PipeWire's PulseAudio compatibility layer on Fedora.

**`windows.rs`** — opens the default *render* device for capture, which is how WASAPI
expresses loopback. `autoconvert: true` makes the shared-mode audio engine resample and
downmix to 16 kHz mono for us.

**`macos.rs`** — the odd one out, and the only backend with two paths. System audio comes
from a Core Audio process tap (`tap.rs`), drained by `tap_capture_loop`; the microphone
fallback still goes through `cpal` + `build_stream`, because enumeration is exactly right for
a real input device. The tap reports its own native format (48kHz stereo on built-in output,
44.1kHz over Bluetooth), so this backend owns the downmix + resample to 16 kHz that the other
two get for free. `Resampler` is shared by both paths and is the regression net for the chunk
contract — its two tests are the reason the tap rewrite could be trusted.

`mod.rs` also exports `preflight()`, `capture_fault()` and `set_prefer_microphone()` — no-ops
on Linux/Windows, real on macOS — plus the shared `CaptureFault` enum, whose `status()` maps a
fault to the frontend status string. `preflight()` only checks what is knowable before
starting (the OS version); permission cannot be checked up front, because the tap API reports
success either way.

**`tap.rs`** — all the `unsafe`. The `extern "C"` declarations bindgen skips, the
`CATapDescription` construction, the aggregate `CFDictionary`, the IOProc, the
default-output listener, the lock-free `Ring`, and `Drop`-based teardown. Read the module
comment before touching it; see also "macOS specifics" below.

### `src-tauri/src/recognizer.rs`
Wraps the Vosk `Model` + `Recognizer`. Processes each audio chunk
synchronously — Vosk takes < 10 ms per 250 ms chunk, so it keeps up in
real-time. Returns `Partial`, `Final`, or `Silent` per chunk.

### `src-tauri/src/lib.rs`
Manages the pipeline with `Mutex<PipelineState>` (holds an `Arc<AtomicBool>`
stop flag + thread handle). Exposes three Tauri commands:

| Command | Description |
|---------|-------------|
| `start_listening` | Starts audio capture + recognizer thread. Args: `mode`, `ollamaKey`, `ollamaModel`, `useLocalTranslation`, `preferMicrophone` (macOS) |
| `stop_listening` | Sets stop flag; thread exits on next chunk |
| `download_vosk_model` | Downloads + extracts a Vosk speech model, emits progress |
| `download_ct2_model` | Downloads a CTranslate2 translation model, emits progress |
| `local_model_exists` | Whether a CTranslate2 model is already on disk |
| `pull_model` | Streams `ollama pull` progress |

Emits two Tauri events to the frontend:

| Event | Payload |
|-------|---------|
| `transcription` | `{ text: string, type: "partial" \| "final" }` |
| `status` | `{ state: "loading" \| "listening" \| "idle" \| "error" \| "model_missing" \| "vosk_{ja,es}_model_missing" \| "ct2_{ja,es}_model_missing" \| "audio_permission_denied" \| "audio_tap_unavailable" }` |

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
`shadow: false`, `resizable: true`, `minWidth: 420`, `minHeight: 72`, starts at
`1200×100`, `center: true`. `macOSPrivateApi: true` sits one level up under `app` —
without it the transparent window is an opaque slab on macOS. The macOS window's
Spaces/Mission Control behaviour is *not* configurable here; see below.

---

## macOS specifics

Everything here is non-obvious and cost real debugging time — read before touching the
mac build.

**System audio comes from a Core Audio process tap.** Linux and Windows tap the output mix
directly; macOS gave ordinary apps no way to until 14.4. The app used to require a *virtual
loopback driver* (BlackHole) plus a hand-built Multi-Output Device — six manual steps that
also broke the keyboard volume keys, captured nothing after a switch to Bluetooth, and fed
silence to anyone who muted their output. All of that is gone. `audio/tap.rs` creates a
`CATapDescription` global tap, wraps it in a **private** aggregate device, and reads it with
an IOProc. `audio/macos.rs` drains that into the same 250ms/16kHz/mono/i16 chunks the other
backends produce.

Three things about taps are not discoverable from Apple's headers, and each one cost real
debugging time:

1. **A tap is not readable on its own.** It must be wrapped in an aggregate device whose
   *main sub-device* is a real output device — that device clocks the aggregate. The
   aggregate here is created with `kAudioAggregateDeviceIsPrivateKey`, so it never appears in
   Audio MIDI Setup and is never the system output. That is what keeps the user's device
   choice and their volume keys working, and it is also why `cpal` cannot be used on this
   path: a private aggregate is invisible to device enumeration, and cpal's API starts from
   enumeration. We drive it by `AudioObjectID` instead.

2. **`AudioHardwareCreateProcessTap` returns `noErr` even when TCC denied permission.** There
   is no error to check, ever. A denied tap either delivers buffers of bit-exact zeros
   forever, or stops delivering buffers at all — both observed. So `Ring` tracks the frame
   count *and* the OR of every sample's raw bits, and `silence_verdict()` turns
   "buffers arriving, all zero, for 6s" into `CaptureFault::PermissionDenied` and "no buffers
   at all for 3s" into `CaptureFault::NoAudioFrames`. Heuristics, unavoidably.

3. **The capture thread must service a run loop.** TCC cannot present its authorization
   dialog to a process that never pumps one, and *silently denies* instead. A
   `thread::sleep` drain loop reproduced the denial every single time; swapping it for
   `CFRunLoopRunInMode` reproduced a working capture every time. This is why
   `tap_capture_loop` pumps the run loop instead of sleeping when the ring is empty — it is
   load-bearing, not stylistic. Pumping also delivers the default-output property-listener
   callbacks.

**The IOProc is a realtime thread.** It may only copy into the preallocated lock-free ring
and bump atomics. No allocation, no `Mutex`, no `mpsc::Sender::send` (std's channel allocates
per send), no `eprintln!`, no ObjC messages, no CoreAudio property calls, no panics. All
resampling, `i16` conversion, chunking and channel sending happen on the supervisor thread.
`Resampler` allocates, so it must never be called from the IOProc.

**Output-device changes rebuild the aggregate, not the tap.** The property listener on
`kAudioHardwarePropertyDefaultOutputDevice` only sets a flag — calling back into CoreAudio
from inside a listener is a documented deadlock source. The supervisor debounces 300ms
(connecting AirPods fires the property several times as the device appears, is selected and
settles), then stops the IOProc, destroys and recreates the aggregate against the new output,
and clears the ring so pre-switch frames are not spliced in. The tap survives, which avoids a
second TCC evaluation, and the pipeline above never stops — Vosk just sees a short gap. If
the new tap format differs (44.1kHz Bluetooth vs 48kHz built-in) the `Resampler` is rebuilt
and the partial chunk dropped.

**Teardown order matters.** `AudioDeviceStop` → `DestroyIOProcID` →
`DestroyAggregateDevice` → `DestroyProcessTap` → remove listener, and it must also run on
`SystemTap::new`'s error paths — hence building the struct incrementally and letting `Drop`
clean up. A leaked aggregate leaves a phantom device behind until reboot.

**Permission is tied to the code signature.** Release builds are ad-hoc signed, so the cdhash
changes on every build and the TCC grant evaporates while System Settings still lists the
stale entry as enabled. Expect to hit the permission screen constantly in development and
after every user-facing update; its copy is written to read as routine
("Not hearing any audio") rather than as an accusation. A stable
`codesign --identifier` helps but does not fix it; only a Developer ID would.

**`libvosk.dylib` is fetched, not committed.** `scripts/fetch-libvosk-macos.sh` pulls
Vosk's `universal2` wheel from PyPI (`vosk/libvosk.dyld` inside — note the odd `.dyld`
extension) rather than the GitHub release, because the wheel filename and contents are
queryable from the PyPI JSON API instead of guessed at. **Vosk 0.3.45 has no macOS build**
— 0.3.44 is the newest that does, so the script resolves the newest `universal2` wheel
dynamically rather than pinning.

**The dylib needs its install name rewritten.** Vosk ships it with a bare `libvosk.dylib`
install name, which dyld resolves against system paths only — never the rpaths `build.rs`
embeds — so the copy in `Contents/Frameworks` would be ignored. The script rewrites it to
`@rpath/libvosk.dylib`, then **re-ad-hoc-signs it**, because `install_name_tool`
invalidates the existing signature and an invalid signature is fatal on Apple Silicon.

**Deployment target must be ≥ 14.4.** That is where Core Audio process taps became
dependable, and system-audio capture is the whole app — there is no driverless route below
it. Set in `tauri.macos.conf.json`, `Info.plist` (`LSMinimumSystemVersion`) and the workflow
env; all three must agree.

**Builds are per-architecture, not universal.** `ct2rs` CMake-builds CTranslate2 for the
host arch only (it sets `CMAKE_OSX_ARCHITECTURES=arm64` itself), so a
`universal-apple-darwin` target would fail to link — which is why this is a per-arch matrix
job rather than one universal build. Since `da4f615` CI runs **only** `macos-14` (Apple
Silicon) and ships one aarch64 DMG; Intel is deliberately not built. The build is baseline
— no `-mcpu=native` anywhere — so that DMG covers every M-series chip, not just the one CI
built on.

**Signing.** Tauri only codesigns when a real identity is configured, so CI ad-hoc signs
the finished `.app` itself (`codesign --force --deep --sign -`) and then builds the DMG
around it with `hdiutil`. Ad-hoc means unnotarized, so Gatekeeper blocks first launch:
right-click → Open, or `xattr -dr com.apple.quarantine`. `entitlements.plist` exists for
whenever real Developer ID signing happens — under the hardened runtime,
`disable-library-validation` is required or the app refuses to load `libvosk.dylib`.

**Two TCC categories, two usage strings.** `NSAudioCaptureUsageDescription` gates the
process tap; `NSMicrophoneUsageDescription` gates the microphone fallback. Both live in
`src-tauri/Info.plist` and are merged into the bundle by tauri-bundler. They are *different*
permissions — "System Audio Recording" and "Microphone" in System Settings — so reading one
tells you nothing about the other. Omitting either is silent: the tap path denies without
error, and the mic path kills the app the moment the stream starts.

---

## Tunable Constants

| File | Constant | Default | Effect |
|------|----------|---------|--------|
| `App.jsx` | `MAX_WORDS` | `10` | Words visible at once in sliding window |
| `App.jsx` | `FINAL_LINGER_MS` | `2500` | Ms a finalised sentence stays on screen |
| `App.css` | `font-size` clamp | `2.2vw` | Font size relative to window width |
| `audio/*.rs` | `CHUNK_SAMPLES` | `RATE/4` = 4000 | Audio chunk size (250 ms) — same on all 3 backends |

---

## Why Vosk Instead of Whisper

Whisper (even the tiny model) is a **batch encoder-decoder** — it always
processes a 30-second internal window, taking 2–4 seconds per call on CPU.
This creates unavoidable multi-second lag.

Vosk is a **streaming CTC model** — it processes 250 ms chunks in < 10 ms,
outputting partial words as they are spoken (~100–200 ms end-to-end latency).
This gives the YouTube-captions feel the project requires.

For non-English → English, the JA/ES modes use Vosk (Japanese/Spanish
models) for recognition and Ollama for translation, keeping the
streaming feel while still producing English output.

---

## Known Limitations & Future Work

| Issue | Notes |
|-------|-------|
| Model loads twice on rapid start/stop | `drop(h)` doesn't wait for the thread; rapid toggle can start a new Vosk load before the old one exits. Fix: `h.join()` with a timeout, or a proper cancellation token. |
| Vosk logs to stderr | `LOG (VoskAPI:...)` lines appear in the terminal. Suppress by redirecting stderr in the Vosk init, or setting `VOSK_LOG_LEVEL=0` env var. |
| macOS 14.4+ only | Core Audio process taps do not exist below 14.4, and the BlackHole fallback was deliberately deleted rather than maintained. A machine below the floor gets `audio_tap_unavailable` and the microphone fallback. Restoring older support would mean reinstating the whole loopback-driver path. |
| TCC grant dies on every update | macOS ties the audio-recording permission to the code signature, and releases are ad-hoc signed, so updating the app silently revokes it while System Settings still shows it enabled. A Developer ID certificate is the only real fix; until then the permission screen is a routine part of the flow. |
| Permission denial is detected heuristically | The tap API reports success even when denied, so the only signal is "buffers arriving, every sample bit-exact zero". `silence_verdict()` waits 6s before calling it. A genuinely silent 6s with nothing playing is indistinguishable in principle — it is only safe because the verdict clears permanently on the first non-zero sample. |
