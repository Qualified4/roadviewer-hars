"""Decoded frames vs host FFmpeg framemd5: bitexact.py OUT INSTR_BINARY INPUT...
Checks the whole 140-packet window (HARS_THUMB_ALLFRAMES) and the frames a thumbnail run outputs."""
import json, os, subprocess, sys, tempfile
from pathlib import Path
out = Path(sys.argv[1]); binary = sys.argv[2]; inputs = sys.argv[3:]
out.mkdir(parents=True, exist_ok=True)
rows = []
def lines(path):
    return [l.split() for l in Path(path).read_text().splitlines()] if Path(path).exists() else []
for src in inputs:
    name = Path(src).stem
    ff = subprocess.run(['ffmpeg', '-v', 'error', '-threads', '1', '-i', src, '-map', '0:v:0', '-frames:v', '160',
                         '-pix_fmt', 'yuv420p', '-f', 'framemd5', '-'], capture_output=True, text=True).stdout
    ff = [l.split(',')[-1].strip() for l in ff.splitlines() if l and not l.startswith('#')]
    with tempfile.TemporaryDirectory(dir=out) as t:
        allf, thumb = Path(t) / 'all', Path(t) / 'thumb'
        subprocess.run([binary, '__thumbnail', src, str(Path(t) / 'x.jpg')], env=dict(os.environ, HARS_THUMB_ALLFRAMES=str(allf)), capture_output=True)
        subprocess.run([binary, '__thumbnail', src, str(Path(t) / 'y.jpg')], env=dict(os.environ, HARS_THUMB_FRAMES=str(thumb)), capture_output=True)
        full, used = lines(allf), lines(thumb)
    index = {l[0]: i for i, l in enumerate(full)}
    window_same = sum(1 for i, l in enumerate(full) if i < len(ff) and l[2] == ff[i])
    used_same = sum(1 for l in used if l[0] in index and index[l[0]] < len(ff) and ff[index[l[0]]] == l[2])
    row = {'input': name, 'size': full[0][1] if full else None, 'window_frames': len(full), 'window_identical': window_same,
           'thumbnail_frames_output': len(used), 'thumbnail_frames_identical': used_same}
    rows.append(row); print(row, flush=True)
s = {k: sum(r[k] for r in rows) for k in ('window_frames', 'window_identical', 'thumbnail_frames_output', 'thumbnail_frames_identical')}
s['inputs'] = len(rows)
print(json.dumps(s))
(out / 'bitexact.json').write_text(json.dumps({'summary': s, 'rows': rows}, indent=1))
