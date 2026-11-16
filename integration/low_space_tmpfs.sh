#!/bin/bash
# Real ENOSPC without root: the MP4 goes to a small tmpfs mounted in a user namespace, the
# synthetic raw HEVC input (about 10 MB) stays on the caller's file system.
#   1.5x the input:        under the engine's need (2x for payload + MP4, plus the 100 MiB margin):
#                          refused before any write as a resource failure (`low_space`).
#   2x + margin + 1 MiB:   enough; the pure engine writes the MP4.
#   1.5x, check bypassed:  (DEBUG_BINARY only, HARS_AVAILABLE_SPACE_BYTES states the room) what the
#                          engine does when it runs anyway: ENOSPC, a resource failure, nothing left.
# usage: low_space_tmpfs.sh BINARY [DEBUG_BINARY]   (prints one line per run)
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
run() { # label binary tmpfs_bytes [stated_free_bytes]
  unshare --user --map-root-user --mount bash -c "
    mount -t tmpfs -o size=$3 tmpfs '$W/mnt' || exit 9
    env -u HARS_REMUX_ENGINE ${4:+HARS_AVAILABLE_SPACE_BYTES=$4} RUST_LOG=roadviewer_hars=info '$2' remux '$W/fcamera.hevc' fcamera.hevc '$W/clock.json' '$W/mnt/out.mp4' > '$W/run.out' 2> '$W/run.err'
    rc=\$?
    echo \"$1 tmpfs=$3 exit=\$rc out=\$(stat -c %s '$W/mnt/out.mp4' 2>/dev/null || echo none) left=\$(ls '$W/mnt' | wc -l)\"
  "
  grep -o '"event":"remux_[a-z_]*"\|"failure_class":"[a-z]*"\|"reason_code":"[a-z_]*"\|"engine":"[a-z]*"' "$W/run.err" | tr '\n' ' '; echo
  grep -v '^{' "$W/run.err" | head -2
}
margin=$(( 100 * 1024 * 1024 ))
run "release 1.5x" "$1" $(( size * 3 / 2 ))
run "release 2x+margin" "$1" $(( size * 2 + margin + 1024 * 1024 ))
if [ -n "${2:-}" ]; then
  run "debug 1.5x check-bypassed" "$2" $(( size * 3 / 2 )) $(( size * 4 + margin ))
fi
