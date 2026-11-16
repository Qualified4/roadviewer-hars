#!/bin/bash
# Real ENOSPC without root: the MP4 goes to a small tmpfs mounted in a user namespace, the
# synthetic raw HEVC input (about 10 MB) stays on the caller's file system.
#   1.5x the input: the pure engine needs about 2x (payload + MP4), so it is skipped (low_space)
#                   and FFmpeg writes the single copy.
#   0.5x the input: FFmpeg fails with ENOSPC; its worker exits 75, a resource failure.
# usage: low_space_tmpfs.sh BINARY [BINARY...]   (prints one line per binary and size)
set -u
W=$(mktemp -d /tmp/low-space.XXXXXX)
trap 'rm -rf "$W"' EXIT
ffmpeg -v error -nostdin -y -f lavfi -i testsrc2=size=1280x720:rate=20 -frames:v 200 -an \
  -c:v libx265 -x265-params pools=2:frame-threads=1:bframes=0:keyint=20:log-level=error:qp=8 \
  -f hevc "$W/fcamera.hevc"
python3 -c "import json;print(json.dumps([i*0.05 for i in range(200)]))" > "$W/clock.json"
size=$(stat -c %s "$W/fcamera.hevc")
echo "input bytes $size"
mkdir -p "$W/mnt"
run() { # label binary tmpfs_bytes
  unshare --user --map-root-user --mount bash -c "
    mount -t tmpfs -o size=$3 tmpfs '$W/mnt' || exit 9
    env -u HARS_REMUX_ENGINE RUST_LOG=roadviewer_hars=info '$2' remux '$W/fcamera.hevc' fcamera.hevc '$W/clock.json' '$W/mnt/out.mp4' > '$W/run.out' 2> '$W/run.err'
    rc=\$?
    echo \"$1 tmpfs=$3 exit=\$rc out=\$(stat -c %s '$W/mnt/out.mp4' 2>/dev/null || echo none) left=\$(ls '$W/mnt' | wc -l)\"
  "
  grep -o '"event":"remux_[a-z_]*"\|"reason_code":"[a-z_]*"\|"engine":"[a-z]*"\|"fallback_result":"[a-z]*"' "$W/run.err" | tr '\n' ' '; echo
  grep -v '^{' "$W/run.err" | head -2
}
for binary in "$@"; do
  run "$binary 1.5x" "$binary" $(( size * 3 / 2 ))
  run "$binary 0.5x" "$binary" $(( size / 2 ))
done
