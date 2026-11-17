"""One controlled API-only comparison. Never touches production data/ports."""
import argparse
import asyncio
import json
import math
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid

import aiohttp

ROOT=Path(__file__).resolve().parents[1]
def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1",0))
        return s.getsockname()[1]
def process_tree(pid):
    result=[pid]
    try:
        for child in (Path("/proc")/str(pid)/"task"/str(pid)/"children").read_text().split():
            result.extend(process_tree(int(child)))
    except OSError:
        pass
    return result
def metrics(pid):
    rss,ticks=0,0
    for p in process_tree(pid):
        try:
            status=(Path("/proc")/str(p)/"status").read_text()
            rss+=int(next(line for line in status.splitlines() if line.startswith("VmRSS:")).split()[1])
            stat=(Path("/proc")/str(p)/"stat").read_text().rsplit(")",1)[1].split()
            ticks+=int(stat[11])+int(stat[12])
        except (OSError,StopIteration):
            pass
    return rss,ticks/os.sysconf("SC_CLK_TCK")
def percentiles(values):
    values=sorted(values)
    return {"samples":len(values),**{f"p{p}_ms":round(values[min(len(values)-1,math.ceil(p/100*len(values))-1)]*1000,3) for p in (50,95,99)}}
def seed(root,count):
    root.mkdir()
    (root/".processing-settings.json").write_text(json.dumps({"concurrency":1,"auto_convert":False,"keep_original_video":True}))
    for i in range(count):
        name=f"{i+1:032x}";folder=root/name;folder.mkdir()
        (folder/"rlog.zst").write_bytes(b"synthetic")
        meta={"id":name,"name":f"2026-11-15--12-00-00 / 구간 {i}","uploaded":1700000000+i,"status":"unconverted","video":False,"bytes":9,"files":{"rlog.zst":"rlog.zst"},"error":None}
        (folder/"meta.json").write_text(json.dumps(meta))
        for path in folder.iterdir():
            os.utime(path,(1700000000,1700000000))
        os.utime(folder,(1700000000,1700000000))
async def measure(kind,rust,base,legacy,count):
    data=base/(kind+"-data");seed(data,count)
    url="http://127.0.0.1:"+str(port())
    if kind=="python":
        env={**os.environ,"RV_DATA":str(data),"RV_INGRESS_ONLY":"0","RV_DEVICE_HOST_PORT":"0","RV_ARCHIVE_OPTIONS_JSON":json.dumps({"archive_enabled":False,"archive_path":"","archive_gdrive_fill_tracks":False})}
        command=[sys.executable,"-m","gunicorn","--chdir",str(legacy/"app"),"--bind",url.removeprefix("http://"),"--workers","1","--threads","8","server:app"]
    else:
        env={**os.environ,"HARS_DATA":str(data),"HARS_WEB":str(ROOT/"web"),"HARS_INGRESS_ONLY":"0","HARS_BIND":url.removeprefix("http://"),"HARS_DEVICE_BIND":"127.0.0.1:"+str(port())}
        command=[rust]
    process=subprocess.Popen(command,env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    peak=0;stopped=False
    async def monitor():
        nonlocal peak
        while not stopped:
            peak=max(peak,metrics(process.pid)[0]);await asyncio.sleep(.02)
    try:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=20)) as http:
            response_bytes=0
            async def get():
                nonlocal response_bytes
                start=time.perf_counter()
                async with http.get(url+"/api/logs") as r:
                    assert r.status==200,"API failed"
                    response_bytes=len(await r.read())
                return time.perf_counter()-start
            for _ in range(120):
                if process.poll() is not None:
                    raise RuntimeError("benchmark server exited")
                try:
                    await get();break
                except (aiohttp.ClientError,OSError):
                    await asyncio.sleep(.1)
            else:
                raise RuntimeError("benchmark readiness timeout")
            for _ in range(10):
                await get()
            idle,cpu_before=metrics(process.pid)
            peak=idle;timer=time.perf_counter();monitor_task=asyncio.create_task(monitor())
            ordinary=[await get() for _ in range(100)]
            latencies=[];errors=0;requests=0;pending=4
            payload=b"x"*(256*1024)
            async def upload(index):
                nonlocal errors,requests,pending
                headers={"X-RoadViewer-Request":"1"}
                try:
                    async with http.post(url+"/api/uploads",json={"files":[{"name":f"2026-11-15--12-00-00--{index}--rlog.zst","size":8*1024*1024}]},headers=headers) as r:
                        assert r.status==201
                        session=await r.json()
                    for offset in range(0,8*1024*1024,len(payload)):
                        start=time.perf_counter();requests+=1
                        try:
                            async with http.put(url+f"/api/uploads/{session['id']}/files/0?offset={offset}",data=payload,headers=headers) as r:
                                await r.read()
                                if r.status!=200:
                                    errors+=1
                        except (aiohttp.ClientError,TimeoutError):
                            errors+=1
                        latencies.append(time.perf_counter()-start)
                finally:
                    pending-=1
            loaded=[]
            async def poll():
                while len(loaded)<200 or (pending and len(loaded)<1000):
                    loaded.append(await get())
            upload_start=time.perf_counter()
            await asyncio.wait_for(asyncio.gather(*(upload(i) for i in range(4)),poll()),30)
            load_elapsed=time.perf_counter()-upload_start
            cpu=metrics(process.pid)[1]-cpu_before;elapsed=time.perf_counter()-timer
            stopped=True;await monitor_task
            return {"last_api_response_bytes":response_bytes,"idle_rss_tree_kib":idle,"observed_peak_rss_tree_kib":peak,"server_cpu_seconds":round(cpu,4),"measurement_wall_seconds":round(elapsed,4),"cpu_percent_one_core":round(cpu/elapsed*100,2),"idle_api":percentiles(ordinary),"api_during_upload":percentiles(loaded),"chunk_response":percentiles(latencies),"chunk_requests":requests,"chunk_failures":errors,"chunk_failure_rate":errors/requests,"upload_phase_wall_seconds":round(load_elapsed,4)}
    finally:
        stopped=True
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill();process.wait(timeout=5)
async def main(args):
    with tempfile.TemporaryDirectory(prefix="hars-api-bench-") as temp:
        base=Path(temp);legacy=base/"legacy";shutil.copytree(ROOT/"reference/python/roadviewer",legacy)
        results={}
        for kind in ("python","rust"):
            results[kind]=await measure(kind,args.rust,base,legacy,args.records)
    report={"scope":"synthetic API-only development comparison; not full backend parity or production performance","functional_note":"Rust list omits some derived metadata and archive work; response sizes and feature scope differ, so these numbers are not a full-parity speedup claim","records":args.records,"repetitions":1,"parallel_uploads":4,"bytes_per_upload":8*1024*1024,"chunk_bytes":256*1024,"rss_note":"sum of process-tree RSS, including Gunicorn master and worker; shared pages may be counted twice; peak sampled every 20 ms","durability_note":"Rust syncs each acknowledged chunk; Python writes without per-chunk fsync","transport":"loopback HTTP; no TLS, Ingress, NAS, Google Drive, decoding, or browser rendering workload","runtime":{"platform":platform.platform(),"python":platform.python_version(),"rust_build":"release","cpu_count":os.cpu_count(),"python_workers":1,"python_threads":8,"rust_tokio_workers":"default CPU count"},"results":results}
    path=Path(args.output);path.parent.mkdir(parents=True,exist_ok=True);path.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n")
    print(json.dumps(report,ensure_ascii=False))
if __name__=="__main__":
    p=argparse.ArgumentParser();p.add_argument("--rust",required=True);p.add_argument("--records",type=int,default=200);p.add_argument("--output",default="artifacts/api-benchmark.json")
    asyncio.run(main(p.parse_args()))
