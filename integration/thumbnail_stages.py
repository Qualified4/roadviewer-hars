"""Stage timings of one __thumbnail worker run (instrumented build): stages.py BINARY OUTDIR RUNS INPUT..."""
import json, os, statistics, subprocess, sys, tempfile, time
from pathlib import Path
binary, out, runs, inputs = sys.argv[1], Path(sys.argv[2]), int(sys.argv[3]), sys.argv[4:]
out.mkdir(parents=True, exist_ok=True)
result = {}
def one(src):
    with tempfile.TemporaryDirectory(dir=out) as t:
        prof = Path(t) / 'p.jsonl'
        env = dict(os.environ, HARS_PROFILE_FILE=str(prof))
        start = time.perf_counter()
        r = subprocess.run([binary, '__thumbnail', src, str(Path(t) / 'thumbnail.jpg')], env=env, capture_output=True)
        total = time.perf_counter() - start
        stages = json.loads(prof.read_text().splitlines()[-1])['stages'] if prof.exists() and prof.read_text() else {}
        return total, stages, r.returncode
for src in inputs + ['/nonexistent.mp4']:
    one(src)
    rows = [one(src) for _ in range(runs)]
    name = Path(src).stem
    stages = {}
    for k in sorted({k for _, s, _ in rows for k in s}):
        stages[k] = {
            'ms': round(statistics.median(s[k]['wall_seconds'] * 1000 for _, s, _ in rows if k in s), 2),
            'calls': statistics.median(s[k]['calls'] for _, s, _ in rows if k in s),
        }
    total = statistics.median(t for t, _, _ in rows) * 1000
    result[name] = {'total_ms': round(total, 2), 'exit': rows[0][2], 'stages': stages}
    print(name, 'total %.1f ms exit %d' % (total, rows[0][2]))
    for k, v in stages.items():
        print('   %-20s %8.2f ms  calls %s' % (k, v['ms'], v['calls']))
(out / 'stages.json').write_text(json.dumps(result, indent=1))
