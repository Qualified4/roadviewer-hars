#!/bin/bash
# Synthetic inputs for integration/format_support.py, made by the development ffmpeg (never committed).
#   format_support_inputs.sh C13 C14 OUT
# C13: 13-segment original corpus, C14: 14-segment MP4 corpus (read only), OUT: new folder.
set -eu
C13=$1
C14=$2
O=$3
mkdir -p $O
cd $O
FF="nice prlimit --as=4294967296 ffmpeg -v error -nostdin -y"
q13() { ls $C13/*--$1--qcamera.ts; }
f13() { ls $C13/*--$1--fcamera.hevc; }
X265B="pools=2:frame-threads=1:bframes=0:keyint=30:min-keyint=30:scenecut=0:open-gop=0:vui-timing-info=1:slices=2:log-level=error"
t0=$(date +%s)
# B1: AR0231/OX03C10 road/wide camera (1928x1208, Main, level 5, GOP 30, no B, 10 Mbps). libx265 without a
# global header repeats VPS/SPS/PPS before every IRAP, as the device file does (corpus: 60 for 60 IDRs).
[ -s b1-1928.hevc ] || $FF -f lavfi -i testsrc2=size=1928x1208:rate=20 -frames:v 1200 -an -c:v libx265 -preset ultrafast \
  -profile:v main -x265-params "$X265B:level-idc=5:bitrate=10000:vbv-maxrate=10000:vbv-bufsize=10000" -pix_fmt yuv420p -f hevc b1-1928.hevc
echo "b1 $(($(date +%s)-t0))s"
# B5: qcamera.ts with the RecordAudio AAC track (mono, 16 kHz, 32 kbit/s as video_writer.cc/micd.py).
V0=$(ffprobe -v error -select_streams v:0 -show_entries stream=start_time -of csv=p=0 $(q13 2) | head -1)
[ -s b5-audio.ts ] || $FF -copyts -i $(q13 2) -itsoffset $V0 -f lavfi -i sine=frequency=440:sample_rate=16000 -map 0:v:0 -map 1:a:0 \
  -c:v copy -c:a aac -b:a 32k -ac 1 -shortest -muxdelay 0 -f mpegts b5-audio.ts
# B6: PC path qcamera (FfmpegEncoder: libx264 with libavcodec defaults, 526x330 yuv420p) muxed to TS.
[ -s b6-pc.ts ] || $FF -copyts -i $(q13 3) -map 0:v:0 -c:v libx264 -fps_mode passthrough -enc_time_base:v 1:90000 -muxdelay 0 -f mpegts b6-pc.ts
# B7: PC path fcamera (BIG_BOX_LOSSLESS: FFVHUFF in Matroska, named fcamera.hevc).
[ -s b7-pc.mkv ] || $FF -f lavfi -i testsrc2=size=1344x760:rate=20 -frames:v 60 -an -c:v ffvhuff -pix_fmt yuv420p -f matroska b7-pc.mkv
echo "b done $(($(date +%s)-t0))s"
# C cases
[ -s c01-bframe.ts ] || $FF -copyts -i $(q13 4) -map 0:v:0 -c:v libx264 -bf 2 -g 15 -fps_mode passthrough -enc_time_base:v 1:90000 -muxdelay 0 -f mpegts c01-bframe.ts
[ -s c02-interlaced.ts ] || $FF -copyts -i $(q13 5) -map 0:v:0 -vf scale=526:332 -c:v libx264 -bf 0 -g 15 -x264-params interlaced=1:tff=1 -flags +ildct+ilme -fps_mode passthrough -enc_time_base:v 1:90000 -muxdelay 0 -f mpegts c02-interlaced.ts
[ -s c03-bframe.hevc ] || $FF -f lavfi -i testsrc2=size=1344x760:rate=20 -frames:v 1200 -an -c:v libx265 -preset ultrafast -x265-params "pools=2:bframes=3:keyint=20:log-level=error" -pix_fmt yuv420p -f hevc c03-bframe.hevc
if [ ! -s c04-joined.hevc ]; then
  $FF -f lavfi -i testsrc2=size=176x96:rate=20 -frames:v 20 -an -c:v libx265 -x265-params pools=1:frame-threads=1:bframes=0:keyint=20:log-level=error -f hevc other.hevc
  cat $(f13 7) other.hevc > c04-joined.hevc; rm other.hevc
fi
python3 -I - "$(q13 8)" "$(f13 9)" <<'EOF'
import sys, os, random
ts = open(sys.argv[1], 'rb').read()
pids = [((ts[i+1] & 0x1f) << 8) | ts[i+2] for i in range(0, len(ts) - 187, 188)]
video = max(set(pids), key=pids.count)
idx = [i for i, p in enumerate(pids) if p == video]
cut = idx[int(len(idx) * 0.6)] * 188 + 100   # inside a video packet's payload
open('c05-truncated.ts', 'wb').write(ts[:cut])
hevc = open(sys.argv[2], 'rb').read()
open('c06-truncated.hevc', 'wb').write(hevc[:int(len(hevc) * 0.6) + 37])
open('c07-empty.hevc', 'wb').write(b'')
random.seed(20261117)
open('c08-random.hevc', 'wb').write(bytes(random.getrandbits(8) for _ in range(1 << 20)))
# PAT and PMT: flip payload bytes of every PID 0 packet and of the PMT PID packets (CRC fails).
pmt = None
out = bytearray(ts)
for i, p in enumerate(pids):
    o = i * 188
    if p == 0:
        if pmt is None:
            pl = o + 4 + (1 + ts[o + 4] if ts[o + 1] & 0x40 else 0)
            pmt = ((ts[pl + 8 + 2] & 0x1f) << 8) | ts[pl + 8 + 3]
        for k in range(o + 10, o + 16):
            out[k] ^= 0x5a
for i, p in enumerate(pids):
    if p == pmt:
        o = i * 188
        for k in range(o + 10, o + 16):
            out[k] ^= 0x5a
open('c09-psi.ts', 'wb').write(bytes(out))
print('pmt pid', pmt)
EOF
ls $C14/*--2--qcamera.mp4 >/dev/null
[ -s c10-bframe.mp4 ] || $FF -i $(ls $C14/*--2--qcamera.mp4) -map 0:v:0 -c:v libx264 -bf 2 -g 15 -fps_mode passthrough -f mp4 c10-bframe.mp4
python3 -I - "$(ls $C14/*--3--qcamera.mp4)" <<'EOF'
import sys, struct
d = bytearray(open(sys.argv[1], 'rb').read())
i = 0
while i < len(d):
    size, kind = struct.unpack('>I4s', d[i:i+8])
    if kind == b'moov':
        d[i+4:i+8] = b'free'
    i += size if size else len(d)
open('c11-nomoov.mp4', 'wb').write(bytes(d))
EOF
[ -s c12-main10.hevc ] || $FF -f lavfi -i testsrc2=size=1344x760:rate=20 -frames:v 1200 -an -c:v libx265 -preset ultrafast -profile:v main10 -x265-params "pools=2:bframes=0:keyint=20:log-level=error" -pix_fmt yuv420p10le -f hevc c12-main10.hevc
echo "all done $(($(date +%s)-t0))s"
for f in b1-1928.hevc b5-audio.ts b6-pc.ts c01-bframe.ts c02-interlaced.ts c03-bframe.hevc c10-bframe.mp4 c12-main10.hevc; do
  echo "$f: $(ffprobe -v error -show_entries stream=codec_type,codec_name,profile,level,width,height,pix_fmt,field_order,has_b_frames -of compact=p=0 $f | tr '\n' ' ')"
done
