"""Upload a real segment into an owned loopback HARS, then wait for conversion.
No operating Home Assistant/Openpilot settings or original source is modified.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import urllib.request


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1",0));return s.getsockname()[1]


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("corpus",type=Path);p.add_argument("binary",type=Path);p.add_argument("output",type=Path)
    p.add_argument("--segment",type=int,default=0)
    args=p.parse_args();args.output.mkdir(parents=True,exist_ok=False)
    ui,device=port(),port();url=f"http://127.0.0.1:{ui}"
    env=dict(os.environ,HARS_DATA=str(args.output/"data"),HARS_INGRESS_ONLY="0",HARS_BIND=f"127.0.0.1:{ui}",HARS_DEVICE_BIND=f"127.0.0.1:{device}")
    def request(path,method="GET",body=None,raw=False):
        if isinstance(body,dict):body=json.dumps(body).encode()
        req=urllib.request.Request(url+path,data=body,method=method,headers={"X-RoadViewer-Request":"1","Content-Type":"application/octet-stream" if raw else "application/json"})
        with urllib.request.urlopen(req,timeout=30) as r:
            b=r.read();return b if raw else json.loads(b)
    with (args.output/"server.log").open("wb") as log:
        server=subprocess.Popen([str(args.binary)],env=env,stdout=log,stderr=log)
        try:
            end=time.monotonic()+10
            while True:
                try:request("/health");break
                except OSError:
                    if server.poll() is not None or time.monotonic()>end:raise
                    time.sleep(.05)
            request("/api/settings/processing","POST",{"auto_convert":True,"concurrency":1})
            files=sorted(args.corpus.glob(f"*--{args.segment}--*"))
            assert 2<=len(files)<=4
            def digest(path):
                h=hashlib.sha256()
                with path.open("rb") as f:
                    while chunk:=f.read(1048576):h.update(chunk)
                return h.hexdigest()
            hashes={f.name:digest(f) for f in files}
            manifest={"files":[{"name":f.name,"size":f.stat().st_size,"sha256":hashes[f.name]} for f in files]}
            session=request("/api/uploads","POST",manifest)["id"]
            for i,f in enumerate(files):
                with f.open("rb") as source:
                    offset=0
                    while chunk:=source.read(2*1024*1024):
                        request(f"/api/uploads/{session}/files/{i}?offset={offset}","PUT",chunk,True);offset+=len(chunk)
            # An active, independent upload must hold back automatic conversion.
            held=request("/api/uploads","POST",{"files":[{"name":"hold/rlog.zst","size":1024,"sha256":None}]})["id"]
            request(f"/api/uploads/{held}/files/0?offset=0","PUT",b"hold",True)
            result=request(f"/api/uploads/{session}/finish","POST",{})
            rows=request("/api/logs")["logs"];assert len(rows)==1
            record=rows[0]["id"]
            time.sleep(2)
            assert request("/api/logs")["logs"][0]["status"]=="queued"
            assert request("/api/progress")["progress"][record]["stage"]=="waiting_upload"
            request(f"/api/uploads/{held}","DELETE")
            end=time.monotonic()+120;latencies=[];states=[]
            while True:
                started=time.monotonic();row=request("/api/logs")["logs"][0];latencies.append(1000*(time.monotonic()-started));states.append(row["status"])
                if row["status"]=="ready":break
                assert row["status"]!="error",row.get("error")
                assert time.monotonic()<end,"automatic conversion deadline"
                time.sleep(.2)
            data=request(f"/api/logs/{record}/data")
            telemetry=request(f"/api/logs/{record}/telemetry")
            jpg=request(f"/api/logs/{record}/thumbnail",raw=True)
            expected_cameras={camera for camera,kind in (("front","fcamera.mp4"),("qcamera","qcamera.mp4"),("wide","ecamera.mp4")) if any(f.name.endswith("--"+kind) for f in files)}
            assert data["replayV"]==2 and len(data["frames"])>0 and set(data["videos"])==expected_cameras
            assert telemetry["streams"]["carState"]["times"] and jpg.startswith(b"\xff\xd8")
            assert all(digest(f)==hashes[f.name] for f in files)
            report={"record_id":record,"files":len(files),"model_frames":len(data["frames"]),"cameras":list(data["videos"]),"upload_wait_policy":True,"states":list(dict.fromkeys(states)),"api_samples":len(latencies),"api_max_ms":max(latencies),"source_hashes_unchanged":True,"browser_playback_validated":False}
            (args.output/"result.json").write_text(json.dumps(report,indent=2));print(json.dumps(report))
        finally:
            server.terminate()
            try:server.wait(timeout=5)
            except subprocess.TimeoutExpired:server.kill();server.wait()


if __name__=="__main__":main()
