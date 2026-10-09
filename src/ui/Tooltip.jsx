import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";
import { tooltipPosition } from "./tooltipPosition";
import "./tooltip.css";

/**
 * Shared visible label for compact controls, including keyboard focus. The
 * children render as a function so a control can also style itself from the
 * hover state the tooltip already tracks, instead of tracking it twice.
 *
 * Ported from Mimi's `src/components/Tooltip.tsx`, minus its native
 * overlay-pointer bridge (our window activates on click, so React's own
 * pointer events are enough).
 */
export function Tooltip({ label, popupClassName, children }) {
  const id = useId();
  const trigger = useRef(null);
  const popup = useRef(null);
  const hovered = useRef(false);
  const dismissed = useRef(false);
  const pointerFocus = useRef(false);
  const [isHovered, setIsHovered] = useState(false);
  const [open, setOpen] = useState(false);

  const showOnHover = useCallback(() => {
    if (!hovered.current) {
      hovered.current = true;
      setIsHovered(true);
    }
    if (!dismissed.current) setOpen(true);
  }, []);

  const leaveHover = useCallback(() => {
    if (hovered.current) setIsHovered(false);
    hovered.current = false;
    dismissed.current = false;
    setOpen(false);
  }, []);

  const dismiss = () => {
    dismissed.current = true;
    setOpen(false);
  };

  useLayoutEffect(() => {
    if (!open) return;
    const position = () => {
      const anchor = trigger.current;
      const tip = popup.current;
      if (!anchor || !tip) return;
      const next = tooltipPosition(
        anchor.getBoundingClientRect(),
        tip.getBoundingClientRect(),
        { width: window.innerWidth, height: window.innerHeight },
      );
      tip.style.left = `${next.left}px`;
      tip.style.top = `${next.top}px`;
      tip.style.visibility = "visible";
    };
    position();
    const observer = new ResizeObserver(position);
    if (trigger.current) observer.observe(trigger.current);
    if (popup.current) observer.observe(popup.current);
    window.addEventListener("resize", position);
    document.addEventListener("scroll", position, true);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", position);
      document.removeEventListener("scroll", position, true);
    };
  }, [open, label]);

  useEffect(() => {
    const useKeyboard = () => {
      pointerFocus.current = false;
    };
    const blurWindow = () => leaveHover();
    document.addEventListener("keydown", useKeyboard, true);
    window.addEventListener("blur", blurWindow);
    return () => {
      document.removeEventListener("keydown", useKeyboard, true);
      window.removeEventListener("blur", blurWindow);
    };
  }, [leaveHover]);

  return (
    <span
      ref={trigger}
      className="mimi-tooltip-trigger"
      onPointerEnter={(event) => {
        if (event.pointerType !== "touch") showOnHover();
      }}
      onPointerMove={(event) => {
        if (event.pointerType !== "touch") showOnHover();
      }}
      onMouseMove={showOnHover}
      onPointerLeave={leaveHover}
      onMouseLeave={leaveHover}
      onPointerDownCapture={() => {
        pointerFocus.current = true;
        dismiss();
      }}
      onClickCapture={dismiss}
      onFocus={(event) => {
        if (!pointerFocus.current && event.target.matches(":focus-visible")) {
          dismissed.current = false;
          setOpen(true);
        }
      }}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) {
          pointerFocus.current = false;
          if (!hovered.current) setOpen(false);
        }
      }}
      onKeyDown={(event) => {
        if (event.key === "Escape" && open) {
          event.preventDefault();
          event.stopPropagation();
          dismiss();
        }
      }}
    >
      {children(open ? id : undefined, isHovered)}
      {open &&
        createPortal(
          <div
            ref={popup}
            id={id}
            className={popupClassName ? `mimi-tooltip ${popupClassName}` : "mimi-tooltip"}
            role="tooltip"
          >
            {label}
          </div>,
          document.body,
        )}
    </span>
  );
}
