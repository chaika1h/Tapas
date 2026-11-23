# Bundled ffmpeg

`ffmpeg.exe` is a minimal build of [FFmpeg](https://ffmpeg.org) 7.1.1 for Windows x64. `build.rs`
embeds it into `Tapas.exe`, and the app unpacks it to `%LOCALAPPDATA%\Tapas\data` on first start.
It decodes the live video, the hub's recordings and the event pictures that come through the Tapo
cloud.

- **Licence:** LGPL version 2.1 or later, with no GPL or non-free parts enabled. See
  `ffmpeg-LICENSE-LGPLv2.1.txt`.
- **Source:** the unmodified upstream release `ffmpeg-7.1.1.tar.xz`, SHA-256
  `733984395e0dbbe5c046abda2dc49a5544e7e0e1e2366bba849222ae9e3a03b1`, from
  <https://ffmpeg.org/releases/>.
- **Build:** `tools/build-ffmpeg.sh windows`, cross-compiled with mingw-w64. The script lists the
  exact `configure` options. Only these are enabled:
  - the pipe protocol: video is read from stdin; networking is disabled;
  - the MPEG-TS, H.264 and HEVC demuxers, parsers and decoders;
  - the scale, pad, format, null and showinfo filters (showinfo gives each frame's time, for
    playback at a chosen speed);
  - the rawvideo encoder and muxer, for frames out.

To use a different ffmpeg, set its path in the app under *Settings › Video & display*.
