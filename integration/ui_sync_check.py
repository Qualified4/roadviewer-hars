"""Check the Python Road Viewer 1.3.17-1.3.18 UI changes on a running HARS replay page
(`/view/<id>/`) in an isolated headless browser.

Same harness as ui_view_check.py: a loopback proxy serves the HARS UI under its own origin plus
one harness page that loads the replay page in a same-origin iframe and operates it. The record
must have video and GPS and belong to a drive with at least two segments.

1.3.17 map vehicle colour: the nine colour choices on the map's options and in the display
settings, a colour applied to the map tab's vehicle and to the small map over the video, kept
after a reload (localStorage), the direction arrow coloured too, and 자동 keeping the theme's
look. 1.3.18: the vehicle's body and windows drawn opaque; a segment card held and pushed up off
the strip grows to 1.3x at the top of its travel, under the title, which sits clear of the app
bar (the geometry tests/segment_peek.cjs asserts, with the same pointer and touch events
dispatched from script).

The browser gets a new temporary profile and cannot resolve any outside host: map tiles are not
fetched, so a real drive's position does not leave the machine. Writes one JSON result; items it
could not judge are reported as such, not as passes.

usage: ui_sync_check.py --browser EXE --profile DIR --port N --server URL --record ID --output result.json
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
<iframe id=f style="width:1280px;height:900px;border:0"></iframe><script>
const sleep = ms => new Promise(ok => setTimeout(ok, ms));
const until = async (test, ms, step = 100) => { for (let t = 0; t < ms; t += step) { try { const v = test(); if (v) return v; } catch (e) {} await sleep(step); } return null; };
const report = r => fetch('/__result', {method: 'POST', body: JSON.stringify(r)});
const COLORS = [['auto','자동'],['white','흰색'],['mint','민트'],['blue','파랑'],['cyan','하늘'],['violet','보라'],['rose','분홍'],['amber','주황'],['red','빨강']];
const HEX = {white:'#fff', blue:'#8cbcff', rose:'#f2a4bd'};
(async () => {
  const r = {checks: {}};
  const set = (name, pass, detail) => { r.checks[name] = {pass: !!pass, detail}; };
  const f = document.getElementById('f');
  let w, d, $;
  const load = async (width, height) => {
    f.style.width = width + 'px'; f.style.height = height + 'px';
    const loaded = new Promise(ok => f.addEventListener('load', ok, {once: true}));
    f.src = '/view/%RECORD%/?' + Date.now(); await loaded;
    w = f.contentWindow; d = f.contentDocument; $ = id => d.getElementById(id);
    return until(() => $('play') && !$('play').disabled && d.querySelectorAll('#segmentStrip .segment-block').length, 30000);
  };
  // A colour as the page computes it: rgb(...) of an element given `color: value` in the same document.
  const rgb = value => { const probe = d.createElement('span'); probe.style.color = value; d.body.append(probe); const c = w.getComputedStyle(probe).color; probe.remove(); return c; };
  const resolved = (el, name) => { const probe = d.createElement('span'); probe.style.color = `var(${name})`; el.append(probe); const c = w.getComputedStyle(probe).color; probe.remove(); return c; };
  const car = root => d.querySelector(root + ' .map-car-body');
  const look = body => { if (!body) return null; const cs = w.getComputedStyle(body), shell = body.querySelector('.map-vehicle-shell'), glass = body.querySelector('.map-vehicle-glass'), path = body.querySelector('svg path');
    const g = e => e && w.getComputedStyle(e);
    return {icon: body.dataset.carIcon, color: body.dataset.carColor, var: body.style.getPropertyValue('--map-car-color'), ink: cs.color, background: cs.backgroundColor,
      shell: shell && {fill: g(shell).fill, opacity: g(shell).fillOpacity, stroke: g(shell).stroke}, glass: glass && {fill: g(glass).fill, opacity: g(glass).fillOpacity},
      arrow: path && {fill: g(path).fill}, ego: resolved(body.parentElement, '--map-ego'), label: resolved(body.parentElement, '--map-label'), accent: resolved(body.parentElement, '--accent')}; };
  const choices = root => root ? [...root.querySelectorAll('[data-map-color]')].map(b => [b.dataset.mapColor, b.title, b.textContent.trim(), b.getAttribute('aria-pressed')]) : null;
  const pressed = root => root && [...root.querySelectorAll('[data-map-color][aria-pressed=true]')].map(b => b.dataset.mapColor);
  try {
    if (!await load(1280, 900)) throw new Error('replay page did not become ready');
    r.segments = d.querySelectorAll('#segmentStrip .segment-block').length;
    try { w.localStorage.removeItem('roadviewer-map-car-color'); w.localStorage.removeItem('roadviewer-map-car-icon'); } catch (e) {}
    $('mapTab').click();
    const big = await until(() => car('#routeMapView'), 20000);
    if (!big) throw new Error('no vehicle on the map tab');
    // (가) nine choices, the same in the map's options and in the display settings
    d.querySelector('#routeMapView .rv-map-options-button')?.click();
    const pop = await until(() => d.querySelector('.rv-map-options:not([hidden]) .map-car-colors'), 5000);
    const settings = $('mapCarColors');
    const want = COLORS.map(([k, n]) => [k, n, n]);
    const strip3 = list => list && list.map(c => c.slice(0, 3));
    set('a_color_choices', pop && settings && JSON.stringify(strip3(choices(pop))) === JSON.stringify(want) && JSON.stringify(strip3(choices(settings))) === JSON.stringify(want)
        && JSON.stringify(pressed(pop)) === '["auto"]' && JSON.stringify(pressed(settings)) === '["auto"]',
        {map_options: choices(pop), display_settings: choices(settings), group_label: settings && settings.querySelector('[role=group]')?.getAttribute('aria-label')});
    // (라) 자동 keeps the theme's look; (바) the body and windows are opaque
    let a = look(big);
    set('d_auto_keeps_theme_look', a.icon === 'vehicle' && a.color === 'auto' && a.var === '' && a.ink === a.ego && a.shell.fill === a.ego && a.glass.fill === a.label, a);
    set('f_vehicle_opaque', a.shell.opacity === '1' && a.glass.opacity === '1', {shell: a.shell, glass: a.glass});
    // (나) a colour reaches the map tab's vehicle and the small map's
    pop.querySelector('[data-map-color=blue]').click();
    a = look(car('#routeMapView'));
    const blue = rgb(HEX.blue), ink = rgb('#17212b');
    const bigBlue = a.color === 'blue' && a.var === HEX.blue && a.ink === blue && a.shell.fill === blue && a.glass.fill === ink && a.shell.opacity === '1';
    $('videoMiniMap').checked || $('videoMiniMap').click();
    $('roadTab').click();
    const mini = await until(() => !$('videoMap').hidden && car('#videoMap'), 20000);
    const m = look(mini);
    set('b_color_on_both_maps', bigBlue && m && m.color === 'blue' && m.var === HEX.blue && m.ink === blue && m.shell.fill === blue
        && w.localStorage.getItem('roadviewer-map-car-color') === 'blue' && JSON.stringify(pressed(settings)) === '["blue"]',
        {map_tab: a, small_map: m, stored: w.localStorage.getItem('roadviewer-map-car-color'), settings_pressed: pressed(settings)});
    // the display settings' choice changes the small map shown right now; (마) the arrow is coloured too
    settings.querySelector('[data-map-color=rose]').click();
    $('mapCarIcon').querySelector('[data-map-car=arrow]').click();
    const rose = rgb(HEX.rose);
    const arrow = look(car('#videoMap'));
    set('e_arrow_colored', arrow && arrow.icon === 'arrow' && arrow.color === 'rose' && arrow.var === HEX.rose && arrow.background === rose && arrow.arrow?.fill === ink,
        {small_map: arrow, map_tab: look(car('#routeMapView'))});
    // (다) kept after a reload
    if (!await load(1280, 900)) throw new Error('replay page did not become ready after the reload');
    const kept = await until(() => !$('videoMap').hidden && car('#videoMap'), 20000);
    const k = look(kept);
    set('c_kept_after_reload', k && k.color === 'rose' && k.icon === 'arrow' && k.var === HEX.rose && k.background === rose
        && JSON.stringify(pressed($('mapCarColors'))) === '["rose"]' && w.localStorage.getItem('roadviewer-map-car-color') === 'rose',
        {small_map: k, settings_pressed: pressed($('mapCarColors')), stored: w.localStorage.getItem('roadviewer-map-car-color')});
    // 자동 again restores the theme defaults (the original test's last colour step)
    $('mapCarColors').querySelector('[data-map-color=auto]').click();
    $('mapCarIcon').querySelector('[data-map-car=vehicle]').click();
    const back = look(car('#videoMap'));
    set('d_auto_restores_theme_look', back.color === 'auto' && back.var === '' && back.icon === 'vehicle' && back.ink === back.ego && back.shell.fill === back.ego && back.glass.fill === back.label
        && back.shell.opacity === '1', back);
    $('videoMiniMap').checked && $('videoMiniMap').click();
  } catch (e) { r.map_error = String(e); }
  try {
    // (사) phone: hold a segment, push it to the top
    if (!await load(390, 844)) throw new Error('phone replay page did not become ready');
    const strip = $('segmentStrip'), blocks = strip.querySelectorAll('.segment-block');
    const target = blocks[Math.min(1, blocks.length - 1)], br = target.getBoundingClientRect(), x = br.left + br.width / 2, y = br.top + br.height / 2;
    const pointer = (type, el, px, py) => el.dispatchEvent(new w.PointerEvent(type, {pointerType: 'touch', pointerId: 7, isPrimary: true, clientX: px, clientY: py, bubbles: true, cancelable: true}));
    const move = (px, py) => { let e; try { e = new w.TouchEvent('touchmove', {bubbles: true, cancelable: true, touches: [new w.Touch({identifier: 7, target, clientX: px, clientY: py})]}); }
      catch (err) { e = new w.Event('touchmove', {bubbles: true, cancelable: true}); Object.defineProperty(e, 'touches', {value: [{clientX: px, clientY: py}]}); }
      target.dispatchEvent(e); };
    const card = $('segmentGestureCard');
    pointer('pointerdown', target, x, y); await sleep(550);
    const held = card && card.classList.contains('is-shown');
    const rest = card ? card.getBoundingClientRect().top : null;
    move(x, y - 30); await sleep(250); move(x, y - 100); await sleep(250);
    const armed = card && card.classList.contains('is-armed'), title = d.querySelector('.sg-title');
    const mid = card ? card.getBoundingClientRect().width : null;
    move(x, 10); await sleep(300);
    let top = null;
    if (card) { const c = card.getBoundingClientRect(), t = title.getBoundingClientRect(), bar = d.querySelector('.app-bar').getBoundingClientRect();
      top = {card: [c.left, c.top, c.right, c.width], title: [t.top, t.bottom], bar: bar.bottom, vw: d.documentElement.clientWidth, base: card.offsetWidth, shown: title.classList.contains('is-shown'), num: d.querySelector('.sg-num')?.textContent}; }
    const g = top && {
      held, armed, title_shown: top.shown,
      scale_at_top: top.card[3] / top.base, grows_on_the_way: mid > top.base * 1.02 && mid < top.card[3] - 4,
      under_title: top.card[1] >= top.title[1] + 15, on_screen: top.card[0] >= 7.5 && top.card[2] <= top.vw - 7.5,
      clear_of_app_bar: top.title[0] >= top.bar + 19.5, well_below_top: top.title[0] >= (rest - top.bar) * .2};
    set('g_segment_push_grows_under_title', g && held && armed && top.shown && Math.abs(g.scale_at_top - 1.3) < .01 && g.grows_on_the_way && g.under_title && g.on_screen && g.clear_of_app_bar && g.well_below_top,
        {...g, mid, rest, top});
    move(x, y); await sleep(150); pointer('pointercancel', strip, x, y); await sleep(600);
    r.stayed = f.contentWindow.location.pathname;
  } catch (e) { r.gesture_error = String(e); }
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
                                "--no-default-browser-check", "--disable-extensions", "--window-size=1400,1000",
                                "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1",
                                f"http://127.0.0.1:{args.port}/__check"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    finished = done.wait(timeout=args.timeout)
    browser.terminate()
    try: browser.wait(timeout=10)
    except subprocess.TimeoutExpired: browser.kill(); browser.wait()
    server.shutdown()
    checks = result.get("checks", {})
    out = {"finished": finished, "browser": "headless=new, new temporary profile, no outside hosts, harness iframe on the HARS origin", **result}
    args.output.write_text(json.dumps(out, indent=1, ensure_ascii=False))
    errors = {k: result[k] for k in ("map_error", "gesture_error") if result.get(k)}
    print(json.dumps({"finished": finished, "checks": {k: v["pass"] for k, v in checks.items()}, "errors": errors}, ensure_ascii=False))
    raise SystemExit(0 if finished and len(checks) == 8 and all(v["pass"] for v in checks.values()) and not errors else 1)


if __name__ == "__main__":
    main()
