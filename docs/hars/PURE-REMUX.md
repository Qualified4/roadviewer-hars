# 순수 Rust 리먹싱 타당성 검증 (실험)

<!-- toc -->
**목차**

- [결론](#결론)
- [범위와 격리](#범위와-격리)
- [라이브러리 조사](#라이브러리-조사)
- [구현 요약](#구현-요약)
- [13구간 26영상 비교](#13구간-26영상-비교)
- [FFmpeg 엔진과 다른 지점(전체)](#ffmpeg-엔진과-다른-지점전체)
- [브라우저 재생·시킹](#브라우저-재생시킹)
- [성능](#성능)
- [손상·경계 입력](#손상경계-입력)
- [구현 복잡성과 유지보수성](#구현-복잡성과-유지보수성)
- [미검증 항목](#미검증-항목)
- [재현](#재현)
<!-- /toc -->

## 결론

**권고: 보류(조건부 채택 후보).** 기존 FFmpeg 엔진(`src/media.rs`, `src/native_video.rs`)은 기준 엔진으로 그대로 유지한다. 제품 변환 경로는 바꾸지 않았다.

- 정확성: 13구간 26영상 전부에서 두 엔진의 sample 수·PTS/DTS·keyframe·sample 크기·sample payload SHA가 **바이트 단위로 같았다**. 디코딩한 프레임 MD5 목록, 정규화 전체 NAL SHA, 제품 서명(VCL SHA+decoder header set)도 같았다.
- 남은 차이: TS 마지막 sample duration(11/13영상), hvcC 안 PPS의 끝 0 바이트 1개(HEVC 13/13), 컨테이너 부가 박스(elst·btrt·udta·chunk 구성·mvhd timescale·free 예약 공간)다. 디코딩 결과와 시킹에는 영향이 없었다. 마지막 duration 규칙은 다른 작업자가 정하는 중이라 이 실험에서 정하지 않았다.
- 성능(검증 포함, 같은 작업량, 13영상 중앙값 합): HEVC 경과 7.820→3.175초(-59.4%), CPU 4.932→1.308초(-73.5%). TS 경과 2.581→0.527초(-79.6%), CPU 2.274→0.385초(-83.1%). 최고 RSS는 95.9/87.5 MiB에서 약 34 MiB로 줄었다(20 ms 표본). 차이는 주로 프로세스 수(TS 3개→1개), FFmpeg 초기화·IPC, libavformat 순회에서 나온다. 순수 HEVC는 NTFS 입력 읽기가 경과의 대부분이다. 리먹싱 단계만의 수치이며, 변환 전체의 개선율이 아니다.
- 채택 조건: (1) TS 마지막 duration 규칙 결정 후 같은 규칙 적용, (2) edit list 미기록이 Python 원본·기존 출력과의 duration 표시에 미치는 영향 확인, (3) 실기기·HAOS(aarch64)와 Safari/iOS 재생 확인, (4) 손상 입력 fuzzing을 CI 시간 안에서 지속 운영. 이 조건 전에는 기본 엔진을 바꾸지 않는다.

## 범위와 격리

- 코드: `src/experimental/pure_remux.rs`와 `src/experimental/pure_remux/{annexb,hevc,ts,mp4}.rs`. Cargo feature `pure-remux`(기본 꺼짐)일 때만 컴파일된다.
- 내부 CLI: `roadviewer-hars __pure-remux SOURCE KIND TIMES_JSON OUTPUT [--no-verify]`. feature가 켜진 빌드에만 있다. 기본 빌드에서는 이 하위 명령이 없다.
- 새 의존성은 모두 optional이다. 기본 feature의 `cargo tree --locked -e normal` crate 목록(118개)은 기준 커밋 `a76c0c5`와 `diff`로 같다. feature를 켜면 `shiguredo_mp4`, `mpeg2ts` 2개가 늘어 120개가 된다. `Cargo.lock`에는 두 crate 항목이 추가된다(lock 파일은 feature와 무관하게 기록된다).
- 썸네일 디코딩, 역변환(`reverse`), 업로드 검사는 FFmpeg을 계속 쓴다. `convert.rs`/`jobs.rs`에 연결하지 않았다.

## 라이브러리 조사

2026-11-16에 crates.io API와 `static.crates.io` 원본 tarball을 직접 받아 확인했다. "지원" 열은 원본 소스 grep과 공개 API 확인 결과다. 실제 형식 호환성은 채택한 두 crate만 코퍼스로 검증했다. unsafe 수는 `src/` 안 `unsafe` 토큰 수다.

| crate | 최신 | 릴리스일 | 라이선스 | 다운로드(최근) | unsafe | hvc1/hvcC | avcC | ctts | stss | co64 | faststart | 비고 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `shiguredo_mp4` | 2026.5.0 | 2026-08-28 | Apache-2.0 | 75k(43k) | 0 | 빌더 있음(SPS 파싱) | 빌더 있음 | v0/v1 | 있음 | 자동 | moov 예약 영역 | 의존성 0, no_std, demux 포함. **채택** |
| `mp4-atom` | 0.17.1 | 2026-11-04 | MIT/Apache-2.0 | 706k(457k) | 0 | atom만 | atom만 | 있음 | 있음 | 있음 | 직접 배치 | atom codec. stbl 작성·샘플 표는 직접 구현해야 함 |
| `mp4` (alfg) | 0.14.0 | 2023-08-01 | MIT | 16M(3.8M) | 1 | 있음 | 있음 | 있음 | 있음 | 있음 | 없음(moov 끝) | 3년 넘게 릴리스 없음 |
| `re_mp4` | 0.5.1 | 2026-07-08 | MIT | 2.6M | 1 | 읽기 | 읽기 | 읽기 | 읽기 | 읽기 | - | 읽기 전용 |
| `muxide` | 0.5.0 | 2026-10-19 | MIT/Apache-2.0 | 41k | 22 | 있음 | 있음 | 있음 | 있음 | 없음 | 있음 | 4GiB 이상 불가, unsafe 많음 |
| `scuffle-mp4` | 0.1.5 | 2025-06-16 | MIT/Apache-2.0 | 5k | 0 | 있음 | 있음 | 있음 | 있음 | 있음 | 확인 못 함 | 사용자 적음 |
| `oxideav-mp4` | 0.0.11 | 2026-11-07 | MIT | 5k | 0 | 있음 | 있음 | 있음 | 있음 | 있음 | 있음 | 0.0.x, 암호 의존성 다수 |
| `symphonia-format-isomp4` | 0.6.1 | 2026-08-13 | MPL-2.0 | 14M | 0 | demux | demux | demux | demux | demux | - | 오디오 중심 demux, MPL |
| `mp4parse` (Mozilla) | 0.17.0 | 2023-05-29 | MPL-2.0 | 3.6M | - | 파싱 | 파싱 | - | - | - | - | 파서 전용, MPL |
| `mpeg2ts` | 0.6.1 | 2026-08-11 | MIT | 213k(154k) | 0 | - | - | - | - | - | - | TS/PAT/PMT/PES 헤더 파서, 의존성 0. **채택**(PES 조립은 직접) |
| `mpeg2ts-reader` | 0.18.2 | 2025-01-29 | MIT/Apache-2.0 | 123k | 0(forbid) | - | - | - | - | - | - | 콜백 기반, 1년 넘게 릴리스 없음 |
| `h264-reader` | 0.9.0 | 2026-09-14 | MIT/Apache-2.0 | 2.9M | 0(forbid) | - | SPS/PPS | - | - | - | - | 쓰지 않음(shiguredo가 SPS 제공) |
| `scuffle-h265`/`-h264` | 0.2.2 | 2025-05-17 | MIT/Apache-2.0 | 472k/32k | 0 | SPS | SPS | - | - | - | - | 쓰지 않음 |
| `hevc-parser` | 0.6.12 | 2026-08-29 | MIT | 100k | 미확인 | - | - | - | - | - | - | 원본 tarball 받기 실패, 미검토 |
| `cros-codecs` | 0.0.6 | 2025-06-18 | BSD-3-Clause | 3.9M | - | - | - | - | - | - | - | VAAPI 하드웨어 코덱용. 리먹서 용도 아님 |
| `mp4ra` | - | - | - | - | - | - | - | - | - | - | - | crates.io에 없음(404) |

선택 근거: `shiguredo_mp4`는 의존성·unsafe가 없고, 스트리밍 mux(예약 moov → faststart, 재작성 없음), hvc1/avc1 sample entry 빌더, co64 자동 전환, 같은 crate의 demuxer를 모두 제공한다. 라이선스는 Apache-2.0이다. `mpeg2ts`는 의존성이 없고 PAT/PMT/PES 헤더를 파싱한다. 다만 PES 조립기(`PesPacketReader`)는 크기 한도와 PID 필터가 없어 쓰지 않고, 패킷 리더만 쓴다. 두 crate의 비시험 코드에 `unwrap`/`expect`/`panic!` 문자열이 각각 230개, 55개 있다(시험 코드 포함 grep 수). 예: `mpeg2ts`의 PAT/PMT `expect("Never fails")`. 그래서 변이 입력 패닉 시험을 두었다([손상·경계 입력](#손상경계-입력)).

## 구현 요약

- **HEVC Annex B → MP4**: 1 MiB 청크로 한 번 읽는다. 같은 순회에서 NAL 분할, 기존 정책과 같은 서명(VCL SHA+header set), access unit 경계 판정(FFmpeg HEVC parser 규칙: VPS/SPS/PPS/AUD/prefix SEI/41..44/48..55 또는 `first_slice_segment_in_pic_flag`), sample 쓰기를 모두 한다. VPS/SPS/PPS는 hvcC에만 넣고 sample에서는 뺀다(hvc1). PTS=DTS=`round(times[i]*1e6)`(timescale 1,000,000), 마지막 duration은 FFmpeg 엔진과 같은 0.05초다. SPS의 `sps_max_num_reorder_pics`가 0이 아니면 거부한다(기존 엔진의 `has_b_frames` 거부와 같은 정책). keyframe은 IRAP(16..23)이다.
- **H.264 MPEG-TS → MP4**: PAT/PMT에서 비디오 ES가 H.264 하나인지 확인하고 PES를 직접 조립한다(32 MiB 한도, continuity counter 검사). PES PTS/DTS를 보존한다(33비트 wrap 보정). 첫 PTS를 0으로 맞추고(Python 원본의 `offset=packet.pts` 규칙), PTS-DTS 차이는 ctts로 쓴다. B-frame이면 PTS가 DTS보다 앞서는 sample이 있어 **ctts v1(음수 offset)** 을 쓰고 edit list는 쓰지 않는다. 기존 TS 경로는 B-frame을 거부하지 않으므로 같은 정책으로 지원했다(합성 `-bf 2` 시험만 했고, 실주행 TS에는 B-frame이 없다). 마지막 duration은 직전 DTS 간격이다(규칙 미확정, 차이만 기록).
- **sample 프레이밍**: FFmpeg `ff_nal_parse_units`처럼 다음 start code가 4바이트일 때 그 앞 0 하나만 빼고, 버퍼 끝 패딩은 남긴다. 처음 시험에서는 끝 0을 전부 지웠다. 그때는 TS sample 1124/1200개의 크기가 달랐다. 디코딩 결과는 같았다. 이 규칙으로 바꾼 뒤 26영상 모두 sample 바이트가 같아졌다. 해시는 이전처럼 끝 0을 지운 NAL로 계산한다.
- **검증(같은 작업량)**: `remux(..., verify=true)`는 출력 MP4를 순수 Rust로 한 번 더 읽는다(shiguredo demuxer). 그리고 moov<mdat 배치, hvc1/avc1, 4바이트 NAL 길이, sample 안 파라미터 세트 부재(hvc1), 해상도 한도, DTS 단조 증가, 첫 sample keyframe, 입력과 같은 서명, 로그 시각 ±2 ms를 확인한 뒤 원자적 rename을 한다. FFmpeg 엔진의 출력 재검사(`inspect`+`validate_times`+`verify_mp4_boxes`)에 대응한다.

## 13구간 26영상 비교

입력은 원본 형식 코퍼스(13 `fcamera.hevc` + 13 `qcamera.ts`, 읽기 전용)다. 시각은 기존 private clock jobs를 썼다. FFmpeg 엔진은 기본 빌드의 `remux` CLI, 순수 엔진은 feature 빌드의 `__pure-remux`다. 비교는 출력 생성 후 PyAV 16.1(FFmpeg 8 계열)과 system ffmpeg 6.1.1로 했다. 전체 1회 실행이다. 그 전에 같은 비교를 한 번 돌려 위의 프레이밍 차이를 찾았고, 수정 후 전체를 다시 돌렸다.

| 항목 | HEVC FFmpeg | HEVC 순수 | TS FFmpeg | TS 순수 |
|---|---|---|---|---|
| 성공 | 13/13 | 13/13 | 13/13 | 13/13 |
| 제품 서명(VCL SHA+header set) = 입력 | 13 | 13 | 13 | 13 |
| 정규화 전체 NAL SHA = 입력 | 13 | 13 | 13 | 13 |
| 패킷 1200 + ffmpeg 전체 디코딩 프레임 1200 | 13 | 13 | 13 | 13 |
| ffmpeg 디코딩 오류 줄 0 | 13 | 13 | 13 | 13 |
| moov가 mdat 앞 | 13 | 13 | 13 | 13 |
| keyframe 합계 | 780 | 780 | 1040 | 1040 |
| 출력 합계 bytes | 489,035,478 | 489,630,541 | 25,255,228 | 25,859,828 |

두 엔진 출력 사이의 비교(영상별 26쌍):

| 항목 | HEVC 13쌍 | TS 13쌍 |
|---|---|---|
| 패킷별 PTS/DTS | 차이 0 | 차이 0 |
| keyframe 인덱스(stss) | 같음 13 | 같음 13 |
| sample 크기·payload SHA | 차이 0 | 차이 0 |
| 패킷 duration | 차이 0 | 마지막 sample만 11영상 다름 |
| stts | 같음 13 | 같음 2(다른 11은 마지막 항목만) |
| ctts | 둘 다 없음 | 둘 다 없음 |
| 디코딩 프레임 MD5 목록 | 같음 13 | 같음 13 |
| decoder configuration 바이트 | 다름 13(PPS 끝 0 1바이트) | 같음 13 |
| stream duration_ts | 같음 13 | 같음 2 |

## FFmpeg 엔진과 다른 지점(전체)

1. **TS 마지막 sample duration**: FFmpeg는 4500 ticks(90 kHz, 추정 프레임 속도 20 fps)를 쓴다. 순수 엔진은 직전 DTS 간격을 쓴다. 다른 영상과 값(FFmpeg→순수): 2번 4500→4303, 3번 4500→4502, 4번 4500→4650, 6번 4500→4446, 7번 4500→4351, 8번 4500→4202, 9번 4500→4840, 10번 4500→4713, 11번 4500→4506, 12번 4500→4541, 13번 4500→4496. 1·5번은 같다. stts 마지막 항목, mdhd duration, stream `duration_ts`가 이만큼 다르다. Python 원본(PyAV)의 값은 이 실험에서 다시 측정하지 않았다. 기존 문서에는 3·9번 4481/4518로 기록되어 있다. 이 셋 모두 서로 다르다. **규칙은 정하지 않았다.**
2. **hvcC의 PPS**: FFmpeg hvcC의 PPS는 끝 0이 2바이트(12바이트)이고, 순수 엔진은 1바이트(11바이트)다. 입력 Annex B에서 PPS 뒤에 0이 여러 개 붙어 있기 때문이다. header set SHA는 끝 0을 지우고 계산하므로 같다. 디코딩도 같다. 그 밖의 hvcC 필드(profile/tier/level, compatibility/constraint, chroma, bit depth, `min_spatial_segmentation_idc`, `parallelismType`, `avgFrameRate`, `constantFrameRate`, temporal 필드, lengthSize)는 이 코퍼스에서 바이트가 같다. `shiguredo_mp4`는 `min_spatial_segmentation_idc`/`parallelismType`을 0으로 고정한다. 이 값이 0이 아닌 스트림에서는 다를 수 있다(코퍼스에서는 둘 다 0).
3. **edit list**: FFmpeg는 `edts/elst`(media_time 0, 전체 길이)를 쓴다. 순수 엔진은 쓰지 않는다. 첫 PTS가 0이라 재생 시작 위치는 같다. B-frame TS는 elst 대신 음수 ctts로 첫 PTS를 0에 맞춘다(합성 시험만 함).
4. **mvhd/tkhd timescale**: FFmpeg는 movie timescale 1000(ms)이다. 순수 엔진은 media timescale(1,000,000 / 90,000)을 그대로 쓴다. 표시 길이는 반올림 차이 범위에서 같다.
5. **chunk 구성**: FFmpeg는 HEVC 약 37개, TS 2개 chunk(stsc 30~33/2 항목)를 쓴다. 순수 엔진은 1개 chunk다. 트랙이 하나라 interleave 영향은 없다. 시킹은 브라우저로 확인했다.
6. **부가 박스**: 순수 엔진에는 `btrt`(sample entry 안)와 `udta`(encoder 문자열)가 없다.
7. **free 예약 공간**: 순수 엔진은 재작성 없이 faststart를 하려고 moov 자리를 미리 비워 둔다(4096+48바이트×예상 sample 수). 실제 moov(약 16 KB)를 뺀 나머지 약 46 KB가 `free` 박스로 남는다. HEVC는 +0.12%, TS는 +2.4% 크다. sample당 예약량을 줄이면 작아지지만, 모자라면 오류가 난다(faststart 아닌 파일은 만들지 않는다).
8. **입력 계약 차이**: HEVC는 `times[0]==0`을 요구한다(제품 clock은 항상 0에서 시작한다). FFmpeg 엔진은 0이 아니어도 elst로 처리한다. PES에 PTS가 없으면 오류다(Python은 DTS 없는 패킷을 건너뛴다). TS continuity 오류와 transport error는 오류다(FFmpeg는 경고만 하고 계속한다). SPS/PPS가 바뀌는 스트림은 출력 header set이 입력과 달라져 서명 검사에서 거부된다. ES rate·trick mode·CRC·PES extension 플래그가 있는 PES는 `mpeg2ts`가 지원하지 않아 오류가 난다(실주행 qcamera에는 없다).

## 브라우저 재생·시킹

Windows의 Microsoft Edge 157(HeadlessChrome 157, `--headless=new`)로 확인했다. 매번 새 임시 profile을 쓰고, 영상은 Range를 지원하는 loopback 서버(127.0.0.1:18161)로 제공했다. 사용자의 브라우저 세션과 ego-browser는 쓰지 않았다. `<video>`의 `canPlayType`(hvc1/avc1 모두 `probably`), `loadedmetadata`, 중간(50%)·말미(99.5%) seek의 `seeked`와 `readyState`, `play()`, `error`를 기록했다.

- 순수 엔진 26영상: 26/26에서 중간·말미 seek 모두 `seeked`, readyState 3~4, 재생 성공, error 없음. 첫 시도에서 5번 TS의 `loadedmetadata`가 15초 대기 시간을 넘겼다. 같은 파일을 FFmpeg 출력과 함께 두 번 다시 돌렸을 때는 둘 다 정상이었다. 단일 스레드 시험 서버가 직전 HEVC 전송에 묶인 영향으로 보지만, 원인은 확정하지 못했다.
- 3번 구간 4파일(두 엔진 × 두 형식): 모두 정상이다. duration은 TS FFmpeg 59.989089 / 순수 59.989111초, HEVC는 둘 다 59.989083초다.
- 미검증: Safari/iOS, Firefox, Android WebView, Linux Chromium, HA Ingress 경유 재생.

## 성능

조건: WSL2 Ubuntu 24.04 x86_64, 16 logical CPU, Rust 1.100 release(opt3/thin LTO). 입력은 NTFS(drvfs) 원본, 출력은 WSL ext4다. 영상마다 구현별 warmup 1회를 한 뒤, 순서를 회전·역순 교차해 3회씩 돌렸다. warm cache이고 전역 캐시는 지우지 않았다. 실행 수는 26영상 × 4방식 × (1+3)회다. CPU는 회수한 자식 프로세스를 포함한다. RSS는 20 ms 간격 프로세스 트리 표본이다. 순수 엔진은 30~250 ms 만에 끝나 표본이 중앙값 6.5개뿐이므로, 평균 RSS는 과소 표집이다. 최고 RSS만 참고한다.

작업량:
- 검증 포함: 두 방식 모두 입력 압축 그림 SHA·header set, mux, 출력 재검사(서명·PTS/DTS·로그 시각·박스 배치)를 한다. FFmpeg 엔진은 TS에서 별도 inspect 프로세스, 출력 inspect 프로세스, 상위 `remux` 프로세스를 쓴다. 순수 엔진은 한 프로세스에서 입력 1회와 출력 1회를 읽는다.
- 코어: 순수 엔진 `--no-verify`는 mux+입력 SHA다. FFmpeg HEVC 코어(`__native-hevc`)도 mux+입력 SHA라 작업량이 같다. FFmpeg TS 코어는 ffmpeg CLI 스트림 복사뿐이고 SHA가 없으므로, 순수 쪽 작업량이 더 많다.

| 13영상 합계(영상별 중앙값의 합) | 경과(초) | CPU(초) | 입력 처리량(MB/s) | 최고 RSS(KiB) |
|---|---|---|---|---|
| HEVC FFmpeg 검증 포함 | 7.820 | 4.932 | 62.5 | 95,868 |
| HEVC 순수 검증 포함 | 3.175 | 1.308 | 154.0 | 34,596 |
| HEVC FFmpeg 코어(mux+SHA) | 6.278 | 3.553 | 77.9 | 64,952 |
| HEVC 순수 코어(mux+SHA) | 2.887 | 1.030 | 169.4 | 34,624 |
| TS FFmpeg 검증 포함 | 2.581 | 2.274 | 11.7 | 87,500 |
| TS 순수 검증 포함 | 0.527 | 0.385 | 57.2 | 34,036 |
| TS FFmpeg 코어(복사만, SHA 없음) | 1.209 | 1.090 | 24.9 | 54,624 |
| TS 순수 코어(mux+SHA) | 0.496 | 0.358 | 60.8 | 34,904 |

검증 포함 기준으로 순수 엔진은 HEVC 경과 59.4%, CPU 73.5%가 적었고, TS 경과 79.6%, CPU 83.1%가 적었다. 영상 1개 중앙값은 HEVC 0.601→0.244초, TS 0.202→0.041초다. 3번 HEVC 3회차에서 FFmpeg 쪽 두 표본이 각각 8.51초(CPU 0.58초)와 6.18초(CPU 0.70초)였다. CPU는 정상이었으므로 I/O 지연으로 본다. 중앙값에는 반영되지 않았다.

무엇이 수치를 제한하는가(3번 HEVC, `/usr/bin/time` 진단 3회, 벤치 표본 아님):
- 순수 HEVC 코어는 drvfs 입력에서 경과 0.20~0.22초, CPU 약 0.07초다. 같은 파일을 `cat`으로 읽기만 해도 0.16~0.17초가 걸린다. 같은 입력을 ext4에 복사하면 순수 코어는 0.06~0.07초다. 즉 **NTFS 입력 읽기가 경과의 대부분**이다. ext4 입력에서 FFmpeg HEVC 코어는 0.21~0.22초(CPU 약 0.21초)로 CPU가 제한한다.
- FFmpeg 엔진의 나머지 차이는 프로세스 기동·동적 라이브러리 초기화(기존 진단에서 프로세스당 수십 ms), JSON IPC, 출력 재검사를 위한 libavformat 순회에서 나온다. TS는 영상이 작아(2.3 MB) 프로세스 3개의 고정 비용이 대부분이다.
- 순수 엔진의 남은 CPU는 SHA-256과 NAL 탐색·복사다. 이번에는 프로파일링하지 않았다.

이 수치는 리먹싱 단계만의 값이다. 로그 변환·썸네일·업로드 전체의 개선율로 쓰지 않는다. HAOS·aarch64·cold cache는 측정하지 않았다.

## 손상·경계 입력

`tests/pure_remux.rs`(합성 입력, `#![cfg(feature = "pure-remux")]`)에서 패닉 없이 `Err`를 반환하는지, 출력·임시 파일이 남지 않는지 확인한다.

| 입력 | 결과 |
|---|---|
| 188바이트 경계가 아닌 곳에서 자른 TS | `invalid MPEG-TS packet` |
| 앞 절반만 남긴 TS(프레임 수 부족) | 프레임 수/시각 불일치 |
| 비디오 PID 패킷 1개 누락 | `MPEG-TS continuity error` |
| VPS/SPS/PPS를 뺀 HEVC | `missing pictures or decoder configuration` |
| 32 MiB를 넘는 NAL | `NAL unit exceeds bounded memory limit` |
| 빈 파일(HEVC/TS), 0프레임 | 오류 |
| Annex B가 아닌 입력, 형식 뒤바뀜 | 오류 |
| 로그보다 프레임이 많음, 로그 시각 10 ms 어긋남 | 오류 |
| 무작위 바이트 변이 300건(TS/HEVC 각 150, 앞 4 KiB 집중·잘라내기 포함) | 패닉 0. 성공한 변이 출력은 ffmpeg로 디코딩 가능 |

잘린 TS에서 **마지막 PES가 패킷 경계에서 잘리고** `PES_packet_length`가 0(비디오의 일반적인 경우)이면, 마지막 프레임이 짧아진 것을 컨테이너 수준에서 알 수 없다. 이 점은 FFmpeg 엔진·Python도 같다(입력 대비 서명이므로 둘 다 통과한다).

4 GiB: co64 전환은 `shiguredo_mp4`에서 오프셋 5 GiB 시험으로 확인했다(`stco` 없음, `co64` 있음). 실제 크기를 계산하면 HEVC 1구간은 약 37.6 MB이므로, 4 GiB에 이르려면 약 114구간(약 1.9시간 분량)을 한 파일에 넣어야 한다. 제품은 구간 단위로 변환하므로 현재 경로에서는 co64가 필요하지 않다. sample 하나는 32 MiB, moov는 16 MiB로 제한한다.

## 구현 복잡성과 유지보수성

| 항목 | 값 |
|---|---|
| 실험 코드 | 1,154줄(`src/experimental` 6개 파일) |
| 시험 코드 | `tests/pure_remux.rs` 386줄, 시험 5개 |
| 기존 공유 파일 변경 | `Cargo.toml` 6줄, `src/lib.rs` 2줄, `src/main.rs` 18줄, `Cargo.lock` 14줄 |
| 우리 코드의 unsafe | 0 (FFmpeg 엔진 `native_video.rs`에는 FFI unsafe 블록이 있다) |
| 새 의존성 | 2개(optional). 둘 다 추가 하위 의존성 없음, unsafe 0 |
| 직접 구현한 부분 | Annex B 스트리밍 분할·프레이밍, HEVC AU 경계, SPS reorder 파싱, PES 조립·CC·33비트 wrap, 출력 재검사 |
| 미지원 | HEVC B-frame(정책상 거부), H.265 in TS, 여러 비디오 ES, SPS/PPS 변경 스트림, PES 특수 플래그, `times[0]≠0`, edit list, fragmented MP4 |

유지보수 위험: `shiguredo_mp4`는 연 단위 버전(2026.x)이고 다운로드 수가 적다. API가 바뀌면 이 실험 코드도 고쳐야 한다. Cargo.toml에 `=2026.5.0`으로 고정했다. FFmpeg 엔진은 system FFmpeg(Alpine/Debian 패키지)의 동작에 의존한다. 순수 엔진은 그 의존을 리먹싱 경로에서만 없앤다. 썸네일·역변환은 여전히 FFmpeg이 필요하다. 그래서 배포 이미지에서 FFmpeg를 뺄 수는 없다.

## 미검증 항목

- HAOS·aarch64 실기기, 실제 업로드→변환 경로(제품에 연결하지 않음).
- Safari/iOS·Firefox·Android 재생. Edge 외 브라우저.
- 실주행 B-frame TS(코퍼스에 없음). B-frame은 합성 x264 `-bf 2`로만 시험했다.
- Python 원본 출력과의 직접 비교(이번에는 FFmpeg 엔진과만 비교).
- cold cache 성능, 다른 CPU.
- `hevc-parser` crate(원본을 받지 못함).

## 재현

```sh
cargo test --features pure-remux --test pure_remux
cargo build --release --features pure-remux
roadviewer-hars __pure-remux /private/fcamera.hevc fcamera.hevc /private/clock.json /private/new.mp4
```

비교·측정 스크립트는 기존 `integration/benchmark_remux.py`·`nal_inventory.py`·`profile_conversion.measure`를 재사용해 private 경로에서 실행했다. 숫자 집계는 [측정 JSON](measurements/pure-remux-2026-11-16.json)에 있다. 영상·경로·clock은 커밋하지 않았다.
