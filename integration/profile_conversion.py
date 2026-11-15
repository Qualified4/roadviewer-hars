"""One Linux real-input conversion profile, including native descendants.

RSS is a sampled process-tree sum (shared pages may be counted repeatedly).
Per-stage CPU is a sampled lower bound: very short-lived children can escape a
sample. Total CPU uses wait4/getrusage for the reaped child and its descendants.
Stage names/granularity differ between implementations, so compare whole jobs.
"""
import argparse
from collections import defaultdict
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import sys
import threading
import time


def processes(pid):
    found=set();pending=[pid]
    while pending:
        pid=pending.pop()
        if pid in found:continue
        found.add(pid)
        try:
            for thread in (Path('/proc')/str(pid)/'task').iterdir():
                try:pending.extend(map(int,(thread/'children').read_text().split()))
                except OSError:pass
        except OSError:pass
    return found


def measure(command,env,output):
    before=resource.getrusage(resource.RUSAGE_CHILDREN);started=time.monotonic()
    process=subprocess.Popen(command,env=env,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True)
    stop=threading.Event();samples=[];stage='startup';stage_cpu=defaultdict(float);seen={}
    def monitor():
        while not stop.is_set():
            rss=0;delta=0.
            for pid in processes(process.pid):
                try:
                    text=(Path('/proc')/str(pid)/'stat').read_text().rsplit(')',1)[1].split()
                    identity=(pid,text[19]);cpu=(int(text[11])+int(text[12]))/os.sysconf('SC_CLK_TCK')
                    delta+=max(0.,cpu-seen.get(identity,0.));seen[identity]=cpu
                    status=(Path('/proc')/str(pid)/'status').read_text().splitlines()
                    rss+=int(next(s for s in status if s.startswith('VmRSS:')).split()[1])
                except (OSError,StopIteration,ValueError):pass
            samples.append((time.monotonic()-started,rss));stage_cpu[stage]+=delta;stop.wait(.02)
    thread=threading.Thread(target=monitor);thread.start()
    with output.open('w') as log:
        for line in process.stdout:
            log.write(line)
            try:event=json.loads(line)
            except ValueError:continue
            if isinstance(event,dict) and isinstance(event.get('stage'),str):stage=event['stage']
    assert process.wait(timeout=600)==0,'profile conversion failed'
    stop.set();thread.join();elapsed=time.monotonic()-started
    after=resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu=after.ru_utime+after.ru_stime-before.ru_utime-before.ru_stime
    integral=sum((b[0]-a[0])*a[1] for a,b in zip(samples,samples[1:]))
    return {'elapsed_seconds':elapsed,'total_cpu_seconds':cpu,'cpu_percent_one_core':100*cpu/elapsed,
            'rss_samples':len(samples),'rss_interval_seconds':.02,'mean_rss_tree_kib':integral/elapsed,
            'peak_rss_tree_kib':max(v for _,v in samples),'sampled_stage_cpu_seconds_lower_bound':dict(stage_cpu)}


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path);parser.add_argument('binary',type=Path);parser.add_argument('output',type=Path)
    parser.add_argument('--baseline',type=Path)
    args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=False)
    root=Path(__file__).resolve().parents[1];legacy=args.output/'reference';shutil.copytree(root/'reference/python/roadviewer',legacy)
    env=dict(os.environ,RV_PREPARED_DIR=str(args.output/'python'),RV_PROGRESS_FD='1',RV_MEDIA_PATHS=json.dumps({kind:str(args.source.with_name(args.source.name.replace('rlog.zst',name))) for kind,name in (('camera.mp4','qcamera.mp4'),('fcamera.mp4','fcamera.mp4'),('ecamera.mp4','ecamera.mp4'))}))
    results={'real_input':True,'runs_per_implementation':1,'python':json.loads(args.baseline.read_text())['python'] if args.baseline else measure([sys.executable,str(legacy/'app/decoder.py'),str(args.source),'profile'],env,args.output/'python.log'),'rust':measure([str(args.binary),'convert-log',str(args.source),str(args.output/'rust'),'profile'],dict(os.environ),args.output/'rust.log')}
    if args.baseline:results['python_baseline_reused']=True
    (args.output/'profile.json').write_text(json.dumps(results,indent=2));print(json.dumps(results))


if __name__=='__main__':main()
