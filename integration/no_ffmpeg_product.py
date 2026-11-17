#!/usr/bin/env python3
"""The product path on a server whose PATH holds no media tool, with real segments and inputs the
pure engine refuses.

    no_ffmpeg_product.py CORPUS BINARY NEW_OUT --ports UI DEVICE --path NO_FFMPEG_DIR [--segments 3 7]

The server (HARS_DATA under NEW_OUT, HARS_INGRESS_ONLY=0, PATH=NO_FFMPEG_DIR) converts uploaded
segments automatically with original retention off, so a converted camera's original goes and
`/original/<kind>` is the export. Checked for each normal segment: ready, Range playback of both
cameras, the thumbnail, `/original/<kind>` and the ZIP of original files. For each refused input
(made here by the host's development ffmpeg from a corpus segment): the record's state, its
warning, which cameras play, and which uploaded originals are still stored, unchanged.
The report holds labels, counts, states and booleans only.
"""
import argparse
import hashlib
import io
import json
import subprocess
import sys
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pure_default_product as product  # noqa: E402


def digest(data):
    return hashlib.sha256(data).hexdigest()


def variant(work, files, kind, how):
    """A copy of the segment with its `kind` file replaced by a refused variant."""
    source = next(f for f in files if f.name.endswith("--" + kind))
    out = work / source.name
    data = source.read_bytes()
    if how == "bframes":
        subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-y", "-copyts", "-i", str(source), "-map", "0:v:0",
                        "-c:v", "libx264", "-bf", "2", "-g", "20", "-fps_mode", "passthrough",
                        "-enc_time_base:v", "1:90000", "-muxdelay", "0", "-f", "mpegts", str(out)],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    elif how in ("lost_packet", "scrambled"):
        pids = [((data[i + 1] & 0x1f) << 8) | data[i + 2] for i in range(0, len(data) - 187, 188)]
        video = max(set(pids), key=pids.count)
        packets, seen = [], 0
        for i in range(0, len(data), 188):
            packet = bytearray(data[i:i + 188])
            if pids[i // 188] == video:
                seen += 1
                if seen == 500 and how == "lost_packet":
                    continue
                if seen == 500:
                    packet[3] |= 0x80
            packets.append(bytes(packet))
        out.write_bytes(b"".join(packets))
    elif how == "joined_sets":
        other = work / "other.hevc"
        subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "testsrc2=size=176x96:rate=20",
                        "-frames:v", "20", "-an", "-c:v", "libx265", "-x265-params",
                        "pools=1:frame-threads=1:bframes=0:keyint=20:log-level=error", "-f", "hevc", str(other)],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        out.write_bytes(data + other.read_bytes())
        other.unlink()
    return [out if f is source else f for f in files]


def playback(server, record, camera):
    """Range playback of a camera's MP4 against the whole download."""
    _, _, whole = server.request(f"/api/logs/{record}/download?source={camera}&format=mp4", raw=True, full=True)
    status, headers, head = server.request(f"/api/logs/{record}/video?source={camera}", raw=True, full=True,
                                           headers={"Range": "bytes=0-1023"})
    status2, _, tail = server.request(f"/api/logs/{record}/video?source={camera}", raw=True, full=True,
                                      headers={"Range": f"bytes={len(whole) - 1024}-"})
    return {"range206": status == 206 and status2 == 206, "mp4": whole[4:8] == b"ftyp",
            "range_bytes_equal": head == whole[:1024] and tail == whole[-1024:],
            "content_range": headers.get("Content-Range", "").startswith("bytes 0-1023/")}


def check_ready(server, record, files):
    row, states = server.wait(record, ("ready", "error"))
    meta = json.loads((server.data / record / "meta.json").read_text())
    stored = {k: digest((server.data / record / k).read_bytes()) for k in product.KINDS
              if (server.data / record / k).is_file()}
    uploaded = {f.name.split("--")[-1]: digest(f.read_bytes()) for f in files}
    return {"states": states, "status": row["status"], "cameras": sorted(meta.get("videos", {})),
            "warnings": meta.get("warnings", []), "error": meta.get("error"),
            "originals": {k: ("unchanged" if stored.get(k) == v else "gone" if k not in stored else "CHANGED")
                          for k, v in uploaded.items()},
            "events": [e for e in server.events(record) if e["event"] != "remux_engine"]}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("corpus", type=Path)
    p.add_argument("binary", type=Path)
    p.add_argument("out", type=Path)
    p.add_argument("--ports", type=int, nargs=2, required=True)
    p.add_argument("--path", required=True)
    p.add_argument("--segments", type=int, nargs="+", default=[3, 7])
    p.add_argument("--refused-segments", type=int, nargs=4, default=[9, 10, 11, 12])
    a = p.parse_args()
    a.out.mkdir(parents=True)
    work = a.out / "work"
    work.mkdir()
    server = product.Server(a.binary, a.out, a.ports, env={"PATH": a.path})
    report = {"segments": [], "refused": {}}
    try:
        server.request("/api/settings/processing", "POST",
                       {"auto_convert": True, "concurrency": 1, "keep_original_video": False})
        for segment in a.segments:
            files = product.segment_files(a.corpus, segment)
            record = product.upload(server, files)
            entry = check_ready(server, record, files)
            for camera in ("qcamera", "front"):
                entry[camera] = playback(server, record, camera)
            status, headers, thumb = server.request(f"/api/logs/{record}/thumbnail", raw=True, full=True)
            entry["thumbnail"] = {"status": status, "jpeg": thumb[:2] == b"\xff\xd8", "bytes": len(thumb)}
            originals = {}
            for kind in ("qcamera.ts", "fcamera.hevc"):
                status, _, body = server.request(f"/api/logs/{record}/original/{kind}", raw=True, full=True)
                originals[kind] = {"status": status, "bytes": len(body),
                                   "equals_upload": digest(body) == digest(next(
                                       f for f in files if f.name.endswith(kind)).read_bytes())}
            entry["original"] = originals
            _, _, body = server.request(
                f"/api/logs/download.zip?ids={record}&parts=rlog,front,qcamera&format=original", raw=True, full=True)
            with zipfile.ZipFile(io.BytesIO(body)) as archive:
                entry["zip_members"] = len(archive.namelist())
            report["segments"].append(entry)
            print(json.dumps({"segment": segment, **{k: entry[k] for k in ("status", "cameras", "thumbnail")}}),
                  flush=True)
        # Each refused input comes from its own segment, so no upload matches another's files.
        for segment, (how, kind) in zip(a.refused_segments, (("bframes", "qcamera.ts"), ("lost_packet", "qcamera.ts"),
                                                            ("scrambled", "qcamera.ts"),
                                                            ("joined_sets", "fcamera.hevc"))):
            folder = work / how
            folder.mkdir()
            files = variant(folder, product.segment_files(a.corpus, segment), kind, how)
            record = product.upload(server, files)
            entry = check_ready(server, record, files)
            report["refused"][how] = entry
            print(json.dumps({"refused": how, **{k: entry[k] for k in ("status", "cameras", "warnings",
                                                                       "originals")}},
                             ensure_ascii=False), flush=True)
            for f in folder.iterdir():
                f.unlink()
    finally:
        server.stop()
    (a.out / "report.json").write_text(json.dumps(report, indent=1, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
