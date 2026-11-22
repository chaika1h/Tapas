# Tapo Viewer

A Windows desktop app for TP-Link **Tapo** cameras and the **H500 HomeBase**, written in Rust with
[Slint](https://slint.dev) (Fluent style) on top of the
[`tapo`](https://github.com/mihai-dinculescu/tapo) crate.

- **Live view from anywhere** through the Tapo cloud, like the Tapo app: no VPN, port forwarding
  or camera accounts.
- **Live grid** of all cameras, sized to fit the window; click a camera for a full view.
- **Fullscreen** (F11 / double-click, Esc to leave) for a single camera or the whole grid.
- **Pan/tilt** and **presets** for C2xx/C5xx/TC40/TC70 cameras (arrow keys work too), on the
  local network.
- **H500 recordings**: browse each paired camera's clips by day, download and play them.
- **Snapshots** to `Pictures\TapoViewer`.
- Passwords and the cloud session are encrypted with Windows DPAPI, so they only work for your
  Windows user.

## How it talks to the devices

Everything uses your **Tapo app login** (email and password).

| Feature | How | Where it works |
|---|---|---|
| Live video | Tapo cloud video relay (HTTPS) | Anywhere with internet |
| Pan/tilt, presets, H500 recordings, Find devices | Tapo local API | On the cameras' network |

### Getting started

1. Under *Settings › Tapo account*, enter your Tapo email and password and press **Save account**.
   This signs in. The first time, Tapo asks for a verification code, sent to the Tapo app or by
   email; the PC is remembered after that.
2. The cameras on your account are added automatically. **Update camera list** picks up cameras
   added later. To change which account camera an entry shows, edit it and pick its *Tapo camera*.
3. Open **Live view**.

The grid uses each camera's sub-stream (SD) and the single view its main stream (HD); you can
change either per camera. The PC keeps the session token, encrypted like the password. If the
session expires, the app signs in again by itself; if Tapo then asks for a code, sign in
again under *Settings*.

Pan/tilt still uses the camera's local API, so it needs the camera's IP address (edit the camera,
or use *Find devices*) and only works when the PC is on the camera's network. The same goes for
H500 recordings. To use those away from the office, put the PC on the office network over a VPN
(e.g. a [Tailscale subnet router](https://tailscale.com/kb/1019/subnets) on an always-on machine
there, or your router's built-in WireGuard/OpenVPN server).

### About the cloud

Live view uses TP-Link's private, undocumented cloud service, ported from the MIT-licensed
[Topo](https://github.com/filirnd/topo) and [Topo TV](https://github.com/abdulmominsakib/tapo-android-tv)
projects (see `src/cloud.rs` and `src/relay.rs`). TP-Link can change it at any time; if live view
stops working after a Tapo app update, the protocol has probably changed (see *Troubleshooting*).
Cameras that send encrypted video through the cloud aren't supported yet; their tile says so.

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

- **Pan/tilt or hub login fails**: check the Tapo account and the camera's IP address under
  *Settings*, and that the PC is on the camera's network. Cameras on very new
  firmware may need a newer version of the `tapo` crate.
- **"Camera is offline"**: the cloud can't reach the camera; check it in the
  Tapo app.
- **"The cloud relay refused the stream (code …)"** or **sign-in refused**:
  TP-Link may have changed its service. Raising `APP_VERSION` in `src/cloud.rs` to the current
  Tapo app version is the first thing to try. The log (see *Files*) has the details.

## License notes

Slint is used under its Royalty-free License, which requires showing attribution (the "Made with
Slint" badge, or a mention in the About information). The Tapo cloud client (`src/cloud.rs`, `src/relay.rs`) is a port
of the MIT-licensed [Topo](https://github.com/filirnd/topo) and
[Topo TV](https://github.com/abdulmominsakib/tapo-android-tv) projects. `certs/tapo-cloud-roots.pem`
holds TP-Link's two public root certificates, as shipped by those projects.
