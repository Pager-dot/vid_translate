import { Icon } from "./Icon";
import { Tooltip } from "./Tooltip";
import "./control-button.css";

/**
 * 24×24 rounded overlay control with a 10pt icon, or — with `text` instead of
 * `icon` — the same chrome around a short word.
 *
 * Ported from Mimi's `src/windows/overlay/ControlButton.tsx`.
 */
export function ControlButton({
  icon,
  text,
  label,
  onClick,
  disabled = false,
  busy = false,
  /** Renders filled, for a mode that is currently engaged. */
  on = false,
  /** "start" | "running": tints the session toggle without a second class. */
  tone,
}) {
  return (
    <Tooltip label={label}>
      {(descriptionId, hovered) => (
        <button
          type="button"
          onClick={onClick}
          aria-label={label}
          aria-describedby={descriptionId}
          aria-busy={busy || undefined}
          aria-pressed={on || undefined}
          disabled={disabled || busy}
          data-hovered={hovered || undefined}
          data-on={on || undefined}
          data-tone={tone}
          className={
            text === undefined
              ? "overlay-control-button"
              : "overlay-control-button overlay-control-button--text"
          }
        >
          {busy ? (
            <span className="overlay-control-button__busy" aria-hidden="true" />
          ) : text !== undefined ? (
            text
          ) : (
            <Icon name={icon} />
          )}
        </button>
      )}
    </Tooltip>
  );
}
