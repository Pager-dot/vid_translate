import { useEffect, useId, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Icon } from "./Icon";
import "./select.css";

/**
 * One app-styled picker for every settings row: a button that reads the
 * current choice and a portalled menu that flips above the trigger when there
 * is no room below. Keyboard behaviour matches a native `<select>` — arrows,
 * Home/End, Enter, Escape and first-letter typeahead.
 *
 * Ported from Mimi's `src/components/Select.tsx`, minus its searchable and
 * lazily-loaded variants (every list here is short and known up front).
 *
 * Options are `{ value, label, description?, icon? }`.
 */
export function Select({ label, value, options, disabled = false, onChange }) {
  const id = useId();
  const trigger = useRef(null);
  const menu = useRef(null);
  const revealActive = useRef(true);
  const optionNodes = useRef(new Map());
  const typeahead = useRef({ text: "", at: 0 });
  const [popup, setPopup] = useState(null);

  const selected = options.findIndex((option) => option.value === value);
  const selectedLabel = options[selected]?.label ?? value;
  const selectedIcon = selected >= 0 ? options[selected].icon : undefined;
  const [cursor, setCursor] = useState({ selection: value, index: 0 });
  // A value can change from elsewhere while the menu is open, so the active row
  // falls back to the current selection whenever the cursor is stale.
  const active =
    options.length === 0
      ? -1
      : Math.min(
          options.length - 1,
          cursor.selection === value && cursor.index >= 0
            ? cursor.index
            : Math.max(0, selected),
        );
  const open = popup !== null && !disabled;

  function setActive(index, reveal = true) {
    revealActive.current = reveal;
    setCursor((previous) => ({
      selection: value,
      index:
        typeof index === "number"
          ? index
          : index(
              previous.selection === value && previous.index >= 0
                ? previous.index
                : Math.max(0, selected),
            ),
    }));
  }

  function show() {
    const button = trigger.current;
    if (!button || disabled || options.length === 0) return;
    button.focus();
    const rect = button.getBoundingClientRect();
    const theme = getComputedStyle(button);
    const detailed = options.some((option) => !!option.description);
    const width = Math.min(
      Math.max(rect.width, detailed ? 280 : 200),
      window.innerWidth - 16,
    );
    const below = window.innerHeight - rect.bottom - 12;
    const above = rect.top - 12;
    const desired = Math.min(
      options.reduce((height, option) => height + (option.description ? 74 : 38), 10),
      280,
    );
    const upwards = below < desired && above > below;
    const height = Math.min(desired, Math.max(40, upwards ? above : below));
    setPopup({
      position: "fixed",
      width,
      maxHeight: height,
      left: Math.max(8, Math.min(rect.left, window.innerWidth - width - 8)),
      ...(upwards ? { bottom: window.innerHeight - rect.top + 5 } : { top: rect.bottom + 5 }),
      color: theme.color,
      background: theme.getPropertyValue("--select-menu-bg"),
      borderColor: theme.getPropertyValue("--select-menu-border"),
      fontFamily: theme.fontFamily,
      "--select-hover": theme.getPropertyValue("--select-hover"),
    });
    revealActive.current = true;
    setCursor({ selection: value, index: -1 });
    typeahead.current = { text: "", at: 0 };
  }

  function choose(index) {
    const option = options[index];
    setPopup(null);
    trigger.current?.focus();
    if (option && option.value !== value) onChange(option.value);
  }

  useEffect(() => {
    if (!open) return;
    const outside = (event) => {
      const node = event.target;
      if (!trigger.current?.contains(node) && !menu.current?.contains(node)) setPopup(null);
    };
    const dismiss = () => setPopup(null);
    const scroll = (event) => {
      if (!menu.current?.contains(event.target)) dismiss();
    };
    document.addEventListener("pointerdown", outside, true);
    document.addEventListener("focusin", outside);
    document.addEventListener("scroll", scroll, true);
    window.addEventListener("resize", dismiss);
    window.addEventListener("blur", dismiss);
    return () => {
      document.removeEventListener("pointerdown", outside, true);
      document.removeEventListener("focusin", outside);
      document.removeEventListener("scroll", scroll, true);
      window.removeEventListener("resize", dismiss);
      window.removeEventListener("blur", dismiss);
    };
  }, [open]);

  useEffect(() => {
    const option = options[active];
    const node = option && optionNodes.current.get(option.value);
    const scroller = menu.current;
    if (!open || !node || !scroller || !revealActive.current) return;
    // scrollIntoView also scrolls overflow:hidden ancestors in WebKit. Scroll
    // only the menu itself.
    const row = node.getBoundingClientRect();
    const bounds = scroller.getBoundingClientRect();
    const top = bounds.top + scroller.clientTop;
    const bottom = top + scroller.clientHeight;
    if (row.top < top) scroller.scrollTop -= top - row.top;
    else if (row.bottom > bottom) scroller.scrollTop += row.bottom - bottom;
  }, [active, cursor, open, options]);

  function onKeyDown(event) {
    if (disabled || event.nativeEvent.isComposing) return;
    if (event.key === "Tab" || event.key === "Escape") {
      if (open && event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
      }
      setPopup(null);
      return;
    }
    if (["ArrowDown", "ArrowUp", "Home", "End", "Enter", " "].includes(event.key)) {
      event.preventDefault();
      if (!open) {
        show();
        return;
      }
      if (options.length === 0) return;
      if (event.key === "Enter" || event.key === " ") choose(active);
      else if (event.key === "Home") setActive(0);
      else if (event.key === "End") setActive(options.length - 1);
      else
        setActive(
          (index) =>
            (index + (event.key === "ArrowDown" ? 1 : -1) + options.length) % options.length,
        );
      return;
    }
    if (event.key.length === 1 && !event.metaKey && !event.ctrlKey && !event.altKey) {
      if (!open) show();
      const now = event.timeStamp;
      const text =
        (now - typeahead.current.at < 700 ? typeahead.current.text : "") +
        event.key.toLocaleLowerCase();
      typeahead.current = { text, at: now };
      const index = options.findIndex((option) =>
        option.label.toLocaleLowerCase().startsWith(text),
      );
      if (index >= 0) setActive(index);
    }
  }

  const activeId = open && active >= 0 ? `${id}-${active}` : undefined;

  return (
    <span className="mimi-select">
      <button
        ref={trigger}
        type="button"
        className="mimi-select__trigger"
        role="combobox"
        aria-label={label}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        aria-activedescendant={activeId}
        disabled={disabled || options.length === 0}
        onKeyDown={onKeyDown}
        onClick={() => (open ? setPopup(null) : show())}
      >
        {/* Replacing the label node also invalidates retained WebKit pixels on
            external value changes, while the focused trigger stays stable. */}
        <span key={`${value}\u0000${selectedLabel}`} className="mimi-select__content">
          {selectedIcon && (
            <span className="mimi-select__icon" aria-hidden="true">
              {selectedIcon}
            </span>
          )}
          <span className="mimi-select__label">{selectedLabel}</span>
        </span>
        <Icon name="chevron-down" />
      </button>
      {open &&
        createPortal(
          <div
            ref={menu}
            id={id}
            className="mimi-select__menu"
            role="listbox"
            aria-label={label}
            style={popup}
          >
            {options.map((option, index) => (
              <div
                key={option.value}
                id={`${id}-${index}`}
                ref={(node) => {
                  if (node) optionNodes.current.set(option.value, node);
                  else optionNodes.current.delete(option.value);
                }}
                className="mimi-select__option"
                role="option"
                aria-selected={option.value === value}
                data-active={index === active}
                onPointerMove={() => setActive(index, false)}
                onPointerDown={(event) => event.preventDefault()}
                onClick={() => choose(index)}
              >
                <span className="mimi-select__content">
                  {option.icon && (
                    <span className="mimi-select__icon" aria-hidden="true">
                      {option.icon}
                    </span>
                  )}
                  <span
                    className={`mimi-select__option-copy${
                      option.description ? " mimi-select__option-copy--detailed" : ""
                    }`}
                  >
                    <span className="mimi-select__label">{option.label}</span>
                    {option.description && (
                      <span className="mimi-select__description">{option.description}</span>
                    )}
                  </span>
                </span>
                {option.value === value && <Icon name="checkmark" />}
              </div>
            ))}
          </div>,
          document.body,
        )}
    </span>
  );
}
