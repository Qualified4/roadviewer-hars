"""Verify the add-on settings that keep HARS isolated on HAOS."""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def top_level(text):
    return dict(re.findall(r"^([a-z_]+):[ \t]*(\S.*)?$", text, re.M))


def block(text, key):
    match = re.search(rf"^{key}:\n((?:[ \t]+.*\n|\n)*)", text, re.M)
    return match.group(1) if match else ""


hars_text = (ROOT / "roadviewer_hars/config.yaml").read_text(encoding="utf-8")
hars = top_level(hars_text)

# Keep the released add-on identity and the container-private Ingress listener.
assert hars["slug"] == "roadviewer_hars"
assert hars["image"] == "ghcr.io/qualified4/roadviewer-hars"
assert hars["url"] == "https://github.com/Qualified4/roadviewer-hars"
assert hars["ingress"] == "true" and hars["ingress_port"] == "8099"

# Supervisor publishes a host port only after an operator configures it.
assert re.findall(r"^\s+(\d+/tcp):\s*(\S+)", block(hars_text, "ports"), re.M) == [("8443/tcp", "null")]

# /data is private per slug and the app uses its own root inside it.
assert set(re.findall(r'"([^/"]+)/', block(hars_text, "backup_exclude"))) == {"roadviewer-hars"}
assert hars["hassio_api"] == "true" and hars["boot"] == "auto"
source = (ROOT / "src/main.rs").read_text(encoding="utf-8")
assert '"/data/roadviewer-hars"' in source and '"/data/roadviewer"' not in source
print("HAOS add-on isolation verified:", hars["slug"])
