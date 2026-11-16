"""Self-test of terminal_rule.reconcile on synthetic documents (no media).

Run: python3 integration/terminal_rule_selftest.py
"""
import copy
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import terminal_rule  # noqa: E402

KNOWN = {"front": {"python": 60.05, "hars": 60.048, "basis": "next_frame"}}


def docs(front, wide, default):
    """(python, hars) data.json.gz-shaped documents; `front` is raw (modelled)
    and `wide` a reused MP4 whose duration the model does not cover."""
    out = []
    for side in ("python", "hars"):
        videos = {"front": {"start": 0.0, "duration": front[side]}, "wide": {"start": 0.0, "duration": wide[side]}}
        end = max(59.0, *(v["start"] + v["duration"] for v in videos.values()))
        out.append({"videos": videos, "video": dict(videos[default]), "logEnd": 59.0, "duration": end})
    return tuple(out)


def run(files):
    files = copy.deepcopy(files)
    applied, errors = terminal_rule.reconcile(files, KNOWN, lambda telemetry: None)
    return files, applied, errors


def main():
    # The default video is the unmodelled MP4 and differs between the sides: it must stay visible.
    wide = {"python": 60.0, "hars": 61.0}
    files, _, errors = run({"data.json.gz": docs(KNOWN["front"], wide, "wide")})
    python, hars = files["data.json.gz"]
    assert not errors, errors
    assert python["videos"]["front"]["duration"] == hars["videos"]["front"]["duration"]
    assert python["video"]["duration"] != hars["video"]["duration"], "unchecked video.duration was aligned"
    assert python["videos"]["wide"]["duration"] != hars["videos"]["wide"]["duration"]

    # The default video is the modelled camera and both sides match the model: aligned.
    files, applied, errors = run({"data.json.gz": docs(KNOWN["front"], {"python": 60.0, "hars": 60.0}, "front")})
    python, hars = files["data.json.gz"]
    assert not errors and "videos.front.duration" in applied, errors
    assert python == hars, (python, hars)

    # A side that misses its model is reported and never aligned.
    wrong = {"python": 60.05, "hars": 60.2}
    files, _, errors = run({"data.json.gz": docs(wrong, {"python": 60.0, "hars": 60.0}, "front")})
    python, hars = files["data.json.gz"]
    assert "data.json.gz.videos.front.duration:hars" in errors and "data.json.gz.video.duration:hars" in errors, errors
    assert python["videos"]["front"]["duration"] != hars["videos"]["front"]["duration"]
    assert python["video"]["duration"] != hars["video"]["duration"]

    # A file the model does not check keeps its own duration difference.
    extra = ({"duration": 1.0}, {"duration": 2.0})
    files, _, errors = run({"data.json.gz": docs(KNOWN["front"], {"python": 60.0, "hars": 60.0}, "front"), "other.json": extra})
    assert files["other.json"][0]["duration"] != files["other.json"][1]["duration"]
    print("terminal_rule reconcile self-test: PASS")


if __name__ == "__main__":
    main()
