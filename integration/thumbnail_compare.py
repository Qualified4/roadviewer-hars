"""85-input thumbnail equivalence: thumbs85.py OUT BASE_REL NEW_REL BASE_INSTR NEW_INSTR INPUT..."""
import hashlib, json, os, subprocess, sys
from pathlib import Path
out = Path(sys.argv[1]); bins = dict(zip(['base', 'new', 'base_i', 'new_i'], sys.argv[2:6])); inputs = sys.argv[6:]
rows = []
def run(binary, src, d, env=None):
    d.mkdir(parents=True, exist_ok=True)
    for f in d.iterdir():
        f.unlink()
    r = subprocess.run([binary, '__thumbnail', src, str(d / 'thumbnail.jpg')], capture_output=True, env=dict(os.environ, **(env or {})))
    files = {f.name: f.read_bytes() for f in d.iterdir()}
    return r.returncode, r.stdout.decode().strip(), files
def sof(jpeg):
    i = jpeg.find(b'\xff\xc0')
    return [int.from_bytes(jpeg[i+7:i+9], 'big'), int.from_bytes(jpeg[i+5:i+7], 'big')] if i >= 0 else None
for src in inputs:
    name = Path(src).stem
    d = out / name
    res = {k: run(b, src, d / k, {'HARS_THUMB_DUMP': '1', 'HARS_THUMB_FRAMES': str(d / (k + '.frames'))} if k.endswith('_i') else None) for k, b in bins.items()}
    frames = {}
    for k in ('base_i', 'new_i'):
        p = d / (k + '.frames')
        frames[k] = dict(l.split(' ', 1) for l in p.read_text().splitlines()) if p.exists() else {}
        if p.exists():
            p.unlink()
    nb, nn = frames['base_i'], frames['new_i']
    (b_rc, b_out, b_f), (n_rc, n_out, n_f) = res['base'], res['new']
    row = {
        'input': name, 'exit_same': b_rc == n_rc == 0,
        'stdout_same': b_out == n_out, 'result': json.loads(n_out) if n_out else None,
        'json_same': b_f.get('thumbnail.json') == n_f.get('thumbnail.json') and 'thumbnail.json' in n_f,
        'jpeg_same': b_f.get('thumbnail.jpg') == n_f.get('thumbnail.jpg') and 'thumbnail.jpg' in n_f,
        'size': sof(n_f.get('thumbnail.jpg', b'')), 'size_same': sof(b_f.get('thumbnail.jpg', b'')) == sof(n_f.get('thumbnail.jpg', b'')),
        'jpeg_sha256': hashlib.sha256(n_f.get('thumbnail.jpg', b'')).hexdigest(),
        'instr_jpeg_same_as_release': res['new_i'][2].get('thumbnail.jpg') == n_f.get('thumbnail.jpg') and res['base_i'][2].get('thumbnail.jpg') == b_f.get('thumbnail.jpg'),
        'candidates_same': res['base_i'][2].get('thumbnail.cand') == res['new_i'][2].get('thumbnail.cand') and 'thumbnail.cand' in res['new_i'][2],
        'levelled_same': res['base_i'][2].get('thumbnail.levelled') == res['new_i'][2].get('thumbnail.levelled') and 'thumbnail.levelled' in res['new_i'][2],
        'frames_output_base': len(nb), 'frames_output_new': len(nn),
        'output_frames_equal_to_base': bool(nn) and all(nb.get(t) == v for t, v in nn.items()),
    }
    rows.append(row)
    print(name, {k: v for k, v in row.items() if k not in ('input', 'jpeg_sha256')}, flush=True)
keys = ['exit_same', 'stdout_same', 'json_same', 'jpeg_same', 'size_same', 'instr_jpeg_same_as_release', 'candidates_same', 'levelled_same', 'output_frames_equal_to_base']
summary = {'inputs': len(rows), **{k: sum(r[k] for r in rows) for k in keys},
           'frames_output_base_total': sum(r['frames_output_base'] for r in rows), 'frames_output_new_total': sum(r['frames_output_new'] for r in rows),
           'seconds': sorted({r['result']['second'] for r in rows if r['result']})}
print(json.dumps(summary))
(out / 'thumbs85.json').write_text(json.dumps({'summary': summary, 'rows': rows}, indent=1))
