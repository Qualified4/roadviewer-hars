"""Warm-cache, interleaved full conversions; diagnostics/profilers stay disabled.

Reuse immutable binary artifacts, not a new baseline PR or CI run. Every measured
result is checked against Python at the established tolerance outside the timed
interval. Input is read only; outputs are private and must not be committed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import sys
from profile_conversion import measure
from real_conversion_compare import load
from real_replay_compare import differences
import terminal_rule


def digest(path):
    h=hashlib.sha256()
    with path.open('rb') as source:
        while chunk:=source.read(1048576):h.update(chunk)
    return h.hexdigest()


def compare(a,b,known=None,drive_summary=None):
    """`known` (terminal_rule.model) applies only when `a` is the Python output."""
    files={}
    for name in ('data.json.gz','telemetry.json.gz','track.json.gz','summary.json'):
        if not (a/name).exists() and not (b/name).exists():continue
        x,y=load(a/name),load(b/name)
        if name=='data.json.gz':x.pop('key');y.pop('key')
        files[name]=(x,y)
    if known:
        _,mismatch=terminal_rule.reconcile(files,known,drive_summary)
        assert not mismatch,mismatch
    for name,(x,y) in files.items():
        error=differences(x,y)
        assert not error,{'file':name,'differences':dict(error)}


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('source',type=Path);p.add_argument('output',type=Path)
    p.add_argument('--binary',action='append',required=True,help='NAME=/absolute/immutable/binary[@pure|@ffmpeg] (HARS_REMUX_ENGINE)')
    p.add_argument('--runs',type=int,default=5)
    args=p.parse_args();args.output.mkdir(parents=True,exist_ok=False)
    assert args.runs>=3
    root=Path(__file__).resolve().parents[1];reference=args.output/'reference';shutil.copytree(root/'reference/python/roadviewer',reference)
    binaries=dict(x.split('=',1) for x in args.binary);assert 'baseline' in binaries
    engines={name:path.rsplit('@',1)[1] for name,path in binaries.items() if '@' in path}
    binaries={name:path.rsplit('@',1)[0] for name,path in binaries.items()}
    env={k:v for k,v in os.environ.items() if not k.startswith('HARS_PROFILE') and k!='HARS_REMUX_ENGINE'}
    media={kind:args.source.with_name(args.source.name.replace('rlog.zst',name)) for kind,name in (
        ('camera.mp4','qcamera.mp4'),('fcamera.mp4','fcamera.mp4'),('ecamera.mp4','ecamera.mp4'),
        ('qcamera.ts','qcamera.ts'),('fcamera.hevc','fcamera.hevc'),('ecamera.hevc','ecamera.hevc'))}
    pyenv=dict(env,RV_PREPARED_DIR='',RV_MEDIA_PATHS=json.dumps({k:str(v) for k,v in media.items()}),RV_PROGRESS_FD='1')
    inputs=[args.source,*media.values()]
    sys.path.insert(0,str(reference/'app'))
    from telemetry import drive_summary
    known=terminal_rule.model(args.source,{k:v for k,v in media.items() if not k.endswith('.mp4')},reference/'schema/cereal/log.capnp')
    hashes={path.name:digest(path) for path in inputs if path.is_file()}
    results={'conditions':{'kernel':platform.release(),'python':platform.python_version(),'logical_cpus':os.cpu_count(),'warmups_per_variant':1,'runs':args.runs,'cache':'warm, no global drop_caches','input_filesystem':'caller path; same input for all variants','output_filesystem':str(args.output.parent),'compression_level':9,'diagnostics':False},'binary_sha256':{name:digest(Path(path)) for name,path in binaries.items()},'samples':[]}
    variants=['python',*binaries]
    golden=None
    def run(name,label,warmup=False):
        nonlocal golden
        dest=args.output/label
        if name=='python':
            command=[sys.executable,str(reference/'app/decoder.py'),str(args.source),'benchmark'];runenv=dict(pyenv,RV_PREPARED_DIR=str(dest))
        else:command=[binaries[name],'convert-log',str(args.source),str(dest),'benchmark'];runenv=dict(env,HARS_REMUX_ENGINE=engines[name]) if name in engines else env
        data=measure(command,runenv,args.output/(label+'.log'))
        if golden is None:assert name=='python';golden=dest
        compare(golden,dest,known if name!='python' else None,drive_summary)
        if name in engines:
            log=(args.output/(label+'.log')).read_text(errors='replace')
            assert f'"engine":"{engines[name]}"' in log and 'remux_fallback' not in log,f'{name} did not use {engines[name]}' 
        replay=load(dest/'data.json.gz')
        expected={camera for camera,kinds in (('qcamera',('qcamera.ts','camera.mp4')),('front',('fcamera.hevc','fcamera.mp4')),('wide',('ecamera.hevc','ecamera.mp4'))) if any(media[k].is_file() for k in kinds)}
        assert set(replay['videos'])==expected and not replay['warnings'],'benchmark input must produce every camera without fallback'
        data.update({'variant':name,'round':label,'output_bytes':sum(x.stat().st_size for x in dest.iterdir() if x.is_file()),'files':{x.name:x.stat().st_size for x in dest.iterdir() if x.is_file()}})
        if name!='python':
            data['thumbnail_metadata']=load(dest/'thumbnail.json')
        if not warmup:results['samples'].append(data)
        (args.output/'benchmark.json').write_text(json.dumps(results,indent=2))
        print(json.dumps({'variant':name,'round':label,'seconds':data['elapsed_seconds'],'peak_kib':data['peak_rss_tree_kib'],'equivalent':True}),flush=True)
    for name in variants:run(name,'warmup-'+name,True)
    for i in range(args.runs):
        # Rotate positions, reversing on alternate rounds to avoid fixed first/last.
        order=variants[i%len(variants):]+variants[:i%len(variants)]
        if i%2:order=list(reversed(order))
        for name in order:run(name,f'{i+1}-{name}')
    assert all(digest(path)==hashes[path.name] for path in inputs if path.is_file())
    for name in binaries:
        for sample in (x for x in results['samples'] if x['variant']==name):
            a=args.output/sample['round'];b=args.output/sample['round'].replace(name,'baseline')
            assert load(a/'thumbnail.json')==load(b/'thumbnail.json')
            assert digest(a/'thumbnail.jpg')==digest(b/'thumbnail.jpg')
    results['summary']={name:{metric:statistics.median(x[metric] for x in results['samples'] if x['variant']==name) for metric in ('elapsed_seconds','total_cpu_seconds','mean_rss_tree_kib','peak_rss_tree_kib','output_bytes')} for name in variants}
    results['input_hashes_unchanged']=True
    (args.output/'benchmark.json').write_text(json.dumps(results,indent=2));print(json.dumps(results['summary']))


if __name__=='__main__':main()
