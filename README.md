# vid_translate 🎙️

A transparent, always-on-top, frameless **live caption & translation overlay** for your desktop — built with **Tauri v2 + React**.

It listens to your **system audio** (whatever is playing — YouTube, a meeting, a movie) and shows YouTube-style live captions at the bottom of your screen:

- **EN** — live English captions (streaming, ~100–200 ms latency) via [Vosk](https://alphacephei.com/vosk/)
- **JA** — Japanese speech → **English translation** (Vosk recognition + Ollama translation)
- **ES** — Spanish speech → **English translation** (Vosk recognition + Ollama translation)

The widget is draggable, remembers its position and size, spans the full display width by default, and stays out of your way with a click-through-friendly transparent design.

---

## ✨ Features

- 🪟 Frameless, transparent, always-on-top overlay widget
- ⚡ Real-time streaming captions (Vosk processes 250 ms chunks in <10 ms)
- 🌐 Japanese / Spanish → English translation via Ollama (local or ollama.com with an API key)
- 📥 One-click speech-model download (no manual setup)
- 🔴 **LIVE** toggle — show only the current spoken sentence, hide the translation history
- 🖱️ Drag anywhere, resize freely — size & position persist across launches
- 🎨 Settings panel: font, font scale, opacity, widget width & heights, with a Reset button
- 🖥️ Cross-platform: Linux (PulseAudio/PipeWire) and Windows (WASAPI loopback)

---

## 📁 Project Structure

```
vid_translate/
├── index.html                      # Single HTML page Tauri loads (mounts #root)
├── package.json                    # Frontend deps (React 19, Vite 7, @tauri-apps/api) & scripts
├── vite.config.js                  # Vite dev-server config for Tauri (port 1420)
├── README.md                       # This file
├── HANDOFF.md                      # Developer handoff notes / design rationale
├── public/
│   └── vite.svg                    # Favicon
├── src/                            # ── React frontend ──
│   ├── main.jsx                    # React entry point (mounts <App />)
│   ├── App.jsx                     # Entire UI: modes, start/stop, LIVE toggle, settings,
│   │                               #   window sizing/position persistence, backend events
│   └── App.css                     # All styling: overlay bar, buttons, settings, animations
├── dist/                           # Vite build output (generated — embedded in release builds)
└── src-tauri/                      # ── Rust backend ──
    ├── Cargo.toml                  # Rust deps: tauri 2, vosk, ureq, zip, dirs, serde, tokio,
    │                               #   wasapi (Windows-only)
    ├── Cargo.lock                  # Pinned dependency versions
    ├── build.rs                    # Linux: links vendor/libvosk.so, embeds $ORIGIN rpaths
    ├── tauri.conf.json             # Window config (frameless, transparent, always-on-top),
    │                               #   bundle resources (Windows DLLs), dev URL
    ├── tauri.linux.conf.json       # Linux-only overlay: bundles libvosk.so into the package
    ├── capabilities/
    │   └── default.json            # Tauri v2 permissions (drag, resize, reposition, close…)
    ├── src/
    │   ├── main.rs                 # Executable entry point → calls vid_translate_lib::run()
    │   ├── lib.rs                  # The heart: Tauri commands (start/stop_listening,
    │   │                           #   download_vosk_model, pull_model), model paths,
    │   │                           #   EN pipeline, JA/ES translation pipeline (Ollama),
    │   │                           #   TranscriptionEvent / StatusEvent emission
    │   ├── recognizer.rs           # Vosk streaming recognizer wrapper (Partial/Final/Silent)
    │   └── audio/
    │       ├── mod.rs              # SAMPLE_RATE = 16000, cfg-switch between platforms
    │       ├── linux.rs            # Linux system-audio capture (parec / PulseAudio)
    │       └── windows.rs          # Windows system-audio capture (WASAPI loopback)
    ├── vendor/
    │   └── linux-x86_64/
    │       └── libvosk.so          # Vosk shared library for Linux builds
    ├── libvosk.dll                 # ┐
    ├── libvosk.lib                 # │ Vosk + MinGW runtime libraries
    ├── libgcc_s_seh-1.dll          # │ vendored for Windows builds
    ├── libstdc++-6.dll             # │ (bundled as resources)
    ├── libwinpthread-1.dll         # ┘
    ├── icons/                      # App icons for every platform (ico, icns, PNGs)
    ├── gen/schemas/                # Generated capability JSON schemas (do not edit)
    └── target/                     # Cargo build output, incl. release bundles (generated)
```

---

## 🌐 How the EN / JA / ES modes work

The **mode button** in the widget bar cycles through the three modes (click it while stopped: `EN → JA → ES → EN …`). Each mode is a different pipeline under the hood:

<details>
<summary><b>🇬🇧 EN — Live English captions</b></summary>

<br>

1. System audio is captured at 16 kHz mono (PulseAudio on Linux, WASAPI loopback on Windows).
2. 250 ms chunks are streamed into the **Vosk English model** (`vosk-model-small-en-us-0.15`).
3. Vosk emits **partial** results (the sentence being spoken right now, updating live) and **final** results (completed utterances).
4. Captions appear instantly in the overlay — no translation step, no network, fully offline.

**Latency:** ~100–200 ms end-to-end — the "YouTube captions" feel.

</details>

<details>
<summary><b>🇯🇵 JA — Japanese speech → English translation</b></summary>

<br>

1. Same audio capture, but streamed into the **Vosk Japanese model** (`vosk-model-small-ja-0.22`).
2. Live Japanese text is shown at the bottom of the widget as it's spoken (the dimmed line).
3. When Vosk **finalizes** an utterance, it's sent to a background worker that calls **Ollama** with a translation prompt.
4. The English translation **streams in word-by-word** and is added to a scrolling history above the live line.

**Ollama options** (in ⚙ Settings):
- **Local** — leave the API key empty; the app talks to `http://localhost:11434`. Pull a model first (e.g. `ollama pull gemma3:27b`) or use the in-app **pull** with progress.
- **Cloud** — set an [ollama.com](https://ollama.com) API key to use hosted models instead.

**LIVE toggle:** press **LIVE** to hide the history and show *only* the current spoken Japanese, big and centered — useful when you just want to shadow speech.

</details>

<details>
<summary><b>🇪🇸 ES — Spanish speech → English translation</b></summary>

<br>

Identical to JA mode, but uses the **Vosk Spanish model** (`vosk-model-small-es-0.42`) for recognition. Live Spanish appears at the bottom; finalized sentences are translated to English via Ollama and pushed into the history. The **LIVE** toggle works the same way.

</details>

<details>
<summary><b>📥 Speech models — auto-download</b></summary>

<br>

The first time you start a mode whose model is missing, the widget shows a **Download** button. One click fetches the model from `alphacephei.com`, shows progress, and extracts it to:

```
~/.local/share/vid_translate/          (Linux)
%LOCALAPPDATA%\vid_translate\          (Windows)
    ├── vosk-model        # English
    ├── vosk-model-ja     # Japanese
    └── vosk-model-es     # Spanish
```

No manual steps needed.

</details>

---

## 🚀 Install & Run Locally (development)

### Prerequisites

| Requirement | Notes |
|---|---|
| **Node.js** ≥ 18 + npm | Frontend tooling |
| **Rust** (stable) + Cargo | Install via [rustup](https://rustup.rs) |
| **Tauri v2 system deps** | Linux: `webkit2gtk-4.1`, `libappindicator`, etc. — see [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) |
| **Ollama** *(optional)* | Only needed for JA/ES translation — [ollama.com/download](https://ollama.com/download) |

### Steps

```bash
git clone <repo-url>
cd vid_translate
npm install
npm run tauri dev
```

- First compile takes **5–15 minutes** (builds the Vosk bindings). Later builds are fast.
- The frontend hot-reloads; Rust changes trigger a rebuild.

---

## 📦 Building for Release

### 🐧 Linux (AppImage)

```bash
cd vid_translate
export APPIMAGE_EXTRACT_AND_RUN=1
export NO_STRIP=1
npx tauri build --bundles appimage
```

**Why the environment variables?**

- `APPIMAGE_EXTRACT_AND_RUN=1` — the bundler's own tools (`linuxdeploy`, `appimagetool`) are themselves AppImages that need FUSE to mount. This flag makes them self-extract and run directly, so the build works on systems without (working) FUSE.
- `NO_STRIP=1` — the bundler normally strips binaries, but `strip` corrupts the prebuilt `libvosk.so` on some systems (e.g. Fedora's binutils). This skips stripping.

Output lands in:

```
src-tauri/target/release/bundle/appimage/vid_translate_0.0.1_amd64.AppImage
```

First run — make it executable:

```bash
chmod +x src-tauri/target/release/bundle/appimage/vid_translate_0.0.1_amd64.AppImage
./src-tauri/target/release/bundle/appimage/vid_translate_0.0.1_amd64.AppImage
```

> `libvosk.so` is bundled inside the AppImage (via `tauri.linux.conf.json` + rpath magic in `build.rs`) — no system-wide Vosk install needed.

### 🪟 Windows

On a Windows machine with Rust + Node installed:

```powershell
cd vid_translate
npx tauri build
```

This produces an `.msi` / NSIS installer under:

```
src-tauri\target\release\bundle\
```

The required DLLs (`libvosk.dll`, `libgcc_s_seh-1.dll`, `libstdc++-6.dll`, `libwinpthread-1.dll`) are vendored in `src-tauri/` and bundled automatically as resources. Audio capture uses **WASAPI loopback**, so it hears whatever the system is playing.

---

## ⚙️ Settings

Open with the **⚙** button. Everything persists in `localStorage`:

| Setting | Default | Notes |
|---|---|---|
| Ollama API key | *(empty)* | Empty = local Ollama at `localhost:11434` |
| Ollama model | `gemma3:27b` | Any model Ollama can run/pull |
| Font / Font scale | System UI / 1.0× | |
| Opacity | 0.78 | Overlay background transparency |
| Width | *(full display)* | Set a px value for a narrower widget (also makes it draggable on both axes) |
| EN height / JA-ES height | 90 / 280 px | Per-mode widget heights |
| **Reset** | | Restores UI defaults but keeps your Ollama key & model |

Manually resizing the window with the mouse **updates and saves** these presets automatically. The widget also remembers **where you last placed it** and reopens there.

---

## 🧠 Why Vosk (not Whisper)?

Whisper is a batch encoder-decoder — it always processes a 30-second window, adding multi-second lag. Vosk is a **streaming CTC model**: partial words appear as they're spoken, giving true live-caption latency. For JA/ES, translation quality comes from Ollama instead, keeping recognition streaming and only translating finalized sentences.

---

## 🛠️ Tech Stack

- [Tauri v2](https://v2.tauri.app/) — window shell, IPC, bundling
- [React 19](https://react.dev/) + [Vite 7](https://vitejs.dev/) — frontend
- [Vosk](https://alphacephei.com/vosk/) — streaming speech recognition (EN/JA/ES models)
- [Ollama](https://ollama.com/) — LLM translation (local or cloud)
- PulseAudio/PipeWire (`parec`) on Linux, WASAPI loopback on Windows — system audio capture
