# Tapo Viewer

A Windows desktop app for TP-Link **Tapo** cameras and the **H500 HomeBase**, written in Rust with
[Slint](https://slint.dev) (Fluent style) on top of the
[`tapo`](https://github.com/mihai-dinculescu/tapo) crate.

- **Live grid** of all cameras, sized to fit the window; click a camera for a full view.
- **Fullscreen** (F11 / double-click, Esc to leave) for a single camera or the whole grid.
- **Pan/tilt** and **presets** for C2xx/C5xx/TC40/TC70 cameras (arrow keys work too).
- **H500 recordings**: browse each paired camera's clips by day, download and play them.
- **Find devices** on the network and add them in one click.
- **Snapshots** to `Pictures\TapoViewer`.
- Passwords are encrypted with Windows DPAPI, so they only work for your Windows user.

## How it talks to the devices

Everything stays on the local network; nothing goes through TP-Link's cloud servers.

| Feature | Protocol | Credentials |
|---|---|---|
| Live video | RTSP from each camera, port 554 | **Camera account** (per camera) |
| Pan/tilt, presets, discovery, H500 recordings | Tapo local API | **Tapo app login** (email + password) |

The **camera account** is created in the Tapo app on your phone: open the camera, then
*Camera Settings › Advanced Settings › Camera Account*. Set one on each camera.

The cameras paired with the H500 are ordinary Wi-Fi cameras with their own IP addresses, so you
add them as cameras for live view. The hub itself is only used for the recordings it stores.

### Access from outside the office

The app talks to the devices directly, so it needs to be on the same network as them. TP-Link
has no public cloud API for video, and the Tapo app's remote viewing uses a private relay protocol.
To use Tapo Viewer from home or on the road, put the PC on the office network over a VPN:

- **Tailscale** (simplest): install it on one always-on machine at the office (a mini PC,
  Raspberry Pi or NAS) and enable it as a
  [subnet router](https://tailscale.com/kb/1019/subnets) for the office LAN. Then install
  Tailscale on each PC that runs Tapo Viewer.
- **WireGuard or OpenVPN**: many routers, including TP-Link's, have a VPN server built in.

Live view, pan/tilt and H500 recordings then work exactly as they do in the office. Don't
port-forward RTSP (554) to the internet: the video isn't encrypted and the cameras would be
exposed. Over a slow connection, use the *Fast* or *SD* stream.

### Stream options

| Option | Camera stream | Decoder | Notes |
|---|---|---|---|
| Fast (built-in) | `stream8`, MJPEG, ~640×360 | built in | Default for the grid. Low CPU, nothing to install. |
| SD (ffmpeg) | `stream2`, H.264/H.265 | ffmpeg | Use this if a camera has no MJPEG stream. |
| HD (ffmpeg) | `stream1`, H.264/H.265, full resolution | ffmpeg | Default for the single-camera view when ffmpeg is available. |

SD/HD use a minimal ffmpeg that is **built into `TapoViewer.exe`**, so there is nothing else to
install. It is unpacked to `%LOCALAPPDATA%\TapoViewer` on first start. To use your own ffmpeg
instead, set its path in *Settings › Video & display*.

## Rendering on Windows

The default renderer is **FemtoVG on wgpu with Direct3D 12**. It avoids OpenGL, whose fullscreen
switch makes some Windows drivers blank the screen for a moment. You can change the renderer under
*Settings › Video & display* (it applies after a restart):

- **Direct3D 12**: the default.
- **Vulkan**: an alternative GPU path.
- **Software**: works everywhere (remote desktop, VMs, very old GPUs).
- **OpenGL**: only for comparison.

If the app won't start, run `TapoViewer.exe --software`, or set `SLINT_BACKEND=winit-software`.

## Files

| What | Where |
|---|---|
| Settings | `%APPDATA%\TapoViewer\config\config.json` |
| Log | `%APPDATA%\TapoViewer\config\tapo-viewer.log` |
| Snapshots | `Pictures\TapoViewer` |
| Recordings | `Videos\TapoViewer` (`.ts` files; VLC or Windows Media Player play them) |
| Built-in ffmpeg (unpacked) | `%LOCALAPPDATA%\TapoViewer\data` |

## Building

The minimal ffmpeg that gets embedded lives in `vendor/ffmpeg.exe` (see `vendor/README.md`).
`build.rs` embeds it automatically; without it, the app looks for ffmpeg next to the exe and on
PATH instead.

On Windows (MSVC toolchain):

```bash
cargo build --release
```

The exe is `target\release\tapo-viewer.exe`.

Cross-compiling from Linux:

```bash
rustup target add x86_64-pc-windows-gnu
```

```bash
sudo apt-get install mingw-w64
```

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

The exe is `target/x86_64-pc-windows-gnu/release/tapo-viewer.exe`.

To rebuild the embedded ffmpeg (on Linux or WSL, with mingw-w64 installed):

```bash
tools/build-ffmpeg.sh windows
```

### Testing without cameras

`tools/fake_camera.py` serves Tapo-style RTSP streams (`stream1`, `stream2`, `stream8`) with a
camera account (`viewer` / `secret`), using GStreamer:

```bash
python3 tools/fake_camera.py --port 8554
```

Add a camera with IP `127.0.0.1`, port `8554`, user `viewer` and password `secret`.

## Troubleshooting

- **"Camera account username/password rejected"**: re-check the camera account in the Tapo app.
  It is not your TP-Link login.
- **"Camera did not answer"**: check the IP address. RTSP (port 554) must be reachable from
  the PC, so watch for guest-network isolation or VLANs.
- **"Camera has no built-in MJPEG stream"**: switch that camera's grid stream to SD (ffmpeg).
- **Pan/tilt or hub login fails**: check the Tapo account under *Settings*. Cameras on very new
  firmware may need a newer version of the `tapo` crate.

## License notes

Slint is used under its Royalty-free License, which requires showing attribution (the "Made with
Slint" badge, or a mention in the About information). The MJPEG depacketizing approach comes from
the MIT-licensed `tapo` crate.
