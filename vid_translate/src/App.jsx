import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

// How long a finalized line stays visible before fading (ms)
const FINAL_LINGER_MS = 2000;

export default function App() {
  // partial: words currently being spoken (unstable, gray)
  // final: completed sentence (stable, white)
  const [partial, setPartial] = useState("");
  const [finals, setFinals] = useState([]); // last 2 finalized lines
  const [status, setStatus] = useState("idle");
  const [modelPath, setModelPath] = useState("");
  const [running, setRunning] = useState(false);
  const unlistenRefs = useRef([]);
  const clearTimer = useRef(null);

  useEffect(() => {
    invoke("get_model_path").then(setModelPath);

    const setupListeners = async () => {
      const unlistenTx = await listen("transcription", (event) => {
        const { text, type: kind } = event.payload;

        if (kind === "partial") {
          setPartial(text);
          // Cancel any pending clear so partial keeps updating
          clearTimeout(clearTimer.current);
        } else {
          // Final result: move to finalized lines, clear partial
          setPartial("");
          setFinals((prev) => [...prev.slice(-1), text]); // keep last 2

          // Clear finalized lines after they've been on screen long enough
          clearTimeout(clearTimer.current);
          clearTimer.current = setTimeout(() => {
            setFinals([]);
          }, FINAL_LINGER_MS);
        }
      });

      const unlistenStatus = await listen("status", (event) => {
        setStatus(event.payload.state);
        if (event.payload.state === "model_missing") {
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
      setPartial("");
      setFinals([]);
    } else {
      setPartial("");
      setFinals([]);
      setRunning(true);
      await invoke("start_listening");
    }
  };

  if (status === "model_missing") {
    return (
      <div className="bar bar--setup" data-tauri-drag-region>
        <span className="setup-text">
          Vosk model not found. Run:
          <code>
            mkdir -p {modelPath} && curl -L
            https://alphacephei.com/vosk/models/vosk-model-small-en-us-0.15.zip -o
            /tmp/vosk.zip && unzip /tmp/vosk.zip -d /tmp && mv
            /tmp/vosk-model-small-en-us-0.15 {modelPath}
          </code>
        </span>
        <button className="btn" onClick={toggle}>Retry</button>
      </div>
    );
  }

  const hasText = finals.length > 0 || partial;

  return (
    <div className="bar" data-tauri-drag-region>
      <button className="btn" onClick={toggle} title={running ? "Stop" : "Start"}>
        {running ? "■" : "▶"}
      </button>
      <span className={`dot dot--${status}`} />
      <div className="transcript" data-tauri-drag-region>
        {hasText ? (
          <div className="lines">
            {finals.map((line, i) => (
              <span key={i} className="line line--final">{line}</span>
            ))}
            {partial && (
              <span className="line line--partial">{partial}</span>
            )}
          </div>
        ) : (
          <span className="placeholder">
            {running ? "Listening…" : "Press ▶ to start"}
          </span>
        )}
      </div>
    </div>
  );
}
