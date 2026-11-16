"""Interleaved benchmark of the original-format export (MP4 -> camera original).

    benchmark_reverse.py MP4_DIR OUTPUT_JSON --scratch NEW_DIR --impl NAME=SPEC [--impl ...]
                         [--segments 3 7 9] [--runs 5]

MP4_DIR holds `<segment>-camera.mp4` / `<segment>-fcamera.mp4` and the camera original each came
from (`<segment>-qcamera.ts`, `<segment>-fcamera.hevc`), whose smallest TS PTS is the export
offset (`sourcePts`).

SPEC is one of
  binary:PATH@ENGINE  `PATH reverse MP4 KIND OUTPUT OFFSET` with HARS_REMUX_ENGINE=ENGINE: the
                      product's export (worker processes, checks, fsync and rename included)
  worker:PATH         `PATH __pure-reverse MP4 KIND TICKS OUTPUT`: the pure worker alone, for its
                      own peak RSS

Each command runs as its own child; elapsed is wall time, CPU is user+system from wait4 (the
child and the descendants it reaped), peak RSS is the largest single process of that tree. One
warm-up round, then `--runs` rounds with the implementations interleaved per video, their order
rotated and reversed round by round. Outputs are removed after each run. No paths, clocks or file
names are written to the result.
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import time


def timed(command, env=None):
    start = time.monotonic()
    pid = os.posix_spawn(command[0], command, env if env is not None else os.environ,
                         file_actions=[(os.POSIX_SPAWN_OPEN, 1, os.devnull, os.O_WRONLY, 0),
                                       (os.POSIX_SPAWN_OPEN, 2, os.devnull, os.O_WRONLY, 0)])
    _, status, usage = os.wait4(pid, 0)
    elapsed = time.monotonic() - start
    code = os.waitstatus_to_exitcode(status)
    if code:
        raise RuntimeError(f"command failed ({code}): {command[1]}")
    return elapsed, usage.ru_utime + usage.ru_stime, usage.ru_maxrss


def offset(original):
    data = original.read_bytes()
    pmt = video = None
    best = None
    for i in range(0, len(data) - 187, 188):
        p = data[i:i + 188]
        pid, start, afc = ((p[1] & 31) << 8) | p[2], p[1] & 0x40, (p[3] >> 4) & 3
        at = 5 + p[4] if afc & 2 else 4
        payload = p[at:]
        if not start or not afc & 1:
            continue
        if pid == 0:
            s = payload[1 + payload[0]:]
            pmt = ((s[10] & 31) << 8) | s[11]
        elif pid == pmt:
            s = payload[1 + payload[0]:]
            info = ((s[10] & 15) << 8) | s[11]
            video = ((s[13 + info] & 31) << 8) | s[14 + info]
        elif pid == video and payload[7] & 0x80:
            b = payload[9:14]
            pts = ((b[0] >> 1) & 7) << 30 | b[1] << 22 | (b[2] >> 1) << 15 | b[3] << 7 | b[4] >> 1
            best = pts if best is None else min(best, pts)
    return best


def run_impl(spec, source, kind, output, ticks):
    if spec.startswith("binary:"):
        path, _, engine = spec[len("binary:"):].partition("@")
        env = {k: v for k, v in os.environ.items() if k != "HARS_REMUX_ENGINE"}
        env["HARS_REMUX_ENGINE"] = engine or "pure"
        return list(timed([path, "reverse", str(source), kind, str(output), repr(ticks / 90000)], env=env))
    if spec.startswith("worker:"):
        path = spec[len("worker:"):]
        return list(timed([path, "__pure-reverse", str(source), kind, str(ticks), str(output)]))
    raise SystemExit(f"unknown implementation {spec}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("mp4_dir", type=Path)
    p.add_argument("output", type=Path)
    p.add_argument("--impl", action="append", required=True)
    p.add_argument("--runs", type=int, default=5)
    p.add_argument("--segments", type=int, nargs="+")
    p.add_argument("--scratch", type=Path, required=True)
    args = p.parse_args()
    impls = dict(s.split("=", 1) for s in args.impl)
    args.scratch.mkdir(parents=True, exist_ok=True)
    videos = []
    for mp4 in sorted(args.mp4_dir.glob("*-*.mp4"), key=lambda p: (int(p.name.split("-")[0]), p.name)):
        segment, name = mp4.name.split("-", 1)
        if args.segments and int(segment) not in args.segments:
            continue
        kind = "qcamera.ts" if name == "camera.mp4" else "fcamera.hevc"
        ticks = offset(args.mp4_dir / f"{segment}-qcamera.ts") if kind == "qcamera.ts" else 0
        videos.append((int(segment), kind, mp4, ticks))
    rows = []
    load = [os.getloadavg()]
    names = list(impls)
    for round_ in range(args.runs + 1):
        order = names[round_ % len(names):] + names[:round_ % len(names)]
        if round_ % 2:
            order.reverse()
        for segment, kind, mp4, ticks in videos:
            for name in order:
                output = args.scratch / f"{name}.{'ts' if kind == 'qcamera.ts' else 'hevc'}"
                output.unlink(missing_ok=True)
                elapsed, cpu, rss = run_impl(impls[name], mp4, kind, output, ticks)
                output.unlink(missing_ok=True)
                if round_:
                    rows.append({"impl": name, "segment": segment, "kind": kind, "run": round_,
                                 "elapsed_s": round(elapsed, 4), "cpu_s": round(cpu, 4), "peak_rss_kib": rss})
        load.append(os.getloadavg())
    summary = {}
    for name in impls:
        for kind in ("qcamera.ts", "fcamera.hevc"):
            mine = [r for r in rows if r["impl"] == name and r["kind"] == kind]
            if not mine:
                continue
            per_video = {}
            for r in mine:
                per_video.setdefault(r["segment"], []).append(r)
            summary[f"{name}/{kind}"] = {
                "videos": len(per_video),
                "per_video": {str(s): {
                    "elapsed_s_median": round(statistics.median(r["elapsed_s"] for r in v), 4),
                    "elapsed_s_range": [min(r["elapsed_s"] for r in v), max(r["elapsed_s"] for r in v)],
                    "cpu_s_median": round(statistics.median(r["cpu_s"] for r in v), 4),
                    "peak_rss_kib_max": max(r["peak_rss_kib"] for r in v)} for s, v in sorted(per_video.items())},
                "elapsed_s_median": round(statistics.median(r["elapsed_s"] for r in mine), 4),
                "elapsed_s_range": [min(r["elapsed_s"] for r in mine), max(r["elapsed_s"] for r in mine)],
                "cpu_s_median": round(statistics.median(r["cpu_s"] for r in mine), 4),
                "peak_rss_kib_max": max(r["peak_rss_kib"] for r in mine),
                "peak_rss_kib_median": statistics.median(r["peak_rss_kib"] for r in mine),
            }
    result = {"runs": args.runs, "warmups": 1, "order": "rotated and alternately reversed per round",
              "impls": {k: v.split(":")[0] + ("@" + v.rpartition("@")[2] if "@" in v else "") for k, v in impls.items()},
              "loadavg_1_5_15_per_round": [[round(x, 2) for x in l] for l in load], "cpu_count": os.cpu_count(),
              "summary": summary, "samples": rows}
    args.output.write_text(json.dumps(result, indent=1) + "\n")
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
