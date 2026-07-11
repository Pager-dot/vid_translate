import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize, LogicalPosition } from "@tauri-apps/api/window";
import "./App.css";

const FINAL_LINGER_MS = 2500;
const MAX_WORDS = 10;
const SETTINGS_H = 640;

const DEFAULT_SETTINGS = {
  ollamaKey: "",
  ollamaModel: "gemma3:27b",
  fontFamily: "system-ui",
  fontScale: 1.0,
  opacity: 0.78,
  width: null, // null = full display width
  enHeight: 90,
  jaHeight: 280,
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

function SettingsPanel({ draft, setDraft, onSave, onClose, onReset }) {
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
            value={draft.width ?? ""} min={300} max={7680}
            placeholder="full"
            onChange={(e) => upd("width", parseInt(e.target.value) || null)}
          />
          <span className="settings-unit">px (empty = full display)</span>
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
        <button className="btn btn--reset" onClick={onReset} title="Reset UI settings to defaults">
          Reset
        </button>
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
  const [running, setRunning]             = useState(false);
  const [mode, setMode]                   = useState("vosk");
  const [liveOnly, setLiveOnly]           = useState(false);
  const [settingsOpen, setSettingsOpen]   = useState(false);
  const [downloadProgress, setDownloadProgress] = useState(null); // { kind, status, downloaded, total, error }

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
  const liveOnlyRef = useRef(liveOnly);
  useEffect(() => { liveOnlyRef.current = liveOnly; }, [liveOnly]);
  const settingsOpenRef = useRef(settingsOpen);
  useEffect(() => { settingsOpenRef.current = settingsOpen; }, [settingsOpen]);

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
      const h = isJa && !liveOnly ? settings.jaHeight : settings.enHeight;
      await doResize(settings.width ?? screenWidth(), h);
    })();
  }, [mode, liveOnly, settings.width, settings.enHeight, settings.jaHeight]);

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

      const unlistenDownload = await listen("vosk_download_progress", (event) => {
        setDownloadProgress(event.payload);
        if (event.payload.status === "done") {
          setTimeout(() => {
            setDownloadProgress(null);
            toggleRef.current();
          }, 400);
        }
      });

      unlistenRefs.current = [unlistenTx, unlistenStatus, unlistenDownload];
    };

    setupListeners();
    return () => {
      unlistenRefs.current.forEach((fn) => fn());
      clearTimeout(clearTimer.current);
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
  useEffect(() => { toggleRef.current = toggle; });

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
    const w = Math.min(1200, screenWidth());
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

  // ── Setup screens: model missing → one-click auto-download, no manual steps ──
  const MISSING_MODEL_KIND = {
    model_missing: { kind: "en", label: "English" },
    vosk_ja_model_missing: { kind: "ja", label: "Japanese" },
    vosk_es_model_missing: { kind: "es", label: "Spanish" },
  };

  if (MISSING_MODEL_KIND[status]) {
    const { kind, label } = MISSING_MODEL_KIND[status];
    const dl = downloadProgress && downloadProgress.kind === kind ? downloadProgress : null;
    const pct = dl && dl.total ? Math.round((dl.downloaded / dl.total) * 100) : null;

    return (
      <div className="bar bar--setup" data-tauri-drag-region>
        {!dl && (
          <>
            <span className="setup-text">{label} speech model not downloaded yet.</span>
            <button className="btn" onClick={() => invoke("download_vosk_model", { kind })}>
              Download
            </button>
          </>
        )}
        {dl && dl.status !== "error" && (
          <span className="setup-text">
            {dl.status === "downloading" ? `Downloading… ${pct !== null ? pct + "%" : ""}` : "Extracting…"}
          </span>
        )}
        {dl && dl.status === "error" && (
          <>
            <span className="setup-text">Download failed: {dl.error}</span>
            <button className="btn" onClick={() => invoke("download_vosk_model", { kind })}>
              Retry
            </button>
          </>
        )}
      </div>
    );
  }

  // ── JA / ES translation mode ────────────────────────────────────────────────
  if (mode === "vosk-ja" || mode === "vosk-es") {
    const isEs = mode === "vosk-es";
    return (
      <div className="bar bar--ja" data-tauri-drag-region>
        <div className="bar-top" data-tauri-drag-region>
          <div className="bar-left">
            <button
              className={running ? "btn btn--toggle btn--toggle-running" : "btn btn--toggle"}
              onClick={toggle}
              title={running ? "Stop" : "Start"}
            >
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
            <button
              className={liveOnly ? "btn btn--mode btn--live-active" : "btn btn--mode"}
              onClick={() => setLiveOnly((v) => !v)}
              title={liveOnly ? "Show full translation view" : "Show only live speech text"}
            >
              LIVE
            </button>
            <span className={`dot dot--${status}`} />
          </div>
          <div className="bar-actions">
            <button className="btn btn--settings" onClick={openSettings} title="Settings">⚙</button>
            <button className="btn btn--close" onClick={closeApp} title="Close">✕</button>
          </div>
        </div>

        <div
          className={
            liveOnly || translationHistory.length === 0 ? "ja-body ja-body--center" : "ja-body"
          }
          data-tauri-drag-region
        >
          {liveOnly ? (
            japaneseStream ? (
              <div className="ja-live" data-tauri-drag-region>{japaneseStream}</div>
            ) : (
              <span className="placeholder" data-tauri-drag-region>
                {running ? "Listening…" : "Press ▶ to start"}
              </span>
            )
          ) : translationHistory.length === 0 ? (
            // nothing finalized yet — center the streaming text instead of
            // pinning it under an empty history area
            pendingEnglish || japaneseStream ? (
              <>
                {pendingEnglish && <div className="ja-pending" data-tauri-drag-region>{pendingEnglish}</div>}
                {japaneseStream && <div className="ja-japanese" data-tauri-drag-region>{japaneseStream}</div>}
              </>
            ) : (
              <span className="placeholder" data-tauri-drag-region>
                {running ? "Listening…" : "Press ▶ to start"}
              </span>
            )
          ) : (
            <>
              <div className="ja-history" data-tauri-drag-region>
                {translationHistory.map((line, i) => (
                  <div
                    key={i}
                    data-tauri-drag-region
                    className={
                      i === translationHistory.length - 1
                        ? "ja-history-item ja-history-item--latest"
                        : "ja-history-item"
                    }
                  >
                    {line}
                  </div>
                ))}
                <div ref={historyEndRef} />
              </div>
              {pendingEnglish && <div className="ja-pending" data-tauri-drag-region>{pendingEnglish}</div>}
              {japaneseStream && <div className="ja-japanese" data-tauri-drag-region>{japaneseStream}</div>}
            </>
          )}
        </div>
      </div>
    );
  }

  // ── English mode ───────────────────────────────────────────────────────────
  return (
    <div className="bar" data-tauri-drag-region>
      <div className="bar-left">
        <button
          className={running ? "btn btn--toggle btn--toggle-running" : "btn btn--toggle"}
          onClick={toggle}
          title={running ? "Stop" : "Start"}
        >
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
