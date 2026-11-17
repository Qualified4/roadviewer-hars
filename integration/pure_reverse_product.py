#!/usr/bin/env python3
"""Product path of the pure original-format export on the real executable.

    pure_reverse_product.py CORPUS BINARY NEW_OUT --ports UI DEVICE [--segments 3 7]

A new loopback server (HARS_DATA under NEW_OUT, HARS_INGRESS_ONLY=0) with original retention off
gets real segments uploaded from the browser API; after the automatic conversion removed the
uploaded TS/HEVC, every `/api/logs/<id>/original/<kind>` is the export. Checked: the whole
response, Range requests (start, middle, suffix) against the whole, the ZIP download
(`/api/logs/download.zip?format=original`) against the whole, which engine wrote each
(`reverse_engine` events), and the export against the uploaded original
(`pure_reverse_audit.describe`). (Before 2.0.0-alpha.4 the same data folder was also served
with the FFmpeg engine; that engine is gone.) The report holds labels, counts and booleans only.
"""
import argparse
import hashlib
import io
import json
import sys
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pure_default_product as product  # noqa: E402
import pure_reverse_audit as audit  # noqa: E402

CAMERAS = {"qcamera.ts": "qcamera", "fcamera.hevc": "front"}


def events(server):
    out = []
    for line in server.log.read_text(errors="replace").splitlines():
        try:
            fields = json.loads(line).get("fields", {})
        except ValueError:
            continue
        if str(fields.get("event", "")).startswith("reverse_"):
            out.append({k: fields.get(k) for k in ("event", "engine", "kind", "failure_class", "reason_code")
                        if fields.get(k) is not None})
    return out


def fetch(server, record, kind, scratch):
    status, headers, whole = server.request(f"/api/logs/{record}/original/{kind}", raw=True, full=True)
    size = len(whole)
    ranges = {"start": (0, 1023), "middle": (size // 2, size // 2 + 4095), "suffix": (size - 2048, size - 1)}
    checked = {}
    for name, (a, b) in ranges.items():
        spec = f"bytes=-2048" if name == "suffix" else f"bytes={a}-{b}"
        s, h, body = server.request(f"/api/logs/{record}/original/{kind}", raw=True, full=True,
                                    headers={"Range": spec})
        checked[name] = {"status": s, "equal": body == whole[a:b + 1],
                         "content_range": h.get("Content-Range") == f"bytes {a}-{b}/{size}"}
    path = scratch / f"{record[:8]}-{kind}"
    path.write_bytes(whole)
    return {"status": status, "bytes": size, "disposition": "attachment" in headers.get("Content-Disposition", ""),
            "ranges": checked}, path


def summary(described):
    return {k: v for k, v in described.items() if k not in ("_raw", "clocks", "keyframes", "sha256")}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("corpus", type=Path)
    p.add_argument("binary", type=Path)
    p.add_argument("out", type=Path)
    p.add_argument("--ports", nargs=2, type=int, required=True)
    p.add_argument("--segments", nargs="+", type=int, default=[3, 7])
    args = p.parse_args()
    args.out.mkdir(parents=True)
    scratch = args.out / "exports"
    scratch.mkdir()
    report = {"segments": len(args.segments), "records": []}
    server = product.Server(args.binary, args.out, args.ports)
    try:
        server.request("/api/settings/processing", "POST",
                       {"auto_convert": True, "concurrency": 1, "keep_original_video": False})
        records = []
        for segment in args.segments:
            files = product.segment_files(args.corpus, segment)
            record = product.upload(server, files)
            row, states = server.wait(record, ("ready", "error"))
            originals = {f.name.split("--")[-1]: f for f in files}
            left = [k for k in CAMERAS if (server.data / record / k).exists()]
            records.append((segment, record, originals))
            report["records"].append({"status": row["status"], "states": states, "uploaded_originals_left": left})
        pure = {}
        for (segment, record, originals), entry in zip(records, report["records"]):
            for kind in CAMERAS:
                checked, path = fetch(server, record, kind, scratch)
                hevc = kind.endswith(".hevc")
                got, original = audit.describe(path, hevc), audit.describe(originals[kind], hevc)
                checked["vs_upload"] = {
                    "bytes": audit.difference(got, original, hevc),
                    "normalized_nal_equal": got["normalized_nal_sha256"] == original["normalized_nal_sha256"],
                    "frames_equal": got["frames"] == original["frames"],
                    "keyframes_equal": got["keyframes"] == original["keyframes"],
                    "clocks_equal": None if hevc else got["clocks"] == original["clocks"]}
                entry[kind] = checked
                pure[(record, kind)] = (path, got)
            parts = "rlog,front,qcamera"
            _, _, body = server.request(f"/api/logs/download.zip?ids={record}&parts={parts}&format=original",
                                        raw=True, full=True)
            with zipfile.ZipFile(io.BytesIO(body)) as archive:
                names = archive.namelist()
                entry["zip"] = {"members": len(names)}
                for kind in CAMERAS:
                    member = next(n for n in names if n.endswith(kind))
                    entry["zip"][kind + "_equals_original_route"] = (
                        hashlib.sha256(archive.read(member)).hexdigest() == pure[(record, kind)][1]["sha256"])
        report["pure_server_events"] = events(server)
    finally:
        server.stop()
    server.log.rename(args.out / "server-pure.log")
    for f in scratch.iterdir():
        f.unlink()
    (args.out / "report.json").write_text(json.dumps(report, indent=1) + "\n")
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()
