import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize, LogicalPosition } from "@tauri-apps/api/window";
import { Icon } from "./ui/Icon";
import { Select } from "./ui/Select";
import { Switch } from "./ui/Switch";
import { Tooltip } from "./ui/Tooltip";
import { ControlButton } from "./ui/ControlButton";
import { DragHandle } from "./ui/DragHandle";
import { PulseRing, ACTIVITY_PHASES, activityPhase } from "./ui/PulseRing";
import "./App.css";

const FINAL_LINGER_MS = 2500;
const PENDING_CHUNK_MS = 500;
const MAX_WORDS = 10;
// Matches the backend's MAX_CHUNK_WORDS (src-tauri/src/marian.rs) so the live line reads
// as "the chunk just translated", not a growing multi-chunk run-on sentence.
const MAX_PENDING_WORDS = 8;
const SETTINGS_H = 640;
// The settings console is a sidebar beside a pane of cards, so it wants a
// document-shaped window rather than the full-display strip the overlay uses.
const SETTINGS_W = 920;
// Fixed compact window height while a setup card (model download / audio setup) is
// showing — tall mode windows would otherwise leave a large invisible click-blocking
// strip around the small centered card.
const SETUP_SCREEN_H = 190;
const SETUP_STATUSES = [
  "model_missing",
  "vosk_ja_model_missing",
  "whisper_ja_model_missing",
  "whisper_es_model_missing",
  "ct2_ja_model_missing",
  "ct2_es_model_missing",
  "audio_permission_denied",
  "audio_tap_unavailable",
];

const DEFAULT_SETTINGS = {
  ollamaKey: "",
  ollamaModel: "gemma3:27b",
  fontFamily: "system-ui",
  fontScale: 1.0,
  opacity: 0.78,
  width: null, // null = full display width
  enHeight: 100,
  jaHeight: 280,
  useLocalTranslation: false,
  // Which Whisper size recognises Japanese. "small" is what the accuracy numbers in docs/
  // were measured at and what an Apple Silicon machine should stay on; a slower CPU needs a
  // cheaper one, because a pass that overruns the 2s step makes the recognizer fall
  // permanently behind the audio. See WHISPER_MODELS below.
  whisperModel: "small",
  // macOS only: capture the default microphone instead of a virtual loopback device. Set
  // from the audio setup screen when the user would rather not install a loopback driver.
  preferMicrophone: false,
};

/// The Whisper sizes offered, cheapest first. Must stay in step with WHISPER_MODELS in
/// src-tauri/src/lib.rs, which owns the filenames and the download.
///
/// The q5 entries are the same models quantized: roughly a third of the size and
/// noticeably faster per pass, for a small accuracy loss. On a machine that cannot keep up
/// with `small`, `small-q5_1` is the first thing to try — it keeps far more of small's
/// accuracy than dropping to `base` does.
///
/// Note that below `small` the *translation* quality falls off faster than the
/// transcription quality: Japanese→English is the harder half of what the model is being
/// asked to do here, and `tiny` in particular produces English that is fluent and wrong.
const WHISPER_MODELS = [
  { id: "tiny-q5_1",   label: "Tiny (quantized)",  description: "31MB · fastest, rough translation" },
  { id: "tiny",        label: "Tiny",              description: "74MB · very fast, rough translation" },
  { id: "base-q5_1",   label: "Base (quantized)",  description: "57MB" },
  { id: "base",        label: "Base",              description: "141MB" },
  { id: "small-q5_1",  label: "Small (quantized)", description: "181MB · best speed/accuracy trade" },
  { id: "small",       label: "Small",             description: "465MB · recommended on Apple Silicon" },
  { id: "medium-q5_0", label: "Medium (quantized)", description: "514MB · slow on most CPUs" },
  { id: "medium",      label: "Medium",            description: "1.4GB · needs a fast machine" },
];

/// The font stacks offered for the caption surfaces.
const FONT_OPTIONS = [
  { value: "system-ui", label: "System UI" },
  { value: "Georgia, serif", label: "Georgia" },
  { value: "Arial, sans-serif", label: "Arial" },
  { value: "'Courier New', monospace", label: "Courier New" },
  { value: "'Times New Roman', serif", label: "Times New Roman" },
];

/// Sidebar categories for the settings console, in reading order.
const SETTINGS_CATEGORIES = [
  { id: "translation", label: "Translation", icon: "languages" },
  { id: "speech",      label: "Speech",      icon: "waves" },
  { id: "appearance",  label: "Appearance",  icon: "type" },
  { id: "layout",      label: "Layout",      icon: "sliders" },
];

const CATEGORY_BLURB = {
  translation: "Where translations come from, and which model does the work.",
  speech: "How speech is recognised before it is translated.",
  appearance: "How the caption overlay looks on screen.",
  layout: "How much of the display the overlay takes up.",
};

function loadSettings() {
  try {
    const stored = JSON.parse(localStorage.getItem("vt_settings") || "{}");
    // migrate away from the old defaults
    if (stored.jaHeight === 500) delete stored.jaHeight;
    if (stored.width === 1200) delete stored.width;
    return { ...DEFAULT_SETTINGS, ...stored };
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
}

function screenWidth() {
  return Math.round(window.screen.width);
}

function applySettings(s) {
  const root = document.documentElement;
  root.style.setProperty("--bg-opacity", s.opacity);
  root.style.setProperty("--font-family", s.fontFamily);
  root.style.setProperty("--font-scale", s.fontScale);
}

function slideWindow(text) {
  const words = text.trim().split(/\s+/).filter(Boolean);
  return words.slice(-MAX_WORDS);
}

// Caps the in-progress translation line to its most recent words so a long chunked
// translation (each chunk appended to the running total) can't grow into a paragraph that
// overflows the overlay — mirrors slideWindow's role for the EN caption ticker.
function capPendingWords(text) {
  const words = text.trim().split(/\s+/).filter(Boolean);
  if (words.length <= MAX_PENDING_WORDS) return text.trim();
  return words.slice(-MAX_PENDING_WORDS).join(" ");
}

// Resize while keeping whatever bottom-left corner the window is currently at
// (instead of resetting to a screen-relative default), so a manual drag isn't
// discarded the next time content changes size. Full-display-wide windows snap
// to the left edge so they actually fit on screen.
async function resizeKeepingBottom(win, width, height) {
  const scale = await win.scaleFactor();
  const curPos = (await win.outerPosition()).toLogical(scale);
  const curSize = (await win.outerSize()).toLogical(scale);
  const bottom = curPos.y + curSize.height;
  const newY = Math.max(0, bottom - height);
  const newX = width >= screenWidth() - 4 ? 0 : curPos.x;
  await win.setSize(new LogicalSize(width, height));
  await win.setPosition(new LogicalPosition(newX, newY));
}

/** One label-and-description row with its control on the right. */
function SettingsRow({ label, description, feedback, tight, children }) {
  return (
    <div className={tight ? "settings-row settings-row--tight" : "settings-row"}>
      <div className="settings-row__copy">
        <span className="settings-row__label">{label}</span>
        {description && <span className="settings-row__description">{description}</span>}
      </div>
      <div className="settings-row__control">{children}</div>
      {feedback && <div className="settings-row__feedback">{feedback}</div>}
    </div>
  );
}

function SettingsCard({ title, children }) {
  return (
    <section className="settings-card">
      <div className="settings-card__header">
        <h2>{title}</h2>
      </div>
      <div className="settings-card__body">{children}</div>
    </section>
  );
}

function SettingsPanel({ draft, setDraft, onSave, onClose, onReset }) {
  const [category, setCategory]     = useState("translation");
  const [pullInput, setPullInput]   = useState("");
  const [pullStatus, setPullStatus] = useState("idle"); // idle | pulling | done | error
  const [pullProgress, setPullProgress] = useState(null);
  const [pullMsg, setPullMsg]       = useState("");

  useEffect(() => {
    let unlisten;
    listen("pull_progress", (e) => {
      const d = e.payload;
      if (d.status === "success") {
        setPullStatus("done");
        setPullProgress(null);
        setPullMsg("Done!");
      } else if (d.completed != null && d.total != null && d.total > 0) {
        setPullProgress({ completed: d.completed, total: d.total });
        setPullMsg(d.status || "downloading…");
      } else if (d.status === "error") {
        setPullStatus("error");
        setPullMsg("Pull failed");
      } else if (d.status) {
        setPullMsg(d.status);
      }
    }).then((fn) => { unlisten = fn; });
    return () => unlisten?.();
  }, []);

  const handlePull = async () => {
    const model = pullInput.trim() || draft.ollamaModel;
    if (!model) return;
    setPullStatus("pulling");
    setPullProgress(null);
    setPullMsg("Starting…");
    try {
      await invoke("pull_model", { model });
    } catch (e) {
      setPullStatus("error");
      setPullMsg(String(e));
    }
  };

  const upd = (key, val) => setDraft((d) => ({ ...d, [key]: val }));

  const pullFeedback = (pullProgress || pullMsg) && (
    <div
      className="settings-feedback"
      data-tone={pullStatus === "done" ? "success" : pullStatus === "error" ? "error" : "info"}
    >
      <Icon
        name={
          pullStatus === "done" ? "checkmark"
          : pullStatus === "error" ? "alert-triangle"
          : "download"
        }
      />
      <div style={{ display: "flex", flexDirection: "column", gap: 7, minWidth: 0, flex: 1 }}>
        <span>{pullMsg}</span>
        {pullProgress && (
          <div className="settings-progress">
            <div
              className="settings-progress__fill"
              style={{
                width: `${Math.round((pullProgress.completed / pullProgress.total) * 100)}%`,
              }}
            />
          </div>
        )}
      </div>
    </div>
  );

  return (
    <div className="settings-console">
      <aside className="settings-sidebar">
        <div className="settings-brand" data-tauri-drag-region>
          <span className="settings-brand__name">VidTranslate</span>
          <span className="settings-brand__label">Settings</span>
        </div>
        <nav className="settings-category-nav" aria-label="Settings categories">
          {SETTINGS_CATEGORIES.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-current={category === item.id ? "page" : undefined}
              className={
                category === item.id
                  ? "settings-category-nav__item is-selected"
                  : "settings-category-nav__item"
              }
              onClick={() => setCategory(item.id)}
            >
              <Icon name={item.icon} />
              <span>{item.label}</span>
            </button>
          ))}
        </nav>
        <div className="settings-sidebar-footer">
          <button
            type="button"
            className="settings-sidebar-action"
            onClick={onReset}
            title="Reset UI settings to defaults"
          >
            <Icon name="reset" />
            <span>Reset to defaults</span>
          </button>
        </div>
      </aside>

      <div className="settings-pane">
        <header className="settings-pane__header" data-tauri-drag-region>
          <div>
            <h1>{SETTINGS_CATEGORIES.find((c) => c.id === category).label}</h1>
            <p>{CATEGORY_BLURB[category]}</p>
          </div>
          <ControlButton icon="close" label="Close settings" onClick={onClose} />
        </header>

        <div className="settings-pane__scroll">
          {category === "translation" && (
            <div className="settings-category-panel">
              <SettingsCard title="Ollama Cloud">
                <SettingsRow
                  label="API key"
                  description="With a key, requests go to ollama.com — no local install needed."
                >
                  <input
                    type="password"
                    value={draft.ollamaKey}
                    onChange={(e) => upd("ollamaKey", e.target.value)}
                    placeholder="paste from ollama.com/settings/keys"
                    aria-label="Ollama API key"
                  />
                </SettingsRow>
                <SettingsRow label="Model" description="The model asked to translate each clause.">
                  <input
                    type="text"
                    value={draft.ollamaModel}
                    onChange={(e) => upd("ollamaModel", e.target.value)}
                    aria-label="Ollama model"
                  />
                </SettingsRow>
              </SettingsCard>

              <SettingsCard title="Offline translation">
                <SettingsRow
                  label="Translate locally"
                  description="Use a bundled offline model instead of Ollama. The first run downloads and loads it, which can take a while."
                  tight
                >
                  <Switch
                    checked={draft.useLocalTranslation}
                    onChange={(checked) => upd("useLocalTranslation", checked)}
                    aria-label="Translate locally"
                  />
                </SettingsRow>
              </SettingsCard>

              <SettingsCard title="Pull a model (local Ollama)">
                <SettingsRow
                  label="Model name"
                  description="Downloads into the Ollama instance running on this machine."
                  feedback={pullFeedback}
                >
                  <input
                    type="text"
                    value={pullInput}
                    onChange={(e) => setPullInput(e.target.value)}
                    placeholder={draft.ollamaModel || "gemma3:27b"}
                    aria-label="Model to pull"
                  />
                  <button
                    type="button"
                    className="settings-button settings-button--quiet settings-button--compact"
                    onClick={handlePull}
                    disabled={pullStatus === "pulling"}
                  >
                    {pullStatus === "pulling" ? "Pulling…" : "Pull"}
                  </button>
                </SettingsRow>
              </SettingsCard>
            </div>
          )}

          {category === "speech" && (
            <div className="settings-category-panel">
              <SettingsCard title="Whisper">
                <SettingsRow
                  label="Model size"
                  description="Used for Japanese and Spanish. Bigger is more accurate, smaller is faster. If captions lag further and further behind the audio, this machine cannot keep up with the current size — drop one. A size you have not used yet downloads the first time you press play."
                >
                  <Select
                    label="Whisper model size"
                    value={draft.whisperModel}
                    options={WHISPER_MODELS.map((m) => ({
                      value: m.id,
                      label: m.label,
                      description: m.description,
                    }))}
                    onChange={(value) => upd("whisperModel", value)}
                  />
                </SettingsRow>
              </SettingsCard>

              <SettingsCard title="Audio source">
                <SettingsRow
                  label="Capture the microphone"
                  description="macOS only. Captions what the microphone hears instead of what your Mac is playing — useful when system-audio capture is unavailable."
                  tight
                >
                  <Switch
                    checked={draft.preferMicrophone}
                    onChange={(checked) => upd("preferMicrophone", checked)}
                    aria-label="Capture the microphone"
                  />
                </SettingsRow>
              </SettingsCard>
            </div>
          )}

          {category === "appearance" && (
            <div className="settings-category-panel">
              <SettingsCard title="Type">
                <SettingsRow label="Font" description="The family the captions are set in.">
                  <Select
                    label="Caption font"
                    value={draft.fontFamily}
                    options={FONT_OPTIONS}
                    onChange={(value) => upd("fontFamily", value)}
                  />
                </SettingsRow>
                <SettingsRow label="Font size" description="Scales every caption surface at once.">
                  <div className="settings-range">
                    <input
                      type="range" min="0.5" max="2" step="0.1"
                      value={draft.fontScale}
                      onChange={(e) => upd("fontScale", parseFloat(e.target.value))}
                      aria-label="Font size"
                    />
                    <output>{draft.fontScale.toFixed(1)}×</output>
                  </div>
                </SettingsRow>
              </SettingsCard>

              <SettingsCard title="Background">
                <SettingsRow
                  label="Opacity"
                  description="How much of what is behind the overlay shows through."
                >
                  <div className="settings-range">
                    <input
                      type="range" min="0.05" max="1" step="0.05"
                      value={draft.opacity}
                      onChange={(e) => upd("opacity", parseFloat(e.target.value))}
                      aria-label="Background opacity"
                    />
                    <output>{Math.round(draft.opacity * 100)}%</output>
                  </div>
                </SettingsRow>
              </SettingsCard>
            </div>
          )}

          {category === "layout" && (
            <div className="settings-category-panel">
              <SettingsCard title="Dimensions">
                <SettingsRow
                  label="Width"
                  description="Leave empty to span the whole display."
                >
                  <input
                    type="number"
                    value={draft.width ?? ""} min={300} max={7680}
                    placeholder="full"
                    onChange={(e) => upd("width", parseInt(e.target.value) || null)}
                    aria-label="Overlay width"
                  />
                  <span className="settings-unit">px</span>
                </SettingsRow>
                <SettingsRow
                  label="English height"
                  description="The compact ticker used by English mode and by LIVE."
                >
                  <input
                    type="number"
                    value={draft.enHeight} min={60} max={400}
                    onChange={(e) => upd("enHeight", parseInt(e.target.value) || draft.enHeight)}
                    aria-label="English overlay height"
                  />
                  <span className="settings-unit">px</span>
                </SettingsRow>
                <SettingsRow
                  label="Japanese / Spanish height"
                  description="The taller canvas that shows translated history as well as the live line."
                >
                  <input
                    type="number"
                    value={draft.jaHeight} min={160} max={1200}
                    onChange={(e) => upd("jaHeight", parseInt(e.target.value) || draft.jaHeight)}
                    aria-label="Japanese and Spanish overlay height"
                  />
                  <span className="settings-unit">px</span>
                </SettingsRow>
              </SettingsCard>
              <p className="settings-help">
                Dragging the overlay's own edges also saves these, so the numbers here are
                only needed for an exact size.
              </p>
            </div>
          )}
        </div>

        <footer className="settings-pane__footer">
          <span className="settings-pane__footer-note">
            Changes apply when you save.
          </span>
          <button type="button" className="settings-button settings-button--quiet" onClick={onClose}>
            Cancel
          </button>
          <button type="button" className="settings-button settings-button--primary" onClick={onSave}>
            <Icon name="checkmark" />
            Save
          </button>
        </footer>
      </div>
    </div>
  );
}

export default function App() {
  const [words, setWords]         = useState([]);
  const [currentJa, setCurrentJa] = useState("");
  const [isPartial, setIsPartial] = useState(false);
  const [translationHistory, setTranslationHistory] = useState([]);
  const [pendingEnglish, setPendingEnglish]         = useState("");
  // True while the live English line is a re-translation of an unfinished clause (a
  // "partial-chunk", or a chunk the backend cut on a length/time guard rather than a real
  // clause boundary). Rendered dimmed, because it will be replaced as the speaker finishes.
  const [pendingProvisional, setPendingProvisional] = useState(false);
  const [japaneseStream, setJapaneseStream]         = useState("");
  const historyEndRef = useRef(null);

  const [status, setStatus]               = useState("idle");
  const [running, setRunning]             = useState(false);
  const [mode, setMode]                   = useState("vosk");
  const [liveOnly, setLiveOnly]           = useState(false);
  const [settingsOpen, setSettingsOpen]   = useState(false);
  const [downloadProgress, setDownloadProgress] = useState(null); // { kind, status, downloaded, total, error }
  const [ct2DownloadProgress, setCt2DownloadProgress] = useState(null); // same shape, local translation models
  const [whisperDownloadProgress, setWhisperDownloadProgress] = useState(null); // same shape, JA Whisper model

  const [settings, setSettings] = useState(loadSettings);
  const [draft, setDraft]       = useState(settings);

  const unlistenRefs = useRef([]);
  const clearTimer   = useRef(null);

  // Queues incoming translated chunks and reveals them one at a time, each held on screen
  // for PENDING_CHUNK_MS — without this, chunks that translate faster than a person can
  // read (which happens constantly with the local translator) would just flash by.
  const pendingQueueRef = useRef([]);
  const pendingTimerRef = useRef(null);

  const clearPendingQueue = () => {
    pendingQueueRef.current = [];
    clearTimeout(pendingTimerRef.current);
    pendingTimerRef.current = null;
  };

  const showNextPendingChunk = () => {
    if (pendingQueueRef.current.length === 0) {
      pendingTimerRef.current = null;
      return;
    }
    const next = pendingQueueRef.current.shift();
    setPendingEnglish(next.text);
    pendingTimerRef.current = setTimeout(showNextPendingChunk, PENDING_CHUNK_MS);
  };

  const enqueuePendingChunk = (text) => {
    setPendingProvisional(false);
    pendingQueueRef.current.push({ kind: "chunk", text });
    if (!pendingTimerRef.current) {
      showNextPendingChunk();
    }
  };

  // What an empty caption bar should say. Whisper's model load (487MB) and its first
  // ~1.3s window both happen before any text exists, so "Listening…" during them is a
  // small lie that reads as the app being broken. Naming the actual state instead makes
  // the same wait feel intentional, which is most of the perceived-latency problem.
  const emptyStateLabel = () => {
    if (!running) return "Press play to start";
    if (status === "loading_model" || status === "loading") return "Loading model…";
    return "Listening…";
  };

  // Apply persisted CSS vars on mount
  useEffect(() => { applySettings(settings); }, []);

  // The overlay is a floating transparent surface; the settings console is an
  // opaque document. Swapping a body class rather than restyling in place keeps
  // the window padding and background in one place (App.css).
  useEffect(() => {
    document.body.classList.toggle("settings-body", settingsOpen);
    return () => document.body.classList.remove("settings-body");
  }, [settingsOpen]);

  // Load the Whisper model while the user is still looking at the window, rather than
  // after they press Start. Fire-and-forget and idempotent on the Rust side.
  useEffect(() => {
    if (mode !== "vosk-ja" && mode !== "vosk-es") return;
    invoke("warm_whisper_model", { model: settings.whisperModel }).catch(() => {});
  }, [mode, settings.whisperModel]);

  // Auto-scroll JA history. Eager chunking can push new lines in quick succession — a
  // "smooth" scrollIntoView call gets interrupted by the next one before finishing, so it
  // can visibly stall short of the bottom. Setting scrollTop directly is instant and each
  // call always lands exactly at the bottom regardless of how fast the next one follows.
  useEffect(() => {
    const container = historyEndRef.current?.parentElement;
    if (container) {
      container.scrollTop = container.scrollHeight;
    }
  }, [translationHistory]);

  // The boundary id of the live line. A "final-chunk" carrying this id is that same line
  // finished, so the live line is cleared instead of being left on screen as a duplicate.
  const liveIdRef = useRef(0);

  const modeRef = useRef(mode);
  useEffect(() => { modeRef.current = mode; }, [mode]);
  const liveOnlyRef = useRef(liveOnly);
  useEffect(() => { liveOnlyRef.current = liveOnly; }, [liveOnly]);
  const statusRef = useRef(status);
  useEffect(() => { statusRef.current = status; }, [status]);
  const settingsOpenRef = useRef(settingsOpen);
  useEffect(() => { settingsOpenRef.current = settingsOpen; }, [settingsOpen]);
  const useLocalRef = useRef(settings.useLocalTranslation);
  useEffect(() => { useLocalRef.current = settings.useLocalTranslation; }, [settings.useLocalTranslation]);

  // Programmatic resizes go through here so the manual-resize listener below
  // can tell them apart from the user dragging a window edge.
  const expectedSizeRef = useRef(null);
  const doResize = async (width, height) => {
    expectedSizeRef.current = { width, height };
    await resizeKeepingBottom(getCurrentWindow(), width, height);
  };

  // Resize window on mount and when mode, live-only, or saved dimensions change.
  // Width defaults to the full display; JA/ES mode gets the full jaHeight so the
  // live line is never clipped; live-only collapses to the compact EN height.
  // On the very first run this also restores the last saved window position.
  const restoredPosRef = useRef(false);
  useEffect(() => {
    (async () => {
      if (settingsOpenRef.current) return; // settings panel manages its own size
      const win = getCurrentWindow();
      if (!restoredPosRef.current) {
        restoredPosRef.current = true;
        try {
          const saved = JSON.parse(localStorage.getItem("vt_pos") || "null");
          if (saved) {
            const scale = await win.scaleFactor();
            const size = (await win.outerSize()).toLogical(scale);
            await win.setPosition(
              new LogicalPosition(saved.x, Math.max(0, saved.bottom - size.height))
            );
          }
        } catch { /* ignore corrupt saved position */ }
      }
      const isJa = mode === "vosk-ja" || mode === "vosk-es";
      const h = SETUP_STATUSES.includes(status)
        ? SETUP_SCREEN_H
        : isJa && !liveOnly ? settings.jaHeight : settings.enHeight;
      await doResize(settings.width ?? screenWidth(), h);
    })();
  }, [mode, liveOnly, status, settings.width, settings.enHeight, settings.jaHeight]);

  // Remember where the user puts the widget (anchored to its bottom edge so
  // mode-height changes don't shift it) and restore it on next launch.
  useEffect(() => {
    const win = getCurrentWindow();
    let unlisten, timer;
    win.onMoved(({ payload }) => {
      clearTimeout(timer);
      timer = setTimeout(async () => {
        if (settingsOpenRef.current) return;
        const scale = await win.scaleFactor();
        const pos = payload.toLogical(scale);
        const size = (await win.outerSize()).toLogical(scale);
        localStorage.setItem(
          "vt_pos",
          JSON.stringify({ x: Math.round(pos.x), bottom: Math.round(pos.y + size.height) })
        );
      }, 300);
    }).then((fn) => { unlisten = fn; });
    return () => { unlisten?.(); clearTimeout(timer); };
  }, []);

  // When the user drags a window edge, persist the new size as the preset.
  useEffect(() => {
    const win = getCurrentWindow();
    let unlisten, timer;
    win.onResized(({ payload }) => {
      clearTimeout(timer);
      timer = setTimeout(async () => {
        if (settingsOpenRef.current) return; // don't treat the settings window as a preset
        if (SETUP_STATUSES.includes(statusRef.current)) return; // setup cards use a fixed height, not a preset
        const scale = await win.scaleFactor();
        const size = payload.toLogical(scale);
        const exp = expectedSizeRef.current;
        if (exp && Math.abs(size.width - exp.width) < 2 && Math.abs(size.height - exp.height) < 2) return;
        expectedSizeRef.current = { width: size.width, height: size.height };
        const isJa = modeRef.current === "vosk-ja" || modeRef.current === "vosk-es";
        const heightKey = isJa && !liveOnlyRef.current ? "jaHeight" : "enHeight";
        setSettings((s) => {
          const next = { ...s, width: Math.round(size.width), [heightKey]: Math.round(size.height) };
          localStorage.setItem("vt_settings", JSON.stringify(next));
          return next;
        });
      }, 350);
    }).then((fn) => { unlisten = fn; });
    return () => { unlisten?.(); clearTimeout(timer); };
  }, []);

  useEffect(() => {
    const setupListeners = async () => {
      const unlistenTx = await listen("transcription", (event) => {
        const { text, current, type: kind, id, provisional } = event.payload;

        if (modeRef.current === "vosk-ja" || modeRef.current === "vosk-es") {
          if (kind === "partial") {
            setJapaneseStream(text);
          } else if (kind === "partial-chunk") {
            // The in-progress clause, re-translated from its start on every update. This
            // replaces the live line in place and deliberately does NOT go through the
            // 500ms paced queue: the whole point is that it self-corrects as the Japanese
            // predicate lands, and a queue would show every superseded guess in turn.
            liveIdRef.current = id;
            setPendingEnglish(text);
            setPendingProvisional(true);
          } else if (kind === "streaming-en") {
            if (modeRef.current === "vosk-ja" && !useLocalRef.current) {
              // Ollama streams the accumulated translation token-by-token — already a
              // natural live ticker. Pacing those dozens of updates at 500ms each would
              // back the queue up by half a minute, so show them directly.
              setPendingEnglish(capPendingWords(text));
            } else {
              enqueuePendingChunk(capPendingWords(text));
            }
          } else if (kind === "final-chunk") {
            // A clause boundary was reached: this chunk is now immutable. Record it in
            // history and leave the paced queue running — the utterance isn't over yet.
            setTranslationHistory((h) => {
              if (h.length > 0 && h[h.length - 1] === text) return h;
              return [...h, text];
            });
            // If this is the line the user was watching live, it has been promoted; drop
            // the provisional copy rather than showing the same words twice.
            if (id && id === liveIdRef.current) {
              setPendingEnglish("");
              setPendingProvisional(false);
            }
          } else if (kind === "utterance-end") {
            clearPendingQueue();
            setPendingEnglish("");
            setPendingProvisional(false);
            setJapaneseStream("");
          } else if (modeRef.current === "vosk-ja" && useLocalRef.current) {
            // Local JA: the live line is the re-translated tail (above), not a paced
            // word-by-word reveal, so a finalized line commits straight to history.
            clearPendingQueue();
            setTranslationHistory((h) => {
              if (h.length > 0 && h[h.length - 1] === text) return h;
              return [...h, text];
            });
            setPendingEnglish("");
            setPendingProvisional(false);
            setJapaneseStream("");
          } else {
            clearPendingQueue();
            setTranslationHistory((h) => {
              if (h.length > 0 && h[h.length - 1] === text) return h;
              return [...h, text];
            });
            setPendingEnglish("");
            setPendingProvisional(false);
            setJapaneseStream("");
          }
        } else {
          clearTimeout(clearTimer.current);
          if (kind === "partial") {
            setWords(slideWindow(text));
            setCurrentJa(current || "");
            setIsPartial(true);
          } else {
            setWords(slideWindow(text));
            setCurrentJa("");
            setIsPartial(false);
            clearTimer.current = setTimeout(() => {
              setWords([]);
              setCurrentJa("");
              setIsPartial(false);
            }, FINAL_LINGER_MS);
          }
        }
      });

      const unlistenStatus = await listen("status", (event) => {
        setStatus(event.payload.state);
        if (SETUP_STATUSES.includes(event.payload.state)) {
          setRunning(false);
        }
      });

      const unlistenDownload = await listen("vosk_download_progress", (event) => {
        setDownloadProgress(event.payload);
        if (event.payload.status === "done") {
          setTimeout(() => {
            setDownloadProgress(null);
            toggleRef.current();
          }, 400);
        }
      });

      const unlistenCt2Download = await listen("ct2_download_progress", (event) => {
        setCt2DownloadProgress(event.payload);
        if (event.payload.status === "done") {
          setTimeout(() => {
            setCt2DownloadProgress(null);
            toggleRef.current();
          }, 400);
        }
      });

      const unlistenWhisperDownload = await listen("whisper_download_progress", (event) => {
        setWhisperDownloadProgress(event.payload);
        if (event.payload.status === "done") {
          setTimeout(() => {
            setWhisperDownloadProgress(null);
            toggleRef.current();
          }, 400);
        }
      });

      unlistenRefs.current = [
        unlistenTx,
        unlistenStatus,
        unlistenDownload,
        unlistenCt2Download,
        unlistenWhisperDownload,
      ];
    };

    setupListeners();
    return () => {
      unlistenRefs.current.forEach((fn) => fn());
      clearTimeout(clearTimer.current);
      clearPendingQueue();
    };
  }, []);

  const toggleRef = useRef(() => {});

  const toggle = async () => {
    if (running) {
      await invoke("stop_listening");
      setRunning(false);
      setWords([]);
      setCurrentJa("");
      setTranslationHistory([]);
      clearPendingQueue();
      setPendingEnglish("");
      setJapaneseStream("");
    } else {
      setWords([]);
      setCurrentJa("");
      setTranslationHistory([]);
      clearPendingQueue();
      setPendingEnglish("");
      setJapaneseStream("");
      setRunning(true);
      await invoke("start_listening", {
        mode,
        ollamaKey: settings.ollamaKey || null,
        ollamaModel: settings.ollamaModel || null,
        useLocalTranslation: settings.useLocalTranslation,
        preferMicrophone: settings.preferMicrophone,
        whisperModel: settings.whisperModel,
      });
    }
  };
  useEffect(() => { toggleRef.current = toggle; });

  // Wipe what is on screen without stopping the session — the overlay's own
  // "clear" control, as in Mimi. Incoming chunks simply start a fresh history.
  const clearCaptions = () => {
    setTranslationHistory([]);
    clearPendingQueue();
    setPendingEnglish("");
    setPendingProvisional(false);
    setJapaneseStream("");
    setWords([]);
    setCurrentJa("");
  };

  const MODES = ["vosk", "vosk-ja", "vosk-es"];
  const toggleMode = () => {
    if (!running) {
      setMode((m) => MODES[(MODES.indexOf(m) + 1) % MODES.length]);
    }
  };

  const toggleLocalTranslation = () => {
    if (running) return;
    setSettings((s) => {
      const next = { ...s, useLocalTranslation: !s.useLocalTranslation };
      localStorage.setItem("vt_settings", JSON.stringify(next));
      return next;
    });
  };

  const openSettings = async () => {
    setDraft({ ...settings });
    const win = getCurrentWindow();
    const screenH = window.screen.height;
    const w = Math.min(SETTINGS_W, screenWidth());
    const h = Math.min(SETTINGS_H, screenH - 40);
    const y = Math.max(0, screenH - h - 20);
    expectedSizeRef.current = { width: w, height: h };
    await win.setSize(new LogicalSize(w, h));
    await win.setPosition(new LogicalPosition(0, y));
    setSettingsOpen(true);
  };

  const overlayHeight = (s) =>
    (mode === "vosk-ja" || mode === "vosk-es") && !liveOnly ? s.jaHeight : s.enHeight;

  const closeSettings = async () => {
    setSettingsOpen(false);
    doResize(settings.width ?? screenWidth(), overlayHeight(settings));
  };

  const saveSettings = async () => {
    localStorage.setItem("vt_settings", JSON.stringify(draft));
    setSettings(draft);
    applySettings(draft);
    setSettingsOpen(false);
    doResize(draft.width ?? screenWidth(), overlayHeight(draft));
  };

  const resetSettings = () => {
    const d = { ...DEFAULT_SETTINGS, ollamaKey: draft.ollamaKey, ollamaModel: draft.ollamaModel };
    localStorage.setItem("vt_settings", JSON.stringify(d));
    setDraft(d);
    setSettings(d);
    applySettings(d);
  };

  const closeApp = () => getCurrentWindow().close();

  // ── Settings panel (replaces normal UI) ────────────────────────────────────
  if (settingsOpen) {
    return (
      <SettingsPanel
        draft={draft}
        setDraft={setDraft}
        onSave={saveSettings}
        onClose={closeSettings}
        onReset={resetSettings}
      />
    );
  }

  // ── macOS audio capture faults ───────────────────────────────────────────────
  // Linux and Windows tap the system output mix directly, so these screens only ever
  // appear on macOS. Capture there goes through a Core Audio process tap, which needs no
  // driver and no routing setup — only permission, which macOS ties to the app's code
  // signature. Release builds are ad-hoc signed, so the grant is lost on every update
  // while the stale entry still reads as enabled: the permission screen is routine, not
  // an edge case, and its copy says "we're not hearing anything" rather than accusing the
  // user of denying something.
  if (status === "audio_permission_denied" || status === "audio_tap_unavailable") {
    const useMicrophoneInstead = () => {
      setSettings((s) => {
        const next = { ...s, preferMicrophone: true };
        localStorage.setItem("vt_settings", JSON.stringify(next));
        return next;
      });
      setStatus("idle");
    };
    const retry = () => {
      setStatus("idle");
      setTimeout(() => toggleRef.current(), 100);
    };
    const denied = status === "audio_permission_denied";

    return (
      <div className="overlay" data-tauri-drag-region>
        <div className="overlay__band" data-tauri-drag-region>
          <DragHandle label="Drag to move" />
        </div>
        <div className="setup-card" data-tauri-drag-region>
          <PulseRing phase="error" compact motionEnabled={false} />
          <div className="setup-card__title" data-tauri-drag-region>
            {denied ? "Not hearing any audio" : "System audio unavailable"}
          </div>
          <p className="setup-card__message" data-tauri-drag-region>
            {denied
              ? "macOS needs permission to let VidTranslate hear what your Mac is playing. Allow it under Privacy & Security → Audio Recording, then try again."
              : "Capturing system audio needs macOS 14.4 or later. You can caption your microphone instead."}
          </p>
          <div className="setup-card__actions">
            {denied && (
              <button
                type="button"
                className="overlay-button overlay-button--primary"
                onClick={() => invoke("open_audio_privacy_settings")}
              >
                Open Privacy Settings
              </button>
            )}
            <button
              type="button"
              className={denied ? "overlay-button" : "overlay-button overlay-button--primary"}
              onClick={retry}
            >
              Try again
            </button>
            <button
              type="button"
              className="overlay-button"
              onClick={useMicrophoneInstead}
              title="Caption your microphone instead of system audio"
            >
              <Icon name="microphone" />
              Use microphone
            </button>
          </div>
        </div>
      </div>
    );
  }

  // ── Setup screens: model missing → one-click auto-download, no manual steps ──
  const MISSING_MODEL_KIND = {
    model_missing: { kind: "en", label: "English speech", type: "vosk" },
    vosk_ja_model_missing: { kind: "ja", label: "Japanese speech", type: "vosk" },
    // Japanese recognises with Whisper now. One ggml file whose size is the user's choice
    // (31MB to 1.4GB — see WHISPER_MODELS), so the progress bar earns its keep.
    whisper_ja_model_missing: { kind: "whisper", label: "Japanese speech", type: "whisper" },
    // Spanish recognises with Whisper too now — transcribing rather than translating, so
    // the Spanish caption line and the es->en model both stay. Same download as Japanese.
    whisper_es_model_missing: { kind: "whisper", label: "Spanish speech", type: "whisper" },
    ct2_ja_model_missing: { kind: "ja", label: "Japanese local translation", type: "ct2" },
    ct2_es_model_missing: { kind: "es", label: "Spanish local translation", type: "ct2" },
  };

  if (MISSING_MODEL_KIND[status]) {
    const { kind, label, type } = MISSING_MODEL_KIND[status];
    const progressState =
      type === "ct2" ? ct2DownloadProgress : type === "whisper" ? whisperDownloadProgress : downloadProgress;
    const dl = progressState && progressState.kind === kind ? progressState : null;
    const pct = dl && dl.total ? Math.round((dl.downloaded / dl.total) * 100) : null;
    const startDownload = () =>
      type === "ct2"
        ? invoke("download_ct2_model", { lang: kind })
        : type === "whisper"
        ? invoke("download_whisper_model", { model: settings.whisperModel })
        : invoke("download_vosk_model", { kind });

    return (
      <div className="overlay" data-tauri-drag-region>
        <div className="overlay__band" data-tauri-drag-region>
          <DragHandle label="Drag to move" />
        </div>
        <div className="setup-card" data-tauri-drag-region>
          {!dl && (
            <>
              <PulseRing phase="idle" compact motionEnabled={false} />
              <div className="setup-card__title" data-tauri-drag-region>{label} model required</div>
              <p className="setup-card__message" data-tauri-drag-region>
                Downloads once, then everything runs offline.
              </p>
              <div className="setup-card__actions">
                <button
                  type="button"
                  className="overlay-button overlay-button--primary"
                  onClick={startDownload}
                >
                  <Icon name="download" />
                  Download
                </button>
              </div>
            </>
          )}
          {dl && dl.status !== "error" && (
            <>
              <PulseRing phase="connecting" compact motionEnabled />
              <div className="setup-card__title" data-tauri-drag-region>
                {dl.status === "downloading" ? `Downloading ${label} model…` : "Extracting…"}
              </div>
              <div className="setup-card__progress">
                <div
                  className={
                    dl.status === "downloading" && pct !== null
                      ? "setup-card__progress-fill"
                      : "setup-card__progress-fill setup-card__progress-fill--indeterminate"
                  }
                  style={{ width: dl.status === "downloading" && pct !== null ? `${pct}%` : "100%" }}
                />
              </div>
              {dl.status === "downloading" && pct !== null && (
                <span className="setup-card__percent" data-tauri-drag-region>{pct}%</span>
              )}
            </>
          )}
          {dl && dl.status === "error" && (
            <>
              <PulseRing phase="error" compact motionEnabled={false} />
              <div className="setup-card__title" data-tauri-drag-region>Download failed</div>
              <p className="setup-card__message" data-tauri-drag-region>{dl.error}</p>
              <div className="setup-card__actions">
                <button
                  type="button"
                  className="overlay-button overlay-button--primary"
                  onClick={startDownload}
                >
                  Retry
                </button>
              </div>
            </>
          )}
        </div>
      </div>
    );
  }

  // ── JA / ES translation mode ────────────────────────────────────────────────
  if (mode === "vosk-ja" || mode === "vosk-es") {
    const isEs = mode === "vosk-es";
    // A "final-chunk" pushes its text into history but deliberately leaves the pending
    // queue/display running (the utterance isn't over yet). If that chunk was the one
    // currently on screen, it now reads as both the bold "latest" history line and the
    // pending line right below it — same words twice. Hide the duplicate.
    const latestHistoryLine = translationHistory.length
      ? capPendingWords(translationHistory[translationHistory.length - 1])
      : "";
    const showPending = pendingEnglish && pendingEnglish !== latestHistoryLine;
    const showSource = !liveOnly && japaneseStream;
    const hasContent = translationHistory.length > 0 || showPending || showSource;
    const phase = activityPhase(status, running);

    return (
      <div className="overlay" data-tauri-drag-region>
        <div className="overlay__band" data-tauri-drag-region>
          <DragHandle label="Drag to move" />
        </div>

        <div className="overlay__controls overlay__controls--left">
          <ControlButton
            icon={running ? "stop" : "play"}
            tone={running ? "running" : "start"}
            label={running ? "Stop captioning" : "Start captioning"}
            onClick={toggle}
          />
          <ControlButton
            text={isEs ? "ES" : "JA"}
            on
            label={isEs ? "Switch to English mode" : "Switch to Spanish → English"}
            onClick={toggleMode}
            disabled={running}
          />
          <ControlButton
            text="LIVE"
            on={liveOnly}
            label={liveOnly ? "Show the full translation view" : "Show only the live translated line"}
            onClick={() => setLiveOnly((v) => !v)}
          />
          <ControlButton
            text="LOCAL"
            on={settings.useLocalTranslation}
            label={
              settings.useLocalTranslation
                ? "Translating with a local offline model — the first use downloads and loads it, which may take a while"
                : "Translating with Ollama"
            }
            onClick={toggleLocalTranslation}
            disabled={running}
          />
          <Tooltip label={ACTIVITY_PHASES[phase].label}>
            {() => <PulseRing phase={phase} compact motionEnabled />}
          </Tooltip>
        </div>

        <div className="overlay__controls overlay__controls--right">
          <ControlButton
            icon="eraser"
            label="Clear captions"
            onClick={clearCaptions}
            disabled={!hasContent}
          />
          <ControlButton icon="gear" label="Settings" onClick={openSettings} />
          <ControlButton icon="close" label="Close" onClick={closeApp} />
        </div>

        <div className="overlay__body">
          {!hasContent ? (
            <div className="overlay__empty" data-tauri-drag-region>
              <PulseRing phase={phase} prominent compact={liveOnly} motionEnabled />
              <div className="overlay__empty-text" data-tauri-drag-region>
                {emptyStateLabel()}
              </div>
            </div>
          ) : (
            <div className="overlay-timeline" data-tauri-drag-region>
              {translationHistory.map((line, i) => (
                <div
                  key={i}
                  data-tauri-drag-region
                  className={
                    i === translationHistory.length - 1
                      ? "subtitle-block subtitle-block--latest"
                      : "subtitle-block"
                  }
                >
                  {isEs ? capPendingWords(line) : line}
                </div>
              ))}
              {showPending && (
                <div
                  data-tauri-drag-region
                  className={
                    pendingProvisional
                      ? "subtitle-block subtitle-block--live subtitle-block--provisional"
                      : "subtitle-block subtitle-block--live"
                  }
                >
                  {pendingEnglish}
                </div>
              )}
              {showSource && (
                <div className="subtitle-block subtitle-block--source" data-tauri-drag-region>
                  {japaneseStream}
                </div>
              )}
              <div ref={historyEndRef} />
            </div>
          )}
        </div>
      </div>
    );
  }

  // ── English mode ───────────────────────────────────────────────────────────
  const phase = activityPhase(status, running);
  return (
    <div className="overlay overlay--compact" data-tauri-drag-region>
      <div className="overlay__row" data-tauri-drag-region>
        <DragHandle label="Drag to move" />
        <ControlButton
          icon={running ? "stop" : "play"}
          tone={running ? "running" : "start"}
          label={running ? "Stop captioning" : "Start captioning"}
          onClick={toggle}
        />
        <ControlButton
          text="EN"
          on
          label="Switch to Japanese → English"
          onClick={toggleMode}
          disabled={running}
        />
        <Tooltip label={ACTIVITY_PHASES[phase].label}>
          {() => <PulseRing phase={phase} compact motionEnabled />}
        </Tooltip>

        {words.length === 0 && !currentJa ? (
          <span className="overlay__phase overlay__phase--fill" data-tauri-drag-region>
            {running ? "Listening…" : "Press play to start"}
          </span>
        ) : (
          <span className="overlay__ticker" data-tauri-drag-region>
            {words.map((word, i) => {
              const isEnLive = !currentJa && isPartial && i === words.length - 1;
              return (
                <span
                  key={i}
                  className={
                    isEnLive ? "overlay__word--current"
                    : isPartial ? "overlay__word--spoken"
                    : "overlay__word--final"
                  }
                >
                  {word}
                  {(i < words.length - 1 || currentJa) ? " " : ""}
                </span>
              );
            })}
            {currentJa && <span className="overlay__word--source">{currentJa}</span>}
          </span>
        )}

        <ControlButton icon="gear" label="Settings" onClick={openSettings} />
        <ControlButton icon="close" label="Close" onClick={closeApp} />
      </div>
    </div>
  );
}
