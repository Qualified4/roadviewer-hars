#!/usr/bin/env python3
"""Damaged camera inputs through Python Road Viewer (PyAV) and both HARS engines.

Usage: boundary_inputs.py HARS_BINARY WORK_DIR [OUT_JSON]

Each case is a synthetic input made from a 10-frame fixture. For every case the
script records what Python's own conversion code does (`hevc_video.prepare_video`
for raw HEVC; the `decoder.py` qcamera steps for TS), what the HARS `remux` CLI
does with the pure engine and with `HARS_REMUX_ENGINE=ffmpeg`, and, for each
published file, whether every input picture is in it byte for byte (compared
against an independent split of the input file, or for TS against the intact
fixture) and how many frames FFmpeg decodes from it.
"""
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'reference/python/roadviewer/app'))
import av  # noqa: E402
from hevc_video import prepare_video  # noqa: E402
from video_packets import Pictures, inspect_mp4  # noqa: E402

START = re.compile(b'\x00\x00\x00?\x01')


class Quiet:
    def update(self, *args, **kwargs):
        pass


def fixture(path, hevc):
    codec = ['-c:v', 'libx265', '-x265-params', 'pools=1:frame-threads=1:bframes=0:keyint=10:log-level=error', '-f', 'hevc'] if hevc \
        else ['-c:v', 'libx264', '-bf', '0', '-g', '10', '-f', 'mpegts']
    subprocess.run(['ffmpeg', '-v', 'error', '-nostdin', '-y', '-f', 'lavfi', '-i', 'testsrc2=size=160x96:rate=10', '-t', '1',
                    '-an', '-threads', '1', *codec[:2], '-pix_fmt', 'yuv420p', *codec[2:], str(path)], check=True)


def units(data):
    return [u.rstrip(b'\0') for u in START.split(data)[1:]]


def vcl(unit_list, hevc):
    return [u for u in unit_list if u and ((u[0] >> 1) & 63 < 32 if hevc else 1 <= (u[0] & 31) <= 5)]


def mp4_samples(path):
    """Per-sample list of NAL units of an MP4 (4-byte lengths)."""
    out = []
    with av.open(str(path)) as inp:
        stream = inp.streams.video[0]
        for packet in inp.demux(stream):
            if not packet.size:
                continue
            data, i, nal = bytes(packet), 0, []
            while i + 4 <= len(data):
                n = int.from_bytes(data[i:i + 4], 'big')
                nal.append(data[i + 4:i + 4 + n])
                i += 4 + n
            out.append(nal)
    return out


def decoded(path):
    run = subprocess.run(['ffmpeg', '-v', 'error', '-nostdin', '-i', str(path), '-map', '0:v:0', '-f', 'framemd5', '-'],
                         capture_output=True, text=True)
    frames = sum(1 for line in run.stdout.splitlines() if line and not line.startswith('#'))
    return frames, len([line for line in run.stderr.splitlines() if line.strip()])


def check(output, expected_vcl, hevc):
    samples = mp4_samples(output)
    got = [u.rstrip(b'\0') for s in samples for u in vcl(s, hevc)]
    frames, errors = decoded(output)
    return {'samples': len(samples), 'pictures_equal_input': got == expected_vcl,
            'vcl_units': len(got), 'expected_vcl_units': len(expected_vcl), 'decoded_frames': frames, 'decode_error_lines': errors}


def python_hevc(source, output, frames):
    rows = [{'segmentId': i, 'timestampEof': 10**9 + i * 10**8} for i in range(frames)]
    try:
        info = prepare_video(source, output, rows, 0, Quiet())
        return {'result': 'published', 'frames': info['frames']}
    except Exception as e:  # noqa: BLE001 - the outcome is the record
        return {'result': 'failed', 'error': str(e)}


def python_ts(source, output):
    """decoder.py's qcamera steps (source scan, remux, inspect_mp4) without the log analysis."""
    try:
        pts = []
        with av.open(str(source)) as c:
            stream = c.streams.video[0]
            pictures = Pictures(stream, annex_b=True)
            for packet in c.demux(stream):
                if packet.pts is None or packet.size == 0:
                    continue
                pts.append(float(packet.pts * stream.time_base))
                pictures.add(packet)
        signature = pictures.finish()
        pts.sort()
        with av.open(str(source)) as inp, av.open(str(output), 'w', options={'movflags': '+faststart'}) as out:
            stream = inp.streams.video[0]
            target = out.add_stream_from_template(stream, opaque=True)
            offset = round(pts[0] / float(stream.time_base))
            for packet in inp.demux(stream):
                if packet.dts is None:
                    continue
                packet.pts -= offset
                packet.dts -= offset
                packet.stream = target
                out.mux(packet)
        info = inspect_mp4(output, [p - pts[0] for p in pts], Quiet(), signature)
        return {'result': 'published', 'frames': info['frames']}
    except Exception as e:  # noqa: BLE001
        output.unlink(missing_ok=True)
        return {'result': 'failed', 'error': str(e)}


def hars(binary, source, kind, times, output, engine):
    clock = output.with_suffix('.times.json')
    clock.write_text(json.dumps(times))
    env = dict(os.environ, HARS_REMUX_ENGINE=engine)
    run = subprocess.run([binary, 'remux', str(source), kind, str(clock), str(output)], capture_output=True, text=True, env=env)
    events = []
    for line in run.stderr.splitlines():
        try:
            row = json.loads(line)
        except ValueError:
            continue
        fields = row.get('fields', row)
        if str(fields.get('event', fields.get('message', ''))).startswith('remux_'):
            events.append({k: fields.get(k) for k in ('event', 'engine', 'failure_class', 'reason_code', 'fallback', 'fallback_result', 'bytes')
                           if fields.get(k) is not None})
    clock.unlink()
    return {'result': 'published' if run.returncode == 0 and output.exists() else 'failed', 'events': events,
            'error': None if run.returncode == 0 else run.stderr.strip().splitlines()[-1][:200] if run.stderr.strip() else 'exit'}


def main():
    binary, work = sys.argv[1], Path(sys.argv[2])
    work.mkdir(parents=True, exist_ok=True)
    hevc_path, ts_path = work / 'fixture.hevc', work / 'fixture.ts'
    fixture(hevc_path, True)
    fixture(ts_path, False)
    raw_hevc, raw_ts = hevc_path.read_bytes(), ts_path.read_bytes()
    hevc_units = START.split(raw_hevc)[1:]
    first_slice = next(i for i, u in enumerate(hevc_units) if (u[0] >> 1) & 63 < 32)
    sei = next((i for i, u in enumerate(hevc_units) if (u[0] >> 1) & 63 in (39, 40)), None)

    def rebuilt(edit):
        return b''.join(b'\0\0\0\1' + u for u in (edit(i, bytearray(u)) for i, u in enumerate(hevc_units)) if u is not None)

    def flag(index):
        def edit(i, u):
            if i == index:
                u[0] |= 0x80
            return bytes(u)
        return edit

    def zeros(i, u):
        return bytes(u).rstrip(b'\0') + (b'\0' * 3 if (u[0] >> 1) & 63 < 32 else b'')

    cases = [
        ('hevc-intact', raw_hevc, 'hevc'),
        ('hevc-forbidden-bit-slice', rebuilt(flag(first_slice + 2)), 'hevc'),
        ('hevc-trailing-zeros', rebuilt(zeros), 'hevc'),
        ('hevc-leading-junk-4', b'JUNK' + raw_hevc, 'hevc'),
        ('hevc-leading-junk-1k', bytes((i * 7 + 3) % 255 + 1 for i in range(1024)) + raw_hevc, 'hevc'),
        ('hevc-leading-partial-nal', raw_hevc[len(raw_hevc) // 2:], 'hevc'),
        ('ts-intact', raw_ts, 'ts'),
        ('ts-cut-100', raw_ts[:-100], 'ts'),
        ('ts-cut-187', raw_ts[:-187], 'ts'),
        ('ts-cut-1', raw_ts[:-1], 'ts'),
        ('ts-cut-188', raw_ts[:-188], 'ts'),
        ('ts-partial-pat-after-end', raw_ts + raw_ts[:100], 'ts'),
    ]
    ts_flagged = bytearray(raw_ts)
    for at in range(0, len(raw_ts), 188):
        packet = raw_ts[at:at + 188]
        found = next((m.start() for m in re.finditer(b'\x00\x00\x01[\x01\x21\x41\x61]', packet[4:])), None)
        if ((packet[1] & 0x1f) << 8 | packet[2]) == 0x100 and found is not None:
            flagged_at = at + 4 + found + 3
            ts_flagged[flagged_at] |= 0x80
            break
    cases.append(('ts-forbidden-bit-slice', bytes(ts_flagged), 'ts'))
    sps = next(i for i, u in enumerate(hevc_units) if (u[0] >> 1) & 63 == 33)
    cases.insert(2, ('hevc-forbidden-bit-sps', rebuilt(flag(sps)), 'hevc'))
    if sei is not None:
        cases.insert(2, ('hevc-forbidden-bit-sei', rebuilt(flag(sei)), 'hevc'))
    # The last whole packet carrying video payload, for the TS cut cases.
    packets = [raw_ts[i:i + 188] for i in range(0, len(raw_ts), 188)]
    video_pid = 0x100
    last_video = max(i for i, p in enumerate(packets) if ((p[1] & 0x1f) << 8 | p[2]) == video_pid)
    ts_intact_samples = None
    report = {'ts_packets': len(packets), 'ts_last_video_packet': last_video, 'cases': []}
    for name, data, family in cases:
        hevc = family == 'hevc'
        case_dir = work / name
        case_dir.mkdir(exist_ok=True)
        source = case_dir / ('fcamera.hevc' if hevc else 'qcamera.ts')
        source.write_bytes(data)
        row = {'case': name, 'bytes': len(data)}
        if hevc:
            expected = vcl(units(data), True)
            frames = len([u for u in expected if len(u) > 2 and u[2] & 0x80])  # first_slice_segment_in_pic_flag
            times = [i / 10 for i in range(frames)]
            row['input_pictures'] = frames
            py_out = case_dir / 'python.mp4'
            row['python'] = python_hevc(source, py_out, frames)
            kind = 'fcamera.hevc'
        else:
            with av.open(str(ts_path)) as c:
                s = c.streams.video[0]
                pts = sorted(p.pts for p in c.demux(s) if p.pts is not None and p.size)
            times = [(p - pts[0]) / 90000 for p in pts]
            py_out = case_dir / 'python.mp4'
            row['python'] = python_ts(source, py_out)
            kind = 'qcamera.ts'
            if ts_intact_samples is None:
                ts_intact_samples = [[u.rstrip(b'\0') for u in vcl(s, False)] for s in mp4_samples(py_out)] if py_out.exists() else []
            expected = [u for s in ts_intact_samples for u in s]
            if name == 'ts-forbidden-bit-slice':
                head = raw_ts[flagged_at:flagged_at + 8]
                hit = next(i for i, u in enumerate(expected) if u[:8] == head)
                expected[hit] = bytes([expected[hit][0] | 0x80]) + expected[hit][1:]
            if len(data) % 188:
                cut = data[len(data) // 188 * 188:]
                row['partial_packet'] = {'bytes': len(cut), 'pid': ((cut[1] & 0x1f) << 8 | cut[2]) if len(cut) >= 3 else None,
                                         'index': len(data) // 188, 'is_last_video_packet': len(data) // 188 == last_video}
        if py_out.exists():
            row['python'].update(check(py_out, expected, hevc))
        for engine in ('pure', 'ffmpeg'):
            out = case_dir / f'hars-{engine}.mp4'
            row[f'hars_{engine}'] = hars(binary, source, kind, times, out, engine)
            if out.exists():
                row[f'hars_{engine}'].update(check(out, expected, hevc))
        digests = {name: [hashlib.sha256(b''.join(s)).hexdigest() for s in mp4_samples(path)]
                   for name, path in (('python', py_out), ('pure', case_dir / 'hars-pure.mp4'), ('ffmpeg', case_dir / 'hars-ffmpeg.mp4'))
                   if path.exists()}
        if 'pure' in digests:
            row['pure_samples_equal_python'] = digests['pure'] == digests.get('python')
            row['ffmpeg_samples_equal_pure'] = digests['pure'] == digests.get('ffmpeg')
        report['cases'].append(row)
        print(json.dumps(row, ensure_ascii=False))
    if len(sys.argv) > 3:
        Path(sys.argv[3]).write_text(json.dumps(report, ensure_ascii=False, indent=1))


if __name__ == '__main__':
    main()
