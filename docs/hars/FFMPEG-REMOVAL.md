# FFmpeg 제거 1단계: 기준 측정과 순수 Rust 역변환

<!-- toc -->
**목차**

- [결론](#결론)
- [FFmpeg 의존 지점](#ffmpeg-의존-지점)
- [기준 이미지와 반사실 비교 (amd64)](#기준-이미지와-반사실-비교-amd64)
- [역변환 계약](#역변환-계약)
- [설계](#설계)
- [검증 결과](#검증-결과)
- [성능](#성능)
- [3단계에서 지울 것](#3단계에서-지울-것)
- [미검증과 남은 위험](#미검증과-남은-위험)
- [재현](#재현)
<!-- /toc -->

## 결론

- 원본 형식 내보내기(역변환, MP4 → `qcamera.ts`/`fcamera.hevc`/`ecamera.hevc`)의 기본 엔진은 순수 Rust(`src/pure_reverse.rs`, `src/pure_reverse/`)다. 기존 FFmpeg CLI 역변환(`media::reverse_ffmpeg`)은 대체 경로로 남는다(3단계에서 제거).
- 실주행 13구간 26영상의 왕복(원본 → 순수 정방향 MP4 → 순수 역변환)에서 **qcamera.ts 13/13은 업로드한 원본과 파일 바이트가 같다.** fcamera.hevc 13/13은 정규화 NAL·프레임·키프레임·parameter set·디코딩 프레임 MD5가 원본과 같고, 파일은 IRAP마다 PPS 뒤 0 바이트 하나(영상당 60곳, +60 B)만 다르다. 이 HEVC 결과는 기존 FFmpeg CLI 역변환·Python 내보내기와 바이트가 같다.
- FFmpeg 런타임 패키지(`ffmpeg`)와 그것만 끌어오던 의존 패키지 192개를 뺀 반사실 이미지와 직접 비교하면 amd64 이미지는 **풀린 크기 580.9 MB → 125.2 MB(−455.7 MB, −78.5 %)**, 레지스트리 전송 크기(layer blob, gzip) **217.5 MB → 46.5 MB(−171.0 MB, −78.6 %)**다. 이 숫자는 FFmpeg 패키지만 뺀 값이고, 빌더의 libav dev 패키지·`ffmpeg-next`를 뺀 실행 파일 크기 변화는 3단계에서 따로 잰다.

## FFmpeg 의존 지점

`d846fc6` 기준. "이 브랜치 뒤"는 `feat/pure-reverse`의 상태다.

| 위치 | 쓰임 | 이 브랜치 뒤 |
|---|---|---|
| `src/media.rs` `probe`/`validate_upload` (`ffprobe` CLI) | 업로드 검사(`upload.rs`, `transfer.rs` 가져오기), 썸네일 전 검사, FFmpeg 역변환 전 검사 | 그대로(역변환 기본 경로는 쓰지 않음) |
| `src/media.rs` `signature` (`ffmpeg` CLI, `*_mp4toannexb`) | FFmpeg 역변환의 압축 그림 서명 | FFmpeg 대체 경로에서만 |
| `src/media.rs` `reverse_ffmpeg` (`ffmpeg` CLI 스트림 복사) | 역변환 | 대체 경로(기본은 순수) |
| `src/native_video.rs` (`ffmpeg-next`, libavformat/libavcodec BSF) | FFmpeg 리먹싱 엔진, `__inspect`(TS 원본 검사·MP4 검사), errno 판정 | 그대로. TS 정방향은 원본 검사를 이 inspector로 한다 |
| `src/thumbnail.rs` (`ffmpeg-next` 디코딩·swscale) | 썸네일 | 그대로(다른 작업자의 2단계) |
| `src/pure_remux/` | 순수 정방향 엔진. FFmpeg를 부르지 않는다(`annexb`·`readback`의 주석과 FFmpeg 사본 판독 규칙만 FFmpeg를 언급) | 그대로 |
| `Dockerfile` | 빌더 `libavformat-dev libavcodec-dev libavutil-dev libswscale-dev`, 런타임 `ffmpeg` | 그대로(3단계) |
| `.github/workflows/ci.yml`, `prerelease.yml` | `ffmpeg`과 libav dev 패키지 설치 | 그대로 |
| 시험(`tests/media.rs`, `pure_remux.rs`, `pure_remux_faults.rs`, `terminal_duration.rs`, `ts_duration.rs`, `conversion_keep_previous.rs`, `legacy_archive.rs`, `common/recording.rs`, `pure_reverse.rs`) | `ffmpeg` CLI로 합성 입력을 만들고 대조 | 개발 전용 도구로 남길 방침 |
| `integration/`, `scripts/container_smoke.py` | 대조·측정 도구 | 개발 전용 |

## 기준 이미지와 반사실 비교 (amd64)

방법: `d846fc6`의 `Dockerfile`로 `hars-a4-reverse:base`를, 같은 Dockerfile에서 런타임 `apt-get install`의 `ffmpeg` 한 단어만 뺀 것으로 `hars-a4-reverse:noffmpeg`를 로컬 amd64에서 `docker build`했다(게시·태그 push 없음). 반사실 이미지의 실행 파일은 여전히 libav*를 링크하므로 실행되지 않는다. 둘을 `scripts/image_report.py base --compare noffmpeg`로 쟀다. 숫자: [ffmpeg-image-amd64-2026-11-17.json](measurements/ffmpeg-image-amd64-2026-11-17.json).

| | 기준(`ffmpeg` 포함) | 반사실(`ffmpeg` 없음) | 줄어드는 양 |
|---|---|---|---|
| 풀린 layer 합(tar 바이트) | 580,879,872 | 125,169,152 | **455,710,720 (78.5 %)** |
| 레지스트리 layer blob(gzip) 합 | 217,472,878 | 46,508,246 | **170,964,632 (78.6 %)** |
| layer별 gzip -6 합 | 212,837,475 | 46,248,833 | 166,588,642 (78.3 %) |
| `docker save \| gzip -6` | 214,888,770 | 46,140,685 | 168,748,085 (78.5 %) |
| `docker image inspect` Size | 813,379,464 | 180,586,727 | 632,792,737 (77.8 %) |
| 설치 패키지 수 | 299 | 107 | 192 |

- layer별: 기반 debian 77.9 MB, apt layer **473.2 MB → 17.5 MB**, Rust 실행 파일 25.3 MB(stripped 아님: symtab 있음, debug section 0), 웹 자산 4.4 MB. 줄어드는 것은 apt layer 하나다.
- `docker image inspect`의 Size는 containerd 이미지 저장소에서 압축 blob까지 더한 값이라(813 MB ≈ 풀린 581 MB + blob 217 MB + 블록 반올림) 비압축 크기로 쓰지 않는다. 이전 인수인계의 "비압축 약 813 MB"는 이 값이었다.
- `ffmpeg` 때문에만 설치되는 192 패키지가 소유한 파일은 446.6 MB(하드링크 1회)이고 그중 공유 라이브러리 240개가 431.5 MB다. 큰 것: libllvm15 117.3 MB(Mesa), libicu72 37.0 MB, libmfx1 28.0 MB, libflite1 27.1 MB, libgl1-mesa-dri 25.8 MB, libz3-4 23.3 MB, libcodec2 16.9 MB, libx265 16.4 MB, libavcodec59 15.0 MB. FFmpeg 자체(`ffmpeg`, `ffprobe`, libav*/libsw*/libpostproc)는 32.9 MB뿐이다. 즉 절감의 대부분은 FFmpeg가 끌어오는 그래픽·오디오·코덱 의존성이다.
- 남는 것(반사실): nginx, ca-certificates와 그 의존성, 기반 이미지. 실행 파일의 libav 링크 4개(`libavutil.so.57`, `libavformat.so.59`, `libswscale.so.6`, `libavcodec.so.59`)는 3단계에서 `ffmpeg-next`를 빼야 없어진다.
- aarch64는 로컬에서 재지 않았다. 스크립트는 docker만 쓰고 아키텍처를 가리지 않으므로 CI의 컨테이너 작업에서 같은 명령으로 잴 수 있다(그 CI 변경은 이 단계 범위 밖).

## 역변환 계약

찾은 계약(코드·시험·문서·Python 참조):

- Python `media_export.export`(`reference/python/roadviewer/app/media_export.py`): PyAV로 MP4의 첫 영상 스트림 패킷을 그대로 `mpegts`/`hevc` muxer에 넣고, PTS/DTS에 `round(offset / time_base)`를 더한다. 재인코딩 없음. 코덱이 맞지 않으면 실패.
- Python 시험 `test_media_downloads.py`: 내보낸 원본의 **디코딩 픽셀 해시가 원래 원본과 같음**, TS 첫 프레임 시각이 offset만큼(소수 4자리) 이동, ZIP 안의 파일도 같은 픽셀. 파일 바이트 동일은 요구하지 않는다.
- `d846fc6` Rust(`media::reverse`): `ffprobe` 검사(영상 1개, 코덱, 크기) → MP4와 결과의 정규화 서명(`NalHasher`: 그림 NAL 순서 해시, parameter set 집합, 기타 NAL 순서 해시) 일치 → TS는 결과 PTS 목록이 MP4 PTS + offset과 ±2 ms 안. fsync 없음.
- 문서(`VALIDATION.md`): "재생성 파일 SHA는 달랐지만 정규화 전체 NAL과 decoder header는 같고 TS PTS 차이 0".

결론: **원본 업로드 파일과 바이트가 같아야 한다는 계약은 없다.** 계약은 "압축 영상을 재인코딩하지 않고, 정규화 NAL·decoder header·(TS) 시각을 보존"이다. 새 구현은 이것보다 강하게 확인한다:

| 항목 | FFmpeg CLI 역변환(`d846fc6`) | 순수 역변환 |
|---|---|---|
| 정규화 서명(그림·parameter set·기타 NAL) | MP4 ↔ 결과 | MP4(독립 판독기) ↔ 결과(다시 읽기), worker와 부모에서 두 번 |
| TS PTS | 정렬한 목록 ±2 ms | PES마다 PTS·DTS가 정확히 같음(90 kHz 틱) |
| 키프레임 | 확인 안 함 | TS: PES의 random access 표시 = MP4 sync sample. HEVC: 그림 순서 해시에 IRAP 유형 포함 |
| 프레임 수 | TS만 간접 | 같음 |
| 입력 MP4 구조 | `ffprobe` | 독립 판독기(`readback::inspect_copy`): sample 표 일관성, sample당 그림, sync 표시, 한도 |
| 결과 내구성 | rename만 | `sync_all` 후 rename, 폴더 sync(`durable_rename`) |

바이트 결과(실측, [검증 결과](#검증-결과)): TS는 원본 업로드와 같다(원본이 Road Viewer 원본 형식 내보내기, 즉 libavformat `mpegts` muxer 출력이었기 때문이다. Python 내보내기도 같은 바이트를 낸다). HEVC는 원본과 PPS 뒤 0 바이트만 다르다: 정방향 변환이 hvcC PPS에 끝 0 두 개를 남기고(libavformat 규칙, [PURE-REMUX.md](PURE-REMUX.md) 차이 2), `hevc_mp4toannexb`가 그 뒤에 4바이트 start code를 붙이므로 원본의 0 4개가 5개가 된다. 이 0 바이트는 `trailing_zero_8bits`라 NAL 내용이 아니고, 어느 내보내기(FFmpeg CLI, Python)도 원본으로 되돌리지 못한다(정보가 MP4에 없다). 그래서 HEVC는 FFmpeg 내보내기와 바이트가 같게 맞췄다.

## 설계

- 위치: `src/pure_reverse.rs`(작업·검증), `src/pure_reverse/annexb.rs`(sample → Annex B), `src/pure_reverse/ts.rs`(MPEG-TS writer). `src/media.rs`는 `reverse`가 `reverse_using(Engine::configured()?, …)`를 부르게만 바꿨고 기존 FFmpeg 코드는 `reverse_ffmpeg`로 이름만 바꿔 그대로 둔다.
- 엔진 선택: 정방향과 같은 내부 설정 `HARS_REMUX_ENGINE=pure|ffmpeg`(기본 `pure`, 다른 값이면 서버가 시작하지 않음)를 역변환도 따른다. 공개 API·파일명·메타는 엔진과 무관하다. 어느 엔진이 썼는지는 구조화 로그 `reverse_engine`(`engine`, `kind`, 대체 때 `requested`)에만 남는다.
- 실패와 대체: 순수 엔진의 실패는 정방향과 같은 5분류(`pure_remux::FailureClass`)와 같은 정책(`media::falls_back`)이다. `unsupported`·`invalid`·`internal`·`verification`이면 FFmpeg 역변환을 한 번 시도하고 `reverse_fallback`(정방향의 `remux_fallback`과 같은 수준·필드)을 남긴다. `resource`(ENOSPC·EFBIG·EDQUOT·ENOMEM, 180초, worker SIGKILL·SIGABRT, 종료 코드 75)는 대체 없이 `reverse_failure`와 함께 실패한다. HTTP 응답은 기존과 같이 `media_export_failed`다.
- 격리: 서버·CLI에서는 worker 프로세스 `roadviewer-hars __pure-reverse MP4 KIND OFFSET_TICKS OUTPUT`이다. `media::run`(동시 2개, 180초, 응답 64 MiB, `kill_on_drop`, `PR_SET_PDEATHSIG`)과 정방향 worker의 메모리 상한(`RLIMIT_AS` = 현재 + 256 MiB)을 그대로 쓴다. 라이브러리 내장(시험) 경로는 `spawn_blocking` + `catch_unwind`.
- 두 번 읽기: (1) `pure_remux::readback::inspect_copy`가 MP4를 검사하고 내보내기가 담아야 할 것(서명, 프레임 수, sync sample, PTS/DTS)을 정한다. (2) 쓰기는 이와 코드를 공유하지 않는 `shiguredo_mp4`의 `Mp4FileDemuxer`가 읽은 sample 표로 한다. (3) 쓴 파일을 `NalHasher`(HEVC) 또는 정방향 엔진의 TS demuxer(`pure_remux::ts::demux`, `mpeg2ts`)로 다시 읽어 (1)과 비교한다. 두 판독기가 입력을 다르게 읽으면 `verification`이다.
- Annex B: 각 sample의 4바이트 길이 NAL을 그대로 옮기고, libavcodec `h264_mp4toannexb`/`hevc_mp4toannexb`가 하는 선택(decoder configuration을 넣는 자리, start code 길이)을 따른다. H.264는 in-band SPS/PPS가 없는 IDR 앞에 avcC의 SPS/PPS를 넣고, HEVC는 sample의 첫 IRAP NAL 앞에 hvcC의 NAL을 모두 넣는다. FFmpeg 코드는 옮기지 않았다(동작만 맞춤).
- MPEG-TS: 원본이 가진 libavformat `mpegts` 배치를 따른다. 프로그램 1, PMT PID 0x1000, 영상 PID 0x100(PCR 운반), stream type 0x1B, SDT(서비스 이름 `Service01`, 제공자 `FFmpeg`: 원본에 있는 값 그대로), PAT/PMT는 첫 PES·100 ms마다·키프레임 묶음의 첫 키프레임에, SDT는 500 ms마다, PES 길이 0, 모든 PES 첫 패킷에 PCR(= DTS × 300, muxdelay 0)과 키프레임의 random access 표시, 마지막 패킷은 adaptation field stuffing, AUD로 시작하지 않는 access unit에는 AUD를 붙인다. PTS/DTS = MP4 시각을 90 kHz로(반올림) + offset 틱. offset 틱은 `round(video_origins × 90000)`.
- 한도: MP4 메타 16 MiB, sample 32 MiB, sample 수 262,144(판독기), NAL·parameter set 한도(`NalHasher`), 출력 4 GiB, 33비트 PES 시각. 넘으면 `unsupported`/`invalid`로 실패하고 FFmpeg 대체로 넘어간다. 메모리는 sample 하나와 해시 버퍼로 묶이고(파일을 통째로 읽지 않음), `unsafe`는 새 코드에 없다.
- 순수 엔진이 거부하는 정상 입력(→ FFmpeg 대체): 첫 항목이 media time 0이 아닌 edit list(FFmpeg가 B-frame MP4에 쓰는 것), 트랙이 둘 이상, sample entry 변경, hev1·in-band HEVC parameter set, 길이 필드 4바이트가 아닌 avcC/hvcC, 음수 composition 시각, 33비트를 넘는 시각.

## 검증 결과

모두 이 브랜치의 release 실행 파일, WSL2 Ubuntu 24.04 x86_64. 영상·이름·경로·clock은 커밋하지 않았다. 숫자: [pure-reverse-2026-11-17.json](measurements/pure-reverse-2026-11-17.json).

(a) 13구간 26영상 왕복(`integration/pure_reverse_audit.py`): 원본 → `remux`(순수) → `reverse`(순수)와 `reverse`(`HARS_REMUX_ENGINE=ffmpeg`). 비교는 스크립트 자체의 Annex B·TS 파서로 한다.

| | qcamera.ts 13 | fcamera.hevc 13 |
|---|---|---|
| 순수 ↔ 원본: 정규화 NAL SHA, 프레임 수(1,200), 키프레임 위치, parameter set, (TS) PES별 PTS/DTS | 13/13 같음 | 13/13 같음 |
| 순수 ↔ 원본: 파일 바이트 | **13/13 같음** | 13/13 다름: PPS 뒤 0 바이트 60곳(+60 B) |
| 순수 ↔ FFmpeg 역변환: 위 항목 | 13/13 같음 | 13/13 같음 |
| 순수 ↔ FFmpeg 역변환: 파일 바이트 | 5/13 같음, 8/13은 PCR 배치만 다름(아래) | **13/13 같음** |
| 디코딩 프레임 MD5(호스트 FFmpeg 6.1.1, `-f framemd5 -fps_mode passthrough`): 원본 = 순수 = FFmpeg | 13/13 | 13/13 |
| 대체 발생 | 0 | 0 |

TS에서 FFmpeg CLI 역변환과의 차이: CLI는 MP4에서 읽은 평균 frame rate로 PCR 간격을 약 100 ms로 잡아 PES 두 개에 한 번꼴로 PCR을 쓰고, 원본(Python 내보내기)과 순수 엔진은 모든 PES에 PCR을 쓴다. 그 때문에 패킷 수가 조금 다르다(영상당 +2,632 ~ +5,264 B). PES 내용·시각·PAT/PMT/SDT 수는 같다.

(b) 14구간 MP4 6개(qcamera 3, fcamera 3), 47구간 MP4 6개(qcamera 2, fcamera 2, ecamera 2), offset 0(MP4로 올라온 기록의 제품 값). 순수 ↔ FFmpeg 역변환: 정규화 NAL·프레임·키프레임·parameter set·TS 시각 12/12 같음, HEVC 7/7(fcamera 5, ecamera 2) 파일 바이트 같음, TS 5개 중 1개 바이트 같음·4개는 PCR 배치만 다름(2개는 크기도 같고 PCR 위치만 다름). 디코딩 프레임 MD5(`-fps_mode passthrough`): 순수 = FFmpeg = MP4 자체 디코딩 12/12(각 1,200프레임). 처음에 기본 constant-rate 출력으로 돌렸을 때는 MP4 쪽만 1,197·1,199프레임이 나와 3건이 달랐다: 출력 단계가 시각에 따라 프레임을 버리거나 반복한 것이고 내보내기 차이가 아니어서 passthrough로 다시 쟀다.

(c) 손상·합성·무작위 변형 300건(`integration/pure_reverse_faults.py`, 시드 20261117, `systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=0` 아래, 기본 입력 3개: 합성 H.264·HEVC MP4와 실주행 qcamera MP4 1개). 변형 10종 × 30건: 잘림, box 크기, sample 표 값, 빈 트랙, 과대 선언(sample 크기·moov 크기·config·NAL 길이), moov 비트 반전, payload 비트 반전, avcC/hvcC 개수, 가짜 box 삽입, 무작위 바이트. 결과: `invalid` 258, `verification` 5, 성공 37(payload 비트 반전 29처럼 구조가 맞는 입력), **panic 0, 비정상 종료 0, 실패 뒤 남은 출력 0**, worker 최고 RSS 37.3 MiB, 최장 0.047초(상한 256 MiB 추가·180초 대비). `verification` 5건은 avcC의 SPS를 바꾼 입력이다: 독립 판독기는 바뀐 avcC SPS를 기대 parameter set에 넣는데 sample에는 in-band SPS가 있어 쓰기가 avcC를 넣지 않으므로 일치하지 않는다(→ FFmpeg 대체). 단위 시험(`tests/pure_reverse.rs`)은 빈 파일, 쓰레기, 절반 잘림, moov만, moov 크기 초과, sample 수·크기 초과, sample 0개, mdat 앞 chunk offset, sample 밖 NAL 길이, 다른 코덱을 두 코덱 모두에서 확인한다.

(d) 제품 경로(`integration/pure_reverse_product.py`, release 실행 파일, 새 임시 데이터 폴더, loopback 18610/18611, `HARS_INGRESS_ONLY=0`, 원본 보존 끔). 13구간 원본의 3·7번 구간을 브라우저 업로드 API로 올리고 자동 변환(`unconverted → processing → ready`) 뒤 업로드 TS/HEVC가 지워진 상태에서:

| 항목 | 결과(2구간 × 2카메라) |
|---|---|
| `/api/logs/<id>/original/<kind>` 전체 | 200, `Content-Disposition` attachment, 4/4 순수(`reverse_engine` pure), 대체 0 |
| Range 앞(0–1023)·중간(4 KiB)·끝 suffix(−2048) | 12/12 206, 바이트 = 전체 응답의 같은 구간, `Content-Range` 맞음 |
| 업로드한 원본과 비교 | TS 2/2 바이트 같음, HEVC 2/2 PPS 뒤 0 바이트 60곳만 다름(정규화 NAL·프레임·키프레임 같음) |
| ZIP(`/api/logs/download.zip?…&format=original`) | 2/2 구간 3파일, 원본 형식 파일 4/4가 `/original` 응답과 SHA 같음 |
| 같은 데이터 폴더를 `HARS_REMUX_ENGINE=ffmpeg` 서버로 | 4/4 `reverse_engine` ffmpeg. 순수와 정규화 NAL·프레임·키프레임·parameter set·TS 시각 같음, HEVC 바이트 같음, TS는 PCR 배치만 다름 |

(e) 시험: `tests/pure_reverse.rs` 4개(두 엔진 결과 비교·반복 내보내기 같은 바이트, edit list 입력은 순수가 `unsupported`로 넘기고 결과가 FFmpeg 엔진의 답과 같음, 손상 MP4 11종 × 2코덱, offset·종류 검사), `src/pure_reverse/` 단위 시험 4개. 마지막 전체 실행: `cargo fmt --all --check` 통과, `cargo clippy --locked --all-targets -- -D warnings` 경고 0, `cargo test --locked --all-targets` 291 통과 0 실패(`tests/ts_duration.rs` 포함). 그 직전 첫 전체 실행에서는 기존 시험 `tests/pure_remux.rs`의 `boundary_inputs_take_the_pure_engine_or_an_explicit_fallback`(정방향, PTS 없는 TS 사례 399행)이 한 번 실패해 cargo가 멈췄고, 그 파일만 3회 다시 돌리면 1회 실패·2회 통과였다(load average 6~8, 다른 작업자 동시 실행). 이 브랜치가 고치지 않은 정방향 시험이다. 기준 커밋 `d846fc6`에서는 같은 파일 6회 모두 통과, 이 브랜치에서 그 시험만 골라 6회 돌리면 모두 통과였다. 원인으로 보이는 것: 시험의 `classify`가 프로세스 전역 카운터 `media::remux_counts()`의 증가로 "대체 실패"와 "거부"를 가르는데, 같은 시험 파일의 다른 시험이 병렬로 순수 엔진 실패를 세면 거부 사례가 대체 실패로 읽힌다(시험 자체의 경쟁). 이 해석은 코드 읽기로 낸 것이고 재현으로 확정하지 않았다. 시험은 고치지 않았다.

## 성능

`integration/benchmark_reverse.py`, 같은 release 실행 파일에서 `HARS_REMUX_ENGINE`만 바꿈, 13구간의 3·7·9번 구간 MP4(HEVC 3, TS 3; 입력 ext4, warm cache), 구현별 warmup 1회 + 순서를 회전·역순으로 바꾼 교차 5회. 제품과 같은 `reverse` 명령(worker 프로세스, 검사, fsync, rename 포함). **측정 중 load average 8.1~8.7**: 다른 작업자의 `ffmpeg` 인코딩(약 5 CPU)이 동시에 돌았다(16 CPU). 절대값은 이 부하를 포함한다.

| 구현 | TS 영상당 경과 중앙값(범위) | TS CPU | HEVC 영상당 경과 중앙값(범위) | HEVC CPU | 최고 RSS(프로세스 트리 중 최대) |
|---|---|---|---|---|---|
| FFmpeg CLI 역변환 | 0.449 s (0.428–0.520) | 0.577 s | 0.437 s (0.409–0.496) | 0.665 s | TS 54.6 MiB, HEVC 74.6 MiB |
| 순수 역변환 | **0.069 s** (0.062–0.166) | 0.064 s | **0.135 s** (0.128–0.149) | 0.129 s | TS 37.2 MiB, HEVC 36.8 MiB |
| 순수 worker 단독(`__pure-reverse`) | 0.038 s (0.033–0.047) | 0.037 s | 0.104 s (0.096–0.115) | 0.104 s | **TS 37.3 MiB, HEVC 36.7 MiB** |

순수 역변환은 경과 기준 TS 약 6.5배, HEVC 약 3.2배 빠르고 CPU는 1/9, 1/5이다. FFmpeg 경로는 `ffprobe` 1회 + `ffmpeg` 2~3회(서명 추출, 복사, 결과 서명) + TS `ffprobe` 패킷 목록 2회로 프로세스 기동과 MP4 재해석이 대부분이다. 순수 HEVC는 37 MB를 읽고 쓰고 다시 해시하는 시간(입력 판독 SHA-256, 출력 SHA-256)과 fsync가 대부분이다. 순수 worker RSS는 입력 크기와 무관하게 약 37 MiB(실행 파일 매핑 + sample 버퍼)다.

## 3단계에서 지울 것

FFmpeg를 완전히 없앨 때 이 단계가 남긴 대체 경로와 그것만 검사하는 것:

- `src/media.rs`: `reverse_ffmpeg`, `signature`(ffmpeg CLI), `reverse_using`의 대체 분기와 `reverse_fallback` 기록(순수 실패는 그대로 실패로), `probe`·`validate_upload`(`ffprobe`, 업로드·가져오기 검사의 순수 대체 필요), `input_format`.
- `tests/pure_reverse.rs`: `pure_export_holds_what_the_ffmpeg_export_holds`의 FFmpeg 엔진 비교 부분, `an_edit_list_the_engine_does_not_copy_falls_back_to_ffmpeg`의 엔진 비교(순수 `unsupported` 확인은 유지). 합성 입력 생성(`ffmpeg` CLI)은 개발 도구로 남길지 정해야 한다.
- `tests/media.rs`의 `media::reverse` 호출 두 곳은 순수 경로를 지나므로 유지 가능(합성 입력은 `ffmpeg` CLI).
- `integration/pure_reverse_audit.py`·`pure_reverse_product.py`의 `ffmpeg` 엔진 비교 단계, `benchmark_reverse.py`의 `binary:…@ffmpeg` 구현. 디코딩 MD5 oracle(호스트 `ffmpeg`)은 개발 전용으로 남길 수 있다.
- 문서: 이 문서와 [PURE-REMUX.md](PURE-REMUX.md)의 "역변환은 바뀌지 않았다"(조정자가 통합).

## 미검증과 남은 위험

- aarch64(HAOS) 이미지 크기와 역변환 실행은 미검증. 새 코드는 정수 연산과 `f64` 곱셈·반올림 한 번(offset 틱)만 쓴다.
- 실주행 B-frame MP4, field 부호화, 다중 트랙 MP4, hev1 MP4는 코퍼스에 없다. 순수 엔진은 이들을 거부하고 FFmpeg 대체로 넘긴다. 3단계에서 FFmpeg가 없어지면 이 입력의 원본 형식 내보내기는 실패한다(현재 FFmpeg 역변환도 B-frame edit list MP4는 시각 검사에서 실패한다: 시험 `an_edit_list_…`).
- avcC SPS/PPS와 sample의 in-band SPS/PPS가 다른 MP4는 `verification`으로 FFmpeg 대체에 간다(위 (c)). 3단계 뒤에는 실패다.
- AUD 없는 H.264 MP4: muxer가 AUD를 붙여 "기타 NAL" 서명이 달라지므로 두 엔진 모두 실패한다(기존 동작과 같음). 제품의 MP4는 TS에서 왔으므로 AUD가 있다.
- TS 바이트가 원본과 같은 것은 원본이 libavformat `mpegts`로 만든 Road Viewer 원본 형식 내보내기였기 때문이다. 카메라가 처음 쓴 TS(다른 muxer)라면 PES 내용·시각은 같아도 PSI·PCR 배치는 다를 수 있다(코퍼스에 없음).
- 역변환 결과는 요청마다 임시 파일로 만들고 응답 뒤 지운다(기존과 같음). 새로 넣은 fsync는 이 임시 파일에도 적용된다(성능 표에 포함).

## 재현

```sh
cargo test --test pure_reverse
cargo test --lib pure_reverse
python3 scripts/image_report.py BASE_TAG --compare NO_FFMPEG_TAG > report.json
python3 integration/pure_reverse_audit.py BINARY NEW_OUT --original13 CORPUS JOBS --sample LABEL=MP4 ...
systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=0 \
  python3 integration/pure_reverse_faults.py BINARY NEW_WORKDIR REPORT.json --cases 300 --seed 20261117 --real MP4
python3 integration/pure_reverse_product.py CORPUS BINARY NEW_OUT --ports 18610 18611 --segments 3 7
python3 integration/benchmark_reverse.py MP4_DIR OUT.json --scratch NEW_DIR --segments 3 7 9 --runs 5 \
  --impl ffmpeg=binary:BINARY@ffmpeg --impl pure=binary:BINARY@pure --impl worker=worker:BINARY
```
