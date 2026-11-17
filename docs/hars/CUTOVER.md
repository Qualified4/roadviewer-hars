# HAOS 병행 설치·업로드 주소 승계·롤백

**현재는 실행할 수 있는 운영 전환 후보가 아니다. 전체 기능·호환성·실제 HAOS 검증이 끝나야 한다. 이 문서는 승인 후 수행할 절차이며 실제 운영 변경을 실행하지 않았다.**

## 병행 설치

포트 승계 전에 HARS를 격리 상태로 설치해 확인하는 계획과 HARS만 제거하는 절차는 [HAOS-VALIDATION.md](HAOS-VALIDATION.md)에 있다.

| 항목 | Python 1.x | HARS 2.x |
|---|---|---|
| 저장소 | Qualified4/roadviewer-ha | Qualified4/roadviewer-hars |
| slug | roadviewer | roadviewer_hars |
| 이미지 | ghcr.io/qualified4/roadviewer-ha | ghcr.io/qualified4/roadviewer-hars |
| Ingress | 기존 경로·사이드바 | Supervisor가 부여한 독립 경로·HARS 사이드바 |
| 컨테이너 Ingress 포트 | 8099 | 8099 |
| 컨테이너 HTTPS 포트 | 8443 | 8443 |
| 호스트 HTTPS 포트 | 현재 운영 포트 유지 | 기본 비활성(null), 검증 시 사용하지 않는 별도 포트 |
| 로컬 데이터 | 해당 앱 전용 /data/roadviewer | 해당 앱 전용 /data/roadviewer-hars |
| 인증 상태 | 해당 데이터 루트/.devices.json | 해당 데이터 루트/.devices.json |
| 아카이브 쓰기 위치 | Drive 폴더 `Road Viewer`, NAS sentinel owner `roadviewer` | Drive 폴더 `Road Viewer HARS`(`hars_owner=roadviewer-hars`), NAS sentinel owner `roadviewer-hars` |
| Supervisor API | `hassio_api: true` | `hassio_api: true`(자기 앱의 포트 매핑 조회·변경·재시작만 호출) |
| 리먹싱 엔진 | PyAV | 순수 Rust만(2.0.0-alpha.4부터 FFmpeg 없음. `HARS_REMUX_ENGINE=ffmpeg`은 경고 후 순수 엔진 사용) |

## 같은 Drive 계정·NAS를 함께 쓸 때

구성 수준과 모의 서버로 검증했다. 실제 Google Drive와 SMB 마운트에서는 검증하지 않았다.

- HARS는 소유 표식이 있는 자기 루트에만 쓰고 지운다. 표식이 없는 대상의 쓰기·삭제는 요청을 보내기 전에 거부한다. NAS는 쓰기 직전에 sentinel을 다시 읽는다.
- Python이 보관한 기록은 `POST /api/settings/archive/legacy/scan`과 `legacy/import`로 읽기 전용으로 조회하고 HARS 로컬로 복사한다. 원격에는 조회와 다운로드 요청만 나간다. 태그·북마크·고정은 원격에 없어 옮겨지지 않는다.
- **같은 OAuth client ID가 필요하다.** Drive 권한이 `drive.file` 범위라서 다른 client로 만든 파일은 보이지 않는다. HARS 옵션 `archive_gdrive_client_id`/`archive_gdrive_client_secret`에 Python과 같은 값을 넣는다.
- **HARS에서 Drive 연결을 해제하면 토큰을 revoke한다.** 같은 client와 계정을 쓰는 Python의 연결도 끊길 수 있다. 병행 운영 중에는 HARS의 연결 해제를 피하거나 Python 재연결을 준비한다.
- **Python의 `.archive.json`을 HARS 데이터 폴더에 복사하지 않는다.** storage id가 섞여 Drive는 `unavailable`이 되고 NAS는 sentinel을 새로 만들지 않는다.
- NAS에서 두 앱은 서로 다른 하위 폴더를 쓴다. HARS 경로를 Python의 공유 폴더로 지정하면 `not_owned`로 거부된다.
- owner 표식이 없는 구버전 Python sentinel의 NAS 폴더는 읽기 전용 가져오기도 거부한다.
- 읽기 전용 가져오기에서 NAS 파일과 체크섬이 없는 Drive 파일은 크기, mtime, 로컬 재해시로만 받아들인다. 결과의 `verified_by`가 이를 드러낸다(`size`, `rv_sha256`, `sha256Checksum`, `md5Checksum`, `readback`). 파일이 20만 개를 넘는 원격은 `422 legacy_too_large`로 거부한다.

앱별 `/data`는 Supervisor에서 격리한다. 두 컨테이너에 같은 데이터 호스트 경로를 연결하지 않는다. 같은 NAS/Drive 계정이어도 아카이브의 루트·삭제 소유권을 분리해야 한다. HARS에 기존 Python 아카이브의 이동·삭제 권한을 주지 않는다.

개발 시 HARS 호스트 포트는 예를 들어 18443처럼 현재 사용하지 않는 포트를 선택한다. 이 숫자가 사용 가능한지는 HAOS에서 직접 확인해야 한다. 기존 포워딩·DNS·인증서 설정과 openpilot URL을 변경하지 않는다. 테스트 클라이언트만 테스트 주소를 사용한다.

HARS 앱 옵션 `device_https_enabled`는 기본 false이며 HTTPS 포트 매핑도 null이다. 외부 TLS 프록시는 Supervisor가 알려 준 호스트 포트가 있고 이 옵션이 켜져 있을 때만 뜬다. 설정 UI의 포트 변경과 재시작은 Supervisor의 `addons/self` API로 동작하며, 모의 Supervisor로만 검증했다. Ingress와 외부 장치 API는 분리되어 있다.

## 승인 전 완료해야 할 게이트

- `STATUS.md`의 "미검증" 항목 중 실제 HAOS·실제 Drive/NAS·실제 장치 항목을 운영 환경의 격리된 테스트로 확인함.
- 동일 원본 로그의 재생 JSON·텔레메트리·GPS·영상/로그 시간축에 대한 Python/Rust 동등성 확인.
- 실제 qcamera/fcamera/ecamera 영상의 프레임·비트스트림·브라우저 재생/시킹 확인.
- 실제 ccnc-hda1 장치에서 테스트 주소로 업로드 중단·재연결·영수증 복구·장치 폐기 검증.
- 저장 공간 부족·아카이브 장애·재시작 중에도 데이터 손실/중복 삭제가 없음.
- HAOS에서 두 앱의 독립 Ingress·데이터·포트·재시작을 확인.
- 설치·업그레이드·컨테이너 권한·파일 복사 경로 확인. 현재 환경에는 실제 HAOS 접근이 없다.

## 승인 후 전환 절차

1. **운영 작업 확인 및 업로드 drain.** Python의 변환·보관 이동·삭제·업로드와 다른 작업을 확인한다. 현재 업로더가 배치를 끝내도록 기다린 뒤 새 업로드 작업을 중단한다. 기존 Python에는 원자적인 drain API가 없으므로, 페어링 취소나 장치 revoke를 drain으로 대체하면 안 된다. 안전한 클라이언트 정지 방법을 운영 환경에서 확인한다. `.uploads`에 세션/예약이 전혀 없는지 확인한다. 활성 세션을 옮기지 않는다.
2. **일관된 오프라인 인증 스냅샷.** 원본 `.devices.json`의 접근 제한된 복사본을 만든다. 마스터 비밀값·장치 ID·nonce·영수증을 채팅이나 로그로 출력하지 않는다. Python 원본 파일을 변경하지 않는다. Python이 새 인증 요청을 받지 않는 상태에서 복사본을 확정한다. 테스트 HARS 상태는 별도로 백업하고 HARS 쓰기 프로세스를 중지한 상태에서 새 데이터 루트에 가져온다. 기존 HARS 인증 파일을 덮어쓰지 않는다.
3. **HARS 인증 검증.** `roadviewer-hars import-auth OFFLINE_SNAPSHOT NEW_EMPTY_HARS_DATA`를 실행한다. 도구는 업로드가 남은 스냅샷·잘못된 상태·이미 존재하는 대상 인증 파일·공유/중첩 경로·실행 중인 대상 프로세스를 거부한다. import는 본문을 출력하지 않고 장치 수만 보고한다. 이전과 같은 HMAC 파생식이므로 장치 ID와 마스터가 보존되면 기존 장치 비밀값을 재생성할 수 있다. 실제 원본 ccnc client와의 오프라인 통합 시험으로도 검증한다. 새 테스트 주소에서 기존 장치 인증을 검증하되, 클라이언트 URL 변경은 운영 승인 범위 내의 격리된 검사만 수행한다.
4. **Python 외부 포트 매핑만 해제.** 앱·사이드바·Ingress·데이터는 유지한다. 필요 시 Supervisor의 앱 재시작 전에 다시 실행 중인 작업을 확인한다. 포트 설정 반영을 위한 재시작은 짧은 UI 접속 중단을 일으킬 수 있으며 무중단을 보장하지 않는다.
5. **HARS에 기존 호스트 HTTPS 포트 배정.** 기존 포트가 비워졌는지 확인한 뒤 배정한다. 동시에 두 앱에 같은 호스트 포트를 지정하지 않는다. DNS·외부 URL·라우터 전달 목적지를 불필요하게 바꾸지 않는다. 포워딩 대상 호스트 포트가 그대로라면 openpilot 업로드 주소도 유지된다.
6. **TLS 및 외부 접근 확인.** 기존 도메인으로 접속해 인증서 이름·만료·전체 체인을 검증한다. HARS의 `/api/device/test`만 외부에서 제공되고 `/api/logs`·설정·Ingress 콘텐츠가 차단되는지 확인한다. HARS와 Python의 Ingress는 각각 별도로 확인한다.
7. **실제 장치 업로드 확인.** 원래 openpilot URL로 새 배치 1개를 전송하고 로그/영상 크기·SHA-256·완료 영수증·재연결·변환·재생·시킹을 확인한다. Nginx request ID와 Rust request ID로 수신·서버 진입·락 대기·쓰기·응답 시각을 연결한다. 재시도/타임아웃을 늘려 오류를 감추지 않는다.
8. **안정화 또는 롤백.** 오류율과 지연이 합의된 기준을 만족할 때만 신규 데이터 수신을 유지한다. 안정화 전에는 Python 1.x 앱·데이터·인증 백업을 제거하지 않는다.

인증 이전 도구는 오프라인 데이터 파일만 다룬다. HAOS의 앱별 전용 `/data`에 접근·복사하는 실제 관리 경로는 HAOS 검증 후 확정해야 한다. 현재 도구를 HAOS 포트 자동 승계 도구라고 간주하면 안 된다.

## 롤백

1. HARS의 새 배치를 중단하고 활성 업로드·변환·보관 작업이 끝날 때까지 기다린다. HARS에서 받은 신규 데이터는 그 앱의 데이터 루트에 보존한다.
2. 두 서버의 외부 인증 수신을 멈춘 상태에서 HARS의 최신 `.devices.json` 오프라인 복사본을 검증한다. 기존 root·장치 ID 형식, 장치 폐기 상태, nonce·영수증을 포함한다. **과거 Python 인증 백업만 무조건 복원하면 안 된다.** HARS에서 소비한 nonce가 Python에 없으면 시간 창 안에서 재사용될 수 있다.
3. Python이 인증 상태를 메모리에 보유하므로, 최신 호환 상태를 설치하려면 쓰기/인증 프로세스 정지와 재시작이 필요하다. 진행 중인 작업을 확인하고 수행한다. 양쪽 상태 파일을 동시에 수정하거나 공유하지 않는다. 이 역방향 가져오기 절차는 실제 Python 상태·HAOS에서 별도 검증이 필요하다.
4. HARS의 기존 호스트 포트 매핑을 해제하고 Python에 원래 포트를 다시 배정한다. 먼저 포트 해제를 확인한다.
5. 기존 URL의 TLS·장치 인증·새 업로드와 두 앱의 Ingress를 확인한다. 장치 인증 상태가 동일하게 보존되면 재페어링을 요구하지 않는다. 복원이 불가능하면 원인을 조사하고 장치 재페어링 필요 여부를 별도로 결정한다.
6. HARS의 신규 데이터는 Python 데이터 폴더에 합치지 않는다. 필요한 데이터 이동은 별도 검증된 읽기/가져오기 절차로 수행한다.

HARS에서 만든 완료 영수증이 가리키는 기록은 HARS에 남을 수 있다. Python으로 롤백했다고 그 기록이 Python UI에서 보인다고 가정하면 안 된다. 두 앱의 데이터와 조회 경로를 그대로 유지한다.
