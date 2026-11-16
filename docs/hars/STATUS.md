# 구현 및 검증 현황

현재 버전은 **2.0.0-alpha.3**이다. Python Road Viewer 1.3.16의 53개 API 라우트가 모두 등록되어 있고 `501 feature_not_ported`를 반환하는 경로는 없다. 운영 전환 후보는 아니다. 실제 HAOS, 실제 Google Drive·NAS, 실제 차량 장치에서의 검증이 남아 있다. 정식 `2.0.0` 릴리스, 운영 포트 승계, Python 설정 변경, openpilot 목적지 변경은 수행하지 않았다.

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
| 영상 리먹싱 | 순수 Rust 엔진이 기본, FFmpeg 엔진은 대체 경로 | 원본 13구간 26영상, 두 엔진 출력 동일, 경계·장애 테스트 | aarch64 실기기, Safari·Firefox, 실주행 B-frame·interlaced |
| 마지막 프레임 길이 | rlog의 다음 카메라 프레임 기준 | 연속 12쌍에서 실제 간격과 0~1 tick | [TERMINAL-DURATION.md](TERMINAL-DURATION.md) |
| 기존 MP4 재사용·역변환·썸네일 | 완료(FFmpeg 사용) | 61구간, 역변환 NAL·PTS 일치 | 썸네일 픽셀 동일성(점수 ±0.01) |
| 변환 큐·취소·복구 | 완료 | worker 강제 종료, 쓰기 한도, 프로세스 그룹 종료 | |
| HAOS 독립 설치 구성 | 완료 | `scripts/check_coexistence.py`, amd64·aarch64 컨테이너 기동·TLS | 실제 HAOS 병행 설치·재시작·Ingress |
| 인증 오프라인 승계 | 완료 | Python 발급 상태로 재페어링 없이 업로드(CI) | 실제 운영 스냅샷 |

## Python과 의도적으로 다른 점

- **UI 오류 형식.** `{"error": "<코드>", "message": "<Python 문장>"}`이다. Python은 `error`에 문장을 넣는다. 화면은 `message`를 먼저 표시하므로 보이는 문장은 같다.
- **영상 길이.** 마지막 프레임 길이를 Python의 프레임레이트 추정이 아니라 rlog의 실제 타이밍으로 정한다. `videos.*.duration`, `video.duration`, replay·summary·telemetry의 `duration`이 구간별로 -3.96 ms ~ +1.16 ms 다르다. 주행 요약은 같다.
- **원격 쓰기 범위.** HARS는 `Road Viewer HARS` 폴더(Drive)와 owner `roadviewer-hars` sentinel(NAS)에만 쓴다. Python의 폴더는 읽기만 한다.
- **삭제.** 숨김 rename을 확정 시점으로 하는 저널을 쓴다. Python은 `rmtree` 한 번이다.
- **finish 중 크래시.** 등록 저널로 복구해 같은 영수증을 돌려준다. Python은 영수증을 잃는다.
- **보충 변환.** `backfill.py`·`gap_backfill.py`는 이식하지 않았다. 옛 Python 변환기가 쓰지 않던 값을 채우는 용도이고 HARS 변환기는 처음부터 그 값을 쓴다. Python에서 가져온 옛 기록의 해당 값은 재변환으로 채운다.
- **지도 보충의 대기 조건.** Python은 영상 재생 중이면 기다린다. HARS는 업로드·변환·보관 실행만 본다.
- **썸네일.** 요청 시 즉시 생성하지 않고 변환 때 만든다.

## 운영 전환 전 남은 위험

1. 실제 HAOS에서 두 앱의 병행 설치, Ingress, 재시작, 포트 승계를 확인하지 못했다.
2. 실제 Google Drive와 SMB 마운트에 접속하지 않았다. 모든 원격 테스트는 모의 서버와 임시 폴더다.
3. `drive.file` 범위에서는 같은 OAuth client ID로 만든 파일만 보인다. HARS가 Python 파일을 읽으려면 같은 client ID가 필요하다. Drive 연결 해제는 토큰을 revoke하므로 같은 client를 쓰는 Python의 연결도 끊길 수 있다.
4. Python의 `.archive.json`을 HARS 데이터 폴더에 복사하면 안 된다.
5. NAS에서 확인과 열기 사이에 파일을 심볼릭 링크로 바꿔치기하는 경합은 막지 못한다. NAS 쓰기 권한이 있는 공격자가 전제다.
6. 실제 차량 장치의 장시간·동시 업로드, 전원 차단, aarch64 실기기 재생은 확인하지 못했다.
