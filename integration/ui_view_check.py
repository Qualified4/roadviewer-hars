"""Drives the real HARS replay page (`/view/<id>/`) in an isolated headless browser.

A loopback proxy serves the HARS UI under its own origin plus one harness page
that loads `/view/<id>/` in a same-origin iframe and operates it the way a user
does: camera choice, play/pause, seeks to the middle and the end through the
seek bar, the video overlay canvas, the map tab (track request and map element)
and the vehicle graph tab (telemetry request and graph elements). The browser
gets a new temporary profile and nothing else opens in it. Writes one JSON
result; items it could not judge are reported as such, not as passes.

usage: ui_view_check.py --browser EXE --profile DIR --port N --server URL --record ID --output result.json
"""
import argparse
import http.client
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import threading
import urllib.parse

HARNESS = r"""<!doctype html><meta charset=utf-8>
<iframe id=f style="width:1280px;height:900px" src="/view/%RECORD%/"></iframe><script>
const sleep = ms => new Promise(ok => setTimeout(ok, ms));
const until = async (test, ms, step = 100) => { for (let t = 0; t < ms; t += step) { try { const v = test(); if (v) return v; } catch (e) {} await sleep(step); } return null; };
const report = r => fetch('/__result', {method: 'POST', body: JSON.stringify(r)});
const once = (target, name, ms) => new Promise(ok => { const t = setTimeout(() => ok(false), ms); target.addEventListener(name, () => { clearTimeout(t); ok(true); }, {once: true}); });
(async () => {
  const r = {checks: {}};
  const set = (name, pass, detail) => { r.checks[name] = {pass, detail}; };
  try {
    const f = document.getElementById('f');
    await once(f, 'load', 30000);
    const w = f.contentWindow, d = f.contentDocument, $ = id => d.getElementById(id);
    const v = await until(() => $('video') && $('video').readyState >= 1 && $('video'), 30000);
    set('video_metadata', !!v, v ? {src: v.currentSrc.replace(location.origin, ''), duration: v.duration} : null);
    if (!v) throw new Error('no video metadata');
    const src = $('videoSource'), options = [...src.options].map(o => o.value);
    r.cameras = options;
    const start = src.value, other = options.find(o => o !== start);
    if (other) {
      const before = v.currentSrc;
      src.value = other; src.dispatchEvent(new Event('change', {bubbles: true}));
      const switched = await until(() => v.currentSrc !== before && v.readyState >= 1, 20000);
      set('camera_switch', !!switched, {from: start, to: other, src: v.currentSrc.replace(location.origin, ''), error: v.error && v.error.code});
      src.value = start; src.dispatchEvent(new Event('change', {bubbles: true}));
      await until(() => v.currentSrc === before && v.readyState >= 1, 20000);
    } else set('camera_switch', null, 'only one camera');
    const play = $('play');
    // The page disables the button while a camera source loads.
    await until(() => !play.disabled && v.readyState >= 2, 20000);
    play.click();
    const advanced = await until(() => !v.paused && v.currentTime > 0.3, 8000);
    set('play', !!advanced, {time: v.currentTime, paused: v.paused});
    play.click();
    await sleep(300);
    const t = v.currentTime; await sleep(500);
    set('pause', v.paused && v.currentTime === t, {paused: v.paused, time: v.currentTime});
    const seek = $('seek');
    for (const [name, at] of [['seek_middle', 0.5], ['seek_end', 0.995]]) {
      const max = Number(seek.max) || 1, target = Number(seek.min || 0) + (max - Number(seek.min || 0)) * at;
      seek.value = String(target);
      seek.dispatchEvent(new Event('input', {bubbles: true})); seek.dispatchEvent(new Event('change', {bubbles: true}));
      const ok = await until(() => !v.seeking && v.readyState >= 2 && Math.abs(v.currentTime - v.duration * at) < Math.max(1, v.duration * 0.03), 15000);
      set(name, !!ok, {seek_value: seek.value, seek_max: seek.max, time: v.currentTime, duration: v.duration, ready: v.readyState, error: v.error && v.error.code});
    }
    const canvas = $('videoOverlay');
    let drawn = null;
    if (canvas && !canvas.hidden && canvas.width && canvas.height) {
      const px = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data;
      let painted = 0; for (let i = 3; i < px.length; i += 4) if (px[i]) painted++;
      drawn = painted;
    }
    set('overlay_canvas', drawn === null ? null : drawn > 0, {hidden: canvas ? canvas.hidden : null, width: canvas && canvas.width, height: canvas && canvas.height, painted_pixels: drawn});
    const resources = () => w.performance.getEntriesByType('resource').map(e => e.name.replace(location.origin, ''));
    $('mapTab').click();
    const track = await until(() => resources().find(n => n.includes('/track')), 15000);
    const map = await until(() => { const c = d.querySelector('#routeMapView .route-map-canvas'); return c && c.querySelector('svg,canvas,img,path,.leaflet-pane,div') && c; }, 15000);
    set('map', !!(track && map), {track_request: track, map_children: map ? map.querySelectorAll('*').length : 0, map_visible: map ? map.getBoundingClientRect().height > 0 : false});
    $('telemetryTab').click();
    const telemetry = await until(() => resources().find(n => n.includes('telemetry')), 20000);
    const graphs = await until(() => { const g = $('telemetryGraphs'); return g && g.querySelector('canvas,svg') && g; }, 20000);
    set('vehicle_graphs', !!(telemetry && graphs), {telemetry_request: telemetry, graph_elements: graphs ? graphs.querySelectorAll('canvas,svg').length : 0, status: $('telemetryStatus') ? $('telemetryStatus').textContent.trim().slice(0, 80) : null});
    r.page_errors = $('error') && !$('error').hidden ? $('errorText').textContent.slice(0, 200) : null;
  } catch (e) { r.error = String(e); }
  await report(r);
})();
</script>"""


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--browser", required=True); p.add_argument("--profile", required=True)
    p.add_argument("--port", type=int, required=True); p.add_argument("--server", required=True)
    p.add_argument("--record", required=True); p.add_argument("--output", type=Path, required=True)
    p.add_argument("--timeout", type=int, default=240)
    args = p.parse_args()
    upstream = urllib.parse.urlsplit(args.server)
    result, done = {}, threading.Event()

    class Proxy(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def log_message(self, *a): pass
        def forward(self, body=None):
            if self.path.startswith("/__check"):
                page = HARNESS.replace("%RECORD%", args.record).encode()
                self.send_response(200); self.send_header("Content-Type", "text/html"); self.send_header("Content-Length", str(len(page))); self.end_headers(); self.wfile.write(page); return
            conn = http.client.HTTPConnection(upstream.hostname, upstream.port, timeout=60)
            headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection")}
            conn.request(self.command, self.path, body=body, headers=headers)
            response = conn.getresponse()
            self.send_response(response.status)
            for k, v in response.getheaders():
                if k.lower() not in ("connection", "transfer-encoding", "keep-alive"):
                    self.send_header(k, v)
            data = response.read()
            if not response.getheader("Content-Length"):
                self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            try: self.wfile.write(data)
            except (BrokenPipeError, ConnectionResetError): pass
            conn.close()
        def do_GET(self): self.forward()
        def do_HEAD(self): self.forward()
        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            if self.path == "/__result":
                result.update(json.loads(body)); done.set()
                self.send_response(204); self.send_header("Content-Length", "0"); self.end_headers(); return
            self.forward(body)
        def do_PUT(self): self.do_POST()

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Proxy)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    browser = subprocess.Popen([args.browser, "--headless=new", f"--user-data-dir={args.profile}", "--no-first-run",
                                "--no-default-browser-check", "--autoplay-policy=no-user-gesture-required",
                                "--disable-extensions", "--window-size=1400,1000", f"http://127.0.0.1:{args.port}/__check"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    finished = done.wait(timeout=args.timeout)
    browser.terminate()
    server.shutdown()
    out = {"finished": finished, "browser": "headless=new, new temporary profile, harness iframe on the HARS origin", **result}
    args.output.write_text(json.dumps(out, indent=1))
    print(json.dumps({"finished": finished, "checks": {k: v["pass"] for k, v in result.get("checks", {}).items()}, "error": result.get("error")}))


if __name__ == "__main__":
    main()
