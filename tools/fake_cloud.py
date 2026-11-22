#!/usr/bin/env python3
"""Fake Tapo cloud for testing Tapo Viewer's cloud live view without an account.

Serves over HTTPS on 127.0.0.1:
  * the account API: sign-in (with a regional redirect and, with --mfa, a
    verification code), the service lookup and the camera list;
  * the CIPC relay request; and
  * the video relay itself, streaming live MPEG-TS made by ffmpeg.

Signed account calls are checked like the real cloud checks them (Content-MD5
and the HMAC signature), so a client signing wrongly fails here too.

Only debug builds of Tapo Viewer can be pointed at it:

  tools/fake_cloud.py --certs target/fake-cloud --mfa
  TAPO_VIEWER_FAKE_CLOUD=https://127.0.0.1:8443 \
  TAPO_VIEWER_FAKE_CLOUD_CA=target/fake-cloud/ca.pem cargo run

Sign in with any email and the password "secret"; the verification code is
123456. "Back yard" is reported offline. --expire-after N makes session tokens
stop working after N seconds, to test signing in again.

Needs Python 3, the openssl command and an ffmpeg with libx264 on PATH.
"""

import argparse
import base64
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


class State:
    def __init__(self, args):
        self.args = args
        self.lock = threading.Lock()
        self.tokens = {}  # token -> time issued
        self.verified = set()  # terminal UUIDs that passed two-step verification
        self.pending_terminal = None
        self.relays = {}  # relay token -> (device id, resolution)

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
            cipc = self.base.replace("https://", "http://") + "/cipc"
            return self.reply({"error_code": 0, "result": {"serviceUrls": {"cipc.api.cloud": cipc}}})
        if endpoint == "/api/v2/common/getDeviceListByPage":
            index, limit = int(payload.get("index", 0)), int(payload.get("limit", 20))
            page = CAMERAS[index:index + limit]
            return self.reply({"error_code": 0, "result": {"deviceList": page, "totalNum": len(CAMERAS)}})
        return self.reply({"error_code": -1, "msg": f"unknown endpoint {endpoint}"}, 404)

    def cipc(self, endpoint, body):
        if not self.state.valid(self.headers.get("Authorization", "")):
            return self.reply({"error_code": -20651, "msg": "Token expired"})
        if endpoint != "/v2/relay/request":
            return self.reply({"error_code": -1}, 404)
        payload = json.loads(body or b"{}")
        device = payload.get("deviceId")
        if device in OFFLINE:
            return self.reply({"error_code": -40401, "msg": "Device is offline"})
        if device not in PATTERNS:
            return self.reply({"error_code": -40210, "msg": "Unknown device"})
        relay_token = "relay-" + uuid.uuid4().hex
        with self.state.lock:
            self.state.relays[relay_token] = (device, payload.get("resolution"))
        host = urlsplit(self.base).netloc
        return self.reply({"error_code": 0, "result": {
            "relayUrl": f"{host}/relayservice?deviceid={device}&type=video&sig=Zm9v%2Bbar==",
            "relayToken": relay_token,
            "elbCookie": "AWSELB=fake",
        }})

    def relay(self, query):
        with self.state.lock:
            target = self.state.relays.pop(self.headers.get("X-token", ""), None)
        if not target or query.get("retryTime") != ["0"] or self.headers.get("X-Pull-Mode") != "2":
            self.send_response(403)
            self.end_headers()
            return
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
            while chunk := proc.stdout.read1(16 * 1316):
                head = (f"----device-stream-boundary--\r\nContent-Type: video/mp2t\r\n"
                        f"Content-Length: {len(chunk)}\r\nX-If-Encrypt: 0\r\n\r\n").encode()
                self.wfile.write(head + chunk + b"\r\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
            pass
        finally:
            proc.kill()
            print(f"  relay: {device} closed", flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=8443)
    ap.add_argument("--certs", default="target/fake-cloud", help="directory for the test CA and certificate")
    ap.add_argument("--mfa", action="store_true", help="ask for a verification code at the first sign-in")
    ap.add_argument("--expire-after", type=float, default=0, help="session tokens stop working after N seconds")
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
