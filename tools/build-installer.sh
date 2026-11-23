#!/usr/bin/env bash
# Builds the Windows installer and the portable zip from Linux (or WSL):
#   target/installer/TapoViewer-Setup-<version>.exe
#   target/installer/TapoViewer-<version>-portable.zip
# Needs the x86_64-pc-windows-gnu Rust target, mingw-w64, NSIS (makensis) and
# zip. The version is the one in Cargo.toml.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT="$ROOT/target/installer"

cargo build --release --target x86_64-pc-windows-gnu
rm -rf "$OUT"
mkdir -p "$OUT/portable/TapoViewer"
cp target/x86_64-pc-windows-gnu/release/tapo-viewer.exe "$OUT/TapoViewer.exe"

(cd installer && makensis -V2 -DVERSION="$VERSION" -DOUT_DIR="$OUT" TapoViewer.nsi)

cp "$OUT/TapoViewer.exe" README.md "$OUT/portable/TapoViewer/"
(cd "$OUT/portable" && zip -q -9 -r "$OUT/TapoViewer-$VERSION-portable.zip" TapoViewer)
rm -rf "$OUT/portable"
(cd "$OUT" && sha256sum TapoViewer-Setup-"$VERSION".exe TapoViewer-"$VERSION"-portable.zip > SHA256SUMS.txt)
ls -la "$OUT"
