#!/usr/bin/env bash
# Assemble a plugin zip from locally built artifacts.
#
#   scripts/package.sh            # linux native + windows cross (mingw) when available
#
# Contents included (whatever exists under target/):
#   micyou_hotkey.so     — Linux   (native build)
#   micyou_hotkey.dll    — Windows (cross via mingw: `cargo build --release
#                                   --target x86_64-pc-windows-gnu`, or native
#                                   on a Windows box)
#   micyou_hotkey.dylib  — macOS   (only when this script runs on a Mac; the
#                                   Apple SDK is required)
#
# The FULL three-platform bundle is produced by .github/workflows/release.yml.
# Zips built here are labeled `plugin-local.zip` and are intended for
# development / LAN distribution. The zip root IS the plugin directory
# (plugin.json at the root) — unzip straight into MicYou's plugin folder or
# use the in-app import.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-release}"
OUT="dist/stage"
rm -rf "$OUT" dist/plugin-local.zip
mkdir -p "$OUT"

cp plugin.json panel.html LICENSE README.md "$OUT/"

# Linux (native)
if [ -f "target/$PROFILE/libmicyou_hotkey.so" ]; then
  cp "target/$PROFILE/libmicyou_hotkey.so" "$OUT/micyou_hotkey.so"
fi

# Windows (cross via mingw, or native on Windows runners)
for cand in \
  "target/x86_64-pc-windows-gnu/$PROFILE/micyou_hotkey.dll" \
  "target/$PROFILE/micyou_hotkey.dll"; do
  if [ -f "$cand" ]; then
    cp "$cand" "$OUT/micyou_hotkey.dll"
    break
  fi
done

# macOS (native only — cannot be cross-compiled from Linux)
if [ "$(uname)" = "Darwin" ] && [ -f "target/$PROFILE/libmicyou_hotkey.dylib" ]; then
  cp "target/$PROFILE/libmicyou_hotkey.dylib" "$OUT/micyou_hotkey.dylib"
fi

# At least one platform binary must be present
if ! ls "$OUT"/micyou_hotkey.* >/dev/null 2>&1; then
  echo "error: no plugin binary found under target/$PROFILE (or the windows-gnu cross dir)." >&2
  echo "hint:  cargo build --release   [ && cargo build --release --target x86_64-pc-windows-gnu ]" >&2
  exit 1
fi

( cd "$OUT" && zip -qr "../plugin-local.zip" . )
echo "==> dist/plugin-local.zip"
python3 - <<'PY'
import zipfile
for i in sorted(zipfile.ZipFile("dist/plugin-local.zip").infolist(), key=lambda i: i.filename):
    print(f"{i.file_size:>9}  {i.filename}")
PY
