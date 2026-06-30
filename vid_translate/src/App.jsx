import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize, LogicalPosition } from "@tauri-apps/api/window";
import "./App.css";

const FINAL_LINGER_MS = 2500;
const MAX_WORDS = 10;

function slideWindow(text) {
  const words = text.trim().split(/\s+/).filter(Boolean);
  return words.slice(-MAX_WORDS);
}

export default function App() {
  // English mode state
  const [words, setWords]           = useState([]);
  const [currentJa, setCurrentJa]   = useState("");
  const [isPartial, setIsPartial]   = useState(false);
  // JA drama mode state
  const [translationHistory, setTranslationHistory] = useState([]);
  const [pendingEnglish, setPendingEnglish]         = useState("");
  const [japaneseStream, setJapaneseStream]         = useState("");
  const historyEndRef = useRef(null);

  const [status, setStatus]         = useState("idle");
  const [modelPath, setModelPath]   = useState("");
  const [voskJaModelPath, setVoskJaModelPath]   = useState("");
  const [running, setRunning]       = useState(false);
  const [mode, setMode]             = useState("vosk");

  const unlistenRefs = useRef([]);
  const clearTimer   = useRef(null);

  // Auto-scroll history to bottom whenever a new translation lands
  useEffect(() => {
    historyEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [translationHistory]);
  // Ref so the event listener always sees the current mode without re-registering
  const modeRef = useRef(mode);
  useEffect(() => { modeRef.current = mode; }, [mode]);

  // Auto-resize window when switching modes (skip initial mount)
  const isMountedRef = useRef(false);
  useEffect(() => {
    if (!isMountedRef.current) { isMountedRef.current = true; return; }
    const win = getCurrentWindow();
    if (mode === "vosk-ja") {
      win.setSize(new LogicalSize(1200, 500));
      win.setPosition(new LogicalPosition(0, 500));
    } else {
      win.setSize(new LogicalSize(1200, 90));
      win.setPosition(new LogicalPosition(0, 950));
    }
  }, [mode]);

  useEffect(() => {
    invoke("get_model_path").then(setModelPath);
    invoke("get_vosk_ja_model_path").then(setVoskJaModelPath);

    const setupListeners = async () => {
      const unlistenTx = await listen("transcription", (event) => {
        const { text, current, type: kind } = event.payload;

        if (modeRef.current === "vosk-ja") {
          // ── JA drama mode ──────────────────────────────────────────────
          if (kind === "partial") {
            setJapaneseStream(text);
          } else if (kind === "streaming-en") {
            setPendingEnglish(text);
          } else {
            // "final" — promote pending to confirmed history
            setTranslationHistory((h) => {
              // Skip if identical to the last entry (flush + Final race)
              if (h.length > 0 && h[h.length - 1] === text) return h;
              return [...h, text];
            });
            setPendingEnglish("");
            setJapaneseStream("");
          }
        } else {
          // ── English mode ────────────────────────────────────────────────
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
        if (["model_missing", "whisper_model_missing", "vosk_ja_model_missing"]
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
      await invoke("start_listening", { mode });
    }
  };

  const toggleMode = () => {
    if (!running) setMode((m) => (m === "vosk" ? "vosk-ja" : "vosk"));
  };

  // ── Setup screens ──────────────────────────────────────────────────────────
  if (status === "model_missing") {
    return (
      <div className="bar bar--setup" data-tauri-drag-region>
        <span className="setup-text">
          Vosk model not found. Run:
          <code>
            mkdir -p {modelPath} && curl -L
            https://alphacephei.com/vosk/models/vosk-model-small-en-us-0.15.zip
            -o /tmp/vosk.zip && unzip /tmp/vosk.zip -d /tmp && mv
            /tmp/vosk-model-small-en-us-0.15 {modelPath}
          </code>
        </span>
        <button className="btn" onClick={toggle}>Retry</button>
      </div>
    );
  }

  if (status === "vosk_ja_model_missing") {
    return (
      <div className="bar bar--setup" data-tauri-drag-region>
        <span className="setup-text">
          Vosk Japanese model not found. Run:
          <code>
            curl -L https://alphacephei.com/vosk/models/vosk-model-ja-0.22.zip
            -o /tmp/vosk-ja.zip && unzip /tmp/vosk-ja.zip -d /tmp && mv
            /tmp/vosk-model-ja-0.22 {voskJaModelPath}
          </code>
        </span>
        <button className="btn" onClick={toggle}>Retry</button>
      </div>
    );
  }

  // ── JA drama mode layout ──────────────────────────────────────────────────
  if (mode === "vosk-ja") {
    return (
      <div className="bar bar--ja" data-tauri-drag-region>
        <div className="ja-controls">
          <button className="btn" onClick={toggle} title={running ? "Stop" : "Start"}>
            {running ? "■" : "▶"}
          </button>
          <button
            className="btn btn--mode btn--mode-active"
            onClick={toggleMode}
            disabled={running}
            title="Switch to English mode"
          >
            JA
          </button>
          <span className={`dot dot--${status}`} />
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
          {pendingEnglish && (
            <div className="ja-pending">{pendingEnglish}</div>
          )}
          {japaneseStream && (
            <div className="ja-japanese">{japaneseStream}</div>
          )}
        </div>
      </div>
    );
  }

  // ── English mode layout ───────────────────────────────────────────────────
  return (
    <div className="bar" data-tauri-drag-region>
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
                    isEnLive
                      ? "word word--current"
                      : isPartial
                      ? "word word--spoken"
                      : "word word--final"
                  }
                >
                  {word}
                  {(i < words.length - 1 || currentJa) ? " " : ""}
                </span>
              );
            })}
            {currentJa && (
              <span className="word word--current-ja">{currentJa}</span>
            )}
          </span>
        )}
      </div>
    </div>
  );
}
