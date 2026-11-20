# Bundled ffmpeg

`ffmpeg.exe` is a minimal build of [FFmpeg](https://ffmpeg.org) 7.1.1 for Windows x64. `build.rs`
embeds it into `TapoViewer.exe`, and the app unpacks it to `%LOCALAPPDATA%\TapoViewer\data` on
first start. It is used only for the SD/HD camera streams.

- **License:** LGPL version 2.1 or later (no GPL or non-free parts enabled). See
  `ffmpeg-LICENSE-LGPLv2.1.txt`.
- **Source:** the unmodified upstream release `ffmpeg-7.1.1.tar.xz`, SHA-256
  `733984395e0dbbe5c046abda2dc49a5544e7e0e1e2366bba849222ae9e3a03b1`, from
  <https://ffmpeg.org/releases/>.
- **Build:** `tools/build-ffmpeg.sh windows` (cross-compiled with mingw-w64). The script lists
  the exact `configure` options. Only these are enabled: the RTSP/RTP/SDP demuxers; the TCP,
  UDP and RTP protocols; the H.264, HEVC and MJPEG decoders and parsers; the scale, pad and
  format filters; and the rawvideo encoder and muxer.

To use a different ffmpeg, set its path in the app under *Settings › Video & display*.
