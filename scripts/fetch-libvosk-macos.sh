#!/usr/bin/env bash
# Downloads the prebuilt Vosk dylib into src-tauri/vendor/macos/, where build.rs expects it
# and tauri.macos.conf.json bundles it from. Run once before `npm run tauri dev|build` on a
# Mac; CI runs it as a build step.
#
# Linux and Windows vendor their Vosk libraries in-tree, but the macOS one is a universal
# (x86_64 + arm64) binary that Vosk already publishes, so it is fetched rather than committed.
#
# Source is Vosk's own macOS *wheel* on PyPI rather than the GitHub release: the wheel is
# published as `macosx_*_universal2`, which is what makes Apple Silicon work, and its exact
# filename and contents are queryable from the PyPI JSON API instead of guessed at. The
# GitHub release is kept only as a last-ditch fallback.
set -euo pipefail

cd "$(dirname "$0")/.."
dest=src-tauri/vendor/macos
mkdir -p "$dest"

if [ -f "$dest/libvosk.dylib" ] && [ "${FORCE:-}" != "1" ]; then
  echo "$dest/libvosk.dylib already present (FORCE=1 to re-download)"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Looking up the newest universal2 vosk wheel on PyPI..."
wheel_url=$(python3 - <<'PY'
import json, urllib.request
d = json.load(urllib.request.urlopen("https://pypi.org/pypi/vosk/json", timeout=60))
def as_tuple(v):
    try: return tuple(int(x) for x in v.split("."))
    except ValueError: return ()
# Newest first: the latest release does not always ship a macOS wheel (0.3.45 did not).
for ver in sorted(d["releases"], key=as_tuple, reverse=True):
    for f in d["releases"][ver]:
        name = f["filename"]
        if name.endswith(".whl") and "macosx" in name and "universal2" in name:
            print(f["url"])
            raise SystemExit(0)
raise SystemExit("no universal2 macOS wheel found on PyPI")
PY
) || wheel_url=""

got=
if [ -n "$wheel_url" ]; then
  echo "Downloading $wheel_url"
  if curl -sSfL -o "$tmp/vosk.whl" "$wheel_url"; then
    unzip -q -o "$tmp/vosk.whl" -d "$tmp/whl"
    # Vosk names the library libvosk.dyld inside the wheel; the linker wants libvosk.dylib.
    lib=$(find "$tmp/whl" -type f \( -name 'libvosk.dyld' -o -name 'libvosk.dylib' \) | head -1)
    if [ -n "$lib" ]; then cp "$lib" "$dest/libvosk.dylib"; got=pypi; fi
  fi
fi

if [ -z "$got" ]; then
  # Unverified fallback — asset naming here could not be confirmed, so PyPI above is the
  # path that is actually expected to run. Only versions known to have had a macOS build.
  echo "PyPI lookup failed; trying GitHub releases..."
  for v in 0.3.44 0.3.43 0.3.42; do
    url="https://github.com/alphacephei/vosk-api/releases/download/v$v/vosk-osx-$v.zip"
    echo "Trying $url"
    if curl -sSfL -o "$tmp/vosk-osx.zip" "$url"; then
      unzip -q -o "$tmp/vosk-osx.zip" -d "$tmp/gh"
      lib=$(find "$tmp/gh" -type f \( -name 'libvosk.dylib' -o -name 'libvosk.dyld' \) | head -1)
      if [ -n "$lib" ]; then cp "$lib" "$dest/libvosk.dylib"; got="github v$v"; break; fi
    fi
  done
fi

[ -n "$got" ] || { echo "error: could not obtain libvosk for macOS from any source" >&2; exit 1; }

# Guard the Apple Silicon case explicitly: an Intel-only dylib would link fine on an Intel
# runner and fail only for M-series users, which is exactly the kind of break that reaches
# a release unnoticed.
archs=$(lipo -archs "$dest/libvosk.dylib")
echo "Installed libvosk from $got — architectures: $archs"
case "$archs" in
  *arm64*) ;;
  *) echo "error: libvosk.dylib has no arm64 slice ($archs) — Apple Silicon builds would fail" >&2; exit 1 ;;
esac

# The shipped install name is a bare filename, which dyld resolves against system paths only
# — never the rpaths build.rs embeds — so the copy inside Contents/Frameworks would be
# ignored. Rewrite it to @rpath, then re-ad-hoc-sign: install_name_tool invalidates the
# existing signature, and an invalid signature is fatal on Apple Silicon.
install_name_tool -id @rpath/libvosk.dylib "$dest/libvosk.dylib"
codesign --force --sign - "$dest/libvosk.dylib"
