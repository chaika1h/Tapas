# Tapo Viewer

A Windows desktop app for TP-Link **Tapo** cameras and the **H500 HomeBase**, written in Rust with
[Slint](https://slint.dev) (Fluent style) on top of the
[`tapo`](https://github.com/mihai-dinculescu/tapo) crate.

- **Live view from anywhere** through the Tapo cloud, like the Tapo app: no VPN, port forwarding
  or camera accounts.
- **Live grid** of all cameras, sized to fit the window; click a camera for a full view.
- **Fullscreen** (F11 / double-click, Esc to leave) for a single camera or the whole grid.
- **Pan/tilt** and **presets** from anywhere for cameras that have them (arrow keys work too);
  the app finds out which do.
- **H500 recordings**: browse each paired camera's clips by day, download and play them.
- **Snapshots** to `Pictures\TapoViewer`.
- **Sign-in like the Tapo app**: welcome screen, log in, and two-step verification with the code
  sent to your phone.
- The password (with *Remember me*) and the cloud session are encrypted with Windows DPAPI, so
  they only work for your Windows user.

## How it talks to the devices

Everything uses your **Tapo app login** (email and password).

| Feature | How | Where it works |
|---|---|---|
| Live video | Tapo cloud video relay (HTTPS) | Anywhere with internet |
| Pan/tilt, presets | Tapo cloud camera control (HTTPS) | Anywhere with internet |
| H500 recordings, Find hubs | Tapo local API | On the hub's network |

### Getting started

1. When you're not signed in, the app opens on its welcome screen. Press **Log In** and enter your
   TP-Link ID (the email and password you use in the Tapo app). TP-Link IDs are created in the
   Tapo app; *Create a TP-Link ID* and *Forgot Password?* explain where.
2. The first time this PC logs in, Tapo asks for a verification code. Like the Tapo app, Tapo
   Viewer has it sent straight to the Tapo app on your phone; *Receive Code via Email* sends it by
   email instead, and *Resend Code* becomes available after a minute. The PC is remembered after
   that.
3. The cameras on your account are added automatically and **Live view** opens. **Update camera
   list** (*Settings › Tapo account*) picks up cameras added later. To change which account
   camera an entry shows, edit it and pick its *Tapo camera*.

With **Remember me** ticked, the password is saved (encrypted) so the app can log in again by
itself when the session expires and can open the H500's recordings after a restart. Without it,
the password is only kept until the app closes. The session token is always saved, encrypted.
If the session expires and the app can't renew it, the log-in card comes back. **Sign out**
(*Settings › Tapo account*) forgets the session and the password and keeps the email.

The grid uses each camera's sub-stream (SD) and the single view its main stream (HD); you can
change either per camera.

In the single-camera view, the pan/tilt panel appears for cameras that have it. Each move goes
through the cloud, so it takes a moment; arrow presses made meanwhile are combined into one move.

H500 recordings are still read from the hub on the local network, because there is no known way
to reach them through the cloud. To browse them away from the office, put the PC on the office
network over a VPN (e.g. a [Tailscale subnet router](https://tailscale.com/kb/1019/subnets) on an
always-on machine there, or your router's built-in WireGuard/OpenVPN server).

### About the cloud

Live view and pan/tilt use TP-Link's private, undocumented cloud service, ported from the MIT-licensed
[Topo](https://github.com/filirnd/topo) and [Topo TV](https://github.com/abdulmominsakib/tapo-android-tv)
projects (see `src/cloud.rs` and `src/relay.rs`). TP-Link can change it at any time; if it
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
can ask for a verification code (`--mfa`, code `123456`), can expire sessions
(`--expire-after 60`) and simulates pan/tilt and presets, with the cloud's delay if you like
(`--control-delay 0.4`). It needs `openssl` and an ffmpeg with libx264. Only debug builds can be
pointed at it:

```bash
python3 tools/fake_cloud.py --certs target/fake-cloud --mfa
```

```bash
TAPO_VIEWER_FAKE_CLOUD=https://127.0.0.1:8443 TAPO_VIEWER_FAKE_CLOUD_CA=target/fake-cloud/ca.pem cargo run
```

Sign in with any email and the password `secret`. "Back yard" is reported offline; "Front door"
has pan/tilt and "Garage" hasn't.

## Troubleshooting

- **Hub login fails**: check the Tapo account and the hub's IP address under *Settings*, and that
  the PC is on the hub's network. Hubs on very new firmware may need a newer version of the
  `tapo` crate.
- **"Pan/tilt: Reached the end of the pan/tilt range"**: the camera is at its end stop in that
  direction.
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
