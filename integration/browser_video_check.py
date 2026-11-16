"""Headless browser playback check of converted MP4s.

Serves the given folders over a loopback Range server and drives an isolated
headless Chromium-family browser (new temporary profile, nothing else open)
through every video: loadedmetadata wait, video.duration, seeks to 50 % and
99.5 %, readyState after each, play/pause, the time reported at `ended` after
playing the last 0.3 s, and media errors. Writes one JSON result list.
Usage: browser_video_check.py --browser EXE --profile DIR --port N --runs 3
       --output result.json label=folder [label=folder ...]
"""
import argparse
import json
import os
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import threading
import time

PAGE = r"""<!doctype html><meta charset=utf-8><body><script>
const items = %ITEMS%, runs = %RUNS%;
const wait = (target, name, ms) => new Promise((ok, fail) => {
  const timer = setTimeout(() => fail(new Error(name + ' timeout')), ms);
  target.addEventListener(name, () => { clearTimeout(timer); ok(); }, {once: true});
});
async function check(url) {
  const v = document.createElement('video'); v.muted = true; v.preload = 'auto';
  document.body.append(v);
  const r = {url, error: null}; const t0 = performance.now();
  try {
    v.src = url;
    await wait(v, 'loadedmetadata', 15000);
    r.metadata_ms = Math.round(performance.now() - t0); r.duration = v.duration;
    for (const [name, at] of [['middle', 0.5], ['end', 0.995]]) {
      const p = wait(v, 'seeked', 15000); v.currentTime = v.duration * at; await p;
      r[name + '_ready'] = v.readyState; r[name + '_time'] = v.currentTime;
    }
    v.currentTime = 0; await wait(v, 'seeked', 15000);
    await v.play(); await new Promise(ok => setTimeout(ok, 400));
    r.played_to = v.currentTime; v.pause(); r.paused = v.paused;
    const p = wait(v, 'seeked', 15000); v.currentTime = Math.max(0, v.duration - 0.3); await p;
    const ended = wait(v, 'ended', 5000); await v.play(); await ended;
    r.ended_time = v.currentTime; r.ended_minus_duration = v.currentTime - v.duration;
  } catch (e) { r.error = String(e); }
  r.media_error = v.error ? v.error.code : null; r.can_play = v.canPlayType(url.includes('fcamera') ? 'video/mp4; codecs="hvc1"' : 'video/mp4; codecs="avc1.4d4028"');
  v.remove(); return r;
}
(async () => {
  for (let run = 1; run <= runs; run++)
    for (const url of items) {
      const r = await check(url); r.run = run;
      await fetch('/result', {method: 'POST', body: JSON.stringify(r)});
    }
  await fetch('/done', {method: 'POST'});
})();
</script>"""


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--browser", required=True);p.add_argument("--profile", required=True)
    p.add_argument("--port", type=int, required=True);p.add_argument("--runs", type=int, default=3)
    p.add_argument("--output", type=Path, required=True);p.add_argument("folders", nargs="+")
    args = p.parse_args()
    roots = dict(f.split("=", 1) for f in args.folders)
    items = []
    for label, folder in roots.items():
        for path in sorted(Path(folder).glob("segment-*/*.mp4"), key=lambda p: (int(p.parent.name.split("-")[1]), p.name)):
            items.append(f"/{label}/{path.parent.name}/{path.name}")
    results, done = [], threading.Event()

    class Handler(SimpleHTTPRequestHandler):
        def log_message(self, *a):pass
        def translate_path(self, path):
            label, _, rest = path.lstrip("/").partition("/")
            return str(Path(roots[label]) / rest) if label in roots else "/nonexistent"
        def do_GET(self):
            if self.path == "/":
                body = PAGE.replace("%ITEMS%", json.dumps(items)).replace("%RUNS%", str(args.runs)).encode()
                self.send_response(200);self.send_header("Content-Type", "text/html");self.send_header("Content-Length", str(len(body)));self.end_headers();self.wfile.write(body);return
            path = Path(self.translate_path(self.path.split("?")[0]))
            if not path.is_file():
                self.send_error(404);return
            size = path.stat().st_size;start, end = 0, size - 1
            ranged = self.headers.get("Range", "").startswith("bytes=")
            if ranged:
                a, _, b = self.headers["Range"][6:].partition("-")
                start = int(a) if a else size - int(b);end = int(b) if a and b else size - 1
            self.send_response(206 if ranged else 200)
            self.send_header("Content-Type", "video/mp4");self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Length", str(end - start + 1))
            if ranged:self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            self.end_headers()
            try:
                with path.open("rb") as f:
                    f.seek(start);left = end - start + 1
                    while left > 0:
                        chunk = f.read(min(left, 1 << 20))
                        if not chunk:break
                        self.wfile.write(chunk);left -= len(chunk)
            except (BrokenPipeError, ConnectionResetError):pass
        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            if self.path == "/result":results.append(json.loads(body))
            elif self.path == "/done":done.set()
            self.send_response(204);self.end_headers()

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    browser = subprocess.Popen([args.browser, "--headless=new", f"--user-data-dir={args.profile}", "--no-first-run",
                                "--no-default-browser-check", "--autoplay-policy=no-user-gesture-required",
                                "--disable-extensions", f"http://127.0.0.1:{args.port}/"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    finished = done.wait(timeout=60 * 30)
    browser.terminate()
    server.shutdown()
    args.output.write_text(json.dumps({"finished": finished, "items": len(items), "runs": args.runs, "results": results}, indent=1))
    print(json.dumps({"finished": finished, "results": len(results), "errors": sum(1 for r in results if r["error"] or r["media_error"])}))


if __name__ == "__main__":main()
