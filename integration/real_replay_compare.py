"""Read-only real-corpus frame oracle; raw coordinates never enter its report.

This checks frame analysis, not yet the whole upload/conversion pipeline. Python
also prepares the retained MP4 inputs, whereas analyze-log projects the log only.
Consequently its timeline is compared relative to the first model frame; the
absolute video-aligned timeline must be checked by the full conversion test.
Run under the comparison venv in WSL; outputs and private data stay outside Git.
"""
import argparse
from collections import Counter
import gzip
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time


def differences(expected, actual, path="", errors=None):
    errors = Counter() if errors is None else errors
    if isinstance(expected, dict) and isinstance(actual, dict):
        for key in expected.keys() | actual.keys():
            child = f"{path}.{key}"
            if key not in expected or key not in actual:
                errors[child + ":missing"] += 1
            else:
                differences(expected[key], actual[key], child, errors)
    elif isinstance(expected, list) and isinstance(actual, list):
        if len(expected) != len(actual):
            errors[path + ":length"] += 1
        for a, b in zip(expected, actual):
            differences(a, b, path + "[]", errors)
    elif (type(expected) in (int, float) and type(actual) in (int, float)):
        # Smallest saved geometry/clock precision is 1e-6. Other fields retain
        # their original precision, so no relative tolerance hides large errors.
        if not math.isfinite(expected) or not math.isfinite(actual) or abs(expected - actual) > 1e-6:
            errors[path] += 1
    elif type(expected) is not type(actual) or expected != actual:
        errors[path] += 1
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("corpus", type=Path)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--limit", type=int, default=1)
    parser.add_argument("--offset", type=int, default=0)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    oracle = output / "python-reference"
    # English module/schema paths avoid the documented pycapnp path crash.
    if not oracle.exists():
        shutil.copytree(root / "reference/python/roadviewer", oracle)
    logs = sorted(args.corpus.glob("*--rlog.zst"), key=lambda p: int(p.name.split("--")[-2]))[args.offset:args.offset + args.limit]
    assert logs, "no rlogs in corpus"
    report = output / "frame-comparison.json"
    rows = json.loads(report.read_text()) if args.offset and report.exists() else []
    for source in logs:
        number = int(source.name.split("--")[-2])
        work = output / f"segment-{number}"
        work.mkdir(exist_ok=True)
        env = dict(os.environ, RV_PREPARED_DIR=str(work / "python"), RV_MEDIA_PATHS=json.dumps({
            "camera.mp4": str(source.with_name(source.name.replace("rlog.zst", "qcamera.mp4"))),
            "fcamera.mp4": str(source.with_name(source.name.replace("rlog.zst", "fcamera.mp4"))),
            "ecamera.mp4": str(source.with_name(source.name.replace("rlog.zst", "ecamera.mp4"))),
        }))
        started = time.monotonic()
        with (work / "python.log").open("wb") as log:
            subprocess.run([sys.executable, str(oracle / "app/decoder.py"), str(source), f"segment-{number}"], env=env, stdout=log, stderr=log, check=True, timeout=600)
        python_seconds = time.monotonic() - started
        started = time.monotonic()
        with (work / "rust.log").open("wb") as log:
            subprocess.run([str(args.binary), "analyze-log", str(source), str(work / "rust.json.gz")], stdout=log, stderr=log, check=True, timeout=600)
        rust_seconds = time.monotonic() - started
        with gzip.open(work / "python/data.json.gz", "rt") as f:
            expected = json.load(f)
        with gzip.open(work / "rust.json.gz", "rt") as f:
            actual = json.load(f)
        e_start, a_start = expected["frames"][0]["t"], actual["frames"][0]["t"]
        for frames, start in ((expected["frames"], e_start), (actual["frames"], a_start)):
            for frame in frames:
                frame["t"] = round(frame["t"] - start, 6)
        keys = ("frames", "counts", "replayV", "laneX", "cameraInfos")
        errors = differences({k: expected[k] for k in keys if k in expected}, {k: actual[k] for k in keys if k in actual})
        row = {"segment": number, "frames": len(actual["frames"]), "differences": dict(errors),
               "python_warnings": expected["warnings"], "python_cameras": list(expected["videos"]),
               "python_full_conversion_seconds": round(python_seconds, 3), "rust_frame_analysis_seconds": round(rust_seconds, 3),
               "performance_comparable": False}
        rows.append(row)
        report.write_text(json.dumps(rows, indent=2, ensure_ascii=False))
        print(json.dumps(row, ensure_ascii=False), flush=True)
    return int(any(row["differences"] for row in rows))


if __name__ == "__main__":
    raise SystemExit(main())
