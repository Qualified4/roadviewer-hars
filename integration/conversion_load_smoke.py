"""Bounded loopback-only upload/API probe during an explicit reconversion.

Use an owned, idle test server with a ready real recording. This removes only
that recording's prepared output through its public API, then recreates it.
Never point this tool at Home Assistant or an operating upload listener.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import math
from pathlib import Path
import time
import urllib.request
from urllib.parse import urlparse


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('url');p.add_argument('record');p.add_argument('output',type=Path)
    args=p.parse_args()
    assert urlparse(args.url).hostname in ('127.0.0.1','localhost')
    def request(path,method='GET',body=None):
        binary=isinstance(body,bytes)
        if body is not None and not binary:body=json.dumps(body).encode()
        req=urllib.request.Request(args.url+path,data=body,method=method,headers={'X-RoadViewer-Request':'1','Content-Type':'application/octet-stream' if binary else 'application/json'})
        with urllib.request.urlopen(req,timeout=30) as response:return json.load(response)
    request('/api/settings/processing','POST',{'auto_convert':False,'concurrency':1})
    assert next(r for r in request('/api/logs')['logs'] if r['id']==args.record)['status']=='ready'
    request(f'/api/logs/{args.record}/prepared','DELETE')
    session=request('/api/uploads','POST',{'files':[{'name':'load/rlog.zst','size':16*1024*1024,'sha256':None}]})['id']
    chunks=[];latencies=[];overlap=0
    def upload():
        for i in range(64):
            started=time.monotonic()
            request(f'/api/uploads/{session}/files/0?offset={i*262144}','PUT',b'x'*262144)
            chunks.append(1000*(time.monotonic()-started));time.sleep(.02)
    try:
        request(f'/api/logs/{args.record}/convert','POST',{})
        with ThreadPoolExecutor(max_workers=1) as pool:
            future=pool.submit(upload);deadline=time.monotonic()+120
            while True:
                started=time.monotonic();rows=request('/api/logs')['logs']
                latencies.append(1000*(time.monotonic()-started))
                row=next(r for r in rows if r['id']==args.record)
                if row['status']=='processing' and not future.done():overlap+=1
                assert row['status']!='error'
                if row['status']=='ready' and future.done():break
                assert time.monotonic()<deadline
                time.sleep(.02)
            future.result()
        assert overlap>0 and len(chunks)==64
        def stats(values):
            values=sorted(values)
            return {'samples':len(values),**{f'p{p}_ms':values[math.ceil(len(values)*p/100)-1] for p in (50,95,99)},'max_ms':max(values)}
        report={'real_recording':True,'synthetic_upload_bytes':16777216,'processing_with_upload_samples':overlap,'api':stats(latencies),'chunks':stats(chunks),'request_failures':0,'scope':'one loopback run, no TLS or HAOS'}
        args.output.write_text(json.dumps(report,indent=2));print(json.dumps(report))
    finally:request(f'/api/uploads/{session}','DELETE')


if __name__=='__main__':main()
