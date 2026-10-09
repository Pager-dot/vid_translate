import { memo, useEffect, useState } from "react";

import "./PulseRing.css";

/**
 * Phase colours and durations, matching Mimi's `OVERLAY_ACTIVITY_PHASES`.
 * `animationSpeed: 0` means the phase is a resting state, so its clock settles
 * and then freezes rather than looping forever behind an idle overlay.
 */
export const ACTIVITY_PHASES = {
  idle:        { label: "Idle",        color: "#FFFFFF", working: false },
  error:       { label: "Error",       color: "#FF8A80", working: false },
  connecting:  { label: "Starting",    color: "#FFFFFF", working: true },
  listening:   { label: "Listening",   color: "#7AA8FF", working: true },
  recognizing: { label: "Recognising", color: "#7AA8FF", working: true },
  translating: { label: "Translating", color: "#B894FF", working: true },
  paused:      { label: "Paused",      color: "#FFB852", working: false },
};

/** Maps a backend `status` event (plus whether a session is running) onto a phase. */
export function activityPhase(status, running) {
  if (status === "error") return "error";
  if (status === "loading" || status === "loading_model") return "connecting";
  if (!running) return "idle";
  if (status === "processing") return "translating";
  if (status === "listening") return "listening";
  return "connecting";
}

const syllables = [8, 12, 16, 20, 24, 28, 32];
const heights = [6, 12, 23, 29, 22, 13, 6];
// One repeat occupies 40 viewBox units: translating the persistent track
// by 40 units loops without replacing its animation instance.
const wave =
  "M-40 20 Q-35 20 -30 12 T-20 20 T-10 28 T0 20 T10 12 T20 20 T30 28 T40 20 T50 12 T60 20 T70 28 T80 20";

/**
 * Decorative sound language from session phases, never measured audio.
 * Tracks stay mounted. Phase changes reshape their enclosing layers; a resting
 * phase settles for 520ms, then freezes its clocks; resuming continues them.
 *
 * Ported from Mimi's `src/windows/overlay/PulseRing.tsx`.
 */
export const PulseRing = memo(function PulseRing({
  phase,
  compact = false,
  prominent = false,
  motionEnabled = true,
  pulseStyle = "ribbon",
}) {
  const size = prominent ? (compact ? 48 : 80) : compact ? 24 : 40;
  const working = ACTIVITY_PHASES[phase].working;
  const [clockPaused, setClockPaused] = useState(!motionEnabled || !working);

  useEffect(() => {
    if (!motionEnabled || working) {
      setClockPaused(!motionEnabled);
      return;
    }
    const settle = window.setTimeout(() => setClockPaused(true), 520);
    return () => window.clearTimeout(settle);
  }, [motionEnabled, working]);

  return (
    <div
      className={`sound-light${motionEnabled ? "" : " sound-light--still"}`}
      data-phase={phase}
      data-clock={clockPaused ? "paused" : "running"}
      data-sound-style={pulseStyle}
      data-pulse-style={pulseStyle}
      data-size={
        prominent ? (compact ? "prominent-compact" : "prominent") : compact ? "compact" : "normal"
      }
      aria-hidden="true"
      style={{ width: size, height: size, color: ACTIVITY_PHASES[phase].color }}
    >
      <svg
        className="sound-light__drawing"
        viewBox="0 0 40 40"
        fill="none"
        stroke="currentColor"
        strokeLinecap="round"
      >
        <g className="sound-light__syllables">
          {syllables.map((x, index) => (
            <g className="sound-light__envelope" key={x}>
              <g className="sound-light__beat" style={{ animationDelay: `${-index * 0.19}s` }}>
                <path
                  strokeWidth="2.75"
                  d={`M${x} ${20 - heights[index] / 2}V${20 + heights[index] / 2}`}
                />
              </g>
            </g>
          ))}
        </g>
        <g className="sound-light__ribbons">
          <g className="sound-light__ribbon-envelope">
            <path className="sound-light__wave sound-light__wave--back" strokeWidth="1.5" d={wave} />
            <path className="sound-light__wave sound-light__wave--front" strokeWidth="2.2" d={wave} />
          </g>
        </g>
        <g className="sound-light__translation">
          <g className="sound-light__current-envelope">
            <path
              className="sound-light__current sound-light__current--back"
              strokeWidth="1.5"
              d={wave}
            />
            <path
              className="sound-light__current sound-light__current--front"
              strokeWidth="2.2"
              d={wave}
            />
          </g>
        </g>
        <path className="sound-light__flat" strokeWidth="2.2" d="M9 20H17M23 20H31" />
        <g className="sound-light__idle" fill="currentColor" stroke="none">
          <circle cx="16" cy="20" r="1.3" />
          <circle cx="20" cy="20" r="1.3" />
          <circle cx="24" cy="20" r="1.3" />
        </g>
        <path className="sound-light__error" strokeWidth="2.2" d="M16 16L24 24M24 16L16 24" />
      </svg>
    </div>
  );
});
