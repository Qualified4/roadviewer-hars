# 실제 HAOS 병행 설치 검증 준비 (2.0.0-alpha.4)

**이 문서는 준비 점검과 계획이다. 태그 생성, 릴리스·GHCR 게시, HAOS 설치는 아직 실행하지 않았다.** 운영 포트 승계와 Python으로의 롤백은 [CUTOVER.md](CUTOVER.md)가 다루고, 이 문서는 그 앞 단계인 "Python 옆에 HARS를 설치해 격리 상태로 확인하고, 문제가 있으면 HARS만 걷어내기"를 다룬다.

검증 중에도 기존 Python Road Viewer, openpilot의 업로드 주소, 운영 HAOS의 외부 포트·도메인·포워딩은 바꾸지 않는다.

## 충돌 점검 (저장소 기준, 2026-11-17)

`scripts/check_coexistence.py`가 CI에서 매 커밋 확인한다. Python 쪽 값은 보존된 `reference/python/roadviewer/config.yaml.reference`(1.3.16) 기준이며, 운영 중인 Python 앱의 실제 설정과 같은지는 HAOS에서 확인해야 한다.

| 항목 | Python 1.x | HARS 2.x | 충돌 |
|---|---|---|---|
| slug | `roadviewer` | `roadviewer_hars` | 없음 |
| 이미지 | `ghcr.io/qualified4/roadviewer-ha` | `ghcr.io/qualified4/roadviewer-hars` | 없음 |
| 앱 이름·사이드바 | Road Viewer | Road Viewer HARS | 없음 |
| 데이터 | 앱 전용 `/data/roadviewer` | 앱 전용 `/data/roadviewer-hars` | 없음. `/data`는 Supervisor가 slug별로 격리한다 |
| Ingress | 컨테이너 8099 | 컨테이너 8099 | 없음. 컨테이너 내부 포트이고 Supervisor가 앱마다 다른 경로를 준다 |
| 호스트 HTTPS 포트 | 운영 포트 사용 중 | `8443/tcp: null`(게시 안 함) | 기본값에서는 없음. HARS에 포트를 줄 때 Python의 포트와 다른 값을 골라야 한다 |
| 자동 시작 | `boot: auto` | `boot: manual` | HARS는 호스트 재부팅 뒤 스스로 뜨지 않는다 |
| `/ssl` | 읽기 전용 | 읽기 전용 | 없음 |
| `/share` | 읽기·쓰기 | 읽기·쓰기 | NAS 보관을 켤 때만 관련. HARS는 자기 sentinel이 있는 폴더에만 쓴다([CUTOVER.md](CUTOVER.md)) |
| 백업 제외 | `roadviewer/...` | `roadviewer-hars/...` | 없음 |

## 이미지 빌드·게시 설정

- 앱 설정의 `image`에 아키텍처 자리표시자가 없으므로 HAOS는 `ghcr.io/qualified4/roadviewer-hars:<version>` 하나를 받는다. 이 태그는 amd64와 aarch64를 묶은 manifest여야 한다.
- 게시는 `.github/workflows/prerelease.yml`이 한다. `2.0.0-alpha.*` 태그가 push되면 다음 순서로 돈다.
  1. `gate`: `scripts/check_release.py --tag`(태그와 `Cargo.toml`·`config.yaml`·`Dockerfile`의 버전 일치), fmt, clippy, 전체 테스트.
  2. `image`(amd64, aarch64 각각의 네이티브 러너): 빌드 → FFmpeg 잔존 검사 → `scripts/container_smoke.py`(기동, TLS, 변환, 내보내기, 썸네일) → 통과한 뒤에만 `:<tag>-<arch>` push.
  3. `manifest`: 두 이미지를 `:<tag>`로 묶고 GitHub prerelease를 만든다(본문은 `STATUS.md`).
- 현재 상태: 저장소에 태그와 릴리스가 없고 GHCR 패키지 `roadviewer-hars`도 아직 없다(2026-11-17 API 조회 404). 따라서 지금 HAOS에 이 저장소를 추가하면 앱은 보이지만 설치는 이미지를 받지 못해 실패한다.
- `main`의 CI는 같은 Dockerfile을 두 아키텍처에서 빌드·검사한다(`cf94bba`에서 통과). 게시 워크플로 자체는 한 번도 실행된 적이 없으므로 첫 실행이 곧 검증이다.

## 설치에 필요한 게시 절차 (승인 후 실행)

1. `main`이 검증된 커밋인지, CI가 통과했는지 확인한다.
2. `git tag 2.0.0-alpha.4 <main 커밋>` 후 `git push origin 2.0.0-alpha.4`. 이 push가 게시를 시작한다.
3. `HARS prerelease` 워크플로의 세 작업이 모두 통과했는지 확인한다. 한 아키텍처라도 실패하면 manifest와 릴리스는 만들어지지 않는다.
4. **GHCR 패키지 공개 여부를 확인한다.** 개인 계정의 새 컨테이너 패키지는 비공개로 만들어질 수 있다. 비공개면 HAOS가 이미지를 받지 못한다. GitHub의 패키지 설정에서 공개로 바꾸거나 HAOS에 레지스트리 인증을 등록한다. 공개 전환은 웹 UI에서만 할 수 있다.
5. 인증 없이 `docker manifest inspect ghcr.io/qualified4/roadviewer-hars:2.0.0-alpha.4`로 두 아키텍처가 보이는지 확인한다.

되돌리기: 릴리스와 태그는 지울 수 있지만, 이미 누군가 받은 이미지는 회수할 수 없다. 태그를 다시 쓰지 말고 문제가 있으면 다음 버전(`alpha.5`)으로 올린다.

## 설치 후 검증 계획

설치 전: 운영 중인 Python 앱에서 진행 중인 업로드·변환·보관 작업이 없는 시간대를 고른다. HARS 설치 자체는 Python을 재시작시키지 않지만 호스트의 CPU·디스크를 함께 쓴다. 호스트의 남은 디스크 공간을 기록한다.

설치: HAOS 앱 저장소에 `https://github.com/Qualified4/roadviewer-hars`를 추가하고 "Road Viewer HARS"를 설치한다. 옵션은 기본값 그대로 둔다(외부 HTTPS 끔, 포트 null). 수동으로 시작한다.

| 단계 | 확인 | 통과 기준 |
|---|---|---|
| 1. 병행 | 두 앱의 사이드바·Ingress가 각각 열린다. Python 앱의 버전·옵션·포트 매핑·데이터가 설치 전과 같다 | Python 쪽 변화 0 |
| 2. 기동 | HARS 로그에 오류 없이 기동, 빈 목록 표시, 설정 화면의 저장 공간 수치 | 로그에 panic·권한 오류 없음 |
| 3. 업로드 | Ingress 화면에서 실주행 구간 사본을 브라우저로 올린다(qcamera.ts + fcamera.hevc + rlog, 가능하면 ecamera 포함 구간과 1928×1208 장치의 구간). 모바일 HA 앱에서도 1회 | 업로드 완료, 파일 크기 일치 |
| 4. 변환 | 자동 변환이 `ready`로 끝난다. 경고, 썸네일, 변환 시간, 변환 중 메모리(HA의 앱 자원 표시)를 기록한다 | 경고 없음. aarch64 호스트라면 이 수치가 첫 실측이다 |
| 5. 재생 | 전방·광각·qcamera 전환, 시킹, 끝까지 재생, 지도·텔레메트리·오버레이 동기. 데스크톱 브라우저와 모바일 앱 | 재생 오류 0, 로그와 영상 시간 일치 |
| 6. 다운로드 | 재생용 MP4, 원본 형식(`.hevc`, `.ts`), ZIP | 원본 TS는 올린 파일과 SHA-256 일치 |
| 7. 재시작 | (가) 대기 중 앱 재시작, (나) 변환 중 재시작, (다) 업로드 중 재시작, (라) 앱 중지 후 시작. 가능하면 (마) 호스트 재부팅 | 기록·인증 상태 보존, 변환은 다시 돌아 `ready`, 끊긴 업로드는 다시 올리면 완료. 재부팅 뒤 HARS는 꺼져 있고(`boot: manual`) Python은 평소대로 뜬다 |
| 8. 삭제·공간 | 기록 삭제 후 공간 회수, HA 백업 1회 생성 시 영상이 백업에 들어가지 않는지 | 삭제한 만큼 회수, 백업 크기가 영상 크기만큼 늘지 않음 |

이 단계에서 하지 않는 것:

- **장치(openpilot) 업로드.** 장치 업로드는 HARS에 호스트 HTTPS 포트와 인증서를 주고 장치의 업로드 주소를 바꿔야 한다. 주소 변경은 이번 범위 밖이다. 필요하면 사용하지 않는 LAN 포트(예: 18443, 실제로 비어 있는지 HAOS에서 확인)를 HARS에만 배정하고 PC의 테스트 클라이언트(`integration/real_device_media.py`)로 확인한다. 라우터 포워딩과 도메인은 건드리지 않는다.
- **Google Drive·NAS 보관.** Python과 같은 OAuth client를 쓰면 HARS의 연결 해제가 Python 연결까지 끊을 수 있다. 보관 검증은 별도 단계로 두고, 그 전에는 HARS에 보관 설정을 넣지 않는다.

## 문제가 생겼을 때 HARS만 중단·제거

HARS는 기본 설정에서 Python의 데이터·포트·인증·원격 저장소를 건드리지 않으므로, 걷어내는 데 Python 쪽 작업은 필요 없다.

1. **중단.** HAOS에서 HARS 앱을 중지한다. 업로드·변환이 진행 중이면 원본과 이미 게시된 결과는 디스크에 남고, 다시 시작하면 이어서 처리한다. 원인 조사가 필요하면 제거 전에 앱 로그를 저장한다.
2. **Python 확인.** Python 앱의 Ingress, 외부 포트, 최근 업로드가 평소와 같은지 본다. HARS에 테스트용 호스트 포트를 줬다면 그 포트가 해제됐는지 확인한다.
3. **제거.** HARS 앱을 제거한다. 앱 데이터(`/data/roadviewer-hars`)도 함께 지울지는 HAOS가 묻는다. 검증용으로 올린 사본뿐이라면 함께 지운다. 지우기 전에 남길 기록이 있으면 HARS 화면에서 내보낸다.
4. **저장소 제거.** 앱 저장소 목록에서 HARS 저장소를 지운다.
5. **보관을 켰던 경우에만.** Drive의 `Road Viewer HARS` 폴더와 NAS의 HARS 폴더는 앱을 지워도 남는다. 필요하면 직접 지운다. Python의 `Road Viewer` 폴더와는 별개다. 제거 전에 HARS 화면에서 Drive "연결 해제"를 누르지 않는다(토큰 revoke가 Python 연결에 영향을 줄 수 있다).

제거 뒤 남는 것: 게시된 GHCR 이미지와 GitHub 릴리스(HAOS와 무관), HAOS 호스트의 이미지 캐시(Supervisor가 정리).

운영 포트를 HARS로 넘긴 뒤의 롤백은 이 절차가 아니라 [CUTOVER.md](CUTOVER.md)의 "롤백"을 따른다.

## 설치 전에 남은 준비

- 태그 push와 게시(위 절차). 사용자 승인이 필요하다.
- GHCR 패키지 공개 여부 확인.
- 운영 HAOS의 아키텍처와 남은 디스크 공간 확인, Python 앱의 실제 호스트 포트 확인.
- 검증에 쓸 실주행 구간 사본 준비(운영 데이터 원본을 옮기지 않는다).
- 미검증으로 남아 있는 항목은 [STATUS.md](STATUS.md)의 "운영 전환 전 남은 위험"과 [VALIDATION.md](VALIDATION.md)의 alpha.4 "미검증"에 있다.
