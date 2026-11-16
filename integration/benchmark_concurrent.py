#!/usr/bin/env python3
"""Concurrent full conversions: N segments started together, per implementation.

Python runs N `decoder.py` processes (its job runner starts one per conversion);
HARS runs N `convert-log` processes (the server starts one per conversion, each
with its own media workers). Per round: total elapsed until the last finishes,
total CPU of all children (getrusage), and the 20 ms sampled peak of the summed
RSS of all their process trees (shared pages may repeat). Every output is checked
against the round's Python output of the same segment outside the timed interval.
Implementations rotate order per round; one warmup round each.

usage: benchmark_concurrent.py CORPUS OUT --segments 3 7 9 1 --jobs 2 4 --runs 5
       --binary NAME=PATH[@pure|@ffmpeg] [...]
"""
import argparse
import json
import os
from pathlib import Path
import resource
import shutil
import statistics
import subprocess
import sys
import threading
import time
from benchmark_conversion import compare
from profile_conversion import processes
import terminal_rule


def rss(pid):
    try:
        return int(next(l for l in (Path("/proc") / str(pid) / "status").read_text().splitlines() if l.startswith("VmRSS:")).split()[1])
    except (OSError, StopIteration):
        return 0


def media(log):
    return {kind: log.with_name(log.name.replace("rlog.zst", name)) for kind, name in (
        ("camera.mp4", "qcamera.mp4"), ("fcamera.mp4", "fcamera.mp4"), ("ecamera.mp4", "ecamera.mp4"),
        ("qcamera.ts", "qcamera.ts"), ("fcamera.hevc", "fcamera.hevc"), ("ecamera.hevc", "ecamera.hevc"))}


def round_(commands, out):
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    procs = []
    for command, env, log in commands:
        with log.open("w") as f:
            procs.append(subprocess.Popen(command, env=env, stdout=f, stderr=subprocess.STDOUT))
    peak, stop = [0], threading.Event()

    def monitor():
        while not stop.is_set():
            pids = set()
            for p in procs:
                pids |= processes(p.pid)
            peak[0] = max(peak[0], sum(rss(p) for p in pids))
            stop.wait(.02)
    thread = threading.Thread(target=monitor)
    thread.start()
    codes = [p.wait() for p in procs]
    elapsed = time.monotonic() - started
    stop.set()
    thread.join()
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    assert codes == [0] * len(codes), codes
    return {"elapsed_seconds": elapsed, "total_cpu_seconds": after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime,
            "peak_rss_trees_kib": peak[0]}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("corpus", type=Path)
    p.add_argument("output", type=Path)
    p.add_argument("--segments", type=int, nargs="+", required=True)
    p.add_argument("--jobs", type=int, nargs="+", default=[2, 4])
    p.add_argument("--runs", type=int, default=5)
    p.add_argument("--binary", action="append", required=True)
    args = p.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    reference = args.output / "reference"
    shutil.copytree(Path(__file__).resolve().parents[1] / "reference/python/roadviewer", reference)
    sys.path.insert(0, str(reference / "app"))
    from telemetry import drive_summary
    binaries = dict(x.split("=", 1) for x in args.binary)
    engines = {n: v.rsplit("@", 1)[1] for n, v in binaries.items() if "@" in v}
    binaries = {n: v.rsplit("@", 1)[0] for n, v in binaries.items()}
    env = {k: v for k, v in os.environ.items() if not k.startswith("HARS_PROFILE") and k != "HARS_REMUX_ENGINE"}
    logs = {s: next(args.corpus.glob(f"*--{s}--rlog.zst")) for s in args.segments}
    known = {s: terminal_rule.model(log, {k: v for k, v in media(log).items() if not k.endswith(".mp4")}, reference / "schema/cereal/log.capnp")
             for s, log in logs.items()}
    variants = ["python", *binaries]
    results = {"binaries": list(binaries), "jobs": args.jobs, "runs": args.runs, "segments": len(args.segments), "samples": []}
    for jobs in args.jobs:
        segments = args.segments[:jobs]
        assert len(segments) == jobs, "one segment per concurrent job"

        def run(name, label, warmup=False):
            dest = args.output / f"j{jobs}-{label}"
            dest.mkdir()
            commands = []
            for s in segments:
                out = dest / f"segment-{s}"
                if name == "python":
                    e = dict(env, RV_PREPARED_DIR=str(out), RV_MEDIA_PATHS=json.dumps({k: str(v) for k, v in media(logs[s]).items()}), RV_PROGRESS_FD="1")
                    commands.append(([sys.executable, str(reference / "app/decoder.py"), str(logs[s]), "benchmark"], e, dest / f"{s}.log"))
                else:
                    e = dict(env, HARS_REMUX_ENGINE=engines[name]) if name in engines else env
                    commands.append(([binaries[name], "convert-log", str(logs[s]), str(out), "benchmark"], e, dest / f"{s}.log"))
            metrics = round_(commands, dest)
            for s in segments:
                golden = args.output / f"j{jobs}-warmup-python" / f"segment-{s}"
                if dest.name != golden.parent.name:
                    compare(golden, dest / f"segment-{s}", known[s] if name != "python" else None, drive_summary)
                log = (dest / f"{s}.log").read_text(errors="replace")
                if name in engines:
                    assert f'"engine":"{engines[name]}"' in log and "remux_fallback" not in log, name
            metrics.update({"variant": name, "jobs": jobs, "round": label, "verified": True})
            if not warmup:
                results["samples"].append(metrics)
            print(json.dumps({k: metrics[k] for k in ("variant", "jobs", "round", "elapsed_seconds", "total_cpu_seconds", "peak_rss_trees_kib")}), flush=True)
            (args.output / "benchmark.json").write_text(json.dumps(results, indent=1))

        # The Python warmup output is the reference every measured round is checked against.
        for name in variants:
            run(name, f"warmup-{name}", True)
        for i in range(args.runs):
            order = variants[i % len(variants):] + variants[:i % len(variants)]
            if i % 2:
                order = list(reversed(order))
            for name in order:
                run(name, f"{i + 1}-{name}")
            for name in variants:
                shutil.rmtree(args.output / f"j{jobs}-{i + 1}-{name}", ignore_errors=True)
        for name in variants:
            shutil.rmtree(args.output / f"j{jobs}-warmup-{name}", ignore_errors=True)
    results["summary"] = {f"{name}@{jobs}": {m: {"median": statistics.median(x[m] for x in rows), "min": min(x[m] for x in rows), "max": max(x[m] for x in rows)}
                                            for m in ("elapsed_seconds", "total_cpu_seconds", "peak_rss_trees_kib")}
                          for jobs in args.jobs for name in variants
                          for rows in [[x for x in results["samples"] if x["variant"] == name and x["jobs"] == jobs]]}
    (args.output / "benchmark.json").write_text(json.dumps(results, indent=1))
    print(json.dumps(results["summary"]))


if __name__ == "__main__":
    main()
