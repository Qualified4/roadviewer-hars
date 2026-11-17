"""Differential fuzzing of the thumbnail worker: damaged and crafted camera MP4s through two
`__thumbnail` workers (before and after a decoder change), compared case by case.

    thumbnail_differential.py WORK OLD_BINARY NEW_BINARY [--h264 MP4...] [--hevc MP4...]
        [--synthetic DIR] [--cases 300] [--seed 20261117] [--keep]

Each seed MP4 (read only) is first rebuilt with only its first 150 samples in one `mdat`. With
--synthetic, seeds of other encoder structures are added (made in DIR with the host's FFmpeg when
missing): H.264 with 4 and 16 reference pictures, with 4 slices, and at 1920x1080; HEVC with 4
slices. A case applies one to three random changes to a seed of its codec: flipped bytes, NAL
units deleted, duplicated or reordered, a slice deleted, a parameter set (the track's, damaged or
not) put in band, junk NAL units, a NAL unit other than a slice with the forbidden bit set, a box
renamed, the file cut short. Both workers run under an address-space limit and a time limit;
their exit status, output line, `thumbnail.json` and JPEG bytes must be the same. WORK receives
`thumbnail-differential.json` and, with --keep, every case that differs.
"""
import argparse
import hashlib
import json
import random
import shutil
import struct
import subprocess
from pathlib import Path

LIMIT = 2147483648
KEEP = 150
STBL = [b'moov', b'trak', b'mdia', b'minf', b'stbl']


def walk(buf, start, end):
    at = start
    while at + 8 <= end:
        size, kind = struct.unpack('>I4s', buf[at:at + 8])
        header = 8
        if size == 1:
            size, header = struct.unpack('>Q', buf[at + 8:at + 16])[0], 16
        elif size == 0:
            size = end - at
        if size < header or at + size > end:
            return
        yield kind, at, header, size
        at += size


def find(buf, path, start=0, end=None):
    """(box start, payload start, box end) of the first box at `path`."""
    end = len(buf) if end is None else end
    for kind, at, header, size in walk(buf, start, end):
        if kind == path[0]:
            inner = at + header + {b'stsd': 8, b'avc1': 78, b'hvc1': 78}.get(kind, 0)
            return (at, inner, at + size) if len(path) == 1 else find(buf, path[1:], inner, at + size)
    raise KeyError(path)


def table(buf, kind, skip, width, entry='>I'):
    _, at, _ = find(buf, STBL + [kind])
    at += 4 + skip
    n = struct.unpack('>I', buf[at:at + 4])[0]
    step = struct.calcsize(entry)
    return at + 4, [struct.unpack(entry, buf[at + 4 + step * i:at + 4 + step * (i + 1)])[0] for i in range(n * width)]


def layout(buf):
    """Sample offsets and sizes, and where the size and chunk tables are."""
    sizes_at, sizes = table(buf, b'stsz', 4, 1)
    if not sizes:
        raise ValueError('constant sample size')
    try:
        chunks_at, chunks = table(buf, b'stco', 0, 1)
        wide = False
    except KeyError:
        chunks_at, chunks = table(buf, b'co64', 0, 1, '>Q')
        wide = True
    _, runs = table(buf, b'stsc', 0, 3)
    runs = [runs[i:i + 3] for i in range(0, len(runs), 3)]
    offsets, per_chunk = [], []
    for c, offset in enumerate(chunks):
        per = [r for r in runs if r[0] <= c + 1][-1][1]
        per_chunk.append(per)
        for _ in range(per):
            if len(offsets) == len(sizes):
                break
            offsets.append(offset)
            offset += sizes[len(offsets) - 1]
    return sizes_at, sizes, chunks_at, wide, per_chunk, offsets


def nals(data):
    out = []
    while len(data) >= 4:
        n = struct.unpack('>I', data[:4])[0]
        out.append(data[4:4 + n])
        data = data[4 + n:]
    if data:
        out.append(None)  # trailing bytes that are no NAL unit
    return out


def rebuild(buf, samples):
    """`buf` without its `mdat` boxes, the first samples given as NAL lists in a new one."""
    sizes_at, sizes, chunks_at, wide, per_chunk, offsets = layout(buf)
    head = bytearray()
    for kind, at, header, size in walk(buf, 0, len(buf)):
        if kind != b'mdat':
            head += buf[at:at + size]
    base = len(head) + 8
    data = bytearray()
    new_sizes = []
    for i in range(len(sizes)):
        before = len(data)
        for nal in samples[i] if i < len(samples) else [b'\x00']:
            data += nal if nal is not None else b'\x00\x00\x01'
            if nal is not None:
                data[len(data) - len(nal):len(data) - len(nal)] = struct.pack('>I', len(nal))
        new_sizes.append(len(data) - before)
    # Patch the copies of the tables inside `head`, found again there.
    sizes_at, _, chunks_at, wide, per_chunk, _ = layout(bytes(head))
    for i, s in enumerate(new_sizes):
        head[sizes_at + 4 * i:sizes_at + 4 * i + 4] = struct.pack('>I', s)
    index, at = 0, base
    for c, per in enumerate(per_chunk):
        if wide:
            head[chunks_at + 8 * c:chunks_at + 8 * c + 8] = struct.pack('>Q', at)
        else:
            head[chunks_at + 4 * c:chunks_at + 4 * c + 4] = struct.pack('>I', at)
        for _ in range(per):
            if index < len(new_sizes):
                at += new_sizes[index]
                index += 1
    return bytes(head) + struct.pack('>I', len(data) + 8) + b'mdat' + bytes(data)


def track_sets(buf, hevc):
    _, at, end = find(buf, STBL + [b'stsd', b'hvc1' if hevc else b'avc1', b'hvcC' if hevc else b'avcC'])
    c, sets = buf[at:end], []
    if hevc:
        p = 23
        for _ in range(c[22]):
            p += 1
            for _ in range(struct.unpack('>H', c[p:p + 2])[0]):
                n = struct.unpack('>H', c[p + 2:p + 4])[0]
                sets.append(c[p + 4:p + 4 + n])
                p += 4 + n
    else:
        p = 6
        for _ in range(c[5] & 31):
            n = struct.unpack('>H', c[p:p + 2])[0]
            sets.append(c[p + 2:p + 2 + n])
            p += 2 + n
        for _ in range(c[p]):
            n = struct.unpack('>H', c[p + 1:p + 3])[0]
            sets.append(c[p + 3:p + 3 + n])
            p += 2 + n
    return sets


class Seed:
    def __init__(self, path, hevc):
        buf = Path(path).read_bytes()
        _, sizes, _, _, _, offsets = layout(buf)
        self.hevc = hevc
        self.name = Path(path).name
        self.samples = [nals(buf[o:o + s]) for o, s in list(zip(offsets, sizes))[:KEEP]]
        self.sets = track_sets(buf, hevc)
        self.bytes = rebuild(buf, self.samples)


H264 = ['-c:v', 'libx264', '-bf', '0', '-g', '15', '-keyint_min', '15', '-sc_threshold', '0', '-forced-idr', '1']
HEVC = ['-c:v', 'libx265', '-tag:v', 'hvc1', '-x265-params',
        'pools=1:frame-threads=1:bframes=0:keyint=20:min-keyint=20:scenecut=0:open-gop=0:log-level=error']
SYNTHETIC = {
    'h264_ref4.mp4': (False, '526x330', H264 + ['-x264-params', 'ref=4']),
    'h264_ref16.mp4': (False, '526x330', H264 + ['-x264-params', 'ref=16']),
    'h264_slices4.mp4': (False, '526x330', H264 + ['-x264-params', 'slices=4']),
    'h264_1080p.mp4': (False, '1920x1080', H264 + ['-x264-params', 'ref=4']),
    'hevc_slices4.mp4': (True, '526x330', HEVC[:-1] + [HEVC[-1] + ':slices=4']),
}


def synthetic(directory):
    """(hevc, path) of the synthetic seeds, encoded into `directory` when missing."""
    directory.mkdir(parents=True, exist_ok=True)
    out = []
    for name, (hevc, size, codec) in SYNTHETIC.items():
        path = directory / name
        if not path.exists():
            subprocess.run(['ffmpeg', '-v', 'error', '-nostdin', '-y', '-f', 'lavfi', '-i',
                            'testsrc2=size=%s:rate=20' % size, '-t', '7', '-an', '-threads', '1',
                            '-pix_fmt', 'yuv420p', *codec, '-movflags', '+faststart', str(path)], check=True)
        out.append((hevc, path))
    return out


def vcl(nal, hevc):
    return nal is not None and len(nal) > 1 and (((nal[0] >> 1) & 63) < 32 if hevc else 1 <= (nal[0] & 31) <= 5)


def flipped(data, rng, n):
    data = bytearray(data)
    for _ in range(n):
        if data:
            data[rng.randrange(len(data))] ^= 1 << rng.randrange(8)
    return bytes(data)


def mutate(seed, rng):
    samples = [list(s) for s in seed.samples]
    done = []
    pick = lambda: rng.randrange(min(len(samples), 140))
    file_changes = []
    for _ in range(rng.choice([1, 1, 2, 3])):
        what = rng.choice(['flip', 'flip', 'delete', 'duplicate', 'reorder', 'slice', 'set', 'junk', 'header',
                           'forbidden', 'rename', 'truncate', 'swap'])
        i = pick()
        s = samples[i]
        if what == 'flip' and s:
            j = rng.randrange(len(s))
            if s[j] is not None:
                s[j] = flipped(s[j], rng, rng.choice([1, 1, 2, 8]))
        elif what == 'delete' and s:
            del s[rng.randrange(len(s))]
        elif what == 'duplicate' and s:
            nal = s[rng.randrange(len(s))]
            target = samples[rng.choice([i, i, pick()])]
            target.insert(rng.randrange(len(target) + 1), nal)
        elif what == 'reorder' and len(s) > 1:
            a, b = rng.sample(range(len(s)), 2)
            s[a], s[b] = s[b], s[a]
        elif what == 'swap':
            j = pick()
            samples[i], samples[j] = samples[j], samples[i]
        elif what == 'slice':
            slices = [j for j, n in enumerate(s) if vcl(n, seed.hevc)]
            if slices:
                del s[rng.choice(slices)]
        elif what == 'set':
            nal = rng.choice(seed.sets)
            if rng.random() < 0.6:
                nal = flipped(nal, rng, 1) if rng.random() < 0.5 else nal + b'\x00' * rng.choice([1, 2])
            s.insert(rng.randrange(len(s) + 1), nal)
        elif what == 'junk':
            s.insert(rng.randrange(len(s) + 1), rng.choice([b'\x80\x01', b'\x00', b'\x7e\x00', b'\x0c', bytes(rng.randrange(256) for _ in range(rng.choice([1, 2, 5, 40])))]))
        elif what == 'header' and s:
            j = rng.randrange(len(s))
            if s[j]:
                s[j] = bytes([rng.randrange(256)]) + s[j][1:]
        elif what == 'forbidden':
            # SEI, access unit delimiter or filler data, long enough to pass for a whole unit.
            if seed.hevc:
                head = bytes([0x80 | (rng.choice([35, 38, 39, 40]) << 1), 0x01])
            else:
                head = bytes([0x80 | rng.choice([6, 9, 12])])
            nal = head + bytes(rng.randrange(1, 256) for _ in range(rng.choice([1, 3, 12])))
            s.insert(rng.choice([0, rng.randrange(len(s) + 1)]), nal)
        else:
            file_changes.append(what)
        done.append(what)
    data = bytearray(rebuild(seed.bytes, samples))
    for what in file_changes:
        if what == 'rename':
            kinds = []
            def every(start, end, depth):
                for kind, at, header, size in walk(data, start, end):
                    kinds.append(at)
                    if kind in (b'moov', b'trak', b'mdia', b'minf', b'stbl', b'edts') and depth < 6:
                        every(at + header, at + size, depth + 1)
            every(0, len(data), 0)
            at = rng.choice(kinds)
            data[at + 4:at + 8] = rng.choice([b'free', b'ctts', b'stss', b'edts', b'skip'])
        elif what == 'truncate':
            del data[rng.randrange(len(data) // 3, len(data)):]
        elif what == 'flip':
            data = bytearray(flipped(data, rng, rng.choice([1, 4])))
    return bytes(data), done


def run(binary, source, d):
    if d.exists():
        shutil.rmtree(d)
    d.mkdir(parents=True)
    r = subprocess.run(['prlimit', '--as=%d' % LIMIT, 'timeout', '120', binary, '__thumbnail', str(source),
                        str(d / 'thumbnail.jpg')], capture_output=True)
    files = {f.name: hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(d.iterdir())}
    out = r.stdout.decode(errors='replace').strip()
    return {'exit': r.returncode, 'stdout': out, 'files': files}


def main():
    a = argparse.ArgumentParser()
    a.add_argument('work')
    a.add_argument('old')
    a.add_argument('new')
    a.add_argument('--h264', nargs='+', default=[])
    a.add_argument('--hevc', nargs='+', default=[])
    a.add_argument('--synthetic', type=Path, help='directory of the synthetic seeds')
    a.add_argument('--cases', type=int, default=300, help='per codec')
    a.add_argument('--seed', type=int, default=20261117)
    a.add_argument('--keep', action='store_true')
    args = a.parse_args()
    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    seeds = {False: [Seed(p, False) for p in args.h264], True: [Seed(p, True) for p in args.hevc]}
    for hevc, path in synthetic(args.synthetic) if args.synthetic else []:
        seeds[hevc].append(Seed(path, hevc))
    if not seeds[False] or not seeds[True]:
        a.error('no seed for one of the codecs')
    rows, differ = [], 0
    for codec in (False, True):
        for k in range(args.cases):
            rng = random.Random(args.seed * 1000003 + int(codec) * 100003 + k)
            seed = seeds[codec][k % len(seeds[codec])]
            data, changes = mutate(seed, rng)
            case = work / 'case.mp4'
            case.write_bytes(data)
            old, new = run(args.old, case, work / 'old'), run(args.new, case, work / 'new')
            same = old == new
            row = {'codec': 'hevc' if codec else 'h264', 'case': k, 'seed': seed.name, 'changes': changes,
                   'same': same, 'ok': old['exit'] == 0, 'old': old, 'new': new if not same else None}
            rows.append(row)
            if not same:
                differ += 1
                print('DIFFERENT', row, flush=True)
                if args.keep:
                    shutil.copy(case, work / ('differs-%s-%d.mp4' % (row['codec'], k)))
            elif k % 50 == 0:
                print(row['codec'], k, 'ok' if row['ok'] else 'fails', changes, flush=True)
    summary = {'cases': len(rows), 'same': len(rows) - differ, 'different': differ,
               'h264': sum(r['codec'] == 'h264' for r in rows), 'hevc': sum(r['codec'] == 'hevc' for r in rows),
               'thumbnail_made': sum(r['ok'] for r in rows), 'seed': args.seed,
               'seeds': {('hevc' if c else 'h264'): [s.name for s in v] for c, v in seeds.items()}}
    print(json.dumps(summary))
    for p in ('case.mp4', 'old', 'new'):
        q = work / p
        if q.is_dir():
            shutil.rmtree(q)
        elif q.exists():
            q.unlink()
    (work / 'thumbnail-differential.json').write_text(json.dumps({'summary': summary, 'rows': rows}, indent=1))


if __name__ == '__main__':
    main()
