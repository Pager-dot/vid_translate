#!/usr/bin/env bash
# Offline JA eval harness (Phase 5).
#
# Runs the real pipeline (Vosk -> chunker -> CTranslate2) over every clip in eval/ja,
# writes hypotheses, and scores them against the hand-written references with sacrebleu.
#
#   ./eval/run.sh                     # score every clip in eval/ja
#   ./eval/run.sh out/baseline        # write results to a named directory
#   VID_TRANSLATE_JA_MODEL_DIR=... ./eval/run.sh out/fp32   # Phase 4 A/B
#
# BLEU on 20-30 short clips is noisy. Treat it as a regression tripwire, not a leaderboard:
# a 3-4 point swing is within the noise of this corpus, a 10 point drop is a real break.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
clips_dir="$repo_root/eval/ja"
out_dir="${1:-$repo_root/eval/out/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$out_dir"

shopt -s nullglob
clips=("$clips_dir"/*.wav)
if [ ${#clips[@]} -eq 0 ]; then
  echo "no clips in $clips_dir — see eval/ja/README.md for how to add them" >&2
  exit 1
fi

echo "==> building ja_eval"
cargo build --release --manifest-path "$repo_root/src-tauri/Cargo.toml" --bin ja_eval

echo "==> running ${#clips[@]} clip(s)"
"$repo_root/src-tauri/target/release/ja_eval" "${clips[@]}" \
  > "$out_dir/results.jsonl" 2> "$out_dir/run.log"
tail -1 "$out_dir/run.log"

# One hypothesis per line, in the same clip order as the references file.
python3 - "$out_dir" "$clips_dir" <<'PY'
import json, sys, pathlib
out_dir, clips_dir = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
rows = {}
for line in (out_dir / "results.jsonl").read_text().splitlines():
    if line.strip():
        r = json.loads(line)
        rows[r["clip"]] = r

hyps, refs, missing = [], [], []
for wav in sorted(clips_dir.glob("*.wav")):
    ref = wav.with_suffix(".txt")
    if not ref.exists():
        missing.append(ref.name)
        continue
    row = rows.get(wav.name)
    if row is None:
        missing.append(wav.name + " (no result)")
        continue
    hyps.append(" ".join(row["hypothesis"].split()))
    refs.append(" ".join(ref.read_text().split()))

(out_dir / "hyp.txt").write_text("\n".join(hyps) + "\n")
(out_dir / "ref.txt").write_text("\n".join(refs) + "\n")

chunks = sum(r["chunk_count"] for r in rows.values())
forced = sum(r["forced_cuts"] for r in rows.values())
print(f"scored {len(hyps)} clip(s); {chunks} chunk(s), {forced} forced cut(s)"
      f" ({(100*forced/chunks if chunks else 0):.0f}%)")
if missing:
    print("skipped (no reference / no result): " + ", ".join(missing))
PY

if command -v sacrebleu >/dev/null 2>&1; then
  echo "==> sacrebleu"
  sacrebleu "$out_dir/ref.txt" -i "$out_dir/hyp.txt" -m bleu chrf --width 2 \
    | tee "$out_dir/score.txt"
else
  echo "sacrebleu not installed — 'pip install sacrebleu' to score. Hypotheses are in $out_dir/hyp.txt" >&2
fi

echo "==> results in $out_dir"
