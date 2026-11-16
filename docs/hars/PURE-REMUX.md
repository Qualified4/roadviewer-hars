# 순수 Rust 리먹싱 엔진 (기본 엔진)

<!-- toc -->
**목차**

- [결론](#결론)
- [구성과 엔진 선택](#구성과-엔진-선택)
- [실패 분류와 대체 정책](#실패-분류와-대체-정책)
- [손상 입력 사례](#손상-입력-사례)
- [격리](#격리)
- [메모리 구조와 상한](#메모리-구조와-상한)
- [출력 검증](#출력-검증)
- [컨테이너 차이 처리](#컨테이너-차이-처리)
- [검증 결과](#검증-결과)
- [제품 경로 검증](#제품-경로-검증)
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

## 실패 분류와 대체 정책

순서: `media`가 입력을 검사하고(TS는 FFmpeg 패킷 검사가 두 엔진 공통의 원본 검사다) 마지막 sample 길이를 한 번 정한 뒤 순수 엔진을 실행한다. 순수 엔진의 실패는 타입(`pure_remux::FailureClass`)으로만 분류한다. 오류 문자열은 판정에 쓰지 않는다. 분류 순서: `Unsupported` 원인 → OS 자원 오류(`io::Error`의 errno) → 오류가 난 단계(`Stage`: 입력 읽기 `invalid`, 쓰기 `internal`, 다시 읽기 `verification`). worker 프로세스의 종료 방식도 같은 분류로 바꾼다(`media::failure`). 기계 때문에 실패한 media worker는 종료 코드 75로 끝난다(`media::RESOURCE_EXIT`; FFmpeg 엔진의 errno도 아래 표 하나로 판정, `native_video::resource_error`). 그래서 FFmpeg 사본의 디스크 부족도 `resource`다. 변환(`convert.rs`)도 문자열이 아니라 이 `Failure` 값으로 판정한다.

OS 오류(errno)의 분류(`pure_remux::resource_code`, 두 엔진 공통). `resource`는 다음 시도에서 풀릴 수 있는 기계 조건만이다. 나머지 errno는 같은 입력이면 매번 같은 결과이므로 그 오류가 난 단계(`Stage`)로 분류한다: 입력 읽기면 `invalid`, 쓰기면 `internal`. 둘 다 FFmpeg 대체를 한 번 시도하고, 그것도 실패하면 그 카메라 없이 경고로 끝난다.

| errno | `reason_code` | 분류 |
|---|---|---|
| ENOSPC | `no_space` | `resource` |
| EFBIG(`RLIMIT_FSIZE` 포함) | `file_too_large` | `resource` |
| EDQUOT | `quota` | `resource` |
| ENOMEM | `out_of_memory` | `resource` |
| EIO(불량 섹터), EACCES·EPERM(권한), ENOENT, ESTALE·ENOTCONN(끊긴 마운트), EINTR·EAGAIN, 그 밖의 모든 errno | 단계별(`invalid_input`, `engine_error`) | `invalid`(읽기) / `internal`(쓰기) |

errno가 아닌 `resource`: 180초 시간 상한(`deadline`), worker SIGKILL(`worker_killed`), SIGABRT(메모리 상한, `out_of_memory`), worker 종료 코드 75(`worker_resource`). 이전(`09c48e9`까지)에는 EINTR·EAGAIN을 뺀 모든 errno가 `resource`(`io_error`)여서, 읽을 수 없는 카메라 파일(EIO, EACCES) 하나가 다시 변환할 때마다 같은 실패를 냈다.

| 분류 | 예 | 대체 | 로그 이벤트(수준) | 최종 상태 |
|---|---|---|---|---|
| `unsupported` | B-frame TS, interlaced, scaling matrix, HEVC 시각이 0에서 시작하지 않음, HEVC parameter set 중간 변경, 한 PES에 여러 picture, 스크램블 표시, 출력 3 GiB 이상(payload를 쓰는 중 그 NAL에서 판정), 여유 공간이 입력×2 + 100 MiB 미만(`low_space`, 순수 엔진을 실행하지 않음) | FFmpeg 1회 | `remux_fallback`(INFO) | FFmpeg 출력이 같은 검사를 통과하면 게시, 아니면 해당 영상 없음 |
| `invalid` | parameter set의 forbidden bit, parameter set 없음, NAL 32 MiB 초과, 첫 그림 전 parameter set 64개/1 MiB 초과, 프레임 수 불일치, 시작 코드 없는 PES 앞부분, TS 연속성 오류·transport error·PTS 없는 PES, 잘린 TS 패킷 | FFmpeg 1회, 같은 입력 검사·한도로 | `remux_fallback`(WARN) | [손상 입력 사례](#손상-입력-사례) |
| `internal` | 쓰기 단계 오류, panic(worker 종료 코드 101), worker 신호 종료(SIGSEGV 등), worker 응답 해석 실패 | FFmpeg 1회(독립 검사 통과 시만 게시) | `remux_fallback`(ERROR) | 게시 또는 해당 영상 없음 |
| `verification` | 순수 출력이 판독 검사(서명, 시각, 레이아웃, sync 표시, sample 경계)와 불일치 | FFmpeg 1회(같은 검사) | `remux_fallback`(ERROR) | 게시 또는 해당 영상 없음 |
| `resource` | ENOSPC, EFBIG(`RLIMIT_FSIZE`), EDQUOT, ENOMEM(위 표), 180초 시간 상한, worker SIGKILL(OOM killer·종료), SIGABRT(메모리 상한에서 할당 실패), worker 종료 코드 75 | **없음** | `remux_failure`(WARN) | 디스크에 게시된 결과가 그 카메라를 재생하면 이번 시도를 버리고 이전 결과로 `ready` + 경고(아래). 아니면 해당 영상 없음 + 경고. 부분 출력 삭제 |

- "해당 영상 없음"은 Python `prepare_video` 실패와 같다: 그 카메라 MP4 없이 기록은 `ready`, `warnings`에 `<카메라> 영상 준비 실패: …`. `resource`면 경고가 `… 영상 준비 실패: resource (<reason_code>): <이유>`로 분류를 드러낸다. 원본은 남는다(원본 보존 끔이어도 `jobs`는 MP4를 만든 카메라의 원본만 지운다). 다시 변환하려면 사용자가 변환 결과를 지우고 다시 변환한다(Python과 같음). 시험: `tests/conversion_resource.rs`, `tests/conversion_keep_previous.rs`.
- 이전 결과 보호: `resource` 실패는 같은 디스크·메모리·시간이 다음 시도에서는 풀릴 수 있다. 그래서 지금 디스크에 게시된 결과가 그 카메라를 재생하면 그 카메라가 빠진 결과로 바꾸지 않는다. 판정은 디스크만 본다: `prepared/summary.json`의 `videos.<카메라>`가 있고, 그 MP4가 `prepared/<mp4>`, 기록 폴더의 저장된 `<mp4>`, 또는 아카이브에 있어야 한다(`convert::previous_plays`). 기록 메타의 `videos`는 보지 않는다: Python과 같이 `clear_prepared`가 이 필드를 지우지 않아 재변환(변환 결과 삭제 → 변환, 카메라 추가) 때는 낡은 값이 남기 때문이다(`09c48e9`까지는 이 낡은 값으로 판정해, `prepared/`가 이미 지워진 재변환이 `error`로 끝나고 다시 해도 같았다).
  - 게시된 결과가 있을 때: 변환 프로세스가 `conversion_kept_previous`(ERROR, `kind`·`failure_class`·`reason_code`·`reason`)를 남기고 staging에 `kept-previous.json`을 쓴 뒤 실패한다. `jobs`는 staging을 지우고, 그 세대가 아직 기록의 세대이고 `prepared/summary.json`이 있으면 기록을 그 결과의 `ready`로 되돌린다: 결과 필드는 `prepared/summary.json`에서 다시 쓰고, `warnings`는 그 결과의 경고 + `<카메라> 영상 준비 실패: resource (<reason_code>): <이유> (이전 변환 결과 유지)` 한 줄, `error`는 null. `prepared/`, 저장된 MP4(원본 보존 끔), 원본은 바이트 단위로 그대로이고 영상 Range 요청이 계속 재생된다. 그 결과가 사라졌거나 다른 세대가 기록을 가졌으면 기존 실패 규칙(`error`)이다.
  - 게시된 결과가 없을 때(일반적인 재변환, 첫 변환): Python처럼 그 카메라 없이 `ready` + `resource (<reason_code>)` 경고. 저장된 MP4(원본 보존 끔)는 게시가 건드리지 않으므로 그대로 남고, 영상 라우트가 색인의 저장본을 재생한다.
  - 어느 경우도 `error`에 머물지 않는다. 같은 기록에 실패를 몇 번 반복해도 `ready`다(`repeated_resource_failures_on_one_record_stay_ready`). 입력·엔진 실패(`unsupported`·`invalid`·`internal`·`verification`)에는 보호를 적용하지 않는다: 같은 입력이면 다시 해도 같은 결과이고, 원본(또는 그 카메라의 유일한 MP4 원본)은 어느 경우에도 지워지지 않는다. Python과 다른 점은 게시된 결과가 있을 때의 `resource` 실패 하나다(Python은 카메라 실패를 원인과 무관하게 경고로 넘기고 그 카메라 없는 결과를 게시한다). 취소는 실패가 아니라 기존 취소 규칙(`unconverted`)으로 끝나고 이전 결과는 그대로다.
- 이벤트 필드: `event`, `engine`(`pure`), `kind`(카메라 종류), `failure_class`, `reason_code`(집계용 고정 코드), `reason`(300자 이내 오류 사슬), `fallback`(bool), `fallback_result`(`published`/`failed`), `fallback_reason`. raw HEVC 앞부분의 NAL 아닌 바이트는 `remux_input_prefix`(WARN, `bytes`)로 남긴다. 서버는 변환 자식 프로세스의 이 이벤트를 레코드 id(`record`)와 같은 수준으로 다시 기록한다(`media::relay_remux_event`). 조회는 서버 로그(구조화 JSON)만 쓴다. 기록 메타나 API 응답에는 넣지 않았다: 공개 스키마가 Python과 같아야 하고, 엔진 진단은 운영자용이기 때문이다.
- 대체는 카메라 파일 하나당 최대 1회다(`remux_inner`에 재시도 루프가 없다). 한 기록에는 카메라 파일이 셋까지라 기록당 최대 3회다. 실패한 변환은 기존 변환 큐 규칙대로 끝나고 자동으로 다시 돌지 않는다.
- FFmpeg 대체 출력도 순수 출력과 같은 검사를 받는다: raw HEVC는 libavformat 파서와 무관한 `NalHasher` 원본 서명(`media::raw_signature`, 같은 NAL·parameter set 한도)이 FFmpeg 패킷 서명·출력 서명과 같아야 하고, 출력은 FFmpeg inspect와 `pure_remux::readback` 두 판독기로 읽는다(sync 표시, sample당 그림 1개, 서명, 마지막 sample 길이). 공통 검사는 `verify_and_place`다.
- 부분 출력: 엔진의 sample payload는 출력 폴더의 이름 없는 임시 파일(`O_TMPFILE`, `tempfile::tempfile_in`)이라 어떤 종료에도 남지 않는다. 출력 임시 파일과 job/clock JSON은 drop 가드(`media::Scratch`)가 실패·취소·시간 초과 때 지운다. 변환이 실패·취소되면 `jobs`가 프로세스 그룹을 종료한 뒤 그 세대의 staging 폴더를 지운다(아직 게시 전이므로 안전). 서버가 변환 중에 죽어 남은 staging 폴더는 다음 시작 때 지운다(`Jobs::open`; 저널에 없는 세대만. 저널에 있는 세대는 기존대로 `publish`가 처리하고, 취소된 완료 세대는 staging을 남긴다). 게시는 원자적 rename이라 기존 결과는 새 결과가 검사를 통과하기 전에는 바뀌지 않는다.
- 내구성: 두 엔진의 MP4는 rename 전에 `sync_all`하고, rename 뒤 그 폴더도 sync한다(`media::durable_rename`). 원본 보존 끔에서 `publish`는 MP4를 기록 폴더로 옮긴 뒤 기록 폴더와 `prepared/`를 sync하고 나서야 원본을 지운다. 그 사이에 서버가 죽으면 원본이 남고, 재시작의 저널 복구가 이미 옮긴 MP4를 알아보고 원본 삭제까지 마친다(`retention_off_deletes_an_original_only_after_its_mp4_is_durable`; 이전에는 복구가 옮긴 MP4를 못 보고 원본을 남겼다). 정전 자체는 모의하지 못했다(순서만 시험).
- 고아 worker: 모든 media worker와 변환 프로세스는 `PR_SET_PDEATHSIG(SIGKILL)`로 시작한다(`media::die_with_parent`). 부모가 SIGKILL로 죽어도 worker가 끝나며, 시험(`killed_conversion_process_takes_its_worker_and_a_retry_completes`)은 이 설정을 빼면 실패한다.

`times[0] ≠ 0`: 제품 경로에서는 일어나지 않는다. `convert.rs`가 raw HEVC 시각을 첫 프레임 기준으로, TS 시각을 최소 PTS 기준으로 만들기 때문이다. CLI 등에서 들어오면 순수 엔진이 거부하고 FFmpeg 엔진이 edit list로 처리한다(기존 동작).

## 손상 입력 사례

판정 기준: 입력 파일에 있는 모든 픽처 바이트가 출력에 그대로 있고 독립 검증(입력 대비 서명)을 통과하면 게시한다. 파일에 있는 바이트를 일부 버려야만 출력이 만들어지면 명확한 실패다. Python 열은 Python 원본 코드(`hevc_video.prepare_video`, `decoder.py`의 qcamera 단계, PyAV 16.1/FFmpeg 8.0.1)를 같은 합성 입력(10프레임)에 실제로 돌린 결과다(`integration/boundary_inputs.py`). "픽처 일치"는 출력 sample의 VCL 바이트를 입력 파일의 독립 분할(TS는 손상 전 원본)과 비교한 것이고, 디코딩은 시스템 FFmpeg 6.1.1이다.

| 사례 | Python | HARS 순수(기본) | HARS FFmpeg 엔진 | 결정과 근거 |
|---|---|---|---|---|
| slice NAL forbidden bit | 게시, 픽처 일치, 디코딩 9/10(해당 slice 건너뜀) | 게시(대체 없음), 바이트 = Python | 게시, 바이트 = Python | 입력 바이트를 그대로 옮긴다. 디코더가 건너뛰는 것은 원본에서도 같다 |
| SEI NAL forbidden bit | 게시, 디코딩 10/10 | 게시, 바이트 = Python | 게시 | 같음 |
| SPS forbidden bit | 실패(libavformat이 해상도를 못 읽음) | `invalid` → 대체 실패 | 실패 | 어떤 디코더도 쓸 수 없는 설정. Python과 같이 실패 |
| 첫 시작 코드 앞 바이트(4 B, 1 KiB) | 게시, 픽처 일치, 디코딩 10/10 | 게시, `remux_input_prefix` 경고(4, 1024 B), 바이트 = Python | 게시 | 시작 코드 앞 바이트는 NAL이 아니다(어떤 sample에도 들어갈 수 없다). Python·libavformat도 버린다 |
| NAL 중간에서 시작(앞 절반 잘림) | 실패(libavformat probe) | `invalid` → 대체 실패(로그와 프레임 수 불일치) | 실패 | 남은 픽처가 로그와 맞지 않는다 |
| 유닛 뒤 0 바이트 | 게시 | 게시, 바이트 = Python | 게시, 바이트 = 순수 | 두 엔진 모두 입력 바이트를 보존(아래) |
| TS 마지막 패킷이 영상 payload 안에서 잘림(1·100 B 잘림) | **게시, 마지막 프레임 바이트 손실**(디코딩 오류 1줄) | 원본 검사에서 실패 | 원본 검사에서 실패 | libavformat은 188 B 미만 마지막 패킷을 버린다. 그 패킷의 payload는 파일에 있으므로 버리면 조용한 손실이다. Python 결함으로 본다 |
| TS 마지막 패킷이 동기 바이트만 남음(187 B 잘림) | 게시 | `invalid` → FFmpeg 게시 | 게시 | 버려지는 바이트에 payload가 없다. 출력은 파일의 모든 영상 바이트를 담는다 |
| TS가 패킷 경계에서 잘림(188 B) | 게시 | 게시(대체 없음) | 게시 | 파일에서 버려지는 바이트가 없다. 마지막 프레임은 파일 자체가 불완전하다(디코딩 오류 1줄, Python과 같음). 컨테이너만으로는 알 수 없다 |
| TS 끝에 다른 PID의 잘린 패킷 | 게시 | `invalid` → FFmpeg 게시 | 게시 | 영상 바이트 손실 없음 |
| TS slice forbidden bit | 게시, 디코딩 9/10 | 게시(대체 없음) | 게시 | HEVC와 같음 |

TS 잘림 규칙(`native_video::truncated_video_payload`): 파일 길이가 188의 배수가 아니고, 마지막 조각이 영상 PID 패킷이며 payload 바이트를 하나 이상 담으면 원본 검사가 거부한다. 조각이 동기 바이트로 시작하지 않으면(188 B 정렬이 아님) 무엇이 버려지는지 알 수 없으므로 거부한다.

## 격리

서버·CLI에서는 순수 엔진을 별도 worker 프로세스(`roadviewer-hars __pure-remux SOURCE JOB_JSON OUTPUT`)로 실행한다. FFmpeg worker와 같은 `media::run`을 쓰므로 동시 실행 2개, 180초 시간 상한, 출력 64 MiB 상한, `kill_on_drop`이 같다. 패닉은 worker 종료로 끝나고 대체 경로로 넘어간다. 라이브러리 내장(시험) 경로는 `spawn_blocking` + `catch_unwind`로 같은 결과를 낸다. 입력 한도는 FFmpeg 엔진과 같다: NAL 32 MiB, sample 32 MiB, 패킷 262,144개, video stream 1개, 코덱·해상도 검사(최대 4096×2160, 짝수).

## 메모리 구조와 상한

- worker가 메모리에 두는 것: sample 표(프레임당 크기·길이·키프레임, 측정 기울기 약 1.8 KB/프레임), parameter set(첫 그림 전 64개/1 MiB 한도), NAL 분할 버퍼와 해시 버퍼(각 NAL 32 MiB 한도). sample payload는 메모리에 모으지 않는다: 첫 읽기에서 출력 폴더의 이름 없는 임시 파일(`O_TMPFILE`)에 쓰고, moov를 쓴 뒤 `copy_file_range`로 mdat 자리에 옮긴다. 그래서 입력 바이트 수가 아니라 프레임 수와 가장 큰 NAL이 메모리를 정한다.
- 상한: worker는 job을 읽은 뒤 주소 공간을 현재 크기 + 256 MiB로 묶는다(`RLIMIT_AS`, `pure_remux::MEMORY_BUDGET_MIB`; 시험용 `HARS_PURE_WORKER_MEMORY_MB`는 debug 빌드에서만 읽는다. 제품에서 줄일 이유가 없고, 너무 작은 값은 모든 영상을 `resource`로 실패시킨다). 넘으면 할당 실패로 SIGABRT → `resource`/`out_of_memory`, 대체 없음. 실제 실행 파일 경로에서 확인했다(아래 [제품 경로 검증](#제품-경로-검증)의 memory).
- 동시 실행: 한 프로세스의 media worker는 2개까지(`media::WORKERS`). 서버는 변환 설정(1~4)만큼 변환 프로세스를 띄우고, 각 변환은 카메라를 차례로 처리하므로 순수 worker는 변환당 1개다. 측정: 동시 2변환에서 순수 worker 최대 2개, 동시 4변환에서 4개.

측정(합성 raw HEVC 1280×720, 실제 실행 파일 `remux` CLI, 외부 상한 `systemd-run --scope -p MemoryMax=2G -p MemorySwapMax=0`, 각 1회). "최고 RSS"는 `wait4`로 읽은 가장 큰 단일 프로세스(CLI 또는 worker)의 정확한 값이다. 측정 스크립트가 입력을 메모리에 들고 있으면 exec 전 공유 페이지가 자식 값에 잡히므로(처음 측정에서 적대적 입력이 약 292 MiB로 잘못 나옴) 입력은 디스크로만 흘려 쓴다(`integration/pure_remux_memory.py`).

| 입력 | 최적화 전 `0ed74af` 최고 RSS | 최종 최고 RSS | 최종 worker(자체 기록) | 최종 경과 |
|---|---|---|---|---|
| 20 MB, 600프레임 | 54.2 MiB | 36.5 MiB | 35.0 MiB | 0.12초 |
| 102 MB, 3,000프레임 | 99.1 MiB | 40.9 MiB | 37.8 MiB | 0.36초 |
| 408 MB, 12,000프레임 | 99.6 MiB | 57.3 MiB | 54.1 MiB | 1.38초 |
| 154 MB, 600프레임(같은 프레임, 7.5배 바이트) | 100.0 MiB | 36.8 MiB | 36.5 MiB | 0.51초 |
| parameter set만 100 MB | **2,056 MiB, 120초 뒤 실패(2 GiB 외부 상한)** | 34.7 MiB, `invalid` 2.4초 | 33.3 MiB | 2.4초 |
| 5바이트 SEI만 100 MB | **2,056 MiB, 34.7초 뒤 실패(2 GiB 외부 상한)** | 34.2 MiB, `invalid` | 33.9 MiB | 0.43초 |
| 33 MiB NAL 하나 | 157.6 MiB | 95.4 MiB, `invalid` | 95.4 MiB | 0.17초 |

적대적 입력은 순수 엔진이 `invalid`로 거부한 뒤 FFmpeg도 같은 한도로 거부한다(대체 1회, 게시 없음). 서버의 동시 변환 트리 최고 RSS는 [제품 경로 검증](#제품-경로-검증)에 있다. 변환 프로세스의 로그 분석이 대부분이다(변환당 약 380 MB).

## 출력 검증

엔진과 무관한 같은 검사(`media::verify_and_place`)를 통과해야 원자적으로 rename한다.

- 순수 엔진 출력은 `pure_remux::readback`이 다시 읽는다. 이 판독기는 box 트리와 sample table(stsz/stts/ctts/stsc/stco·co64/stss, avcC/hvcC)을 직접 해석하며, 쓰기 코드나 `shiguredo_mp4`와 코드를 공유하지 않는다. 입력 서명은 `media::NalHasher`(쓰기 쪽 Annex B 분할과 다른 구현)로 계산한다.
- FFmpeg 엔진 출력은 FFmpeg inspect와 같은 독립 판독기로 읽는다.
- 공통: 코덱·해상도, 프레임 수와 로그 시각 ±2 ms, 정규화 VCL SHA + decoder header set이 입력과 같음, TS는 패킷별 PTS/DTS가 입력 PES 시각 − 최소 PTS와 정확히 같음, 마지막 sample 길이가 정한 값과 같음, time base(TS 1/90000, HEVC 1/1000000), moov가 mdat 앞, avcC/hvcC 존재.
- 마지막 sample 길이: composition offset이 없으면(순수 출력, B-frame 없는 FFmpeg 사본) libavformat stream duration − (마지막 DTS − 첫 DTS). B-frame FFmpeg 사본은 libavformat이 edit list를 따른 stream duration을 보고하므로 이 식이 맞지 않는다(카메라 시각이 흔들리면 첫·마지막 composition offset이 다르다). 그 사본은 독립 판독기가 읽은 stts의 마지막 값으로 확인한다. 이전에는 이 식을 모든 출력에 적용해 실주행형 B-frame TS의 대체가 늘 실패했다(`b_frame_ts_falls_back_to_ffmpeg`가 흔들리는 시각과 기록된 다음 프레임으로 이것을 재현한다).
- 순수 엔진 추가: top-level이 정확히 `ftyp, moov, mdat`(패딩 없음), mdhd duration = stts 합(이 엔진은 composition offset을 쓰지 않는다. FFmpeg의 B-frame 사본에는 적용하지 않는다).
- sample 경계: 순수 출력은 sample당 그림 1개. FFmpeg 사본(`readback::inspect_copy`)은 H.264에 한해 필드 두 개(PAFF, 한 액세스 유닛)를 한 sample로 허용한다. 순수 엔진은 interlaced를 `unsupported`로 넘기므로 PAFF는 FFmpeg 사본만 받는다. 규칙 단위 시험만 있고, 실제 PAFF 파일은 만들 도구가 없어(x264는 MBAFF만 부호화) 미검증이다.
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
| 8 | 입력 계약 차이 | `times[0]≠0`·PTS 없는 PES·연속성 오류 등은 명시적 분류로 FFmpeg 대체([실패 분류와 대체 정책](#실패-분류와-대체-정책)) |
| 9 | 유닛 뒤 0 바이트(`trailing_zero_8bits`) | **두 엔진을 맞춤.** FFmpeg 엔진은 in-band parameter set을 `filter_units`(CBS)로 지우면서 다른 유닛도 다시 써 끝 0을 버렸다. 순수 엔진·Python(libavformat 원시 복사)은 유지한다. 입력 바이트 보존 쪽으로 맞췄다: FFmpeg 엔진이 parameter set만 직접 빼고 나머지 유닛은 순수 엔진과 같은 파일 수준 framing(`annexb::Unit::sample`; 패킷 끝에 붙은 다음 start code의 `zero_byte`는 다음 패킷을 보고 정한다)으로 넘긴다. 대체가 일어나도 sample 바이트가 같다(`engines_keep_the_zero_bytes_after_units_alike`, 13구간 26영상 FFmpeg 엔진 payload = Python). CBS가 거부하던 forbidden bit 유닛도 이제 그대로 옮긴다 |

추가로 찾은 차이: raw HEVC 시각이 정확히 0.5 µs에 걸리는 프레임(13구간 15,600프레임 중 5개)에서 Python `round()`(짝수 쪽)와 Rust `.round()`(0에서 먼 쪽)가 1 µs 달랐다. 두 Rust 엔진 모두 `round_ties_even`으로 바꿔 Python과 같게 했다.

## 검증 결과

이 절의 코퍼스 결과는 최종 실행 파일(커밋 `1425384`의 release)로 다시 돌린 것이다. 영상·경로·clock은 커밋하지 않았다.

- 두 엔진 교차: 같은 13구간을 `HARS_REMUX_ENGINE=ffmpeg`로도 변환했다. replay JSON 13/13 동등, 26영상 모두 Python과 sample payload·PTS/DTS·키프레임·decoder configuration이 같다(유닛 뒤 0 바이트 처리를 맞춘 뒤에도). 순수 기본 실행에서 대체는 0회였다. 순수 엔진 26개 MP4는 이 브랜치의 이전 측정 출력(메모리 최적화 전후 비교에 쓴 것)과 바이트가 같다.
- 실제 실행 파일 e2e는 [제품 경로 검증](#제품-경로-검증)에 있다.

- 13구간 26영상(`integration/pure_remux_audit.py`, PyAV 16.1/FFmpeg 8.0.1, system ffmpeg 6.1.1 디코딩): qcamera 13·front 13 모두 패킷 1,200, PTS/DTS·키프레임 flag·sample payload SHA·마지막 외 duration·decoder configuration 바이트가 Python과 같음, 정규화 전체 NAL SHA = 원본, 전체 디코딩 1,200프레임·오류 줄 0·프레임 MD5 목록 = Python, stream duration = 각 쪽 모델 값. 키프레임 합계 TS 1,040, HEVC 780.
- 전체 replay JSON(`integration/real_conversion_compare.py`): 원본 13구간 13/13(모델링한 duration 파생 필드 외 차이 0, `drive_summary` 변화 없음), 14구간 14/14, 47구간 47/47.
- CI 합성 oracle `integration/pipeline_contract.py`: 통과(qcamera만 Python probe 값과 다른 것이 모델대로).
- 경계 시험: `tests/pure_remux.rs`, `tests/terminal_duration.rs`, `tests/pure_remux_faults.rs`, `tests/archive_pure_remux.rs`.

## 제품 경로 검증

최종 release 실행 파일, 매번 새 임시 데이터 폴더, loopback 포트, 실주행 13구간 원본(읽기 전용). `integration/pure_default_product.py`(시나리오별 JSON, 경로·clock·좌표 없음), `integration/real_device_media.py`, `integration/ui_view_check.py`.

| 항목 | 결과 |
|---|---|
| 브라우저 업로드 → 자동 변환 → 재생 | `unconverted → processing → ready`, 두 카메라 `remux_engine` pure·`NextFrame`, 대체 없음. Range 206(앞·끝 1 KiB 바이트 = 저장 파일, `Content-Range`), MP4 다운로드 = 저장 파일, 원본 3종 다운로드 SHA = 업로드 |
| RV1 장치 업로드(3번 구간) | 보존 모드: 원본 다운로드 SHA 일치, MP4 Range 206. 비보존 모드: 순수 MP4에서 역변환한 원본의 정규화 전체 NAL SHA(유형별 개수·해시 포함)가 원본과 같음(TS·HEVC), TS PTS 차 0 |
| 변환 취소(순수 worker 실행 중, `video_read` 단계) | 상태 `unconverted`, 데이터 폴더를 쓰는 프로세스 0, staging 없음, 원본 SHA 그대로, 다시 변환 `ready` |
| 순수 worker SIGKILL | `remux_failure` `resource`/`worker_killed`, 대체 없음, 그 카메라 없이 `ready` + 경고(Python과 같음), 원본 그대로, 결과 지우고 다시 변환 → 두 카메라 `ready` |
| 변환 프로세스 SIGKILL | worker도 종료(남은 프로세스 0), 상태 `error`("original data preserved"), staging 없음, 다시 변환 `ready` |
| 서버 SIGKILL(변환 중) → 재시작 | 고아 프로세스 0, 재시작 후 `processing → ready`. **수정 전에는 그 세대 staging 폴더가 성공 뒤에도 남았다**(`Jobs::open`이 지우게 고침, `startup_removes_the_staging_of_a_conversion_a_killed_server_left`) |
| 쓰기 한도(`RLIMIT_FSIZE` 8 MiB, SIGXFSZ 무시 → EFBIG) | fcamera `resource`/`file_too_large`, 대체 없음, qcamera만 `ready` + 경고, 원본 그대로, 한도 없이 다시 변환 → 두 카메라 |
| 메모리 상한(`HARS_PURE_WORKER_MEMORY_MB=1`) | 두 카메라 `resource`/`out_of_memory`(SIGABRT), 대체 없음, 영상 없이 `ready` + 경고, 원본 그대로, 다시 변환 → 두 카메라 |
| 동시 업로드 4 + 변환 동시 2 | 열린 업로드 세션이 있는 동안 4개 모두 `queued`(`waiting_upload`), 세션을 닫자 4/4 `ready`, 모두 pure. 변환 프로세스 최대 2, 순수 worker 최대 2, 트리 최고 RSS 790 MiB(809,156 KiB) |
| 다시 변환 동시 4 | 4/4 `ready`, 변환 프로세스 최대 4, 순수 worker 최대 4, 트리 최고 RSS 1,467 MiB(1,502,396 KiB) |
| B-frame qcamera.ts(실주행 qcamera를 B-frame 2로 다시 인코딩, PTS 보존, 합성) | `remux_fallback` `unsupported` → FFmpeg `published`, `ready`, Range 206. **수정 전에는 대체가 늘 실패했다**([출력 검증](#출력-검증)) |
| `HARS_REMUX_ENGINE=ffmpeg` 서버 | 같은 흐름 `ready`, 두 카메라 `remux_engine` ffmpeg, Range 206 |
| 실제 UI(`/view/<id>/`, headless Edge, 매번 새 임시 프로필) | 순수 기록 2회, B-frame 대체 기록 1회 모두 9/9: 메타데이터, 카메라 전환(front ↔ qcamera), 재생, 일시정지, 중간·말미 시킹(readyState 4), 오버레이 캔버스 그려짐, 지도(track 요청, 지도 요소), 차량 그래프(telemetry 요청, 그래프 6개) |

UI 검사는 iframe 안의 실제 페이지를 사용자처럼 조작한다(버튼 클릭, seek 막대 입력). 단독 `<video>` 재생 검사([브라우저 재생](#브라우저-재생))와 별개다. 자동화하지 않은 것: 영상 화질의 눈 확인, 오버레이 그림이 영상과 맞는지의 눈 확인, 지도 타일(외부 네트워크) 로드, 터치·키보드 단축키. 실제 디스크 부족(ENOSPC)은 user namespace 안의 작은 tmpfs로 확인했다(`integration/low_space_tmpfs.sh`, root 불필요, 합성 raw HEVC 10,126,246바이트, release `remux` CLI). 서버 전체 흐름의 ENOSPC는 `RLIMIT_FSIZE`의 EFBIG와 여유 공간 주입(`HARS_AVAILABLE_SPACE_BYTES`, debug 전용)으로 확인했다.

| tmpfs 크기 | `3834c10` | 이번 수정 |
|---|---|---|
| 입력 × 1.5 | 순수 엔진 `resource`/`no_space`, 대체 없음, 실패(종료 1) | `remux_fallback` `unsupported`/`low_space` → FFmpeg `published`, 출력 10,127,301바이트, 남은 임시 파일 0 |
| 입력 × 0.5 | (측정 안 함) | `low_space` → FFmpeg도 ENOSPC, FFmpeg worker 종료 코드 75(`resource`), 남은 파일 0 |

## 브라우저 재생

`integration/browser_video_check.py`로 Windows Microsoft Edge(`--headless=new`, 매번 새 임시 프로필, 사용자 브라우저·ego-browser 미사용)를 돌렸다. 영상은 WSL의 다중 스레드 Range 서버(127.0.0.1, 지정 포트)로 제공했다. 순수 엔진 26개와 FFmpeg 엔진(`HARS_REMUX_ENGINE=ffmpeg`) 26개를 각각 3회, 총 156회 확인했다.

- 오류(`error`, `MediaError`) 0. 중간(50 %)·말미(99.5 %) seek 후 readyState 최소 4. 재생 400 ms 후 진행, 일시정지 정상.
- 병합 커밋 `40854ee` 출력으로 다시 돌린 결과다(병합 전 출력으로 한 첫 실행도 같은 항목 모두 통과, `loadedmetadata` 최대 120 ms).
- `loadedmetadata`: 중앙값 4 ms, 최대 86 ms(순수 엔진 1회차 첫 영상, cold start). 15초 대기 초과 0회. 이전 실험의 5번 TS 대기 초과는 3회 반복에서 재현되지 않았다. 이번 서버는 요청마다 스레드를 쓰므로, 이전의 단일 스레드 시험 서버가 원인이었다는 추정과 맞는다(확정은 아님).
- `video.duration`: 두 엔진 26/26 같음, replay JSON의 `videos.*.duration`과 26/26 같음(최대 차이 4.4e-7초, float 표현 차이).
- 말미 동기: 마지막 0.3초를 재생했을 때 `ended` 시점의 `currentTime − duration` = 0(156/156). 영상은 다음 프레임 시각에서 끝나고 그 뒤로 늘지 않는다.
- `canPlayType`: hvc1은 `probably`. avc1 질의 문자열(`avc1.4d4028`)은 빈 값이었지만 재생은 모두 정상이어서, 질의 문자열이 이 영상의 profile과 맞지 않은 것으로 본다(재생 판단에 쓰지 않음).

## 성능

측정 실행 파일은 커밋 `1425384`의 release 빌드 하나이고, `HARS_REMUX_ENGINE`만 바꿨다. 최적화 전(`0ed74af`, 메모리 상한·payload 디스크 staging 전) 빌드를 같은 실행에 넣어 교차 비교했다(d). WSL2 Ubuntu 24.04 x86_64, 16 logical CPU, 입력 NTFS(drvfs) 원본, 출력 ext4, warm cache. 단계마다 측정 직전 load average 0.7~1.5(다른 작업 없음, 마지막 단계 직후 2.4). 구현별 warmup 1회 + 순서를 회전·역순으로 바꾼 교차 5회. 숫자 집계: [pure-memory-2026-11-16.json](measurements/pure-memory-2026-11-16.json).

(a) 리먹싱 단독(`integration/benchmark_remux.py`, `remux` CLI 또는 Python PyAV 어댑터, 검증 포함), 13영상 합(영상별 중앙값의 합, 괄호는 회차별 합의 범위). 520표본 모두 Python 대비 검증 통과. RSS는 20 ms 표본의 프로세스 트리 합이다.

| | 경과(초) | CPU(초) | 평균 RSS 중앙값(KiB) | 최고 RSS(KiB) |
|---|---|---|---|---|
| HEVC Python | 7.905 (7.777–10.682) | 4.339 | 44,104 | 51,048 |
| HEVC FFmpeg 엔진 | 11.924 (12.015–15.484) | 5.939 | 71,330 | 100,120 |
| HEVC 순수(최종) | **5.009** (5.050–5.280) | **2.545** | 58,990 | 71,216 |
| HEVC 순수(최적화 전) | 5.471 (5.419–5.671) | 2.982 | 76,930 | 108,600 |
| TS Python | 2.342 (2.334–2.435) | 1.653 | 32,156 | 50,104 |
| TS FFmpeg 엔진 | 2.617 (2.595–2.665) | 1.920 | 46,022 | 76,404 |
| TS 순수(최종) | 2.150 (2.142–2.189) | 1.465 | 43,071 | 76,096 |
| TS 순수(최적화 전) | 2.057 (2.078–2.149) | 1.440 | 44,568 | 79,836 |

(b) 전체 변환(`integration/benchmark_conversion.py`, 로그 분석·리먹싱·검증·텔레메트리·썸네일), 3·7·9번 구간(9번은 Python과 마지막 길이 차이가 가장 큼). 모든 결과를 Python 전체 출력과 모델 포함 비교해 통과했고 경고·대체 없음. 구간별 중앙값(범위):

| 구간 | Python | FFmpeg 엔진 | 순수(최종) | 순수(최적화 전) |
|---|---|---|---|---|
| 3 경과 | 4.630 (4.522–4.968) | 3.843 (3.755–3.957) | 3.268 (3.084–3.612) | 3.212 (3.162–3.462) |
| 7 경과 | 4.658 (4.457–4.912) | 3.777 (3.679–5.008) | 3.354 (3.149–3.702) | 3.337 (3.186–3.487) |
| 9 경과 | 4.359 (4.330–5.074) | 3.771 (3.618–4.012) | 3.270 (3.132–3.321) | 3.161 (3.000–3.315) |
| 합: 경과 / CPU | 13.647 / 11.690 | 11.391 / 8.948 | **9.891 / 8.202** | 9.709 / 8.074 |
| 최고 트리 RSS(KiB, 3번) | 346,968 | 376,096 | 376,952 | 379,640 |

순수 기본은 FFmpeg 엔진보다 경과 13 %, CPU 8 % 적고 Python보다 경과 28 %, CPU 30 % 적다. 최적화 전 빌드와는 경과 +1.9 %, CPU +1.6 %로 회차 범위 안이다.

(c) 동시 변환(`integration/benchmark_concurrent.py`, 서로 다른 구간을 동시에 시작, 마지막이 끝날 때까지): 중앙값(범위), 모든 결과 Python 출력과 비교 통과.

| | Python | FFmpeg 엔진 | 순수 |
|---|---|---|---|
| 2동시 경과(초) | 4.692 (4.346–4.935) | 3.853 (3.668–4.126) | **3.248** (3.125–3.482) |
| 2동시 CPU(초) | 7.954 | 6.035 | **5.288** |
| 2동시 트리 최고 RSS(KiB) | 698,024 | 731,420 | 720,780 |
| 4동시 경과(초) | 5.066 (4.887–5.519) | 4.120 (4.010–4.148) | **3.423** (3.408–3.642) |
| 4동시 CPU(초) | 17.116 | 12.287 | **10.889** |
| 4동시 트리 최고 RSS(KiB) | 1,388,508 | 1,422,800 | 1,442,204 |

(d) 최적화 전과 최종: 메모리는 [메모리 구조와 상한](#메모리-구조와-상한), 시간은 위 (a)(b). HEVC 리먹싱은 최종이 경과 8 %, CPU 15 % 적고 평균 RSS 23 % 적다. TS 리먹싱은 경과 +4.5 %(영상당 약 7 ms)이고, 이번 작업 직전 커밋 `2d0b87d`와 같다(같은 실행 3구간: 0.483 대 0.484초). 즉 이번 작업이 아니라 payload를 디스크에 두는 메모리 구조의 비용이다.

FFmpeg 엔진(대체 경로): 같은 실행 3구간 HEVC 합 최종 2.834초, `2d0b87d` 2.980초, `0ed74af` 2.283초. 이번 작업의 parameter set 직접 제거는 `filter_units`보다 조금 빠르고, `0ed74af` 대비 증가는 그 뒤에 넣은 FFmpeg 사본의 독립 판독 검사(출력 재읽기·서명) 몫이다. 검사는 유지한다.

(e) MP4 fsync·여유 공간 판정(이번 수정)의 비용. 같은 기계, release 빌드, 구현별 warmup 1회 + 교차 5회, 측정 직전 load average 1.2~1.8(writeback 단계 시작 직후 5.1, 빌드 직후). 출력 바이트는 바뀌지 않았다: 13구간 26영상 MP4가 `3834c10` 빌드와 바이트가 같고, replay JSON은 Python과 13/13 동등하다. 숫자 집계: 같은 JSON의 `e_durability_and_low_space`.

| HEVC 13영상 리먹싱(영상별 중앙값의 합, 괄호는 회차 합 범위) | 경과(초) | CPU(초) |
|---|---|---|
| 1차: `3834c10` | 4.795 (4.706–5.002) | 2.398 |
| 1차: fsync(`460a046`) | 5.268 (5.114–5.410) | 2.634 |
| 1차: FFmpeg 엔진(`460a046`) | 11.920 (11.919–12.235) | 5.872 |
| 2차: `3834c10` | 5.007 (4.864–5.289) | 2.446 |
| 2차: fsync(`460a046`) | 5.444 (5.406–5.545) | 2.575 |
| 2차: fsync + 먼저 writeback 시작(최종 `76db648`) | **5.253** (5.102–5.460) | 2.517 |

fsync만 넣으면 HEVC 리먹싱이 약 9 % 느려졌다(영상당 약 35 ms, MP4 약 37 MB를 ext4에 쓰고 기다리는 시간). MP4를 다 쓴 직후 `sync_file_range(SYNC_FILE_RANGE_WRITE)`로 쓰기를 먼저 시작해 판독 검사와 겹치게 하자 `3834c10` 대비 +4.9 %(영상당 약 19 ms)로 줄었다. 내구성 보장(rename 전 `sync_all`, 폴더 sync)은 그대로다. 순수 엔진은 여전히 FFmpeg 엔진의 절반 이하 시간이다. 여유 공간 판정은 `statvfs` 한 번이라 측정에서 따로 보이지 않는다.

| 전체 변환 3·7·9번 합(구간별 중앙값의 합) | 경과(초) | CPU(초) |
|---|---|---|
| Python | 13.510 | 11.572 |
| FFmpeg 엔진(`460a046`) | 11.521 | 9.018 |
| 순수 fsync(`460a046`) | 9.848 | 8.142 |
| 순수 `3834c10` | 9.667 | 8.070 |

전체 변환은 fsync로 경과 +1.9 %, CPU +0.9 %이고 회차 범위 안이다(로그 분석이 대부분). 모든 결과는 Python 출력과 비교해 통과했다.

무엇이 수치를 제한하는가:

- 순수 HEVC: drvfs 입력 읽기와 SHA-256 두 번(입력 `NalHasher`, 출력 판독)이다.
- 순수 TS: 입력이 약 2.3 MB라 프로세스 기동과 FFmpeg 원본 검사 worker(영상당 약 39 ms)가 대부분이다. 순수 worker의 읽기는 약 22 ms다.
- 전체 변환: 로그 읽기/zstd, 텔레메트리 생성, 로그 분석 CPU가 대부분이고 리먹싱은 약 1/10이다.
- 동시 변환: 변환마다 CPU 위주이고 16 CPU에서 2→4동시 경과가 5 % 늘었다. 트리 RSS는 변환 프로세스의 로그 분석이 대부분이다(변환당 약 360 MB).

검증 선택: 기본 경로는 FFmpeg 출력 inspect를 쓰지 않고 독립 판독기로 출력을 검사한다. 입력 쪽 TS 검사(PES 시각·서명, 잘린 마지막 패킷)는 FFmpeg를 계속 쓴다. 순수 엔진의 PES 해석이 잘못되면 FFmpeg 검사와 PTS/DTS·서명이 달라져 대체 경로로 간다. 이 검사를 순수 Rust로 바꾸면 TS 경로의 프로세스 하나를 줄일 수 있지만, 입력 해석과 검사가 같은 코드가 되므로 하지 않았다.

## 의존성

- `shiguredo_mp4 =2026.5.0`(Apache-2.0, 하위 의존성 0, `src/`의 unsafe 0)과 `mpeg2ts =0.6.1`(MIT, 하위 의존성 0, unsafe 0)이 기본 의존성이 되었다. Cargo feature `pure-remux`는 없앴다.
- `=` 고정 유지: `shiguredo_mp4`는 연 단위 버전이고 출력 바이트(moov 구성)가 판독 검사와 크기 계산에 직접 쓰인다. 올릴 때는 이 문서의 26영상 감사를 다시 돌린다.

## 남은 차이와 미검증

- Python과의 차이: 마지막 sample 길이와 그 파생 필드(규칙 문서의 표). 컨테이너 부가 박스(elst, btrt, udta, mvhd/tkhd timescale, chunk 구성, Python 쪽 8바이트 free). TS 마지막 패킷이 영상 payload 안에서 잘린 입력은 Python이 마지막 프레임 바이트를 잃은 채 게시하고 HARS는 그 카메라를 실패로 끝낸다([손상 입력 사례](#손상-입력-사례)).
- 패킷 경계에서 잘린 TS처럼 파일 자체의 마지막 프레임이 불완전한 경우는 컨테이너만으로 알 수 없어 Python과 같이 게시한다(파일의 바이트는 모두 있다). 픽셀 디코딩 검사는 하지 않는다.
- 실주행 B-frame TS, field 부호화, scaling matrix SPS는 코퍼스에 없다. B-frame은 실주행 qcamera를 다시 인코딩한 입력(시각 보존)으로 제품 경로까지 확인했고, 나머지는 합성 시험과 대체 경로로만 확인했다.
- aarch64(HAOS) 미검증. 규칙 계산은 정수 연산만 쓴다. HEVC 시각 변환 `round_ties_even(t × 1e6)`은 IEEE 754 곱셈 1회와 반올림이라 아키텍처·FMA와 무관하다(덧셈이 없어 융합 대상이 아니다).
- Safari/iOS, Firefox, Android WebView, HA Ingress 경유 재생은 미검증.

## 재현

```sh
cargo test --test pure_remux --test terminal_duration --test ts_duration --test pure_remux_faults \
  --test archive_pure_remux --test conversion_resource --test conversion_keep_previous
python integration/real_conversion_compare.py CORPUS BINARY ORACLE OUT --limit 13
python integration/pure_remux_audit.py CORPUS OUT ORACLE
python integration/pipeline_contract.py BINARY
python integration/boundary_inputs.py BINARY WORKDIR REPORT.json          # 손상 입력, Python과 비교
systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=0 -p OOMPolicy=continue \
  python3 integration/pure_remux_memory.py BINARY NEW_WORKDIR
python integration/pure_default_product.py CORPUS BINARY NEW_OUT --ports UI DEVICE
python integration/ui_view_check.py --browser EXE --profile NEW_DIR --port N --server URL --record ID --output OUT.json
python integration/benchmark_concurrent.py CORPUS NEW_OUT --segments 3 7 9 1 --jobs 2 4 --runs 5 --binary pure=BINARY@pure
```
