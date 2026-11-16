# 순수 Rust 리먹싱 엔진 (기본 엔진)

<!-- toc -->
**목차**

- [결론](#결론)
- [구성과 엔진 선택](#구성과-엔진-선택)
- [자동 대체 규칙](#자동-대체-규칙)
- [격리](#격리)
- [출력 검증](#출력-검증)
- [컨테이너 차이 처리](#컨테이너-차이-처리)
- [검증 결과](#검증-결과)
- [브라우저 재생](#브라우저-재생)
- [성능](#성능)
- [의존성](#의존성)
- [남은 차이와 미검증](#남은-차이와-미검증)
- [재현](#재현)
<!-- /toc -->

## 결론

원시 카메라 원본(`qcamera.ts` H.264 MPEG-TS, `fcamera.hevc`/`ecamera.hevc` raw HEVC)을 MP4로 옮기는 기본 엔진은 순수 Rust 엔진(`src/pure_remux.rs`, `src/pure_remux/`)이다. 기존 FFmpeg 엔진(`src/native_video.rs`)은 자동 대체 경로로 남는다. 실주행 13구간 26영상에서 순수 엔진 출력은 Python 출력과 패킷 PTS/DTS·키프레임·sample payload·decoder configuration 바이트·디코딩 프레임이 모두 같고, 다른 것은 정해진 마지막 프레임 길이뿐이다([TERMINAL-DURATION.md](TERMINAL-DURATION.md)).

## 구성과 엔진 선택

- 내부 설정 `HARS_REMUX_ENGINE=pure|ffmpeg`(기본 `pure`). 다른 값이면 서버가 시작하지 않는다. 공개 API·메타 JSON·파일명은 엔진과 무관하다.
- 진입점은 그대로 `media::remux_probed`(변환), `media::remux`(CLI)다. 업로드 자동 변환, 수동 재변환, 아카이브 입력(NAS 제자리 읽기, Drive 임시 사본)이 모두 이 경로를 지난다.
- 어떤 엔진이 썼는지는 구조화 로그(`remux_engine`, `remux_fallback`)에만 남는다. 서버는 변환 자식 프로세스의 이 이벤트를 레코드 id와 함께 다시 기록한다(`media::relay_remux_event`). 프로세스 단위 카운터는 `media::remux_counts()`다.
- 기존 MP4(`camera.mp4`, `fcamera.mp4`, `ecamera.mp4`) 재사용, 역변환(MP4 → 원본 형식), 썸네일(FFmpeg 디코딩), 원본 다운로드는 바뀌지 않았다.

## 자동 대체 규칙

1. `media`가 입력을 검사하고(TS는 FFmpeg 패킷 검사, 기존과 같음) 마지막 sample 길이를 한 번 정한다.
2. 순수 엔진을 실행한다. 다음이면 그 출력을 지우고 같은 입력을 FFmpeg 엔진으로 처리한다.
   - 순수 엔진이 범위 밖 입력으로 거부(`unsupported:`): TS 픽처 재정렬(B-frame), 첫 PES가 가장 이른 PTS가 아님, PES PTS 없음, 연속성 오류, 스크램블·transport error 표시, 한 PES에 여러 access unit, interlaced/field 부호화(`frame_mbs_only_flag=0`), H.264 scaling matrix, HEVC B-frame(`sps_max_num_reorder_pics>0`), HEVC 시각이 0에서 시작하지 않음, 3 GiB를 넘는 출력.
   - 순수 엔진 오류(`failed:`)나 worker 실패·시간 초과·출력 크기 초과·패닉(`worker:`).
   - 출력 검증 실패(`verification:`). 검증 항목은 [출력 검증](#출력-검증)에 있다.
3. FFmpeg 엔진도 실패하면 기존과 같은 실패 처리다(경고, 해당 MP4 없음).

`times[0] ≠ 0`: 제품 경로에서는 일어나지 않는다. `convert.rs`가 raw HEVC 시각을 첫 프레임 기준으로, TS 시각을 최소 PTS 기준으로 만들기 때문이다. CLI 등에서 들어오면 순수 엔진이 거부하고 FFmpeg 엔진이 edit list로 처리한다(기존 동작).

## 격리

서버·CLI에서는 순수 엔진을 별도 worker 프로세스(`roadviewer-hars __pure-remux SOURCE JOB_JSON OUTPUT`)로 실행한다. FFmpeg worker와 같은 `media::run`을 쓰므로 동시 실행 2개, 180초 시간 상한, 출력 64 MiB 상한, `kill_on_drop`이 같다. 패닉은 worker 종료로 끝나고 대체 경로로 넘어간다. 라이브러리 내장(시험) 경로는 `spawn_blocking` + `catch_unwind`로 같은 결과를 낸다. 입력 한도는 FFmpeg 엔진과 같다: NAL 32 MiB, sample 32 MiB, 패킷 262,144개, video stream 1개, 코덱·해상도 검사(최대 4096×2160, 짝수).

## 출력 검증

엔진과 무관한 같은 검사(`media::verify_and_place`)를 통과해야 원자적으로 rename한다.

- 순수 엔진 출력은 `pure_remux::readback`이 다시 읽는다. 이 판독기는 box 트리와 sample table(stsz/stts/ctts/stsc/stco·co64/stss, avcC/hvcC)을 직접 해석하며, 쓰기 코드나 `shiguredo_mp4`와 코드를 공유하지 않는다. 입력 서명은 `media::NalHasher`(쓰기 쪽 Annex B 분할과 다른 구현)로 계산한다.
- FFmpeg 엔진 출력은 기존처럼 FFmpeg inspect로 읽는다.
- 공통: 코덱·해상도, 프레임 수와 로그 시각 ±2 ms, 정규화 VCL SHA + decoder header set이 입력과 같음, TS는 패킷별 PTS/DTS가 입력 PES 시각 − 최소 PTS와 정확히 같음, 마지막 sample 길이가 정한 값과 같음, time base(TS 1/90000, HEVC 1/1000000), moov가 mdat 앞, avcC/hvcC 존재.
- 순수 엔진 추가: top-level이 정확히 `ftyp, moov, mdat`(패딩 없음), mdhd duration = stts 합.
- 판독기는 시험에서 FFmpeg inspect·ffprobe와 같은 값을 내는지 교차 확인한다(`tests/media.rs`의 필드 비교, `tests/pure_remux.rs`).

## 컨테이너 차이 처리

이전 실험 문서의 차이 1~8 처리:

| # | 차이 | 처리 |
|---|---|---|
| 1 | TS 마지막 sample duration | 두 엔진 모두 [TERMINAL-DURATION.md](TERMINAL-DURATION.md)의 규칙. Python과의 차이는 비교 도구가 모델링한다 |
| 2 | hvcC PPS 끝 0 1바이트 | **수정.** 원인: libavformat은 `extract_extradata`가 첫 패킷의 parameter set을 `00 00 01` 직전까지(`ff_h2645_extract_rbsp`의 `raw_size`) 잘라 3바이트 start code로 이어 붙이고, hvcC/avcC 작성기(`ff_nal_parse_units`)가 다시 나누면서 마지막 NAL만 끝 0을 모두 유지한다(FFmpeg n8.0 `libavcodec/bsf/extract_extradata.c`, `libavcodec/h2645_parse.c`, `libavformat/nal.c`). 같은 규칙(`annexb::configuration_sets`)을 구현해 13/13 HEVC, 13/13 H.264에서 Python과 바이트가 같다 |
| 3 | edit list 없음 | 유지. 첫 PTS가 0이라 media time 0, 전체 길이 elst와 의미가 같다. PyAV stream duration, PTS 목록, 브라우저 `video.duration`이 FFmpeg 엔진 출력과 같은지 확인했다([검증 결과](#검증-결과), [브라우저 재생](#브라우저-재생)) |
| 4 | movie timescale(1000 대 media timescale) | 유지. HARS·Python이 읽는 값은 stream(mdhd) duration이다. mvhd/tkhd는 같은 길이를 media timescale로 정확히 적는다(FFmpeg는 ms로 반올림) |
| 5 | 단일 chunk | 유지. 트랙이 하나라 interleave 영향이 없다. 시킹은 브라우저에서 확인 |
| 6 | `btrt`/`udta` 없음 | 유지. 디코딩·재생·duration에 쓰이지 않는다 |
| 7 | 약 46 KB `free` 예약 | **수정.** 입력을 두 번 읽는다. 첫 번째에 모든 sample 크기·길이·키프레임을 정하고, 같은 표로 moov 크기를 미리 계산해 정확히 그만큼 예약한다. 두 번째 읽기에서 sample을 쓰며 크기가 첫 번째와 같은지 확인한다(입력이 바뀌면 오류). 출력은 `ftyp, moov, mdat`이다. Python 출력에는 FFmpeg의 8바이트 `free`가 있다 |
| 8 | 입력 계약 차이 | `times[0]≠0`·PTS 없는 PES·연속성 오류 등은 명시적 `unsupported`로 FFmpeg 대체([자동 대체 규칙](#자동-대체-규칙)) |

추가로 찾은 차이: raw HEVC 시각이 정확히 0.5 µs에 걸리는 프레임(13구간 15,600프레임 중 5개)에서 Python `round()`(짝수 쪽)와 Rust `.round()`(0에서 먼 쪽)가 1 µs 달랐다. 두 Rust 엔진 모두 `round_ties_even`으로 바꿔 Python과 같게 했다.

## 검증 결과

병합 커밋 `40854ee`(main `4cee231` 병합) 기준이다. 영상·경로·clock은 커밋하지 않았다.

- 전체 시험: `cargo fmt --check`, `cargo clippy --locked --all-targets -D warnings`, `cargo test --locked --all-targets` 250개 통과, 실패 0.
- 두 엔진 교차: 같은 13구간을 `HARS_REMUX_ENGINE=ffmpeg`로도 변환했다. replay JSON 13/13 동등, 26영상 모두 패킷 PTS/DTS·duration·키프레임·payload·extradata·stream duration이 순수 엔진 출력과 같다. 순수 기본 실행에서 대체는 0회였다.
- 실제 실행 파일 e2e(3번 구간, `integration/real_device_media.py`, 지정 loopback 포트): RV1 업로드 → 자동 변환 → ready → replay/telemetry API → MP4 다운로드·Range 206 → 원본 다운로드. 보존 모드는 원본 SHA 일치, 비보존 모드는 순수 엔진 MP4에서 역변환한 원본이 정규화 전체 NAL SHA·TS PTS(차이 0)로 원본과 같다. 서버 로그에 두 카메라 모두 `engine: pure`, `terminal_basis: NextFrame`, 대체 없음.

- 13구간 26영상(`integration/pure_remux_audit.py`, PyAV 16.1/FFmpeg 8.0.1, system ffmpeg 6.1.1 디코딩): qcamera 13·front 13 모두 패킷 1,200, PTS/DTS·키프레임 flag·sample payload SHA·마지막 외 duration·decoder configuration 바이트가 Python과 같음, 정규화 전체 NAL SHA = 원본, 전체 디코딩 1,200프레임·오류 줄 0·프레임 MD5 목록 = Python, stream duration = 각 쪽 모델 값. 키프레임 합계 TS 1,040, HEVC 780.
- 전체 replay JSON(`integration/real_conversion_compare.py`): 원본 13구간 13/13(모델링한 duration 파생 필드 외 차이 0, `drive_summary` 변화 없음), 14구간 14/14, 47구간 47/47.
- CI 합성 oracle `integration/pipeline_contract.py`: 통과(qcamera만 Python probe 값과 다른 것이 모델대로).
- 경계 시험: `tests/pure_remux.rs`, `tests/terminal_duration.rs`, `tests/pure_remux_faults.rs`, `tests/archive_pure_remux.rs`.

## 브라우저 재생

`integration/browser_video_check.py`로 Windows Microsoft Edge(`--headless=new`, 매번 새 임시 프로필, 사용자 브라우저·ego-browser 미사용)를 돌렸다. 영상은 WSL의 다중 스레드 Range 서버(127.0.0.1, 지정 포트)로 제공했다. 순수 엔진 26개와 FFmpeg 엔진(`HARS_REMUX_ENGINE=ffmpeg`) 26개를 각각 3회, 총 156회 확인했다.

- 오류(`error`, `MediaError`) 0. 중간(50 %)·말미(99.5 %) seek 후 readyState 최소 4. 재생 400 ms 후 진행, 일시정지 정상.
- 병합 커밋 `40854ee` 출력으로 다시 돌린 결과다(병합 전 출력으로 한 첫 실행도 같은 항목 모두 통과, `loadedmetadata` 최대 120 ms).
- `loadedmetadata`: 중앙값 4 ms, 최대 86 ms(순수 엔진 1회차 첫 영상, cold start). 15초 대기 초과 0회. 이전 실험의 5번 TS 대기 초과는 3회 반복에서 재현되지 않았다. 이번 서버는 요청마다 스레드를 쓰므로, 이전의 단일 스레드 시험 서버가 원인이었다는 추정과 맞는다(확정은 아님).
- `video.duration`: 두 엔진 26/26 같음, replay JSON의 `videos.*.duration`과 26/26 같음(최대 차이 4.4e-7초, float 표현 차이).
- 말미 동기: 마지막 0.3초를 재생했을 때 `ended` 시점의 `currentTime − duration` = 0(156/156). 영상은 다음 프레임 시각에서 끝나고 그 뒤로 늘지 않는다.
- `canPlayType`: hvc1은 `probably`. avc1 질의 문자열(`avc1.4d4028`)은 빈 값이었지만 재생은 모두 정상이어서, 질의 문자열이 이 영상의 profile과 맞지 않은 것으로 본다(재생 판단에 쓰지 않음).

## 성능

병합 커밋 `40854ee`의 release 빌드 하나를 `HARS_REMUX_ENGINE`만 바꿔 측정했다. WSL2 Ubuntu 24.04 x86_64, 16 logical CPU, 입력 NTFS(drvfs) 원본, 출력 ext4, warm cache. 측정 전 load average 0.9~1.3(다른 작업 없음, `uptime` 기록). 구현별 warmup 1회 + 순서를 회전·역순으로 바꾼 교차 5회. 숫자 집계: [리먹싱](measurements/pure-default-remux-2026-11-16.json), [전체 변환](measurements/pure-default-conversion-2026-11-16.json).

(a) 리먹싱 단독(`integration/benchmark_remux.py`, `remux` CLI 또는 Python PyAV 어댑터, 검증 포함), 13영상 합(영상별 중앙값의 합, 괄호는 회차별 합의 범위), 390표본 모두 검증 통과:

| | 경과(초) | CPU(초) | 평균 RSS 중앙값(KiB) | 최고 RSS(KiB) |
|---|---|---|---|---|
| HEVC Python | 6.642 (6.617–6.679) | 3.714 | 43,767 | 50,412 |
| HEVC FFmpeg 엔진 | 8.009 (7.947–8.018) | 4.899 | 81,583 | 96,312 |
| HEVC 순수 | 4.700 (4.613–4.721) | 2.575 | 71,936 | 102,768 |
| TS Python | 2.000 (1.979–2.034) | 1.410 | 31,644 | 50,232 |
| TS FFmpeg 엔진 | 2.149 (2.113–2.172) | 1.607 | 45,000 | 73,688 |
| TS 순수 | 1.809 (1.770–1.846) | 1.257 | 40,624 | 71,048 |

CLI에는 rlog가 없어 HARS 마지막 길이는 중앙값 규칙이다(작업량은 같다).

(b) 전체 변환(`integration/benchmark_conversion.py`, 로그 분석·리먹싱·검증·텔레메트리·썸네일), 대표 3구간(3, 7, 9번: 9번은 Python과 마지막 길이 차이가 가장 큼), 각 결과를 Python 전체 출력과 모델 포함 비교해 모두 통과:

| 3구간 중앙값 합 | 경과(초) | CPU(초) |
|---|---|---|
| Python | 11.439 | 10.014 |
| HARS FFmpeg 엔진 | 8.709 | 7.501 |
| HARS 순수(기본) | 7.883 | 6.885 |

순수 기본은 Python보다 경과 31 %, CPU 31 %, FFmpeg 엔진보다 경과 9.5 %, CPU 8.2 % 적다. 구간별 최고 RSS: Python 346,944~354,304 KiB, HARS FFmpeg 엔진 380,140~397,768 KiB, 순수 377,868~390,368 KiB(로그 분석 메모리가 대부분이라 엔진 차이가 작다). HARS 표본 CPU에서 영상 단계(`video_read`, 하한 표본값)는 순수 0.20초, FFmpeg 엔진 0.36~0.38초로 전체 CPU의 약 9 % 대 15 %다.

무엇이 수치를 제한하는가:

- 순수 HEVC: 입력 1회 읽기와 CPU가 반씩이다. 3번 HEVC 진단(벤치 표본 아님): drvfs 입력 경과 0.34~0.36초, ext4 사본 입력 0.19~0.20초, CPU 0.17~0.19초, 같은 파일 `cat` 0.18초. CPU는 SHA-256 두 번(입력 `NalHasher`, 출력 판독)과 NAL 탐색이다.
- 처음 구현(입력 2회 읽기)은 drvfs에서 경과 0.47~0.48초였다(첫 측정 HEVC 합 6.191초). 그래서 sample 바이트가 64 MiB 이하이면 첫 읽기에서 메모리에 두고 한 번만 읽게 바꿨다. 대가로 최고 RSS가 HEVC 약 37 MiB 늘었다(FFmpeg 엔진 최고 RSS와 같은 수준).
- 순수 TS: 입력이 2.3 MB로 작아 프로세스 기동이 대부분이다. 기본 경로도 입력 검사는 FFmpeg inspect worker(1회)를 쓰고, 순수 worker가 1개 더 뜬다. 입력을 FFmpeg로 독립 검사하는 것을 유지했기 때문이다(아래 검증 선택).
- 전체 변환: 로그 읽기/zstd(약 0.55초), 텔레메트리 생성(약 0.9초), 로그 분석(약 0.4초) CPU가 대부분이다. 엔진 교체의 효과는 전체 CPU의 약 8 %, 경과의 약 9.5 %다.

검증 선택: 기본 경로는 FFmpeg 출력 inspect를 쓰지 않고 독립 판독기로 출력을 검사한다. 입력 쪽 TS 검사(PES 시각·서명)는 FFmpeg를 계속 쓴다. 순수 엔진의 PES 해석이 잘못되면 FFmpeg 검사와 PTS/DTS·서명이 달라져 대체 경로로 간다. 이 검사를 순수 Rust로 바꾸면 TS 경로의 프로세스 하나를 줄일 수 있지만, 입력 해석과 검사가 같은 코드가 되므로 하지 않았다.

## 의존성

- `shiguredo_mp4 =2026.5.0`(Apache-2.0, 하위 의존성 0, `src/`의 unsafe 0)과 `mpeg2ts =0.6.1`(MIT, 하위 의존성 0, unsafe 0)이 기본 의존성이 되었다. Cargo feature `pure-remux`는 없앴다.
- `=` 고정 유지: `shiguredo_mp4`는 연 단위 버전이고 출력 바이트(moov 구성)가 판독 검사와 크기 계산에 직접 쓰인다. 올릴 때는 이 문서의 26영상 감사를 다시 돌린다.

## 남은 차이와 미검증

- Python과의 차이: 마지막 sample 길이와 그 파생 필드(규칙 문서의 표). 컨테이너 부가 박스(elst, btrt, udta, mvhd/tkhd timescale, chunk 구성, Python 쪽 8바이트 free).
- 실주행 B-frame TS, field 부호화, scaling matrix SPS는 코퍼스에 없어 합성 시험과 대체 경로로만 확인했다.
- aarch64(HAOS) 미검증. 규칙 계산은 정수 연산만 쓴다. HEVC 시각 변환 `round_ties_even(t × 1e6)`은 IEEE 754 곱셈 1회와 반올림이라 아키텍처·FMA와 무관하다(덧셈이 없어 융합 대상이 아니다).
- Safari/iOS, Firefox, Android WebView, HA Ingress 경유 재생은 미검증.

## 재현

```sh
cargo test --test pure_remux --test terminal_duration --test ts_duration --test pure_remux_faults --test archive_pure_remux
python integration/real_conversion_compare.py CORPUS BINARY ORACLE OUT --limit 13
python integration/pure_remux_audit.py CORPUS OUT ORACLE
python integration/pipeline_contract.py BINARY
```
