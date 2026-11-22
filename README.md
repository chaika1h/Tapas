# Tapo Viewer

A Windows desktop app for TP-Link **Tapo** cameras and the **H500 HomeBase**, written in Rust with
[Slint](https://slint.dev) (Fluent style). Everything goes through the Tapo cloud with your Tapo
app login, like the Tapo app.

- **Live view from anywhere** through the Tapo cloud, like the Tapo app: no VPN, port forwarding
  or camera accounts.
- **Home**: a card per camera with a recent picture, and the H500 with its recordings.
- **Live view**: all cameras in a grid sized to fit the window, one click from anywhere.
- **Camera view**: the video with Take Photo, Pan & Tilt (pad and presets; arrow keys work too),
  Privacy Mode, SD/HD, Fullscreen, and Playback & Download for that camera's recordings on the hub.
- **Fullscreen** (F11 / double-click, Esc to leave) for a single camera or the whole grid.
- **Playback from anywhere**: the H500's recordings of each camera, by day, played in the app
  through the cloud and saved to `Videos\TapoViewer` as they play.
- **Snapshots** to `Pictures\TapoViewer`.
- **Light or dark**, or the same as Windows (*Settings › Video & display*).
- **Sign-in like the Tapo app**: welcome screen, log in, and two-step verification with the code
  sent to your phone.
- The password (with *Remember me*) and the cloud session are encrypted with Windows DPAPI, so
  they only work for your Windows user.

## How it talks to the devices

Everything uses your **Tapo app login** (email and password).

| Feature | How | Where it works |
|---|---|---|
| Live video | Tapo cloud video relay (HTTPS) | Anywhere with internet |
| Pan/tilt, presets, privacy | Tapo cloud camera control (HTTPS) | Anywhere with internet |
| H500 recordings: list | Tapo cloud, passed through to the hub (HTTPS) | Anywhere with internet |
| H500 recordings: play, save | Tapo cloud video relay, from the hub (HTTPS) | Anywhere with internet |

### Getting started

1. When you're not signed in, the app opens on its welcome screen. Press **Log In** and enter your
   TP-Link ID (the email and password you use in the Tapo app). TP-Link IDs are created in the
   Tapo app; *Create a TP-Link ID* and *Forgot Password?* explain where.
2. The first time this PC logs in, Tapo asks for a verification code. Like the Tapo app, Tapo
   Viewer has it sent straight to the Tapo app on your phone; *Receive Code via Email* sends it by
   email instead, and *Resend Code* becomes available after a minute. The PC is remembered after
   that.
3. The cameras and the H500 on your account are added automatically and **Home** opens. **Update camera
   list** (⟳ on Home, or *Settings › Tapo account*) picks up cameras added later. To change which account
   camera an entry shows, edit it and pick its *Tapo camera*.

With **Remember me** ticked, the password is saved (encrypted) so the app can log in again by
itself when the session expires. Without it,
the password is only kept until the app closes. The session token is always saved, encrypted.
If the session expires and the app can't renew it, the log-in card comes back. **Sign out**
(*Settings › Tapo account*) forgets the session and the password and keeps the email.

The grid uses each camera's sub-stream (SD) and the single view its main stream (HD); you can
change either per camera.

Home's cards get their pictures from one frame of each camera when Home first opens, and are
refreshed whenever you've been watching a camera. **Live view** (in the sidebar or on Home) shows
all cameras at once; click one for its camera view.

In the camera view, **Pan & Tilt** works for cameras that have it. Each move goes through the
cloud, so it takes a moment; arrow presses made meanwhile are combined into one move. **Privacy
Mode** covers the camera's lens (the Tapo app's privacy mode); the video comes back by itself
when it's turned off. **Playback & Download** opens that camera's recordings on the H500.

### Playback

**Playback** (in the sidebar, on the H500's Home card, or *Playback & Download* in a camera view)
shows the recordings the H500 holds. Pick the camera, then the day: the week strip marks the days
with recordings, and the arrows go a day back or forward. Click a recording to play it in the app.
It comes from the hub through the cloud, the way the Tapo app plays recordings away from home,
and is saved to `Videos\TapoViewer` while it plays, so playing it again (or *Open file*) uses the
saved copy. The ↓ button saves a recording without playing it.

Days follow this PC's time zone. Downloading usually runs faster than playback; the picture is
paced to real time. There's no seeking yet: to jump around in a recording, open the saved file.

### About the cloud

Live view and pan/tilt use TP-Link's private, undocumented cloud service, ported from the MIT-licensed
[Topo](https://github.com/filirnd/topo) and [Topo TV](https://github.com/abdulmominsakib/tapo-android-tv)
projects (see `src/cloud.rs` and `src/relay.rs`). TP-Link can change it at any time; if it
stops working after a Tapo app update, the protocol has probably changed (see *Troubleshooting*).
Cameras that send encrypted video through the cloud aren't supported yet; their tile says so.

Playback asks the cloud for a relay to the hub naming the camera, as the Tapo app does, and then
has the conversation the hub has on its local media port (request, acknowledgements every 25
parts, heartbeats, stop), ported from the MIT-licensed
[`tapo`](https://github.com/mihai-dinculescu/tapo) crate (see `src/clip.rs`). It has been tested
against `tools/fake_cloud.py`, not yet against a real H500; the log has the details of every
step if it doesn't work.

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
(`--expire-after 60`) and simulates pan/tilt, presets and privacy mode, with the cloud's delay if
you like (`--control-delay 0.4`). It needs `openssl` and an ffmpeg with libx264. Only debug builds can be
pointed at it:

```bash
python3 tools/fake_cloud.py --certs target/fake-cloud --mfa
```

```bash
TAPO_VIEWER_FAKE_CLOUD=https://127.0.0.1:8443 TAPO_VIEWER_FAKE_CLOUD_CA=target/fake-cloud/ca.pem cargo run
```

Sign in with any email and the password `secret`. "Back yard" is reported offline; "Front door"
has pan/tilt and "Garage" hasn't. The H500 "HomeBase" has recordings of "Front door" (today,
yesterday and three days ago) and "Garage" (today).

## Troubleshooting

- **No hub on Playback**: hubs come from the Tapo account's device list; press ⟳ on Home after
  adding one in the Tapo app. Only camera hubs (H500, H200) are shown.
- **A recording won't play** ("The cloud relay refused the stream", "The hub refused the
  recording", "encrypted"): the log has the hub's answers (lines starting `clip:`). Please send
  them, as the cloud's playback path hasn't been checked against a real H500 yet.
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
