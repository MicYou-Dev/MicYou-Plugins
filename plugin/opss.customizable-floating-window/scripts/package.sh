#!/usr/bin/env bash
# Assemble a cross-platform plugin zip from locally built artifacts.
#
#   scripts/package.sh            # linux native + windows cross (mingw) if present
#
# The FULL three-platform bundle (incl. macOS dylibs) is produced by
# .github/workflows/release.yml — macOS binaries require the Apple SDK and
# cannot be cross-compiled from Linux. Zips built here are therefore labeled
# `-linux+windows` and are intended for development / LAN distribution.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-release}"
OUT="dist/stage"
rm -rf "$OUT" dist/plugin-linux+windows.zip
mkdir -p "$OUT/bin" "$OUT/themes"

cp plugin.json panel.html LICENSE README.md "$OUT/"
cp themes/*.slint "$OUT/themes/"

# Linux (native)
if [ -f "target/$PROFILE/libcustomizable_floating_window.so" ]; then
  cp "target/$PROFILE/libcustomizable_floating_window.so" "$OUT/customizable_floating_window.so"
  cp "target/$PROFILE/floating_helper" "$OUT/bin/floating-helper-linux-x86_64"
  chmod +x "$OUT/bin/floating-helper-linux-x86_64"
fi

# Windows (cross via mingw, or native on Windows runners)
for cand in \
  "target/x86_64-pc-windows-gnu/$PROFILE/customizable_floating_window.dll" \
  "target/$PROFILE/customizable_floating_window.dll"; do
  if [ -f "$cand" ]; then
    cp "$cand" "$OUT/customizable_floating_window.dll"
    break
  fi
done
for cand in \
  "target/x86_64-pc-windows-gnu/$PROFILE/floating_helper.exe" \
  "target/$PROFILE/floating_helper.exe"; do
  if [ -f "$cand" ]; then
    cp "$cand" "$OUT/bin/floating-helper-windows-x86_64.exe"
    break
  fi
done

# macOS helper (built natively on a Mac; absent in local dev bundles)
if [ -f "target/$PROFILE/floating_helper" ] && [ "$(uname)" = "Darwin" ]; then
  cp "target/$PROFILE/floating_helper" "$OUT/bin/floating-helper-macos-aarch64"
  chmod +x "$OUT/bin/floating-helper-macos-aarch64"
fi
# NOTE: round 6 rolled back to the helper-subprocess process model on ALL
# platforms (the in-process winit thread proved unstable inside the host
# process on Windows), so bin/ carries a helper per platform again.


( cd "$OUT" && zip -qr "../plugin-linux+windows.zip" . )
echo "==> dist/plugin-linux+windows.zip"
python3 - <<'PY'
import zipfile
for i in zipfile.ZipFile("dist/plugin-linux+windows.zip").infolist():
    print(f"{i.file_size:>9}  {i.filename}")
PY
