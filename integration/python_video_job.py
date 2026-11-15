"""Private-input benchmark adapter for preserved Python video code.

validated: HEVC uses the original prepare_video; TS reproduces its inline
decoder.py video block without log/render work. core measures only packet muxing,
with every output independently verified by the caller outside the timed job.
No source checkout is modified and no encoder is opened.
"""
import argparse
import gc
from fractions import Fraction
import json
from pathlib import Path
import sys


class Progress:
    def update(self,*args,**kwargs):pass


def mux(source,output,times,hevc):
    import av
    with av.open(str(source)) as inp,av.open(str(output),'w',options={'movflags':'+faststart'}) as out:
        stream=inp.streams.video[0];target=out.add_stream_from_template(stream,opaque=True)
        if hevc:
            target.codec_context.codec_tag='hvc1';target.codec_context.thread_count=2
            target.time_base=Fraction(1,1000000)
        offset=None;count=0
        for packet in inp.demux(stream):
            if not packet.size:continue
            if hevc:
                assert not stream.codec_context.has_b_frames and count<len(times)
                packet.pts=packet.dts=round(times[count]*1000000)
                packet.duration=round((times[count+1]-times[count] if count+1<len(times) else .05)*1000000)
                packet.time_base=target.time_base
            else:
                if packet.dts is None:continue
                if offset is None:offset=packet.pts
                packet.pts-=offset;packet.dts-=offset
            packet.stream=target;out.mux(packet);count+=1
        assert count==len(times)


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('reference',type=Path);p.add_argument('job',type=Path);p.add_argument('output',type=Path)
    p.add_argument('--mode',choices=('validated','core'),default='validated')
    args=p.parse_args();sys.path.insert(0,str(args.reference/'app'))
    import av
    from hevc_video import prepare_video
    from video_packets import Pictures,inspect_mp4
    # decoder.py disables cyclic collection in its one-job conversion child.
    gc.disable()
    job=json.loads(args.job.read_text());source=Path(job['source']);times=job['times'];hevc=source.suffix=='.hevc'
    if args.mode=='core':mux(source,args.output,times,hevc)
    elif hevc:prepare_video(source,args.output,job['indices'],job['indices'][0]['timestampEof']/1e9,Progress())
    else:
        pts=[]
        with av.open(str(source)) as inp:
            stream=inp.streams.video[0];pictures=Pictures(stream,annex_b=True)
            for packet in inp.demux(stream):
                if packet.pts is None or not packet.size:continue
                pts.append(float(packet.pts*stream.time_base));pictures.add(packet)
            expected=pictures.finish()
        pts.sort();rows=job['indices'];assert len(pts)==len(rows)
        assert [r['segmentId'] for r in rows]==list(range(len(rows)))
        offsets=[r['timestampEof']/1e9-t for r,t in zip(rows,pts)]
        assert max(offsets)-min(offsets)<.005
        mux(source,args.output,times,False)
        inspect_mp4(args.output,times,Progress(),expected)


if __name__=='__main__':main()
