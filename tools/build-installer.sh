#!/usr/bin/env bash
# Builds the Windows installer and the portable zip from Linux (or WSL):
#   target/installer/Tapas-Setup-<version>.exe
#   target/installer/Tapas-<version>-portable.zip
# Needs the x86_64-pc-windows-gnu Rust target, mingw-w64, NSIS (makensis) and
# zip. The version is the one in Cargo.toml.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT="$ROOT/target/installer"

cargo build --release --target x86_64-pc-windows-gnu
rm -rf "$OUT"
mkdir -p "$OUT/portable/Tapas"
cp target/x86_64-pc-windows-gnu/release/tapas.exe "$OUT/Tapas.exe"

(cd installer && makensis -V2 -DVERSION="$VERSION" -DOUT_DIR="$OUT" Tapas.nsi)

cp "$OUT/Tapas.exe" README.md LICENSE "$OUT/portable/Tapas/"
(cd "$OUT/portable" && zip -q -9 -r "$OUT/Tapas-$VERSION-portable.zip" Tapas)
rm -rf "$OUT/portable"
(cd "$OUT" && sha256sum Tapas-Setup-"$VERSION".exe Tapas-"$VERSION"-portable.zip > SHA256SUMS.txt)
ls -la "$OUT"
