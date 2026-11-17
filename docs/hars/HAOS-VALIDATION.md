# 실제 HAOS 병행 설치 검증 준비 (2.0.0-alpha.4)

**준비 점검과 계획에 이어, 2026-11-17에 게시와 실제 HAOS 병행 설치 검증을 실행했다. 결과는 이 문서 끝의 "실제 HAOS 검증 결과"에 있다.** 운영 포트 승계와 Python으로의 롤백은 [CUTOVER.md](CUTOVER.md)가 다루고, 이 문서는 그 앞 단계인 "Python 옆에 HARS를 설치해 격리 상태로 확인하고, 문제가 있으면 HARS만 걷어내기"를 다룬다.

검증 중에도 기존 Python Road Viewer, openpilot의 업로드 주소, 운영 HAOS의 외부 포트·도메인·포워딩은 바꾸지 않는다.

## 충돌 점검 (저장소 기준, 2026-11-17)

`scripts/check_coexistence.py`가 CI에서 매 커밋 확인한다. Python 쪽 값은 보존된 `reference/python/roadviewer/config.yaml.reference`(1.3.18, `d3de134`) 기준이며, 운영 중인 Python 앱의 실제 설정과 같은지는 HAOS에서 확인해야 한다.

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
- 게시 상태(2026-11-17): 태그 `2.0.0-alpha.4`를 push해 이 워크플로가 처음 실행됐고 네 작업이 모두 통과했다. `ghcr.io/qualified4/roadviewer-hars:2.0.0-alpha.4`(amd64·arm64 manifest)가 인증 없이 조회되며 GitHub prerelease가 만들어졌다. 그 전에는 태그·릴리스·패키지가 없어 저장소를 추가해도 설치가 실패하는 상태였다.
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

설치: HAOS 앱 저장소에 `https://github.com/Qualified4/roadviewer-hars`를 추가하고 "Road Viewer"(2.0.0-alpha.5까지는 "Road Viewer HARS")를 설치한다. 옵션은 기본값 그대로 둔다(외부 HTTPS 끔, 포트 null). 수동으로 시작한다.

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

## 설치 전에 남은 준비 (2026-11-17에 모두 수행)

- 태그 push와 게시(위 절차). 사용자 승인이 필요하다.
- GHCR 패키지 공개 여부 확인.
- 운영 HAOS의 아키텍처와 남은 디스크 공간 확인, Python 앱의 실제 호스트 포트 확인.
- 검증에 쓸 실주행 구간 사본 준비(운영 데이터 원본을 옮기지 않는다).
- 미검증으로 남아 있는 항목은 [STATUS.md](STATUS.md)의 "운영 전환 전 남은 위험"과 [VALIDATION.md](VALIDATION.md)의 alpha.4 "미검증"에 있다.

## 실제 HAOS 검증 결과 (2026-11-17)

게시된 `ghcr.io/qualified4/roadviewer-hars:2.0.0-alpha.4`를 운영 HAOS에 Python Road Viewer와 나란히 설치해 확인했다. 조작은 Home Assistant의 Supervisor API와 앱 Ingress로만 했다. Python 앱, openpilot 업로드 주소, 외부 포트·도메인, Google Drive·NAS는 건드리지 않았고 호스트 재부팅도 하지 않았다. 아래 수치는 이 HAOS 한 대에서 한 번씩 잰 값이며 개발 PC 측정([PERFORMANCE.md](PERFORMANCE.md))과 섞어 읽지 않는다.

### 환경

| 항목 | 값 |
|---|---|
| HAOS | amd64 가상 머신(qemux86-64), OS 18.4, Supervisor 2026.11.0, Core 2026.11.1 |
| 자원 | 메모리 약 4 GB, 디스크 196.2 GB 중 여유 175.0 GB |
| Python Road Viewer | 1.3.18, 실행 중, 자동 시작, 컨테이너 8443 → 호스트 포트 사용 중, 기록 795개 모두 `ready`, 진행 중인 업로드·변환·보관 없음 |
| HARS | 2.0.0-alpha.4, 기본 옵션, 호스트 포트 없음(`8443/tcp: null`), `boot: manual`, 보관 미설정 |

이 검증 당시 저장소에 보존된 Python 설정은 1.3.16이고 운영 중인 것은 1.3.18이었다. slug, 이미지, 데이터 경로가 달라 병행 설치에는 영향이 없었다. 이후 보존 원본을 1.3.18(`d3de134`)로 올려 지금은 둘이 같다(설정 차이는 `version` 한 줄이었다).

### 기능별 결과

검증 자료는 따로 보관하던 실주행 구간 사본이다: 원본 형식 구간(qcamera.ts + fcamera.hevc + rlog) 5개와 MP4 3종(전방·광각·qcamera) + rlog 구간 1개. 같은 구간이 Python 앱에도 있어 목록과 썸네일을 읽기 전용으로 조회해 비교했다.

| 항목 | 결과 |
|---|---|
| 병행 설치 | 통과. 두 앱의 Ingress가 각각 열림. Python 앱의 버전·옵션·포트·자동 시작 설정 지문이 설치 전, 각 단계 뒤, 종료 시점에 모두 같음 |
| 업로드(브라우저 API, Ingress 경유) | 통과. qcamera.ts, fcamera.hevc, ecamera.mp4 포함 6개 구간 |
| rlog 해석과 자동 변환 | 통과. 6개 구간 모두 `ready`, 경고·오류 0, HARS 로그의 WARN·ERROR 0 |
| 썸네일 | 통과. 320×200 JPEG. 점수가 Python과 같음(62.08, 10.8). 디코딩한 픽셀의 Python 대비 PSNR 50.0 dB, 48.6 dB |
| 재생 화면(격리된 headless Edge) | 통과. 영상 로드, 카메라 전환(전방·qcamera·광각), 재생·일시정지, 중간·말미 시킹, 오버레이, 지도, 차량 그래프. 2개 구간 |
| Range 요청 | 통과. 카메라별 앞·중간·끝 범위가 206이고 전체 파일의 같은 구간과 바이트 일치 |
| Python과의 비교 | 모델 프레임 수, 주행 요약, 코덱, 경고 없음이 같음. 영상 길이는 원본 형식 구간이 +0.161 ms(rlog 기반 마지막 프레임 규칙의 범위 안), MP4 구간은 동일 |
| MP4 다운로드, ZIP(mp4·original) | 통과. ZIP의 CRC 정상, rlog는 올린 파일과 SHA-256 일치, MP4 항목은 재생용 파일과 일치 |
| 원본 형식 내보내기 | 통과. 원본을 올린 구간은 올린 파일과 SHA-256 일치. MP4를 올린 구간은 순수 Rust 역변환 결과의 디코딩 프레임 MD5가 MP4와 같음(HEVC 2개, H.264 1개, 각 1,200프레임) |
| 디코딩 대조 | 원본, HAOS가 만든 MP4, 내보내기 파일의 프레임 MD5가 모두 같음(전방·qcamera 각 1,200프레임) |
| 유휴 재시작 | 통과. 0.5초 뒤 응답, 기록·변환 결과·설정 동일 |
| 변환 중 재시작 | 통과. `processing`에서 재시작, 2.8초 뒤 `ready`, 원본 동일 |
| 업로드 중 재시작 | 통과. 절반을 보낸 뒤 재시작, 같은 업로드 세션으로 이어 보내 완료, 기록 1개만 생성 |
| 삭제와 공간 회수 | 통과. 테스트 기록 1개 삭제 시 원본 53.6 MB + 변환 결과 42.2 MB에 해당하는 95.8 MB 회수 |

### 실측 (이 HAOS, 각 1회)

| 항목 | 값 | 비고 |
|---|---|---|
| 업로드 53.5 MB(3파일) | 0.97 s, 약 55 MB/s | LAN, Ingress 경유, 2 MiB 조각 |
| 변환(원본 형식 1구간) | 3.58 s | finish 뒤 `ready`까지. 0.5초 간격 조회라 그만큼의 오차 포함 |
| 변환(MP4 3종 1구간) | 3.07 s | |
| 동시 업로드 2구간 | 업로드 2.42 s, 둘 다 `ready`까지 8.34 s | |
| 목록 API(유휴) | 중앙값 4.0 ms | Ingress 왕복 포함 |
| 목록 API(동시 업로드·변환 중) | 중앙값 8.6 ms, p95 26.4 ms, 최대 785 ms(68표본 중 1회) | 서버 로그에는 50 ms를 넘는 목록 요청이 없어, 이 한 번은 측정 쪽 연결이나 Ingress 구간의 지연으로 본다 |
| 원본 형식 내보내기(역변환) | HEVC 37 MB 0.46 s, TS 0.06 s | 전송 포함 |
| 컨테이너 메모리 | 기동 직후 3 MB, 유휴 23 MB, 변환 중 표본 최대 303 MB, 동시 2구간 중 324 MB | Supervisor 통계를 1초 간격으로 읽은 값이라 Peak RSS가 아니고 page cache가 일부 포함됨 |
| 컨테이너 CPU | 변환 중 표본 최대 약 50 % | 같은 통계 |
| 저장 공간 | 원본 형식 구간당 원본 53.5 MB + 변환 결과 42 MB | |

재지 못한 것: 썸네일 단독 생성 시간(운영 이미지에는 단계별 계측을 켜는 수단이 없다), 프로세스 단위 Peak RSS.

### 확인하지 못한 것

- Home Assistant 프런트엔드 안의 패널과 모바일 앱에서의 조작. 브라우저 검증은 Ingress 응답을 로컬 중계로 받아 격리 브라우저에서 했다.
- 장치(openpilot) 업로드와 장치 인증의 재시작 유지. 외부 HTTPS 포트를 열지 않아 페어링된 장치가 없다. 확인한 것은 빈 장치 목록과 처리 설정이 재시작 뒤에도 같다는 것뿐이다.
- 실제 백업 생성. 분석만 했다: `backup_exclude`가 기록 폴더, 업로드·내보내기 임시 폴더를 제외하므로 다음 정기 백업에는 HARS의 설정과 상태 파일만 들어간다. 실제 백업 크기로는 확인하지 않았다.
- Google Drive·NAS 보관, 호스트 재부팅, aarch64 기기, 장시간 운영.

### 발견한 문제

HARS의 기능 결함은 나오지 않았다. 운영에 참고할 점:

- 사이드바 항목은 기본으로 꺼져 있다. 앱 페이지의 "웹 UI 열기"로 들어가거나 "사이드바에 표시"를 켠다.
- 썸네일 단독 시간과 Peak RSS를 실기기에서 재려면 변환 완료 로그에 단계별 시간과 자원 사용을 남기는 기능이 필요하다.
