#!/usr/bin/env python3
"""Fake Tapo cloud for testing Tapo Viewer's cloud live view without an account.

Serves over HTTPS on 127.0.0.1:
  * the account API: sign-in (with a regional redirect and, with --mfa, a
    verification code), the service lookup and the camera list;
  * the CIPC relay request;
  * the video relay itself, streaming live MPEG-TS made by ffmpeg; and
  * camera control (pan/tilt, presets and privacy mode) through the IoT gateway;
  * an H500 hub with recordings: its paired cameras, the days and clips it
    has, and clip downloads through the relay, with the hub's flow control.

Signed account calls are checked like the real cloud checks them (Content-MD5
and the HMAC signature), so a client signing wrongly fails here too.

Only debug builds of Tapo Viewer can be pointed at it:

  tools/fake_cloud.py --certs target/fake-cloud --mfa
  TAPO_VIEWER_FAKE_CLOUD=https://127.0.0.1:8443 \
  TAPO_VIEWER_FAKE_CLOUD_CA=target/fake-cloud/ca.pem cargo run

Sign in with any email and the password "secret"; the verification code is
123456. "Back yard" is reported offline; "Front door" has pan/tilt and
"Garage" hasn't. The H500 "HomeBase" has recordings of "Front door" (today,
yesterday and three days ago) and "Garage" (today). --expire-after N makes session tokens stop working after N
seconds, to test signing in again; --control-delay adds the cloud's latency to
pan/tilt calls.

Needs Python 3, the openssl command and an ffmpeg with libx264 on PATH.
"""

import argparse
import base64
import datetime
import hashlib
import hmac
import json
import subprocess
import ssl
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

# The Tapo Android app's API key pair (see src/cloud.rs).
ACCESS_KEY = "4d11b6b9d5ea4d19a829adbb9714b057"
ACCESS_SECRET = "6ed7d97f3e73467f8a5bab90b577ba4c"
PASSWORD = "secret"
MFA_CODE = "123456"

CAMERAS = [
    {"deviceId": "FAKE0001", "alias": "Front door", "deviceModel": "C200", "deviceMac": "A842A1000001", "status": 1},
    {"deviceId": "FAKE0002", "alias": "Garage", "deviceModel": "C310", "deviceMac": "A842A1000002", "status": 1},
    {"deviceId": "FAKE0003", "alias": "Back yard", "deviceModel": "C520WS", "deviceMac": "A842A1000003", "status": 0},
]
for _camera in CAMERAS:
    _camera["deviceType"] = "SMART.IPCAMERA"
OFFLINE = {"FAKE0003"}
PATTERNS = {"FAKE0001": "testsrc2", "FAKE0002": "testsrc"}
# Cameras with pan/tilt (the C310 has none), and how far they can turn.
PTZ = {"FAKE0001", "FAKE0003"}
PAN_RANGE, TILT_RANGE = 170, 35

HUBS = [
    {"deviceId": "FAKEHUB1", "alias": "HomeBase", "deviceModel": "H500", "deviceMac": "A842A1000100", "status": 1},
    # A smart-home hub: same device type, no video. Tapo Viewer leaves it out.
    {"deviceId": "FAKEHUB2", "alias": "Sensor hub", "deviceModel": "H100", "deviceMac": "A842A1000101", "status": 1},
]
for _hub in HUBS:
    _hub["deviceType"] = "SMART.TAPOHUB"
HUB_CAMERAS = [
    {"device_id": "FAKE0001", "alias": "Front door", "device_model": "C200", "mac": "A8-42-A1-00-00-01", "hub_storage_enabled": True},
    {"device_id": "FAKE0002", "alias": "Garage", "device_model": "C310", "mac": "A8-42-A1-00-00-02", "hub_storage_enabled": True},
    {"device_id": "FAKE0003", "alias": "Back yard", "device_model": "C520WS", "mac": "A8-42-A1-00-00-03", "hub_storage_enabled": False},
]
# Days ago with recordings, per camera, and the clips of such a day:
# (hour, minute, seconds long, video_type).
RECORDED_DAYS = {"FAKE0001": [0, 1, 3], "FAKE0002": [0]}
DAY_CLIPS = [(0, 5, 12, 2), (8, 2, 25, 6), (9, 41, 18, 2), (12, 15, 40, 8), (15, 33, 20, 6), (18, 7, 30, 9), (21, 48, 15, 2)]


def clips_for(camera, start, end):
    """The fake recordings of `camera` overlapping start..end (Unix seconds)."""
    today = datetime.date.today()
    clips = []
    for ago in RECORDED_DAYS.get(camera, []):
        day = today - datetime.timedelta(days=ago)
        for hour, minute, length, kind in DAY_CLIPS:
            t = int(datetime.datetime(day.year, day.month, day.day, hour, minute).timestamp())
            if t + length > start and t <= end and t + length < time.time():
                clips.append((t, t + length, kind))
    return sorted(clips)


class State:
    def __init__(self, args):
        self.args = args
        self.lock = threading.Lock()
        self.tokens = {}  # token -> time issued
        self.verified = set()  # terminal UUIDs that passed two-step verification
        self.pending_terminal = None
        self.relays = {}  # relay token -> (device id, resolution)
        self.position = {}  # device id -> [pan, tilt]
        self.presets = {}  # device id -> [(id, name, pan, tilt)]
        self.privacy = set()  # device ids with privacy mode on
        self.search_players = set()  # player ids clip searches used

    def new_token(self):
        token = "tok-" + uuid.uuid4().hex
        with self.lock:
            self.tokens[token] = time.monotonic()
        return token

    def valid(self, token):
        with self.lock:
            issued = self.tokens.get(token)
        if issued is None:
            return False
        expire = self.args.expire_after
        return not expire or time.monotonic() - issued < expire


def make_certs(directory: Path) -> None:
    """A test CA and a certificate for 127.0.0.1 signed by it."""
    directory.mkdir(parents=True, exist_ok=True)
    if (directory / "cert.pem").exists():
        return

    def run(*cmd):
        subprocess.run(cmd, check=True, cwd=directory, capture_output=True)

    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem",
        "-days", "30", "-subj", "/CN=Fake Tapo cloud test CA",
        "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "key.pem", "-out", "cert.csr",
        "-subj", "/CN=127.0.0.1")
    (directory / "ext.cnf").write_text(
        "subjectAltName=IP:127.0.0.1,DNS:localhost\nbasicConstraints=CA:FALSE\n"
        "keyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    run("openssl", "x509", "-req", "-in", "cert.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
        "-out", "cert.pem", "-days", "30", "-extfile", "ext.cnf")


class Handler(BaseHTTPRequestHandler):
    state: State
    base: str

    def log_message(self, fmt, *args):
        print(f"{self.command} {self.path.split('?')[0]} -> {fmt % args}", flush=True)

    def reply(self, obj, status=200):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length) if 0 < length < 1_000_000 else b""

    def signature_problem(self, body: bytes, endpoint: str):
        md5 = base64.b64encode(hashlib.md5(body).digest()).decode()
        if self.headers.get("Content-MD5") != md5:
            return "Content-MD5 does not match the body"
        auth = self.headers.get("X-Authorization", "")
        fields = dict(p.strip().split("=", 1) for p in auth.split(",") if "=" in p)
        if fields.get("AccessKey") != ACCESS_KEY:
            return "wrong AccessKey"
        message = "\n".join((md5, fields.get("Timestamp", ""), fields.get("Nonce", ""), endpoint))
        expected = hmac.new(ACCESS_SECRET.encode(), message.encode(), hashlib.sha1).hexdigest()
        if fields.get("Signature") != expected:
            return "bad signature"
        return None

    def do_POST(self):
        url = urlsplit(self.path)
        query = parse_qs(url.query)
        if url.path == "/relayservice":
            return self.relay(query)
        body = self.read_body()
        if url.path.startswith("/cipc/"):
            return self.cipc(url.path[len("/cipc"):], body)
        if url.path.startswith("/iot/"):
            return self.camera_control(url.path, body)

        regional = url.path.startswith("/eu/")
        endpoint = url.path[len("/eu"):] if regional else url.path
        problem = self.signature_problem(body, endpoint)
        if problem:
            print(f"  REJECTED {endpoint}: {problem}", flush=True)
            return self.reply({"error_code": -1, "msg": problem})
        payload = json.loads(body or b"{}")
        state = self.state

        if endpoint == "/api/v2/account/login":
            if not regional:  # like an account outside the default region
                return self.reply({"error_code": 0, "result": {"error_code": -20212, "appServerUrl": self.base + "/eu"}})
            if payload.get("cloudPassword") != PASSWORD:
                return self.reply({"error_code": -20601, "msg": "Incorrect email or password"})
            if state.args.mfa and payload.get("terminalUUID") not in state.verified:
                return self.reply({"error_code": 0, "result": {"MFAProcessId": "proc-1", "supportedMFATypes": [1, 2]}})
            return self.reply({"error_code": 0, "result": {"token": state.new_token()}})
        if endpoint in ("/api/v2/account/getPushVC4TerminalMFA", "/api/v2/account/getEmailVC4TerminalMFA"):
            state.pending_terminal = payload.get("terminalUUID")
            print(f"  verification code: {MFA_CODE}", flush=True)
            return self.reply({"error_code": 0, "result": {}})
        if endpoint == "/api/v2/account/checkMFACodeAndLogin":
            if payload.get("code") != MFA_CODE:
                return self.reply({"error_code": -20607, "msg": "Incorrect verification code"})
            state.verified.add(state.pending_terminal)
            return self.reply({"error_code": 0, "result": {"token": state.new_token()}})

        if not state.valid(query.get("token", [""])[0]):
            return self.reply({"error_code": -20651, "msg": "Token expired"})
        if endpoint == "/api/v2/common/getAppServiceUrl":
            # The real cloud answers with http://; clients switch to https.
            base = self.base.replace("https://", "http://")
            known = {"cipc.api.cloud": base + "/cipc", "nbu.iot-app-server.app": base + "/iot"}
            urls = {s: known[s] for s in payload.get("serviceIds", []) if s in known}
            return self.reply({"error_code": 0, "result": {"serviceUrls": urls}})
        if endpoint == "/api/v2/common/getDeviceListByPage":
            types = payload.get("deviceTypeList") or ["SMART.IPCAMERA", "SMART.TAPOHUB"]
            devices = [d for d in CAMERAS + HUBS if d["deviceType"] in types]
            index, limit = int(payload.get("index", 0)), int(payload.get("limit", 20))
            page = devices[index:index + limit]
            return self.reply({"error_code": 0, "result": {"deviceList": page, "totalNum": len(devices)}})
        return self.reply({"error_code": -1, "msg": f"unknown endpoint {endpoint}"}, 404)

    def cipc(self, endpoint, body):
        if not self.state.valid(self.headers.get("Authorization", "")):
            return self.reply({"error_code": -20651, "msg": "Token expired"})
        if endpoint != "/v2/relay/request":
            return self.reply({"error_code": -1}, 404)
        payload = json.loads(body or b"{}")
        device = payload.get("deviceId")
        if device == "FAKEHUB1":
            return self.clip_relay_request(payload)
        if device in OFFLINE:
            return self.reply({"error_code": -40401, "msg": "Device is offline"})
        if device not in PATTERNS:
            return self.reply({"error_code": -40210, "msg": "Unknown device"})
        if device in self.state.privacy:
            return self.reply({"error_code": -64324, "msg": "Privacy mode is on"})
        relay_token = "relay-" + uuid.uuid4().hex
        with self.state.lock:
            self.state.relays[relay_token] = (device, payload.get("resolution"))
        host = urlsplit(self.base).netloc
        return self.reply({"error_code": 0, "result": {
            "relayUrl": f"{host}/relayservice?deviceid={device}&type=video&sig=Zm9v%2Bbar==",
            "relayToken": relay_token,
            "elbCookie": "AWSELB=fake",
        }})

    def clip_relay_request(self, payload):
        problems = []
        if payload.get("streamType") != 3:
            problems.append(f"streamType {payload.get('streamType')}")
        if payload.get("deviceType") != "SMART.TAPOHUB":
            problems.append(f"deviceType {payload.get('deviceType')}")
        custom = json.loads(payload.get("customParams") or "{}")
        camera = custom.get("camera_device_id")
        if camera not in {c["device_id"] for c in HUB_CAMERAS}:
            problems.append(f"camera_device_id {camera}")
        if problems:
            print(f"  clip relay REFUSED: {', '.join(problems)}", flush=True)
            return self.reply({"error_code": -40210, "msg": "bad clip request"})
        relay_token = "relay-" + uuid.uuid4().hex
        with self.state.lock:
            self.state.relays[relay_token] = ("clip", camera, payload.get("playerId"))
        print(f"  clip relay for {camera} (subType {payload.get('subType')}, customParams {custom})", flush=True)
        host = urlsplit(self.base).netloc
        return self.reply({"error_code": 0, "result": {
            "relayUrl": f"{host}/relayservice?deviceid=FAKEHUB1&type=download&sig=Zm9v%2Bbar==",
            "relayToken": relay_token,
        }})

    def hub_method(self, method, params):
        result, code = {}, 0
        if method == "getGeneralDeviceList":
            result = {"general_camera_manage": {"max_bound": 4, "paired_general_device_list": HUB_CAMERAS}}
        elif method == "searchDateWithVideo":
            query = params.get("playback", {}).get("search_year_utility", {})
            camera = query.get("child_device_id")
            first = datetime.datetime.strptime(query.get("start_date"), "%Y%m%d").date()
            last = datetime.datetime.strptime(query.get("end_date"), "%Y%m%d").date()
            days = [datetime.date.today() - datetime.timedelta(days=ago) for ago in RECORDED_DAYS.get(camera, [])]
            days = sorted(d for d in days if first <= d <= last)
            result = {"playback": {"search_results": [
                {f"search_results_{i + 1}": {"date": d.strftime("%Y%m%d")}} for i, d in enumerate(days)]}}
        elif method == "searchVideoWithUTC":
            query = params.get("playback", {}).get("search_video_with_utc", {})
            if not query.get("player_id"):
                code = -40209
            else:
                self.state.search_players.add(query["player_id"])
                clips = clips_for(query.get("child_device_id"), int(query["start_time"]), int(query["end_time"]))
                page = clips[int(query["start_index"]):int(query["end_index"]) + 1]
                result = {"playback": {"search_video_results": [
                    {f"search_video_results_{i + 1}": {"startTime": a, "endTime": b, "video_type": str(k)}}
                    for i, (a, b, k) in enumerate(page)]}}
        else:
            code = -40106
        print(f"  hub {method} -> code {code}", flush=True)
        reply = {"method": method, "error_code": code}
        if code == 0:
            reply["result"] = result
        return reply

    def camera_control(self, path, body):
        # /iot/v2/things/<device id>/services-sync
        parts = path.strip("/").split("/")
        if len(parts) != 5 or parts[1] not in ("v1", "v2") or parts[4] != "services-sync":
            return self.reply({"message": "not found"}, 404)
        auth = self.headers.get("Authorization", "")
        if not auth.startswith("ut|") or not self.state.valid(auth[3:]):
            return self.reply({"message": "token expired"}, 401)
        device = parts[3]
        time.sleep(self.state.args.control_delay)
        payload = json.loads(body or b"{}")
        requests = payload.get("inputParams", {}).get("requestData", {}).get("params", {}).get("requests", [])
        if device in OFFLINE:
            return self.reply({"outputParams": {"responseData": {"error_code": -40401}}})
        if device == "FAKEHUB1":
            responses = [self.hub_method(r.get("method"), r.get("params", {})) for r in requests]
        else:
            responses = [self.camera_method(device, r.get("method"), r.get("params", {})) for r in requests]
        return self.reply({"outputParams": {"responseData": {"error_code": 0, "result": {"responses": responses}}}})

    def camera_method(self, device, method, params):
        state = self.state
        pos = state.position.setdefault(device, [0, 0])
        presets = state.presets.setdefault(device, [])
        result, code = {}, 0
        if method == "getAppComponentList":
            components = [{"name": "video", "version": 1}] + ([{"name": "ptz", "version": 1}] if device in PTZ else [])
            result = {"app_component": {"app_component_list": components}}
        elif method == "getLensMaskConfig":
            result = {"lens_mask": {"lens_mask_info": {"enabled": "on" if device in state.privacy else "off"}}}
        elif method == "setLensMaskConfig":
            enabled = params.get("lens_mask", {}).get("lens_mask_info", {}).get("enabled")
            (state.privacy.add if enabled == "on" else state.privacy.discard)(device)
        elif device in state.privacy:
            code = -64324  # the lens is covered
        elif device not in PTZ:
            code = -40106  # method not supported
        elif method == "motorMove":
            move = params.get("motor", {}).get("move", {})
            dx, dy = int(move.get("x_coord", 0)), int(move.get("y_coord", 0))
            pan = max(-PAN_RANGE, min(PAN_RANGE, pos[0] + dx))
            tilt = max(-TILT_RANGE, min(TILT_RANGE, pos[1] + dy))
            if [pan, tilt] == pos and (dx or dy):
                code = -64304  # already at the end stop
            pos[:] = [pan, tilt]
        elif method == "getPresetConfig":
            result = {"preset": {"preset": {
                "id": [p[0] for p in presets], "name": [p[1] for p in presets],
                "position_pan": [str(p[2]) for p in presets], "position_tilt": [str(p[3]) for p in presets],
                "read_only": ["0" for _ in presets],
            }}}
        elif method == "motorMoveToPreset":
            target = params.get("preset", {}).get("goto_preset", {}).get("id")
            match = [p for p in presets if p[0] == target]
            if match:
                pos[:] = [match[0][2], match[0][3]]
            else:
                code = -64321
        elif method == "addMotorPostion":
            name = params.get("preset", {}).get("set_preset", {}).get("name", "")
            presets.append((str(len(presets) + 1), name, pos[0], pos[1]))
        else:
            code = -40106
        print(f"  {device} {method} -> code {code}, position pan {pos[0]} tilt {pos[1]}", flush=True)
        reply = {"method": method, "error_code": code}
        if code == 0:
            reply["result"] = result
        return reply

    def relay(self, query):
        with self.state.lock:
            target = self.state.relays.pop(self.headers.get("X-token", ""), None)
        if not target or query.get("retryTime") != ["0"] or self.headers.get("X-Pull-Mode") != "2":
            self.send_response(403)
            self.end_headers()
            return
        if target[0] == "clip":
            return self.clip_relay(*target[1:])
        device, resolution = target
        size = "1280x720" if resolution == "HD" else "640x360"
        print(f"  relay: {device} {resolution} {size}", flush=True)
        self.close_connection = True
        self.wfile.write(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/mixed;boundary=--device-stream-boundary--\r\n\r\n")
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-re",
               "-f", "lavfi", "-i", f"{PATTERNS[device]}=size={size}:rate=15",
               "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=8000",
               "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "30", "-pix_fmt", "yuv420p",
               "-c:a", "aac", "-b:a", "16k", "-f", "mpegts", "pipe:1"]
        proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            while (chunk := proc.stdout.read1(16 * 1316)) and device not in self.state.privacy:
                head = (f"----device-stream-boundary--\r\nContent-Type: video/mp2t\r\n"
                        f"Content-Length: {len(chunk)}\r\nX-If-Encrypt: 0\r\n\r\n").encode()
                self.wfile.write(head + chunk + b"\r\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
            pass
        finally:
            proc.kill()
            print(f"  relay: {device} closed", flush=True)


    def read_client_part(self):
        """The next part the client sends in its (endless) request body."""
        while True:
            line = self.rfile.readline(65536)
            if not line:
                return None
            if b"----client-stream-boundary--" in line:
                break
        headers = {}
        while (line := self.rfile.readline(65536).strip()):
            name, _, value = line.decode("latin-1").partition(":")
            headers[name.strip().lower()] = value.strip()
        body = self.rfile.read(int(headers.get("content-length", "0")))
        return headers, json.loads(body or b"{}")

    def send_part(self, content_type, body, extra=""):
        head = (f"----device-stream-boundary--\r\nContent-Type: {content_type}\r\n"
                f"Content-Length: {len(body)}\r\n{extra}\r\n").encode()
        self.wfile.write(head + body + b"\r\n")
        self.wfile.flush()

    def clip_relay(self, camera, player_id):
        """A hub sending one recording, as the tapo crate saw an H200 do it."""
        self.close_connection = True
        first = self.read_client_part()
        if not first:
            return
        headers, request = first
        download = request.get("params", {}).get("download", {})
        problems = []
        if request.get("params", {}).get("method") != "get" or download.get("dev_id") != camera:
            problems.append(f"request {request}")
        if headers.get("x-data-window-size") != "50":
            problems.append("no X-Data-Window-Size: 50")
        if download.get("player_id") != player_id or player_id not in self.state.search_players:
            problems.append("the download's player id isn't the one the clips were searched with")
        self.wfile.write(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/mixed;boundary=--device-stream-boundary--\r\n\r\n")
        if problems:
            print(f"  clip REFUSED: {'; '.join(problems)}", flush=True)
            reply = {"type": "response", "seq": request.get("seq"), "params": {"error_code": -40210}}
            self.send_part("application/json", json.dumps(reply).encode())
            return
        start, end = int(download["start_time"]), int(download["end_time"])
        print(f"  clip: {camera} {end - start}s from {datetime.datetime.fromtimestamp(start)}", flush=True)
        reply = {"type": "response", "seq": request.get("seq"), "params": {"error_code": 0, "session_id": "42"}}
        self.send_part("application/json", json.dumps(reply).encode())
        pattern = PATTERNS.get(camera, "testsrc")
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error",
               "-f", "lavfi", "-i", f"{pattern}=size=1280x720:rate=15:duration={end - start}",
               "-c:v", "libx264", "-preset", "ultrafast", "-g", "30", "-pix_fmt", "yuv420p", "-f", "mpegts", "pipe:1"]
        ts = subprocess.run(cmd, capture_output=True, check=True).stdout
        chunks = [ts[i:i + 4 * 1316] for i in range(0, len(ts), 4 * 1316)]
        acked, heartbeats = 0, 0
        try:
            for seq, chunk in enumerate(chunks, 1):
                while seq - acked > 50:  # the window: wait for the client's acknowledgement
                    part = self.read_client_part()
                    if part is None:
                        print("  clip: client went away", flush=True)
                        return
                    headers, message = part
                    event = message.get("params", {}).get("event_type")
                    if event == "stream_sequence":
                        acked = int(headers.get("x-data-received", acked))
                        if headers.get("x-session-id") != "42":
                            print("  clip: acknowledgement without the session id", flush=True)
                    elif event == "heartbeat":
                        heartbeats += 1
                    elif message.get("params", {}).get("method") == "do":
                        print("  clip: client stopped early", flush=True)
                        return
                self.send_part("video/mp2t", chunk, f"X-Data-Sequence: {seq}\r\nX-If-Encrypt: 0\r\n")
            done = {"type": "notification", "params": {"event_type": "stream_status", "status": "finished"}}
            self.send_part("application/json", json.dumps(done).encode())
            print(f"  clip: sent {len(chunks)} parts, {len(ts)} bytes; acknowledged up to {acked}", flush=True)
            while (part := self.read_client_part()) is not None:
                if part[1].get("params", {}).get("method") == "do":
                    print("  clip: client said stop", flush=True)
                    break
        except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
            print("  clip: connection lost", flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=8443)
    ap.add_argument("--certs", default="target/fake-cloud", help="directory for the test CA and certificate")
    ap.add_argument("--mfa", action="store_true", help="ask for a verification code at the first sign-in")
    ap.add_argument("--expire-after", type=float, default=0, help="session tokens stop working after N seconds")
    ap.add_argument("--control-delay", type=float, default=0, help="seconds each camera-control call takes, like the real cloud")
    args = ap.parse_args()

    certs = Path(args.certs)
    make_certs(certs)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    Handler.state = State(args)
    Handler.base = f"https://127.0.0.1:{server.server_address[1]}"
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certs / "cert.pem", certs / "key.pem")
    server.socket = context.wrap_socket(server.socket, server_side=True)
    print(f"fake Tapo cloud at {Handler.base}; CA: {(certs / 'ca.pem').resolve()}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
