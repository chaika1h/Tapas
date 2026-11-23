#!/usr/bin/env bash
# Builds the Windows installer from Linux (or WSL):
#   target/installer/Tapas-Setup-<version>.exe
# Needs the x86_64-pc-windows-gnu Rust target, mingw-w64 and NSIS (makensis).
# The version is the one in Cargo.toml.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT="$ROOT/target/installer"

cargo build --release --target x86_64-pc-windows-gnu
rm -rf "$OUT"
mkdir -p "$OUT"
cp target/x86_64-pc-windows-gnu/release/tapas.exe "$OUT/Tapas.exe"

(cd installer && makensis -V2 -DVERSION="$VERSION" -DOUT_DIR="$OUT" Tapas.nsi)

(cd "$OUT" && sha256sum Tapas-Setup-"$VERSION".exe > SHA256SUMS.txt)
ls -la "$OUT"
