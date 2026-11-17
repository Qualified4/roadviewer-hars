"""Fill OUT/roadviewer/app/web with what a running HARS serves for every preserved web file, so
that a preserved browser test that reads its pages from disk (tests/segment_peek.cjs) runs
against HARS's output when started with OUT as its working directory.
usage: mirror_web.py SERVER WEB RECORD OUT"""
import sys, urllib.request
from pathlib import Path

server, web, record, out = sys.argv[1:]
dest = Path(out) / "roadviewer/app/web"
served = {"library.html": "/?fragment=1", "index.html": f"/view/{record}/?fragment=1"}
count = 0
for path in sorted(Path(web).rglob("*")):
    if not path.is_file(): continue
    rel = path.relative_to(web).as_posix()
    url = server + served.get(rel, "/assets/" + rel)
    with urllib.request.urlopen(url, timeout=30) as r:
        body = r.read()
    target = dest / rel
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(body)
    count += 1
print(f"mirrored {count} files from HARS")
