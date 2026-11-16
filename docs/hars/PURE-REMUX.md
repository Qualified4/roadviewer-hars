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

(병합된 커밋 기준 수치는 아래 [재현](#재현)의 명령으로 얻었다. 영상·경로·clock은 커밋하지 않았다.)

- 13구간 26영상(`integration/pure_remux_audit.py`, PyAV 16.1/FFmpeg 8.0.1, system ffmpeg 6.1.1 디코딩): qcamera 13·front 13 모두 패킷 1,200, PTS/DTS·키프레임 flag·sample payload SHA·마지막 외 duration·decoder configuration 바이트가 Python과 같음, 정규화 전체 NAL SHA = 원본, 전체 디코딩 1,200프레임·오류 줄 0·프레임 MD5 목록 = Python, stream duration = 각 쪽 모델 값. 키프레임 합계 TS 1,040, HEVC 780.
- 전체 replay JSON(`integration/real_conversion_compare.py`): 원본 13구간 13/13(모델링한 duration 파생 필드 외 차이 0, `drive_summary` 변화 없음), 14구간 14/14, 47구간 47/47.
- CI 합성 oracle `integration/pipeline_contract.py`: 통과(qcamera만 Python probe 값과 다른 것이 모델대로).
- 경계 시험: `tests/pure_remux.rs`, `tests/terminal_duration.rs`, `tests/pure_remux_faults.rs`, `tests/archive_pure_remux.rs`.

## 브라우저 재생

측정 후 기록한다.

## 성능

측정 후 기록한다.

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
