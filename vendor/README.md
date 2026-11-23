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
  - the pipe and file protocols: video is read from stdin or a temporary recording file; networking
    is disabled;
  - the MPEG-TS, H.264, HEVC and PCM (`pcm_s16le`) demuxers, parsers and decoders (plus AAC decoding
    and `aac_adtstoasc` bitstream filtering);
  - the scale, pad, format, null, showinfo, aresample, aformat and anull filters (`showinfo` gives
    each frame's time, for playback at a chosen speed);
  - the rawvideo encoder/muxer (for live frames out) and the LGPL AAC encoder + MP4 muxer (for
    lossless-video `.mp4` clip exports with `+faststart`).

To use a different ffmpeg, set its path in the app under *Settings › Appearance and video*.
