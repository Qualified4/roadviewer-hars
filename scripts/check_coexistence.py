"""Verify HARS and the preserved Python app can be installed side by side on one HAOS host."""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def top_level(text):
    return dict(re.findall(r"^([a-z_]+):[ \t]*(\S.*)?$", text, re.M))


def block(text, key):
    match = re.search(rf"^{key}:\n((?:[ \t]+.*\n|\n)*)", text, re.M)
    return match.group(1) if match else ""


hars_text = (ROOT / "roadviewer_hars/config.yaml").read_text(encoding="utf-8")
python_text = (ROOT / "reference/python/roadviewer/config.yaml.reference").read_text(encoding="utf-8")
hars, python = top_level(hars_text), top_level(python_text)

for key in ("slug", "image", "name", "panel_title", "url"):
    assert hars[key] != python[key], f"{key} must differ from the Python app"
assert hars["ingress"] == "true" and hars["ingress_port"] == python["ingress_port"], "each app keeps its own container-private Ingress listener"

# Supervisor rejects two apps publishing one host port; both stay unpublished until an operator assigns distinct ports.
for text in (hars_text, python_text):
    assert set(re.findall(r"^\s+\d+/tcp:\s*(\S+)", block(text, "ports"), re.M)) == {"null"}

# /data is private per slug, and the data roots inside it differ as well, so a restored backup cannot cross over.
hars_roots = set(re.findall(r'"([^/"]+)/', block(hars_text, "backup_exclude")))
python_roots = set(re.findall(r'"([^/"]+)/', block(python_text, "backup_exclude")))
assert hars_roots == {"roadviewer-hars"} and python_roots == {"roadviewer"}

# The device network panel reads and writes this app's own port mapping through Supervisor.
assert hars.get("hassio_api") == "true" and python.get("hassio_api") == "true"
assert hars["boot"] == "manual", "HARS must not start on boot before cutover is approved"
source = (ROOT / "src/main.rs").read_text(encoding="utf-8")
assert '"/data/roadviewer-hars"' in source and '"/data/roadviewer"' not in source, "HARS default data root must be its own"
print("HAOS coexistence invariants verified:", hars["slug"], "next to", python["slug"])
