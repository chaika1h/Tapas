#!/usr/bin/env bash
# Publishes the version in Cargo.toml as a GitHub release, which is where the
# app looks for updates: builds the installer (tools/build-installer.sh) and
# uploads it as release v<version>.
#
# Usage: tools/release.sh [owner/repo] ["release notes"]
#   The repository defaults to the app's (src/update.rs). Its releases must be
#   public for the app to see them. Needs the GitHub CLI (gh), signed in.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
REPO=${1:-$(sed -n 's/^pub const DEFAULT_REPO: &str = "\(.*\)";/\1/p' src/update.rs)}
NOTES=${2:-}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

if gh release view "v$VERSION" --repo "$REPO" > /dev/null 2>&1; then
  echo "v$VERSION is already released on $REPO: raise the version in Cargo.toml first." >&2
  exit 1
fi

tools/build-installer.sh
OUT="$ROOT/target/installer"
# Tapo Viewer 0.2.0 (the app's former name) looks for TapoViewer-Setup-*.exe;
# the same installer under that name lets it update to Tapas.
cp "$OUT/Tapas-Setup-$VERSION.exe" "$OUT/TapoViewer-Setup-$VERSION.exe"
gh release create "v$VERSION" --repo "$REPO" --title "Tapas $VERSION" --notes "$NOTES" \
  "$OUT/Tapas-Setup-$VERSION.exe" "$OUT/SHA256SUMS.txt" \
  "$OUT/TapoViewer-Setup-$VERSION.exe"
echo "Released v$VERSION on $REPO; the app offers it at its next start."
