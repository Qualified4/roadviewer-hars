# Thumbnail decoding without FFmpeg (2.0.0-alpha.4, step 2)

<!-- toc -->
**Contents**

- [Result](#result)
- [What the thumbnail needs](#what-the-thumbnail-needs)
- [Candidate decoders](#candidate-decoders)
- [Design](#design)
- [Verification](#verification)
- [Differences from alpha.3](#differences-from-alpha3)
- [Not verified, and remaining risks](#not-verified-and-remaining-risks)
- [Speed-up (2026-11-17)](#speed-up-2026-11-17)
  - [Where the time went (alpha.4, before)](#where-the-time-went-alpha4-before)
  - [SIMD and threads, measured](#simd-and-threads-measured)
  - [What was changed](#what-was-changed)
  - [Tried and not applied](#tried-and-not-applied)
  - [Correctness](#correctness)
  - [After](#after)
  - [What limits it now](#what-limits-it-now)
  - [Not verified](#not-verified)
<!-- /toc -->

## Result

`src/thumbnail.rs` no longer uses `ffmpeg-next`. The `__thumbnail` worker reads the camera MP4
itself, decodes H.264 with `rusty_h264-decoder 0.17.2` and HEVC with `rusty_h265 0.7.1` (both
pure Rust, SIMD features off, so the four rusty crates are all `forbid(unsafe_code)`), reduces the
picture with a port of libswscale's bilinear scaler, and writes the JPEG with its own baseline
encoder. Frame selection, the score, the levelling and `thumbnail.json` are unchanged.

On the real corpus the result is identical to alpha.3 up to the JPEG encoder: same decoded
frames, same scaled candidates, same chosen second, same score, same levelled picture. The JPEG
bytes differ (a different DCT), within 4 to 9 levels of alpha.3's.

`Cargo.toml` still depends on `ffmpeg-next` for the other modules (step 3 removes it). The
thumbnail path no longer links `libswscale`, but `media::thumbnail` still runs
`validate_upload` (`ffprobe`) before the worker; that check belongs to step 3.

## What the thumbnail needs

Measured on the corpus (`ffprobe` and the `trace_headers` bitstream filter over the first 160
packets of every camera file: 13 + 14 + 47 segments, 175 files).

Which video: `convert::prepare` makes the thumbnail from the converted MP4 of the first camera
that converts, in the order qcamera, front, wide (Python `decoder.py` has the same order). Every
segment of the three corpora has a qcamera, so every real thumbnail comes from the H.264
`camera.mp4`. HEVC is the fallback when a segment has no usable qcamera (an upload of
`fcamera`/`ecamera` alone, or a failed qcamera conversion). The decoder input is always an MP4
(`camera.mp4`, `fcamera.mp4`, `ecamera.mp4`), never the raw `.ts`/`.hevc`.

Frames used: the first decodable frame and the first frame at or after 1, 2, 3, 4 and 5 s from
it, within 140 video packets; at 20 fps that is frames 1 to 101.

| | qcamera (H.264) | fcamera / ecamera (HEVC) |
|---|---|---|
| profile / level | High (100) / 2.1 | Main / 5.0 |
| bit depth, chroma | 8 bit, 4:2:0 | 8 bit, 4:2:0 |
| coded / displayed size | 528x336 / 526x330 (crop 2 right, 6 bottom) | 1344x768 / 1344x760 (conformance window 8 bottom) |
| picture types | IDR + P, no B (`has_b_frames` 0, POC type 2) | IDR_N_LP + TRAIL_R, I + P, no B, no reordering |
| entropy coding | CABAC (`cabac_init_idc` 1) | CABAC |
| tools | 8x8 transform on, no scaling matrices, 1 reference, no weighted prediction, single slice, frame_mbs_only | tiles on, 2 slice segments per picture, SAO, `cu_qp_delta`, TMVP, 3 merge candidates, no AMP/PCM/scaling lists/transform skip/sign hiding/weighted prediction/long-term refs |
| colour range | not signalled (limited) | VUI `tv` (limited) |
| MP4 | `avc1`, one `elst` entry with media time 0, no `ctts` | `hvc1`, same |

HEVC decoding is therefore needed: not for any thumbnail of the corpus, but to keep the fallback
to the front and wide cameras that alpha.3 and Python have.

## Candidate decoders

Sizes are of stripped release binaries (`lto = "thin"`) that read a file and decode it, against
the same program without a decoder (331,960 bytes). Exactness: frame MD5 against host FFmpeg
6.1.1 over 140 frames of one qcamera and one fcamera sample.

| | rusty_h264-decoder 0.17.2 + rusty_h265 0.7.1 (chosen) | rust_h264 0.6.1 + rust_h265 0.4.1 | openh264 (+ a HEVC decoder) | libde265 |
|---|---|---|---|---|
| language | Rust | Rust | C++ (bundled, `cc`) | C++ |
| licence | BSD-2-Clause / Apache-2.0 | MIT OR Apache-2.0 | BSD-2-Clause code; Cisco's patent licence covers only Cisco's binaries, not a build from source | LGPL-3.0: static linking needs relinkable objects, dynamic linking a shipped `.so` |
| codecs | H.264 (no 4:2:2/4:4:4, no >8 bit, no interlace) / HEVC Main, Main 10 | H.264 / HEVC | H.264 Constrained Baseline + some High; no HEVC | HEVC only |
| `unsafe` | none in the four rusty crates with SIMD off (`forbid(unsafe_code)`); their dependencies `bytemuck`, `once_cell`, `wide` and `safe_arch` (SIMD wrappers) do contain `unsafe` | 62 / 41 `unsafe` sites in the decoder core itself (DPB, shared tables, SIMD) | C++ | C++ |
| build | cargo only | cargo only | C++ compiler, nasm for x86 assembly | system package or C++ build |
| added size | +1,472,864 bytes (decoder program); +1,471,392 bytes on the stripped HARS binary | +2,173,344 bytes (decoder program) | not built | `libde265-0` package 375 KiB installed, plus an H.264 decoder |
| exactness (140 + 140 frames) | 140/140, 140/140 | 140/140, 140/140 | not measured | not measured |
| corpus exactness | 5,040/5,040 frames (below) | not measured | | |
| maintenance | releases 2026-11; HEVC gated on the JCT-VC conformance suite, H.264 on the openh264 corpus; fuzzed against panics | releases 2026 | maintained | maintained |
| aarch64 | `cargo check --target aarch64-unknown-linux-gnu` passes | not checked | | |

The rusty pair is chosen: the only candidate with no `unsafe` and no C, under permissive
licences, the smallest of the measured Rust options, and bit exact on the whole corpus. The C
options would bring back a native dependency, a patent question (openh264) or LGPL obligations
(libde265) and still need a second decoder for the other codec. For comparison, alpha.3's
runtime FFmpeg libraries are `libavcodec60` 16,237 KiB, `libavformat60` 2,848 KiB, `libavutil58`
1,152 KiB and `libswscale7` 689 KiB installed (Ubuntu 24.04 packages; the image's Debian packages
were not measured here).

## Design

- `src/thumbnail/video.rs`: a bounded MP4 reader (moov at most 16 MiB like the upload check,
  tables at most 262,144 entries, samples at most 32 MiB, only the first 140 samples listed),
  the decoders behind `catch_unwind`, and FFmpeg's frame order and timing: presentation time
  `(dts + ctts - elst media time) / timescale`, samples before the edit decoded but not output,
  a sample that fails to decode skipped (the H.264 decoder is reset and resumes at the next IDR),
  a file cut short read up to its end, and at the 140-packet limit the last sample's picture
  withheld and nothing flushed, as the two-thread frame-threaded decoder of alpha.3 does. The
  size bound (4096x2160) is checked on the parameter sets' cropped size, as FFmpeg's stream
  parameters carry it, and again on every decoded picture.
- `src/thumbnail/scale.rs`: `initFilter` for `SWS_BILINEAR` (x86 filter alignment 4 / 2),
  `hScale8To15`, the limited-to-full range step (`lumRangeToJpeg`, `chrRangeToJpeg`) unless the
  stream is full range (FFmpeg then decodes to `yuvj420p` and converts nothing), and the vertical
  pass with the x86 `yuv2yuvX` rounding (products truncated to 16 bits) for all rows but the last
  two luma rows, where swscale itself uses the C function. Integer only, so the output is the same
  on every machine; it equals alpha.3 on x86-64.
- `src/thumbnail/jpeg.rs`: baseline 4:2:0 JPEG with FFmpeg's quantiser at `qmin = qmax = 4`
  (MPEG-1 intra matrix x 4 / 8, DC step 8, one table), FFmpeg's rounding (AC 3/8 of a step towards
  zero, DC half up) and per-picture optimal Huffman tables (FFmpeg's default `huffman=optimal`);
  libjpeg's integer DCT.
- `__thumbnail` worker: `pure_remux::limit_memory(thumbnail::MEMORY_BUDGET_MIB = 512)` caps its
  address space; the existing 180 s deadline, `PR_SET_PDEATHSIG` and output limit of `media::run`
  apply. Failure stays as before: the worker exits non-zero, `convert` tries the next camera and
  writes `thumbnail.failed` when none gives a thumbnail (Python's marker).

## Verification

Measurements are in `docs/hars/measurements/thumbnail-decoder-2026-11-17.json`.

All on x86-64 (WSL Ubuntu 24.04), against the alpha.3 release binary built from `d846fc6`
with the system FFmpeg 6.1.1 and a harness that compiles this branch's `video.rs` and `scale.rs`
unchanged next to the alpha.3 `ffmpeg-next` code to compare intermediates.

Decoding, bit exact. Inputs: the 13 original segments' `camera.mp4` (converted by alpha.3) and
6 samples each of the 14 and 47 segment corpora, both their qcamera and fcamera (36 videos:
13 + 12 H.264 526x330, 11 HEVC 1344x760; the 47 corpus has one fcamera fewer than its index list).
Frames: the whole 140-packet window, a superset of the frames a thumbnail uses. Result: 5,040 of
5,040 frames have the MD5 of `ffmpeg -threads 1` (yuv420p); 0 differ, largest pixel error 0.

Thumbnails, against alpha.3, on every qcamera target of the 13 + 14 + 47 segments (74) and the
HEVC fallback of the 11 fcamera samples (85 inputs, `__thumbnail` worker of each binary):

| item | equal | values |
|---|---|---|
| thumbnail made | 85 / 85 both | |
| chosen second | 85 / 85 | seconds 1 to 5 all occur |
| score (`thumbnail.json`) | 85 / 85 | `thumbnail.json` byte equal 85 / 85 |
| output size | 85 / 85 | 320x200 (H.264), 320x180 (HEVC) |
| the five scaled candidates (MD5) | 85 / 85 | |
| levelled picture before JPEG (MD5) | 85 / 85 | |
| JPEG bytes | 0 / 85 | decoded PSNR new vs alpha.3: 52.1 / 55.0 / 58.7 dB (min / median / max), largest error 4 / 6 / 9 levels |
| JPEG fidelity to the levelled picture | | PSNR alpha.3 39.95 / 42.34 / 44.64 dB, new 40.01 / 42.40 / 44.67 dB |
| JPEG size | | 616,335 bytes in total for alpha.3, 620,206 new (+0.6%) |

The JPEG bytes differ because the DCT differs (libjpeg's integer DCT here, FFmpeg's own there);
the quantiser, its rounding, the sampling and the Huffman method are FFmpeg's. A first version
rounded coefficients to nearest: its thumbnails were 11.5% larger and 40.3 to 45.8 dB from
alpha.3's, so FFmpeg's rounding was adopted.

Failure cases: 40 inputs, each run through both workers under `prlimit --as=2147483648`:
files cut at 1/20/90% (qcamera) and 5/70% (fcamera), empty, `moov` only (twice), empty and
zeroed `mdat`, 3 + 2 randomly damaged bitstreams, a sample entry declaring 8192x4320, oversized
`stsz` count and sample size, a chunk offset past the end, `avcC` without SPS, garbage `hvcC`,
a wrong codec tag, a `moov` over 16 MiB, a box larger than its parent, random bytes, timescale 0,
an invalid `stsc`, H.264 4096x2304, H.264 High 4:4:4 / High 10 / High 4:2:2 / interlaced, HEVC
Main 10 / 4:4:4, H.264 and HEVC with B-frames, H.264 and HEVC full range, HEVC 4096x2160, and a
cut B-frame file.

| | alpha.3 | new |
|---|---|---|
| panics | 0 | 0 |
| killed / resource exit / deadline | 0 | 0 |
| outputs left after a failure (`thumbnail.jpg`, `.json`, `.tmp`) | 0 | 0 |
| thumbnail made | 27 | 18 |
| same outcome as alpha.3 (made or not) | | 31 / 40 |
| largest peak RSS | 168 MiB (4096x2160 HEVC) | 332 MiB (same input) |
| longest run | 0.50 s | 1.99 s |

The 9 different outcomes are listed under the differences below. Where both made a thumbnail
the second and score are equal except for the 5 damaged bitstreams.

Speed and memory of one `__thumbnail` run (whole worker process: start, decode, scale, JPEG),
6 inputs, warmup 1 then 5 runs alternating the two binaries; median (range) and the largest
peak RSS. Load average 2.80 before and 1.83 after (another task shared the machine).

| input | alpha.3 | new | peak RSS alpha.3 / new |
|---|---|---|---|
| 13-1 qcamera | 48.3 ms (47.7-51.2) | 122.0 ms (121.7-143.9) | 47.7 / 36.9 MiB |
| 14-0 qcamera | 53.2 ms (51.5-54.4) | 141.4 ms (140.5-143.1) | 47.6 / 36.6 MiB |
| 47-9 qcamera | 53.0 ms (51.3-55.0) | 144.4 ms (139.9-146.7) | 47.6 / 36.5 MiB |
| 13-1 fcamera | 520.2 ms (506.6-526.7) | 2,097.5 ms (2,017.0-2,172.4) | 63.5 / 43.1 MiB |
| 14-0 fcamera | 510.3 ms (501.3-514.4) | 1,937.9 ms (1,922.9-1,998.5) | 63.4 / 43.3 MiB |
| 47-9 fcamera | 498.9 ms (493.7-511.2) | 2,085.5 ms (2,054.3-2,139.6) | 62.9 / 43.1 MiB |

The pure path is 2.5 to 2.7 times slower for the H.264 thumbnail every segment gets (0.07 to
0.09 s more per segment) and 3.8 to 4.2 times slower for the HEVC fallback; it uses less memory. alpha.3 decoded with
two frame threads and FFmpeg's SIMD; the decoders here run single-threaded with their SIMD
features off (the price of no `unsafe`). Both stay far below the worker's 180 s deadline.

Size of the release binary (same toolchain and machine): 25,294,496 bytes for alpha.3 as built
(the profile strips debug information), 26,915,560 here (+1,621,064); fully stripped 19,005,232
and 20,476,624 (+1,471,392). The binary no longer links `libswscale.so.7` (4 FFmpeg libraries
instead of 5). No system package is added and none can be dropped from the image until step 3.

Product path: each release binary ran on a new temporary `HARS_DATA` with `HARS_INGRESS_ONLY=0`
on loopback ports 18630/18631 (new) and 18632/18633 (alpha.3), received two real segments
through the browser upload API (13-segment original 1: `qcamera.ts`, `fcamera.hevc`, rlog; 47
segment corpus 9: three MP4s and rlog) and converted them automatically. For both segments
`GET /api/logs/{id}/thumbnail` answered a complete JPEG of 320x200, `thumbnail.json` and the
list's `thumbnail`, `thumbnail_score`, `thumbnail_version` fields were equal to alpha.3's (scores
9.73 and 38.67), and no `thumbnail.failed` was written.

## Differences from alpha.3

- JPEG bytes (above). Everything before the encoder is equal on the corpus.
- Inputs that alpha.3 turned into a thumbnail and this branch does not (the camera then falls to
  the next one, or the segment gets `thumbnail.failed`; the conversion itself is unaffected):
  - pixel formats outside the decoders' range: H.264 4:2:2, 4:4:4, 10 bit and interlaced
    (`rusty_h264-decoder` supports none), HEVC 4:4:4 (refused by `rusty_h265`) and HEVC Main 10
    (decoded by `rusty_h265`, refused here because the scaler port covers 8-bit input only). No
    comma camera produces any of them; none occurs in the corpus.
  - malformed metadata FFmpeg tolerates: a `moov` over 16 MiB (the upload check already refuses
    it), a box larger than its parent, a media timescale of 0.
- Damaged bitstreams: both make a thumbnail, but FFmpeg conceals errors where these decoders drop
  the sample, so the frame and score chosen can differ (5 of the 5 damaged cases).
- Memory: a 4096x2160 HEVC input peaks at 332 MiB RSS (`rusty_h265` keeps 16-bit samples),
  alpha.3 at 168 MiB. The worker's limit is its own size plus 512 MiB.
- B-frames: alpha.3 got display order from FFmpeg; here H.264 pictures are reordered by MP4
  presentation time (`rusty_h265` returns output order itself). Equal on one synthetic H.264 and
  one HEVC B-frame stream; no real B-frame input exists.

## Not verified, and remaining risks

- aarch64: only `cargo check --target aarch64-unknown-linux-gnu` of the new modules and both
  decoders was run; decoding, speed and memory on aarch64 are not verified (the CI container job
  checks them). The scaler reproduces FFmpeg's x86 vertical rounding on every machine, so an
  aarch64 thumbnail now equals the x86 one. Whether alpha.3 on aarch64 (NEON `yuv2planeX`, C
  rounding) gave the same pixels is not verified; if not, aarch64 scores can differ from alpha.3
  aarch64 in the second decimal on some segments.
- The decoders are young (2026 releases by one organisation) and new to this code base. Their
  claims (HEVC conformance suite, fuzzing against panics) were not reproduced here; 40 failure
  cases and the corpus are the evidence. A panic is caught (`catch_unwind`) and the worker's
  address-space limit and deadline bound the rest.
- B-frame H.264 with edit lists or more than 16 reordered pictures is not covered by any test.
- The intermittent `409` of `tests/conversion_keep_previous.rs` noted here was a race in that
  test, present on `d846fc6` too (2 of 20 runs there); step 3 found and fixed it
  ([FFMPEG-REMOVAL.md](FFMPEG-REMOVAL.md#불안정-테스트-2건)).
- Since step 3, `media::thumbnail`'s check before the worker is the pure Rust upload check, and
  the image holds no FFmpeg program or library ([FFMPEG-REMOVAL.md](FFMPEG-REMOVAL.md)). This
  step added 1,471,392 bytes to the stripped binary and dropped its `libswscale.so.7` link.

## Speed-up (2026-11-17)

Branch `perf/thumbnail` from `b94d8a4`. Measurements are in
`docs/hars/measurements/thumbnail-performance-2026-11-17.json`; the tools are
`integration/thumbnail_*.py`. All on x86-64 (WSL Ubuntu 24.04, 16 logical CPUs), warm page cache,
inputs copied to the local ext4 disk.

### Where the time went (alpha.4, before)

Stage timings from a measurement-only build (`integration/thumbnail_instrument.py` applies
`profile::Scope`s to a copy of the tree; the product tree has none), median of 5 runs:

| stage | H.264 qcamera (3 inputs) | HEVC fcamera (3 inputs) |
|---|---|---|
| whole worker, spawn to exit | 102-106 ms | 1,752-1,965 ms |
| start-up and exit (worker total minus `make`) | 2.5-2.7 ms (a run that fails at once: 1.6 ms) | 2.9-3.4 ms |
| address-space limit | 0.05 ms | 0.05 ms |
| MP4 tables | 0.05 ms | 0.05 ms |
| sample reads | 0.4 ms (101 samples) | 0.7 ms (102-103 samples) |
| decoding | 91-95 ms (101 samples, 89%) | 1,725-1,939 ms (98.5%) |
| decoder picture to planes | 0.25 ms | 9-10 ms (16 to 8 bit) |
| scaling (6 pictures) | 6.0-6.2 ms (6%) | 19.7-20.8 ms (1.1%) |
| score, levelling | 0.1 ms | 0.1 ms |
| JPEG | 0.55 ms | 0.6 ms |
| writing `thumbnail.jpg`, `.json` (no fsync in this path) | 0.15 ms | 0.16 ms |

The parent's wait is the whole worker time; `media::run` adds no measurable work around it.

Inside the decoders (`perf record -e cpu-clock`, self time, one run each, symbols grouped by
hand, approximate):

| H.264 (`rusty_h264-*`) | share | HEVC (`rusty_h265*`) | share |
|---|---|---|---|
| deblocking | ~34% | inverse transform and residual add | ~29% |
| inter prediction (luma/chroma interpolation) | ~26% | motion compensation (FIR filters) | ~23% |
| CABAC parsing | ~15% | CABAC residual and tree parsing | ~18% |
| transform, residual add | ~4% | SAO | ~12% |
| memmove/memset | ~4% | deblocking | ~8% |
| thumbnail scaler | ~5% | thumbnail scaler | ~1% |

The real cause of the 2.5x / 3.8x gap to alpha.3 was the amount decoded, more than SIMD or
threads: a thumbnail needs the first picture and the first at or after 1 to 5 s, i.e. pictures
up to about 100, and alpha.4 (like alpha.3) decoded every sample up to the fifth candidate. The
camera files have an IDR picture every 15 samples (qcamera, 0.75 s) and every 20 (fcamera,
1 s, IDR_W_RADL), and no B-frames. Of the 101 decoded pictures, 6 are used.

### SIMD and threads, measured

- SIMD: `rusty_h265` has a `simd` feature (`rusty_h265-accel`: SSE2/SSE4.1/AVX2 with runtime
  detection, NEON on aarch64, a scalar twin of every kernel); `rusty_h264-decoder` has `asm`
  (`rusty_h264-accel`: despite the name pure Rust intrinsics, no assembly, runtime AVX2
  detection). alpha.4 builds both off. Not entirely scalar: `rusty_h264-common` always uses
  `wide` (a safe SIMD wrapper over `safe_arch`) in its transform, and the compiler vectorises with
  SSE2 (x86-64) / NEON (aarch64) by default. Turning both features on (on top of the decoding
  range below) made decoding 1.9x faster: H.264 36 -> 19 ms, 51-53 -> 29 ms; HEVC 215 -> 123 ms,
  287-316 -> 151 ms (worker total 46-64 -> 29-40 ms, 244-342 -> 151-178 ms).
- Threads: neither decoder threads the per-picture API used here. `rusty_h264-decoder` has frame
  threads only behind the `RS_H264_FRAME_THREADS` environment variable and its stream API
  (`decode_stream_threaded`); `rusty_h265` has none (tiles and WPP decode serially). The
  stretches from each IDR to a candidate are independent and could be decoded in parallel, but
  this was not built or measured: after the range change below the largest stretch is a few
  pictures, so the bound is the remaining 40-250 ms divided by the stretches, at the cost of a
  second decoder's memory under the worker's 512 MiB limit and of CPU taken from parallel
  conversions.

### What was changed

1. Decoding starts at the IDR picture before each wanted time (`video::restart`). The callback
   now returns the time before which it needs no picture; without B-frames the samples before
   the last IDR picture at or before the first sample presented at that time (1 ms margin) are
   not decoded and a fresh decoder starts there. Allowed only when every skipped sample lies
   inside the file (a file cut short still stops where it did), carries no parameter set other
   than the track's own, and the restart sample's pictures are all IDR (H.264 type 5, HEVC
   19/20; not CRA). A picture from that IDR on references nothing before it, so it is the one
   sequential decoding gives. Samples decoded: H.264 101 -> 37-59, HEVC 102 -> 12-17.
2. HEVC: `rusty_h265` finishes a picture only when the next one starts, so each candidate IDR
   cost the following P picture. A sample at or after the wanted time is now flushed. When the
   caller then wants a time with an IDR ahead, the flushed decoder is replaced anyway; otherwise
   the stretch is decoded again from its IDR without passing its pictures on twice, and no
   flush is tried again until the next restart. Never at the 140-packet limit (nothing is
   flushed there, as before). Samples decoded: HEVC 6-11.
3. The scaler's horizontal pass takes the filter window as one slice and the vertical pass sums
   a whole row per tap. Integer sums (wrapping where the x86 rounding wraps), so the order does
   not change them. 6 scalings: 6.1 -> 4.0 ms (H.264), 20 -> 11 ms (HEVC).

### Tried and not applied

| what | measured | why not |
|---|---|---|
| decoder SIMD features (`simd`, `asm`) | decoding 1.9x faster (above); 85/85 byte equal, 5,004/5,004 and 1,265/1,265 frames equal to FFmpeg, 40/40 failure cases, aarch64 `cargo check` passes | after 1-3 the thumbnail is already as fast as alpha.3 (H.264) or 1.8-3.1x faster (HEVC); the gain left is 15-25 ms per segment. It would add `rusty_h264-accel` (161 `unsafe fn`, 75 `unsafe` blocks by its own `UNSAFE.md`) and `rusty_h265-accel` with `simd` (761 `unsafe fn`/blocks counted in its source) on attacker-controlled bitstreams, with bounds proved by their dispatchers, not here; the NEON paths could not be run here (no qemu-aarch64), and the AVX2/SSE4.1 choice differs per machine |
| `codegen-units = 1` for the decoder crates only | no change (within run ranges) | no effect |
| `codegen-units = 1` for the whole binary | no change in speed; stripped binary 20.8 -> 16.1 MB | no thumbnail effect; the size change concerns the whole binary and belongs to its own change with a full regression |
| `lto = "fat"` + `codegen-units = 1` | HEVC -9% (238 -> 217 ms, 314 -> 283 ms), H.264 -1 to -2% (in the range); stripped 20.8 -> 15.3 MB; release build 48 s -> 2 min 4 s (the latter partly concurrent with another build) | as above; worth its own evaluation for the image size and CI time |
| decoder threads | not built (see above) | |
| scaling the first picture lazily (it is used only when no candidate exists) | not built | about 1 ms (H.264) / 2 ms (HEVC), under 2% |

### Correctness

| check | result |
|---|---|
| 85 thumbnails (74 qcamera of 13 + 14 + 47 segments, 11 HEVC fcamera samples) vs the alpha.4 release binary | exit, stdout (second, score), `thumbnail.json`, JPEG size and JPEG bytes 85/85 equal; the five scaled candidates and the levelled picture before JPEG (measurement build) 85/85 equal; every picture the new worker outputs equals the alpha.4 picture of the same time (4,052 output pictures; alpha.4 output 8,622) |
| 36 videos (13 converted qcamera, 6 + 6 qcamera and 6 + 5 fcamera samples), 140-packet window | 5,004/5,004 output pictures have the MD5 of `ffmpeg -threads 1` (139 per video: the product path withholds the 140th sample's picture, as before; the earlier count of 5,040 included it from a separate harness) |
| the same 36, pictures a thumbnail run outputs | 1,265/1,265 equal to FFmpeg's. Pictures decoded only to be skipped or flushed away are not output and not compared |
| 40 failure cases (`integration/thumbnail_faults.py`, fixed seeds), `prlimit --as=2147483648` | 40/40 same outcome and same result and JPEG as alpha.4, 18 thumbnails made by each, panics 0, killed / deadline 0, files left after a failure 0; largest peak RSS 121.0 MiB (alpha.4) / 121.7 MiB (new), the 4096x2160 HEVC case |
| unit test | `skipping_to_an_idr_picture_gives_the_pictures_of_decoding_every_sample` (H.264 GOP 15, HEVC GOP 20, wanted times between IDR pictures, on one, at the window's end) |

The 40 cases were rebuilt from the list above because the first scripts were removed; the
interlaced H.264 case is now encoded at 528x336 (x264 needs a height divisible by 4) and fails
in both binaries at the MP4 reader, not at the decoder. The JPEG encoder was not changed.

### After

Worker spawn to exit, median (range) of 7 alternating rounds after one warmup, load 2.29 -> 1.55:

| input | alpha.3 (FFmpeg) | alpha.4 | new | CPU alpha.3 / alpha.4 / new | peak RSS alpha.3 / alpha.4 / new |
|---|---|---|---|---|---|
| 13-1 qcamera | 50.2 (48.0-51.4) | 106.3 (103.3-108.0) | 43.2 (42.6-44.1) | 68 / 104 / 42 ms | 47.4 / 14.4 / 14.4 MiB |
| 14-0 qcamera | 47.6 (46.7-53.1) | 100.5 (99.0-101.8) | 56.6 (55.5-58.1) | 64 / 98 / 55 ms | 47.2 / 14.4 / 14.4 MiB |
| 47-9 qcamera | 48.1 (46.9-49.4) | 99.9 (98.8-102.7) | 59.0 (58.4-61.1) | 65 / 97 / 58 ms | 47.1 / 14.4 / 14.4 MiB |
| 14-0 fcamera | 499.5 (495.6-504.5) | 1,857.6 (1,843-1,868) | 161.9 (159.4-166.5) | 644 / 1,815 / 158 ms | 62.9 / 20.6 / 17.6 MiB |
| 47-6 fcamera | 517.9 (510.2-522.7) | 1,802.9 (1,768-1,823) | 231.9 (225.5-234.9) | 685 / 1,756 / 227 ms | 62.7 / 20.6 / 21.7 MiB |
| 14-8 fcamera | 459.4 (454.9-477.3) | 1,988.7 (1,966-2,048) | 254.1 (251.8-263.0) | 648 / 1,940 / 248 ms | 62.7 / 20.4 / 22.7 MiB |

(Peak RSS here is `wait4` of the worker alone; it is lower than the 37 MiB of the first table,
whose method is not recorded, so the two are not compared.)

After, by stage (same build as before): H.264 decoding 36-53 ms (37-59 samples), scaling 4.0 ms,
the rest about 3.5 ms; HEVC decoding about 150-245 ms (6-11 samples, flushes included), scaling
11 ms, the rest about 4 ms.

H.264, the thumbnail every segment gets, takes 0.86 to 1.23 times alpha.3's time (one input
faster, two 9-11 ms slower), single-threaded, with 11-38% less CPU time than alpha.3's two
threads. The HEVC fallback is 1.8 to 3.1 times faster than alpha.3. Release binary 27,377,048 bytes
(stripped 20,875,688), +37,544 (+64,544 stripped) against alpha.4.
Whole conversion of 3 original segments (warmup 1 + 5 alternating rounds): 7.69 s -> 7.53 s in
total (medians; the per-segment ranges overlap). amd64 image 120.74 MB unpacked, 46.11 MB gzip
(alpha.4: 120.7 / 46.1 MB).

### What limits it now

- H.264: scalar decoding of the 37-59 pictures from the IDR before each candidate (deblocking and
  interpolation first). How far a candidate is from the IDR before it decides the time: 14-0 and
  47-9 need more pictures than 13-1.
- HEVC: decoding the 6-11 pictures, about 60% of it IDR pictures (inverse transform, CABAC).
- SIMD would roughly halve both (measured above); not applied for the reasons above.

### Not verified

- aarch64: `cargo check --target aarch64-unknown-linux-gnu` of the thumbnail modules with both
  decoders (default and SIMD builds) passes; the whole crate cannot be checked here (no aarch64
  C compiler for `ring` and `zstd-sys`). No aarch64 run: qemu-aarch64 is not installed. All code
  added here is safe, integer and target independent, so no `scripts/container_smoke.py` check
  was added; an aarch64 run of the decoders remains for the CI container job.
- A SIGKILL of the `__thumbnail` worker itself was not run separately; the worker start, its
  limit, deadline and `PR_SET_PDEATHSIG` (`main.rs`, `media.rs`, `pure_remux.rs`) are unchanged,
  and the existing worker-kill tests pass.
- Streams with an IDR picture farther than 1 s apart, CRA-only HEVC, or in-band parameter sets
  that differ from the track's decode sequentially as before; only the unit test and the corpus
  cover the restart.
