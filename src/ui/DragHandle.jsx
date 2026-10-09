import { Tooltip } from "./Tooltip";

/**
 * The always-visible move affordance: a 3px pill that widens and takes on the
 * accent colour under the pointer. The press itself is handled natively —
 * `data-tauri-drag-region` makes the whole band draggable — so this element
 * only has to say that the overlay *can* be moved.
 *
 * Adapted from Mimi's `src/windows/overlay/DragHandle.tsx`, which drives the
 * drag through an explicit IPC command because its overlay is nonactivating.
 */
export function DragHandle({ label = "Drag to move", width = 120 }) {
  return (
    <div className="overlay-drag-handle" data-tauri-drag-region>
      <Tooltip label={label}>
        {(descriptionId, hovered) => (
          <span
            className="overlay-drag-handle__hit"
            data-tauri-drag-region
            aria-describedby={descriptionId}
            style={{ width }}
          >
            <span
              aria-hidden="true"
              data-tauri-drag-region
              className="overlay-drag-handle__bar"
              style={{
                width: hovered ? 40 : 32,
                background: hovered
                  ? "rgba(122, 168, 255, 0.78)"
                  : "rgba(255, 255, 255, 0.28)",
              }}
            />
          </span>
        )}
      </Tooltip>
    </div>
  );
}
