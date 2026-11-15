#!/usr/bin/env python3
"""Same requests against the original Python Road Viewer and HARS on loopback; compares status codes and
JSON key sets, and moves exports across (Python export -> HARS import, HARS export -> Python import).

Both servers use new temporary data folders only. Ports default to 18141 (Python) and 18142/18143 (HARS);
8098 is never used. Run with the reference venv (Flask, PyAV, pycapnp):

  ~/hars-venvs/apiparity/bin/python integration/api_contract_compare.py \
      --rust target/debug/roadviewer-hars --report /tmp/api-compare.json
"""
import argparse, hashlib, io, json, os, shutil, signal, subprocess, sys, tarfile, tempfile, time, urllib.error, urllib.request, uuid, zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
APP = ROOT / 'reference/python/roadviewer/app'
HEADERS = {'X-RoadViewer-Request': '1'}


class Server:
    def __init__(self, name, base):
        self.name, self.base = name, base

    def request(self, method, path, body=None, headers=None, raw=False):
        data = body if isinstance(body, bytes) or body is None else json.dumps(body).encode()
        h = dict(HEADERS)
        if data is not None and not isinstance(body, bytes): h['Content-Type'] = 'application/json'
        h.update(headers or {})
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=h)
        try:
            with urllib.request.urlopen(req, timeout=60) as r: status, payload, rh = r.status, r.read(), r.headers
        except urllib.error.HTTPError as e: status, payload, rh = e.code, e.read(), e.headers
        if raw: return status, payload, rh
        try: value = json.loads(payload)
        except ValueError: value = None
        return status, value, rh


def wait(url, proc, seconds=60):
    end = time.time() + seconds
    while time.time() < end:
        if proc.poll() is not None: raise RuntimeError(f'{url} exited with {proc.returncode}')
        try:
            urllib.request.urlopen(url + '/api/progress', timeout=2).read(); return
        except Exception: time.sleep(.3)
    raise RuntimeError(f'{url} did not start')


def keys(v):
    if isinstance(v, dict): return sorted(v)
    if isinstance(v, list) and v and isinstance(v[0], dict): return ['[]'] + sorted(set().union(*map(set, v)))
    return type(v).__name__


def chunked_upload(s, files):
    status, begun, _ = s.request('POST', '/api/uploads', {'files': [{'name': n, 'size': len(d)} for n, d in files]})
    assert status == 201, (s.name, status, begun)
    for i, (_, data) in enumerate(files):
        assert s.request('PUT', f"/api/uploads/{begun['id']}/files/{i}?offset=0", data, {'Content-Type': 'application/octet-stream'})[0] == 200
    return s.request('POST', f"/api/uploads/{begun['id']}/finish")


def multipart(files):
    boundary = uuid.uuid4().hex
    out = io.BytesIO()
    for name, data in files:
        out.write(f'--{boundary}\r\nContent-Disposition: form-data; name="files"; filename="{name}"\r\nContent-Type: application/octet-stream\r\n\r\n'.encode())
        out.write(data + b'\r\n')
    out.write(f'--{boundary}--\r\n'.encode())
    return out.getvalue(), {'Content-Type': f'multipart/form-data; boundary={boundary}'}


def browser_import(s, tar_bytes):
    """log-transfer.js: one import session per recording of an export file."""
    results = []
    with tarfile.open(fileobj=io.BytesIO(tar_bytes)) as tar:
        names = tar.getnames()
        manifest = json.loads(tar.extractfile(names[0]).read())
        groups = {}
        for n in names[1:]: groups.setdefault(n.split('/')[0], []).append(n)
        for rid, members in groups.items():
            record = json.loads(tar.extractfile(f'{rid}/record.json').read())
            status, result, _ = s.request('POST', '/api/logs/import', {'format': manifest['format'], 'version': manifest['version'], 'record': record})
            if status == 201 and result.get('session'):
                for i, kind in enumerate(result['files']):
                    data = tar.extractfile(f'{rid}/{kind}').read()
                    for off in range(0, len(data), result['chunk_size']):
                        assert s.request('PUT', f"/api/uploads/{result['session']}/files/{i}?offset={off}", data[off:off + result['chunk_size']], {'Content-Type': 'application/octet-stream'})[0] == 200
                status, result, _ = s.request('POST', f"/api/logs/import/{result['session']}/finish")
            results.append((status, result))
    return results


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--rust', required=True)
    parser.add_argument('--report', required=True)
    parser.add_argument('--python-port', type=int, default=18141)
    parser.add_argument('--rust-port', type=int, default=18142)
    parser.add_argument('--device-port', type=int, default=18143)
    args = parser.parse_args()
    assert 8098 not in (args.python_port, args.rust_port, args.device_port)
    work = Path(tempfile.mkdtemp(prefix='hars-api-compare-'))
    procs = []
    try:
        env = {**os.environ, 'RV_DATA': str(work / 'python-data'), 'RV_INGRESS_ONLY': '0', 'PYTHONDONTWRITEBYTECODE': '1'}
        env.pop('SUPERVISOR_TOKEN', None)
        procs.append(subprocess.Popen([sys.executable, '-c', f"import sys;sys.path.insert(0,'.');import server;server.app.run('127.0.0.1',{args.python_port},threaded=True)"],
                                      cwd=APP, env=env, stdout=subprocess.DEVNULL, stderr=open(work / 'python.log', 'w')))
        renv = {**os.environ, 'HARS_DATA': str(work / 'hars-data'), 'HARS_INGRESS_ONLY': '0', 'HARS_BIND': f'127.0.0.1:{args.rust_port}',
                'HARS_DEVICE_BIND': f'127.0.0.1:{args.device_port}', 'HARS_WEB': str(APP / 'web')}
        renv.pop('SUPERVISOR_TOKEN', None)
        procs.append(subprocess.Popen([args.rust], cwd=ROOT, env=renv, stdout=open(work / 'hars.log', 'w'), stderr=subprocess.STDOUT))
        py, rs = Server('python', f'http://127.0.0.1:{args.python_port}'), Server('hars', f'http://127.0.0.1:{args.rust_port}')
        wait(py.base, procs[0]); wait(rs.base, procs[1])
        rows = []

        def both(label, method, path, body=None, headers=None, fill=None):
            out = {}
            for s in (py, rs):
                p = path(s) if callable(path) else path
                status, value, h = s.request(method, p, body, headers)
                out[s.name] = {'status': status, 'keys': keys(value), 'error': value.get('error') if isinstance(value, dict) else None,
                               'cache_control': h.get('Cache-Control'), 'content_type': (h.get('Content-Type') or '').split(';')[0]}
                if fill: fill(s, value)
            row = {'case': label, 'python': out['python'], 'hars': out['hars'],
                   'same_status': out['python']['status'] == out['hars']['status'], 'same_keys': out['python']['keys'] == out['hars']['keys']}
            rows.append(row)
            return row

        both('settings auto_convert off', 'POST', '/api/settings/processing', {'auto_convert': False})
        both('empty list', 'GET', '/api/logs')
        ids = {}
        log, ts = b'synthetic-rlog-' + os.urandom(8), b'synthetic-ts-' + os.urandom(8)
        for s in (py, rs):
            status, result, _ = chunked_upload(s, [('1a2b3c4d--0123456789--4--rlog.zst', log), ('1a2b3c4d--0123456789--4--qcamera.ts', ts)])
            assert status == 201, (s.name, status, result)
            ids[s.name] = result['logs'][0]['id']
            body, h = multipart([('1a2b3c4d--0123456789--5--rlog.zst', b'legacy-' + log)])
            status, result, _ = s.request('POST', '/api/upload', body, h)
            ids[s.name + '-legacy'] = (status, keys(result))
        rows.append({'case': 'legacy multipart upload', 'python': ids['python-legacy'], 'hars': ids['hars-legacy'], 'same_status': ids['python-legacy'][0] == ids['hars-legacy'][0], 'same_keys': ids['python-legacy'][1] == ids['hars-legacy'][1]})
        rid = lambda s: ids[s.name]
        row = both('list with logs', 'GET', '/api/logs')
        lists = {s.name: s.request('GET', '/api/logs')[1]['logs'] for s in (py, rs)}
        row['row_keys'] = {n: sorted(set().union(*map(set, v))) for n, v in lists.items()}
        row['row_keys_python_only'] = sorted(set(row['row_keys']['python']) - set(row['row_keys']['hars']))
        row['row_keys_hars_only'] = sorted(set(row['row_keys']['hars']) - set(row['row_keys']['python']))
        row['order_same'] = [r['name'] for r in lists['python']] == [r['name'] for r in lists['hars']]
        for name in ('video_downloads', 'download_options', 'video_download', 'bytes', 'log_bytes', 'files'):
            row[f'field_{name}_same'] = [r.get(name) for r in lists['python']] == [r.get(name) for r in lists['hars']]
        both('bookmark create', 'POST', lambda s: f'/api/logs/{rid(s)}/bookmarks', {'time': 1, 'title': ' a  b ', 'note': ''})
        both('bookmark invalid title', 'POST', lambda s: f'/api/logs/{rid(s)}/bookmarks', {'time': 1, 'title': 'a​', 'note': ''})
        both('bookmark wrong keys', 'POST', lambda s: f'/api/logs/{rid(s)}/bookmarks', {'time': 1})
        both('bookmark missing item', 'PUT', lambda s: f'/api/logs/{rid(s)}/bookmarks/nope', {'time': 1, 'title': 'a', 'note': ''})
        both('bookmark list', 'GET', lambda s: f'/api/logs/{rid(s)}/bookmarks')
        both('tags set', 'POST', lambda s: f'/api/logs/{rid(s)}/tags', {'tags': [{'name': 'Zeta'}, {'name': 'alpha', 'color': 'red'}]})
        both('tags bad color', 'POST', lambda s: f'/api/logs/{rid(s)}/tags', {'tags': [{'name': 'x', 'color': 'black'}]})
        both('pin', 'POST', lambda s: f'/api/logs/{rid(s)}/pin', {'pinned': True})
        both('tracks', 'GET', '/api/tracks')
        both('track', 'GET', lambda s: f'/api/logs/{rid(s)}/track')
        both('progress', 'GET', '/api/progress')
        both('devices', 'GET', '/api/settings/devices')
        both('pairing', 'GET', '/api/settings/devices/pairing')
        both('device network without Supervisor', 'GET', '/api/settings/device-network')
        both('device network restart without Supervisor', 'POST', '/api/settings/device-network/restart', {})
        both('device network invalid', 'POST', '/api/settings/device-network', {'enabled': True, 'port': 0})
        both('upload failure report', 'POST', f'/api/uploads/{uuid.uuid4().hex}/failure', {'stage': 'chunk'})
        both('upload begin invalid', 'POST', '/api/uploads', ['x'])
        both('unknown path', 'GET', '/api/nothing')
        both('missing log data', 'GET', f'/api/logs/{uuid.uuid4().hex}/data')
        both('csrf header missing', 'POST', '/api/settings/processing', {'auto_convert': False}, {'X-RoadViewer-Request': '0'})
        both('import invalid', 'POST', '/api/logs/import', {'format': 'roadviewer-export', 'version': 1, 'record': {'id': '../x', 'name': 'n', 'originals': {}}})
        both('import bad version', 'POST', '/api/logs/import', {'format': 'roadviewer-export', 'version': 2, 'record': {}})
        both('download.zip bad parts', 'GET', lambda s: f'/api/logs/download.zip?ids={rid(s)}&parts=meta')
        # Downloads: names and headers.
        for label, path in (('download rlog', lambda s: f'/api/logs/{rid(s)}/download'), ('original rlog', lambda s: f'/api/logs/{rid(s)}/original/rlog.zst'),
                            ('original ts', lambda s: f'/api/logs/{rid(s)}/original/qcamera.ts')):
            out = {}
            for s in (py, rs):
                status, payload, h = s.request('GET', path(s), raw=True)
                out[s.name] = {'status': status, 'sha256': hashlib.sha256(payload).hexdigest(), 'disposition': h.get('Content-Disposition'), 'cache_control': h.get('Cache-Control'), 'content_type': h.get('Content-Type')}
            rows.append({'case': label, **out, 'same_status': out['python']['status'] == out['hars']['status'], 'same_keys': out['python']['sha256'] == out['hars']['sha256'],
                         'same_disposition': out['python']['disposition'] == out['hars']['disposition']})
        zips = {}
        for s in (py, rs):
            status, payload, h = s.request('GET', f'/api/logs/download.zip?ids={rid(s)}&parts=rlog,qcamera', raw=True)
            with zipfile.ZipFile(io.BytesIO(payload)) as z:
                assert z.testzip() is None
                zips[s.name] = sorted((i.filename, hashlib.sha256(z.read(i)).hexdigest()) for i in z.infolist())
        rows.append({'case': 'download.zip contents', 'python': zips['python'], 'hars': zips['hars'], 'same_status': True, 'same_keys': zips['python'] == zips['hars']})
        # Cross-implementation export/import.
        exports = {s.name: s.request('GET', '/api/logs/export.tar', raw=True)[1] for s in (py, rs)}
        for source, target in ((py, rs), (rs, py)):
            results = browser_import(target, exports[source.name])
            check = []
            for status, result in results:
                if status == 201:
                    digest = hashlib.sha256(target.request('GET', f"/api/logs/{result['imported']}/original/rlog.zst", raw=True)[1]).hexdigest()
                    check.append({'status': status, 'imported': True, 'rlog_sha_in_source': digest in (hashlib.sha256(log).hexdigest(), hashlib.sha256(b'legacy-' + log).hexdigest())})
                else: check.append({'status': status, 'result': result})
            rows.append({'case': f'{source.name} export -> {target.name} import', 'results': check, 'same_status': True,
                         'same_keys': all(c.get('imported') and c.get('rlog_sha_in_source') or c.get('result', {}).get('skipped') for c in check)})
        imported = {s.name: s.request('GET', '/api/logs')[1]['logs'] for s in (py, rs)}
        rows.append({'case': 'tags/bookmarks after cross import', 'python': sorted(json.dumps(r.get('tags')) for r in imported['python']),
                     'hars': sorted(json.dumps(r.get('tags')) for r in imported['hars']), 'same_status': True,
                     'same_keys': sorted(json.dumps(r.get('tags')) for r in imported['python']) == sorted(json.dumps(r.get('tags')) for r in imported['hars'])})
        report = {'python_port': args.python_port, 'rust_port': args.rust_port, 'cases': rows,
                  'summary': {'cases': len(rows), 'same_status': sum(r['same_status'] for r in rows), 'same_keys': sum(bool(r['same_keys']) for r in rows)}}
        Path(args.report).write_text(json.dumps(report, ensure_ascii=False, indent=1))
        print(json.dumps(report['summary']))
    finally:
        for p in procs:
            if p.poll() is None: p.send_signal(signal.SIGTERM)
        for p in procs:
            try: p.wait(timeout=15)
            except subprocess.TimeoutExpired: p.kill()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    main()
