"""Guard the independent product identity and prerelease-only initial pipeline."""
import argparse
from pathlib import Path
import re
import tomllib

ROOT = Path(__file__).resolve().parents[1]
p = argparse.ArgumentParser()
p.add_argument("--tag")
args = p.parse_args()
version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
assert re.fullmatch(r"2\.0\.0-(alpha|beta|rc)\.[1-9][0-9]*", version), "Full parity gates are required before stable publishing is implemented"
config = (ROOT / "roadviewer_hars/config.yaml").read_text(encoding="utf-8")
assert f'version: "{version}"' in config
assert f"ARG BUILD_VERSION={version}\n" in (ROOT/"Dockerfile").read_text()
assert "slug: roadviewer_hars\n" in config
assert "image: ghcr.io/qualified4/roadviewer-hars\n" in config
assert "8443/tcp: null" in config
assert "boot: manual" in config
assert "device_https_enabled: false" in config
assert not (ROOT / "roadviewer/config.yaml").exists()
discoverable = {p.relative_to(ROOT).as_posix() for p in ROOT.glob("**/config.*")
                if p.suffix in (".yaml", ".yml", ".json") and not any(part.startswith(".") or part == "rootfs" for part in p.relative_to(ROOT).parts)}
assert discoverable == {"roadviewer_hars/config.yaml"}, "HA must discover only the independent HARS app"
if args.tag:
    assert args.tag == version, "Tag and product version disagree"
print("HARS identity, isolated ports, and prerelease gate verified:", version)
