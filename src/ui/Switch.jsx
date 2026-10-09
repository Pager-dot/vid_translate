/**
 * Platform-neutral toggle used by settings surfaces.
 * Ported from Mimi's `src/components/Switch.tsx`.
 */
export function Switch({ checked, onChange, disabled = false, ...rest }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      aria-label={rest["aria-label"]}
      aria-describedby={rest["aria-describedby"]}
      className={`mimi-switch${checked ? " is-checked" : ""}`}
    >
      <span className="mimi-switch__thumb" aria-hidden="true" />
    </button>
  );
}
