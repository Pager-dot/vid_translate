#!/usr/bin/env bash
#
# Drives whisper_bench over a matrix of configurations and collects one CSV.
#
# The matrix lives here rather than inside the binary because the two most interesting
# dimensions can only be set from outside a process: core affinity (taskset) and whatever
# else the machine is doing. See bin/whisper_bench.rs for why one process per cell is
# required rather than merely tidy.
#
# ## The load condition is the point
#
# A benchmark run on an idle machine answers a question nobody has. The app runs while the
# user watches a video, which on a 15W laptop with integrated graphics competes for the
# EUs, the memory bandwidth and the power budget all at once. So this script does not try
# to generate load — it refuses to guess at a realistic one — it takes a --label and asks
# you to set the machine up yourself, then records that label against every row. Run the
# matrix twice, once idle and once with a 1080p video playing in a browser, and diff.
#
# ## Usage
#
#   scripts/bench-whisper.sh --audio eval/ja/clip01.wav --label idle
#   scripts/bench-whisper.sh --audio eval/ja/clip01.wav --label browser-video
#
#   --audio <wav>        16kHz mono 16-bit PCM WAV with real speech in it  (required)
#   --label <text>       recorded against every row; name the load condition
#   --models <dir>       where the ggml-*.bin files live  [default: the app's model dir]
#   --preset quick|full  quick: ~12 cells, minutes. full: every cell, ~an hour.  [quick]
#   --out <csv>          output file  [default: bench-<label>-<date>.csv]
#   --warmup-ms <ms>     per cell, discarded  [60000; 'quick' lowers to 20000]
#   --measure-ms <ms>    per cell  [60000; 'quick' lowers to 20000]
#   --dry-run            print the matrix and exit without running anything
#   --yes                do not wait for Enter before starting (for unattended runs)
#
set -euo pipefail

cd "$(dirname "$0")/.."

AUDIO=""
LABEL="unlabelled"
MODELS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/vid_translate/models"
PRESET="quick"
OUT=""
WARMUP_MS=""
MEASURE_MS=""
DRY_RUN=0
ASSUME_READY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --audio)      AUDIO="$2"; shift 2 ;;
    --label)      LABEL="$2"; shift 2 ;;
    --models)     MODELS_DIR="$2"; shift 2 ;;
    --preset)     PRESET="$2"; shift 2 ;;
    --out)        OUT="$2"; shift 2 ;;
    --warmup-ms)  WARMUP_MS="$2"; shift 2 ;;
    --measure-ms) MEASURE_MS="$2"; shift 2 ;;
    --dry-run)    DRY_RUN=1; shift ;;
    --yes)        ASSUME_READY=1; shift ;;
    -h|--help)    sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unrecognised argument '$1' (try --help)" >&2; exit 2 ;;
  esac
done

[[ -n "$AUDIO" ]] || { echo "--audio is required (try --help)" >&2; exit 2; }
[[ -f "$AUDIO" ]] || { echo "no such audio file: $AUDIO" >&2; exit 2; }

case "$PRESET" in
  quick) : "${WARMUP_MS:=20000}"; : "${MEASURE_MS:=20000}" ;;
  full)  : "${WARMUP_MS:=60000}"; : "${MEASURE_MS:=60000}" ;;
  *) echo "--preset must be 'quick' or 'full'" >&2; exit 2 ;;
esac

: "${OUT:=bench-${LABEL}-$(date +%Y%m%d-%H%M).csv}"

# ---------------------------------------------------------------------------
# Topology. On a hybrid Intel part the kernel publishes which CPUs are which,
# and that split is the whole reason this script has an affinity loop: ggml
# divides each tensor op into equal chunks and waits for all of them, so one
# E-core thread paces a pass that P-cores finished early. Asking the kernel
# rather than hardcoding cpu0-3 keeps this honest on other machines.
# ---------------------------------------------------------------------------
P_CORES=""
E_CORES=""
if [[ -d /sys/devices/system/cpu/types ]]; then
  for d in /sys/devices/system/cpu/types/intel_core_*; do
    [[ -r "$d/cpulist" ]] && P_CORES="$(<"$d/cpulist")"
  done
  for d in /sys/devices/system/cpu/types/intel_atom_*; do
    [[ -r "$d/cpulist" ]] && E_CORES="$(<"$d/cpulist")"
  done
fi
NPROC="$(nproc)"

# Affinity sets to sweep, as "name:cpulist" pairs. An empty cpulist means "no taskset",
# i.e. whatever the scheduler decides, which is what the app does today.
AFFINITY_SETS=("all:")
if [[ -n "$P_CORES" && -n "$E_CORES" ]]; then
  AFFINITY_SETS+=("ecores:$E_CORES" "pcores:$P_CORES")
  echo "hybrid CPU detected — P-cores: $P_CORES   E-cores: $E_CORES"
else
  echo "no hybrid-CPU split published by the kernel; sweeping 'all' only ($NPROC CPUs)"
fi

# ---------------------------------------------------------------------------
# Models. Only what is actually on disk — the point is to compare what you
# could ship, not to report on a model the user has not downloaded.
# ---------------------------------------------------------------------------
# Sorted by file size, ascending — NOT by name. Alphabetically 'base' sorts before 'small'
# before 'tiny', so a name sort makes "smallest and largest" pick base and tiny and skip
# small entirely. Size is also the thing that actually predicts cost.
mapfile -t AVAILABLE < <(find "$MODELS_DIR" -maxdepth 1 -name 'ggml-*.bin' -not -name '*silero*' \
  -printf '%s\t%p\n' 2>/dev/null | sort -n | cut -f2- || true)
if [[ ${#AVAILABLE[@]} -eq 0 ]]; then
  echo "no ggml-*.bin models in $MODELS_DIR" >&2
  echo "download at least two sizes to compare (the app's settings pane does this), or" >&2
  echo "pass --models <dir>." >&2
  exit 1
fi

if [[ "$PRESET" == "quick" ]]; then
  # Smallest and largest on disk: the question 'quick' answers is whether model size is the
  # lever, and the extremes answer it with the fewest cells.
  MODELS=("${AVAILABLE[0]}")
  [[ ${#AVAILABLE[@]} -gt 1 ]] && MODELS+=("${AVAILABLE[-1]}")
  THREAD_COUNTS=(2 4 "$((NPROC > 2 ? NPROC - 2 : 2))")
  WINDOWS=(10000)
  FLASH=(0 1)
else
  MODELS=("${AVAILABLE[@]}")
  THREAD_COUNTS=(1 2 4 6 8 "$((NPROC > 2 ? NPROC - 2 : 2))" "$NPROC")
  WINDOWS=(5000 10000 15000)
  FLASH=(0 1)
fi

# Deduplicate thread counts (nproc-2 can collide with a literal above).
mapfile -t THREAD_COUNTS < <(printf '%s\n' "${THREAD_COUNTS[@]}" | sort -n -u)

BIN="src-tauri/target/release/whisper_bench"
CELLS=$(( ${#AFFINITY_SETS[@]} * ${#MODELS[@]} * ${#THREAD_COUNTS[@]} * ${#WINDOWS[@]} * ${#FLASH[@]} ))
SECS=$(( CELLS * (WARMUP_MS + MEASURE_MS) / 1000 ))

cat <<EOF

matrix: $CELLS cells, roughly $(( SECS / 60 )) min of measurement (plus model load per cell)
  affinity:  ${AFFINITY_SETS[*]}
  models:    $(printf '%s ' "${MODELS[@]##*/}")
  threads:   ${THREAD_COUNTS[*]}
  windows:   ${WINDOWS[*]} ms
  flash-attn:${FLASH[*]}
  warmup:    ${WARMUP_MS}ms per cell, measure ${MEASURE_MS}ms per cell
  label:     $LABEL
  out:       $OUT
EOF

if [[ $DRY_RUN -eq 1 ]]; then
  echo
  echo "(--dry-run: nothing executed)"
  exit 0
fi

# ---------------------------------------------------------------------------
# Preflight. Each of these silently invalidates a run rather than failing it,
# which is the worst way for a benchmark to be wrong, so they are checked
# loudly up front.
# ---------------------------------------------------------------------------
echo
if [[ ! -x "$BIN" ]]; then
  echo "building whisper_bench (release — a debug build measures the wrong thing entirely)"
  cargo build --release --features eval --bin whisper_bench --manifest-path src-tauri/Cargo.toml
fi

if command -v powerprofilesctl >/dev/null 2>&1; then
  PROFILE="$(powerprofilesctl get 2>/dev/null || echo unknown)"
  if [[ "$PROFILE" != "performance" ]]; then
    echo "WARNING: power profile is '$PROFILE', not 'performance'."
    echo "         On a 15W part this alone can halve sustained clocks. Consider:"
    echo "           powerprofilesctl set performance"
  fi
fi

if [[ -r /sys/class/power_supply/AC/online ]] && [[ "$(</sys/class/power_supply/AC/online)" != "1" ]]; then
  echo "WARNING: running on battery. Sustained clocks differ from mains; results will not"
  echo "         compare against a mains run."
fi

if [[ ! -r /sys/class/powercap/intel-rapl:0/energy_uj ]]; then
  echo "note: RAPL is not readable, so the watts column will be empty. To populate it:"
  echo "        sudo chmod +r /sys/class/powercap/intel-rapl:0/energy_uj"
  echo "      Without it a power-limited cell is indistinguishable from a slow one."
fi

echo
echo "Set the machine up for the '$LABEL' condition now (e.g. start the browser video)."
if [[ $ASSUME_READY -eq 0 ]]; then
  read -r -p "Press Enter when ready, or Ctrl-C to abort. " _
fi

# ---------------------------------------------------------------------------
# Run. Header once, then one row appended per cell so a long run that is
# interrupted still leaves usable data.
# ---------------------------------------------------------------------------
"$BIN" --print-header > "$OUT" || {
  echo "could not produce a CSV header — is the binary built?" >&2; exit 1; }

CELL=0
for aff in "${AFFINITY_SETS[@]}"; do
  aff_name="${aff%%:*}"
  aff_list="${aff#*:}"
  for model in "${MODELS[@]}"; do
    for threads in "${THREAD_COUNTS[@]}"; do
      for window in "${WINDOWS[@]}"; do
        for flash in "${FLASH[@]}"; do
          CELL=$(( CELL + 1 ))
          args=(
            --model "$model" --audio "$AUDIO"
            --threads "$threads" --window-ms "$window"
            --warmup-ms "$WARMUP_MS" --measure-ms "$MEASURE_MS"
            --label "${LABEL}/${aff_name}"
          )
          [[ "$flash" == "1" ]] && args+=(--flash-attn)

          echo "[$CELL/$CELLS] ${model##*/} threads=$threads window=${window}ms flash=$flash aff=$aff_name"
          if [[ -n "$aff_list" ]]; then
            taskset -c "$aff_list" "$BIN" "${args[@]}" >> "$OUT" || echo "  cell failed, skipped"
          else
            "$BIN" "${args[@]}" >> "$OUT" || echo "  cell failed, skipped"
          fi
        done
      done
    done
  done
done

echo
echo "wrote $OUT"
echo
echo "Cells that keep up at the minimum step (headroom_at_2s >= 1.0), best first:"
# headroom_at_2s is column 16; see CSV_HEADER in bin/whisper_bench.rs.
{ head -1 "$OUT"; tail -n +2 "$OUT" | sort -t, -k16 -gr; } | column -s, -t | head -20
