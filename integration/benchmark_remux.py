"""Interleaved, video-only benchmark. Private prepared jobs contain camera clocks.

Every output is independently audited outside the timed interval. validated
includes video inspections, process startup and muxing, but no log/JSON replay/
thumbnail work. core times the packet-copy mux path only; validation still runs
outside its timer. Never publish private jobs, packet arrays or input filenames.
"""
import argparse
from fractions import Fraction
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
from profile_conversion import measure


def audit(path,times,reference,expected=None):
    import av
    sys.path.insert(0,str(reference/'app'))
    from video_packets import Pictures
    packets=[]
    with av.open(str(path)) as inp:
        stream=inp.streams.video[0]
        pictures=Pictures(stream,annex_b=path.suffix in ('.ts','.hevc'))
        for packet in inp.demux(stream):
            if not packet.size:continue
            packets.append((packet.pts,packet.dts,int(packet.is_keyframe),packet.duration))
            pictures.add(packet)
        signature=pictures.finish()
        if expected is not None:assert signature==expected,'compressed picture or decoder header changed'
        metadata={'codec':stream.codec_context.name,'width':stream.width,'height':stream.height,'time_base':str(stream.time_base),'codec_tag':stream.codec_context.codec_tag,'duration_seconds':float(stream.duration*stream.time_base) if stream.duration is not None else None,'packets':len(packets),'keyframes':sum(p[2] for p in packets),'picture_sha256':signature[0].hex(),'decoder_headers':sorted((k,h.hex()) for k,h in signature[1])}
        tb=stream.time_base
        if path.suffix=='.mp4':
            assert len(packets)==len(times)
            assert all(p[0] is not None and p[1] is not None for p in packets)
            assert all(b[1]>a[1] for a,b in zip(packets,packets[1:]))
            assert all(abs(float(p[0]*tb)-t)<=.002 for p,t in zip(sorted(packets),times))
            config=stream.codec_context.extradata
            assert config and config[0]==1
            if metadata['codec']=='hevc':assert metadata['codec_tag']=='hvc1'
            metadata['codec_configuration_sha256']=hashlib.sha256(config).hexdigest()
    if path.suffix=='.mp4':
        boxes={};at=0;size=path.stat().st_size
        with path.open('rb') as f:
            while at<size:
                f.seek(at);header=f.read(8);assert len(header)==8
                length=int.from_bytes(header[:4],'big');kind=header[4:].decode('ascii')
                if length==1:length=int.from_bytes(f.read(8),'big')
                elif length==0:length=size-at
                assert length>=8 and at+length<=size
                boxes[kind]=at;at+=length
        assert boxes['moov']<boxes['mdat'];metadata['faststart']=True
    return metadata,signature,[(Fraction(p[0])*tb if p[0] is not None else None,Fraction(p[1])*tb if p[1] is not None else None,p[2],Fraction(p[3])*tb) for p in packets]


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('jobs',type=Path);p.add_argument('reference',type=Path);p.add_argument('output',type=Path)
    p.add_argument('--binary',action='append',required=True);p.add_argument('--runs',type=int,default=3)
    p.add_argument('--mode',choices=('validated','core'),default='validated')
    p.add_argument('--segments',type=int,nargs='*')
    args=p.parse_args();assert args.runs>=3;args.output.mkdir(parents=True,exist_ok=False)
    binaries=dict(s.split('=',1) for s in args.binary)
    env={k:v for k,v in os.environ.items() if not k.startswith('HARS_PROFILE')}
    data={'scope':args.mode,'warmups':1,'runs_per_segment':args.runs,'cache':'warm, no global cache drop','python_cyclic_gc':False,'rss':'20ms sampled process tree sum; shared pages may repeat','samples':[],'media':[]}
    helper=Path(__file__).with_name('python_video_job.py')
    for jobpath in sorted(args.jobs.glob('*.json')):
        if jobpath.name.endswith('.times.json'):continue
        job=json.loads(jobpath.read_text());segment=job['segment'];kind=job['kind']
        if args.segments and segment not in args.segments:continue
        source=Path(job['source']);times=job['times'];clock=jobpath.with_suffix('.times.json')
        source_meta,signature,_=audit(source,times,args.reference)
        source_meta.update({'segment':segment,'kind':kind,'input_bytes':source.stat().st_size})
        data['media'].append(source_meta)
        golden=None;golden_metadata=None;variants=['python',*binaries]
        def run(name,label,warmup=False):
            nonlocal golden,golden_metadata
            output=args.output/f'{segment}-{kind}-{label}.mp4'
            if name=='python':command=[sys.executable,str(helper),str(args.reference),str(jobpath),str(output),'--mode',args.mode]
            elif args.mode=='validated':command=[binaries[name],'remux',str(source),kind,str(clock),str(output)]
            elif kind.endswith('.hevc'):command=[binaries[name],'__native-hevc',str(source),str(clock),str(output)]
            else:command=['ffmpeg','-v','error','-nostdin','-n','-protocol_whitelist','file,pipe','-f','mpegts','-i',str(source),'-map','0:v:0','-an','-c:v','copy','-map_metadata','-1','-movflags','+faststart','-f','mp4',str(output)]
            metrics=measure(command,env,output.with_suffix('.log'))
            metadata,_,packets=audit(output,times,args.reference,signature)
            if golden is None:assert name=='python';golden=packets;golden_metadata=metadata
            assert len(packets)==len(golden)
            delta=max(abs(a[i]-b[i]) for a,b in zip(packets,golden) for i in (0,1))
            assert delta<=Fraction(1,1000000),f'PTS/DTS mismatch: {float(delta)}'
            assert [p[2] for p in packets]==[p[2] for p in golden],'keyframe index changed'
            duration_delta=max(abs(a[3]-b[3]) for a,b in zip(packets,golden))
            metrics.update({'variant':name,'segment':segment,'kind':kind,'round':label,'input_bytes':source.stat().st_size,'output_bytes':output.stat().st_size,'max_python_timestamp_difference_seconds':float(delta),'max_python_packet_duration_difference_seconds':float(duration_delta),'python_stream_duration_difference_seconds':metadata['duration_seconds']-golden_metadata['duration_seconds'],'python_duration_equal_1e6':duration_delta<=Fraction(1,1000000),'verified':True,'verification_scope':'compressed pictures, decoder headers, frame/packet count, PTS/DTS, keyframe index, codec config and faststart; duration differences reported separately','metadata':metadata})
            if not warmup:data['samples'].append(metrics)
            (args.output/'benchmark.json').write_text(json.dumps(data,indent=2))
            print(json.dumps({k:metrics[k] for k in ('variant','segment','kind','round','elapsed_seconds','total_cpu_seconds','peak_rss_tree_kib','verified')}),flush=True)
        for name in variants:run(name,'warmup-'+name,True)
        for index in range(args.runs):
            order=variants[index%len(variants):]+variants[:index%len(variants)]
            if index%2:order=list(reversed(order))
            for name in order:run(name,f'{index+1}-{name}')
    data['summary']={}
    for kind in {m['kind'] for m in data['media']}:
        data['summary'][kind]={}
        for name in ['python',*binaries]:
            rows=[s for s in data['samples'] if s['variant']==name and s['kind']==kind]
            totals={metric:sum(statistics.median(s[metric] for s in rows if s['segment']==segment) for segment in {s['segment'] for s in rows}) for metric in ('elapsed_seconds','total_cpu_seconds')}
            size=sum(m['input_bytes'] for m in data['media'] if m['kind']==kind)
            data['summary'][kind][name]={**totals,'input_bytes':size,'weighted_mb_per_second':size/1e6/totals['elapsed_seconds'],'mean_rss_median_kib':statistics.median(s['mean_rss_tree_kib'] for s in rows),'peak_rss_max_kib':max(s['peak_rss_tree_kib'] for s in rows)}
    (args.output/'benchmark.json').write_text(json.dumps(data,indent=2));print(json.dumps(data['summary']))


if __name__=='__main__':main()
