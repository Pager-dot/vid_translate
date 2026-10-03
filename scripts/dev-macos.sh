#!/usr/bin/env bash
# Development run for macOS, with system-audio capture actually working.
#
# `npm run tauri dev` cannot capture system audio, and no amount of signing fixes it. TCC
# assigns responsibility for a permission request to the *process that launched* the
# requester, so a binary spawned by `cargo run` from your shell is attributed to the
# terminal, not to the app — the Core Audio tap is then denied, and because a denied tap
# still reports success and simply delivers silence, it looks like "no audio" rather than
# "no permission". This was established by experiment: the very same signed .app bundle
# captures nothing when its inner binary is started from a shell, and captures correctly
# when launched with `open`.
#
# So this script does what `tauri dev` does, except it launches through LaunchServices:
#
#   1. starts Vite (the binary is built with --no-default-features, so the webview loads
#      devUrl from tauri.conf.json rather than bundled assets),
#   2. builds the Rust binary,
#   3. wraps it in a throwaway .app so it has a bundle identity of its own,
#   4. ad-hoc signs that bundle,
#   5. `open`s it.
#
# The wrapper deliberately uses a *different* bundle identifier from the release app, so the
# two get separate entries under Privacy & Security and a dev build can never invalidate the
# grant belonging to an installed VidTranslate.
#
# Caveat nothing can fix without a Developer ID certificate: ad-hoc signatures change on
# every rebuild, so macOS asks for permission again each time you recompile.
set -euo pipefail

cd "$(dirname "$0")/.."

RELEASE_ID=$(node -p "require('./src-tauri/tauri.conf.json').identifier")
DEV_ID="${RELEASE_ID}.dev"
BINARY=src-tauri/target/debug/vid_translate
APP=src-tauri/target/debug/VidTranslateDev.app
DEV_URL=$(node -p "require('./src-tauri/tauri.conf.json').build.devUrl")

VITE_PID=""
cleanup() {
  [ -n "$VITE_PID" ] && kill "$VITE_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "==> Starting Vite on ${DEV_URL}"
npm run dev >/dev/null 2>&1 &
VITE_PID=$!
for _ in $(seq 1 60); do
  curl -sf -o /dev/null "$DEV_URL" && break
  sleep 0.5
done
curl -sf -o /dev/null "$DEV_URL" || { echo "Vite did not come up at ${DEV_URL}"; exit 1; }

# --no-default-features is what `tauri dev` itself runs: it makes the webview load devUrl
# instead of the assets baked in by the custom-protocol feature.
echo "==> Building ${BINARY}"
cargo build --manifest-path src-tauri/Cargo.toml --no-default-features

echo "==> Wrapping in ${APP} as ${DEV_ID}"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BINARY" "$APP/Contents/MacOS/vid_translate"
# The usage strings are the ones the real app ships; only the identity differs.
python3 - "$APP/Contents/Info.plist" "$DEV_ID" <<'PY'
import plistlib, pathlib, sys

out_path, dev_id = sys.argv[1], sys.argv[2]
# src-tauri/Info.plist is a *partial* plist that tauri-bundler merges into the generated one;
# read it as-is and add the keys a standalone bundle needs on top.
info = plistlib.loads(pathlib.Path("src-tauri/Info.plist").read_bytes())
info.update(
    {
        "CFBundleIdentifier": dev_id,
        "CFBundleExecutable": "vid_translate",
        "CFBundleName": "VidTranslate (dev)",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": "0.0.0",
    }
)
pathlib.Path(out_path).write_bytes(plistlib.dumps(info))
PY

codesign --force --deep --sign - --identifier "$DEV_ID" "$APP"

echo "==> Launching via LaunchServices"
echo "    macOS will ask for permission to record audio on the first capture,"
echo "    and again after every rebuild (ad-hoc signatures change each time)."
echo "    Ctrl-C here stops Vite; quit the app window to close the app."
open -W "$APP"
