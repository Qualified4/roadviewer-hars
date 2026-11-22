# 영상 형식 지원 범위 (2.0.0-alpha.4)

이 문서에 기록한 이식용 비교·측정 도구는 [정리 전 Git 트리](https://github.com/Qualified4/roadviewer-hars/tree/284efa5b0932c918f8f52bee616f216650b23c52/integration)에 있다.

<!-- toc -->
**목차**

- [결론](#결론)
- [openpilot이 기록하는 영상 형식](#openpilot이-기록하는-영상-형식)
  - [기록하지 않는 인코더(제외)](#기록하지-않는-인코더제외)
  - [근거 소스와 upstream 대조](#근거-소스와-upstream-대조)
- [HARS에서의 검증](#hars에서의-검증)
  - [실주행 코퍼스가 덮는 행](#실주행-코퍼스가-덮는-행)
  - [합성 입력으로 확인한 행](#합성-입력으로-확인한-행)
- [지원하지 않는 형식과 손상 입력](#지원하지-않는-형식과-손상-입력)
  - [오류 표시가 모호한 경우](#오류-표시가-모호한-경우)
- [alpha.3 대비 의도적인 호환성 변경](#alpha3-대비-의도적인-호환성-변경)
- [썸네일 실패 시 동작](#썸네일-실패-시-동작)
- [운영자가 알아야 할 기존 동작](#운영자가-알아야-할-기존-동작)
- [미검증](#미검증)
- [재현](#재현)
<!-- /toc -->

## 결론

- comma 장치(tici·tizi·mici)의 loggerd가 기록하는 형식(`fcamera.hevc`·`ecamera.hevc` HEVC Main 8비트 4:2:0, 1928×1208 또는 1344×760, B-frame 없음 / `qcamera.ts` H.264 High 526×330, B-frame 없음, 오디오 트랙이 있을 수 있음)은 alpha.4에서 **하나도 제외되지 않았다**. 업로드 검사·변환·Range 재생·썸네일·원본 형식 내보내기를 실제 실행 파일(FFmpeg 없는 PATH)로 모두 확인했다. 1928×1208 HEVC와 오디오가 든 `qcamera.ts`, raw `ecamera.hevc`는 코퍼스에 없어 합성·조립 입력으로 확인했다.
- 제외되는 것은 **PC·시뮬레이터 경로**(장치가 아닌 openpilot)의 출력뿐이다: `fcamera.hevc` 이름의 FFVHUFF/Matroska(업로드 거부)와 libx264 기본값 `qcamera.ts`(B-frame, 변환 거부). HARS는 장치 업로드를 받는 서버이고 Python 판도 앞의 것은 받지 않았다(HEVC가 아님). 뒤의 것은 alpha.3에서 FFmpeg 대체로 변환되던 입력이다.
- `dcamera.hevc`(운전자 카메라)는 형식과 상관없이 Python 때부터 받지 않는다(업로드 이름 단계에서 `invalid_file_path`). 형식 축소가 아니다.
- 지원하지 않는 입력과 손상 입력은 기록을 `error`로 만들지 않는다. 그 카메라만 빠지고 경고가 붙으며, 업로드 원본은 바이트 그대로 남고, 이미 게시된 결과가 있으면 그 카메라의 MP4를 그대로 이어받는다(12사례 실측). 서버는 한 번도 죽지 않았다.
- 썸네일을 만들 수 없는 카메라(예: HEVC Main 10만 있는 구간)는 썸네일만 없다. 기록은 경고 없이 `ready`이고 재생·로그·내보내기가 모두 정상이다(실측 + 자동 시험 `tests/thumbnail_isolation.rs`).

## openpilot이 기록하는 영상 형식

출처: 이 저장소 운영자가 쓰는 openpilot fork 사본(`ccnc-hda1` 브랜치, `5c2a8c7f6e14dac82678a390e408ef7a9c73eefa`, 2026-11-15; 작업 트리에 문서만 수정됨). 경로는 그 사본의 `openpilot/` 아래다. upstream(`commaai/master`, `070de2aef`, 2026-11-03)과의 차이는 [아래](#근거-소스와-upstream-대조)에 있다.

| 파일 | 장치·센서 | 코덱·profile·level | 해상도 | chroma·bit depth | 프레임 구조 | entropy | slice/tile | 컨테이너 | fps |
|---|---|---|---|---|---|---|---|---|---|
| `fcamera.hevc` (road) | AR0231(tici) 또는 OX03C10 | HEVC Main, level 5 (V4L2 HIGH_TIER_LEVEL_5, 실측 tier 0) | 1928×1208 | 4:2:0 8비트 (NV12 입력) | I(IDR)+P, B 0, GOP 30, IDR 주기 1, 10 Mbit/s VBR_CFR | CABAC(HEVC 고정) | 지정 없음(장치 기본값). 1344 폭 실측: tile 2열, picture당 slice segment 2 | raw Annex B. VPS/SPS/PPS가 IRAP마다 앞에 옴(실측 60/60) | 20 |
| `fcamera.hevc` (road) | OS04C10(mici, 일부 tizi) | 같음 | 1344×760 (2688×1520 sensor, out_scale 2) | 같음 | 같음, GOP 20, 5 Mbit/s | 같음 | 위 실측 | 같음 | 20 |
| `ecamera.hevc` (wide) | 위 센서 중 하나 | `fcamera`와 같은 설정(`MainEncoderSettings`) | 1928×1208 또는 1344×760 | 같음 | 같음 | 같음 | 같음 | 같음 | 20 |
| `dcamera.hevc` (driver) | 위 센서 중 하나 | 같은 설정 | 같음 | 같음 | 같음 | 같음 | 같음 | 같음 | 20 |
| `qcamera.ts` | road 카메라를 축소 | H.264 High, level UNKNOWN(장치 결정, 실측 2.1) | 526×330 (고정, 실측 coded 528×336 crop) | 4:2:0 8비트 | I+P, B 0, GOP 15, 256 kbit/s, POC type 2 | CABAC, cabac_init 0 | single slice(MULTI_SLICE_MODE 0) | MPEG-TS(libavformat), PTS=DTS. `RecordAudio`면 AAC-LC mono 32 kbit/s 트랙 추가(16 kHz) | 20 |
| PC: `fcamera`/`ecamera`/`dcamera.hevc` | `Hardware::PC()`(시뮬레이터·PC) | FFVHUFF(무손실, `BIG_BOX_LOSSLESS`) | 카메라 입력 그대로 | 4:2:0 8비트 | 모든 프레임 intra | — | — | **Matroska**(이름만 `.hevc`) | 20 |
| PC: `qcamera.ts` | PC | libavcodec 기본값의 libx264 H.264(옵션 지정 없음, x264 기본은 B-frame 3·keyint 250) | 526×330 | 4:2:0 8비트 | B-frame 있을 수 있음(합성 재현에서 `has_b_frames` 2) | CABAC | x264 기본 | MPEG-TS, PTS=DTS(B-frame이면 표시 순서와 어긋남) | 20 |

각 카메라 파일의 기록 여부는 fork의 매개변수가 정한다: `RecordRoadCam > 0`이면 `fcamera`, `> 1`이면 `ecamera`, `RecordFront`면 `dcamera`(upstream은 앞의 둘을 항상 기록). `qcamera.ts`는 항상 기록된다.

### 기록하지 않는 인코더(제외)

| 인코더 | 제외 사유 |
|---|---|
| `livestream*EncodeData`(road·wide·driver, `--stream`) | `record = false`, 파일 이름 없음(`loggerd.h` 114–134). WebRTC 송출 전용. fork는 road 송출을 964×604 Baseline CAVLC multi-slice로 바꾼다(`v4l_encoder.cc` 151–155, 259–288) |
| Carrot Vision road 인코더(`--carrot-vision-road`, fork) | 같은 `stream_road_encoder_info`, 기록 없음(`loggerd.h` 182–193, `encoderd.cc` 191–192) |
| `cluster_h264_encoder`(fork) | loggerd의 `EncoderInfo`에 없음, 파일 이름 없음 |
| `thumbnail`(JPEG) | rlog 안의 `thumbnail` 메시지(`encoderd.cc` 79–80, 136–138), 영상 파일 아님 |
| upstream `encoderd --clip`(`clip_encoder.cc`) | 명령으로 만드는 개별 클립. 구간 파일이 아니고 업로더 대상(`uploader.py` 88)도 아님. fork에는 없음 |

### 근거 소스와 upstream 대조

| 사실 | fork 위치 |
|---|---|
| 파일 이름·카메라별 설정·qcamera 526×330 | `system/loggerd/loggerd.h` 89–112, 136–144, 191 |
| 폭 ≤ 1344: 5 Mbit/s·GOP 20, 그 밖: 10 Mbit/s·GOP 30, `b_frames = 0`, qcamera 256 kbit/s·GOP 15 | `loggerd.h` 31–50 |
| 장치는 HEVC, PC는 `BIG_BOX_LOSSLESS` | `loggerd.h` 17 |
| P 프레임 수 = GOP − B − 1, B 0, header mode SEPARATE, VBR_CFR(fork의 `cbr` 기본 false), IDR period 1 | `system/loggerd/encoder/v4l_encoder.cc` 217–232 |
| HEVC Main, HIGH_TIER_LEVEL_5, VUI timing | `v4l_encoder.cc` 234–242 |
| H.264 High, level UNKNOWN, CABAC model 0, loop filter, single slice | `v4l_encoder.cc` 244–257 |
| codec config(header)는 따로 받아 keyframe 메시지에 붙임 | `v4l_encoder.cc` 118–129, `encoder/encoder.cc` |
| 첫 keyframe에서 header를 쓰고 이후 프레임을 그대로 씀, HEVC가 아니면 remux | `system/loggerd/loggerd.cc` 74–103, 138–142 |
| raw 파일 쓰기, TS/Matroska remux(PTS=DTS), AAC 32 kbit/s mono | `system/loggerd/video_writer.cc` 7–49, 52–89, 91–136 |
| 구간마다 인코더를 닫고 다시 엶(새 IDR로 시작) | `system/loggerd/encoderd.cc` 116–124 |
| PC 인코더: qcamera는 `AV_CODEC_ID_H264`(libx264 링크, 옵션 없음), 나머지 FFVHUFF | `system/loggerd/encoder/ffmpeg_encoder.cc` 48–61, `SConstruct` 63–65 |
| 센서 해상도 | `system/camerad/sensors/ar0231.cc` 25–26, `ox03c10.cc` 32–33, `os04c10.cc` 29–31; 출력 = frame / out_scale (`cameras/spectra.cc` 291–292); 센서는 카메라마다 실행 중 감지(`spectra.cc` 1062–1064) |
| 녹음 sample rate 16 kHz | `system/micd.py` 14 |

upstream 대조(`git diff` 대신 `git show commaai/master:<path>`와 줄 끝을 무시한 `diff`, 이미 fetch된 원격 추적 브랜치만): 기록 파일 이름·`MainEncoderSettings`·`QcamEncoderSettings`·`b_frames = 0`·HEVC/H.264 V4L2 제어값은 같다. 다른 점은 publish 이름(`narrowRoad`/`cabin`), 기록 여부 매개변수(upstream은 road·wide 항상 기록), livestream 설정(upstream 5 Mbit/s·GOP 5·level 3.1, 장치별 1152/1280×720), V4L2 poll 처리·bitrate 변경 같은 형식과 무관한 코드, upstream의 `--clip` 인코더와 AR0231 센서 파일 삭제다. 따라서 기록 형식 표는 upstream에도 그대로 맞는다(코드 대조, 실기기 미확인).

## HARS에서의 검증

방법: `format_support.py`. 사례마다 새 임시 데이터 폴더의 release 실행 파일(127.0.0.1:18700/18701, `HARS_INGRESS_ONLY=0`, 서버 PATH에 `nice`·`ionice`·셸 도구만, `ffmpeg`/`ffprobe` 없음 확인), 자동 변환, 원본 보존 끔. 브라우저 업로드 → 상태·경고·오류 → 카메라별 Range 재생(앞·끝 1 KiB = 다운로드) → 썸네일 → `/original/<kind>` → `data`·`telemetry`·`track`·목록·ZIP → 원본 SHA-256 → 서버 생존. 외부 상한 `systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=0`. 결과: [format-support-2026-11-17.json](measurements/format-support-2026-11-17.json).

"정규화 NAL 같음": 압축 그림 단위의 순서, parameter set 집합, 그 밖의 단위(AUD 제외) 순서가 업로드와 내보내기에서 같다(`media::Signature`와 같은 기준). TS는 호스트 `ffmpeg -c copy -f h264`로 영상 기본 스트림을 꺼내 비교했다.

### 실주행 코퍼스가 덮는 행

코퍼스(13·14·47구간)에서 카메라 종류별 앞·가운데·끝 3개씩(24파일)을 호스트 `ffprobe`와 `trace_headers`로 쟀다. 모든 파일이 같다:

| 코퍼스 파일 | 실측 | 표의 행 |
|---|---|---|
| 13구간 `fcamera.hevc`, 14·47구간 `fcamera.mp4`, 47구간 `ecamera.mp4` | HEVC Main, level 5.0, tier 0, 1344×760(coded 1344×768), yuv420p 8비트, `has_b_frames` 0, PTS=DTS, IRAP 간격 20, tile 2열×1행, picture당 slice segment 2, VUI timing 있음, NAL: IDR_W_RADL·TRAIL_R | OS04C10 1344×760 행(road·wide) |
| 13구간 `qcamera.ts`, 14·47구간 `qcamera.mp4` | H.264 High(100), level 2.1, 526×330, yuv420p 8비트, progressive(`frame_mbs_only` 1), `has_b_frames` 0, POC type 2, CABAC, scaling matrix 없음, picture당 slice 1, keyframe 간격 15(MP4), 오디오 트랙 없음 | qcamera 행(오디오 없음) |

전체 13구간 변환·Python 동등성은 [FFMPEG-REMOVAL.md](FFMPEG-REMOVAL.md#제품-경로ffmpeg-없는-path)에 있다.

코퍼스가 덮지 않는 행: 1928×1208 HEVC(AR0231·OX03C10), raw `ecamera.hevc`, `dcamera.hevc`, 오디오가 든 `qcamera.ts`, PC 경로 두 가지.

### 합성 입력으로 확인한 행

합성 입력은 개발용 호스트 FFmpeg 6.1.1로 만들었다(`libx265`/`libx264`/`aac`/`ffvhuff`). 소프트웨어 인코더 출력이므로 장치 출력과 같다고 주장하지 않는다.

| 행 | 입력 | 업로드 | 변환·재생 | 썸네일 | `/original/<kind>` |
|---|---|---|---|---|---|
| 1928×1208 `fcamera.hevc` | 합성: x265 Main, level 5.0, B 0, keyint 30 고정(scene cut 끔, closed GOP), 10 Mbit/s VBV, VUI timing, picture당 slice 2, IRAP마다 VPS/SPS/PPS, 1200프레임. 13구간 1번 rlog와 함께(qcamera 없음: 썸네일이 HEVC에서 나오게) | 수락 | `ready`, front, Range 206·바이트 같음 | 200 JPEG(HEVC에서) | 200, 정규화 NAL 같음, 그림 1200/1200 |
| 1928×1208 `ecamera.hevc` | 같은 합성 파일을 `ecamera.hevc`로 | 수락 | `ready`, wide, Range 206 | 200 JPEG | 200, 정규화 NAL 같음, 1200/1200 |
| 1344×760 raw `ecamera.hevc` | **장치 바이트**: 13구간 3번의 `fcamera.hevc`를 `ecamera.hevc`로(같은 인코더 설정) | 수락 | `ready`, wide, Range 206 | 200 JPEG | 200, NAL 단위까지 같음, 1200/1200 |
| `dcamera.hevc` | 장치 바이트를 `dcamera.hevc`로 | **거부** 400 `invalid_file_path`(manifest) | — | — | — |
| `qcamera.ts` + AAC | 13구간 2번 qcamera 영상을 그대로 두고 AAC-LC mono 16 kHz 32 kbit/s 트랙을 영상 시작 시각에 맞춰 추가(libavformat mpegts) | 수락 | `ready`, qcamera·front, Range 206 | 200 JPEG | 200, 영상 NAL 단위까지 같음, 1200/1200. **오디오는 없음**(바이트 다름, [아래](#운영자가-알아야-할-기존-동작)) |
| PC `qcamera.ts` | 13구간 3번 qcamera를 libx264 기본값으로 재인코딩(B-frame, `has_b_frames` 2) → mpegts | 수락(TS는 업로드 검사 없음) | qcamera **`unsupported`**, front만 `ready` + 경고 `qcamera 영상 준비 실패: unsupported: reading the camera original: unsupported by pure remux: H.264 picture reordering in TS` | 200(front에서) | 200, 업로드 바이트 그대로 |
| PC `fcamera.hevc`(FFVHUFF/Matroska) | 1344×760 60프레임 ffvhuff → matroska | **거부** 400 `invalid_file`(finish) | — | — | — |

재현하지 못한 것: 하드웨어 인코더의 tile 구성(x265는 tile을 만들지 않는다. 1344 폭의 tile 2열은 코퍼스로 확인했고 1928 폭의 tile 여부는 실측 자료가 없다), 장치의 rate control·양자화·SEI 구성, level 5 high tier 요청과 실제 tier의 관계(코퍼스는 tier 0). HARS의 리먹싱·역변환은 slice 데이터를 해석하지 않고, 썸네일 디코더의 tile 처리는 코퍼스로 확인했다([THUMBNAIL-DECODER.md](THUMBNAIL-DECODER.md)).

## 지원하지 않는 형식과 손상 입력

최종 코드(`a5b1ce0`)의 release 서버, 위와 같은 환경, 사례마다 1회. "이전 결과"는 같은 구간의 정상 파일로 먼저 게시한 뒤(원본 보존 켬) 그 카메라 원본만 거부 파일로 바꾸고 기록을 `queued`로 되돌려(변환 중 죽은 서버가 남기는 상태, 디스크 준비는 스크립트가 함) 서버를 다시 띄운 결과다. 각 입력의 분류 근거와 alpha.3·Python 비교는 [FFMPEG-REMOVAL.md의 거부 입력 표](FFMPEG-REMOVAL.md#순수-엔진이-거부하는-입력)에 있다.

모든 사례에서 기록은 `error`가 아니고 서버는 끝까지 돌았다. "원본"은 업로드한 파일과 저장된 파일의 SHA-256 비교다(원본 보존을 꺼도 거부된 카메라의 원본은 지우지 않는다. 게시된 다른 카메라의 원본은 설정대로 지워진다).

| 사례 | 만든 방법 | 사용자에게 보이는 것 | 원본 | 이전 결과가 있을 때 |
|---|---|---|---|---|
| B-frame H.264 TS | qcamera를 libx264 `-bf 2`로 | `ready`(front만) + `qcamera 영상 준비 실패: unsupported: reading the camera original: unsupported by pure remux: H.264 picture reordering in TS` | 그대로 | `ready`, qcamera MP4 바이트 그대로, 경고 끝에 `(이전 변환 결과 유지)`, 두 카메라 Range 206 |
| interlaced H.264 TS | libx264 `interlaced=1:tff=1`(MBAFF, 526×332) | `ready`(front만) + `… unsupported by pure remux: interlaced H.264` | 그대로 | 같음 |
| B-frame HEVC raw | x265 `bframes=3` | `ready`(qcamera만) + `fcamera.hevc 영상 준비 실패: unsupported: … HEVC B frame mapping is not supported` | 그대로 | 같음 |
| parameter set이 중간에 바뀌는 HEVC | 장치 HEVC 뒤에 176×96 HEVC 연결 | `ready`(qcamera만) + `… unsupported: … HEVC parameter sets change mid-stream` | 그대로 | 같음 |
| 잘린 TS(영상 payload 중간) | 영상 패킷 60 % 지점 + 100 B에서 자름 | `ready`(front만) + `qcamera 영상 준비 실패: native media validation failed (exit status: 1)` | 그대로 | 같음 |
| 잘린 HEVC | 60 % + 37 B에서 자름(NAL 중간) | `ready`(qcamera만) + `… invalid: reading the camera original: HEVC frame count differs from logged clock` | 그대로 | 같음 |
| 0바이트 파일 | `fcamera.hevc` 0 B | 업로드 거부 400 `{"error":"invalid_file"}`(manifest 단계) | 저장 안 됨 | 해당 없음(기록이 생기지 않음) |
| 무작위 바이트 | 1 MiB(시드 20261117) | 업로드 거부 400 `invalid_file`(finish의 미디어 검사) | 저장 안 됨 | 해당 없음 |
| PAT/PMT 손상 TS | PID 0과 PMT PID 패킷마다 payload 6바이트 XOR(CRC 실패) | `ready`(front만) + `native media validation failed (exit status: 1)` | 그대로 | 같음 |
| B-frame MP4(edit list) | 14구간 qcamera.mp4를 libx264 `-bf 2` MP4로 | 업로드 수락, `ready`(front만) + `qcamera 영상 준비 실패: native media validation failed (exit status: 1)`. **원본 형식 ZIP(`format=original`)이 400 `media_export_failed`**(qcamera.ts 역변환 불가, 같은 ZIP의 rlog·front도 받을 수 없음) | 그대로(`camera.mp4`로 저장) | 해당 없음: MP4 입력은 올린 MP4를 그대로 재생하므로 이어받을 변환 결과가 없다 |
| moov가 없는 MP4 | `moov` box 형식을 `free`로 | 업로드 거부 400 `invalid_file`(finish) | 저장 안 됨 | 해당 없음 |
| 10비트 HEVC(Main 10) raw | x265 `main10`, yuv420p10le | **거부 아님**: 업로드 수락, 두 카메라 `ready`, 경고 없음, front Range 206, `/original/fcamera.hevc` 정규화 NAL 같음(1200/1200). 썸네일은 qcamera에서 나옴. front만 있으면 [썸네일 없음](#썸네일-실패-시-동작) | 게시 뒤 설정대로 | 해당 없음(거부되지 않음) |

`data`·`telemetry`·`track` API는 업로드가 수락된 모든 사례에서 200, 목록 응답에 기록이 있었다. 원본 형식 ZIP은 B-frame MP4 사례만 400이었다.

### 오류 표시가 모호한 경우

1. **원본 검사 worker의 실패**: 잘린 TS, PAT/PMT 손상 TS, B-frame MP4의 경고는 `qcamera 영상 준비 실패: native media validation failed (exit status: 1)`뿐이다. 무엇이 잘못됐는지(패킷 손실, PSI CRC 등) 알 수 없다. `__inspect` worker의 stderr를 버리기 때문이다(`media::run`, alpha.3과 같음). 엔진 worker가 거부한 경우(B-frame, interlaced, parameter set 변경, 프레임 수 불일치)는 이유가 경고에 나온다. 이 측정 뒤에 문구를 `지원하지 않는 형식이거나 손상된 영상입니다 (media check exit status: 1)`로 바꿨다(신호로 끝난 worker는 `영상 검사 프로세스가 중단되었습니다`). 표와 측정 JSON의 문구는 측정 시점(`a5b1ce0`)의 것이다.
2. **업로드 거부**: `invalid_file`(내용 검사 실패)과 `invalid_file_path`(받지 않는 파일 이름)는 `message` 없이 코드만 온다(Python과 같은 형식, `error.rs`). 0바이트·무작위 바이트·FFVHUFF·moov 없는 MP4가 모두 같은 `invalid_file`이다.
3. B-frame MP4 기록의 원본 형식 ZIP은 `media_export_failed`로 통째로 실패한다. 어느 카메라 때문인지 응답에 없다.
4. 위 셋은 코드 값과 응답 형식을 바꾸지 않고 문구만 고치기에는 worker stderr를 경로 없이 정리하는 작업(1)이나 응답에 새 필드를 넣는 일(2, 3)이 필요해 이번에 고치지 않았다.

## alpha.3 대비 의도적인 호환성 변경

전체 표는 [STATUS.md](STATUS.md#alpha4에서-지원-범위가-달라진-입력)와 [FFMPEG-REMOVAL.md](FFMPEG-REMOVAL.md#순수-엔진이-거부하는-입력)에 있다. 요약:

- 좁아진 것(장치 출력에는 없음): B-frame·interlaced·scaling matrix·PES당 그림 여러 개인 TS, PSI 손상 TS, media를 미루는 edit list·길이 필드가 4가 아닌·sample entry가 여러 개인 MP4의 변환, hev1·다중 트랙 MP4의 원본 형식 내보내기, H.264 4:2:2·4:4:4·10비트·interlaced와 HEVC Main 10·4:4:4의 썸네일, 저공간. 이 문서의 PC 경로 `qcamera.ts`는 첫 줄(B-frame TS)에 해당한다.
- 넓어진 것: scrambling 표시가 선 TS, 비영상 패킷에서 잘린 TS, moov가 mdat 뒤인 MP4, 오디오 트랙이 더 있는 MP4, hev1/avc3 MP4 입력의 변환.
- 그대로인 것: comma 장치 출력 전부(이 문서의 표), 오디오가 든 `qcamera.ts`(영상만 변환), `dcamera.hevc` 거부.

## 썸네일 실패 시 동작

썸네일은 변환된 MP4에서 qcamera → front → wide 순서로 처음 성공한 카메라로 만든다(`convert::prepare`). 모두 실패하면 `prepared/thumbnail.failed`를 남길 뿐 기록 상태·경고·오류에 영향이 없다.

실측(13구간 12번 rlog + 합성 HEVC Main 10 `fcamera.hevc` 1200프레임만, qcamera 없음, release 서버 1회): 업로드 수락, `ready`, 경고·오류 없음, front Range 206(앞·끝 1 KiB = 다운로드), `data`·`telemetry`·`track` 200, 목록 200(행에 `thumbnail` 키 없음, 상태 `ready`), 원본 ZIP 200(2파일), `/original/fcamera.hevc` 200(정규화 NAL 같음, 그림 1200/1200), `prepared/thumbnail.failed` 있음, **썸네일 API 404 `{"error":"not_found"}`**. 웹 UI 목록은 `thumbnail`이 없는 행을 썸네일 없이 그린다(응답 형식은 썸네일이 없는 다른 기록과 같다).

자동 시험: `tests/thumbnail_isolation.rs` `a_camera_without_a_thumbnail_leaves_the_record_ready_and_playable`(실제 실행 파일, 합성 Main 10 HEVC 20프레임 + 합성 rlog): `ready`, 오류·경고 없음, front 20프레임, `thumbnail.failed` 있고 `thumbnail.jpg` 없음, 목록의 `thumbnail`이 true 아님, 썸네일 API 404, Range 206 바이트 같음, `data`·`telemetry`·`track` 200, `/original/fcamera.hevc` = 원본 바이트. 20회 연속 통과.

변환 자체가 거부되는 형식(위 표)은 썸네일의 문제가 아니라 그 카메라의 변환 실패다.

## 운영자가 알아야 할 기존 동작

- **녹음(`RecordAudio`)이 든 `qcamera.ts`의 오디오는 MP4에 들어가지 않는다.** 원본 보존을 끄면 원본 TS가 지워지므로 오디오가 사라지고, `/original/qcamera.ts`는 영상만 든 TS다. Python도 영상 스트림만 옮겼다(`Python 1.x의 decoder.py` 336). alpha.4의 변경이 아니다. 오디오가 필요하면 원본 보존을 켠다.
- `dcamera.hevc`는 받지 않는다(Python과 같음).

## 미검증

- 실제 1928×1208 장치 HEVC(AR0231·OX03C10)와 실제 `RecordAudio` 녹음 파일: 코퍼스에 없다. 합성으로만 확인했다.
- 1928 폭 장치 HEVC의 tile·slice 구성, 장치별(tici/tizi/mici) 센서 조합의 실측.
- upstream openpilot 대조는 이미 fetch된 `commaai/master`(2026-11-03)까지다. 그 뒤 변경은 모른다.
- PC 경로 `qcamera.ts`의 실제 x264 설정은 코드(옵션 없음)와 x264 기본값으로 추정했다. PC openpilot을 실행해 확인하지 않았다.
- `MAIN_ENCODE_TYPE` 등 형식 설정을 바꾸는 사용자 수정 fork는 대상 밖이다.

## 재현

```sh
cargo test --locked --test thumbnail_isolation
```

입력 파일 이름은 `format_support.py`의 `CASES`, 만드는 명령은 `format_support_inputs.sh`에 있다.
