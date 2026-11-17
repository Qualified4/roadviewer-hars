# 구현 및 검증 현황

현재 버전은 **2.0.0-alpha.4**이다. 2.0.0-alpha.4에서 FFmpeg 의존성(CLI, `ffprobe`, `ffmpeg-next`, libav*)을 제품 빌드·실행·배포 이미지에서 완전히 제거했다([FFMPEG-REMOVAL.md](FFMPEG-REMOVAL.md)). Python Road Viewer 1.3.16의 53개 API 라우트가 모두 등록되어 있고 `501 feature_not_ported`를 반환하는 경로는 없다(1.3.18까지 Python 서버 코드 변경이 없어 API 기준은 그대로다). 프런트엔드 기능 기준: Python Road Viewer 1.3.18(`d3de134`). `main`의 웹 자산과 썸네일 처리는 게시된 `2.0.0-alpha.4` 이미지와 다르다. 자산이 버전이 붙은 주소로 1년간 캐시되므로 **다음 게시는 반드시 버전을 올려서 한다**(같은 버전으로 다시 내면 브라우저가 새 페이지와 옛 자산을 섞어 쓸 수 있다). 운영 전환 후보는 아니다. 실제 HAOS, 실제 Google Drive·NAS, 실제 차량 장치에서의 검증이 남아 있다. 정식 `2.0.0` 릴리스, 운영 포트 승계, Python 설정 변경, openpilot 목적지 변경은 수행하지 않았다.

검증 근거는 [VALIDATION.md](VALIDATION.md), 측정 조건과 수치는 [PERFORMANCE.md](PERFORMANCE.md), 라우트별 상태는 [api-parity.json](api-parity.json)(Python 라우트 목록의 출처는 [inventory.json](inventory.json)), 전환 절차는 [CUTOVER.md](CUTOVER.md)에 있다.

## 범위별 상태

"검증"은 자동 테스트 또는 실제 실행 파일로 확인한 것이고, "미검증"은 이 환경에서 확인하지 못한 것이다.

| 범위 | 구현 | 검증 | 미검증·남은 차이 |
|---|---|---|---|
| 장치 페어링·RV1 HMAC·nonce·Bearer | 완료 | 고정 커밋 `client.py`로 CI에서 매 커밋 확인, 만료·폐기·nonce 재사용 테스트 | 실제 차량 장치 |
| 조각 업로드·재전송·재개·영수증 | 완료 | TCP 종료, offset 충돌, SIGKILL 7개 지점, finish 응답 유실, 슬롯 고갈, ENOSPC(`/dev/full`, `RLIMIT_FSIZE`) | 전원 차단·OS 크래시 |
| 중복 업로드·기존 구간에 카메라 추가 | 완료(Python 정책) | 장치·브라우저·레거시 경로, SIGKILL 6개 지점 복구 | |
| 저장 공간 제한·업로드 거부 | 완료 | `admit` 계산식 경계, 507 코드 3종 | 실제 디스크 고갈(tmpfs는 로컬 1회) |
| 오래된 데이터 자동 정리 | 완료 | 주행 단위 그룹, 사전 점검 무삭제, 고정·업로드 중·변환 중 보호, 경합 테스트 | |
| 고정(구간 단위)·`pinned_routes` 이전 | 완료 | 전체·부분·미고정 혼합 주행 | |
| 수동 삭제·삭제 저널 | 완료 | 중단 지점별 재시작 복구, 손상 저널 격리 | |
| Google Drive 아카이빙 | 완료 | 모의 Drive 서버: OAuth 장치 흐름, 토큰 갱신, resumable 업로드 재개, 검증, move/copy, 복구, 수동 실행 | 실제 Google, `drive.file` 범위에서 Python 파일 열람 |
| NAS 아카이빙 | 완료 | 임시 폴더: 복사·검증·move, 미마운트·끊김, 정리, 복구, 취소 경합 | 실제 SMB 마운트 |
| 원격 파일 재생·다운로드·재변환 | 완료 | Drive Range 스트리밍, NAS 경로, 아카이브 입력 재변환, 끊긴 요청의 임시 사본 정리 | Ingress 경유 |
| 지도 보충(`tracks/run`·`stop`) | 완료 | 모의 Drive, GET 요청만 나감 | 실제 실행 파일 e2e |
| Python 원격 데이터 읽기 전용 가져오기 | 완료(HARS 전용 API·CLI) | 요청 로그 GET 전용 단언, 읽기 전용 NAS, SIGKILL 3개 지점 | 실제 Drive 폴더 검색 |
| 원격 쓰기 범위 분리 | 완료 | 소유 표식 없는 대상의 쓰기·삭제 거부, Python 구조 데이터 불변 | |
| 내보내기·가져오기(tar)·ZIP | 완료 | Python과 양방향 교차, 경로 탈출 거부, 중단 후 정리 | 4 GiB 초과 ZIP |
| 기록 목록·검색·정렬·지도·북마크·태그 | 완료 | 실제 Python 서버와 응답 비교 | |
| 네트워크 설정·재시작(Supervisor) | 완료 | 모의 Supervisor | 실제 Supervisor |
| 로그 분석·replay·telemetry·GPS·summary | 완료 | 실제 61구간·72,749프레임, 원본 13구간 | 추가 차량·지역 |
| 영상 리먹싱 | 순수 Rust 엔진만(FFmpeg 엔진·대체 경로 제거) | 원본 13구간 26영상, 14·47구간 회귀, 경계·장애 테스트, FFmpeg 없는 컨테이너에서 변환 | aarch64 실기기, Safari·Firefox. B-frame·interlaced 등은 지원하지 않음(아래 표) |
| 마지막 프레임 길이 | rlog의 다음 카메라 프레임 기준 | 연속 12쌍에서 실제 간격과 0~1 tick | [TERMINAL-DURATION.md](TERMINAL-DURATION.md) |
| 기존 MP4 재사용 | 완료 | 61구간 | |
| 원본 형식 내보내기(역변환) | 순수 Rust([FFMPEG-REMOVAL.md](FFMPEG-REMOVAL.md)) | 26영상 왕복: NAL·프레임·키프레임·PTS/DTS·디코딩 MD5 26/26, TS는 원본과 바이트 동일, 손상 입력 300건 | HEVC는 PPS 뒤 0 바이트 60개가 원본과 다름(MP4에 남지 않는 정보, 이전 FFmpeg 역변환과는 동일). 4 GiB 초과 |
| 썸네일 | 순수 Rust 디코더·축소·JPEG([THUMBNAIL-DECODER.md](THUMBNAIL-DECODER.md)) | 5,040프레임 디코딩이 FFmpeg과 비트 동일, 85개 입력에서 고른 프레임·점수·메타·축소 결과 동일 | JPEG 바이트는 다름(PSNR 중앙값 55 dB). aarch64 실측. 10비트·4:2:2·4:4:4·interlaced는 썸네일 없음 |
| 업로드·원본 영상 검사 | 순수 Rust, 별도 worker 프로세스 | 기존 `ffprobe` 검사와 138건 대조: 기존이 거부한 것을 새로 수락한 경우 0 | |
| 배포 이미지 | FFmpeg 없음. 런타임 패키지는 `nginx`, `ca-certificates` | amd64 로컬 측정, CI가 두 아키텍처에서 FFmpeg 잔존 검사와 크기 측정 | 수치는 [PERFORMANCE.md](PERFORMANCE.md) |
| 변환 큐·취소·복구 | 완료 | worker 강제 종료, 쓰기 한도, 프로세스 그룹 종료 | |
| HAOS 독립 설치 구성 | 완료 | `scripts/check_coexistence.py`, amd64·aarch64 컨테이너 기동·TLS | 실제 HAOS 병행 설치·재시작·Ingress |
| 인증 오프라인 승계 | 완료 | Python 발급 상태로 재페어링 없이 업로드(CI) | 실제 운영 스냅샷 |

## Python과 의도적으로 다른 점

- **UI 오류 형식.** `{"error": "<코드>", "message": "<Python 문장>"}`이다. Python은 `error`에 문장을 넣는다. 화면은 `message`를 먼저 표시하므로 보이는 문장은 같다.
- **영상 길이.** 마지막 프레임 길이를 Python의 프레임레이트 추정이 아니라 rlog의 실제 타이밍으로 정한다. `videos.*.duration`, `video.duration`, replay·summary·telemetry의 `duration`이 구간별로 -3.96 ms ~ +1.16 ms 다르다. 주행 요약은 같다.
- **원격 쓰기 범위.** HARS는 `Road Viewer HARS` 폴더(Drive)와 owner `roadviewer-hars` sentinel(NAS)에만 쓴다. Python의 폴더는 읽기만 한다.
- **잘린 qcamera.** TS의 마지막 패킷이 영상 데이터 중간에서 잘린 파일을 Python은 그 데이터를 버린 채 게시한다. HARS는 그 카메라를 실패로 끝내고 원본을 보존한다. 패킷 경계에서 잘린 파일은 Python처럼 게시한다.
- **자원 실패와 기존 결과.** 리먹싱이 디스크 부족·시간 초과·메모리 상한·강제 종료로 실패했을 때, 디스크에 그 카메라를 재생할 수 있는 게시 결과가 있으면 이번 시도를 버리고 그 결과로 되돌린다. Python은 그 카메라가 빠진 결과를 게시한다. 게시 결과가 없으면 Python과 같다.
- **자원이 아닌 실패와 기존 결과(alpha.4).** 엔진이 입력을 거부했을 때(unsupported·invalid·verification·internal) 디스크의 게시 결과가 그 카메라를 재생할 수 있으면 그 카메라의 이전 MP4만 이어받고 나머지 카메라는 변환해 게시한다. 카메라 추가 업로드는 Python처럼 `prepared/`를 먼저 비우므로 이 경우에는 거부된 카메라가 경고와 함께 빠진다(원본은 보존).
- **여유 공간 부족.** 여유 공간이 입력의 2배 + 100 MiB보다 적으면 쓰기 전에 자원 실패로 끝낸다. alpha.3은 이 경우 FFmpeg 엔진으로 사본 하나만 써서 변환했다.
- **삭제.** 숨김 rename을 확정 시점으로 하는 저널을 쓴다. Python은 `rmtree` 한 번이다.
- **finish 중 크래시.** 등록 저널로 복구해 같은 영수증을 돌려준다. Python은 영수증을 잃는다.
- **보충 변환.** `backfill.py`·`gap_backfill.py`는 이식하지 않았다. 옛 Python 변환기가 쓰지 않던 값을 채우는 용도이고 HARS 변환기는 처음부터 그 값을 쓴다. Python에서 가져온 옛 기록의 해당 값은 재변환으로 채운다.
- **지도 보충의 대기 조건.** Python은 영상 재생 중이면 기다린다. HARS는 업로드·변환·보관 실행만 본다.
- **썸네일.** 요청 시 즉시 생성하지 않고 변환 때 만든다.

## alpha.4에서 지원 범위가 달라진 입력

FFmpeg 대체 경로가 없어지면서 순수 엔진이 거부하는 입력은 명시적 실패로 끝난다. comma 장치가 만드는 영상과 실주행 코퍼스(13·14·47구간)에는 해당 사례가 없다. 실패해도 원본과 이전 게시 결과는 보존된다. 전체 표와 근거는 [FFMPEG-REMOVAL.md](FFMPEG-REMOVAL.md)에 있다.

| 입력 | alpha.3 | alpha.4 |
|---|---|---|
| B-frame TS, interlaced TS, SPS scaling matrix, PES 하나에 그림 여러 장 | FFmpeg 엔진이 변환 | 변환 실패(unsupported) |
| PAT/PMT가 손상됐거나 첫 패킷 앞에 쓰레기가 있는 TS | FFmpeg 엔진이 변환(추정) | 변환 실패(invalid) |
| edit list로 미디어를 지연시킨 MP4(B-frame MP4), NAL 길이 필드가 4가 아닌 MP4, sample entry가 여러 개인 MP4 | 수락 | 실패 |
| hev1·다중 트랙 MP4, avcC와 in-band SPS가 다른 MP4의 원본 형식 내보내기 | FFmpeg이 내보냄 | 내보내기 실패 |
| H.264 4:2:2·4:4:4·10비트·interlaced, HEVC Main10·4:4:4의 썸네일 | 생성 | 썸네일 없음(변환은 영향 없음) |
| 여유 공간이 입력의 2배 + 100 MiB 미만 | FFmpeg 엔진이 변환 | 자원 실패 |
| scrambling 플래그가 선 TS, 비영상 패킷에서 잘린 TS, moov가 mdat 뒤인 MP4, 오디오 트랙이 더 있는 MP4, hev1/avc3 MP4 입력 | FFmpeg 대체 또는 거부 | 순수 엔진이 변환 |

openpilot loggerd가 기록하는 형식(1928×1208·1344×760 HEVC Main, 526×330 H.264 High, 오디오가 든 qcamera.ts)은 모두 지원된다. PC·시뮬레이터 출력(FFVHUFF/Matroska, libx264 기본 B-frame TS)은 받지 않는다. 썸네일을 만들지 못해도 기록은 `ready`가 되고 영상·로그·다운로드는 그대로 쓸 수 있다. 근거와 검증은 [FORMAT-SUPPORT.md](FORMAT-SUPPORT.md).

## 운영 전환 전 남은 위험

1. 실제 HAOS에서 두 앱의 병행 설치, Ingress, 재시작, 포트 승계를 확인하지 못했다.
2. 실제 Google Drive와 SMB 마운트에 접속하지 않았다. 모든 원격 테스트는 모의 서버와 임시 폴더다.
3. `drive.file` 범위에서는 같은 OAuth client ID로 만든 파일만 보인다. HARS가 Python 파일을 읽으려면 같은 client ID가 필요하다. Drive 연결 해제는 토큰을 revoke하므로 같은 client를 쓰는 Python의 연결도 끊길 수 있다.
4. Python의 `.archive.json`을 HARS 데이터 폴더에 복사하면 안 된다.
5. NAS에서 확인과 열기 사이에 파일을 심볼릭 링크로 바꿔치기하는 경합은 막지 못한다. NAS 쓰기 권한이 있는 공격자가 전제다.
6. 실제 차량 장치의 장시간·동시 업로드, 전원 차단, aarch64 실기기 재생은 확인하지 못했다.
7. 썸네일 디코더(`rusty_h264-decoder`, `rusty_h265`)는 2026년에 나온 crate다. 별도 worker 프로세스에서 메모리·시간 상한 아래 돌고 실패는 썸네일 누락으로만 이어진다.
8. TS 원본 검사와 리먹싱 엔진이 같은 `mpeg2ts` demuxer를 쓴다. 출력 MP4는 독립 판독기로 확인하지만 입력 판독의 독립 대조(이전의 libavformat)는 없어졌다.
9. 녹음이 든 qcamera.ts는 원본 보존을 끄면 오디오가 남지 않는다(Python과 같음).
10. 원본 형식 ZIP은 내보낼 수 없는 영상(예: B-frame MP4)이 하나라도 있으면 `media_export_failed`로 전체가 실패한다. 재생용 형식 다운로드와 개별 파일은 받을 수 있다.
