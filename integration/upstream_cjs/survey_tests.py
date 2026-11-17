#!/usr/bin/env python3
"""Inventory of the preserved browser tests (reference/python/tests/*.cjs) as JSON: how each test gets its server, the
fixtures and Python pieces it uses, and countable units. Read from the test text only; nothing is run.

  assertions    assert(...) / assert.<name>(...) call sites in the file (a call site inside a loop counts once)
  pass_markers  console.log('PASS...') call sites (one per scenario the file reports as passed)"""
import json, re, sys
from pathlib import Path

TESTS = Path(__file__).resolve().parents[2] / "reference/python/tests"
SKIP = {"browser-helpers.cjs", "run_group.cjs"}


def survey(file):
    text = file.read_text()
    server = []
    if re.search(r"spawn\(process\.env\.RV_PYTHON", text):
        server.append("python-server (RV_PYTHON)")
    if re.search(r"readFileSync\(\s*(?:'roadviewer/app/web|path\.join\('roadviewer/app/web|file\))", text):
        server.append("disk web files + page.route mocks")
    if "chromium.launch" not in text:
        server.append("node only (no browser)")
    fixtures = [name for name, pattern in [
        ("RV_TEST_VIDEO", r"RV_TEST_VIDEO"), ("fake_drive.py", r"fake_drive"),
        ("overlay_geometry_fixture.py (Python overlay/compact)", r"overlay_geometry_fixture"),
        ("compact.py (Python)", r"from compact import"), ("PyAV fixture video", r"import av"),
        ("tests/fixtures/ui-review-colour.mp4", r"ui-review-colour"), ("roadviewer/config.yaml", r"config\.yaml"),
        ("writes RV_DATA folders", r"writeFileSync\(path\.join\((?:folder|dir|record|root),"),
        ("reads RV_DATA meta.json", r"readFileSync\(path\.join\([a-zA-Z]+,(?:id|ids\[0\]),'meta\.json'"),
        ("python stdin control", r"stdin\.write"), ("git", r"execFileSync\('git'")] if re.search(pattern, text)]
    passes = re.findall(r"console\.log\(\s*[`'\"]PASS:?\s*([^`'\"]*)", text)
    return {"file": file.name, "server": server, "fixtures": fixtures,
            "assertions": len(re.findall(r"\bassert(?:\.\w+)?\(", text)), "pass_markers": len(passes),
            "pass_text": passes}


if __name__ == "__main__":
    rows = [survey(f) for f in sorted(TESTS.glob("*.cjs")) if f.name not in SKIP]
    json.dump(rows, sys.stdout, ensure_ascii=False, indent=1)
