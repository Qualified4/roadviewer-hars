"""Audit of the original-format export (MP4 -> qcamera.ts / fcamera.hevc / ecamera.hevc).

    pure_reverse_audit.py BINARY OUT_DIR --original13 CORPUS JOBS [--sample LABEL=MP4 ...]

--original13: every camera original of the 13-segment corpus goes original -> `remux` (pure)
-> `reverse` (pure) and `reverse` (FFmpeg engine, HARS_REMUX_ENGINE=ffmpeg); both exports are
compared with the original. JOBS holds `<segment>-<kind>.times.json` (the logged camera clock).
--sample: an MP4 kept by an earlier conversion (`qcamera.mp4`/`camera.mp4`, `fcamera.mp4`,
`ecamera.mp4` in its name) exported by both engines and compared with each other.

Compared, from this script's own Annex B / MPEG-TS parsers (no FFmpeg): SHA-256 of the
normalized NAL sequence (every unit with its trailing zero bytes removed, in order), frame
count, keyframe positions, TS PTS/DTS per PES, the set of parameter sets, and the file bytes; a
byte difference is classified. The development oracle is the host `ffmpeg -f framemd5` of every
file (decoded frame hashes). The JSON written holds labels only: no path, file name or clock.
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
from pathlib import Path

START = re.compile(b"\x00\x00\x01")


def units(stream):
    """NAL units of an Annex B byte string, each without the zeros that frame it."""
    starts = [m.end() for m in START.finditer(stream)]
    out = []
    for i, begin in enumerate(starts):
        end = starts[i + 1] - 3 if i + 1 < len(starts) else len(stream)
        out.append(stream[begin:end].rstrip(b"\x00"))
    return out


def ts_demux(data):
    """(pes list, packet stats) of the single video stream: pes = dict(pts, dts, rai, data)."""
    assert len(data) % 188 == 0, "partial TS packet"
    pmt_pid = video = None
    pes, current = [], None
    stats = {"packets": 0, "pids": {}, "pcr": 0, "rai": 0}
    for i in range(0, len(data), 188):
        p = data[i:i + 188]
        assert p[0] == 0x47
        pid = ((p[1] & 31) << 8) | p[2]
        start = bool(p[1] & 0x40)
        afc = (p[3] >> 4) & 3
        stats["packets"] += 1
        stats["pids"][hex(pid)] = stats["pids"].get(hex(pid), 0) + 1
        at, rai = 4, False
        if afc & 2:
            length = p[4]
            if length:
                rai = bool(p[5] & 0x40)
                stats["pcr"] += bool(p[5] & 0x10)
                stats["rai"] += rai
            at = 5 + length
        if not afc & 1:
            continue
        payload = p[at:]
        if pid == 0 and start:
            section = payload[1 + payload[0]:]
            pmt_pid = ((section[10] & 31) << 8) | section[11]
        elif pid == pmt_pid and start:
            section = payload[1 + payload[0]:]
            info = ((section[10] & 15) << 8) | section[11]
            j = 12 + info
            end = 3 + (((section[1] & 15) << 8) | section[2]) - 4
            while j < end:
                kind, es = section[j], ((section[j + 1] & 31) << 8) | section[j + 2]
                if kind == 0x1B:
                    video = es
                j += 5 + (((section[j + 3] & 15) << 8) | section[j + 4])
        elif pid == video:
            if start:
                if current:
                    pes.append(current)
                flags, header = payload[7], payload[8]
                def clock(b):
                    return ((b[0] >> 1) & 7) << 30 | b[1] << 22 | (b[2] >> 1) << 15 | b[3] << 7 | b[4] >> 1
                pts = clock(payload[9:14]) if flags & 0x80 else None
                dts = clock(payload[14:19]) if flags & 0x40 else pts
                current = {"pts": pts, "dts": dts, "rai": rai, "data": bytearray(payload[9 + header:])}
            elif current is not None:
                current["data"] += payload
    if current:
        pes.append(current)
    return pes, stats


def describe(path, hevc):
    data = path.read_bytes()
    out = {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    if hevc:
        nals = units(data)
        frames, keyframes = 0, []
        for n in nals:
            kind = (n[0] >> 1) & 63
            if kind < 32 and len(n) > 2 and n[2] & 0x80:
                if 16 <= kind <= 23:
                    keyframes.append(frames)
                frames += 1
        out.update(frames=frames, keyframes=keyframes)
        sets = [n for n in nals if 32 <= ((n[0] >> 1) & 63) <= 34]
    else:
        pes, stats = ts_demux(data)
        nals = [u for p in pes for u in units(bytes(p["data"]))]
        out.update(frames=len(pes), keyframes=[i for i, p in enumerate(pes) if p["rai"]],
                   clocks=[(p["pts"], p["dts"]) for p in pes], ts=stats)
        sets = [n for n in nals if n[0] & 31 in (7, 8)]
    digest = hashlib.sha256()
    for n in nals:
        digest.update(len(n).to_bytes(4, "big") + n)
    out["nal_units"] = len(nals)
    out["normalized_nal_sha256"] = digest.hexdigest()
    out["parameter_sets"] = sorted({hashlib.sha256(s).hexdigest()[:16] for s in sets})
    out["parameter_sets_in_band"] = len(sets)
    out["_raw"] = data
    return out


def framemd5(path, fmt):
    # passthrough: every decoded frame once; the default constant-rate output drops or repeats
    # frames by timestamp, which differs between an MP4 and its raw export.
    lines = subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-f", fmt, "-i", str(path), "-map", "0:v:0",
                            "-fps_mode", "passthrough", "-f", "framemd5", "-"],
                           check=True, capture_output=True, text=True).stdout
    return [l.rsplit(",", 1)[1].strip() for l in lines.splitlines() if l and not l.startswith("#")]


def difference(a, b, hevc):
    """What differs between two files of equal normalized NAL sequence and clocks."""
    if a["sha256"] == b["sha256"]:
        return "identical"
    if a["normalized_nal_sha256"] != b["normalized_nal_sha256"]:
        return "nal_units"
    if not hevc and a["clocks"] != b["clocks"]:
        return "clocks"
    if hevc:
        za = [len(m.group(0)) for m in re.finditer(b"\x00+(?=\x00\x00\x01)", a["_raw"])]
        zb = [len(m.group(0)) for m in re.finditer(b"\x00+(?=\x00\x00\x01)", b["_raw"])]
        changed = sum(1 for x, y in zip(za, zb) if x != y) if len(za) == len(zb) else None
        return f"zero_bytes_between_units({changed} places, {a['bytes'] - b['bytes']:+d} bytes)"
    sa, sb = a["ts"], b["ts"]
    parts = [k for k in ("pids", "pcr", "rai") if sa[k] != sb[k]]
    return "ts_container(" + ",".join(parts or ["layout"]) + f", {a['bytes'] - b['bytes']:+d} bytes)"


def run(binary, args, engine):
    env = {k: v for k, v in os.environ.items() if k != "HARS_REMUX_ENGINE"}
    env["HARS_REMUX_ENGINE"] = engine
    done = subprocess.run([str(binary), *map(str, args)], env=env, capture_output=True, text=True)
    events = []
    for line in done.stderr.splitlines():
        try:
            fields = json.loads(line).get("fields", {})
        except json.JSONDecodeError:
            continue
        if fields.get("event", "").startswith(("remux_", "reverse_")):
            events.append({k: fields.get(k) for k in ("event", "engine", "failure_class", "reason_code")})
    if done.returncode:
        raise RuntimeError(f"{args[0]} failed ({done.returncode}): {events}")
    return events


def compare(entry, files, hevc, fmt):
    """files: name -> path, the first is the reference."""
    described = {n: describe(p, hevc) for n, p in files.items()}
    md5 = {n: framemd5(p, fmt) for n, p in files.items()}
    names = list(files)
    ref = described[names[0]]
    for name in names:
        d = described[name]
        row = {k: v for k, v in d.items() if k not in ("_raw", "clocks", "keyframes", "sha256")}
        row["keyframe_count"] = len(d["keyframes"])
        row["decoded_frames"] = len(md5[name])
        if name != names[0]:
            row["vs_" + names[0]] = {
                "normalized_nal_equal": d["normalized_nal_sha256"] == ref["normalized_nal_sha256"],
                "frames_equal": d["frames"] == ref["frames"],
                "keyframes_equal": d["keyframes"] == ref["keyframes"],
                "parameter_sets_equal": d["parameter_sets"] == ref["parameter_sets"],
                "clocks_equal": None if hevc else d["clocks"] == ref["clocks"],
                "bytes": difference(d, ref, hevc),
                "framemd5_equal": md5[name] == md5[names[0]],
            }
        entry[name] = row
    if "pure" in described and "ffmpeg" in described:
        a, b = described["pure"], described["ffmpeg"]
        entry["pure_vs_ffmpeg"] = {
            "normalized_nal_equal": a["normalized_nal_sha256"] == b["normalized_nal_sha256"],
            "frames_equal": a["frames"] == b["frames"], "keyframes_equal": a["keyframes"] == b["keyframes"],
            "parameter_sets_equal": a["parameter_sets"] == b["parameter_sets"],
            "clocks_equal": None if hevc else a["clocks"] == b["clocks"],
            "bytes": difference(a, b, hevc), "framemd5_equal": md5["pure"] == md5["ffmpeg"],
        }
    return entry


def min_pts(ts):
    pes, _ = ts_demux(ts.read_bytes())
    return min(p["pts"] for p in pes)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("binary", type=Path)
    p.add_argument("out", type=Path)
    p.add_argument("--original13", nargs=2, type=Path, metavar=("CORPUS", "JOBS"))
    p.add_argument("--sample", action="append", default=[])
    args = p.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    work = args.out / "work"
    work.mkdir(exist_ok=True)
    report = {"original13": [], "samples": []}
    if args.original13:
        corpus, jobs = args.original13
        for source in sorted(corpus.glob("*--*--*--*.*"), key=lambda f: (int(f.name.split("--")[2]), f.name)):
            kind = source.name.split("--")[3]
            if kind not in ("qcamera.ts", "fcamera.hevc"):
                continue
            segment = int(source.name.split("--")[2])
            hevc = kind.endswith(".hevc")
            mp4 = work / f"{segment}-{'fcamera' if hevc else 'camera'}.mp4"
            mp4.unlink(missing_ok=True)
            forward = run(args.binary, ["remux", source, kind, jobs / f"{segment}-{kind}.times.json", mp4], "pure")
            offset = 0.0 if hevc else min_pts(source) / 90000
            exports = {"original": source}
            events = {"remux": forward}
            for engine in ("pure", "ffmpeg"):
                out = work / f"{segment}-{engine}.{'hevc' if hevc else 'ts'}"
                out.unlink(missing_ok=True)
                events[engine] = run(args.binary, ["reverse", mp4, kind, out, repr(offset)], engine)
                exports[engine] = out
            entry = compare({"segment": segment, "kind": kind, "events": events}, exports, hevc,
                            "hevc" if hevc else "mpegts")
            report["original13"].append(entry)
            print(json.dumps({k: entry[k] for k in ("segment", "kind")} | {
                "pure": entry["pure"]["vs_original"], "pure_vs_ffmpeg": entry["pure_vs_ffmpeg"]}))
            for f in (mp4, exports["pure"], exports["ffmpeg"]):
                f.unlink()
    for spec in args.sample:
        label, path = spec.split("=", 1)
        path = Path(path)
        name = path.name
        hevc = "qcamera" not in name and "-camera" not in name and not name.startswith("camera")
        kind = "ecamera.hevc" if "ecamera" in name else "fcamera.hevc" if hevc else "qcamera.ts"
        exports = {"mp4": path}
        events = {}
        for engine in ("pure", "ffmpeg"):
            out = work / f"{label}-{engine}.{'hevc' if hevc else 'ts'}"
            out.unlink(missing_ok=True)
            events[engine] = run(args.binary, ["reverse", path, kind, out, "0.0"], engine)
            exports[engine] = out
        described = {n: describe(exports[n], hevc) for n in ("pure", "ffmpeg")}
        md5 = {"mp4": framemd5(path, "mov"), **{n: framemd5(exports[n], "hevc" if hevc else "mpegts")
                                                for n in ("pure", "ffmpeg")}}
        a, b = described["pure"], described["ffmpeg"]
        entry = {"label": label, "kind": kind, "events": events,
                 "pure": {k: v for k, v in a.items() if k not in ("_raw", "clocks", "keyframes", "sha256")},
                 "pure_vs_ffmpeg": {
                     "normalized_nal_equal": a["normalized_nal_sha256"] == b["normalized_nal_sha256"],
                     "frames_equal": a["frames"] == b["frames"], "keyframes_equal": a["keyframes"] == b["keyframes"],
                     "parameter_sets_equal": a["parameter_sets"] == b["parameter_sets"],
                     "clocks_equal": None if hevc else a["clocks"] == b["clocks"],
                     "bytes": difference(a, b, hevc)},
                 "framemd5": {"decoded_frames": {n: len(v) for n, v in md5.items()},
                              "pure_equals_mp4": md5["pure"] == md5["mp4"],
                              "pure_equals_ffmpeg": md5["pure"] == md5["ffmpeg"],
                              "ffmpeg_equals_mp4": md5["ffmpeg"] == md5["mp4"]}}
        report["samples"].append(entry)
        print(json.dumps({"label": label, "kind": kind, "pure_vs_ffmpeg": entry["pure_vs_ffmpeg"],
                          "framemd5": entry["framemd5"]}))
        for n in ("pure", "ffmpeg"):
            exports[n].unlink()
    (args.out / "report.json").write_text(json.dumps(report, indent=1) + "\n")


if __name__ == "__main__":
    main()
