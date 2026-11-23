# Tapas

Tapas is a Windows app for TP-Link **Tapo** cameras and the **H500 HomeBase** hub. It signs in
with your Tapo account and works through the Tapo cloud, like the Tapo app on your phone, so it
works from anywhere with internet: no VPN, port forwarding or camera passwords.

- **Live view** of every camera in a grid, or one camera at a time.
- **Pan & tilt**, presets, privacy mode, SD/HD, snapshots.
- **Playback** of the H500's recordings on a day timeline: jump to any moment, 1× to 16×,
  detected events with their pictures, a calendar, recording clips and saving them.
- **Full screen** for live view and playback.
- Your **homes and rooms** from the Tapo app, and the hub's firmware and storage.
- **Light or dark**, an **installer** and **automatic updates**.

Tapas is written in Rust with [Slint](https://slint.dev). It isn't made or supported by TP-Link.

## Installing

Run **`Tapas-Setup-<version>.exe`**. It installs Tapas for your Windows user, without
administrator rights, into `%LOCALAPPDATA%\Programs\Tapas`, with a Start menu shortcut, an
optional desktop shortcut and an uninstaller under *Settings › Apps*.

The installer isn't code-signed, so Windows may say *Windows protected your PC*; choose
*More info › Run anyway*. The portable zip has the same `Tapas.exe`, to run without installing.


Windows 10 or 11, 64-bit.

## Signing in

When it isn't signed in, Tapas opens on its welcome screen. **Log In** with your TP-Link ID: the
email and password you use in the Tapo app.

The first time this PC signs in, Tapo asks for a **verification code**, which Tapas has sent to
the Tapo app on your phone (*Receive Code via Email* sends it by email instead). The PC is
remembered after that.

With **Remember me**, the password is kept, encrypted for your Windows user, so Tapas can sign in
again by itself when the session runs out. Without it, the password is forgotten when Tapas
closes, and you sign in again when the session ends. **Sign out** is under *Settings › Tapo
account*.

The cameras and hubs on your account are added by themselves. ⟳ on Home looks again, for
cameras added in the Tapo app later.

## Using Tapas

### Home

Home is titled with your home's name from the Tapo app. The hub's card has its room, model and
firmware, and how full its storage is (a bar for the hard disk, and a line for its own storage);
**Playback** opens its recordings. Each camera's card has a recent picture, its room and model,
a button for its recordings and one to watch it. **Live view** shows every camera.

The pictures are kept from one run to the next, so Home has them at once. A camera's picture is
taken again when it's more than ten minutes old, two cameras at a time, and whenever you've been
watching it; ⟳ takes new ones of every camera.

### Live view and the camera view

**Live view** shows all cameras in a grid that fits the window. Until a camera's video comes, its
tile shows its last picture, dimmed. Click a camera for its own view, which starts with the grid's
picture while the HD video connects:

- **Take Photo** saves the picture to `Pictures\Tapas`.
- **Pan & Tilt** (cameras that have it): the pad or the arrow keys, and presets to go to or save.
  Each move goes through the cloud, so it takes a moment; presses made meanwhile add up to one
  move.
- **Privacy Mode** covers the lens, as in the Tapo app; the picture comes back when it's off.
- **SD / HD** picks the camera's stream. The grid uses SD and the camera view HD unless you
  change it.
- **Playback & Download** opens the camera's recordings on the hub.
- **Full screen**: its button, F11 or a double-click; Esc leaves it.

Video keeps coming for 20 seconds after you leave a page, so going back, e.g. from a camera to the
grid, is instant.

### Playback

**Playback** shows what the hub recorded. Pick the camera and a day; the calendar marks days with
recordings. The timeline under the picture shows the day: recordings in blue, detected events in
amber.

- **Click the timeline**, or an event in the list, to watch from there. Playback carries on through
  the day's recordings, skipping the gaps.
- **Drag the white playhead** to a moment: its label shows the time it will play from.
- **Drag** anywhere else on the timeline to move along it, and **scroll** (or the zoom buttons) to
  zoom between the whole day and five minutes. It follows the picture while playing.
- **1× 2× 4× 8× 16×** set the speed. From 8×, only keyframes are shown, like fast-forward.
- **Click the picture** (or ❚❚) to pause. **Full screen** puts the timeline and controls over the
  picture; the list button at the top hides the list instead.
- **Record**: press it, watch on (or jump ahead), press it again, and that stretch is saved to
  `Videos\Tapas`.
- **↓** next to an event or recording saves it to `Videos\Tapas`. **Take Photo** saves the
  picture.

The list has the day's **detected events** (person, vehicle, animal, motion…) by hour, with the
picture the hub keeps for each. The pictures come while you watch, whenever playback isn't using
the hub, and are kept on this PC so they come at once next time. On a day without events the list
has the recordings.

Recordings come from the hub through the cloud a couple of minutes at a time, just before they're
needed, and nothing is saved unless you ask. Leaving the page pauses what's playing; after ten
seconds paused, Tapas lets go of the hub (so the Tapo app can play) and picks up where it was when
you play on. What has arrived stays in memory for a while, so going
back, or forward into it, starts at once. A jump elsewhere takes a few seconds while the hub
answers. Days are in this PC's time zone. Saved recordings are `.ts` files, which VLC and Windows'
media players play.

### Settings

- **Tapo account**: who is signed in; update the list of cameras and hubs; sign out.
- **Cameras**: rename, reorder, remove or add back a camera, and choose its live view stream.
- **Appearance and video**: light, dark or as Windows; the graphics renderer; a different ffmpeg;
  where the settings file is. Changes apply as you make them.
- **About and updates**: the version, *Check for updates*, and whether to check at start-up.

## Updates

At start-up, and with *Check for updates*, Tapas looks for a newer release on GitHub. When there
is one, Home shows **Update now**: Tapas downloads the new installer, checks it against the
checksum GitHub publishes, closes, installs it and starts again. Settings and the sign-in stay.


## Where things are

| What | Where |
|---|---|
| The app | `%LOCALAPPDATA%\Programs\Tapas` |
| Settings and sign-in (encrypted) | `%APPDATA%\Tapas\config\config.json` |
| Log | `%APPDATA%\Tapas\config\tapas.log` |
| Snapshots | `Pictures\Tapas` |
| Saved recordings | `Videos\Tapas` |
| The built-in ffmpeg, unpacked | `%LOCALAPPDATA%\Tapas\data` |
| The cameras' last pictures, and event pictures | `%LOCALAPPDATA%\Tapas\data\pictures` |

Uninstalling keeps settings, snapshots and recordings. Event pictures are deleted after 45 days.

## Troubleshooting

- **Tapas doesn't start, or the window stays blank**: run `Tapas.exe --software`, then choose
  another renderer under *Settings › Appearance and video* (*Software* works everywhere, e.g. over
  remote desktop or on old graphics chips).
- **"Camera is offline"**: the cloud can't reach the camera; check it in the Tapo app.
- **The Tapo app says "No recordings", or Tapas's playback stops**: a hub serves only a few
  playback sessions at once. Close Tapas, which ends its sessions; if the Tapo app still can't
  play, restart the hub.
- **Sign-in or video refused after a Tapo app update**: TP-Link may have changed its service.
  Raising `APP_VERSION` in `src/cloud.rs` to the current Tapo app's version is the first thing to
  try. The log has the details.
- **"Couldn't check for updates: No releases…"**: the releases repository is private or has no
  releases yet.
- **Live view is slow to start**: the log's `live:` lines say how long each camera took, and
  which part: the cloud's answer, the relay, or the first picture.
- Anything else: the log (see *Where things are*) records every step with the cloud and the hub;
  playback lines start with `clip:` and `playback:`.

## How it works

Tapas uses TP-Link's private cloud service, the one the Tapo app uses. It isn't documented and
TP-Link can change it.

- **Sign-in and devices**: the account API (signed requests, two-step verification) lists the
  cameras and hubs (`getDeviceListByPage`), and the IoT service lists homes and rooms.
- **Control**: camera and hub commands (pan/tilt, privacy, the hub's camera list, recording and
  event searches, disk status) go to the device through the cloud's pass-through, in the device's
  own API.
- **Video**: the cloud opens a relay to the device, and video arrives over it as MPEG-TS, decoded
  by a small built-in ffmpeg. Live view asks a camera; playback asks the hub, naming the camera,
  and then talks to the hub as it would on its local media port (request, acknowledgements,
  heartbeats, stop). Event pictures come the same way, as single keyframes.
- Each frame's recorded time comes from ffmpeg, so playback runs at the chosen speed, and the hub
  stamps every piece of video with its time, which lines up the timeline.

**Limitations**: no sound yet; cameras that send encrypted video through the cloud can't be shown.

**Privacy**: Tapas talks to TP-Link's cloud (sign-in, devices, video) and to GitHub (the update
check, which sends only the app's version). The password, when remembered, and the cloud session
are encrypted with Windows DPAPI, so only your Windows user can read them.

## Development

### Source

| Path | What |
|---|---|
| `src/main.rs` | Start-up, renderer choice, logging |
| `src/app.rs` | The app's state and everything between the UI and the work below |
| `ui/app.slint` | The whole UI |
| `src/cloud.rs` | The Tapo cloud: sign-in, devices, homes, pass-through requests, relays |
| `src/relay.rs` | The relay connection (a long HTTP request carrying multipart parts) |
| `src/stream.rs`, `src/ffmpeg.rs` | Live video: relay → ffmpeg → frames |
| `src/ptz.rs` | Pan/tilt, presets and privacy mode |
| `src/clip.rs` | Hub recordings and event pictures over the relay |
| `src/playback.rs` | Timeline playback: fetching ahead, the in-memory cache, pacing, speeds |
| `src/update.rs` | Updates from GitHub releases |
| `src/pictures.rs` | Cameras' last pictures and event pictures, kept on disk |
| `src/config.rs`, `src/secret.rs` | Settings, and DPAPI encryption of secrets |
| `installer/Tapas.nsi` | The installer (NSIS) |
| `tools/` | Build scripts, the icon, the fake cloud |
| `vendor/` | The built-in ffmpeg (see `vendor/README.md`) |
| `assets/`, `certs/`, `tests/fixtures/` | Icon, TP-Link's root certificates, test video |

### Building

On Windows, with Rust's MSVC toolchain:

```bash
cargo build --release
```

From Linux or WSL, cross-compiling:

```bash
rustup target add x86_64-pc-windows-gnu
```

```bash
sudo apt-get install mingw-w64
```

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

`build.rs` embeds `vendor/ffmpeg.exe` and the icon. `tools/build-ffmpeg.sh` rebuilds that ffmpeg
(MPEG-TS, H.264 and H.265 in, raw frames out, nothing else), and `tools/make-icon.py` redraws the
icon from the in-app logo.

### Installer and releases

```bash
tools/build-installer.sh
```

This builds `target/installer/Tapas-Setup-<version>.exe` (needs `makensis`), the portable zip and
their SHA-256 sums. The installer takes `/S` (silent) and `/RELAUNCH` (start Tapas afterwards),
which the updater uses.

To publish a release that installed copies update to, raise `version` in `Cargo.toml`, then:

```bash
tools/release.sh chaika1h/tapas "What's new"
```

### Testing without devices

`tools/fake_cloud.py` is a fake Tapo cloud: account, cameras with pan/tilt and privacy, an H500
with recordings, detected events and pictures (sending one session at a time, like a real one),
homes, rooms and an update. It needs Python 3, `openssl` and an ffmpeg with libx264 and drawtext.
Debug builds can be pointed at it:

```bash
python3 tools/fake_cloud.py --certs target/fake-cloud --mfa
```

```bash
TAPAS_FAKE_CLOUD=https://127.0.0.1:8443 TAPAS_FAKE_CLOUD_CA=target/fake-cloud/ca.pem TAPAS_UPDATE_URL=https://127.0.0.1:8443/github/releases/latest cargo run
```

Sign in with any email and the password `secret`; the verification code is `123456`. Its options:
`--expire-after N` (sessions expire), `--control-delay S` (slow pan/tilt), `--slow-clips`
(recordings in real time), `--clip-path video|playback|none` (which way the hub sends
recordings). The recorded time is written on the fake recordings' picture.

```bash
cargo test
```

## Credits and licences

Tapas is MIT-licensed (see `LICENSE`). Its cloud client is ported from the MIT-licensed
[Topo](https://github.com/filirnd/topo) and
[Topo TV](https://github.com/abdulmominsakib/tapo-android-tv), and the hub's recording protocol
from the MIT-licensed [tapo](https://github.com/mihai-dinculescu/tapo) crate.
`certs/tapo-cloud-roots.pem` holds TP-Link's two public root certificates, as those projects ship
them.

- [Slint](https://slint.dev) is used under its Royalty-free License, which asks for the
  attribution shown in *Settings › About and updates*.
- The built-in ffmpeg is LGPL 2.1 (see `vendor/README.md`).

Tapo is a trademark of TP-Link; Tapas isn't affiliated with TP-Link.
