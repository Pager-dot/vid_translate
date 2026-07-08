import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize, LogicalPosition } from "@tauri-apps/api/window";
import "./App.css";

const FINAL_LINGER_MS = 2500;
const MAX_WORDS = 10;
const SETTINGS_H = 460;

const DEFAULT_SETTINGS = {
  ollamaKey: "",
  ollamaModel: "gemma4:31b-cloud",
  fontFamily: "system-ui",
  fontScale: 1.0,
  opacity: 0.78,
  width: 1200,
  enHeight: 90,
  jaHeight: 500,
};

function loadSettings() {
  try {
    return { ...DEFAULT_SETTINGS, ...JSON.parse(localStorage.getItem("vt_settings") || "{}") };
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
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

function SettingsPanel({ draft, setDraft, onSave, onClose }) {
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

  return (
    <div className="settings-window">
      <div className="settings-header" data-tauri-drag-region>
        <span className="settings-title">Settings</span>
        <button className="btn btn--close" onClick={onClose} title="Close settings">✕</button>
      </div>

      <div className="settings-form">
        {/* ── Ollama Cloud ──────────────────────────── */}
        <div className="settings-section">Ollama Cloud</div>

        <div className="settings-row">
          <label>API Key</label>
          <input
            type="password"
            className="settings-input"
            value={draft.ollamaKey}
            onChange={(e) => upd("ollamaKey", e.target.value)}
            placeholder="paste from ollama.com/settings/keys"
          />
        </div>
        <div className="settings-hint">
          With a key, requests go to ollama.com — no local install needed.
        </div>

        <div className="settings-row">
          <label>Model</label>
          <input
            type="text"
            className="settings-input"
            value={draft.ollamaModel}
            onChange={(e) => upd("ollamaModel", e.target.value)}
          />
        </div>

        {/* ── Pull model (local) ────────────────────── */}
        <div className="settings-section">Pull Model (local Ollama)</div>

        <div className="settings-row">
          <input
            type="text"
            className="settings-input"
            value={pullInput}
            onChange={(e) => setPullInput(e.target.value)}
            placeholder={draft.ollamaModel || "gemma4:27b"}
          />
          <button
            className="btn btn--pull"
            onClick={handlePull}
            disabled={pullStatus === "pulling"}
          >
            {pullStatus === "pulling" ? "…" : "Pull"}
          </button>
        </div>

        {(pullProgress || pullMsg) && (
          <div className="pull-status">
            {pullProgress && (
              <div className="pull-bar-track">
                <div
                  className="pull-bar-fill"
                  style={{
                    width: `${Math.round((pullProgress.completed / pullProgress.total) * 100)}%`,
                  }}
                />
              </div>
            )}
            <span
              className={
                pullStatus === "done"
                  ? "pull-msg pull-msg--done"
                  : pullStatus === "error"
                  ? "pull-msg pull-msg--error"
                  : "pull-msg"
              }
            >
              {pullMsg}
            </span>
          </div>
        )}

        {/* ── Appearance ───────────────────────────── */}
        <div className="settings-section">Appearance</div>

        <div className="settings-row">
          <label>Font</label>
          <select
            className="settings-input"
            value={draft.fontFamily}
            onChange={(e) => upd("fontFamily", e.target.value)}
          >
            <option value="system-ui">System UI</option>
            <option value="Georgia, serif">Georgia</option>
            <option value="Arial, sans-serif">Arial</option>
            <option value="'Courier New', monospace">Courier New</option>
            <option value="'Times New Roman', serif">Times New Roman</option>
          </select>
        </div>

        <div className="settings-row">
          <label>Font Scale</label>
          <input
            type="range" min="0.5" max="2" step="0.1"
            value={draft.fontScale}
            onChange={(e) => upd("fontScale", parseFloat(e.target.value))}
          />
          <span className="settings-val">{draft.fontScale}×</span>
        </div>

        <div className="settings-row">
          <label>Opacity</label>
          <input
            type="range" min="0.05" max="1" step="0.05"
            value={draft.opacity}
            onChange={(e) => upd("opacity", parseFloat(e.target.value))}
          />
          <span className="settings-val">{Math.round(draft.opacity * 100)}%</span>
        </div>

        {/* ── Dimensions ───────────────────────────── */}
        <div className="settings-section">Dimensions</div>

        <div className="settings-row">
          <label>Width</label>
          <input
            type="number" className="settings-input settings-input--num"
            value={draft.width} min={300} max={3840}
            onChange={(e) => upd("width", parseInt(e.target.value) || draft.width)}
          />
          <span className="settings-unit">px</span>
        </div>

        <div className="settings-row">
          <label>EN Height</label>
          <input
            type="number" className="settings-input settings-input--num"
            value={draft.enHeight} min={60} max={400}
            onChange={(e) => upd("enHeight", parseInt(e.target.value) || draft.enHeight)}
          />
          <span className="settings-unit">px</span>
        </div>

        <div className="settings-row">
          <label>JA/ES Height</label>
          <input
            type="number" className="settings-input settings-input--num"
            value={draft.jaHeight} min={160} max={1200}
            onChange={(e) => upd("jaHeight", parseInt(e.target.value) || draft.jaHeight)}
          />
          <span className="settings-unit">px</span>
        </div>
      </div>

      <div className="settings-footer">
        <button className="btn btn--save" onClick={onSave}>Save</button>
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
  const [japaneseStream, setJapaneseStream]         = useState("");
  const historyEndRef = useRef(null);

  const [status, setStatus]               = useState("idle");
  const [modelPath, setModelPath]         = useState("");
  const [voskJaModelPath, setVoskJaModelPath] = useState("");
  const [voskEsModelPath, setVoskEsModelPath] = useState("");
  const [running, setRunning]             = useState(false);
  const [mode, setMode]                   = useState("vosk");
  const [settingsOpen, setSettingsOpen]   = useState(false);
  const [platform, setPlatform]           = useState("linux");

  const [settings, setSettings] = useState(loadSettings);
  const [draft, setDraft]       = useState(settings);

  const unlistenRefs = useRef([]);
  const clearTimer   = useRef(null);

  // Apply persisted CSS vars on mount
  useEffect(() => { applySettings(settings); }, []);

  // Auto-scroll JA history
  useEffect(() => {
    historyEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [translationHistory]);

  const modeRef = useRef(mode);
  useEffect(() => { modeRef.current = mode; }, [mode]);

  // Resize window when mode or saved dimensions change (skip first mount)
  const hasMountedRef = useRef(false);
  useEffect(() => {
    if (!hasMountedRef.current) { hasMountedRef.current = true; return; }
    if (settingsOpen) return; // settings panel manages its own size
    const win = getCurrentWindow();
    if (mode === "vosk-ja" || mode === "vosk-es") {
      win.setSize(new LogicalSize(settings.width, settings.jaHeight));
      win.setPosition(new LogicalPosition(0, Math.max(0, 950 - (settings.jaHeight - settings.enHeight))));
    } else {
      win.setSize(new LogicalSize(settings.width, settings.enHeight));
      win.setPosition(new LogicalPosition(0, 950));
    }
  }, [mode, settings.width, settings.jaHeight, settings.enHeight]);

  useEffect(() => {
    invoke("get_model_path").then(setModelPath);
    invoke("get_vosk_ja_model_path").then(setVoskJaModelPath);
    invoke("get_vosk_es_model_path").then(setVoskEsModelPath);
    invoke("get_platform").then(setPlatform);

    const setupListeners = async () => {
      const unlistenTx = await listen("transcription", (event) => {
        const { text, current, type: kind } = event.payload;

        if (modeRef.current === "vosk-ja" || modeRef.current === "vosk-es") {
          if (kind === "partial") {
            setJapaneseStream(text);
          } else if (kind === "streaming-en") {
            setPendingEnglish(text);
          } else {
            setTranslationHistory((h) => {
              if (h.length > 0 && h[h.length - 1] === text) return h;
              return [...h, text];
            });
            setPendingEnglish("");
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
        if (["model_missing", "vosk_ja_model_missing", "vosk_es_model_missing"]
            .includes(event.payload.state)) {
          setRunning(false);
        }
      });

      unlistenRefs.current = [unlistenTx, unlistenStatus];
    };

    setupListeners();
    return () => {
      unlistenRefs.current.forEach((fn) => fn());
      clearTimeout(clearTimer.current);
    };
  }, []);

  const toggle = async () => {
    if (running) {
      await invoke("stop_listening");
      setRunning(false);
      setWords([]);
      setCurrentJa("");
      setTranslationHistory([]);
      setPendingEnglish("");
      setJapaneseStream("");
    } else {
      setWords([]);
      setCurrentJa("");
      setTranslationHistory([]);
      setPendingEnglish("");
      setJapaneseStream("");
      setRunning(true);
      await invoke("start_listening", {
        mode,
        ollamaKey: settings.ollamaKey || null,
        ollamaModel: settings.ollamaModel || null,
      });
    }
  };

  const MODES = ["vosk", "vosk-ja", "vosk-es"];
  const toggleMode = () => {
    if (!running) {
      setMode((m) => MODES[(MODES.indexOf(m) + 1) % MODES.length]);
    }
  };

  const openSettings = async () => {
    setDraft({ ...settings });
    const win = getCurrentWindow();
    const screenH = window.screen.height;
    const y = Math.max(0, screenH - SETTINGS_H - 20);
    await win.setSize(new LogicalSize(settings.width, SETTINGS_H));
    await win.setPosition(new LogicalPosition(0, y));
    setSettingsOpen(true);
  };

  const closeSettings = async () => {
    setSettingsOpen(false);
    const win = getCurrentWindow();
    if (mode === "vosk-ja" || mode === "vosk-es") {
      win.setSize(new LogicalSize(settings.width, settings.jaHeight));
      win.setPosition(new LogicalPosition(0, Math.max(0, 950 - (settings.jaHeight - settings.enHeight))));
    } else {
      win.setSize(new LogicalSize(settings.width, settings.enHeight));
      win.setPosition(new LogicalPosition(0, 950));
    }
  };

  const saveSettings = async () => {
    localStorage.setItem("vt_settings", JSON.stringify(draft));
    setSettings(draft);
    applySettings(draft);
    setSettingsOpen(false);
    const win = getCurrentWindow();
    if (mode === "vosk-ja" || mode === "vosk-es") {
      win.setSize(new LogicalSize(draft.width, draft.jaHeight));
      win.setPosition(new LogicalPosition(0, Math.max(0, 950 - (draft.jaHeight - draft.enHeight))));
    } else {
      win.setSize(new LogicalSize(draft.width, draft.enHeight));
      win.setPosition(new LogicalPosition(0, 950));
    }
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
      />
    );
  }

  // ── Setup screens ──────────────────────────────────────────────────────────
  const isWindows = platform === "windows";

  if (status === "model_missing") {
    const cmd = isWindows
      ? `New-Item -ItemType Directory -Force -Path (Split-Path "${modelPath}") | Out-Null; Invoke-WebRequest https://alphacephei.com/vosk/models/vosk-model-small-en-us-0.15.zip -OutFile "$env:TEMP\\vosk.zip"; Expand-Archive "$env:TEMP\\vosk.zip" -DestinationPath "$env:TEMP" -Force; Move-Item "$env:TEMP\\vosk-model-small-en-us-0.15" "${modelPath}" -Force`
      : `mkdir -p ${modelPath} && curl -L https://alphacephei.com/vosk/models/vosk-model-small-en-us-0.15.zip -o /tmp/vosk.zip && unzip /tmp/vosk.zip -d /tmp && mv /tmp/vosk-model-small-en-us-0.15 ${modelPath}`;
    return (
      <div className="bar bar--setup" data-tauri-drag-region>
        <span className="setup-text">
          Vosk model not found. Run:
          <code>{cmd}</code>
        </span>
        <button className="btn" onClick={toggle}>Retry</button>
      </div>
    );
  }

  if (status === "vosk_ja_model_missing") {
    const cmd = isWindows
      ? `New-Item -ItemType Directory -Force -Path (Split-Path "${voskJaModelPath}") | Out-Null; Invoke-WebRequest https://alphacephei.com/vosk/models/vosk-model-ja-0.22.zip -OutFile "$env:TEMP\\vosk-ja.zip"; Expand-Archive "$env:TEMP\\vosk-ja.zip" -DestinationPath "$env:TEMP" -Force; Move-Item "$env:TEMP\\vosk-model-ja-0.22" "${voskJaModelPath}" -Force`
      : `curl -L https://alphacephei.com/vosk/models/vosk-model-ja-0.22.zip -o /tmp/vosk-ja.zip && unzip /tmp/vosk-ja.zip -d /tmp && mv /tmp/vosk-model-ja-0.22 ${voskJaModelPath}`;
    return (
      <div className="bar bar--setup" data-tauri-drag-region>
        <span className="setup-text">
          Vosk Japanese model not found. Run:
          <code>{cmd}</code>
        </span>
        <button className="btn" onClick={toggle}>Retry</button>
      </div>
    );
  }

  if (status === "vosk_es_model_missing") {
    const cmd = isWindows
      ? `New-Item -ItemType Directory -Force -Path (Split-Path "${voskEsModelPath}") | Out-Null; Invoke-WebRequest https://alphacephei.com/vosk/models/vosk-model-es-0.42.zip -OutFile "$env:TEMP\\vosk-es.zip"; Expand-Archive "$env:TEMP\\vosk-es.zip" -DestinationPath "$env:TEMP" -Force; Move-Item "$env:TEMP\\vosk-model-es-0.42" "${voskEsModelPath}" -Force`
      : `curl -L https://alphacephei.com/vosk/models/vosk-model-es-0.42.zip -o /tmp/vosk-es.zip && unzip /tmp/vosk-es.zip -d /tmp && mv /tmp/vosk-model-es-0.42 ${voskEsModelPath}`;
    return (
      <div className="bar bar--setup" data-tauri-drag-region>
        <span className="setup-text">
          Vosk Spanish model not found. Run:
          <code>{cmd}</code>
        </span>
        <button className="btn" onClick={toggle}>Retry</button>
      </div>
    );
  }

  // ── JA / ES translation mode ────────────────────────────────────────────────
  if (mode === "vosk-ja" || mode === "vosk-es") {
    const isEs = mode === "vosk-es";
    return (
      <div className="bar bar--ja" data-tauri-drag-region>
        <div className="bar-top">
          <div className="bar-left">
            <button className="btn" onClick={toggle} title={running ? "Stop" : "Start"}>
              {running ? "■" : "▶"}
            </button>
            <button
              className="btn btn--mode btn--mode-active"
              onClick={toggleMode}
              disabled={running}
              title={isEs ? "Switch to English mode" : "Switch to Spanish→English"}
            >
              {isEs ? "ES" : "JA"}
            </button>
            <span className={`dot dot--${status}`} />
          </div>
          <div className="bar-actions">
            <button className="btn btn--settings" onClick={openSettings} title="Settings">⚙</button>
            <button className="btn btn--close" onClick={closeApp} title="Close">✕</button>
          </div>
        </div>

        <div className="ja-body" data-tauri-drag-region>
          <div className="ja-history">
            {translationHistory.length === 0 ? (
              <span className="placeholder">
                {running ? "Listening…" : "Press ▶ to start"}
              </span>
            ) : (
              translationHistory.map((line, i) => (
                <div
                  key={i}
                  className={
                    i === translationHistory.length - 1
                      ? "ja-history-item ja-history-item--latest"
                      : "ja-history-item"
                  }
                >
                  {line}
                </div>
              ))
            )}
            <div ref={historyEndRef} />
          </div>
          {pendingEnglish && <div className="ja-pending">{pendingEnglish}</div>}
          {japaneseStream && <div className="ja-japanese">{japaneseStream}</div>}
        </div>
      </div>
    );
  }

  // ── English mode ───────────────────────────────────────────────────────────
  return (
    <div className="bar" data-tauri-drag-region>
      <div className="bar-left">
        <button className="btn" onClick={toggle} title={running ? "Stop" : "Start"}>
          {running ? "■" : "▶"}
        </button>
        <button
          className="btn btn--mode"
          onClick={toggleMode}
          title="Switch to Japanese→English"
          disabled={running}
        >
          EN
        </button>
        <span className={`dot dot--${status}`} />
      </div>
      <div className="transcript" data-tauri-drag-region>
        {words.length === 0 && !currentJa ? (
          <span className="placeholder">
            {running ? "Listening…" : "Press ▶ to start"}
          </span>
        ) : (
          <span className="caption">
            {words.map((word, i) => {
              const isEnLive = !currentJa && isPartial && i === words.length - 1;
              return (
                <span
                  key={i}
                  className={
                    isEnLive ? "word word--current"
                    : isPartial ? "word word--spoken"
                    : "word word--final"
                  }
                >
                  {word}
                  {(i < words.length - 1 || currentJa) ? " " : ""}
                </span>
              );
            })}
            {currentJa && <span className="word word--current-ja">{currentJa}</span>}
          </span>
        )}
      </div>
      <div className="bar-actions">
        <button className="btn btn--settings" onClick={openSettings} title="Settings">⚙</button>
        <button className="btn btn--close" onClick={closeApp} title="Close">✕</button>
      </div>
    </div>
  );
}
