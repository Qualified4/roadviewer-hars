"""Check Git-normalized contents of every preserved upstream file."""
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SOURCE = "71add1a04ed505fc2cb6982052db4a9945d0808f"
rows = subprocess.check_output(["git", "ls-tree", "-r", SOURCE], cwd=ROOT, text=True).splitlines()
count = 0
for row in rows:
    info, name = row.split("\t", 1)
    if name == ".gitignore":
        continue
    path = ROOT / name if name == "LICENSE" else ROOT / "reference/python" / name
    if name == "roadviewer/config.yaml":
        path = path.with_name("config.yaml.reference")
    actual = subprocess.check_output(["git", "hash-object", "--path=" + name, str(path)], cwd=ROOT, text=True).strip()
    if actual != info.split()[2]:
        raise SystemExit("Preserved source mismatch: " + name)
    count += 1
print(f"Verified {count} preserved upstream files; no content differences")
