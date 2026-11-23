#!/usr/bin/env bash
# Builds a minimal, LGPL-only ffmpeg with just what Tapo Viewer needs:
# MPEG-TS input from stdin (the Tapo cloud video relay), H.264/H.265
# decoding, scaling and raw video output. No network or other formats.
# The result is a few MB instead of ~100 MB for a full build, and LGPL means
# it can be bundled with the app (see vendor/README.md).
#
# Usage: tools/build-ffmpeg.sh [windows|native] [OUTPUT]
#   windows (default): cross-compiles ffmpeg.exe with mingw-w64 -> vendor/ffmpeg.exe
#   native:            builds for this machine (for testing)  -> vendor/ffmpeg
set -euo pipefail

TARGET=${1:-windows}
VERSION=7.1.1
SHA256=733984395e0dbbe5c046abda2dc49a5544e7e0e1e2366bba849222ae9e3a03b1
URLS=(
  "https://ffmpeg.org/releases/ffmpeg-${VERSION}.tar.xz"
  # Ubuntu's mirror carries the identical upstream tarball.
  "http://archive.ubuntu.com/ubuntu/pool/universe/f/ffmpeg/ffmpeg_${VERSION}.orig.tar.xz"
)

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${FFMPEG_BUILD_DIR:-$ROOT/target/ffmpeg-build}
case "$TARGET" in
  windows) OUT=${2:-$ROOT/vendor/ffmpeg.exe} ;;
  native) OUT=${2:-$ROOT/vendor/ffmpeg} ;;
  *) echo "usage: $0 [windows|native] [OUTPUT]" >&2; exit 2 ;;
esac

mkdir -p "$WORK"
TARBALL="$WORK/ffmpeg-${VERSION}.tar.xz"
if ! echo "$SHA256  $TARBALL" | sha256sum -c --quiet 2>/dev/null; then
  for url in "${URLS[@]}"; do
    echo "downloading $url"
    curl -fsSL --retry 3 -o "$TARBALL" "$url" && break || true
  done
  echo "$SHA256  $TARBALL" | sha256sum -c --quiet
fi

SRC="$WORK/src-$TARGET"
rm -rf "$SRC"
mkdir -p "$SRC"
tar -xf "$TARBALL" -C "$SRC" --strip-components=1
cd "$SRC"

flags=(
  --disable-everything --disable-autodetect --disable-doc --disable-debug
  --disable-ffplay --disable-ffprobe --disable-avdevice --disable-swresample
  --enable-static --disable-shared --enable-small
  --enable-ffmpeg --enable-avcodec --enable-avformat --enable-avfilter --enable-swscale
  --disable-network --enable-protocol=pipe
  --enable-demuxer=mpegts
  --enable-decoder=h264,hevc
  --enable-parser=h264,hevc
  --enable-filter=scale,pad,format,null,showinfo
  --enable-encoder=rawvideo --enable-muxer=rawvideo
)
if [ "$TARGET" = windows ]; then
  flags+=(
    --target-os=mingw32 --arch=x86_64 --cross-prefix=x86_64-w64-mingw32-
    --enable-w32threads --extra-ldflags="-static -static-libgcc"
  )
fi

./configure "${flags[@]}" > configure.log 2>&1 || { tail -30 configure.log; exit 1; }
PROG=ffmpeg
STRIP=strip
if [ "$TARGET" = windows ]; then
  PROG=ffmpeg.exe
  STRIP=x86_64-w64-mingw32-strip
fi
make -j"$(nproc)" "$PROG" > make.log 2>&1 || { tail -30 make.log; exit 1; }

mkdir -p "$(dirname "$OUT")"
"$STRIP" -o "$OUT" "$PROG"
ls -la "$OUT"
