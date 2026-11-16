"""Peak memory of the pure remux worker against input size (synthetic media only).

Builds raw HEVC inputs of about 20, 100 and 400 MB (one x265 encode, repeated
whole: identical parameter sets, a keyframe at each start), the 20 MB input's
frame count at a higher bitrate (bytes without frames), plus hostile inputs
(parameter sets only, 5-byte SEI units only, one 33 MiB NAL unit), runs the
`remux` command of BINARY on each and reads the worker's own peak RSS from its
profile record, and the exact peak of the largest single process (`wait4`;
also set when a build without the worker cap is killed under an external
limit, where the exit is the negative signal). Prints one JSON summary: per
input the size, result and peaks, and the least-squares slope of worker peak
against input size.

usage: python3 integration/pure_remux_memory.py BINARY WORKDIR [--max-mb 400]
WORKDIR must be new; it is removed at the end unless --keep.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import time


def encode(path, frames, rate="5300k"):
    subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i",
                    "testsrc2=size=1280x720:rate=20,noise=alls=30:allf=t", "-frames:v", str(frames), "-an",
                    "-c:v", "libx265", "-preset", "ultrafast", "-b:v", rate, "-x265-params",
                    "bframes=0:keyint=40:min-keyint=40:log-level=error", "-pix_fmt", "yuv420p", "-f", "hevc", str(path)],
                   check=True)


def run(binary, source, frames, out):
    clock = out / (source.name + ".times.json")
    clock.write_text(json.dumps([i * 0.05 for i in range(frames)]))
    output = out / (source.name + ".mp4")
    profile = out / (source.name + ".profile")
    env = {k: v for k, v in os.environ.items() if not k.startswith("HARS_")}
    env["HARS_PROFILE_FILE"] = str(profile)
    started = time.monotonic()
    errors = out / (source.name + ".stderr")
    with errors.open("w") as stderr:
        child = subprocess.Popen([str(binary), "remux", str(source), "fcamera.hevc", str(clock), str(output)],
                                 env=env, stdout=subprocess.DEVNULL, stderr=stderr, text=True)
        # wait4: the exact peak RSS of the largest single process (the CLI or its reaped worker).
        _, status, usage = os.wait4(child.pid, 0)
    elapsed = time.monotonic() - started
    proc = subprocess.CompletedProcess(child.args, os.waitstatus_to_exitcode(status), "", errors.read_text())
    errors.unlink()
    worker = 0
    if profile.exists():
        for line in profile.read_text().splitlines():
            stages = json.loads(line)["stages"]
            if any(k.startswith("pure_") for k in stages):
                worker = max(v["cumulative_peak_rss_kib"] for v in stages.values())
    events = []
    for line in proc.stderr.splitlines():
        try:
            events.append(json.loads(line)["fields"])
        except (ValueError, KeyError, TypeError):
            pass
    failure = next((e for e in events if e.get("event") in ("remux_failure", "remux_fallback")), None)
    if output.exists():
        output.unlink()
    return {"input_mb": round(source.stat().st_size / 1e6, 1), "frames": frames, "ok": proc.returncode == 0,
            "exit": proc.returncode, "failure_class": failure and failure.get("failure_class"),
            "reason_code": failure and failure.get("reason_code"), "fallback": failure and failure.get("fallback"),
            "elapsed_seconds": round(elapsed, 3), "worker_peak_rss_kib": worker,
            "largest_process_peak_rss_kib": usage.ru_maxrss}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("binary", type=Path)
    p.add_argument("workdir", type=Path)
    p.add_argument("--max-mb", type=int, default=400)
    p.add_argument("--keep", action="store_true")
    args = p.parse_args()
    args.workdir.mkdir(parents=True, exist_ok=False)
    try:
        base = args.workdir / "base.hevc"
        frames = 600
        encode(base, frames)
        size = base.stat().st_size
        results = {"scaling": [], "hostile": []}
        # Inputs are streamed to disk, never held here: a child's peak RSS
        # (wait4) includes this process's pages until its exec.
        for repeat in (1, 5, 20):
            if size * repeat / 1e6 > args.max_mb * 1.2:
                continue
            source = args.workdir / f"x{repeat}.hevc"
            with source.open("wb") as f:
                for _ in range(repeat):
                    with base.open("rb") as b:
                        shutil.copyfileobj(b, f)
            results["scaling"].append(run(args.binary, source, frames * repeat, args.workdir))
            source.unlink()
        # Same frame count, several times the bytes: separates the per-frame
        # sample table from the input size.
        dense = args.workdir / "dense.hevc"
        encode(dense, frames, "40000k")
        results["same_frames_more_bytes"] = run(args.binary, dense, frames, args.workdir)
        dense.unlink()
        def repeated(unit, total):
            block = unit * ((1 << 20) // len(unit))

            def write(f):
                for _ in range(total // len(block)):
                    f.write(block)
            return write

        def big_nal(f):
            with base.open("rb") as b:
                f.write(b.read(size // 20))
            f.write(b"\0\0\1\x02\1")
            for _ in range(33):
                f.write(b"\x55" * (1 << 20))
        hostile = {
            "parameter_sets_only": repeated(b"\0\0\1\x40\1", 100_000_000),
            "sei_units_only": repeated(b"\0\0\1\x4e\1", 100_000_000),
            "one_33_mib_nal": big_nal,
        }
        for name, write in hostile.items():
            source = args.workdir / f"{name}.hevc"
            with source.open("wb") as f:
                write(f)
            row = run(args.binary, source, frames, args.workdir)
            row["input"] = name
            results["hostile"].append(row)
            source.unlink()
        xs = [r["input_mb"] for r in results["scaling"]]
        ys = [r["worker_peak_rss_kib"] / 1024 for r in results["scaling"]]
        if len(xs) > 1:
            mx, my = sum(xs) / len(xs), sum(ys) / len(ys)
            results["slope_mib_per_input_mb"] = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sum((x - mx) ** 2 for x in xs)
        print(json.dumps(results, indent=1))
    finally:
        if not args.keep:
            shutil.rmtree(args.workdir, ignore_errors=True)


if __name__ == "__main__":
    main()
