#!/usr/bin/env python3
"""Stands in for RV_PYTHON in tests/maps.cjs: ignores the inline Flask program, starts HARS on
RV_DATA instead and serves it at / and at /api/hassio_ingress/test/ (as the test's
DispatcherMiddleware does), then prints READY <port>."""
import http.client, os, signal, socket, subprocess, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PREFIX = "/api/hassio_ingress/test"


def free():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0)); return s.getsockname()[1]


ui, device = free(), free()
env = dict(os.environ, HARS_DATA=os.environ["RV_DATA"], HARS_WEB=os.environ["HARS_WEB"], HARS_INGRESS_ONLY="0",
           HARS_BIND=f"127.0.0.1:{ui}", HARS_DEVICE_BIND=f"127.0.0.1:{device}")
log = open(os.environ.get("HARS_SHIM_LOG", os.devnull), "ab")
hars = subprocess.Popen([os.environ["HARS_BIN"]], env=env, stdout=log, stderr=log)


def stop(*_):
    hars.terminate()
    try: hars.wait(timeout=5)
    except subprocess.TimeoutExpired: hars.kill()
    os._exit(0)


signal.signal(signal.SIGTERM, stop); signal.signal(signal.SIGINT, stop)
for _ in range(100):
    try:
        socket.create_connection(("127.0.0.1", ui), timeout=.2).close(); break
    except OSError:
        if hars.poll() is not None: sys.exit("HARS exited")
        time.sleep(.1)


class Proxy(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def handle_one(self):
        path = self.path[len(PREFIX):] if self.path.startswith(PREFIX + "/") else self.path
        body = self.rfile.read(int(self.headers.get("Content-Length", 0) or 0)) if self.command in ("POST", "PUT", "DELETE", "PATCH") else None
        conn = http.client.HTTPConnection("127.0.0.1", ui, timeout=120)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection")}
        conn.request(self.command, path, body=body, headers=headers)
        r = conn.getresponse(); data = r.read()
        self.send_response(r.status)
        for k, v in r.getheaders():
            if k.lower() not in ("connection", "transfer-encoding", "keep-alive", "content-length"): self.send_header(k, v)
        self.send_header("Content-Length", str(len(data))); self.end_headers()
        try: self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError): pass
        conn.close()
    do_GET = do_HEAD = do_POST = do_PUT = do_DELETE = do_PATCH = handle_one


front = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
print("READY " + str(front.server_port), flush=True)
try: front.serve_forever()
finally: stop()
