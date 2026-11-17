# FFmpeg 제거 (2.0.0-alpha.4)

<!-- toc -->
**목차**

- [결론](#결론)
- [제거한 것](#제거한-것)
  - [남은 FFmpeg 참조(src/, Cargo.toml, Cargo.lock, Dockerfile)](#남은-ffmpeg-참조src-cargotoml-cargolock-dockerfile)
  - [남은 개발 전용 사용처](#남은-개발-전용-사용처)
- [이미지 전후 (amd64, 로컬)](#이미지-전후-amd64-로컬)
- [업로드 검사 대체의 대조](#업로드-검사-대체의-대조)
- [순수 엔진이 거부하는 입력](#순수-엔진이-거부하는-입력)
- [실패 분류별 동작](#실패-분류별-동작)
- [제품 경로(FFmpeg 없는 PATH)](#제품-경로ffmpeg-없는-path)
- [성능 전후](#성능-전후)
- [지운 시험과 대체 근거](#지운-시험과-대체-근거)
- [불안정 시험 2건](#불안정-시험-2건)
- [미검증과 남은 위험](#미검증과-남은-위험)
- [재현(3단계)](#재현3단계)
- [결론](#결론-1)
  - [FFmpeg 의존 지점](#ffmpeg-의존-지점)
  - [기준 이미지와 반사실 비교 (amd64)](#기준-이미지와-반사실-비교-amd64)
  - [역변환 계약](#역변환-계약)
  - [설계](#설계)
  - [검증 결과](#검증-결과)
  - [성능](#성능)
  - [3단계에서 지울 것](#3단계에서-지울-것)
  - [미검증과 남은 위험](#미검증과-남은-위험-1)
  - [재현](#재현)
<!-- /toc -->

## 결론

- 3단계(`feat/remove-ffmpeg`, 1단계 `feat/pure-reverse`·2단계 `feat/thumbnail-without-ffmpeg`를 합친 뒤): 제품 빌드와 실행에서 FFmpeg을 없앴다. `ffmpeg-next`·`ffmpeg-sys-next`(와 그것만 끌어오던 `bindgen`·`clang-sys` 등 12 crate)가 `Cargo.lock`에서 빠졌고, 실행 파일은 libc·libm·libgcc_s만 링크하며, 업로드 검사·변환·역변환·썸네일 어디서도 `ffmpeg`/`ffprobe`를 부르지 않는다. 대체 경로도 없다.
- amd64 이미지(로컬 측정): **풀린 layer 580.9 MB → 120.6 MB(−460.3 MB, −79.2 %)**, 레지스트리 blob(gzip) **217.5 MB → 46.3 MB(−171.1 MB, −78.7 %)**. aarch64는 CI 컨테이너 작업이 같은 방법으로 잰다(로컬 미측정).
- 실주행 13구간 26영상: `ffmpeg`/`ffprobe`가 없는 PATH에서 새 실행 파일로 변환해 Python 결과와 13/13 동등(15,600프레임).
- FFmpeg은 개발 전용으로만 남는다: 시험의 합성 입력 생성과 독립 대조(oracle). CI에서는 `Rust checks` 작업만 설치한다.

## 제거한 것

| 위치 | 이전 | 지금 |
|---|---|---|
| `src/native_video.rs` | `ffmpeg-next` 엔진(TS·HEVC 리먹싱), 원본 검사(`inspect`), errno 판정 | 파일 삭제. 검사는 `media::inspect_file`(TS: `pure_remux::ts::inspect`, MP4: `readback::inspect_copy`, raw HEVC: `NalHasher`) |
| `src/media.rs` `probe`/`validate_upload` | `ffprobe` CLI + `validate_info` | `media::probe_file`(`ts/hevc/readback::probe`, 순수 Rust, 상한 있는 읽기) + 같은 `validate_info` |
| `src/media.rs` `signature`, `reverse_ffmpeg`, `ffmpeg`, `raw_signature`, `log_fallback`, `falls_back`, `Engine`, `remux_counts` | FFmpeg CLI 서명·역변환, 대체 분기, 엔진 선택, 전역 카운터 | 삭제. `remux_using`/`reverse`는 순수 엔진만 |
| `src/main.rs` | `__native-hevc`, `__native-ts` worker | 삭제. `__inspect`는 순수 검사 |
| `Cargo.toml`/`Cargo.lock` | `ffmpeg-next` | 삭제(`cargo update` 없음; lock은 117줄 삭제만) |
| `Dockerfile` | 빌더 `pkg-config libclang-dev libavformat-dev libavcodec-dev libavutil-dev libswscale-dev`, 런타임 `ffmpeg` | 빌더 `capnproto`만, 런타임 `nginx ca-certificates`만, 실행 파일 `strip` |
| `.github/workflows/ci.yml`, `prerelease.yml` | libav dev 4종·pkg-config·libclang 설치 | `capnproto ffmpeg`만(시험용). 컨테이너 작업에 FFmpeg 부재 검사와 크기 측정(job summary, artifact) |
| `scripts/container_smoke.py` | 이미지 안의 `ffmpeg`으로 입력 생성 | 저장소의 합성 클립(`scripts/smoke/`)으로 이미지 안에서 변환(TS·HEVC)·원본 형식 내보내기·썸네일·서명 대조, `ffmpeg`/`ffprobe`/libav 부재 확인 |

`HARS_REMUX_ENGINE`: 없거나 `pure`면 그대로, `ffmpeg`이면 경고(`remux_engine_setting`)를 남기고 순수 엔진으로 시작, 그 밖의 값은 시작 실패(이전과 같음). `roadviewer_hars/config.yaml`에는 이 설정이 없다(확인). `scripts/check_release.py`·`check_coexistence.py`는 FFmpeg을 전제하지 않아 바꾸지 않았다.

### 남은 FFmpeg 참조(`src/`, `Cargo.toml`, `Cargo.lock`, `Dockerfile`)

`Cargo.toml`·`Cargo.lock`·`Dockerfile`: 0. `src/`의 남은 것은 모두 이유가 있다:

- `src/media.rs`: `HARS_REMUX_ENGINE=ffmpeg`을 알아보고 경고하는 코드와 그 메시지(값 자체가 이전 버전의 설정이다).
- `src/pure_reverse/ts.rs`: TS SDT의 제공자 이름 바이트 `b"FFmpeg"`. 원본 TS가 가진 값이고, 바꾸면 내보내기가 원본과 바이트가 달라진다.
- 동작을 맞춘 기준 구현의 이름을 적은 주석(`pure_remux/annexb.rs`, `ts.rs`, `hevc.rs`, `readback.rs`, `pure_reverse.rs`, `pure_reverse/annexb.rs`·`ts.rs`, `thumbnail/*`): Python 참조가 쓰는 FFmpeg 라이브러리의 함수·규칙 이름(`ff_nal_find_startcode`, libswscale의 상수 등)이다. 무엇을 바이트 단위로 재현하는지 알려 주므로 남겼다. 대체 경로를 전제한 주석은 모두 고쳤다.
- `cargo tree`에 `ffmpeg-next`/`ffmpeg-sys-next`/`bindgen`/`clang-sys` 없음. `pkg-config` crate는 `zstd-sys`의 build-dependency로 남지만 기본 기능에서는 호출하지 않는다(번들 zstd를 `cc`로 빌드). 그래서 이미지 빌더에 pkg-config 패키지가 필요 없다(빌드로 확인).

### 남은 개발 전용 사용처

`tests/*.rs`(합성 입력, `ffprobe` 패킷 표·`ffmpeg -f framemd5` 디코딩 대조, `tests/pure_reverse.rs`의 CLI 내보내기 oracle), `integration/*.py`(대조·측정), `scripts/container_smoke.py` 머리말의 클립 생성 명령, `scripts/image_report.py`(이미지 안 FFmpeg 파일 탐지). `integration/benchmark_*.py`의 `@ENGINE`과 `pure_reverse_audit.py`·`boundary_inputs.py`의 FFmpeg 엔진 단계는 alpha.3 실행 파일과의 비교에만 쓸 수 있다(새 실행 파일에서는 `ffmpeg` 값이 순수 엔진으로 간다). `pure_default_product.py`의 `ffmpeg` 시나리오와 `pure_reverse_product.py`의 FFmpeg 서버 단계는 지웠다.

## 이미지 전후 (amd64, 로컬)

방법: 같은 기계에서 `docker build --build-arg BUILD_ARCH=amd64 -t hars-a4-remove:new .` 한 번, `scripts/image_report.py hars-a4-remove:new --require-no-ffmpeg`. 기준은 [ffmpeg-image-amd64-2026-11-17.json](measurements/ffmpeg-image-amd64-2026-11-17.json)(`d846fc6`), 지금은 [no-ffmpeg-image-amd64-2026-11-17.json](measurements/no-ffmpeg-image-amd64-2026-11-17.json). 게시·push 없음.

| | `d846fc6` | FFmpeg 패키지만 뺀 반사실(1단계) | 지금 | 줄어든 양(기준 대비) |
|---|---|---|---|---|
| 풀린 layer 합 | 580,879,872 | 125,169,152 | **120,607,744** | 460,272,128 (79.2 %) |
| 레지스트리 layer blob(gzip) 합 | 217,472,878 | 46,508,246 | **46,332,953** | 171,139,925 (78.7 %) |
| layer별 gzip -6 합 | 212,837,475 | — | 46,053,061 | 166,784,414 (78.4 %) |
| `docker save \| gzip -6` | 214,888,770 | 46,140,685 | 45,979,882 | 168,908,888 (78.6 %) |
| `docker image inspect` Size | 813,379,464 | 180,586,727 | 175,848,490 | (blob 포함 값, 참고) |
| 설치 패키지 | 299 | 107 | 107 | 192 |
| 실행 파일 | 25,307,136(symtab 있음) | 같음 | 20,745,232(strip) | 4,561,904 |
| 실행 파일 링크 | libav 4종 포함 | 같음 | libc·libm·libgcc_s·ld-linux·vdso | |
| 이미지 안 FFmpeg 파일·라이브러리·패키지 | 있음 | 실행 파일이 libav 링크 | **없음**(`ffmpeg_found` 비어 있음) | |

layer: 기반 debian 77.9 MB(그대로), apt 17.5 MB, 실행 파일 20.7 MB, 웹 자산 4.4 MB.

strip: 같은 실행 파일(로컬 release)이 27,166,832 → 20,724,744바이트(−6.4 MB), gzip -6 9,371,956 → 8,497,026(−0.87 MB, 레지스트리 전송 몫). 진단 영향: panic 메시지와 소스 위치는 그대로 출력되고, `RUST_BACKTRACE=1` backtrace에서 함수 이름이 주소로 바뀐다(worker의 stderr는 원래 버린다). strip 전 실행 파일이 `d846fc6`보다 큰 것(+1.9 MB)은 순수 썸네일 디코더·역변환이 들어오고 FFmpeg 바인딩이 빠진 결과다.

측정 가능한 추가 절감 여지(적용 안 함, 추정): 기반 이미지가 77.9 MB로 이제 가장 크다. distroless/static이나 musl 정적 빌드면 기반 몫 대부분과 apt layer 일부가 줄 수 있으나(nginx·ca-certificates는 여전히 필요) 지시대로 바꾸지 않았다. `opt-level="s"`/LTO는 재지 않았다.

## 업로드 검사 대체의 대조

`integration/upload_check_compare.py`: 같은 파일을 `d846fc6` 실행 파일(`ffprobe` 검사)과 새 실행 파일의 서버에 브라우저 업로드로 올려 `invalid_file` 여부를, 그리고 두 실행 파일의 `__inspect`(변환 전 원본 검사) 종료 상태를 비교했다. 입력: 13구간 26영상(TS 13, HEVC 13), 14구간 MP4 6개(qcamera 3, fcamera 3), 47구간 MP4 6개(qcamera·fcamera·ecamera 각 2), 합성·손상 100건(시드 20261117: 성질별 합성 20건 + 4개 기본 입력의 무작위 손상 80건). `systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=0` 아래, 시간 초과 0. 결과: [upload-check-compare-2026-11-17.json](measurements/upload-check-compare-2026-11-17.json).

| | 둘 다 수락 | 둘 다 거부 | **새 검사만 수락** | 새 검사만 거부 |
|---|---|---|---|---|
| 업로드 검사(`.hevc`·`.mp4`만, 제품과 같음) 104건 | 78 | 20 | **0** | 4 |
| 원본 검사(`__inspect`) 138건 | 85 | 43 | **0** | 10 |

최종 코드(`fix/a4-review`: SPS 크기 판정, `__probe` worker)로 다시 돌린 결과다. 처음 대조(원본 검사 83/43/0/12)와 갈린 것은 두 건뿐이다: 합성 `hev1.mp4`(그 대조 뒤 수락하도록 넓힘), 무작위 손상 `damaged-078-head-flip-h264.mp4`(앞부분 비트 반전이 sample entry 크기만 바꾼 파일. 이전에는 `stsd` 크기로 거부했고, 지금은 `ffprobe`처럼 SPS 크기로 판정해 양쪽 수락).

실주행 38개는 모두 양쪽 수락(업로드 25, 검사 38). 갈린 것:

- 업로드에서 새로 거부(4, 모두 무작위 손상 MP4): `truncated MP4 box` 2(moov 안 box 크기가 부모를 넘음), `inconsistent MP4 sample table` 1, moov를 못 찾음 1. `ffprobe`는 이들을 경고와 함께 받았다.
- 원본 검사에서 새로 거부(10): 손상 TS 5(PSI의 stuffing·CRC 오류, 알 수 없는 PID, 파일 앞 쓰레기 바이트로 동기 바이트 불일치: FFmpeg은 재동기화하며 넘어간다), 손상 MP4 4(위 종류 + `truncated MP4 sample`, 손상된 sample entry의 `camera codec mismatch`), 합성 `bframes.mp4`(media를 미루는 edit list, 아래 표).
- MP4 크기: 업로드 검사와 원본 검사는 sample entry(`stsd`)의 폭·높이가 아니라 `avcC`/`hvcC`의 첫 SPS(hev1/avc3에 없으면 첫 sample의 SPS)에서 crop을 반영한 크기를 읽는다(`annexb::picture_size`, TS·HEVC 검사와 같은 코드). `ffprobe`(디코더)가 SPS 크기를 쓰기 때문이다. 실측: 14구간 MP4 28개 전부와 47구간 MP4 12개(카메라별 4)에서 `stsd`와 SPS 크기가 40/40 같았다([mp4-stsd-sps-size-2026-11-17.json](measurements/mp4-stsd-sps-size-2026-11-17.json)). 그래서 다를 때는 SPS로만 판정한다(정상 파일을 새로 거부하지 않음). 엔진이 쓴 MP4의 다시 읽기만은 두 값이 같아야 한다(같은 SPS에서 쓰므로). 시험: `tests/media.rs` `mp4_size_is_the_sps_size_not_the_sample_entry_size`(4224×96 H.264·HEVC의 `stsd`만 1920×1080으로 고친 파일 거부, 160×96의 `stsd`를 8000×8000으로 고친 파일은 160×96으로 수락).
- 처음 대조에서 raw HEVC 4112 폭이 새 원본 검사만 통과했다(엔진은 나중에 거부). `inspect_file`이 모든 종류에 `validate_info`를 적용하게 고쳐 지금은 양쪽 거부다.

## 순수 엔진이 거부하는 입력

(가) alpha.3 = `d846fc6`(순수 기본 + FFmpeg 대체), (나) Python = `reference/python` 코드, (다) 지금. "실측"은 이번에 돌린 것, "기록"은 1·2단계 문서·시험의 실측, "코드"는 코드를 읽고 낸 판단(추정)이다. 모든 경우 원본은 지워지지 않고(원본 보존 끔이어도 MP4를 만든 카메라의 원본만 지운다), 게시된 결과가 그 카메라를 재생하면 그대로 유지된다.

| 입력 | (가) alpha.3 | (나) Python | (다) 지금 | 근거 |
|---|---|---|---|---|
| B-frame H.264 TS | 순수 거부 → FFmpeg 게시 | 게시(코드: 재정렬 검사 없음) | `unsupported`, 그 카메라 없이 `ready` + 경고 | 실측(제품 경로, 시험) |
| H.264 TS의 interlaced(field) SPS | FFmpeg 게시(기록) | 게시(코드) | `unsupported` | 코드(실제 field 입력은 만들 도구 없음, 미검증) |
| H.264 SPS scaling matrix | FFmpeg 게시(기록) | 게시(코드) | `unsupported` | 코드, 미검증 |
| 한 PES에 picture 여러 개인 TS | FFmpeg 게시(기록) | 게시(코드) | `unsupported` | 코드 |
| TS 패킷 손실(연속성), transport error, PTS 없는 PES | 원본 검사에서 거부 | 거부(기록: 손상 패킷) | `invalid`, 그 카메라 없이 `ready` | 실측(시험, 제품 경로: 경고는 `native media validation failed (exit status: 1)`) |
| PSI CRC·stuffing 오류, 앞에 쓰레기 바이트가 있는 TS | 검사 통과 → 순수 실패 → FFmpeg 게시(추정) | 게시(추정) | `invalid` | 실측(대조 5건) |
| scrambling 표시가 선 TS | 순수 거부 → FFmpeg 게시 | 게시(기록) | **넓힘: 게시**(표시 무시, 판독 검사 통과) | 실측(시험, 제품 경로) |
| 영상이 아닌 PID 패킷에서 잘린 TS, 동기 바이트만 남은 마지막 패킷 | 순수 거부 → FFmpeg 게시 | 게시(기록) | **넓힘: 게시**(188 B 미만 마지막 패킷 제외) | 실측(시험) |
| 영상 payload 안에서 잘린 TS | 원본 검사에서 거부 | 게시(마지막 프레임 손실, 기록) | `invalid` | 실측(시험) |
| HEVC parameter set 중간 변경 | 순수 거부 → FFmpeg도 실패 | 게시(코드) | `unsupported` | 실측(시험, 제품 경로) |
| HEVC B-frame(reorder) | 순수 거부 → FFmpeg도 거부 | 거부(코드: `has_b_frames`) | `unsupported` | 코드·시험 |
| MP4 입력: media를 미루는 edit list(B-frame MP4) | 변환 수락(FFmpeg 검사), 역변환 실패 | 게시(코드: MP4를 그대로 씀) | 변환·역변환 `unsupported` | 실측(`tests/media.rs`, `tests/pure_reverse.rs`) |
| MP4 입력: 오디오 등 다른 트랙 | 수락 | 수락(코드) | **넓힘: 수락**(영상 트랙 하나만 읽음), 역변환은 `invalid`(트랙 1개만) | 실측(시험), 역변환은 코드 |
| MP4 입력: moov가 mdat 뒤 | 수락 | 수락(코드) | **넓힘: 수락** | 실측(시험) |
| MP4 입력: hev1/avc3 | 수락 | 수락(코드) | **넓힘: 변환 수락**, 역변환은 `unsupported`(코드) | 실측(시험) |
| MP4 입력: 길이 필드 4바이트 아닌 avcC/hvcC, 여러 sample entry | 수락 | 수락(코드) | `invalid` | 코드 |
| MP4 역변환: avcC SPS/PPS와 sample 안 SPS/PPS가 다름 | 순수 `verification` → FFmpeg | 게시(코드) | `verification`, `media_export_failed` | 기록(1단계 손상 300건 중 5건) |
| MP4 역변환: 출력 4 GiB 초과, 33비트 넘는 시각 | FFmpeg | 게시(코드) | `unsupported` | 코드 |
| 손상 MP4(box·sample 표) | 업로드 수락(경고) → 변환 실패 다수 | — | 업로드에서 거부될 수 있음(대조 4건) | 실측 |
| 썸네일: H.264 4:2:2/4:4:4/10비트/interlaced, HEVC Main10/4:4:4 | 썸네일 생성 | 생성(코드) | 그 카메라 썸네일 없음(다음 카메라, 모두 안 되면 `thumbnail.failed`), 변환은 영향 없음 | 기록([THUMBNAIL-DECODER.md](THUMBNAIL-DECODER.md)) |
| 저공간(입력 × 2 + 100 MiB 미만) | FFmpeg이 MP4 하나만 씀 | 게시(Python은 공간 검사 없음) | `resource`/`low_space`, 게시 결과가 있으면 유지 | 실측(`low_space_tmpfs.sh`, 시험) |

comma 카메라 원본(코퍼스 26영상, 14·47구간 MP4 12개)은 위 어느 경우에도 해당하지 않는다. 호환성이 줄어드는 범위는 위 표에서 "(다)"가 실패인 줄이다.

## 실패 분류별 동작

어느 분류도 다른 엔진으로 다시 시도하지 않는다. 자세한 표는 [PURE-REMUX.md](PURE-REMUX.md#실패-분류와-대체-정책).

| 분류 | 로그 | 기록 상태(게시된 결과 없음) | 게시된 결과가 그 카메라를 재생할 때 |
|---|---|---|---|
| `unsupported`, `invalid` | `remux_failure` WARN | 그 카메라 없이 `ready`, `<카메라> 영상 준비 실패: …` | **그 카메라만 이전 결과를 이어받고 나머지 카메라는 변환·게시**: 이전 MP4와 summary 항목을 새 결과에 넣고 `<카메라> 영상 준비 실패: … (이전 변환 결과 유지)` 경고, `conversion_carried_previous` WARN (**새로 보호**: alpha.3은 FFmpeg 대체 뒤 실패면 카메라를 뺐다) |
| `internal`, `verification` | `remux_failure` ERROR | 같음 | 같음(새로 보호) |
| `resource`(`low_space` 포함) | `remux_failure` WARN | `… resource (<code>): …` 경고 | 이번 시도 전체를 버리고 이전 결과로 `ready` + `(이전 변환 결과 유지)` 경고, `conversion_kept_previous` ERROR(alpha.3과 같음) |

자원 실패는 공간·메모리·시한이 풀리면 다음 시도에 통과하므로 시도 전체를 멈추고 이전 결과를 지킨다. 자원이 아닌 실패는 같은 입력이면 매번 같으므로, 시도 전체를 멈추면 나중에 올라온 카메라(예: alpha.3에서 FFmpeg 대체로 게시된 qcamera가 있는 기록에 추가된 fcamera)가 영영 게시되지 않는다. 그래서 그 카메라만 이어받는다.

이어받기(`convert::carry_previous`):

- 재생 여부는 디스크로만 판단한다(`prepared/summary.json`에 그 카메라가 있고 MP4가 `prepared/`, 원본 옆, 보관소 중 한 곳에 있음). `meta.videos`는 보지 않는다.
- `prepared/`의 MP4는 새 staging에 hard link로(안 되면 복사 후 fsync) 이름을 하나 더 만든다. 이전 파일은 쓰지 않는다. staging이 `prepared/`로 바뀌기 전에 죽으면 시작 시 staging만 지워지고 이전 `prepared/`가 그대로 남는다. 바뀐 뒤에는 기존 발행 순서(이전 `prepared/` → `.previous-<세대>`, staging → `prepared/`, 메타 저장, 일지 삭제, `.previous-` 삭제)를 그대로 따르므로 이전 결과와 새 결과 중 하나가 온전히 남는다. hard link라 `.previous-` 삭제는 이름 하나를 지울 뿐이다.
- 원본 옆 MP4와 보관된 MP4는 그 자리에 둔다(새 결과는 summary 항목만 가진다).
- summary 항목의 `start`는 이전 변환의 시작 기준이므로 이전 `data.json.gz`의 `logStart`로 같은 로그 시각에 다시 맞춘다. 나머지 필드(`frames`, `duration`, `width`, `height`, `codec`, `sourcePts`, `timestampSpreadMs`)는 그대로다.
- 이어받은 MP4 목록은 발행 일지(`.conversion.json`의 `carried`)에만 적는다(`prepared/`의 파일 구성은 그대로). 발행은 원본 보존을 꺼도 이어받은 카메라의 MP4를 원본 옆으로 옮기지 않고 그 원본을 지우지 않는다.
- 이어받기를 할 수 없으면(이전 `data.json.gz`를 못 읽음, 링크·복사 실패) 자원 실패와 같이 시도 전체를 멈추고 이전 결과를 유지한다(`conversion_carry_failed` WARN).
- 같은 변환을 다시 돌리면 같은 상태로 수렴한다(시험).
- 장치 업로드·브라우저 업로드로 카메라를 추가하면 `clear_prepared`가 `prepared/`를 먼저 지우므로(Python과 같음) 이 경로에서는 이어받을 게시 결과가 없다: 거부되는 카메라는 경고와 함께 빠진다. 이어받기는 `prepared/`를 남긴 채 다시 변환하는 경로(예: 변환 중 재시작된 기록의 재대기)에서 일어난다.

확인: `tests/conversion_keep_previous.rs` `a_camera_the_engine_refuses_keeps_its_published_video_and_a_new_camera_publishes`(원본 보존 켬·끔, 이어받은 MP4 바이트·Range 206, 새 카메라 게시, 로그 시각 정렬 ±2 µs, 원본 그대로, 2회째 같은 상태)와 기존 자원 실패 시험.

원본 형식 내보내기 실패는 같은 분류의 `reverse_failure`와 HTTP `media_export_failed`이고, 원본·MP4·게시 결과는 바뀌지 않는다. 확인: `tests/pure_remux_faults.rs`(분류별 이벤트, 남는 파일 없음, 이전 출력 유지), `tests/conversion_keep_previous.rs`, 제품 경로(아래).

## 제품 경로(FFmpeg 없는 PATH)

`integration/no_ffmpeg_product.py`, release 실행 파일, 새 임시 데이터 폴더, 127.0.0.1:18654/18655, `HARS_INGRESS_ONLY=0`, 서버 PATH에 `nice`·`ionice`·셸 도구만(ffmpeg·ffprobe 없음 확인), 원본 보존 끔, 자동 변환.

- 13구간 원본의 3·7번 구간: `ready`, 두 카메라, Range 206(앞·끝 1 KiB = 다운로드), 썸네일 200 JPEG, `/original/qcamera.ts` = 업로드 원본 바이트, `/original/fcamera.hevc` 200(PPS 뒤 0 바이트 차이, 1단계와 같음), ZIP 3파일.
- 거부 입력(각각 다른 구간에서 합성): B-frame TS → front만 `ready` + `unsupported` 경고, qcamera 원본 그대로. 패킷 손실 TS → front만, 경고, 원본 그대로. scrambling TS → 두 카메라 `ready`(넓힘). parameter set 중간 변경 HEVC → qcamera만 + `unsupported` 경고, fcamera 원본 그대로. 모두 `error` 아님.
- 컨테이너(`scripts/container_smoke.py hars-a4-remove:new`): 통과.
- 만들지 못해 미검증: interlaced·scaling matrix TS, 4 GiB 초과 내보내기, avcC/in-band SPS 불일치 MP4의 제품 경로.
- 최종 코드(`fix/a4-review`) 재확인: 같은 스크립트로 3번 구간과 거부 입력 4종이 위와 같은 결과(Range 206·앞뒤 1 KiB 같음, 썸네일 200 JPEG, `/original/qcamera.ts` = 업로드 바이트, `/original/fcamera.hevc` 200, ZIP 3파일). 이어받기(release 서버, ffmpeg 없는 PATH, 원본 보존 켬, 5·6번 구간): qcamera만 게시한 뒤 qcamera 원본을 B-frame TS로 바꿔 둔 기록에 (가) 카메라 추가 업로드로 fcamera를 올리면 `clear_prepared`가 이전 결과를 먼저 지우므로 front만 `ready` + `unsupported` 경고(qcamera 원본 그대로), (나) fcamera를 두고 기록을 `processing`(변환 중 죽은 서버가 남기는 상태)으로 둔 채 다시 시작하면 front 게시 + qcamera 이어받기(`… (이전 변환 결과 유지)` 경고, `prepared/camera.mp4` 바이트 그대로, 두 카메라 Range 206, 썸네일 200). (나)의 디스크 준비는 손으로 했다.

## 성능 전후

같은 기계, `uptime` 1.45(측정 전)·2.13(후), 다른 빌드·시험 없음. 13구간의 3·7·9번 구간 전체 변환(`convert-log`, 로그 분석·리먹싱·검증·텔레메트리·썸네일), 실행 파일별 warmup 1회 + 순서를 바꾼 교차 5회. 숫자: [no-ffmpeg-performance-2026-11-17.json](measurements/no-ffmpeg-performance-2026-11-17.json).

| 구간 | alpha.3(`d846fc6`) 중앙값(범위) s | 지금 중앙값(범위) s |
|---|---|---|
| 3 | 2.568 (2.537–2.703) | 2.568 (2.459–2.827) |
| 7 | 2.672 (2.604–2.754) | 2.548 (2.505–2.572) |
| 9 | 2.574 (2.539–2.673) | 2.572 (2.446–2.585) |
| 합 | 7.813 | 7.688 (−1.6 %, 범위 안) |

업로드 검사: 실주행 fcamera.hevc(37.7 MB) 한 파일 업로드의 finish 요청(SHA-256 + 미디어 검사), 교차 5회: alpha.3 0.135 s(0.132–0.139), 서버 안 검사 0.064 s(0.063–0.066). `ffprobe` 검사만은 평균 0.083 s였다(프로세스 기동 포함). 순수 검사는 첫 그림까지만 읽는다.

업로드 검사의 격리: alpha.3의 `ffprobe`처럼 검사는 서버 밖 worker 프로세스 `roadviewer-hars __probe FILE KIND`에서 돈다. 다른 media worker와 같은 `media::run`(동시 2개, 180초, 응답 64 MiB, `kill_on_drop`, `PR_SET_PDEATHSIG`)과 메모리 상한(`RLIMIT_AS` = 현재 + 256 MiB)이다. worker가 죽거나(할당 실패의 abort, OOM killer) 시한을 넘기면 그 파일은 `invalid_file`이고 서버는 계속 돈다(시험 `tests/upload_crash.rs` `a_dead_upload_check_worker_refuses_the_file_and_the_server_goes_on`). 라이브러리 내장(시험) 경로만 `spawn_blocking` + `catch_unwind`다. 늘어난 시간(finish 요청, release, warmup 1 + 교차 5회 중앙값, load average 5.3→5.0): fcamera.hevc 37.7 MB 0.0677 → 0.0735 s(+5.8 ms), qcamera.mp4 1.9 MB 0.0075 → 0.0105 s(+3.1 ms), fcamera.mp4 37.4 MB 0.0632 → 0.0674 s(+4.2 ms). 숫자: [upload-check-worker-2026-11-17.json](measurements/upload-check-worker-2026-11-17.json).

## 지운 시험과 대체 근거

삭제·약화한 단언은 FFmpeg 엔진의 출력이나 대체 결과에 관한 것뿐이다. 나머지 단언은 그대로이고, FFmpeg 엔진과의 비교는 독립 판독기(호스트 `ffprobe`, `shiguredo_mp4` demuxer)나 규칙 값과의 비교로 바꿨다.

| 시험(이전 이름 → 지금) | 지운 것 | 그 성질을 지금 지키는 것 |
|---|---|---|
| `pure_remux.rs` `hevc_pure_output_equals_ffmpeg_engine_except_container_extras` → `hevc_pure_output_holds_the_input_and_the_logged_clock` | FFmpeg 엔진 출력과 packets·필드·sample·decoder configuration·durations 비교 | 호스트 `ffprobe`의 packets·필드 = 엔진 info, `validate_times`, duration 948,000, durations = 로그 간격 + 48 ms, `shiguredo_mp4` sample 길이·키프레임, 서명, 디코딩 10/10 |
| `h264_ts_pure_output_equals_ffmpeg_engine` → `h264_ts_pure_output_holds_the_input_and_its_clock` | FFmpeg 엔진 출력 비교 | 호스트 `ffprobe`의 원본 TS packets − 첫 PTS = 출력 packets = 엔진 info, durations = 원본 DTS 간격, sample 10개, 서명, 디코딩 10/10 |
| `b_frame_ts_falls_back_to_ffmpeg` → `b_frame_ts_is_refused_as_unsupported_and_leaves_nothing` | FFmpeg 사본의 디코딩 | 순수 거부 분류·이유, 남는 파일 없음 |
| `boundary_inputs_take_the_pure_engine_or_an_explicit_fallback` → `…_or_fail_with_their_class` | 대체 결과(`Fallback`) | 각 입력의 결과 분류·코드(호출 결과로 판정) |
| `end_of_sequence_units_split_like_ffmpeg` → `end_of_sequence_units_open_the_next_sample` | FFmpeg 엔진과 sample 비교 | EOS가 6번째 sample을 엶, 디코딩 10/10 |
| `engines_keep_the_zero_bytes_after_units_alike` → `the_zero_bytes_after_units_stay_in_the_sample` | FFmpeg 엔진과 sample 비교 | 모든 sample 끝 0 3바이트 유지, 디코딩 10/10 |
| `damaged_inputs_end_verified_or_failed` | `Fallback(…, true)` 사례 | `Failed(class)`; 잘린 비영상 패킷 2건은 이제 `Pure`(디코딩 10) |
| `pure_remux_faults.rs` `engine_errors_and_panics_fall_back_once_as_error_events` → `…_fail_as_error_events_and_publish_nothing` | FFmpeg 사본 게시 | ERROR `remux_failure`, 출력 없음, 재시도 완료 |
| `a_file_that_does_not_read_back_falls_back_or_fails_with_both_kept_safe` → `…_fails_and_the_previous_result_stays` | FFmpeg 사본·`ffmpeg_remux_corrupt` | 검증 실패, 이미 있던 같은 이름의 출력이 바이트 그대로 |
| `memory_limit_…` 큰 NAL 부분, `low_space_selects_the_single_copy_engine` → `low_space_is_a_resource_failure_before_any_write` | 대체 이벤트 | `remux_failure` 분류·코드·수준, 남는 파일 없음, 충분한 공간이면 게시 |
| `pure_reverse.rs` `pure_export_holds_what_the_ffmpeg_export_holds` → `…_what_the_cli_export_holds` | FFmpeg 엔진 내보내기 | 호스트 `ffmpeg` CLI로 같은 명령의 내보내기를 oracle로 비교(HEVC 바이트 같음, TS 서명·시각 같음) |
| `an_edit_list_the_engine_does_not_copy_falls_back_to_ffmpeg` → `…_is_refused_as_unsupported` | 두 엔진의 같은 답 | `unsupported`, 출력 없음 |
| `media.rs` `native_inspection_…` → `inspection_and_mux_hash_match_independent_ffmpeg_extraction` | `native_video::remux_hevc` 서명 | `pure_remux::remux`의 입력 서명 = 원본 서명; `media::probe`(ffprobe) → 호스트 `ffprobe` |
| `terminal_duration.rs` `both`, `ts_clocks_follow_the_same_rule_in_both_engines` → `end`, `ts_clocks_follow_the_rule` | FFmpeg 엔진 실행과 비교 | 같은 규칙 값, `ffprobe` packet duration |
| `ts_duration.rs` 두 시험 | `ffmpeg_next` 라이브러리 호출, FFmpeg 엔진 | `ffprobe -scan_all_pmts 0`(라이브러리 기본값)으로 같은 모델 검사, 순수 출력의 `ffprobe` duration |
| `src/pure_remux/mp4.rs` 단위 시험 | `falls_back` 단언 | 분류 `unsupported` 단언 유지 |

새 시험: `tests/media.rs` `other_muxers_mp4_inputs_are_read_or_refused_explicitly`, `mp4_size_is_the_sps_size_not_the_sample_entry_size`, `a_damaged_ts_packet_is_refused_without_a_build_path`, `tests/conversion_keep_previous.rs` `a_camera_the_engine_refuses_keeps_its_published_video_and_a_new_camera_publishes`, `tests/upload_crash.rs` `a_dead_upload_check_worker_refuses_the_file_and_the_server_goes_on`.

## 불안정 시험 2건

1. `tests/pure_remux.rs` `boundary_inputs_…`: 원인은 판정이 프로세스 전역 카운터(`media::remux_counts`)의 증가로 "대체 실패"와 "거부"를 가른 것이다. 같은 바이너리의 다른 시험이 병렬로 순수 실패를 세면 오판한다. 대체 경로와 함께 카운터를 지우고, 판정을 그 호출의 결과(`media::failure`의 분류·코드)로 바꿨다. 수정 뒤 이 파일 50회 연속 통과(0 실패).
2. `tests/conversion_keep_previous.rs` `a_resource_failure_keeps_the_published_result_and_a_later_conversion_completes`(206 대신 409): 재현 20회씩, `d846fc6` 2/20 실패, 합친 브랜치(`58842aa`) 11/19 실패(20회째는 다른 측정과 겹쳐 기록 안 됨). 원인: 시험은 `meta.json` 파일이 `ready`가 되기를 기다리는데, `store::save_meta`는 파일을 rename하고 폴더를 fsync한 **뒤에** 메모리의 기록을 바꾸고, 영상 라우트는 메모리의 기록으로 `ready`가 아니면 409를 답한다. 그 사이(수 ms)에 Range 요청이 들어간 경우다. 클라이언트가 보는 것은 메모리 기록뿐이고 디스크가 먼저인 순서는 내구성상 맞으므로 제품 결함이 아니라 시험의 경쟁이다. 시험이 재생을 확인하기 전에 서버 자신이 `/api/logs`에서 `ready`를 답할 때까지(10초 상한) 기다리게 했다. 206·바이트 단언은 그대로다. 수정 뒤 이 파일 14회 연속 통과(0 실패). 50회는 시간 상한 때문에 채우지 못했다(다른 측정과 겹쳐 1회 약 2분).

## 미검증과 남은 위험

- aarch64 이미지 크기·FFmpeg 부재·변환은 로컬 미검증(CI 컨테이너 작업이 검사한다).
- TS 원본 검사의 독립성: 이전에는 FFmpeg 판독과 순수 demuxer가 따로 TS를 읽어 서로 대조했다. 이제 원본 검사와 엔진이 같은 `mpeg2ts` demuxer를 쓰므로 "입력 서명 일치" 검사는 같은 코드끼리의 비교다. 출력 MP4는 여전히 엔진과 코드를 공유하지 않는 판독기(`readback`)로 확인하고, 시험·코퍼스 대조는 호스트 FFmpeg(개발 전용)으로 한다. 별도의 TS 판독기를 두는 것은 하지 않았다.
- 원본 검사 worker의 실패 이유가 사용자 경고에 나오지 않는다(`native media validation failed (exit status: 1)`, alpha.3과 같음). `mpeg2ts` 오류 문자열 끝의 소스 위치(빌드 기계의 `~/.cargo/registry/...` 경로, alpha.3부터)는 이제 떼어 낸다(`ts::packet_error`, 시험 `a_damaged_ts_packet_is_refused_without_a_build_path`). 원본 검사 worker의 실패 경고는 종료 상태만 담지만, 엔진 worker의 실패 경고는 실패 이유를 담는다(제품 경로 실측: `qcamera 영상 준비 실패: unsupported: reading the camera original: unsupported by pure remux: H.264 picture reordering in TS`). 같은 `mpeg2ts` demuxer의 원본 검사가 먼저 실패하므로 TS 패킷 오류가 엔진 단계까지 가는 일은 코드상 드물지만, 그때는 경로가 기록의 `warnings`(API 응답)에 들어갈 수 있었다. 이제 그 경로가 없다. 로그(`remux_failure`의 `reason`)와 worker stderr도 같다. `shiguredo_mp4` 오류는 `(at src/…:줄)`처럼 crate 안 상대 경로만 남긴다(빌드 기계 경로 아님).
- 업로드 검사는 첫 그림까지만 읽는다: 첫 access unit에 SPS/PPS(HEVC는 VPS 포함)가 없는 원본은 거부한다(`ffprobe`는 뒤에서 찾으면 받았다). 대조에서 이런 실입력은 없었다.
- interlaced·scaling matrix TS, PAFF MP4, 4 GiB 초과 내보내기는 실제 입력으로 확인하지 못했다.
- 역변환은 hev1·다중 트랙 MP4를 받지 않는다(변환은 받는다).
- integration의 alpha.3 비교 도구(`pure_reverse_audit.py`, `boundary_inputs.py`, `benchmark_*`)의 FFmpeg 엔진 단계는 정리하지 않았다(alpha.3 실행 파일이 있어야 의미가 있다).

## 재현(3단계)

```sh
python3 scripts/image_report.py TAG --require-no-ffmpeg --summary summary.md > report.json
python3 scripts/container_smoke.py TAG
systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=0 python3 integration/upload_check_compare.py \
  ALPHA3_BINARY NEW_BINARY NEW_WORK report.json --ports P1 P2 P3 P4 --real KIND=FILE ... --synthetic 100 --seed 20261117
bash integration/low_space_tmpfs.sh RELEASE_BINARY DEBUG_BINARY
python3 integration/no_ffmpeg_product.py CORPUS BINARY NEW_OUT --ports UI DEVICE --path DIR_WITHOUT_FFMPEG
PATH=DIR_WITHOUT_FFMPEG python integration/real_conversion_compare.py CORPUS BINARY ORACLE NEW_OUT --limit 13
```

---

# 1단계 기록: 기준 측정과 순수 Rust 역변환

아래는 1단계(`feat/pure-reverse`) 당시의 기록이다. "FFmpeg 대체", "3단계에서 지울 것"은 그때의 상태이고, 지금은 위 절들이 기준이다.

## 결론

- 원본 형식 내보내기(역변환, MP4 → `qcamera.ts`/`fcamera.hevc`/`ecamera.hevc`)의 기본 엔진은 순수 Rust(`src/pure_reverse.rs`, `src/pure_reverse/`)다. 기존 FFmpeg CLI 역변환(`media::reverse_ffmpeg`)은 1단계 당시 대체 경로로 남았고 3단계에서 지웠다.
- 실주행 13구간 26영상의 왕복(원본 → 순수 정방향 MP4 → 순수 역변환)에서 **qcamera.ts 13/13은 업로드한 원본과 파일 바이트가 같다.** fcamera.hevc 13/13은 정규화 NAL·프레임·키프레임·parameter set·디코딩 프레임 MD5가 원본과 같고, 파일은 IRAP마다 PPS 뒤 0 바이트 하나(영상당 60곳, +60 B)만 다르다. 이 HEVC 결과는 기존 FFmpeg CLI 역변환·Python 내보내기와 바이트가 같다.
- FFmpeg 런타임 패키지(`ffmpeg`)와 그것만 끌어오던 의존 패키지 192개를 뺀 반사실 이미지와 직접 비교하면 amd64 이미지는 **풀린 크기 580.9 MB → 125.2 MB(−455.7 MB, −78.5 %)**, 레지스트리 전송 크기(layer blob, gzip) **217.5 MB → 46.5 MB(−171.0 MB, −78.6 %)**다. 이 숫자는 FFmpeg 패키지만 뺀 값이고, 빌더의 libav dev 패키지·`ffmpeg-next`를 뺀 실행 파일 크기 변화는 3단계에서 따로 잰다.

### FFmpeg 의존 지점

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

### 기준 이미지와 반사실 비교 (amd64)

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

### 역변환 계약

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

### 설계

- 위치: `src/pure_reverse.rs`(작업·검증), `src/pure_reverse/annexb.rs`(sample → Annex B), `src/pure_reverse/ts.rs`(MPEG-TS writer). `src/media.rs`는 `reverse`가 `reverse_using(Engine::configured()?, …)`를 부르게만 바꿨고 기존 FFmpeg 코드는 `reverse_ffmpeg`로 이름만 바꿔 그대로 둔다.
- 엔진 선택(1단계 당시): 정방향과 같은 내부 설정 `HARS_REMUX_ENGINE=pure|ffmpeg`를 역변환도 따랐다. 3단계 뒤에는 순수 엔진만 있고, `ffmpeg` 값은 경고(`remux_engine_setting`)와 함께 순수 엔진으로 시작하며 그 밖의 값이면 서버가 시작하지 않는다. 공개 API·파일명·메타는 엔진과 무관하다. 구조화 로그 `reverse_engine`은 `engine`, `kind`를 남긴다.
- 실패: 순수 엔진의 실패는 정방향과 같은 5분류(`pure_remux::FailureClass`)다. 1단계 당시에는 `unsupported`·`invalid`·`internal`·`verification`이면 FFmpeg 역변환을 한 번 시도했으나 3단계에서 지웠다. 지금은 모든 분류가 다른 엔진 시도 없이 `reverse_failure`와 함께 실패하고(`resource`: ENOSPC·EFBIG·EDQUOT·ENOMEM, 180초, worker SIGKILL·SIGABRT, 종료 코드 75 포함), HTTP 응답은 기존과 같이 `media_export_failed`다.
- 격리: 서버·CLI에서는 worker 프로세스 `roadviewer-hars __pure-reverse MP4 KIND OFFSET_TICKS OUTPUT`이다. `media::run`(동시 2개, 180초, 응답 64 MiB, `kill_on_drop`, `PR_SET_PDEATHSIG`)과 정방향 worker의 메모리 상한(`RLIMIT_AS` = 현재 + 256 MiB)을 그대로 쓴다. 라이브러리 내장(시험) 경로는 `spawn_blocking` + `catch_unwind`.
- 두 번 읽기: (1) `pure_remux::readback::inspect_copy`가 MP4를 검사하고 내보내기가 담아야 할 것(서명, 프레임 수, sync sample, PTS/DTS)을 정한다. (2) 쓰기는 이와 코드를 공유하지 않는 `shiguredo_mp4`의 `Mp4FileDemuxer`가 읽은 sample 표로 한다. (3) 쓴 파일을 `NalHasher`(HEVC) 또는 정방향 엔진의 TS demuxer(`pure_remux::ts::demux`, `mpeg2ts`)로 다시 읽어 (1)과 비교한다. 두 판독기가 입력을 다르게 읽으면 `verification`이다.
- Annex B: 각 sample의 4바이트 길이 NAL을 그대로 옮기고, libavcodec `h264_mp4toannexb`/`hevc_mp4toannexb`가 하는 선택(decoder configuration을 넣는 자리, start code 길이)을 따른다. H.264는 in-band SPS/PPS가 없는 IDR 앞에 avcC의 SPS/PPS를 넣고, HEVC는 sample의 첫 IRAP NAL 앞에 hvcC의 NAL을 모두 넣는다. FFmpeg 코드는 옮기지 않았다(동작만 맞춤).
- MPEG-TS: 원본이 가진 libavformat `mpegts` 배치를 따른다. 프로그램 1, PMT PID 0x1000, 영상 PID 0x100(PCR 운반), stream type 0x1B, SDT(서비스 이름 `Service01`, 제공자 `FFmpeg`: 원본에 있는 값 그대로), PAT/PMT는 첫 PES·100 ms마다·키프레임 묶음의 첫 키프레임에, SDT는 500 ms마다, PES 길이 0, 모든 PES 첫 패킷에 PCR(= DTS × 300, muxdelay 0)과 키프레임의 random access 표시, 마지막 패킷은 adaptation field stuffing, AUD로 시작하지 않는 access unit에는 AUD를 붙인다. PTS/DTS = MP4 시각을 90 kHz로(반올림) + offset 틱. offset 틱은 `round(video_origins × 90000)`.
- 한도: MP4 메타 16 MiB, sample 32 MiB, sample 수 262,144(판독기), NAL·parameter set 한도(`NalHasher`), 출력 4 GiB, 33비트 PES 시각. 넘으면 `unsupported`/`invalid`로 실패한다(1단계 당시에는 FFmpeg 역변환으로 넘어갔고, 3단계에서 그 경로를 지워 지금은 `reverse_failure`와 `media_export_failed`로 끝난다). 메모리는 sample 하나와 해시 버퍼로 묶이고(파일을 통째로 읽지 않음), `unsafe`는 새 코드에 없다.
- 순수 엔진이 거부하는 정상 입력(지금은 내보내기 실패, 1단계 당시에는 FFmpeg 역변환으로 넘어갔다): 첫 항목이 media time 0이 아닌 edit list(FFmpeg가 B-frame MP4에 쓰는 것), 트랙이 둘 이상, sample entry 변경, hev1·in-band HEVC parameter set, 길이 필드 4바이트가 아닌 avcC/hvcC, 음수 composition 시각, 33비트를 넘는 시각.

### 검증 결과

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

(c) 손상·합성·무작위 변형 300건(`integration/pure_reverse_faults.py`, 시드 20261117, `systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=0` 아래, 기본 입력 3개: 합성 H.264·HEVC MP4와 실주행 qcamera MP4 1개). 변형 10종 × 30건: 잘림, box 크기, sample 표 값, 빈 트랙, 과대 선언(sample 크기·moov 크기·config·NAL 길이), moov 비트 반전, payload 비트 반전, avcC/hvcC 개수, 가짜 box 삽입, 무작위 바이트. 결과: `invalid` 258, `verification` 5, 성공 37(payload 비트 반전 29처럼 구조가 맞는 입력), **panic 0, 비정상 종료 0, 실패 뒤 남은 출력 0**, worker 최고 RSS 37.3 MiB, 최장 0.047초(상한 256 MiB 추가·180초 대비). `verification` 5건은 avcC의 SPS를 바꾼 입력이다: 독립 판독기는 바뀐 avcC SPS를 기대 parameter set에 넣는데 sample에는 in-band SPS가 있어 쓰기가 avcC를 넣지 않으므로 일치하지 않는다(1단계 당시 FFmpeg 대체, 지금은 내보내기 실패). 단위 시험(`tests/pure_reverse.rs`)은 빈 파일, 쓰레기, 절반 잘림, moov만, moov 크기 초과, sample 수·크기 초과, sample 0개, mdat 앞 chunk offset, sample 밖 NAL 길이, 다른 코덱을 두 코덱 모두에서 확인한다.

(d) 제품 경로(`integration/pure_reverse_product.py`, release 실행 파일, 새 임시 데이터 폴더, loopback 18610/18611, `HARS_INGRESS_ONLY=0`, 원본 보존 끔). 13구간 원본의 3·7번 구간을 브라우저 업로드 API로 올리고 자동 변환(`unconverted → processing → ready`) 뒤 업로드 TS/HEVC가 지워진 상태에서:

| 항목 | 결과(2구간 × 2카메라) |
|---|---|
| `/api/logs/<id>/original/<kind>` 전체 | 200, `Content-Disposition` attachment, 4/4 순수(`reverse_engine` pure), 대체 0 |
| Range 앞(0–1023)·중간(4 KiB)·끝 suffix(−2048) | 12/12 206, 바이트 = 전체 응답의 같은 구간, `Content-Range` 맞음 |
| 업로드한 원본과 비교 | TS 2/2 바이트 같음, HEVC 2/2 PPS 뒤 0 바이트 60곳만 다름(정규화 NAL·프레임·키프레임 같음) |
| ZIP(`/api/logs/download.zip?…&format=original`) | 2/2 구간 3파일, 원본 형식 파일 4/4가 `/original` 응답과 SHA 같음 |
| 같은 데이터 폴더를 `HARS_REMUX_ENGINE=ffmpeg` 서버로 | 4/4 `reverse_engine` ffmpeg. 순수와 정규화 NAL·프레임·키프레임·parameter set·TS 시각 같음, HEVC 바이트 같음, TS는 PCR 배치만 다름 |

(e) 시험: `tests/pure_reverse.rs` 4개(두 엔진 결과 비교·반복 내보내기 같은 바이트, edit list 입력은 순수가 `unsupported`로 넘기고 결과가 FFmpeg 엔진의 답과 같음, 손상 MP4 11종 × 2코덱, offset·종류 검사), `src/pure_reverse/` 단위 시험 4개. 마지막 전체 실행: `cargo fmt --all --check` 통과, `cargo clippy --locked --all-targets -- -D warnings` 경고 0, `cargo test --locked --all-targets` 291 통과 0 실패(`tests/ts_duration.rs` 포함). 그 직전 첫 전체 실행에서는 기존 시험 `tests/pure_remux.rs`의 `boundary_inputs_take_the_pure_engine_or_an_explicit_fallback`(정방향, PTS 없는 TS 사례 399행)이 한 번 실패해 cargo가 멈췄고, 그 파일만 3회 다시 돌리면 1회 실패·2회 통과였다(load average 6~8, 다른 작업자 동시 실행). 이 브랜치가 고치지 않은 정방향 시험이다. 기준 커밋 `d846fc6`에서는 같은 파일 6회 모두 통과, 이 브랜치에서 그 시험만 골라 6회 돌리면 모두 통과였다. 원인으로 보이는 것: 시험의 `classify`가 프로세스 전역 카운터 `media::remux_counts()`의 증가로 "대체 실패"와 "거부"를 가르는데, 같은 시험 파일의 다른 시험이 병렬로 순수 엔진 실패를 세면 거부 사례가 대체 실패로 읽힌다(시험 자체의 경쟁). 이 해석은 코드 읽기로 낸 것이고 재현으로 확정하지 않았다. 시험은 고치지 않았다.

### 성능

`integration/benchmark_reverse.py`, 같은 release 실행 파일에서 `HARS_REMUX_ENGINE`만 바꿈, 13구간의 3·7·9번 구간 MP4(HEVC 3, TS 3; 입력 ext4, warm cache), 구현별 warmup 1회 + 순서를 회전·역순으로 바꾼 교차 5회. 제품과 같은 `reverse` 명령(worker 프로세스, 검사, fsync, rename 포함). **측정 중 load average 8.1~8.7**: 다른 작업자의 `ffmpeg` 인코딩(약 5 CPU)이 동시에 돌았다(16 CPU). 절대값은 이 부하를 포함한다.

| 구현 | TS 영상당 경과 중앙값(범위) | TS CPU | HEVC 영상당 경과 중앙값(범위) | HEVC CPU | 최고 RSS(프로세스 트리 중 최대) |
|---|---|---|---|---|---|
| FFmpeg CLI 역변환 | 0.449 s (0.428–0.520) | 0.577 s | 0.437 s (0.409–0.496) | 0.665 s | TS 54.6 MiB, HEVC 74.6 MiB |
| 순수 역변환 | **0.069 s** (0.062–0.166) | 0.064 s | **0.135 s** (0.128–0.149) | 0.129 s | TS 37.2 MiB, HEVC 36.8 MiB |
| 순수 worker 단독(`__pure-reverse`) | 0.038 s (0.033–0.047) | 0.037 s | 0.104 s (0.096–0.115) | 0.104 s | **TS 37.3 MiB, HEVC 36.7 MiB** |

순수 역변환은 경과 기준 TS 약 6.5배, HEVC 약 3.2배 빠르고 CPU는 1/9, 1/5이다. FFmpeg 경로는 `ffprobe` 1회 + `ffmpeg` 2~3회(서명 추출, 복사, 결과 서명) + TS `ffprobe` 패킷 목록 2회로 프로세스 기동과 MP4 재해석이 대부분이다. 순수 HEVC는 37 MB를 읽고 쓰고 다시 해시하는 시간(입력 판독 SHA-256, 출력 SHA-256)과 fsync가 대부분이다. 순수 worker RSS는 입력 크기와 무관하게 약 37 MiB(실행 파일 매핑 + sample 버퍼)다.

### 3단계에서 지울 것

FFmpeg를 완전히 없앨 때 이 단계가 남긴 대체 경로와 그것만 검사하는 것(3단계에서 처리했다: 위 [제거한 것](#제거한-것)과 [지운 시험과 대체 근거](#지운-시험과-대체-근거)):

- `src/media.rs`: `reverse_ffmpeg`, `signature`(ffmpeg CLI), `reverse_using`의 대체 분기와 `reverse_fallback` 기록(순수 실패는 그대로 실패로), `probe`·`validate_upload`(`ffprobe`, 업로드·가져오기 검사의 순수 대체 필요), `input_format`.
- `tests/pure_reverse.rs`: `pure_export_holds_what_the_ffmpeg_export_holds`의 FFmpeg 엔진 비교 부분, `an_edit_list_the_engine_does_not_copy_falls_back_to_ffmpeg`의 엔진 비교(순수 `unsupported` 확인은 유지). 합성 입력 생성(`ffmpeg` CLI)은 개발 도구로 남길지 정해야 한다.
- `tests/media.rs`의 `media::reverse` 호출 두 곳은 순수 경로를 지나므로 유지 가능(합성 입력은 `ffmpeg` CLI).
- `integration/pure_reverse_audit.py`·`pure_reverse_product.py`의 `ffmpeg` 엔진 비교 단계, `benchmark_reverse.py`의 `binary:…@ffmpeg` 구현. 디코딩 MD5 oracle(호스트 `ffmpeg`)은 개발 전용으로 남길 수 있다.
- 문서: 이 문서와 [PURE-REMUX.md](PURE-REMUX.md)의 "역변환은 바뀌지 않았다"(조정자가 통합).

### 미검증과 남은 위험

- aarch64(HAOS) 이미지 크기와 역변환 실행은 미검증. 새 코드는 정수 연산과 `f64` 곱셈·반올림 한 번(offset 틱)만 쓴다.
- 실주행 B-frame MP4, field 부호화, 다중 트랙 MP4, hev1 MP4는 코퍼스에 없다. 순수 엔진은 이들을 거부한다. 3단계에서 FFmpeg를 없앤 지금 이 입력의 원본 형식 내보내기는 실패한다(1단계 당시의 FFmpeg 역변환도 B-frame edit list MP4는 시각 검사에서 실패했다: 시험 `an_edit_list_…`).
- avcC SPS/PPS와 sample의 in-band SPS/PPS가 다른 MP4는 `verification`으로 내보내기가 실패한다(위 (c); 1단계 당시에는 FFmpeg 역변환으로 넘어갔다).
- AUD 없는 H.264 MP4: muxer가 AUD를 붙여 "기타 NAL" 서명이 달라지므로 두 엔진 모두 실패한다(기존 동작과 같음). 제품의 MP4는 TS에서 왔으므로 AUD가 있다.
- TS 바이트가 원본과 같은 것은 원본이 libavformat `mpegts`로 만든 Road Viewer 원본 형식 내보내기였기 때문이다. 카메라가 처음 쓴 TS(다른 muxer)라면 PES 내용·시각은 같아도 PSI·PCR 배치는 다를 수 있다(코퍼스에 없음).
- 역변환 결과는 요청마다 임시 파일로 만들고 응답 뒤 지운다(기존과 같음). 새로 넣은 fsync는 이 임시 파일에도 적용된다(성능 표에 포함).

### 재현

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
