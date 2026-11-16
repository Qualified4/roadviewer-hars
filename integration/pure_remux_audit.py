"""Audit converted camera MP4s against the Python outputs and the originals.

For every segment: packet PTS/DTS, keyframe flags, payload SHA-256 and
durations against the Python MP4; decoder configuration bytes; normalized
all-NAL SHA against the original (nal_inventory); full decode frame MD5s and
error lines (system ffmpeg); stream duration against terminal_rule's model;
top-level box layout. Prints one JSON summary without paths or clocks.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

import av

sys.path.insert(0, str(Path(__file__).parent))
import nal_inventory  # noqa: E402
import terminal_rule  # noqa: E402

SCHEMA = Path(__file__).resolve().parents[1] / "reference/python/roadviewer/schema/cereal/log.capnp"
PAIRS = (("qcamera", "camera.mp4", "qcamera.ts"), ("front", "fcamera.mp4", "fcamera.hevc"))


def packets(path):
    with av.open(str(path)) as c:
        s = c.streams.video[0]
        rows = [(p.pts, p.dts, int(p.is_keyframe), p.duration, hashlib.sha256(bytes(p)).hexdigest())
                for p in c.demux(s) if p.size]
        return rows, bytes(s.codec_context.extradata or b""), s.duration, str(s.time_base)


def decode(path):
    out = subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-i", str(path), "-map", "0:v:0", "-f", "framemd5", "-"],
                         capture_output=True, text=True, check=True)
    return [line.split(",")[-1].strip() for line in out.stdout.splitlines() if not line.startswith("#")], len(out.stderr.splitlines())


def boxes(path):
    out, at, size = [], 0, path.stat().st_size
    with path.open("rb") as f:
        while at < size:
            f.seek(at);head = f.read(16)
            n = int.from_bytes(head[:4], "big");kind = head[4:8].decode("ascii")
            n = int.from_bytes(head[8:16], "big") if n == 1 else (size - at if n == 0 else n)
            out.append(kind);at += n
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("corpus", type=Path);p.add_argument("hars", type=Path);p.add_argument("oracle", type=Path)
    p.add_argument("--segments", type=int, nargs="*", default=list(range(1, 14)))
    args = p.parse_args()
    rows = []
    for n in args.segments:
        log = next(args.corpus.glob(f"*--{n}--rlog.zst"))
        model = terminal_rule.model(log, {k: log.with_name(log.name.replace("rlog.zst", k)) for k in ("qcamera.ts", "fcamera.hevc")}, SCHEMA)
        for camera, mp4, original in PAIRS:
            ours, theirs = args.hars / f"segment-{n}" / mp4, args.oracle / f"segment-{n}" / "python" / mp4
            a, ea, da, tba = packets(ours)
            b, eb, db, tbb = packets(theirs)
            md5a, erra = decode(ours)
            md5b, _ = decode(theirs)
            scale = 90000 if mp4 == "camera.mp4" else 1_000_000
            source = log.with_name(log.name.replace("rlog.zst", original))
            rows.append({
                "segment": n, "camera": camera, "packets": len(a), "python_packets": len(b),
                "pts_dts_equal": [x[:2] for x in a] == [x[:2] for x in b],
                "keyframes_equal": [x[2] for x in a] == [x[2] for x in b], "keyframes": sum(x[2] for x in a),
                "payload_sha_equal": [x[4] for x in a] == [x[4] for x in b],
                "durations_equal_except_last": [x[3] for x in a[:-1]] == [x[3] for x in b[:-1]],
                "last_duration": a[-1][3], "python_last_duration": b[-1][3],
                "decoder_configuration_equal": ea == eb,
                "time_base": tba, "python_time_base": tbb,
                "duration_ts_matches_model": abs((da / scale) - model[camera]["hars"]) < 1e-9,
                "python_duration_ts_matches_model": abs((db / scale) - model[camera]["python"]) < 1e-9,
                "model_basis": model[camera]["basis"],
                "all_nal_sha_equal_original": nal_inventory.inspect(ours)["all_nal_sha256"] == nal_inventory.inspect(source)["all_nal_sha256"],
                "decoded_frames": len(md5a), "decode_error_lines": erra, "frame_md5_equal_python": md5a == md5b,
                "boxes": boxes(ours), "python_boxes": boxes(theirs),
            })
            print(json.dumps({k: rows[-1][k] for k in ("segment", "camera", "pts_dts_equal", "payload_sha_equal", "decoder_configuration_equal", "all_nal_sha_equal_original", "frame_md5_equal_python", "decode_error_lines")}), flush=True)
    checks = [k for k, v in rows[0].items() if isinstance(v, bool)]
    summary = {c: {k: sum(1 for r in rows if r["camera"] == c and r[k]) for k in checks} for c, _, _ in PAIRS}
    for c, _, _ in PAIRS:
        mine = [r for r in rows if r["camera"] == c]
        summary[c].update({"videos": len(mine), "packets": sorted({r["packets"] for r in mine}), "decoded_frames": sorted({r["decoded_frames"] for r in mine}),
                           "decode_error_lines": sum(r["decode_error_lines"] for r in mine), "keyframes": sum(r["keyframes"] for r in mine),
                           "boxes": sorted({",".join(r["boxes"]) for r in mine}), "python_boxes": sorted({",".join(r["python_boxes"]) for r in mine}),
                           "last_duration_pairs": [[r["segment"], r["python_last_duration"], r["last_duration"]] for r in mine]})
    print(json.dumps({"summary": summary}, indent=1))


if __name__ == "__main__":main()
