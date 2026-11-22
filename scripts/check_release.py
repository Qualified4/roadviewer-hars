"""Guard the independent product identity and the release version format."""
import argparse
from pathlib import Path
import re
import tomllib

ROOT = Path(__file__).resolve().parents[1]
p = argparse.ArgumentParser()
p.add_argument("--tag")
args = p.parse_args()
version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
assert re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|beta|rc)\.[1-9][0-9]*)?", version), \
    "Version must be N.N.N (stable) or N.N.N-(alpha|beta|rc).N (prerelease)"
config = (ROOT / "roadviewer_hars/config.yaml").read_text(encoding="utf-8")
assert f'version: "{version}"' in config
assert f"ARG BUILD_VERSION={version}\n" in (ROOT/"Dockerfile").read_text()
for readme, line in [("README.md", f"현재 버전은 **{version}**입니다."), ("README.en.md", f"The current version is **{version}**."),
                     ("roadviewer_hars/README.md", f"현재 버전: **{version}**"), ("roadviewer_hars/README.en.md", f"Current version: **{version}**")]:
    assert line in (ROOT / readme).read_text(encoding="utf-8"), f"{readme} must show version {version}"
assert "slug: roadviewer_hars\n" in config
assert "image: ghcr.io/qualified4/roadviewer-hars\n" in config
assert "8443/tcp: null" in config
assert "boot: auto" in config
assert "device_https_enabled: false" in config
assert not (ROOT / "roadviewer/config.yaml").exists()
discoverable = {p.relative_to(ROOT).as_posix() for p in ROOT.glob("**/config.*")
                if p.suffix in (".yaml", ".yml", ".json") and not any(part.startswith(".") or part == "rootfs" for part in p.relative_to(ROOT).parts)}
assert discoverable == {"roadviewer_hars/config.yaml"}, "HA must discover only the independent HARS app"
if args.tag:
    assert args.tag == version, "Tag and product version disagree"
print("HARS identity, isolated ports, and release version verified:", version)
