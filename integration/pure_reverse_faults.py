"""Damaged, synthetic and randomly mutated MP4s through the pure export worker.

    systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=0 \
        python3 pure_reverse_faults.py BINARY NEW_WORKDIR REPORT.json [--cases 300] [--seed 20261117]

Base inputs are small camera MP4s the product makes (`remux`, pure engine) from synthetic H.264 TS
and raw HEVC (system ffmpeg, development only), plus REAL=MP4 files given with --real. Each case
mutates one base with a seeded generator and runs `BINARY __pure-reverse` the way `media` does
(180 s, its own memory limit). Reported per case: outcome (done or the failure class the worker
answered, or how it ended), peak RSS and elapsed; overall: panics, crashes, outputs left behind,
and the largest RSS and elapsed. The same seed gives the same cases.
"""
import argparse
import json
import os
import random
import struct
import subprocess
import threading
import time
from pathlib import Path

CONTAINERS = {b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"dinf"}
TABLES = (b"stsz", b"stts", b"stsc", b"stco", b"co64", b"stss", b"ctts")
CONFIG = (b"avcC", b"hvcC")


def boxes(data, start=0, end=None, depth=0, out=None):
    """(offset, size, type, depth) of every box the tree walk reaches."""
    out = [] if out is None else out
    end = len(data) if end is None else end
    at = start
    while at + 8 <= end:
        size, kind = struct.unpack(">I4s", data[at:at + 8])
        header = 8
        if size == 1 and at + 16 <= end:
            size, header = struct.unpack(">Q", data[at + 8:at + 16])[0], 16
        elif size == 0:
            size = end - at
        if size < header or at + size > end:
            break
        out.append((at, size, kind, depth))
        if kind in CONTAINERS:
            boxes(data, at + header, at + size, depth + 1, out)
        elif kind == b"stsd":
            boxes(data, at + 16, at + size, depth + 1, out)
        elif kind in (b"avc1", b"hvc1"):
            boxes(data, at + 8 + 78, at + size, depth + 1, out)
        at += size
    return out


def find(data, kind):
    return [b for b in boxes(data) if b[2] == kind]


def put32(data, at, value):
    if 0 <= at and at + 4 <= len(data):
        data[at:at + 4] = struct.pack(">I", value & 0xFFFFFFFF)


def mutate(rng, base, kinds):
    data = bytearray(base)
    kind = rng.choice(kinds)
    tree = boxes(data)
    if kind == "truncate":
        del data[rng.randrange(0, len(data)):]
    elif kind == "box_size":
        at, size, _, _ = rng.choice(tree)
        put32(data, at, rng.choice([0, 1, 7, size - 1, size + 1, size * 2, 0x7FFFFFFF, 0xFFFFFFFF,
                                    rng.randrange(0, 1 << 32)]))
    elif kind == "sample_table":
        tables = [b for b in tree if b[2] in TABLES]
        at, size, _, _ = rng.choice(tables)
        offset = at + 12 + 4 * rng.randrange(0, max(1, min(64, (size - 12) // 4)))
        put32(data, offset, rng.choice([0, 1, 2, 0xFFFF, 0x7FFFFFFF, 0xFFFFFFFF, rng.randrange(0, 1 << 32)]))
    elif kind == "empty_track":
        for at, _, k, _ in tree:
            if k in (b"stsz",):
                put32(data, at + 16, 0)
            if k in (b"stts", b"stsc", b"stco", b"co64", b"stss", b"ctts"):
                put32(data, at + 12, 0)
    elif kind == "oversized":
        target = rng.choice(["sample", "moov", "config", "nal"])
        if target == "sample":
            at = find(data, b"stsz")[0][0]
            put32(data, at + 20 + 4 * rng.randrange(0, 8), rng.choice([0x7FFFFFFF, 0x2000001, 0xFFFFFFFF]))
        elif target == "moov":
            put32(data, find(data, b"moov")[0][0], 0x7FFFFFF0)
        elif target == "config":
            at = [b for b in tree if b[2] in CONFIG][0][0]
            data[at + 8 + rng.randrange(5, 30)] = 0xFF
        else:
            at = find(data, b"mdat")[0][0] + 8
            put32(data, at, rng.choice([0x7FFFFFFF, 0x2000001, len(data)]))
    elif kind == "moov_flips":
        at, size, _, _ = find(data, b"moov")[0]
        for _ in range(rng.randrange(1, 17)):
            data[at + 8 + rng.randrange(0, size - 8)] ^= 1 << rng.randrange(8)
    elif kind == "payload_flips":
        at, size, _, _ = find(data, b"mdat")[0]
        for _ in range(rng.randrange(1, 17)):
            data[at + 8 + rng.randrange(0, size - 8)] ^= 1 << rng.randrange(8)
    elif kind == "config_counts":
        at, size, k, _ = [b for b in tree if b[2] in CONFIG][0]
        field = at + 8 + (5 if k == b"avcC" else 22)
        data[field] = rng.randrange(256)
    elif kind == "garbage_box":
        at, size, _, depth = rng.choice([b for b in tree if b[3] <= 2])
        junk = struct.pack(">I4s", 8 + 32, rng.choice([b"moov", b"trak", b"stsz", b"mdat", b"free", b"zzzz"]))
        data[at:at] = junk + bytes(rng.randrange(256) for _ in range(32))
    elif kind == "random_bytes":
        data = bytearray(rng.randrange(256) for _ in range(rng.choice([0, 1, 8, 100, 4096])))
        if rng.random() < 0.5 and len(data) >= 8:
            data[4:8] = b"ftyp"
    return kind, bytes(data)


def run_worker(binary, source, kind, output):
    start = time.monotonic()
    process = subprocess.Popen([str(binary), "__pure-reverse", str(source), kind, "0", str(output)],
                               stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    deadline = threading.Timer(180, process.kill)
    deadline.start()
    stdout = process.stdout.read()
    _, status, usage = os.wait4(process.pid, 0)
    deadline.cancel()
    return os.waitstatus_to_exitcode(status), stdout, usage, time.monotonic() - start


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("binary", type=Path)
    p.add_argument("workdir", type=Path)
    p.add_argument("report", type=Path)
    p.add_argument("--cases", type=int, default=300)
    p.add_argument("--seed", type=int, default=20261117)
    p.add_argument("--real", action="append", default=[])
    args = p.parse_args()
    args.workdir.mkdir(parents=True)
    bases = []
    for hevc in (False, True):
        kind = "fcamera.hevc" if hevc else "qcamera.ts"
        raw = args.workdir / kind
        cmd = ["ffmpeg", "-v", "error", "-nostdin", "-f", "lavfi", "-i", "testsrc2=size=160x96:rate=10", "-t", "3",
               "-an", "-threads", "1", "-pix_fmt", "yuv420p"]
        cmd += (["-c:v", "libx265", "-x265-params", "pools=1:frame-threads=1:bframes=0:keyint=10:log-level=error",
                 "-f", "hevc"] if hevc else ["-c:v", "libx264", "-bf", "0", "-g", "10", "-f", "mpegts"])
        subprocess.run(cmd + [str(raw)], check=True)
        times = args.workdir / f"{kind}.times.json"
        times.write_text(json.dumps([i / 10 for i in range(30)]))
        mp4 = args.workdir / f"{kind}.mp4"
        subprocess.run([str(args.binary), "remux", str(raw), kind, str(times), str(mp4)], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        bases.append((kind, mp4.read_bytes()))
    for real in args.real:
        name = Path(real).name
        bases.append(("qcamera.ts" if "camera.mp4" in name and "fcamera" not in name and "ecamera" not in name
                      else "fcamera.hevc", Path(real).read_bytes()))
    kinds = ["truncate", "box_size", "sample_table", "empty_track", "oversized", "moov_flips", "payload_flips",
             "config_counts", "garbage_box", "random_bytes"]
    rng = random.Random(args.seed)
    rows = []
    source, output = args.workdir / "case.mp4", args.workdir / "case.out"
    for i in range(args.cases):
        base_kind, base = bases[i % len(bases)]
        mutation, data = mutate(rng, base, kinds)
        kind = base_kind
        if rng.random() < 0.05:
            kind = "fcamera.hevc" if base_kind == "qcamera.ts" else "qcamera.ts"
        source.write_bytes(data)
        output.unlink(missing_ok=True)
        code, stdout, usage, elapsed = run_worker(args.binary, source, kind, output)
        outcome, failure_class = None, None
        if code == 0:
            reply = json.loads(stdout)
            outcome = reply["result"]
            if outcome == "failed":
                failure_class = reply["class"]
                left = output.exists()
            else:
                left = not (output.exists() and output.stat().st_size == reply["bytes"])
        else:
            outcome = "panic" if code == 101 else f"exit_{code}"
            left = output.exists()
        rows.append({"case": i, "base": i % len(bases), "mutation": mutation, "kind": kind, "outcome": outcome,
                     "failure_class": failure_class, "code": reply.get("code") if code == 0 and outcome == "failed" else None,
                     "left_output_on_failure": bool(left) if outcome != "done" else None,
                     "peak_rss_kib": usage.ru_maxrss if usage else None, "elapsed_s": round(elapsed, 3)})
        output.unlink(missing_ok=True)
    source.unlink(missing_ok=True)
    summary = {"cases": len(rows), "seed": args.seed, "bases": len(bases), "by_outcome": {}, "by_mutation": {},
               "panics": sum(r["outcome"] == "panic" for r in rows),
               "abnormal_exits": sum(r["outcome"] not in ("done", "failed") for r in rows),
               "outputs_left_on_failure": sum(bool(r["left_output_on_failure"]) for r in rows),
               "peak_rss_kib_max": max(r["peak_rss_kib"] or 0 for r in rows),
               "elapsed_s_max": max(r["elapsed_s"] for r in rows)}
    for r in rows:
        key = r["failure_class"] or r["outcome"]
        summary["by_outcome"][key] = summary["by_outcome"].get(key, 0) + 1
        per = summary["by_mutation"].setdefault(r["mutation"], {})
        per[key] = per.get(key, 0) + 1
    args.report.write_text(json.dumps({"summary": summary, "cases": rows}, indent=1) + "\n")
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
