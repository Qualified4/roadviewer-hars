"""HARS behind the Python test server's interface, for the preserved browser tests' inline server programs.

The tests start the Python server with `RV_PYTHON -u -c PROGRAM`, where PROGRAM imports `server` and serves
`server.app` with `werkzeug.serving.make_server`. With this folder first on PYTHONPATH (hars_as_python.py does that),
PROGRAM runs unchanged: its own fixtures (tests/fake_drive.py and the environment it sets) run as written, and
`make_server` starts HARS on RV_DATA instead and returns a front that answers at / and at /api/hassio_ingress/test/,
as the test's DispatcherMiddleware does.

Environment: HARS_BIN, HARS_WEB (required); HARS_PORT_RANGE=first-last (loopback ports to use, default: any free);
HARS_SHIM_LOG (HARS output, appended)."""
import atexit, fcntl, json, http.client, os, signal, socket, subprocess, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PREFIX = "/api/hassio_ingress/test"


def _ports():
    spec = os.environ.get("HARS_PORT_RANGE")
    if not spec:
        return None
    first, last = (int(v) for v in spec.split("-"))
    return range(first, last + 1)


def _bindable(port):
    with socket.socket() as s:
        try:
            s.bind(("127.0.0.1", port))
        except OSError:
            return False
    return True


class _PortLock:
    """Two servers of one test (archive_status, gdrive_panel) must not pick the same port between our check and
    HARS's bind, so choosing ports and waiting for HARS to listen happen under one lock per port range."""

    def __enter__(self):
        spec = os.environ.get("HARS_PORT_RANGE", "any")
        self.file = open(os.path.join(os.environ.get("TMPDIR", "/tmp"), f".hars-ports-{spec}.lock"), "w")
        fcntl.flock(self.file, fcntl.LOCK_EX)
        return self

    def __exit__(self, *exc):
        fcntl.flock(self.file, fcntl.LOCK_UN)
        self.file.close()


def free_port(taken=()):
    ports = _ports()
    if ports is None:
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]
    for port in ports:
        if port not in taken and _bindable(port):
            return port
    sys.exit(f"no free loopback port in {os.environ['HARS_PORT_RANGE']}")


def note(line):
    """Adapter events, in the HARS log so they read in order with HARS's own lines."""
    path = os.environ.get("HARS_SHIM_LOG")
    if path:
        with open(path, "a") as f:
            f.write(f"[shim {time.strftime('%H:%M:%S')}.{int(time.time() * 1000) % 1000:03d}] {line}\n")


class Hars:
    def __init__(self, data):
        self.data = Path(data)
        self.log = open(os.environ.get("HARS_SHIM_LOG", os.devnull), "ab")
        self.proc = None

    def name_recordings(self):
        """Every meta.json the Python server writes carries its folder's `id` (server.py, upload commit); a test
        fixture that leaves it out (navigation.cjs, whose list is mocked) is one Python never reads, but HARS checks
        every recording at start and refuses to start on it (store.rs). The id is filled in, nothing else changes."""
        for meta in self.data.glob("*/meta.json"):
            try:
                m = json.loads(meta.read_text())
            except (OSError, ValueError):
                continue
            if isinstance(m, dict) and "id" not in m:
                m["id"] = meta.parent.name
                tmp = meta.with_suffix(".shim-tmp")
                tmp.write_text(json.dumps(m))
                tmp.replace(meta)
                note(f"filled id of {meta.parent.name}")

    def start(self, taken=()):
        self.name_recordings()
        with _PortLock():
            self.ui = free_port(taken)
            self.device = free_port({*taken, self.ui})
            env = dict(os.environ, HARS_DATA=str(self.data), HARS_WEB=os.environ["HARS_WEB"], HARS_INGRESS_ONLY="0",
                       HARS_BIND=f"127.0.0.1:{self.ui}", HARS_DEVICE_BIND=f"127.0.0.1:{self.device}")
            # No PR_SET_PDEATHSIG: it fires when the creating *thread* ends, and a restart runs in a request thread,
            # so HARS was killed right after its first answer. A HARS left by a SIGKILLed shim is ended by the
            # runner's supervisor (child subreaper), which holds every process the test started.
            self.proc = subprocess.Popen([os.environ["HARS_BIN"]], env=env, stdout=self.log, stderr=self.log,
                                         stdin=subprocess.DEVNULL)
            for _ in range(300):
                try:
                    socket.create_connection(("127.0.0.1", self.ui), timeout=.2).close()
                    return
                except OSError:
                    if self.proc.poll() is not None:
                        sys.exit(f"HARS exited with {self.proc.returncode}")
                    time.sleep(.1)
            sys.exit("HARS did not listen")

    def halt(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()


class App:
    """`server.app`: the programs only set `app.logger.disabled` and hand the app to make_server."""

    class _Logger:
        disabled = False

    def __init__(self):
        self.logger = self._Logger()


class Front:
    def __init__(self, host, port):
        data = Path(os.environ["RV_DATA"])
        self.hars = Hars(data)
        self.hars.start()

        def on_disk():
            return frozenset(p.name for p in data.iterdir() if p.is_dir() and (p / "meta.json").is_file())

        # HARS reads the recordings once at start and then learns of new ones only through its own API; maps.cjs (its
        # 'many' part) writes recording folders straight into RV_DATA while the server runs, which the Python server
        # sees because it lists the folder on every request. A request that finds the folder changed restarts HARS
        # first, after the requests in flight have finished, so HARS starts on what the test wrote.
        gate = threading.Condition()
        state = {"known": on_disk(), "active": 0, "restarting": False}
        hars = self.hars

        class Proxy(BaseHTTPRequestHandler):
            # One connection per request, as the test's werkzeug server answers (Connection: close). maps.cjs line 110
            # counts the card route lines right after the 지도 button shows, and the button shows before /api/tracks
            # has answered; over keep-alive the count fell into that gap 2-5 times in 30 page loads, with Python
            # behind this proxy as with HARS, and never once with one connection per request.
            protocol_version = "HTTP/1.0"

            def log_message(self, *a):
                pass

            def handle_one(self):
                note(f"proxy {self.command} {self.path[:120]}")
                with gate:
                    while state["restarting"]:
                        gate.wait()
                    now = on_disk()
                    if now != state["known"]:
                        note(f"restart: {len(now - state['known'])} recordings added, {len(state['known'] - now)} gone")
                        state["restarting"] = True
                        while state["active"]:
                            gate.wait()
                        hars.halt()
                        hars.start()
                        state["known"] = now
                        state["restarting"] = False
                        gate.notify_all()
                    state["active"] += 1
                try:
                    self.forward()
                finally:
                    with gate:
                        state["active"] -= 1
                        if self.command != "GET":
                            state["known"] = on_disk()  # recordings HARS itself made or removed
                        gate.notify_all()

            def forward(self):
                path = self.path[len(PREFIX):] if self.path.startswith(PREFIX + "/") else self.path
                length = int(self.headers.get("Content-Length", 0) or 0)
                body = self.rfile.read(length) if length else None
                conn = http.client.HTTPConnection("127.0.0.1", hars.ui, timeout=120)
                headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection")}
                conn.request(self.command, path, body=body, headers=headers)
                r = conn.getresponse()
                data = r.read()
                # send_response_only: HARS's own Date and Server go through; send_response would add a second Date.
                self.send_response_only(r.status)
                for k, v in r.getheaders():
                    if k.lower() not in ("connection", "transfer-encoding", "keep-alive", "content-length"):
                        self.send_header(k, v)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                try:
                    if self.command != "HEAD":
                        self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                conn.close()

            do_GET = do_HEAD = do_POST = do_PUT = do_DELETE = do_PATCH = handle_one

        with _PortLock():
            front_port = free_port({hars.ui, hars.device}) if port == 0 else port
            self.http = ThreadingHTTPServer((host, front_port), Proxy)
        self.http.daemon_threads = True
        self.server_port = self.http.server_port
        atexit.register(self.hars.halt)

        def stop(*_):
            self.hars.halt()
            os._exit(0)

        if threading.current_thread() is threading.main_thread():
            signal.signal(signal.SIGTERM, stop)
            signal.signal(signal.SIGINT, stop)

    def serve_forever(self):
        try:
            self.http.serve_forever()
        finally:
            self.hars.halt()

    def shutdown(self):
        self.http.shutdown()
        self.hars.halt()
