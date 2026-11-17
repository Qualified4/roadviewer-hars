"""Alternating benchmark of the `__thumbnail` worker of several release binaries.

    thumbnail_benchmark.py OUTPUT_JSON --binary NAME=PATH ... --input NAME=MP4 ... [--runs 7]

Each input: one warmup run per binary, then RUNS rounds in which every binary runs once, the
order rotating each round (ABC, BCA, CAB, ...). Per run: wall time from spawn to exit, the
child's user+sys CPU time and peak RSS (wait4). Warm page cache: the inputs are read by the
warmup. Input names, not paths, go into the report.
"""
import argparse
import json
import os
import statistics
import subprocess
import tempfile
import time
from pathlib import Path


def once(binary, source, out):
    start = time.perf_counter()
    child = subprocess.Popen([binary, "__thumbnail", source, str(out)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    _, status, usage = os.wait4(child.pid, 0)
    wall = time.perf_counter() - start
    child.returncode = os.waitstatus_to_exitcode(status)
    assert child.returncode == 0, (binary, source, child.returncode)
    return {"ms": wall * 1000, "cpu_ms": (usage.ru_utime + usage.ru_stime) * 1000, "rss_kib": usage.ru_maxrss}


def summary(runs):
    out = {}
    for key in ("ms", "cpu_ms"):
        values = [r[key] for r in runs]
        out[key] = {"median": round(statistics.median(values), 1), "min": round(min(values), 1), "max": round(max(values), 1)}
    out["peak_rss_kib_max"] = max(r["rss_kib"] for r in runs)
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--binary", action="append", required=True)
    parser.add_argument("--input", action="append", required=True)
    parser.add_argument("--runs", type=int, default=7)
    args = parser.parse_args()
    binaries = [b.split("=", 1) for b in args.binary]
    report = {"loadavg_before": os.getloadavg(), "runs": args.runs, "cache": "warm", "rows": []}
    with tempfile.TemporaryDirectory() as temp:
        out = Path(temp) / "thumbnail.jpg"
        for name, source in (i.split("=", 1) for i in args.input):
            for _, binary in binaries:
                once(binary, source, out)
            runs = {b: [] for b, _ in binaries}
            for round_ in range(args.runs):
                k = round_ % len(binaries)
                for b, binary in binaries[k:] + binaries[:k]:
                    runs[b].append(once(binary, source, out))
            row = {"input": name, **{b: summary(r) for b, r in runs.items()}}
            report["rows"].append(row)
            print(json.dumps(row), flush=True)
    report["loadavg_after"] = os.getloadavg()
    args.output.write_text(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()
