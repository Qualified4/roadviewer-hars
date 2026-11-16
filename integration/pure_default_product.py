#!/usr/bin/env python3
"""Product-path checks of the default pure remux engine on the real executable.

Owned loopback servers only, each on a new temporary data folder; the corpus is
read only. Scenarios (all by default):

  play         browser upload -> automatic pure conversion -> Range playback,
               camera MP4 and original downloads byte-checked, engine events
  cancel       reconversion cancelled while a pure worker runs: process group
               gone, staging gone, status unconverted, originals unchanged, retry
  kill_worker  the pure worker SIGKILLed: resource failure, no fallback, retry
  kill_convert the conversion process SIGKILLed: its workers die with it, retry
  restart      the server SIGKILLed mid-conversion: no orphan, requeued, ready
  fsize        server under RLIMIT_FSIZE (SIGXFSZ ignored, writes fail EFBIG):
               resource failure without fallback, originals unchanged
  memory       worker memory budget 1 MiB: resource failure without fallback
  concurrent   four uploads at once with two conversions: automatic conversion
               waits for active uploads, all ready; then four reconversions at
               concurrency 4; process-tree peak RSS and concurrent workers
  bframe       a B-frame qcamera.ts (re-encoded from the corpus, synthetic):
               explicit fallback to FFmpeg, ready, Range playback
  ffmpeg       a server with HARS_REMUX_ENGINE=ffmpeg: the same upload flow

Prints and writes one JSON report (counts and states only, no paths, clocks or
coordinates). usage:
  pure_default_product.py CORPUS BINARY OUT --ports UI DEVICE [--segments 3 7 9 1] [--only NAME ...]
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import resource
import signal
import subprocess
import threading
import time
import urllib.error
import urllib.request

KINDS = ("rlog.zst", "qcamera.ts", "fcamera.hevc")


def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as f:
        while block := f.read(1 << 20):
            h.update(block)
    return h.hexdigest()


def tree(pid):
    found, pending = set(), [pid]
    while pending:
        p = pending.pop()
        if p in found:
            continue
        found.add(p)
        try:
            for task in (Path("/proc") / str(p) / "task").iterdir():
                try:
                    pending.extend(map(int, (task / "children").read_text().split()))
                except OSError:
                    pass
        except OSError:
            pass
    return found


def cmdline(pid):
    try:
        return (Path("/proc") / str(pid) / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
    except OSError:
        return ""


def rss(pid):
    try:
        return int(next(l for l in (Path("/proc") / str(pid) / "status").read_text().splitlines() if l.startswith("VmRSS:")).split()[1])
    except (OSError, StopIteration):
        return 0


def holders(data):
    """Processes, anywhere, working on this data folder (orphans included)."""
    out = []
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit() and str(data) in cmdline(int(entry.name)):
            out.append(int(entry.name))
    return out


class Server:
    def __init__(self, binary, root, ports, env=None, fsize=None):
        self.data = root / "data"
        self.url = f"http://127.0.0.1:{ports[0]}"
        self.log = root / "server.log"
        environ = dict(os.environ, HARS_DATA=str(self.data), HARS_INGRESS_ONLY="0",
                       HARS_BIND=f"127.0.0.1:{ports[0]}", HARS_DEVICE_BIND=f"127.0.0.1:{ports[1]}")
        environ.pop("HARS_REMUX_ENGINE", None)
        environ.update(env or {})

        def limit():
            if fsize:
                signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
                resource.setrlimit(resource.RLIMIT_FSIZE, (fsize, fsize))
        try:
            self.request("/health")
            raise AssertionError("another server answers on the port")
        except (OSError, urllib.error.URLError):
            pass
        with self.log.open("ab") as log:
            self.process = subprocess.Popen([str(binary)], env=environ, stdout=log, stderr=log, preexec_fn=limit)
        end = time.monotonic() + 20
        while True:
            try:
                self.request("/health")
                break
            except (OSError, urllib.error.URLError):
                assert self.process.poll() is None and time.monotonic() < end, "server did not start"
                time.sleep(.05)

    def request(self, path, method="GET", body=None, raw=False, headers=None, full=False):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        h = {"X-RoadViewer-Request": "1", "Content-Type": "application/octet-stream" if raw else "application/json"}
        h.update(headers or {})
        req = urllib.request.Request(self.url + path, data=body, method=method, headers=h)
        with urllib.request.urlopen(req, timeout=60) as r:
            data = r.read()
            if full:
                return r.status, r.headers, data
            return data if raw else json.loads(data)

    def stop(self, sig=signal.SIGTERM):
        if self.process.poll() is None:
            self.process.send_signal(sig)
            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()

    def events(self, record=None):
        out = []
        for line in self.log.read_text(errors="replace").splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            fields = row.get("fields", {})
            if str(fields.get("event", "")).startswith("remux_") and (record is None or fields.get("record") == record):
                out.append({k: fields.get(k) for k in ("event", "engine", "kind", "failure_class", "reason_code", "fallback",
                                                         "fallback_result", "terminal_basis") if fields.get(k) is not None})
        return out

    def row(self, record):
        return next(r for r in self.request("/api/logs")["logs"] if r["id"] == record)

    def wait(self, record, states, timeout=180):
        end, seen = time.monotonic() + timeout, []
        while True:
            row = self.row(record)
            if not seen or seen[-1] != row["status"]:
                seen.append(row["status"])
            if row["status"] in states:
                return row, seen
            assert time.monotonic() < end, f"{record}: {seen}"
            time.sleep(.05)

    def workers(self, needle="__pure-remux"):
        return [p for p in tree(self.process.pid) if p != self.process.pid and needle in cmdline(p)]

    def staging(self, record):
        return sorted(p.name for p in (self.data / record).iterdir() if p.name.startswith(".convert-"))

    def prepared(self, record):
        folder = self.data / record / "prepared"
        return {p.name: digest(p) for p in sorted(folder.iterdir())} if folder.is_dir() else {}

    def originals(self, record):
        return {k: digest(self.data / record / k) for k in KINDS if (self.data / record / k).is_file()}


def upload(server, files):
    manifest = {"files": [{"name": f.name, "size": f.stat().st_size, "sha256": digest(f)} for f in files]}
    session = server.request("/api/uploads", "POST", manifest)["id"]
    for i, f in enumerate(files):
        with f.open("rb") as source:
            offset = 0
            while block := source.read(2 << 20):
                server.request(f"/api/uploads/{session}/files/{i}?offset={offset}", "PUT", block, raw=True)
                offset += len(block)
    logs = server.request(f"/api/uploads/{session}/finish", "POST", {})["logs"]
    assert len(logs) == 1, logs
    return logs[0]["id"] if isinstance(logs[0], dict) else logs[0]


def segment_files(corpus, segment, replace=None):
    files = [next(corpus.glob(f"*--{segment}--{kind}")) for kind in KINDS]
    return [replace.get(f.name.split("--")[-1], f) if replace else f for f in files]


def playback(server, record, camera):
    name = {"qcamera": "camera.mp4", "front": "fcamera.mp4"}[camera]
    local = server.data / record / "prepared" / name
    size = local.stat().st_size
    with local.open("rb") as f:
        head = f.read(1024)
        f.seek(size - 1024)
        tail = f.read()
    status, headers, body = server.request(f"/api/logs/{record}/video?source={camera}", raw=True, full=True,
                                           headers={"Range": "bytes=0-1023"})
    status2, _, body2 = server.request(f"/api/logs/{record}/video?source={camera}", raw=True, full=True,
                                       headers={"Range": f"bytes={size - 1024}-"})
    _, _, whole = server.request(f"/api/logs/{record}/download?source={camera}&format=mp4", raw=True, full=True)
    return {"range206": status == 206 and status2 == 206, "range_bytes_equal": body == head and body2 == tail,
            "content_range": headers.get("Content-Range", "").startswith("bytes 0-1023/"),
            "download_equals_prepared": hashlib.sha256(whole).hexdigest() == digest(local)}


def engines(events):
    return sorted((e["event"], e.get("engine"), e.get("kind")) for e in events if e["event"] == "remux_engine")


def reconvert(server, record):
    server.request(f"/api/logs/{record}/prepared", "DELETE")
    server.request(f"/api/logs/{record}/convert", "POST", {})


def wait_worker(server, needle="__pure-remux", timeout=60):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        found = server.workers(needle)
        if found:
            return found
        time.sleep(.002)
    return []


def gone(server, timeout=10):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if not holders(server.data):
            return True
        time.sleep(.05)
    return False


def scenario_play(ctx):
    s = ctx.server()
    s.request("/api/settings/processing", "POST", {"auto_convert": True, "concurrency": 1, "keep_original_video": True})
    files = segment_files(ctx.corpus, ctx.segments[0])
    record = upload(s, files)
    row, states = s.wait(record, ("ready", "error"))
    out = {"states": states, "status": row["status"], "engines": engines(s.events(record)),
           "fallbacks": [e for e in s.events(record) if e["event"] != "remux_engine"],
           "terminal_basis": sorted({e.get("terminal_basis") for e in s.events(record) if e["event"] == "remux_engine"}),
           "originals_equal_upload": s.originals(record) == {f.name.split("--")[-1]: digest(f) for f in files}}
    for camera in ("qcamera", "front"):
        out[camera] = playback(s, record, camera)
    for f in files:
        kind = f.name.split("--")[-1]
        body = s.request(f"/api/logs/{record}/original/{kind}", raw=True)
        out.setdefault("original_downloads_equal", []).append(hashlib.sha256(body).hexdigest() == digest(f))
    ctx.keep = (s, record)
    return out


def ready_record(ctx):
    if ctx.keep is None or ctx.keep[0].process.poll() is not None:
        s = ctx.server()
        s.request("/api/settings/processing", "POST", {"auto_convert": True, "concurrency": 1, "keep_original_video": True})
        record = upload(s, segment_files(ctx.corpus, ctx.segments[0]))
        s.wait(record, ("ready",))
        ctx.keep = (s, record)
    return ctx.keep


def scenario_cancel(ctx):
    s, record = ready_record(ctx)
    originals = s.originals(record)
    reconvert(s, record)
    workers = wait_worker(s)
    stage = s.request("/api/progress")["progress"].get(record, {}).get("stage")
    s.request(f"/api/logs/{record}/cancel", "POST", {})
    row, states = s.wait(record, ("unconverted", "error", "ready"))
    out = {"worker_seen": bool(workers), "stage_at_cancel": stage, "states": states, "status": row["status"],
           "processes_gone": gone(s), "staging_left": s.staging(record), "originals_unchanged": s.originals(record) == originals}
    s.request(f"/api/logs/{record}/convert", "POST", {})
    out["retry"] = s.wait(record, ("ready", "error"))[0]["status"]
    return out


def scenario_kill_worker(ctx):
    s, record = ready_record(ctx)
    originals = s.originals(record)
    mark = len(s.events(record))
    reconvert(s, record)
    workers = wait_worker(s)
    for pid in workers:
        os.kill(pid, signal.SIGKILL)
    row, states = s.wait(record, ("ready", "error"))
    events = s.events(record)[mark:]
    out = {"killed": len(workers), "states": states, "status": row["status"], "events": events,
           "warning": next((w for w in s.request(f"/api/logs/{record}/data").get("warnings", []) if "영상" in w), None) if row["status"] == "ready" else row.get("error"),
           "processes_gone": gone(s), "staging_left": s.staging(record), "originals_unchanged": s.originals(record) == originals}
    s.request(f"/api/logs/{record}/prepared", "DELETE")
    s.request(f"/api/logs/{record}/convert", "POST", {})
    out["retry"] = s.wait(record, ("ready", "error"))[0]["status"]
    out["retry_videos"] = sorted(s.request(f"/api/logs/{record}/data")["videos"])
    return out


def scenario_kill_convert(ctx):
    s, record = ready_record(ctx)
    originals = s.originals(record)
    reconvert(s, record)
    workers = wait_worker(s)
    converters = [p for p in tree(s.process.pid) if p != s.process.pid and "convert-log" in cmdline(p) and "nice" not in cmdline(p).split()[0]]
    for pid in converters:
        os.kill(pid, signal.SIGKILL)
    row, states = s.wait(record, ("ready", "error", "unconverted"))
    out = {"worker_seen": bool(workers), "killed": len(converters), "states": states, "status": row["status"],
           "error": row.get("error"), "processes_gone": gone(s), "staging_left": s.staging(record),
           "originals_unchanged": s.originals(record) == originals}
    s.request(f"/api/logs/{record}/convert", "POST", {})
    out["retry"] = s.wait(record, ("ready", "error"))[0]["status"]
    return out


def scenario_restart(ctx):
    s, record = ready_record(ctx)
    originals = s.originals(record)
    reconvert(s, record)
    workers = wait_worker(s)
    s.stop(signal.SIGKILL)
    out = {"worker_seen": bool(workers), "orphans_gone": gone(s), "staging_after_kill": s.staging(record)}
    s2 = ctx.server(reuse=s)
    row, states = s2.wait(record, ("ready", "error"))
    out.update({"states_after_restart": states, "status": row["status"], "staging_left": s2.staging(record),
                "originals_unchanged": s2.originals(record) == originals})
    ctx.keep = (s2, record)
    return out


def scenario_limited(ctx, env=None, fsize=None):
    s, record = ready_record(ctx)
    originals = s.originals(record)
    s.stop()
    limited = ctx.server(reuse=s, env=env, fsize=fsize)
    mark = len(limited.events(record))
    reconvert(limited, record)
    row, states = limited.wait(record, ("ready", "error"))
    events = limited.events(record)[mark:]
    data = limited.request(f"/api/logs/{record}/data") if row["status"] == "ready" else {}
    out = {"states": states, "status": row["status"], "events": events, "videos": sorted(data.get("videos", {})),
           "warnings": [w[:80] for w in data.get("warnings", [])], "staging_left": limited.staging(record),
           "originals_unchanged": limited.originals(record) == originals, "processes_gone": None}
    limited.stop()
    out["processes_gone"] = gone(limited)
    normal = ctx.server(reuse=limited)
    normal.request(f"/api/logs/{record}/prepared", "DELETE")
    normal.request(f"/api/logs/{record}/convert", "POST", {})
    out["retry"] = normal.wait(record, ("ready", "error"))[0]["status"]
    out["retry_videos"] = sorted(normal.request(f"/api/logs/{record}/data")["videos"])
    ctx.keep = (normal, record)
    return out


def scenario_fsize(ctx):
    return scenario_limited(ctx, fsize=8 << 20)


def scenario_memory(ctx):
    return scenario_limited(ctx, env={"HARS_PURE_WORKER_MEMORY_MB": "1"})


class Observer:
    def __init__(self, server):
        self.server, self.stop, self.peak, self.workers, self.converters, self.samples = server, threading.Event(), 0, 0, 0, 0
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop.is_set():
            pids = tree(self.server.process.pid)
            self.peak = max(self.peak, sum(rss(p) for p in pids))
            lines = [cmdline(p) for p in pids]
            self.workers = max(self.workers, sum("__pure-remux" in l for l in lines))
            self.converters = max(self.converters, sum(l.split(" ")[0].endswith("roadviewer-hars") and " convert-log " in l for l in lines))
            self.samples += 1
            time.sleep(.01)

    def done(self):
        self.stop.set()
        self.thread.join()
        return {"peak_tree_rss_kib": self.peak, "max_pure_workers": self.workers, "max_conversion_processes": self.converters,
                "samples": self.samples}


def scenario_concurrent(ctx):
    s = ctx.server()
    s.request("/api/settings/processing", "POST", {"auto_convert": True, "concurrency": 2, "keep_original_video": True})
    sets = [segment_files(ctx.corpus, n) for n in ctx.segments[:4]]
    observer = Observer(s)
    waited = set()
    stop = threading.Event()

    def watch():
        while not stop.is_set():
            for record, p in s.request("/api/progress")["progress"].items():
                if p.get("stage") == "waiting_upload":
                    waited.add(record)
            time.sleep(.05)
    watcher = threading.Thread(target=watch, daemon=True)
    watcher.start()
    started = time.monotonic()
    # One upload left open (as a device would during a drive) holds back automatic conversion.
    held = s.request("/api/uploads", "POST", {"files": [{"name": "hold/rlog.zst", "size": 1024, "sha256": None}]})["id"]
    s.request(f"/api/uploads/{held}/files/0?offset=0", "PUT", b"hold", raw=True)
    with ThreadPoolExecutor(max_workers=4) as pool:
        records = list(pool.map(lambda files: upload(s, files), sets))
    time.sleep(3)
    held_states = [s.row(r)["status"] for r in records]
    s.request(f"/api/uploads/{held}", "DELETE")
    rows = [s.wait(r, ("ready", "error"), timeout=300)[0]["status"] for r in records]
    stop.set()
    watcher.join()
    first = {"uploads": len(records), "statuses": rows, "records_that_waited_for_uploads": len(waited),
             "states_while_an_upload_was_open": held_states,
             "elapsed_seconds": round(time.monotonic() - started, 3), **observer.done(),
             "engines_pure": all(engines(s.events(r)) == [("remux_engine", "pure", "fcamera.hevc"), ("remux_engine", "pure", "qcamera.ts")] for r in records)}
    s.request("/api/settings/processing", "POST", {"auto_convert": True, "concurrency": 4, "keep_original_video": True})
    observer = Observer(s)
    started = time.monotonic()
    for r in records:
        s.request(f"/api/logs/{r}/prepared", "DELETE")
    for r in records:
        s.request(f"/api/logs/{r}/convert", "POST", {})
    rows = [s.wait(r, ("ready", "error"), timeout=300)[0]["status"] for r in records]
    second = {"statuses": rows, "elapsed_seconds": round(time.monotonic() - started, 3), **observer.done()}
    s.stop()
    return {"concurrency2_uploads": first, "concurrency4_reconversions": second}


def bframe_ts(ctx, source):
    out = ctx.out / "bframe-qcamera.ts"
    if not out.exists():
        subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-y", "-copyts", "-i", str(source), "-map", "0:v:0", "-c:v", "libx264",
                        "-bf", "2", "-g", "20", "-fps_mode", "passthrough", "-enc_time_base:v", "1:90000", "-muxdelay", "0", "-muxpreload", "0", "-f", "mpegts", str(out)],
                       check=True)
    return out


def scenario_bframe(ctx):
    s = ctx.server()
    s.request("/api/settings/processing", "POST", {"auto_convert": True, "concurrency": 1, "keep_original_video": True})
    files = segment_files(ctx.corpus, ctx.segments[0])
    synthetic = bframe_ts(ctx, files[1]).rename(ctx.out / files[1].name) if not (ctx.out / files[1].name).exists() else ctx.out / files[1].name
    record = upload(s, [files[0], synthetic, files[2]])
    row, states = s.wait(record, ("ready", "error"))
    data = s.request(f"/api/logs/{record}/data") if row["status"] == "ready" else {}
    out = {"states": states, "status": row["status"], "events": s.events(record), "videos": sorted(data.get("videos", {})),
           "warnings": [w[:80] for w in data.get("warnings", [])]}
    if "qcamera" in data.get("videos", {}):
        out["qcamera"] = playback(s, record, "qcamera")
    s.stop()
    return out


def scenario_ffmpeg(ctx):
    s = ctx.server(env={"HARS_REMUX_ENGINE": "ffmpeg"})
    s.request("/api/settings/processing", "POST", {"auto_convert": True, "concurrency": 1, "keep_original_video": True})
    record = upload(s, segment_files(ctx.corpus, ctx.segments[0]))
    row, states = s.wait(record, ("ready", "error"))
    out = {"states": states, "status": row["status"], "engines": engines(s.events(record)),
           "other_events": [e for e in s.events(record) if e["event"] != "remux_engine"]}
    for camera in ("qcamera", "front"):
        out[camera] = playback(s, record, camera)
    s.stop()
    return out


class Context:
    def __init__(self, args):
        self.corpus, self.binary, self.out, self.ports, self.segments = args.corpus, args.binary, args.output, args.ports, args.segments
        self.keep, self.count, self.started = None, 0, []

    def server(self, reuse=None, env=None, fsize=None):
        if reuse is not None:
            reuse.stop()
            root = reuse.data.parent
        else:
            self.stop_all()
            self.count += 1
            root = self.out / f"server-{self.count}"
            root.mkdir()
        server = Server(self.binary, root, self.ports, env, fsize)
        self.started.append(server)
        return server

    def stop_all(self):
        for server in self.started:
            server.stop()
        self.started, self.keep = [], None


SCENARIOS = ["play", "cancel", "kill_worker", "kill_convert", "restart", "fsize", "memory", "concurrent", "bframe", "ffmpeg"]


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("corpus", type=Path)
    p.add_argument("binary", type=Path)
    p.add_argument("output", type=Path)
    p.add_argument("--ports", type=int, nargs=2, required=True)
    p.add_argument("--segments", type=int, nargs="+", default=[3, 7, 9, 1])
    p.add_argument("--only", nargs="+", choices=SCENARIOS)
    args = p.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    ctx = Context(args)
    report = {}
    try:
        for name in args.only or SCENARIOS:
            started = time.monotonic()
            try:
                report[name] = globals()[f"scenario_{name}"](ctx)
            except Exception as e:  # noqa: BLE001 - the report records which scenario broke
                report[name] = {"exception": f"{type(e).__name__}: {e}"[:300]}
                ctx.stop_all()
            report[name]["seconds"] = round(time.monotonic() - started, 1)
            print(json.dumps({name: report[name]}, ensure_ascii=False), flush=True)
    finally:
        ctx.stop_all()
    (args.output / "report.json").write_text(json.dumps(report, indent=1, ensure_ascii=False))


if __name__ == "__main__":
    main()
