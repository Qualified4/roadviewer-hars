"""Decisions of the ffprobe upload check (a 2.0.0-alpha.3 binary) and the pure Rust one (a newer
binary) on the same files, as JSON. Development tool: the host's `ffmpeg` makes the synthetic
inputs; it is no oracle here, both decisions come from the binaries.

    python3 integration/upload_check_compare.py BASE_BINARY NEW_BINARY WORK_DIR OUT.json \
        --ports P1 P2 P3 P4 [--real KIND=FILE ...] [--synthetic 100] [--seed 20261117]

Per file and kind:
  upload   the browser upload's verdict: `invalid_file` (the media check refused it) or accepted
           (any other answer). Only `.hevc` and `.mp4` kinds are checked at upload, as in the product.
  inspect  the conversion's source scan (`__inspect` worker): exit status 0 or not.
Run it under an outside memory limit (systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=0).
The JSON holds kinds, case names, sizes and verdicts; no host path or corpus file name.
"""
import argparse
import hashlib
import json
import os
import random
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROUTE = "00000999--0123456789"


class Server:
    def __init__(self, binary, root, ports):
        self.data = root / "data"
        self.data.mkdir(parents=True)
        (self.data / ".processing-settings.json").write_text(json.dumps({"auto_convert": False}))
        self.url = f"http://127.0.0.1:{ports[0]}"
        env = dict(os.environ, HARS_DATA=str(self.data), HARS_INGRESS_ONLY="0",
                   HARS_BIND=f"127.0.0.1:{ports[0]}", HARS_DEVICE_BIND=f"127.0.0.1:{ports[1]}")
        env.pop("HARS_REMUX_ENGINE", None)
        self.log = (root / "server.log").open("wb")
        self.process = subprocess.Popen([str(binary)], env=env, stdout=self.log, stderr=self.log)
        end = time.monotonic() + 20
        while True:
            try:
                self.request("/health")
                return
            except (OSError, urllib.error.URLError):
                assert self.process.poll() is None and time.monotonic() < end, "server did not start"
                time.sleep(.05)

    def request(self, path, method="GET", body=None, raw=False):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        headers = {"X-RoadViewer-Request": "1",
                   "Content-Type": "application/octet-stream" if raw else "application/json"}
        req = urllib.request.Request(self.url + path, data=body, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    def upload(self, path, name):
        data = path.read_bytes()
        manifest = {"files": [{"name": name, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}]}
        status, body = self.request("/api/uploads", "POST", manifest)
        if status not in (200, 201):
            return f"begin {status}"
        session = json.loads(body)["id"]
        for offset in range(0, len(data), 2 << 20):
            status, _ = self.request(f"/api/uploads/{session}/files/0?offset={offset}", "PUT",
                                     data[offset:offset + (2 << 20)], raw=True)
            if status not in (200, 201, 204):
                self.request(f"/api/uploads/{session}", "DELETE")
                return f"chunk {status}"
        status, body = self.request(f"/api/uploads/{session}/finish", "POST", {})
        try:
            error = json.loads(body).get("error")
        except ValueError:
            error = None
        return "rejected" if error == "invalid_file" else "accepted"

    def stop(self):
        self.process.terminate()
        self.process.wait(timeout=30)
        self.log.close()


def inspect(binary, path, kind):
    try:
        done = subprocess.run([str(binary), "__inspect", str(path), kind], capture_output=True, timeout=200)
        return "accepted" if done.returncode == 0 else "rejected"
    except subprocess.TimeoutExpired:
        return "timeout"


def ffmpeg(*args):
    subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-y", *args], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def synthetic(work, count, seed):
    """Generated inputs (one per property) and seeded random damage of four of them."""
    src = work / "synthetic"
    src.mkdir()
    lavfi = ["-f", "lavfi", "-i", "testsrc2=size=160x96:rate=10", "-t", "2", "-an", "-threads", "1"]
    x264 = ["-c:v", "libx264", "-pix_fmt", "yuv420p", "-bf", "0", "-g", "10"]
    x265 = ["-c:v", "libx265", "-pix_fmt", "yuv420p", "-x265-params",
            "pools=1:frame-threads=1:bframes=0:keyint=10:log-level=error"]
    made = {}
    def make(name, kind, *args):
        out = src / name
        try:
            ffmpeg(*args, str(out))
        except subprocess.CalledProcessError:
            print(f"fixture {name} not made by this ffmpeg; skipped", file=sys.stderr)
            return
        made[name] = (out, kind)
    make("h264.ts", "qcamera.ts", *lavfi, *x264, "-f", "mpegts")
    make("hevc.hevc", "fcamera.hevc", *lavfi, *x265, "-f", "hevc")
    make("h264.mp4", "camera.mp4", *lavfi, *x264, "-movflags", "+faststart", "-f", "mp4")
    make("hevc.mp4", "fcamera.mp4", *lavfi, *x265, "-tag:v", "hvc1", "-movflags", "+faststart", "-f", "mp4")
    make("hev1.mp4", "fcamera.mp4", *lavfi, *x265, "-tag:v", "hev1", "-movflags", "+faststart", "-f", "mp4")
    make("bframes.mp4", "camera.mp4", *lavfi, "-c:v", "libx264", "-pix_fmt", "yuv420p", "-bf", "2", "-g", "10",
         "-movflags", "+faststart", "-f", "mp4")
    make("moov-last.mp4", "camera.mp4", *lavfi, *x264, "-f", "mp4")
    make("with-audio.mp4", "camera.mp4", "-f", "lavfi", "-i", "testsrc2=size=160x96:rate=10",
         "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=8000", "-t", "2", "-threads", "1", *x264,
         "-c:a", "aac", "-shortest", "-movflags", "+faststart", "-f", "mp4")
    make("two-videos.mp4", "camera.mp4", "-f", "lavfi", "-i", "testsrc2=size=160x96:rate=10",
         "-f", "lavfi", "-i", "testsrc2=size=160x96:rate=10", "-t", "2", "-threads", "1",
         "-map", "0:v", "-map", "1:v", *x264, "-movflags", "+faststart", "-f", "mp4")
    make("too-wide.hevc", "fcamera.hevc", "-f", "lavfi", "-i", "testsrc2=size=4112x64:rate=10", "-t", "0.3",
         "-an", "-threads", "1", *x265, "-f", "hevc")
    make("too-wide.mp4", "camera.mp4", "-f", "lavfi", "-i", "testsrc2=size=4112x64:rate=10", "-t", "0.3",
         "-an", "-threads", "1", *x264, "-movflags", "+faststart", "-f", "mp4")
    make("odd-crop.mp4", "camera.mp4", "-f", "lavfi", "-i", "testsrc2=size=162x96:rate=10", "-t", "1", "-an",
         "-threads", "1", "-vf", "crop=161:95:0:0", "-c:v", "libx264", "-pix_fmt", "yuv444p", "-bf", "0",
         "-movflags", "+faststart", "-f", "mp4")
    make("h264-as-hevc.mp4", "fcamera.mp4", *lavfi, *x264, "-movflags", "+faststart", "-f", "mp4")
    made["ts-as-hevc"] = (made["h264.ts"][0], "fcamera.hevc")
    made["hevc-as-mp4"] = (made["hevc.hevc"][0], "fcamera.mp4")
    made["h264-mp4-as-hevc-kind"] = (made["h264.mp4"][0], "fcamera.hevc")
    for name, data, kind in [("empty.mp4", b"", "camera.mp4"), ("empty.hevc", b"", "fcamera.hevc"),
                             ("playlist.mp4", b"#EXTM3U\nhttps://example.com/x\n", "camera.mp4"),
                             ("playlist.hevc", b"#EXTM3U\nhttps://example.com/x\n", "fcamera.hevc")]:
        (src / name).write_bytes(data)
        made[name] = (src / name, kind)
    rng = random.Random(seed)
    bases = [made[n] for n in ("h264.ts", "hevc.hevc", "h264.mp4", "hevc.mp4")]
    damage = ["truncate", "head-flip", "any-flip", "box-size", "garbage-prefix", "zero-run"]
    cases = [(name, path, kind) for name, (path, kind) in made.items()]
    i = 0
    while len(cases) < count:
        path, kind = bases[i % len(bases)]
        how = damage[(i // len(bases)) % len(damage)]
        data = bytearray(path.read_bytes())
        if how == "truncate":
            data = data[:rng.randrange(1, len(data))]
        elif how == "head-flip":
            for _ in range(rng.randrange(1, 9)):
                data[rng.randrange(min(4096, len(data)))] ^= 1 << rng.randrange(8)
        elif how == "any-flip":
            for _ in range(rng.randrange(1, 9)):
                data[rng.randrange(len(data))] ^= 1 << rng.randrange(8)
        elif how == "box-size":
            at = rng.randrange(0, min(len(data) - 4, 2048))
            data[at:at + 4] = rng.randrange(1 << 32).to_bytes(4, "big")
        elif how == "garbage-prefix":
            data = bytearray(rng.randbytes(rng.randrange(1, 300))) + data
        else:
            at = rng.randrange(len(data))
            data[at:at + rng.randrange(1, 2000)] = bytes(min(2000, len(data) - at))
        out = src / f"damaged-{i:03d}-{how}-{path.name}"
        out.write_bytes(bytes(data))
        cases.append((out.name, out, kind))
        i += 1
    return cases[:count]


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("base")
    p.add_argument("new")
    p.add_argument("work", type=Path)
    p.add_argument("out", type=Path)
    p.add_argument("--ports", type=int, nargs=4, required=True)
    p.add_argument("--real", action="append", default=[], help="KIND=FILE, a real camera video")
    p.add_argument("--synthetic", type=int, default=100)
    p.add_argument("--seed", type=int, default=20261117)
    a = p.parse_args()
    a.work.mkdir(parents=True)
    cases = [(f"real-{i:02d}", Path(f), k) for i, (k, f) in enumerate(r.split("=", 1) for r in a.real)]
    cases += [(f"synthetic-{n}", path, kind) for n, path, kind in synthetic(a.work, a.synthetic, a.seed)]
    servers = {"base": Server(a.base, a.work / "base", a.ports[:2]),
               "new": Server(a.new, a.work / "new", a.ports[2:])}
    rows = []
    try:
        for n, (name, path, kind) in enumerate(cases):
            row = {"case": name, "kind": kind, "bytes": path.stat().st_size}
            uploaded = f"{ROUTE}--{n}--{'qcamera.mp4' if kind == 'camera.mp4' else kind}"
            for side, binary in (("base", a.base), ("new", a.new)):
                if kind.endswith((".hevc", ".mp4")):
                    row[f"{side}_upload"] = servers[side].upload(path, uploaded)
                row[f"{side}_inspect"] = inspect(binary, path, kind)
            row["upload_differs"] = row.get("base_upload") != row.get("new_upload")
            row["inspect_differs"] = row["base_inspect"] != row["new_inspect"]
            rows.append(row)
            print(json.dumps(row), flush=True)
    finally:
        for s in servers.values():
            s.stop()
    def count(key, value):
        return sum(1 for r in rows if r.get(key) == value)
    summary = {
        "cases": len(rows),
        "upload_checked": sum(1 for r in rows if "base_upload" in r),
        "upload_both_accept": sum(1 for r in rows if r.get("base_upload") == r.get("new_upload") == "accepted"),
        "upload_both_reject": sum(1 for r in rows if r.get("base_upload") == r.get("new_upload") == "rejected"),
        "upload_new_accepts_base_rejected": sum(1 for r in rows if r.get("base_upload") == "rejected"
                                                and r.get("new_upload") == "accepted"),
        "upload_new_rejects_base_accepted": sum(1 for r in rows if r.get("base_upload") == "accepted"
                                                and r.get("new_upload") == "rejected"),
        "inspect_both_accept": sum(1 for r in rows if r["base_inspect"] == r["new_inspect"] == "accepted"),
        "inspect_both_reject": sum(1 for r in rows if r["base_inspect"] == r["new_inspect"] == "rejected"),
        "inspect_new_accepts_base_rejected": sum(1 for r in rows if r["base_inspect"] == "rejected"
                                                 and r["new_inspect"] == "accepted"),
        "inspect_new_rejects_base_accepted": sum(1 for r in rows if r["base_inspect"] == "accepted"
                                                 and r["new_inspect"] == "rejected"),
        "timeouts": count("base_inspect", "timeout") + count("new_inspect", "timeout"),
    }
    a.out.write_text(json.dumps({"summary": summary, "seed": a.seed, "rows": rows}, indent=1))
    print(json.dumps(summary))
    shutil.rmtree(a.work / "base" / "data", ignore_errors=True)
    shutil.rmtree(a.work / "new" / "data", ignore_errors=True)


if __name__ == "__main__":
    main()
