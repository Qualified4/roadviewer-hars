"""End-to-end check of copying Python Road Viewer recordings into HARS in the real executable, against the Python
suite's loopback Drive fake (reference/python/tests/fake_drive.py) and a temporary share folder. Nothing reaches Google
or a real share.

Usage: python3 integration/legacy_import_e2e.py BINARY RLOG [RLOG_B] [--port 18210]
The HARS UI binds PORT, the device API PORT+1 and the Drive fake PORT+2 (all loopback). The qcamera.ts next to each
rlog (same route--segment prefix) is part of the Python recording.

The Drive fake holds Python's own layout: a folder `Road Viewer` (appProperties rv_storage_id) with files
`<id>--<kind>` (rv_id, rv_kind, rv_sha256, rv_storage_id). With HARS's own archive switched off, HARS signs in to the
fake, scans the Python folder, copies the recording, converts it, plays a Range of its video (206), deletes it and
restarts. With RLOG_B the same runs from a read-only share folder (`<id>/<kind>`, sentinel owner `roadviewer`). Every
request naming a Python file must be a GET, and Python's files, appProperties and share bytes must be unchanged.
"""
import argparse, hashlib, json, os, signal, stat, subprocess, sys, tempfile, threading, time, urllib.request, urllib.error
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'reference/python/tests'))
import fake_drive  # noqa: E402

p = argparse.ArgumentParser()
p.add_argument('binary')
p.add_argument('rlog')
p.add_argument('rlog_b', nargs='?')
p.add_argument('--port', type=int, default=18210)
args = p.parse_args()

FOLDER = 'application/vnd.google-apps.folder'
PY_SID, PY_FOLDER = 'e2e-python-sid', 'pyfolder'
PY_A, PY_B = 'c0ffee00000000000000000000000001', 'c0ffee00000000000000000000000002'


class Drive(fake_drive.FakeDrive):
    """The fake on a fixed loopback port."""
    def __init__(self, port):
        fake_drive.ThreadingHTTPServer.__init__(self, ('127.0.0.1', port), fake_drive.Handler)
        self.url = 'http://127.0.0.1:%d' % port
        self.lock = threading.Lock()
        self.scenario = fake_drive.Scenario()
        self.files, self.sessions, self.requests, self.revoked, self.issued = {}, {}, [], [], {}
        self.access_tokens, self.refresh_tokens = set(), set()
        self.file_count = self.session_count = self.token_count = self.device_code_count = 0
        self.waiting = self.most_waiting = 0
        self.thread = None


BASE = 'http://127.0.0.1:%d' % args.port


def call(method, path, body=None, ok=(200, 201, 202)):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={'X-RoadViewer-Request': '1', 'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(req, timeout=60) as r: status, text = r.status, r.read()
    except urllib.error.HTTPError as e: status, text = e.code, e.read()
    out = json.loads(text) if text else None
    assert status in ok, (method, path, status, out)
    return status, out


def wait(what, check, seconds=600):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            value = check()
            if value: return value
        except (AssertionError, OSError, urllib.error.URLError): pass
        time.sleep(0.5)
    raise SystemExit('timed out waiting for ' + what)


def start(env):
    proc = subprocess.Popen([args.binary], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait('server', lambda: call('GET', '/health'), 60)
    return proc


def stop(proc):
    proc.send_signal(signal.SIGTERM)
    proc.wait(30)


def fetch(path, headers=None):
    req = urllib.request.Request(BASE + path, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=120) as r: return r.status, {k.lower(): v for k, v in r.headers.items()}, r.read()
    except urllib.error.HTTPError as e: return e.code, dict(e.headers), e.read()


def sha(b): return hashlib.sha256(b).hexdigest()


def parts(rlog):
    rlog = Path(rlog)
    out = {'rlog.zst': rlog.read_bytes()}
    camera = Path(str(rlog).replace('--rlog.zst', '--qcamera.ts'))
    assert camera != rlog and camera.is_file(), 'the rlog needs its qcamera.ts beside it'
    out['qcamera.ts'] = camera.read_bytes()
    return out


def python_drive(drive, files):
    with drive.lock:
        drive.files[PY_FOLDER] = {'name': 'Road Viewer', 'mimeType': FOLDER, 'parents': [], 'appProperties': {'rv_storage_id': PY_SID},
                                  'content': b'', 'trashed': False, 'md5': None, 'sha256': None, 'created': '2025-01-01T00:00:00.000000Z'}
        for kind, content in files.items():
            drive.files['py-' + kind] = {
                'name': '%s--%s' % (PY_A, kind), 'mimeType': 'application/octet-stream', 'parents': [PY_FOLDER],
                'appProperties': {'rv_id': PY_A, 'rv_kind': kind, 'rv_sha256': sha(content), 'rv_storage_id': PY_SID},
                'content': content, 'trashed': False, 'md5': hashlib.md5(content).hexdigest(), 'sha256': sha(content),
                'created': '2025-01-01T00:00:01.000000Z'}


def drive_snapshot(drive):
    with drive.lock:
        return {k: json.dumps({**v, 'content': sha(v['content'])}, sort_keys=True)
                for k, v in drive.files.items() if k == PY_FOLDER or PY_FOLDER in v['parents']}


def tree(path):
    out = {}
    for p in sorted(Path(path).rglob('*')) + [Path(path)]:
        st = p.lstat()
        out[str(p)] = (st.st_mode, st.st_mtime_ns, sha(p.read_bytes()) if p.is_file() else None)
    return out


def read_only(path, on):
    for p in sorted(Path(path).rglob('*'), reverse=True) + [Path(path)]:
        p.chmod((0o555 if p.is_dir() else 0o444) if on else (0o755 if p.is_dir() else 0o644))


def row(id):
    _, out = call('GET', '/api/logs')
    return next(m for m in out['logs'] if m['id'] == id)


def finished():
    def done():
        _, s = call('GET', '/api/settings/archive/legacy/status')
        return s['job'] if not s['running'] and s['job'] and s['job']['state'] != 'running' else None
    return wait('legacy import', done)


def copied(source, python_id, files):
    """Import, convert, play a range, read the originals back; returns the HARS id."""
    call('POST', '/api/settings/archive/legacy/import', {**source, 'ids': [python_id]}, ok=(202,))
    job = finished()
    assert job['state'] == 'done' and job['records'][0]['state'] == 'imported', job
    hars_id = job['records'][0]['hars_id']
    wait('conversion', lambda: row(hars_id)['status'] in ('ready', 'error'))
    assert row(hars_id)['status'] == 'ready', row(hars_id)
    status, headers, body = fetch('/api/logs/%s/video?source=qcamera' % hars_id, {'Range': 'bytes=0-1023'})
    assert status == 206 and len(body) == 1024 and headers.get('content-range', '').startswith('bytes 0-1023/'), (status, headers)
    for kind, content in files.items():
        status, _, body = fetch('/api/logs/%s/original/%s' % (hars_id, kind))
        assert status == 200 and sha(body) == sha(content), (kind, status)
    return hars_id, job


report = {}
with tempfile.TemporaryDirectory(prefix='hars-legacy-e2e-') as tmp, Drive(args.port + 2) as drive:
    tmp = Path(tmp)
    data_dir, share = tmp / 'data', tmp / 'python-share'
    files_a = parts(args.rlog)
    python_drive(drive, files_a)
    drive_before = drive_snapshot(drive)
    if args.rlog_b:
        files_b = parts(args.rlog_b)
        (share / PY_B).mkdir(parents=True)
        (share / '.roadviewer-storage.json').write_text(json.dumps({'storage_id': 'e2e-python-nas', 'created_at': 1.0, 'owner': 'roadviewer'}))
        for kind, content in files_b.items(): (share / PY_B / kind).write_bytes(content)
        read_only(share, True)
        share_before = tree(share)
    env = dict(os.environ, HARS_DATA=str(data_dir), HARS_INGRESS_ONLY='0', HARS_BIND='127.0.0.1:%d' % args.port,
               HARS_DEVICE_BIND='127.0.0.1:%d' % (args.port + 1), HARS_WEB=str(ROOT / 'reference/python/roadviewer/app/web'),
               RV_ARCHIVE_ALLOW_ANY_PATH='1',
               RV_ARCHIVE_OPTIONS_JSON=json.dumps({'archive_gdrive_client_id': 'e2e-client', 'archive_gdrive_client_secret': 'e2e-secret'}),
               **drive.env())
    proc = start(env)
    try:
        drive_source = {'backend': 'gdrive', 'folder_name': 'Road Viewer'}
        status, out = call('POST', '/api/settings/archive/legacy/scan', drive_source, ok=(409,))
        assert out == {'error': 'gdrive_not_connected'}, out
        # HARS's own archive stays off: signing in only gives the legacy reader a grant.
        call('POST', '/api/settings/archive', {'archive_enabled': False, 'archive_backend': 'gdrive'})
        call('POST', '/api/settings/archive/gdrive/connect', ok=(202,))
        wait('drive connected', lambda: call('POST', '/api/settings/archive/legacy/scan', drive_source, ok=(200, 409))[0] == 200, 60)
        _, scan = call('POST', '/api/settings/archive/legacy/scan', drive_source)
        assert scan['source']['storage_id'] == PY_SID and scan['total'] == 1, scan
        rec = scan['records'][0]
        assert rec['id'] == PY_A and rec['importable'] and rec['in_hars'] is None, rec
        mark = len(drive.requests)
        hars_a, job = copied(drive_source, PY_A, files_a)
        report['drive'] = {'imported': hars_a, 'verified_by': [f['verified_by'] for f in job['records'][0]['files']]}
        _, again = call('POST', '/api/settings/archive/legacy/scan', drive_source)
        assert again['records'][0]['in_hars'] == {'id': hars_a, 'match': 'id'} or again['records'][0]['in_hars']['id'] == hars_a, again
        call('POST', '/api/settings/archive/legacy/import', {**drive_source, 'ids': [PY_A]}, ok=(202,))
        assert finished()['records'][0]['state'] == 'duplicate'
        call('DELETE', '/api/logs/' + hars_a)
        stop(proc); proc = start(env)
        _, after = call('POST', '/api/settings/archive/legacy/scan', drive_source)
        assert after['records'][0]['in_hars'] is None and len(after['records'][0]['files']) == 2, after
        with drive.lock: named = [r for r in drive.requests[mark:] if any(k in r['path'] for k in drive_before)]
        assert named and all(r['method'] == 'GET' for r in named), [r for r in named if r['method'] != 'GET']
        assert drive_snapshot(drive) == drive_before, 'Python Drive data changed'
        report['drive_requests_to_python'] = sorted({r['method'] for r in named})
        if args.rlog_b:
            nas_source = {'backend': 'smb', 'path': str(share)}
            _, scan = call('POST', '/api/settings/archive/legacy/scan', nas_source)
            assert scan['records'][0]['id'] == PY_B and scan['records'][0]['importable'], scan
            hars_b, job = copied(nas_source, PY_B, files_b)
            assert job['records'][0]['files'][0]['verified_by'] == ['size', 'readback'], job
            call('DELETE', '/api/logs/' + hars_b)
            assert tree(share) == share_before, 'Python share changed'
            report['share'] = {'imported': hars_b, 'read_only_mount': True}
        report['result'] = 'python data read only; copies converted, played (206) and deleted in HARS'
    finally:
        stop(proc)
        if args.rlog_b: read_only(share, False)
print(json.dumps(report))
