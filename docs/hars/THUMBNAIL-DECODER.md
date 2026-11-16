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
- `tests/conversion_keep_previous.rs`
  `a_resource_failure_keeps_the_published_result_and_a_later_conversion_completes` failed in the
  full run and in 1 of 2 reruns of that file (`409` instead of `206` on a range request for the
  front video right after the record is ready) and passed in the other. Whether it is
  intermittent on `d846fc6` as well, or made more likely by the slower thumbnail, is not
  established.
- `media::thumbnail` still calls `validate_upload` (`ffprobe`) before the worker; step 3 replaces
  it, after which the thumbnail path runs without any FFmpeg program or library.
- The production image still installs the FFmpeg runtime (step 3), so the image is not smaller
  yet; this step adds 1,471,392 bytes to the stripped binary and drops its `libswscale.so.7` link.
