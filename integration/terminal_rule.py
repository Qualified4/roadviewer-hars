"""Known, modelled difference between Python and HARS video durations.

HARS ends a remuxed raw video's last sample at the next logged camera frame
(docs/hars/TERMINAL-DURATION.md). Python ends it at 1/r_frame_rate of the
libavformat probe (qcamera.ts) or at a fixed 0.05 s (raw HEVC). Each side's
expected value is computed here from the inputs, independently of both
outputs, and each output must match its own value exactly. Only the fields
derived from that one number are then set equal before the full comparison;
every other field keeps the existing 1e-6 rule.
"""
import math
from pathlib import Path

CAMERAS = (("qcamera", "qcamera.ts", "qRoadEncodeIdx", "roadCameraState"),
           ("front", "fcamera.hevc", "roadEncodeIdx", "roadCameraState"),
           ("wide", "ecamera.hevc", "wideRoadEncodeIdx", "wideRoadCameraState"))
EXACT = 1e-9


def std_rate(i):
    if i < 360:return (i + 1) * 1001
    if i < 390:return (i - 360 + 31) * 1001 * 12
    if i < 393:return [80, 120, 240][i - 390] * 1001 * 12
    return [24, 30, 60, 12, 15, 48][i - 393] * 1000 * 12


def probe_rate(dts):
    """libavformat ff_rfps_add_frame/ff_rfps_calculate for an H.264 TS stream
    without SPS timing, stopping after 20 DTS intervals as the library probe
    does (same algorithm as tests/ts_duration.rs). Returns (num, den)."""
    n_rates = 399
    tb = 1 / 90000
    err = [[[0.0] * n_rates for _ in range(2)] for _ in range(2)]
    count = total = gcd_all = 0
    last = None
    for ts in dts:
        if count >= 20:break
        if last is not None and ts > last:
            seconds = ts * tb
            for i in range(n_rates):
                if err[0][1][i] < 1e10:
                    sdts = seconds * std_rate(i) / 12012
                    for j in range(2):
                        error = sdts - round(sdts + j * 0.5) + j * 0.5
                        err[j][0][i] += error
                        err[j][1][i] += error * error
            count += 1
            total += ts - last
            if count % 10 == 0:
                for i in range(n_rates):
                    if err[0][1][i] < 1e10:
                        def e(j):
                            a = err[j][0][i] / count
                            return err[j][1][i] / count - a * a
                        if e(0) > 0.04 and e(1) > 0.04:
                            err[0][1][i] = err[1][1][i] = 2e10
            if count > 3:gcd_all = math.gcd(gcd_all, ts - last)
        last = ts
    if count > 15 and gcd_all > 180:
        g = math.gcd(90000, gcd_all)
        return 90000 // g, gcd_all // g
    best, num = 0.01, 0
    if count > 1:
        for j in range(n_rates):
            if std_rate(j) < 12012 or tb * total / count < 12012 * 0.8 / std_rate(j):continue
            for k in range(2):
                a = err[k][0][j] / count
                error = err[k][1][j] / count - a * a
                if error < best and best > 1e-9:best, num = error, std_rate(j)
    if num == 0:return 90000, 1
    g = math.gcd(num, 12012)
    return num // g, 12012 // g


def hars_terminal(stamps, timescale, gap_ns):
    """Independent re-implementation of src/terminal_duration.rs::decide."""
    intervals = sorted(b - a for a, b in zip(stamps, stamps[1:]))
    typical = intervals[(len(intervals) - 1) // 2] if intervals else None
    reference = typical if typical is not None else -(-timescale // 20)
    if gap_ns is not None and gap_ns > 0:
        ticks = (gap_ns * timescale + 500_000_000) // 1_000_000_000
        if 0 < ticks <= 2 * reference:return ticks, "next_frame"
    return (typical, "median_interval") if typical is not None else (reference, "nominal")


def half_up(x):
    whole = math.floor(x)
    return whole + (1 if x - whole >= 0.5 else 0)


def read_log(rlog, schema):
    import capnp, zstandard
    log = capnp.load(str(schema))
    with Path(rlog).open("rb") as f:
        raw = zstandard.ZstdDecompressor().stream_reader(f).read()
    rows = {}
    for e in log.Event.read_multiple_bytes(raw):
        try:kind = e.which()
        except Exception:continue
        if kind in ("qRoadEncodeIdx", "roadEncodeIdx", "wideRoadEncodeIdx"):
            x = getattr(e, kind);rows.setdefault(kind, []).append((x.segmentId, x.frameId, x.timestampEof))
        elif kind in ("roadCameraState", "wideRoadCameraState"):
            x = getattr(e, kind);rows.setdefault(kind, []).append((x.frameId, x.timestampEof))
    return rows


def model(rlog, sources, schema):
    """{camera: {"python": seconds, "hars": seconds, "basis": str}} for each raw
    original in `sources` ({kind: path}); MP4 originals are reused, not modelled."""
    import av
    rows = read_log(rlog, schema)
    out = {}
    for camera, kind, index, state in CAMERAS:
        path = sources.get(kind)
        if path is None or not Path(path).is_file() or not rows.get(index):continue
        frames = sorted(rows[index])
        eofs = [r[2] for r in frames]
        nxt = {e for f, e in rows.get(state, []) if f == frames[-1][1] + 1}
        gap = next(iter(nxt)) - eofs[-1] if len(nxt) == 1 else None
        if kind.endswith(".ts"):
            with av.open(str(path)) as c:
                s = c.streams.video[0]
                packets = [(p.pts, p.dts) for p in c.demux(s) if p.size]
            dts = [p[1] for p in packets]
            pts = sorted(p[0] for p in packets)
            num, den = probe_rate(dts)
            last = pts[-1] - pts[0]
            ticks, basis = hars_terminal(dts, 90000, gap)
            out[camera] = {"python": (last + 90000 * den // num) / 90000, "hars": (last + ticks) / 90000, "basis": basis}
        else:
            times = [(e - eofs[0]) / 1e9 for e in eofs]
            stamps = [half_up(t * 1e6) for t in times]
            ticks, basis = hars_terminal(stamps, 1_000_000, gap)
            out[camera] = {"python": (round(times[-1] * 1e6) + 50000) / 1e6, "hars": (stamps[-1] + ticks) / 1e6, "basis": basis}
    return out


def _check(errors, label, value, expected):
    if not isinstance(value, (int, float)) or abs(value - expected) > EXACT:
        errors[label] = {"value": value, "expected": expected}


def reconcile(files, known, drive_summary):
    """Check each side against its own model, then align the derived fields.

    `files` maps a file name to (python, hars) parsed JSON; `known` comes from
    `model`. Returns ({field: {"python", "hars"}}, {field: mismatch}).
    """
    applied, errors = {}, {}
    if not known:return applied, errors
    ends = {}
    for name in ("data.json.gz", "summary.json"):
        if name not in files:continue
        for side, doc in zip(("python", "hars"), files[name]):
            videos = doc.get("videos") or {}
            for camera, values in known.items():
                if camera in videos:
                    _check(errors, f"{name}.videos.{camera}.duration:{side}", videos[camera]["duration"], values[side])
                    applied[f"videos.{camera}.duration"] = {k: values[k] for k in ("python", "hars", "basis")}
            default = doc.get("video")
            if isinstance(default, dict):
                camera = next((c for c, v in videos.items() if v == default), None)
                if camera in known:
                    _check(errors, f"{name}.video.duration:{side}", default["duration"], known[camera][side])
            if name == "data.json.gz":
                ends[side] = max([doc["logEnd"]] + [v["start"] + v["duration"] for v in videos.values()])
                _check(errors, f"{name}.duration:{side}", doc["duration"], ends[side])
            elif side in ends:
                _check(errors, f"{name}.duration:{side}", doc["duration"], ends[side])
    if "telemetry.json.gz" in files:
        for side, doc in zip(("python", "hars"), files["telemetry.json.gz"]):
            if side in ends:_check(errors, f"telemetry.json.gz.duration:{side}", doc["duration"], ends[side])
        if "summary.json" in files:
            for side, telemetry, summary in zip(("python", "hars"), files["telemetry.json.gz"], files["summary.json"]):
                if summary.get("drive_summary") != drive_summary(telemetry):
                    errors[f"summary.json.drive_summary:{side}"] = {"value": summary.get("drive_summary"), "expected": drive_summary(telemetry)}
    if ends:applied["duration"] = dict(ends)
    for name, (python, hars) in files.items():
        for camera in known:
            if camera in (python.get("videos") or {}) and camera in (hars.get("videos") or {}):
                hars["videos"][camera]["duration"] = python["videos"][camera]["duration"]
        if isinstance(python.get("video"), dict) and isinstance(hars.get("video"), dict):
            hars["video"]["duration"] = python["video"]["duration"]
        if ends and "duration" in python and "duration" in hars:
            hars["duration"] = python["duration"]
        drive = name == "summary.json" and "drive_summary" in python and "drive_summary" in hars
        if drive and not any(k.startswith("summary.json.drive_summary") for k in errors):
            if python["drive_summary"] != hars["drive_summary"]:
                applied["drive_summary"] = {"python": python["drive_summary"], "hars": hars["drive_summary"]}
            hars["drive_summary"] = python["drive_summary"]
    return applied, errors
