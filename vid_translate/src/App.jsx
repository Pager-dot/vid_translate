import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

const FINAL_LINGER_MS = 2500;
const MAX_WORDS = 10;

function slideWindow(text) {
  const words = text.trim().split(/\s+/).filter(Boolean);
  return words.slice(-MAX_WORDS);
}

export default function App() {
  const [words, setWords] = useState([]);
  const [isPartial, setIsPartial] = useState(false);
  const [status, setStatus] = useState("idle");
  const [modelPath, setModelPath] = useState("");
  const [whisperModelPath, setWhisperModelPath] = useState("");
  const [voskJaModelPath, setVoskJaModelPath] = useState("");
  const [running, setRunning] = useState(false);
  // "vosk" = real-time English, "vosk-ja" = streaming JA + translated EN finals
  const [mode, setMode] = useState("vosk");
  const unlistenRefs = useRef([]);
  const clearTimer = useRef(null);

  useEffect(() => {
    invoke("get_model_path").then(setModelPath);
    invoke("get_whisper_model_path").then(setWhisperModelPath);
    invoke("get_vosk_ja_model_path").then(setVoskJaModelPath);

    const setupListeners = async () => {
      const unlistenTx = await listen("transcription", (event) => {
        const { text, type: kind } = event.payload;
        clearTimeout(clearTimer.current);

        if (kind === "partial") {
          setWords(slideWindow(text));
          setIsPartial(true);
        } else {
          setWords(slideWindow(text));
          setIsPartial(false);
          clearTimer.current = setTimeout(() => {
            setWords([]);
            setIsPartial(false);
          }, FINAL_LINGER_MS);
        }
      });

      const unlistenStatus = await listen("status", (event) => {
        setStatus(event.payload.state);
        if (
          event.payload.state === "model_missing" ||
          event.payload.state === "whisper_model_missing" ||
          event.payload.state === "vosk_ja_model_missing"
        ) {
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
    } else {
      setWords([]);
      setRunning(true);
      await invoke("start_listening", { mode });
    }
  };

  const toggleMode = () => {
    if (!running) setMode((m) => (m === "vosk" ? "vosk-ja" : "vosk"));
  };

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

  if (status === "whisper_model_missing") {
    return (
      <div className="bar bar--setup" data-tauri-drag-region>
        <span className="setup-text">
          Whisper model not found. Run:
          <code>
            mkdir -p ~/.local/share/vid_translate/models && curl -L
            https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin
            -o {whisperModelPath}
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

  const placeholderText = running
    ? "Listening…"
    : "Press ▶ to start";

  return (
    <div className="bar" data-tauri-drag-region>
      <button className="btn" onClick={toggle} title={running ? "Stop" : "Start"}>
        {running ? "■" : "▶"}
      </button>
      <button
        className={`btn btn--mode ${mode === "vosk-ja" ? "btn--mode-active" : ""}`}
        onClick={toggleMode}
        title={running ? "Stop to change mode" : mode === "vosk" ? "Switch to Japanese→English" : "Switch to English"}
        disabled={running}
      >
        {mode === "vosk" ? "EN" : "JA"}
      </button>
      <span className={`dot dot--${status}`} />
      <div className="transcript" data-tauri-drag-region>
        {words.length === 0 ? (
          <span className="placeholder">{placeholderText}</span>
        ) : (
          <span className="caption">
            {words.map((word, i) => {
              const isCurrentWord = isPartial && i === words.length - 1;
              return (
                <span
                  key={i}
                  className={
                    isCurrentWord
                      ? "word word--current"
                      : isPartial
                      ? "word word--spoken"
                      : "word word--final"
                  }
                >
                  {word}
                  {i < words.length - 1 ? " " : ""}
                </span>
              );
            })}
          </span>
        )}
      </div>
    </div>
  );
}
