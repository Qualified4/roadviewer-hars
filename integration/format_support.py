#!/usr/bin/env python3
"""Format support on the real executable: the video formats openpilot records, and the inputs the
pure engines refuse.

    format_support.py C13 C14 INPUTS BINARY OUT --ports UI DEVICE --path NO_FFMPEG_DIR [--only NAME ...]

C13 and C14 are the 13-segment original corpus and the 14-segment MP4 corpus (read only). INPUTS
holds the synthetic files `sh/make_inputs.sh` (development ffmpeg) writes; the names are fixed below.
Every case runs on its own server and new data folder (HARS_INGRESS_ONLY=0, PATH=NO_FFMPEG_DIR,
automatic conversion, original retention off): browser upload of the case's files, then the
record's state, warnings and error, Range playback of each converted camera, the thumbnail, each
camera's `/original/<kind>` (normalized NAL units and picture count against the upload), whether
each uploaded original is still stored byte for byte, and whether the server still runs.
A refused case that passes the upload check runs again with a published result in place: the good
segment is converted with retention on, the camera's original is replaced by the refused file and
the record requeued (as a server killed mid-conversion leaves it), then the server is started again;
checked are the previous MP4's bytes, the warning, the replaced original and the other camera.
The report holds labels, counts, states, codes, messages and booleans only.
"""
import argparse
import hashlib
import io
import json
import shutil
import subprocess
import sys
import time
import urllib.error
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pure_default_product as product  # noqa: E402

MP4 = {"qcamera": "camera.mp4", "front": "fcamera.mp4", "wide": "ecamera.mp4"}
CAMERA = {"qcamera.ts": "qcamera", "qcamera.mp4": "qcamera", "fcamera.hevc": "front", "fcamera.mp4": "front",
          "ecamera.hevc": "wide", "ecamera.mp4": "wide"}

# name: (corpus, segment, {kind: synthetic file or "corpus:<kind>" for another corpus file}, kinds of the
# segment kept from the corpus, the kind under test)
CASES = {
    "B1_fcamera_1928x1208": ("c13", 1, {"fcamera.hevc": "b1-1928.hevc"}, [], "fcamera.hevc"),
    "B2_ecamera_1928x1208": ("c13", 2, {"ecamera.hevc": "b1-1928.hevc"}, [], "ecamera.hevc"),
    "B3_ecamera_device_1344x760": ("c13", 3, {"ecamera.hevc": "corpus:fcamera.hevc"}, [], "ecamera.hevc"),
    "B4_dcamera": ("c13", 3, {"dcamera.hevc": "corpus:fcamera.hevc"}, [], "dcamera.hevc"),
    "B5_qcamera_with_aac": ("c13", 2, {"qcamera.ts": "b5-audio.ts"}, ["fcamera.hevc"], "qcamera.ts"),
    "B6_pc_qcamera_libx264_defaults": ("c13", 3, {"qcamera.ts": "b6-pc.ts"}, ["fcamera.hevc"], "qcamera.ts"),
    "B7_pc_fcamera_ffvhuff_matroska": ("c13", 4, {"fcamera.hevc": "b7-pc.mkv"}, ["qcamera.ts"], "fcamera.hevc"),
    "C01_bframe_ts": ("c13", 4, {"qcamera.ts": "c01-bframe.ts"}, ["fcamera.hevc"], "qcamera.ts"),
    "C02_interlaced_ts": ("c13", 5, {"qcamera.ts": "c02-interlaced.ts"}, ["fcamera.hevc"], "qcamera.ts"),
    "C03_bframe_hevc": ("c13", 6, {"fcamera.hevc": "c03-bframe.hevc"}, ["qcamera.ts"], "fcamera.hevc"),
    "C04_parameter_sets_change_hevc": ("c13", 7, {"fcamera.hevc": "c04-joined.hevc"}, ["qcamera.ts"], "fcamera.hevc"),
    "C05_truncated_ts_in_video_payload": ("c13", 8, {"qcamera.ts": "c05-truncated.ts"}, ["fcamera.hevc"], "qcamera.ts"),
    "C06_truncated_hevc": ("c13", 9, {"fcamera.hevc": "c06-truncated.hevc"}, ["qcamera.ts"], "fcamera.hevc"),
    "C07_empty_hevc": ("c13", 10, {"fcamera.hevc": "c07-empty.hevc"}, ["qcamera.ts"], "fcamera.hevc"),
    "C08_random_bytes_hevc": ("c13", 11, {"fcamera.hevc": "c08-random.hevc"}, ["qcamera.ts"], "fcamera.hevc"),
    "C09_pat_pmt_damaged_ts": ("c13", 8, {"qcamera.ts": "c09-psi.ts"}, ["fcamera.hevc"], "qcamera.ts"),
    "C10_bframe_mp4_edit_list": ("c14", 2, {"qcamera.mp4": "c10-bframe.mp4"}, ["fcamera.mp4"], "qcamera.mp4"),
    "C11_mp4_without_moov": ("c14", 3, {"qcamera.mp4": "c11-nomoov.mp4"}, ["fcamera.mp4"], "qcamera.mp4"),
    "C12_main10_hevc": ("c13", 12, {"fcamera.hevc": "c12-main10.hevc"}, ["qcamera.ts"], "fcamera.hevc"),
    "D_main10_hevc_only": ("c13", 12, {"fcamera.hevc": "c12-main10.hevc"}, [], "fcamera.hevc"),
}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def nals(data, hevc):
    """Annex B units as (type, sha of the unit without trailing zero bytes)."""
    out, i, n = [], 0, len(data)
    starts = []
    while True:
        j = data.find(b"\x00\x00\x01", i)
        if j < 0:
            break
        starts.append(j + 3)
        i = j + 3
    for k, s in enumerate(starts):
        e = starts[k + 1] - 3 if k + 1 < len(starts) else n
        unit = data[s:e].rstrip(b"\x00")
        if unit:
            t = (unit[0] >> 1) & 0x3F if hevc else unit[0] & 0x1F
            out.append((t, sha(unit)))
    return out


def elementary(path, kind):
    """The video elementary stream of a stored or exported original (development ffmpeg)."""
    if kind.endswith(".hevc"):
        return Path(path).read_bytes()
    fmt = "h264"
    return subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-i", str(path), "-map", "0:v:0", "-c", "copy",
                           "-f", fmt, "-"], capture_output=True, check=True).stdout


def vcl(t, hevc):
    return t < 32 if hevc else t in (1, 5)


def normalized(units, hevc):
    """The lossless-remux view (as `media::Signature`): ordered picture units, the set of parameter
    sets, and the ordered other units. Where a container repeats parameter sets does not count."""
    sets = (32, 33, 34) if hevc else (7, 8)
    return ([h for t, h in units if vcl(t, hevc)], sorted({(t, h) for t, h in units if t in sets}),
            [(t, h) for t, h in units if not vcl(t, hevc) and t not in sets and t != (35 if hevc else 9)])


def pictures(data, hevc):
    """Pictures: slices that start a picture (first_slice_segment_in_pic_flag / first_mb_in_slice 0)."""
    count, i = 0, data.find(b"\x00\x00\x01")
    while i >= 0 and i + 5 < len(data):
        t = (data[i + 3] >> 1) & 0x3F if hevc else data[i + 3] & 0x1F
        if vcl(t, hevc) and data[i + (5 if hevc else 4)] & 0x80:
            count += 1
        i = data.find(b"\x00\x00\x01", i + 3)
    return count


def stage(folder, corpus, segment, replace, keep, inputs):
    folder.mkdir(parents=True)
    route_files = sorted(corpus.glob(f"*--{segment}--rlog.zst"))
    prefix = route_files[0].name[: -len("rlog.zst")]
    files = [route_files[0]]
    for kind in keep:
        files.append(corpus / (prefix + kind))
    for kind, source in replace.items():
        src = corpus / (prefix + source.split(":", 1)[1]) if source.startswith("corpus:") else inputs / source
        link = folder / (prefix + kind)
        link.symlink_to(src)
        files.append(link)
    return files


def upload(server, files):
    """Browser upload; an HTTP refusal is returned with its status, code and message."""
    step = "manifest"
    try:
        manifest = {"files": [{"name": f.name, "size": f.stat().st_size, "sha256": sha(f.read_bytes())} for f in files]}
        session = server.request("/api/uploads", "POST", manifest)["id"]
        step = "chunks"
        for i, f in enumerate(files):
            data = f.read_bytes()
            for offset in range(0, max(len(data), 1), 2 << 20):
                server.request(f"/api/uploads/{session}/files/{i}?offset={offset}", "PUT",
                               data[offset:offset + (2 << 20)], raw=True)
        step = "finish"
        body = server.request(f"/api/uploads/{session}/finish", "POST", {})
        logs = body["logs"]
        return {"record": logs[0]["id"] if isinstance(logs[0], dict) else logs[0], "finish": {
            k: v for k, v in body.items() if k not in ("logs",)}}
    except urllib.error.HTTPError as e:
        try:
            detail = json.loads(e.read())
        except ValueError:
            detail = {}
        return {"refused": {"step": step, "status": e.code, "error": detail.get("error"),
                            "message": detail.get("message")}}


def playback(server, record, camera):
    status, _, whole = server.request(f"/api/logs/{record}/download?source={camera}&format=mp4", raw=True, full=True)
    s1, headers, head = server.request(f"/api/logs/{record}/video?source={camera}", raw=True, full=True,
                                       headers={"Range": "bytes=0-1023"})
    s2, _, tail = server.request(f"/api/logs/{record}/video?source={camera}", raw=True, full=True,
                                 headers={"Range": f"bytes={len(whole) - 1024}-"})
    return {"range206": s1 == 206 and s2 == 206, "mp4": whole[4:8] == b"ftyp",
            "range_bytes_equal": head == whole[:1024] and tail == whole[-1024:], "mp4_sha": sha(whole)}


def safe(server, path):
    try:
        status, headers, body = server.request(path, raw=True, full=True)
        return status, headers, body
    except urllib.error.HTTPError as e:
        body = e.read()
        return e.code, e.headers, body


def error_body(body):
    try:
        v = json.loads(body)
        return {"error": v.get("error"), "message": v.get("message")}
    except ValueError:
        return {"bytes": len(body)}


def inspect(server, record, files, tested):
    row, states = server.wait(record, ("ready", "error"), timeout=300)
    folder = server.data / record
    meta = json.loads((folder / "meta.json").read_text())
    uploaded = {f.name.split("--")[-1]: f for f in files}
    entry = {"states": states, "status": row["status"], "cameras": sorted(meta.get("videos", {})),
             "warnings": meta.get("warnings", []), "error": meta.get("error"),
             "row_fields": {k: row.get(k) for k in ("status", "thumbnail", "warnings", "error") if k in row},
             "events": [e for e in server.events(record) if e["event"] != "remux_engine"]}
    entry["originals"] = {}
    for kind, f in uploaded.items():
        stored = folder / ("camera.mp4" if kind == "qcamera.mp4" else kind)  # the stored name of a qcamera MP4
        entry["originals"][kind] = ("unchanged" if stored.is_file() and sha(stored.read_bytes()) == sha(f.read_bytes())
                                    else "gone" if not stored.exists() else "CHANGED")
    entry["stored_files"] = sorted(p.name for p in folder.iterdir() if p.is_file() and not p.name.startswith("."))
    entry["zip_body"] = None
    entry["play"] = {c: playback(server, record, c) for c in entry["cameras"]}
    status, _, body = safe(server, f"/api/logs/{record}/thumbnail")
    entry["thumbnail"] = {"status": status, "jpeg": body[:2] == b"\xff\xd8",
                          **({} if status == 200 else error_body(body))}
    entry["thumbnail_failed_marker"] = (folder / "prepared" / "thumbnail.failed").exists()
    entry["export"] = {}
    for kind, f in uploaded.items():
        if kind == "rlog.zst" or kind.endswith(".mp4"):
            continue
        status, _, body = safe(server, f"/api/logs/{record}/original/{kind}")
        e = {"status": status}
        if status == 200:
            out = server.root / "export.bin"
            out.write_bytes(body)
            e["bytes_equal_upload"] = sha(body) == sha(f.read_bytes())
            try:
                hevc = kind.endswith(".hevc")
                ea, eb = elementary(f, kind), elementary(out, kind)
                a, b = nals(ea, hevc), nals(eb, hevc)
                e.update(normalized_nal_equal=normalized(a, hevc) == normalized(b, hevc),
                         nal_units_equal=a == b, pictures=[pictures(ea, hevc), pictures(eb, hevc)],
                         units=[len(a), len(b)])
            except subprocess.CalledProcessError:
                e["elementary"] = "unreadable"
            out.unlink()
        else:
            e.update(error_body(body))
        entry["export"][kind] = e
    for path in (f"/api/logs/{record}/data", f"/api/logs/{record}/telemetry", f"/api/logs/{record}/track"):
        status, _, _ = safe(server, path)
        entry.setdefault("apis", {})[path.rsplit("/", 1)[1]] = status
    status, _, body = safe(server, f"/api/logs/download.zip?ids={record}&parts=rlog,front,qcamera,wide&format=original")
    entry["zip"] = {"status": status, "members": len(zipfile.ZipFile(io.BytesIO(body)).namelist()) if status == 200 else 0,
                    **({} if status == 200 else error_body(body))}
    del entry["zip_body"]
    status, _, body = safe(server, "/api/logs")
    entry["list"] = {"status": status, "has_record": status == 200 and any(r["id"] == record for r in json.loads(body)["logs"])}
    entry["server_running"] = server.process.poll() is None
    return entry


def requeue(folder):
    meta = json.loads((folder / "meta.json").read_text())
    meta["status"] = "queued"
    meta["manual_conversion"] = True
    (folder / "meta.json").write_text(json.dumps(meta))


def previous(a, name, corpus, segment, replace, keep, tested, root):
    """The refused file in place of a published camera's original, converted again."""
    good = stage(root / "good", corpus, segment, {}, keep + [tested], a.inputs)
    server = product.Server(a.binary, root, a.ports, env={"PATH": a.path})
    try:
        server.request("/api/settings/processing", "POST", {"auto_convert": True, "concurrency": 1,
                                                            "keep_original_video": True})
        record = upload(server, good)["record"]
        server.wait(record, ("ready", "error"), timeout=300)
    finally:
        server.stop()
    folder = server.data / record
    camera = CAMERA[tested]
    before = (folder / "prepared" / MP4[camera]).read_bytes()
    bad = a.inputs / replace[tested]
    (folder / tested).unlink()
    shutil.copyfile(bad, folder / tested)
    requeue(folder)
    server = product.Server(a.binary, root, a.ports, env={"PATH": a.path})
    try:
        row, states = server.wait(record, ("ready", "error"), timeout=300)
        time.sleep(0.5)
        meta = json.loads((folder / "meta.json").read_text())
        after = folder / "prepared" / MP4[camera]
        out = {"status": row["status"], "cameras": sorted(meta.get("videos", {})), "warnings": meta.get("warnings", []),
               "error": meta.get("error"),
               "previous_mp4_bytes_kept": after.is_file() and after.read_bytes() == before,
               "refused_original_unchanged": sha((folder / tested).read_bytes()) == sha(bad.read_bytes()),
               "play": {c: playback(server, record, c)["range206"] for c in sorted(meta.get("videos", {}))},
               "server_running": server.process.poll() is None,
               "events": [e for e in server.events(record) if e["event"] != "remux_engine"]}
    finally:
        server.stop()
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("c13", type=Path)
    p.add_argument("c14", type=Path)
    p.add_argument("inputs", type=Path)
    p.add_argument("binary", type=Path)
    p.add_argument("out", type=Path)
    p.add_argument("--ports", type=int, nargs=2, required=True)
    p.add_argument("--path", required=True)
    p.add_argument("--only", nargs="*")
    a = p.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    report = {}
    for name, (corpus, segment, replace, keep, tested) in CASES.items():
        if a.only and name not in a.only:
            continue
        corpus = a.c13 if corpus == "c13" else a.c14
        root = a.out / name
        files = stage(root / "files", corpus, segment, replace, keep, a.inputs)
        server = product.Server(a.binary, root, a.ports, env={"PATH": a.path})
        server.root = root
        try:
            server.request("/api/settings/processing", "POST", {"auto_convert": True, "concurrency": 1,
                                                                "keep_original_video": False})
            up = upload(server, files)
            entry = {"upload": up}
            if "record" in up:
                entry.update(inspect(server, up["record"], files, tested))
            entry["server_running_at_end"] = server.process.poll() is None
        finally:
            server.stop()
        refused = "refused" in up or CAMERA.get(tested) not in entry.get("cameras", [])
        if name.startswith("C") and refused and "record" in up and tested.endswith(".mp4"):
            # An MP4 upload is played as uploaded (no remux, nothing in `prepared/` to carry over).
            entry["with_previous_result"] = {"skipped": "mp4 input"}
        elif name.startswith("C") and refused and "record" in up and tested in CAMERA:
            entry["with_previous_result"] = previous(a, name, corpus, segment, replace, keep, tested, a.out / (name + "-prev"))
        report[name] = entry
        (a.out / "report.json").write_text(json.dumps(report, indent=1, ensure_ascii=False) + "\n")
        print(json.dumps({"case": name, "upload": up.get("refused", "accepted"), "status": entry.get("status"),
                          "cameras": entry.get("cameras"), "warnings": entry.get("warnings"),
                          "thumbnail": entry.get("thumbnail"), "export": entry.get("export"),
                          "previous": {k: v for k, v in entry.get("with_previous_result", {}).items() if k != "events"}},
                         ensure_ascii=False), flush=True)
        for d in (root, a.out / (name + "-prev")):
            if d.exists():
                shutil.rmtree(d / "data", ignore_errors=True)
    (a.out / "report.json").write_text(json.dumps(report, indent=1, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
