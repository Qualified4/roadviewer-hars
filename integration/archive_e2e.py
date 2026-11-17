"""End-to-end check of the archiver in the real executable, against the Python suite's loopback Drive fake
(reference/python/tests/fake_drive.py) and a temporary NAS folder. Nothing reaches Google or a real share.

Usage: python3 integration/archive_e2e.py BINARY RLOG_A RLOG_B [RLOG_C] [--port 18180]
The HARS UI binds PORT, the device API PORT+1 and the Drive fake PORT+2 (all loopback).
A qcamera.ts next to an rlog (same route--segment prefix) is uploaded with it: after each move the recording's
video plays (Range 206), its stored files download with their SHA-256, it converts again from the archive, and the
same holds after a restart. Recording A is first copied with its original video kept, so the next conversion
deletes the original and its Drive copy becomes the stored file (Python `promote_remote`); its MP4 then moves to
Drive and plays from there, byte for byte. With RLOG_C, a copied MP4 made again by a conversion loses its copy
(Python `drop_remote` in `clear_prepared`) while the Drive file stays.
"""
import argparse, hashlib, json, os, shutil, signal, subprocess, sys, tempfile, threading, time, urllib.request, urllib.error
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'reference/python/tests'))
import fake_drive  # noqa: E402

p = argparse.ArgumentParser()
p.add_argument('binary')
p.add_argument('rlog_a')
p.add_argument('rlog_b')
p.add_argument('rlog_c', nargs='?')
p.add_argument('--port', type=int, default=18180)
args = p.parse_args()


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


def call(method, path, body=None, raw=None, ok=(200, 201, 202)):
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={'X-RoadViewer-Request': '1', 'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(req, timeout=60) as r: status, text = r.status, r.read()
    except urllib.error.HTTPError as e: status, text = e.code, e.read()
    out = json.loads(text) if text else None
    assert status in ok, (method, path, status, out)
    return out


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


def upload(path, name):
    files = [(Path(path), name)]
    camera = Path(str(path).replace('--rlog.zst', '--qcamera.ts'))
    if camera != Path(path) and camera.is_file():
        files.append((camera, name.replace('--rlog.zst', '--qcamera.ts')))
    blobs = [f.read_bytes() for f, _ in files]
    s = call('POST', '/api/uploads', {'files': [{'name': n, 'size': len(b)} for (_, n), b in zip(files, blobs)]})
    chunk = s['chunk_size']
    for index, data in enumerate(blobs):
        for offset in range(0, len(data), chunk):
            call('PUT', '/api/uploads/%s/files/%d?offset=%d' % (s['id'], index, offset), raw=data[offset:offset + chunk])
    result = call('POST', '/api/uploads/%s/finish' % s['id'])
    return result['logs'][0]['id']


def fetch(path, headers=None):
    req = urllib.request.Request(BASE + path, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=120) as r: return r.status, {k.lower(): v for k, v in r.headers.items()}, r.read()
    except urllib.error.HTTPError as e: return e.code, dict(e.headers), e.read()


STORED = ('rlog.zst', 'qcamera.ts', 'fcamera.hevc', 'ecamera.hevc', 'camera.mp4', 'fcamera.mp4', 'ecamera.mp4')


def hashes(data_dir, id):
    # The stored files while they are still local: what every later download must return.
    return {k: hashlib.sha256((data_dir / id / k).read_bytes()).hexdigest() for k in STORED if (data_dir / id / k).is_file()}


def reads_back(id, expected, where, mp4=None):
    """The moved recording plays and downloads as before (Python media_file/remote_response)."""
    m = row(id)
    assert m['storage_locations'].get('rlog') == where, m.get('storage_locations')
    for kind, digest in expected.items():
        status, _, body = fetch('/api/logs/%s/original/%s' % (id, kind))
        assert status == 200 and hashlib.sha256(body).hexdigest() == digest, (kind, status)
    if m.get('video'):
        status, headers, body = fetch('/api/logs/%s/video?source=qcamera' % id, {'Range': 'bytes=0-1023'})
        assert status == 206 and len(body) == 1024 and headers.get("content-range", "").startswith('bytes 0-1023/'), (status, headers)
        if mp4 is not None:
            assert body == mp4[:1024], 'first bytes of the stored MP4'
            mid = len(mp4) // 2
            status, headers, body = fetch('/api/logs/%s/video?source=qcamera' % id, {'Range': 'bytes=%d-%d' % (mid, mid + 65535)})
            assert status == 206 and body == mp4[mid:mid + 65536], (status, headers.get('content-range'))
            assert headers['content-range'] == 'bytes %d-%d/%d' % (mid, mid + 65535, len(mp4)), headers
    return sorted(expected)


def convert_again(id):
    """Replay data made again from the archived inputs (staged from Drive, read in place on the share)."""
    call('DELETE', '/api/logs/%s/prepared' % id)
    assert call('POST', '/api/logs/%s/convert' % id)['status'] == 'queued'
    wait('reconversion', lambda: row(id)['status'] in ('ready', 'error'))
    assert row(id)['status'] == 'ready', row(id)
    assert not (data_dir / id / ".staging").exists(), "staged copies are removed"
    status, _, _ = fetch('/api/logs/%s/data' % id)
    assert status == 200, status


def row(id):
    return next((r for r in call('GET', '/api/logs')['logs'] if r['id'] == id), None)


def meta(data_dir, id):
    return json.loads((data_dir / id / 'meta.json').read_text())


def run_manual():
    call('POST', '/api/settings/archive/run', ok=(202,))
    job = wait('manual run', lambda: (lambda a: a['manual'] if not a['manual_active'] and a['manual'] and a['manual']['state'] != 'running' else None)(call('GET', '/api/settings/storage')['archive']))
    return job


report = {}
with tempfile.TemporaryDirectory(prefix='hars-archive-e2e-') as tmp, Drive(args.port + 2) as drive:
    tmp = Path(tmp)
    data_dir, nas = tmp / 'data', tmp / 'nas'
    nas.mkdir()
    env = dict(os.environ, HARS_DATA=str(data_dir), HARS_INGRESS_ONLY='0', HARS_BIND='127.0.0.1:%d' % args.port,
               HARS_DEVICE_BIND='127.0.0.1:%d' % (args.port + 1), HARS_WEB=str(ROOT / 'web'),
               RV_ARCHIVE_ALLOW_ANY_PATH='1',
               RV_ARCHIVE_OPTIONS_JSON=json.dumps({'archive_gdrive_client_id': 'e2e-client', 'archive_gdrive_client_secret': 'e2e-secret'}),
               **drive.env())
    proc = start(env)
    try:
        # Drive: sign in through the device flow; copy mode while the original video is kept (the default).
        call('POST', '/api/settings/archive', {'archive_enabled': True, 'archive_backend': 'gdrive', 'archive_min_age_hours': 0,
                                               'archive_mode': 'copy'})
        code = call('POST', '/api/settings/archive/gdrive/connect', ok=(202,))
        assert code['user_code'] and 'e2e-secret' not in json.dumps(code)
        wait('drive connected', lambda: call('GET', '/api/settings/storage')['archive']['gdrive']['connected'], 60)
        a = upload(args.rlog_a, '00000459--27751e7c28--1--rlog.zst')
        wait('conversion', lambda: row(a)['status'] == 'ready' or row(a)['status'] == 'error' and SystemExit(row(a)))
        assert row(a)['status'] == 'ready', row(a)
        expected_a = hashes(data_dir, a)
        assert 'qcamera.ts' in expected_a, 'RLOG_A needs its qcamera.ts beside it'
        job = run_manual()
        assert job['state'] == 'done', job
        copy = meta(data_dir, a)['storage']['qcamera.ts']
        assert copy['location'] == 'local' and copy['remote']['sha256'] == expected_a['qcamera.ts'], copy
        # The next publication deletes the original; its verified copy becomes the stored file.
        call('POST', '/api/settings/processing', {'keep_original_video': False})
        convert_again(a)
        promoted = meta(data_dir, a)['storage']['qcamera.ts']
        assert promoted['location'] == 'gdrive' and promoted['file_id'] == copy['remote']['file_id'], promoted
        assert promoted['sha256'] == expected_a['qcamera.ts'] and not (data_dir / a / 'qcamera.ts').exists(), promoted
        with drive.lock: assert hashlib.sha256(drive.files[promoted['file_id']]['content']).hexdigest() == expected_a['qcamera.ts']
        mp4_a = (data_dir / a / 'camera.mp4').read_bytes()
        expected_a['camera.mp4'] = hashlib.sha256(mp4_a).hexdigest()
        report['drive_promoted'] = 'qcamera.ts copy became the stored file at publication'
        # Move mode with deleting allowed: the MP4 and the log move to Drive.
        call('POST', '/api/settings/archive', {'archive_mode': 'move', 'archive_allow_local_delete': True})
        job = run_manual()
        assert job['state'] == 'done' and job['succeeded'] >= 1, job
        m = meta(data_dir, a)
        entry = m['storage']['rlog.zst']
        assert entry['location'] == 'gdrive' and not (data_dir / a / 'rlog.zst').exists(), entry
        file_id = entry['file_id']
        with drive.lock: f = drive.files[file_id]
        assert f['appProperties']['hars_owner'] == 'roadviewer-hars' and f['appProperties']['rv_id'] == a
        folder = drive.files[f['parents'][0]]
        assert folder['name'] == 'Road Viewer HARS', folder['name']
        listed = row(a)
        assert listed['storage_locations']['rlog'] == 'gdrive' and 'storage' not in listed, listed
        report['drive_archived'] = {'files': job['succeeded'], 'bytes': job['bytes_done']}
        moved_mp4 = meta(data_dir, a)['storage']['camera.mp4']
        assert moved_mp4['location'] == 'gdrive' and not (data_dir / a / 'camera.mp4').exists(), moved_mp4
        report['drive_reads'] = reads_back(a, expected_a, 'gdrive', mp4_a)
        report['drive_video'] = 'MP4 played from Drive: Range 206 bytes equal the stored MP4'
        convert_again(a)
        # Restart: the index survives, the archive reconnects with the saved token.
        stop(proc); proc = start(env)
        assert meta(data_dir, a)['storage']['rlog.zst'] == entry
        wait('drive connected after restart', lambda: call('GET', '/api/settings/storage')['archive']['state'] == 'connected', 60)
        assert row(a)['storage_locations']['rlog'] == 'gdrive'
        reads_back(a, expected_a, 'gdrive', mp4_a)
        report['drive_reconverted'] = 'from staged Drive copies, read again after restart'
        # Delete: queued, then removed from Drive by the next cycle.
        call('DELETE', '/api/logs/' + a)
        pending = json.loads((data_dir / '.pending-nas-deletes.json').read_text())
        assert pending[0]['id'] == a and pending[0]['explicit'] is True and file_id in [r['file_id'] for r in pending[0]['refs']], pending
        with drive.lock: assert file_id in drive.files
        run_manual()
        with drive.lock: assert file_id not in drive.files
        assert not (data_dir / '.pending-nas-deletes.json').exists()
        report['drive_delete'] = 'removed after the next cycle'

        if args.rlog_c:
            # A copied MP4 made again by a conversion: its copy no longer describes it and leaves the index; the
            # Drive file is left where it is.
            call('POST', '/api/settings/archive', {'archive_mode': 'copy'})
            call('POST', '/api/settings/processing', {'keep_original_video': True})
            c = upload(args.rlog_c, '00000459--27751e7c28--3--rlog.zst')
            wait('conversion', lambda: row(c)['status'] in ('ready', 'error'))
            assert row(c)['status'] == 'ready', row(c)
            assert run_manual()['state'] == 'done'
            call('POST', '/api/settings/processing', {'keep_original_video': False})
            convert_again(c)
            assert meta(data_dir, c)['storage']['qcamera.ts']['location'] == 'gdrive'
            assert run_manual()['state'] == 'done'
            block = meta(data_dir, c)['storage']['camera.mp4']
            assert block['location'] == 'local' and block['remote']['file_id'], block
            convert_again(c)
            m = meta(data_dir, c)
            assert 'remote' not in m['storage'].get('camera.mp4', {}), m['storage'].get('camera.mp4')
            assert not (data_dir / c / 'camera.mp4').exists() and (data_dir / c / 'prepared' / 'camera.mp4').is_file()
            with drive.lock: assert block['remote']['file_id'] in drive.files, 'the superseded copy stays on Drive'
            status, _, body = fetch('/api/logs/%s/video?source=qcamera' % c, {'Range': 'bytes=0-1023'})
            assert status == 206 and body == (data_dir / c / 'prepared' / 'camera.mp4').read_bytes()[:1024], status
            call('DELETE', '/api/logs/' + c)
            run_manual()
            report['drive_dropped'] = 'copied MP4 made again: copy dropped from the index, Drive file kept'
        else:
            report['drive_dropped'] = 'not run (no RLOG_C)'

        # NAS: a temporary folder (any path allowed for the test), always moving.
        call('POST', '/api/settings/archive', {'archive_backend': 'smb', 'archive_path': str(nas), 'archive_nas_margin_mb': 0})
        b = upload(args.rlog_b, '00000459--27751e7c28--2--rlog.zst')
        wait('conversion', lambda: row(b)['status'] in ('ready', 'error'))
        assert row(b)['status'] == 'ready', row(b)
        expected_b = hashes(data_dir, b)
        mp4_b = (data_dir / b / 'camera.mp4').read_bytes()
        job = run_manual()
        assert job['state'] == 'done', job
        e = meta(data_dir, b)['storage']['rlog.zst']
        assert e['location'] == 'nas' and (nas / b / 'rlog.zst').is_file() and not (data_dir / b / 'rlog.zst').exists(), e
        sentinel = json.loads((nas / '.roadviewer-storage.json').read_text())
        assert sentinel['owner'] == 'roadviewer-hars'
        report['nas_reads'] = reads_back(b, expected_b, 'nas', mp4_b)
        convert_again(b)
        stop(proc); proc = start(env)
        assert meta(data_dir, b)['storage']['rlog.zst'] == e
        wait('share connected after restart', lambda: call('GET', '/api/settings/storage')['archive']['state'] == 'connected', 60)
        reads_back(b, expected_b, 'nas', mp4_b)
        block = call('GET', '/api/settings/storage')['archive']
        assert block['archived_files'] >= 1 and block['state'] == 'connected', block
        call('DELETE', '/api/logs/' + b)
        assert not (nas / b).exists() and not (data_dir / '.pending-nas-deletes.json').exists()
        report['nas'] = 'archived, kept across restart, deleted with the recording'
        text = json.dumps(call('GET', '/api/settings/storage')) + json.dumps(call('GET', '/api/settings/archive'))
        assert 'e2e-secret' not in text and 'secret' not in text.replace('client_configured', '')
    finally:
        stop(proc)
print(json.dumps(report))
