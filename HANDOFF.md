# VidTranslate — Handoff Document

Real-time subtitle overlay widget that captures system audio and transcribes it live.  
Built on **Tauri v2 + React 19 (Vite)** with a **Rust** backend.

---

## What It Does

- Sits as a frameless, always-on-top, transparent bar over the desktop
- Captures whatever is playing through the speakers (system loopback audio) on
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

Plus a **virtual loopback driver** at runtime — see "macOS specifics" below.

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
  macOS    CoreAudio input on a virtual loopback device, downmixed + resampled to 16k
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

**`macos.rs`** — the odd one out, because macOS gives ordinary apps no system-audio API
at all. It enumerates CoreAudio input devices via `cpal` and picks a virtual loopback
driver by ranked name match (`LOOPBACK_HINTS`, most-specific first, so a real BlackHole
beats a generic "Aggregate Device"). That device reports its own native format, so this
backend also owns the downmix + resample to 16 kHz that the other two get for free.

`mod.rs` also exports `loopback_device_name()` and `set_prefer_microphone()` — no-ops
on Linux/Windows, real on macOS.

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
| `status` | `{ state: "loading" \| "listening" \| "idle" \| "error" \| "model_missing" \| "vosk_{ja,es}_model_missing" \| "ct2_{ja,es}_model_missing" \| "audio_setup_missing" }` |

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

## macOS specifics

Everything here is non-obvious and cost real debugging time — read before touching the
mac build.

**There is no system-audio API.** Linux and Windows can tap the output mix directly.
macOS cannot, so the app captures from a *virtual loopback driver* the user installs
(BlackHole, Loopback, VB-Cable…), which presents whatever is played into it as a
recordable input. `start_listening` calls `audio::loopback_device_name()` first and emits
`status: "audio_setup_missing"` if none is found, which the frontend turns into a setup
screen with a BlackHole link and a **Use microphone** fallback button. Without that
pre-flight check a session would "run" and silently transcribe nothing forever.
(The one *native* route to system audio is ScreenCaptureKit on macOS 13+ — but it lives in
the screen-recording framework, so it demands the **Screen Recording** permission, which is
why this app sticks to the driver + microphone-permission-only approach.)

**BlackHole can be installed with zero GUI steps — but not from a non-interactive shell.**
`brew install blackhole-2ch` runs a `.pkg` through `sudo`, which dies with "a terminal is
required to read the password" in any shell without a TTY (CI, agents, scripts). The
workaround is to fetch the pkg and hand it to macOS's GUI authorization dialog, which
prompts the logged-in user directly:

```bash
brew fetch --cask blackhole-2ch
pkg=$(find "$(brew --cache)/downloads" -name '*BlackHole2ch*.pkg' | head -1)
osascript -e "do shell script \"/usr/sbin/installer -pkg '$pkg' -target / && killall coreaudiod\" with administrator privileges"
```

Two gotchas: the installer claims a reboot is required — `killall coreaudiod` suffices
(coreaudiod respawns and loads the driver immediately). And when the cask's sudo step
fails, Homebrew *purges its registration*, so a later direct `installer` run leaves
`brew uninstall` thinking nothing is installed — revert with
`sudo rm -rf /Library/Audio/Plug-Ins/HAL/BlackHole2ch.driver && sudo killall coreaudiod`
(that folder is the entire install; there are no kexts or launch agents).

**The Multi-Output Device is scriptable too.** What Audio MIDI Setup calls a Multi-Output
Device is just a CoreAudio aggregate with `"stacked": 1`. Create it with
`AudioHardwareCreateAggregateDevice` using the raw dictionary keys (the SDK constant names
have churned across releases; the string literals have not):

```
{ "name": "...", "uid": "<unique>", "stacked": 1,
  "master": <speakers UID>,                       // real output = clock master
  "subdevices": [ { "uid": <speakers UID> },
                  { "uid": <BlackHole UID>, "drift": 1 } ] }   // drift-correct BlackHole
```

then point `kAudioHardwarePropertyDefaultOutputDevice` at the returned device ID. The user
keeps hearing audio while BlackHole carries the copy. Known macOS limitation, not a bug:
while any aggregate is the default output, the **keyboard volume keys stop working** —
aggregates expose no master volume control. Revert = set the default output back and
`AudioHardwareDestroyAggregateDevice` (or delete it in Audio MIDI Setup).

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

**Deployment target must be ≥ 11.0.** arm64 macOS does not exist below Big Sur, so a
lower target is rejected outright when building for Apple Silicon. `10.15` will break the
M-series job specifically while the Intel job passes — set in `tauri.macos.conf.json`,
`Info.plist` and the workflow env, all three must agree.

**Builds are per-architecture, not universal.** `ct2rs` CMake-builds CTranslate2 for the
host arch only (it sets `CMAKE_OSX_ARCHITECTURES=arm64` itself), so a
`universal-apple-darwin` target would fail to link. CI runs `macos-14` (Apple Silicon) and
`macos-13` (Intel) and ships two DMGs. Both are baseline builds — no `-mcpu=native`
anywhere — so the arm64 DMG covers every M-series chip, not just the one CI built on.

**Signing.** Tauri only codesigns when a real identity is configured, so CI ad-hoc signs
the finished `.app` itself (`codesign --force --deep --sign -`) and then builds the DMG
around it with `hdiutil`. Ad-hoc means unnotarized, so Gatekeeper blocks first launch:
right-click → Open, or `xattr -dr com.apple.quarantine`. `entitlements.plist` exists for
whenever real Developer ID signing happens — under the hardened runtime,
`disable-library-validation` is required or the app refuses to load `libvosk.dylib`.

**Microphone permission.** A loopback device is an ordinary input device as far as TCC is
concerned, so `NSMicrophoneUsageDescription` (in `src-tauri/Info.plist`, merged into the
bundle by tauri-bundler) is mandatory — without it macOS kills the app the moment the
stream starts rather than prompting.

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
| macOS loopback setup is manual | The user has to install BlackHole and build a Multi-Output Device by hand. Both steps are scriptable (see "macOS specifics" above: GUI-authorized pkg install + `AudioHardwareCreateAggregateDevice`), so an in-app guided flow could automate them. Alternatively, ScreenCaptureKit (macOS 13+) captures system audio driver-free, but requires the Screen Recording permission. |
