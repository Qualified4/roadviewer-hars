"""Compare full conversion outputs against cached, unmodified Python outputs.

The path/mtime cache key is intentionally implementation-specific. No other
replay, telemetry, track or summary field is excluded. Thumbnail measurements
are reported separately because the Python wheel and native system FFmpeg use
different codec/scaler versions; this script does not declare them equivalent.
"""
import argparse
import gzip
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
from real_replay_compare import differences


def load(path):
    with (gzip.open(path, "rt") if path.suffix == ".gz" else path.open()) as source:
        return json.load(source)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("corpus",type=Path)
    parser.add_argument("binary",type=Path)
    parser.add_argument("oracle",type=Path)
    parser.add_argument("output",type=Path)
    parser.add_argument("--limit",type=int,default=14)
    parser.add_argument("--offset",type=int,default=0)
    parser.add_argument("--prepare-python",action="store_true")
    args=parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    report_path=args.output/"conversion-comparison.json"
    report=json.loads(report_path.read_text()) if report_path.exists() else []
    for log in sorted(args.corpus.glob("*--rlog.zst"),key=lambda p:int(p.name.split("--")[-2]))[args.offset:args.offset+args.limit]:
        segment=int(log.name.split("--")[-2])
        expected=args.oracle/f"segment-{segment}"/"python"
        actual=args.output/f"segment-{segment}"
        python_seconds=None
        if args.prepare_python and not (expected/"summary.json").exists():
            oracle=args.oracle/"python-reference"
            if not oracle.exists():
                shutil.copytree(Path(__file__).resolve().parents[1]/"reference/python/roadviewer",oracle)
            expected.parent.mkdir(parents=True,exist_ok=True)
            env=dict(os.environ,RV_PREPARED_DIR=str(expected),RV_MEDIA_PATHS=json.dumps({
                kind:str(log.with_name(log.name.replace("rlog.zst",filename))) for kind,filename in (
                    ("camera.mp4","qcamera.mp4"),("fcamera.mp4","fcamera.mp4"),("ecamera.mp4","ecamera.mp4"))}))
            started=time.monotonic()
            with (expected.parent/"python.log").open("wb") as out:
                subprocess.run([sys.executable,str(oracle/"app/decoder.py"),str(log),f"segment-{segment}"],env=env,stdout=out,stderr=out,check=True,timeout=600)
            python_seconds=round(time.monotonic()-started,3)
        started=time.monotonic()
        with (args.output/f"segment-{segment}.log").open("wb") as out:
            subprocess.run([str(args.binary),"convert-log",str(log),str(actual),f"segment-{segment}"],stdout=out,stderr=out,check=True,timeout=600)
        result={"segment":segment,"rust_conversion_seconds":round(time.monotonic()-started,3),"differences":{}}
        if python_seconds is not None:result["python_conversion_seconds"]=python_seconds
        for name in ("data.json.gz","telemetry.json.gz","summary.json","track.json.gz"):
            if not (expected/name).exists() and not (actual/name).exists():continue
            if not (expected/name).exists() or not (actual/name).exists():
                result["differences"][name]={"file:missing":1};continue
            a,b=load(expected/name),load(actual/name)
            if name=="data.json.gz":
                a.pop("key");b.pop("key")
                result["frames"]=len(b["frames"])
                result["cameras"]=list(b["videos"])
            errors=differences(a,b)
            if errors:result["differences"][name]=dict(errors)
        result["thumbnail_metadata"]={"python":load(expected/"thumbnail.json"),"rust":load(actual/"thumbnail.json")}
        result["thumbnail_pixel_equivalence_validated"]=False
        report.append(result)
        report_path.write_text(json.dumps(report,indent=2,ensure_ascii=False))
        print(json.dumps(result,ensure_ascii=False),flush=True)
    return int(any(r["differences"] for r in report))


if __name__=="__main__":raise SystemExit(main())
