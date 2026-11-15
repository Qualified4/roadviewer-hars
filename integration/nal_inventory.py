"""Read-only normalized NAL inventory, without pixel decode or payload output.

Start-code lengths and Annex B trailing zeros are framing, excluded from hashes.
All NAL types (including non-VCL) are counted; repeated configuration units are
retained in the ordered hashes. This is distinct from the product's VCL SHA plus
decoder-header-set policy. Use the same decoder library for both sides.
"""
import argparse
import collections
import hashlib
import json
from pathlib import Path
import re
import av


def inspect(path):
    counts=collections.Counter();hashes=collections.defaultdict(hashlib.sha256);overall=hashlib.sha256()
    with av.open(str(path),options={'protocol_whitelist':'file,pipe','format_whitelist':'mov,hevc,mpegts'}) as c:
        streams=list(c.streams.video);assert len(streams)==1
        stream=streams[0];codec=stream.codec_context.name;assert codec in ('h264','hevc')
        bsf=av.bitstream.BitStreamFilterContext(codec+'_mp4toannexb',stream) if path.suffix=='.mp4' else None
        for packet in c.demux(stream):
            if not packet.size:continue
            assert not packet.is_corrupt
            for part in bsf.filter(packet) if bsf else (packet,):
                for unit in re.split(b'\x00\x00\x00?\x01',bytes(part))[1:]:
                    unit=unit.rstrip(b'\0');assert len(unit)>=(2 if codec=='hevc' else 1)
                    kind=(unit[0]>>1)&63 if codec=='hevc' else unit[0]&31
                    encoded=len(unit).to_bytes(4,'big')+unit
                    counts[kind]+=1;hashes[kind].update(encoded);overall.update(encoded)
    return {'codec':codec,'all_nal_sha256':overall.hexdigest(),'types':{k:{'count':counts[k],'sha256':h.hexdigest()} for k,h in hashes.items()}}


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('media',type=Path,nargs='+');args=p.parse_args()
    print(json.dumps([inspect(path) for path in args.media],indent=2))
