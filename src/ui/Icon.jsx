import {
  AlertTriangle,
  Check,
  ChevronDown,
  CircleHelp,
  Cloud,
  Download,
  Eraser,
  Gauge,
  Key,
  Languages,
  Mic,
  Play,
  RotateCcw,
  Settings,
  Sliders,
  Sparkles,
  Square,
  Type,
  Volume2,
  Waves,
  X,
} from "lucide-react";

/**
 * Icon set backed by tree-shakeable Lucide SVG components. Icons render at
 * 1em and inherit the current text color, so a control sizes its icon with
 * `font-size` and never has to restate a color.
 *
 * Ported from Mimi's `src/components/Icon.tsx`.
 */
const ICONS = {
  "alert-triangle": AlertTriangle,
  checkmark: Check,
  "chevron-down": ChevronDown,
  cloud: Cloud,
  close: X,
  download: Download,
  eraser: Eraser,
  gauge: Gauge,
  gear: Settings,
  help: CircleHelp,
  key: Key,
  languages: Languages,
  microphone: Mic,
  play: Play,
  reset: RotateCcw,
  sliders: Sliders,
  sparkles: Sparkles,
  speaker: Volume2,
  stop: Square,
  type: Type,
  waves: Waves,
};

export function Icon({ name, className, style }) {
  const Component = ICONS[name];
  if (!Component) return null;
  return (
    <Component
      className={className}
      style={style}
      width="1em"
      height="1em"
      aria-hidden="true"
    />
  );
}
