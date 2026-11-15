"""Pinned RV1 client uploads real files to an owned loopback server only.

Credentials remain in memory/private server state. Original corpus is read only.
Run once with retention, once without, to verify exact downloads and lossless
reverse export separately. Reports contain counts/metrics, never raw replay/GPS.
"""
import argparse
import asyncio
from fractions import Fraction
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import threading
import time
import urllib.request
import uuid
import aiohttp
from ccnc_compat import client,pairing,port,ready,stop,ROOT
from profile_conversion import processes
from benchmark_remux import audit
from benchmark_conversion import digest as file_digest


async def main(args):
    args.output.mkdir(parents=True,exist_ok=False)
    files=[next(args.corpus.glob(f'*--{args.segment}--{kind}')) for kind in ('rlog.zst','qcamera.ts','fcamera.hevc','ecamera.hevc') if list(args.corpus.glob(f'*--{args.segment}--{kind}'))]
    assert len(files)>=3 and files[0].name.endswith('rlog.zst')
    route=files[0].name.rsplit('--',2)[0]
    ui='http://127.0.0.1:'+str(port());device='http://127.0.0.1:'+str(port())
    env=dict(os.environ,HARS_DATA=str(args.output/'data'),HARS_INGRESS_ONLY='0',HARS_BIND=ui.removeprefix('http://'),HARS_DEVICE_BIND=device.removeprefix('http://'),HARS_WEB=str(ROOT/'reference/python/roadviewer/app/web'))
    phases={'upload':{'rss':[],'api_ms':[]},'conversion':{'rss':[],'api_ms':[]}};phase='upload';done=False;chunks=[];monitor_errors=[]
    with (args.output/'server.log').open('wb') as log:
        server=subprocess.Popen([str(args.binary)],env=env,stdout=log,stderr=log)
        async with aiohttp.ClientSession() as http:
            async def request(path,method='GET',body=None):
                async with http.request(method,ui+path,json=body,headers={'X-RoadViewer-Request':'1'}) as r:
                    assert r.status==200
                    return await r.json()
            def monitor():
                while not done:
                    selected=phase;rss=0
                    for pid in processes(server.pid):
                        try:rss+=int(next(s for s in (Path('/proc')/str(pid)/'status').read_text().splitlines() if s.startswith('VmRSS:')).split()[1])
                        except (OSError,StopIteration):pass
                    phases[selected]['rss'].append(rss)
                    # Independent observer: source reads / hashing in the
                    # uploader's asyncio loop must not inflate UI latency.
                    started=time.monotonic()
                    try:
                        with urllib.request.urlopen(ui+'/api/logs',timeout=30) as response:
                            assert response.status==200;response.read()
                    except Exception as error:
                        monitor_errors.append(type(error).__name__);return
                    phases[selected]['api_ms'].append(1000*(time.monotonic()-started));time.sleep(.02)
            task=None
            try:
                await ready(http,ui,server)
                await request('/api/settings/processing','POST',{'auto_convert':True,'concurrency':1,'keep_original_video':not args.reverse})
                credentials=await pairing(http,ui,device)
                manifest={'batch_id':uuid.uuid4().hex,'segments':[{'route':route,'segment':args.segment,'files':[{'kind':f.name.split('--')[-1],'size':f.stat().st_size,'sha256':None} for f in files]}]}
                session=await client.begin(http,credentials,manifest);task=threading.Thread(target=monitor);task.start();hashes=[]
                for index,file in enumerate(files):
                    digest=hashlib.sha256();offset=0
                    with file.open('rb') as source:
                        while block:=source.read(min(session['chunk_size'],2*1024*1024)):
                            started=time.monotonic();response=await client.session_request(http,device,session,'PUT',f'/files/{index}?offset={offset}',block);chunks.append(1000*(time.monotonic()-started))
                            assert response['received']==offset+len(block)
                            if index==0 and offset==0:
                                repeated=await client.session_request(http,device,session,'PUT','/files/0?offset=0',block)
                                assert repeated['received']==len(block)
                                session=await client.begin(http,credentials,manifest)
                                assert session['files'][0]['received']==len(block)
                            digest.update(block);offset+=len(block)
                    hashes.append(digest.hexdigest())
                finished=await client.session_request(http,device,session,'POST','/finish',json.dumps({'sha256':hashes}).encode())
                assert len(finished['logs'])==1;record=finished['logs'][0]
                if isinstance(record,dict):record=record['id']
                assert isinstance(record,str)
                assert await client.session_request(http,device,session,'POST','/finish',json.dumps({'sha256':hashes}).encode())==finished
                phase='conversion';deadline=time.monotonic()+120;states=[]
                while True:
                    row=next(r for r in (await request('/api/logs'))['logs'] if r['id']==record);states.append(row['status'])
                    assert row['status']!='error'
                    if row['status']=='ready':break
                    assert time.monotonic()<deadline;await asyncio.sleep(.1)
                done=True;task.join();assert not monitor_errors
                assert (await request(f'/api/logs/{record}/data'))['replayV']==2
                assert (await request(f'/api/logs/{record}/telemetry'))['streams']['carState']['times']
                downloads=[]
                for file,digest in zip(files,hashes):
                    kind=file.name.split('--')[-1];output=args.output/kind
                    async with http.get(ui+f'/api/logs/{record}/original/{kind}') as r:
                        assert r.status==200
                        with output.open('wb') as dest:
                            async for block in r.content.iter_chunked(1048576):dest.write(block)
                    byte_identical=file_digest(output)==digest
                    if kind=='rlog.zst' or not args.reverse:assert byte_identical
                    if kind=='rlog.zst':continue
                    job=json.loads((args.jobs/f'{args.segment}-{kind}.json').read_text())
                    _,signature,original_packets=audit(file,job['times'],args.reference)
                    _,_,restored_packets=audit(output,job['times'],args.reference,signature)
                    clock_delta=None
                    if kind.endswith('.ts'):
                        clock_delta=max(abs(a[0]-b[0]) for a,b in zip(original_packets,restored_packets));assert clock_delta<=Fraction(1,1000000)
                    camera='qcamera' if kind=='qcamera.ts' else ('front' if kind=='fcamera.hevc' else 'wide')
                    mp4=args.output/(camera+'.mp4')
                    async with http.get(ui+f'/api/logs/{record}/download?source={camera}&format=mp4') as r:
                        assert r.status==200
                        with mp4.open('wb') as dest:
                            async for block in r.content.iter_chunked(1048576):dest.write(block)
                    audit(mp4,job['times'],args.reference,signature)
                    with mp4.open('rb') as source:first=source.read(128)
                    async with http.get(ui+f'/api/logs/{record}/download?source={camera}&format=mp4',headers={'Range':'bytes=0-127'}) as r:
                        assert r.status==206 and await r.read()==first
                    downloads.append({'camera':camera,'compressed_bitstream_and_headers':True,'original_file_byte_identity':byte_identical,'file_identity_required':not args.reverse,'ts_pts_delta_seconds':float(clock_delta) if clock_delta is not None else None,'range206':True})
                def stats(rows):
                    rows=sorted(rows);return {'samples':len(rows),**{f'p{p}_ms':rows[math.ceil(len(rows)*p/100)-1] for p in (50,95,99)},'max_ms':max(rows)}
                report={'real_corpus':True,'segment':args.segment,'files':len(files),'reverse_export':args.reverse,'rv1_hmac_client':True,'reconnect_retry_receipt':True,'auto_conversion':True,'states':list(dict.fromkeys(states)),'download_checks':downloads,'chunk_latency':stats(chunks),'phases':{k:{'api':stats(v['api_ms']),'rss_samples':len(v['rss']),'mean_sampled_tree_rss_kib':sum(v['rss'])/len(v['rss']),'peak_sampled_tree_rss_kib':max(v['rss'])} for k,v in phases.items()},'scope':'one real upload, independent API observer thread, sampled process tree RSS; upload includes finish validation; loopback without TLS/Ingress; no physical openpilot device'}
                (args.output/'result.json').write_text(json.dumps(report,indent=2));print(json.dumps(report))
            finally:
                done=True
                if task is not None:
                    task.join(timeout=35)
                stop(server)


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('corpus',type=Path);p.add_argument('binary',type=Path);p.add_argument('jobs',type=Path);p.add_argument('reference',type=Path);p.add_argument('output',type=Path)
    p.add_argument('--segment',type=int,default=3);p.add_argument('--reverse',action='store_true')
    asyncio.run(main(p.parse_args()))
