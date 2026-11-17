#!/usr/bin/env python3
"""Stands in for RV_PYTHON in tests/maps.cjs: ignores the inline Flask program, starts HARS on
RV_DATA instead and serves it at / and at /api/hassio_ingress/test/ (as the test's
DispatcherMiddleware does), then prints READY <port>."""
import http.client, os, signal, socket, subprocess, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PREFIX = "/api/hassio_ingress/test"
DATA = Path(os.environ["RV_DATA"])


def free():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0)); return s.getsockname()[1]


ui, device = free(), free()
env = dict(os.environ, HARS_DATA=str(DATA), HARS_WEB=os.environ["HARS_WEB"], HARS_INGRESS_ONLY="0",
           HARS_BIND=f"127.0.0.1:{ui}", HARS_DEVICE_BIND=f"127.0.0.1:{device}")
log = open(os.environ.get("HARS_SHIM_LOG", os.devnull), "ab")


def on_disk():
    return frozenset(p.name for p in DATA.iterdir() if p.is_dir() and (p / "meta.json").is_file())


def start():
    proc = subprocess.Popen([os.environ["HARS_BIN"]], env=env, stdout=log, stderr=log)
    for _ in range(100):
        try:
            socket.create_connection(("127.0.0.1", ui), timeout=.2).close(); return proc
        except OSError:
            if proc.poll() is not None: sys.exit("HARS exited")
            time.sleep(.1)
    sys.exit("HARS did not listen")


def halt(proc):
    proc.terminate()
    try: proc.wait(timeout=5)
    except subprocess.TimeoutExpired: proc.kill(); proc.wait()


# HARS reads the recordings once at start and then learns of new ones only through its own API; the test writes
# more recording folders straight into RV_DATA while the server runs (its 'many' part), which the Python server sees
# because it lists the folder on every request. A request that finds the folder changed restarts HARS first, after
# the requests in flight have finished, so HARS starts on what the test wrote.
gate = threading.Condition()
state = {"hars": None, "known": on_disk(), "active": 0, "restarting": False}
state["hars"] = start()


def stop(*_):
    halt(state["hars"])
    os._exit(0)


signal.signal(signal.SIGTERM, stop); signal.signal(signal.SIGINT, stop)


class Proxy(BaseHTTPRequestHandler):
    # One connection per request, as the test's werkzeug server answers (Connection: close). maps.cjs line 110 counts
    # the card route lines right after the 지도 button shows, and the button shows before /api/tracks has answered;
    # over keep-alive the count fell into that gap 2-5 times in 30 page loads, with Python behind this proxy as with
    # HARS, and never once with one connection per request.
    protocol_version = "HTTP/1.0"
    def log_message(self, *a): pass
    def handle_one(self):
        with gate:
            while state["restarting"]: gate.wait()
            now = on_disk()
            if now != state["known"]:
                state["restarting"] = True
                while state["active"]: gate.wait()
                halt(state["hars"]); state["hars"] = start(); state["known"] = now
                state["restarting"] = False; gate.notify_all()
            state["active"] += 1
        try: self.forward()
        finally:
            with gate:
                state["active"] -= 1
                if self.command != "GET": state["known"] = on_disk()  # recordings HARS itself made or removed
                gate.notify_all()
    def forward(self):
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
