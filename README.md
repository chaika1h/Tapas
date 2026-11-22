# Tapo Viewer

A Windows desktop app for TP-Link **Tapo** cameras and the **H500 HomeBase**, written in Rust with
[Slint](https://slint.dev) (Fluent style) on top of the
[`tapo`](https://github.com/mihai-dinculescu/tapo) crate.

- **Live grid** of all cameras, sized to fit the window; click a camera for a full view.
- **Fullscreen** (F11 / double-click, Esc to leave) for a single camera or the whole grid.
- **Pan/tilt** and **presets** for C2xx/C5xx/TC40/TC70 cameras (arrow keys work too).
- **H500 recordings**: browse each paired camera's clips by day, download and play them.
- **Find devices** on the network and add them in one click.
- **Live view from anywhere** through the Tapo cloud, like the Tapo app (optional, see below).
- **Snapshots** to `Pictures\TapoViewer`.
- Passwords are encrypted with Windows DPAPI, so they only work for your Windows user.

## How it talks to the devices

By default everything stays on the local network and nothing goes through TP-Link's cloud
servers. Only the optional cloud live view uses the cloud.

| Feature | Protocol | Credentials |
|---|---|---|
| Live video | RTSP from each camera, port 554 | **Camera account** (per camera) |
| Live video from anywhere (optional) | Tapo cloud video relay (HTTPS) | **Tapo app login** |
| Pan/tilt, presets, discovery, H500 recordings | Tapo local API | **Tapo app login** (email + password) |

The **camera account** is created in the Tapo app on your phone: open the camera, then
*Camera Settings › Advanced Settings › Camera Account*. Set one on each camera.

The cameras paired with the H500 are ordinary Wi-Fi cameras with their own IP addresses, so you
add them as cameras for live view. The hub itself is only used for the recordings it stores.

### Access from outside the office

There are two ways to watch the cameras when the PC isn't on their network.

**Live view through the Tapo cloud (built in).** The video comes through TP-Link's cloud relay,
the way the Tapo app shows it away from home, so it needs no VPN, port forwarding or office PC.
Under *Settings › Live view through the Tapo cloud*:

1. Save your Tapo account under *Tapo account*, then press **Sign in**. The first time, Tapo asks
   for a verification code, sent to the Tapo app or by email. The PC is remembered after that.
2. The cameras on the account are linked to the ones you already have (by name) and the rest are
   added. To link a camera by hand, edit it and pick its *Tapo cloud camera*.
3. Set *Live view* to **Tapo cloud** on PCs away from the office. Leave it on **Local network** on
   PCs in the office. Cameras without an IP address always use the cloud.

The grid uses the cameras' sub-stream and the single view their HD stream. Only live video goes
through the cloud: pan/tilt and H500 recordings still need the local network (or the VPN below).
The PC saves the session token, encrypted like the passwords; the Tapo password isn't sent
anywhere else. If the session expires, the app signs in again by itself. If Tapo then asks for a
code, sign in again under *Settings*.

This uses TP-Link's private, undocumented service, ported from the MIT-licensed
[Topo](https://github.com/filirnd/topo) and [Topo TV](https://github.com/abdulmominsakib/tapo-android-tv)
projects (see `src/cloud.rs`). TP-Link can change it at any time. If cloud live view stops working
after a Tapo app update, the protocol has probably changed. Cameras that send encrypted video
through the cloud aren't supported yet; their tile says so.

**A VPN to the office network.** Live view, pan/tilt and H500 recordings then work exactly as in
the office. Put the PC on the office network over a VPN:

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

`tools/fake_cloud.py` is a fake Tapo cloud and video relay for testing cloud live view without
an account. It checks request signatures like the real cloud, redirects to a "regional" server,
can ask for a verification code (`--mfa`, code `123456`) and can expire sessions
(`--expire-after 60`). It needs `openssl` and an ffmpeg with libx264. Only debug builds can be
pointed at it:

```bash
python3 tools/fake_cloud.py --certs target/fake-cloud --mfa
```

```bash
TAPO_VIEWER_FAKE_CLOUD=https://127.0.0.1:8443 TAPO_VIEWER_FAKE_CLOUD_CA=target/fake-cloud/ca.pem cargo run
```

Sign in with any email and the password `secret`. "Back yard" is reported offline.

## Troubleshooting

- **"Camera account username/password rejected"**: re-check the camera account in the Tapo app.
  It is not your TP-Link login.
- **"Camera did not answer"**: check the IP address. RTSP (port 554) must be reachable from
  the PC, so watch for guest-network isolation or VLANs.
- **"Camera has no built-in MJPEG stream"**: switch that camera's grid stream to SD (ffmpeg).
- **Pan/tilt or hub login fails**: check the Tapo account under *Settings*. Cameras on very new
  firmware may need a newer version of the `tapo` crate.
- **Cloud live view: "Camera is offline"**: the cloud can't reach the camera; check it in the
  Tapo app.
- **Cloud live view: "The cloud relay refused the stream (code …)"** or **sign-in refused**:
  TP-Link may have changed its service. Raising `APP_VERSION` in `src/cloud.rs` to the current
  Tapo app version is the first thing to try. The log (see *Files*) has the details.

## License notes

Slint is used under its Royalty-free License, which requires showing attribution (the "Made with
Slint" badge, or a mention in the About information). The MJPEG depacketizing approach comes from
the MIT-licensed `tapo` crate. The Tapo cloud client (`src/cloud.rs`, `src/relay.rs`) is a port
of the MIT-licensed [Topo](https://github.com/filirnd/topo) and
[Topo TV](https://github.com/abdulmominsakib/tapo-android-tv) projects. `certs/tapo-cloud-roots.pem`
holds TP-Link's two public root certificates, as shipped by those projects.
