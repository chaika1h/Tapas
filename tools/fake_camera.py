#!/usr/bin/env python3
"""Fake Tapo camera for testing Tapo Viewer without real hardware.

Serves the same RTSP paths as a Tapo camera, protected by a camera account:
  rtsp://viewer:secret@HOST:PORT/stream1   H.264 1920x1080 (HD)
  rtsp://viewer:secret@HOST:PORT/stream2   H.264 640x360   (SD)
  rtsp://viewer:secret@HOST:PORT/stream8   MJPEG 640x360   (standard RFC 2435)

Note: real Tapo cameras send their MJPEG stream in a non-standard way that
Tapo Viewer handles separately; this server exercises the standard fallback.

Needs GStreamer's RTSP server and Python bindings, e.g. on Ubuntu:
  apt-get install gir1.2-gst-rtsp-server-1.0 gstreamer1.0-plugins-good \
      gstreamer1.0-plugins-ugly python3-gi

Usage: fake_camera.py [--port 8554] [--pattern ball] [--user viewer] [--password secret]
"""

import argparse

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstRtspServer", "1.0")
from gi.repository import GLib, Gst, GstRtspServer  # noqa: E402


def source(pattern: str, w: int, h: int) -> str:
    return (
        f"videotestsrc is-live=true pattern={pattern} ! "
        f"video/x-raw,width={w},height={h},framerate=15/1 ! videoconvert ! video/x-raw,format=I420"
    )


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", default="8554")
    ap.add_argument("--pattern", default="ball", help="videotestsrc pattern (ball, smpte, snow, ...)")
    ap.add_argument("--user", default="viewer")
    ap.add_argument("--password", default="secret")
    args = ap.parse_args()

    Gst.init(None)
    server = GstRtspServer.RTSPServer()
    server.set_service(args.port)

    x264 = "x264enc tune=zerolatency speed-preset=ultrafast key-int-max=15 ! rtph264pay name=pay0 pt=96"
    streams = {
        "/stream1": f"( {source(args.pattern, 1920, 1080)} ! {x264} )",
        "/stream2": f"( {source(args.pattern, 640, 360)} ! {x264} )",
        "/stream8": f"( {source(args.pattern, 640, 360)} ! jpegenc quality=80 ! rtpjpegpay name=pay0 pt=26 )",
    }

    auth = GstRtspServer.RTSPAuth()
    token = GstRtspServer.RTSPToken()
    token.set_string(GstRtspServer.RTSP_TOKEN_MEDIA_FACTORY_ROLE, "user")
    auth.add_basic(GstRtspServer.RTSPAuth.make_basic(args.user, args.password), token)
    server.set_auth(auth)

    mounts = server.get_mount_points()
    for path, launch in streams.items():
        factory = GstRtspServer.RTSPMediaFactory()
        factory.set_launch(launch)
        factory.set_shared(True)
        permissions = GstRtspServer.RTSPPermissions()
        permissions.add_permission_for_role("user", GstRtspServer.RTSP_PERM_MEDIA_FACTORY_ACCESS, True)
        permissions.add_permission_for_role("user", GstRtspServer.RTSP_PERM_MEDIA_FACTORY_CONSTRUCT, True)
        factory.set_permissions(permissions)
        mounts.add_factory(path, factory)

    server.attach(None)
    print(f"fake camera on rtsp://{args.user}:{args.password}@0.0.0.0:{args.port}/stream1|2|8", flush=True)
    GLib.MainLoop().run()


if __name__ == "__main__":
    main()
