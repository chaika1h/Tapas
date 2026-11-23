# Tapo Viewer

A Windows desktop app for TP-Link **Tapo** cameras and the **H500 HomeBase**, written in Rust with
[Slint](https://slint.dev) (Fluent style). Everything goes through the Tapo cloud with your Tapo
app login, like the Tapo app.

- **Live view from anywhere** through the Tapo cloud, like the Tapo app: no VPN, port forwarding
  or camera accounts.
- **Home**: a card per camera with a recent picture and its room, and the H500 with its
  firmware, storage and recordings; the page is titled with your home's name from the Tapo app.
- **Live view**: all cameras in a grid sized to fit the window, one click from anywhere.
- **Camera view**: the video with Take Photo, Pan & Tilt (pad and presets; arrow keys work too),
  Privacy Mode, SD/HD, Fullscreen, and Playback & Download for that camera's recordings on the hub.
- **Fullscreen** (F11 / double-click, Esc to leave) for a single camera or the whole grid.
- **Playback from anywhere**: the H500's recordings of each camera along a day timeline, like the
  Tapo app: click or drag to any moment, 1× to 16×, pause, full screen, detected events with
  their pictures, a month calendar, Record and Take photo. Recordings stream through the cloud;
  ↓ saves one to `Videos\TapoViewer`.
- **Snapshots** to `Pictures\TapoViewer`.
- **Light or dark**, or the same as Windows (*Settings › Video & display*), always in the app's
  own blue rather than the Windows accent colour.
- **Sign-in like the Tapo app**: welcome screen, log in, and two-step verification with the code
  sent to your phone.
- The password (with *Remember me*) and the cloud session are encrypted with Windows DPAPI, so
  they only work for your Windows user.

## Installing and updating

Run **`TapoViewer-Setup-<version>.exe`**. It installs Tapo Viewer for your Windows user (no
administrator rights) into `%LOCALAPPDATA%\Programs\TapoViewer`, with a Start menu shortcut, an
optional desktop shortcut and an uninstaller in *Settings › Apps*. The installer isn't code-signed,
so Windows SmartScreen may say "Windows protected your PC": choose *More info › Run anyway*.
The portable zip has the same `TapoViewer.exe` to run from anywhere instead.

**Updates**: at start-up (and with *Check for updates* in *Settings › About & updates*) the app
looks at the newest release on GitHub. When there's a newer version, Home shows **Update now**:
it downloads the new installer, checks it against the SHA-256 GitHub publishes, closes the app,
installs the update silently and starts the new version. Settings, the sign-in and saved
recordings are kept, also when uninstalling. *Check for updates when the app starts* turns the
automatic check off.

Releases come from `github.com/chaika1h/tapas/releases` (`update_repo` in the settings file
chooses another repository). The app has no GitHub sign-in, so **the releases must be public**:
make the repository public, or publish releases to a separate public repository and set
`update_repo` to it.

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
shows what the H500 recorded. Pick the camera and a day in the calendar (days with recordings have
a dot). The timeline under the video shows the day: recordings in blue, detected events in amber.

- **Click the timeline** (or an event or recording in the list) to watch from there. Playback runs
  on through the day's recordings, skipping the gaps between them.
- **Drag** the timeline to move along it, **scroll** (or ＋/－) to zoom from the whole day down to
  five minutes. While playing, it follows the picture.
- **1× 2× 4× 8× 16×** change the speed; from 8× only keyframes are shown, about one every
  second or two of recording. Click the picture (or ❚❚) to pause.
- **Full screen** (its button, F11 or double-click the picture; Esc leaves it) puts the timeline
  and controls over the picture. The list button by ⟳ hides the list for a bigger picture.
- **Record** marks where the picture is; press it again to save from there to where the picture
  is then (watching on, or jumping ahead) to `Videos\TapoViewer`, from memory where it can.
- **Take photo** saves the picture shown to `Pictures\TapoViewer`; **↓** on an event or recording
  saves it to `Videos\TapoViewer`.

The list shows the day's **detected events** (person, vehicle, animal, motion…) under each hour,
with the picture the hub keeps for each, as in the Tapo app; days without any list the
recordings. Pictures are fetched one at a time while nothing plays, as the hub serves one
session at a time.

Watching keeps nothing on disk: the recording comes from the hub through the cloud a couple of
minutes at a time, shortly before it's needed, and what has arrived stays in memory for a while
(up to about 200 MB), so going back, or forward into what has already arrived, starts at once.
The hub serves one download at a time, so jumps are queued behind the one finishing. Days follow
this PC's time zone.

### About the cloud

Live view and pan/tilt use TP-Link's private, undocumented cloud service, ported from the MIT-licensed
[Topo](https://github.com/filirnd/topo) and [Topo TV](https://github.com/abdulmominsakib/tapo-android-tv)
projects (see `src/cloud.rs` and `src/relay.rs`). TP-Link can change it at any time; if it
stops working after a Tapo app update, the protocol has probably changed (see *Troubleshooting*).
Cameras that send encrypted video through the cloud aren't supported yet; their tile says so.

The day's recordings, events and calendar come from the hub's own searches (`searchVideoWithUTC`,
`searchDetectionList` naming the camera by id and MAC, `searchDateWithVideo` a month at a time).
An event's picture is a single H.264/H.265 keyframe (`image/avc`, `image/hevc`) from a "cover"
download of media type 3, as the app's event list fetches them. Playback asks the cloud for a relay
to the hub naming the camera, as the Tapo app does, and then
has the conversation the hub has on its local media port (request, acknowledgements every 25
parts, heartbeats, stop), ported from the MIT-licensed
[`tapo`](https://github.com/mihai-dinculescu/tapo) crate (see `src/clip.rs`). The Tapo app has
several kinds of relay for recordings, and an H500 accepts its "cover" download but sends nothing,
so Tapo Viewer tries the "video" download, then the real-time playback stream, then "cover", and
uses the one that brought video first from then on. Every session is ended (stop, connection
closed) however the playback ends, as a hub serves only a few at a time. The log has the details
of every step. The picture is decoded by ffmpeg, whose `showinfo` filter gives each frame's time,
so frames are shown at their recorded time divided by the speed.

## Rendering on Windows

The default renderer is **FemtoVG on wgpu with Direct3D 12**. It avoids OpenGL, whose fullscreen
switch makes some Windows drivers blank the screen for a moment. You can change the renderer under
*Settings › Video & display* (it applies after a restart):

- **Direct3D 12**: the default.
- **Vulkan**: an alternative GPU path.
- **Software**: works everywhere (remote desktop, VMs, very old GPUs).
- **OpenGL**: only for comparison.

If the app won't start, run `TapoViewer.exe --software`, or set `SLINT_BACKEND=winit-software`.

### What else comes from the account

- **Names**: hubs' names come base64-encoded from the cloud (an H500 is "VGFwbyBINTAw", "Tapo
  H500") and are decoded.
- **Homes and rooms** (as set up in the Tapo app): the home's name titles Home, and each camera's
  room is on its card. They come from the cloud's IoT service (`/v1/families`, then each home's
  `/rooms`); the log has its answers.
- **The hub's firmware, address and storage**, asked of the hub itself through the cloud
  (`getDeviceInfo`, `getSdCardStatus`).

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
The icon (`assets/app.ico`, and `assets/app-icon.png` for the window) is the in-app logo, drawn by
`tools/make-icon.py`; `build.rs` embeds it in the exe through `assets/app.rc`.
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

### Installer and releases

`tools/build-installer.sh` builds `target/installer/TapoViewer-Setup-<version>.exe` (NSIS,
`installer/TapoViewer.nsi`; needs `makensis`), the portable zip and their SHA-256 sums.
`tools/release.sh [owner/repo] ["notes"]` builds them and publishes release `v<version>` with the
GitHub CLI; raise `version` in `Cargo.toml` first. The installer takes `/S` (silent) and
`/RELAUNCH` (start the app when done), which the app's updater uses.

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
yesterday and three days ago, including six minutes of continuous recording at 10:00 with
detected events) and "Garage" (today); the recorded time is written on the picture. Like a real hub it serves only two downloads
at a time, and `--slow-clips` sends recordings in real time, which makes a download that's left
open (and blocks the hub's playback, the Tapo app's too) easy to spot in its output. Like an H500
it sends nothing for the "cover" download; `--clip-path video|playback|none` picks which other way
works. Debug builds read updates from `TAPO_VIEWER_UPDATE_URL`, and the fake cloud serves a
release 9.9.9 at `<base>/github/releases/latest`. Like an H500 it serves one download at a time (the relay answers HTTP 500 to a second), and
the account has a home ("Office") with rooms, and the hub reports its firmware and storage.

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
