# Bundled ffmpeg

`ffmpeg.exe` is a minimal build of [FFmpeg](https://ffmpeg.org) 7.1.1 for Windows x64. `build.rs`
embeds it into `TapoViewer.exe`, and the app unpacks it to `%LOCALAPPDATA%\TapoViewer\data` on
first start. It decodes the live video and the hub recordings that come through the Tapo cloud.

- **License:** LGPL version 2.1 or later (no GPL or non-free parts enabled). See
  `ffmpeg-LICENSE-LGPLv2.1.txt`.
- **Source:** the unmodified upstream release `ffmpeg-7.1.1.tar.xz`, SHA-256
  `733984395e0dbbe5c046abda2dc49a5544e7e0e1e2366bba849222ae9e3a03b1`, from
  <https://ffmpeg.org/releases/>.
- **Build:** `tools/build-ffmpeg.sh windows` (cross-compiled with mingw-w64). The script lists
  the exact `configure` options. Only these are enabled: the MPEG-TS demuxer and the pipe
  protocol (the Tapo cloud relay's video, read from stdin); the H.264 and HEVC decoders and
  parsers; the scale, pad and format filters, and showinfo (each frame's time, for playback at
  a chosen speed); and the rawvideo encoder and muxer. Networking is
  disabled.

To use a different ffmpeg, set its path in the app under *Settings › Video & display*.
