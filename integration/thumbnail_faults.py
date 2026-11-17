"""The 40 thumbnail failure cases of docs/hars/THUMBNAIL-DECODER.md, run through two `__thumbnail`
workers under an address-space limit, compared case by case.

    thumbnail_faults.py QCAMERA_MP4 FCAMERA_MP4 WORK BASE_BINARY NEW_BINARY

QCAMERA_MP4 is a converted H.264 camera.mp4, FCAMERA_MP4 a HEVC fcamera.mp4 (both read only).
The damaged inputs are derived from them with fixed seeds; the codec cases are encoded by the
host FFmpeg (a test tool). WORK receives the inputs and `thumbnail-faults.json`; it is removed
except for that report unless --keep is given.
"""
import argparse
import json
import random
import re
import shutil
import struct
import subprocess
from pathlib import Path

LIMIT = 2147483648


def boxes(data, start, end):
    at = start
    while at + 8 <= end:
        size, kind = struct.unpack(">I4s", data[at:at + 8])
        header = 8
        if size == 1:
            size = struct.unpack(">Q", data[at + 8:at + 16])[0]
            header = 16
        elif size == 0:
            size = end - at
        yield kind.decode("latin1"), at, header, size
        at += size


def find(data, path, start=0, end=None):
    """(offset, header, size) of the first box along `path` ("moov/trak/mdia")."""
    end = len(data) if end is None else end
    first, *rest = path.split("/")
    for kind, at, header, size in boxes(data, start, end):
        if kind == first:
            if not rest:
                return at, header, size
            skip = {"stsd": 8, "avc1": 78, "hvc1": 78}.get(kind, 0)
            return find(data, "/".join(rest), at + header + skip, at + size)
    raise KeyError(path)


STBL = "moov/trak/mdia/minf/stbl"


def patched(data, path, offset, fmt, value):
    out = bytearray(data)
    at, header, _ = find(data, path)
    struct.pack_into(fmt, out, at + header + offset, value)
    return bytes(out)


def corrupt(data, seed, count):
    at, header, size = find(data, "mdat")
    rng = random.Random(seed)
    out = bytearray(data)
    for _ in range(count):
        i = rng.randrange(at + header + 64, at + size)
        out[i] = rng.randrange(256)
    return bytes(out)


def grow_moov(data, extra):
    """A `free` box of `extra` bytes appended inside `moov` (moov before mdat, offsets shifted)."""
    at, header, size = find(data, "moov")
    free = struct.pack(">I4s", extra, b"free") + bytes(extra - 8)
    moov = bytearray(data[at:at + size] + free)
    struct.pack_into(">I", moov, 0, size + extra)
    for path, wide in (("trak/mdia/minf/stbl/stco", False), ("trak/mdia/minf/stbl/co64", True)):
        try:
            o, h, _ = find(bytes(moov), path, 8, size)
        except KeyError:
            continue
        n = struct.unpack(">I", moov[o + h + 4:o + h + 8])[0]
        for i in range(n):
            p = o + h + 8 + i * (8 if wide else 4)
            fmt = ">Q" if wide else ">I"
            struct.pack_into(fmt, moov, p, struct.unpack(fmt, moov[p:p + struct.calcsize(fmt)])[0] + extra)
    return data[:at] + bytes(moov) + data[at + size:]


def encode(path, size, seconds, video, extra=()):
    cmd = ["ffmpeg", "-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i",
           f"testsrc2=size={size}:rate=20", "-t", str(seconds), "-an", "-threads", "1", *video, *extra,
           "-movflags", "+faststart", str(path)]
    subprocess.run(cmd, check=True)


X264 = ["-c:v", "libx264", "-preset", "ultrafast", "-bf", "0"]
X265 = ["-c:v", "libx265", "-preset", "ultrafast", "-tag:v", "hvc1"]


def x265(params):
    return X265 + ["-x265-params", "pools=1:frame-threads=1:log-level=error" + params]


def cases(q, f, work):
    rng = random.Random(1)
    out = {}
    for i, pct in enumerate((1, 20, 90)):
        out[f"01-{2 * i}-q-truncated-{pct}"] = q[:len(q) * pct // 100]
    for i, pct in enumerate((5, 70)):
        out[f"02-{2 * i}-f-truncated-{pct}"] = f[:len(f) * pct // 100]
    out["03-empty"] = b""
    at, _, size = find(q, "moov")
    out["04-moov-only"] = q[:at + size]
    out["05-empty-mdat"] = q[:at + size] + struct.pack(">I4s", 8, b"mdat")
    for i in range(3):
        out[f"06-{i}-q-corrupt-nal"] = corrupt(q, 10 + i, 40)
    for i in range(2):
        out[f"07-{i}-f-corrupt-nal"] = corrupt(f, 20 + i, 400)
    entry = STBL + "/stsd/avc1"
    out["08-q-declared-8192x4320"] = patched(patched(q, entry, 24, ">H", 8192), entry, 26, ">H", 4320)
    out["09-q-stsz-count-huge"] = patched(q, STBL + "/stsz", 8, ">I", 0xFFFFFFF0)
    out["10-q-sample-size-huge"] = patched(q, STBL + "/stsz", 12, ">I", 0x7FFFFFFF)
    out["11-q-chunk-past-eof"] = patched(q, STBL + "/stco", 8, ">I", len(q) + 4096)
    out["12-q-avcc-no-sps"] = patched(q, entry + "/avcC", 5, ">B", 0xE0)
    hvcc_at, hvcc_header, hvcc_size = find(f, STBL + "/stsd/hvc1/hvcC")
    garbage = bytearray(f)
    garbage[hvcc_at + hvcc_header:hvcc_at + hvcc_size] = bytes(rng.randrange(256) for _ in range(hvcc_size - hvcc_header))
    out["13-f-hvcc-garbage"] = bytes(garbage)
    tag_at, _, _ = find(q, entry)
    wrong = bytearray(q)
    wrong[tag_at + 4:tag_at + 8] = b"xvid"
    out["14-q-wrong-codec-tag"] = bytes(wrong)
    out["15-q-moov-size-overflow"] = grow_moov(q, 17 * 1024 * 1024)
    trak_at, _, trak_size = find(q, "moov/trak")
    out["16-q-trak-larger-than-moov"] = patched(q, "moov/trak", -8, ">I", trak_size + 4096)
    out["17-random-garbage"] = bytes(rng.randrange(256) for _ in range(1 << 20))
    mdat_at, mdat_header, mdat_size = find(q, "mdat")
    zero = bytearray(q)
    zero[mdat_at + mdat_header:mdat_at + mdat_size] = bytes(mdat_size - mdat_header)
    out["18-q-zero-mdat"] = bytes(zero)
    out["19-q-timescale-zero"] = patched(q, "moov/trak/mdia/mdhd", 12, ">I", 0)
    out["20-q-stsc-bad-first-chunk"] = patched(q, STBL + "/stsc", 8, ">I", 2)
    encoded = {
        "21-h264-4096x2304": ("4096x2304", 2, X264, ["-pix_fmt", "yuv420p"]),
        "22-h264-high444": ("526x330", 7, X264, ["-pix_fmt", "yuv444p"]),
        "23-h264-high10": ("526x330", 7, X264, ["-pix_fmt", "yuv420p10le"]),
        "24-h264-high422": ("526x330", 7, X264, ["-pix_fmt", "yuv422p"]),
        "25-h264-interlaced": ("528x336", 7, X264, ["-pix_fmt", "yuv420p", "-flags", "+ildct+ilme", "-x264-params", "interlaced=1"]),
        "26-hevc-main10": ("526x330", 7, x265(""), ["-pix_fmt", "yuv420p10le"]),
        "27-hevc-444": ("526x330", 7, x265(""), ["-pix_fmt", "yuv444p"]),
        "28-h264-bframes": ("526x330", 7, ["-c:v", "libx264", "-preset", "ultrafast", "-bf", "2", "-x264-params", "b-adapt=0"], ["-pix_fmt", "yuv420p"]),
        "30-hevc-bframes": ("526x330", 7, x265(":bframes=2:b-adapt=0"), ["-pix_fmt", "yuv420p"]),
        "31-hevc-full-range": ("526x330", 7, x265(":range=full"), ["-pix_fmt", "yuvj420p", "-color_range", "pc"]),
        "32-h264-full-range": ("526x330", 7, X264, ["-pix_fmt", "yuvj420p", "-color_range", "pc"]),
        "37-hevc-4096x2160": ("4096x2160", 2, x265(":bframes=0"), ["-pix_fmt", "yuv420p"]),
    }
    for name, (size, seconds, video, extra) in encoded.items():
        path = work / f"{name}.mp4"
        if not path.exists():
            encode(path, size, seconds, video, extra)
        out[name] = path.read_bytes()
    out["39-h264-bframes-truncated"] = out["28-h264-bframes"][:len(out["28-h264-bframes"]) * 60 // 100]
    at, _, size = find(f, "moov")
    out["40-f-moov-only"] = f[:at + size]
    return out


def run(binary, source, out_dir):
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    output = out_dir / "thumbnail.jpg"
    cmd = ["prlimit", f"--as={LIMIT}", "/usr/bin/time", "-f", "TIME %e %M", "timeout", "180",
           str(binary), "__thumbnail", str(source), str(output)]
    r = subprocess.run(cmd, capture_output=True, text=True)
    timing = re.findall(r"TIME ([0-9.]+) ([0-9]+)", r.stderr)
    seconds, rss = (float(timing[-1][0]), int(timing[-1][1])) if timing else (None, None)
    files = sorted(p.name for p in out_dir.iterdir())
    made = r.returncode == 0
    return {
        "exit": r.returncode,
        "panic": "panicked" in r.stderr,
        "killed": r.returncode in (124, 137, -9) or "Killed" in r.stderr,
        "result": json.loads(r.stdout) if made and r.stdout.strip() else None,
        "error": next((l for l in r.stderr.splitlines() if l.startswith("Error")), None),
        "outputs_left": [] if made else files,
        "jpeg": output.read_bytes() if made and output.exists() else None,
        "peak_rss_kib": rss,
        "seconds": seconds,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("qcamera", type=Path)
    parser.add_argument("fcamera", type=Path)
    parser.add_argument("work", type=Path)
    parser.add_argument("base", type=Path)
    parser.add_argument("new", type=Path)
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    inputs = args.work / "inputs"
    inputs.mkdir(parents=True, exist_ok=True)
    rows = []
    for name, data in cases(args.qcamera.read_bytes(), args.fcamera.read_bytes(), inputs).items():
        source = inputs / f"case-{name}.mp4"
        source.write_bytes(data)
        row = {"case": name}
        for side in ("base", "new"):
            row[side] = run(getattr(args, side), source, args.work / "out" / name / side)
        jpegs = [row[s].pop("jpeg") for s in ("base", "new")]
        row["same_outcome"] = (row["base"]["exit"] == 0) == (row["new"]["exit"] == 0)
        row["same_result"] = row["base"]["result"] == row["new"]["result"] and jpegs[0] == jpegs[1]
        rows.append(row)
        source.unlink()
        print(name, row["same_outcome"], row["same_result"], row["new"]["exit"], row["new"]["error"] or row["new"]["result"], flush=True)
    summary = {
        "limit": f"prlimit --as={LIMIT} per worker run",
        "cases": len(rows),
        "same_outcome": sum(r["same_outcome"] for r in rows),
        "same_result_and_jpeg": sum(r["same_result"] for r in rows),
        **{k: {s: sum(bool(r[s][k]) for r in rows) for s in ("base", "new")} for k in ("panic", "killed", "outputs_left")},
        "made": {s: sum(r[s]["exit"] == 0 for r in rows) for s in ("base", "new")},
        "peak_rss_kib_max": {s: max(r[s]["peak_rss_kib"] or 0 for r in rows) for s in ("base", "new")},
        "seconds_max": {s: max(r[s]["seconds"] or 0 for r in rows) for s in ("base", "new")},
    }
    print(json.dumps(summary))
    (args.work / "thumbnail-faults.json").write_text(json.dumps({"summary": summary, "rows": rows}, indent=1))
    if not args.keep:
        shutil.rmtree(inputs)
        shutil.rmtree(args.work / "out")


if __name__ == "__main__":
    main()
