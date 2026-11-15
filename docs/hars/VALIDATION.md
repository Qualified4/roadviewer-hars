# 검증 증거와 미검증 범위

## 로컬에서 실제 실행한 항목

- Rust stable 1.100.0, `cargo fmt --all --check`, Clippy `--all-targets -- -D warnings`.
- 초기 Rust 전체 테스트 25개 통과. 이후 요청 ID/진행률 호환 테스트 1개, 이식 경로 불변성 테스트 1개를 추가해 해당 파일들을 다시 검증했다. 최신 전체 실행은 GitHub Actions에서 확인한다.
- 인증·입력 검증, nonce 재사용과 재시작, 토큰 갱신/폐기, 조각 재전송·충돌·중단 복구, SHA 오류, 완료 영수증, 디스크 예약, 중복 등록 경쟁, 등록 저널 복구, 북마크 기본 검증.
- 합성 H.264 TS→MP4, HEVC→MP4와 가변 프레임 시각, hvc1/decoder configuration/faststart, 압축 NAL 해시·패킷 수·PTS/DTS 검사, MP4→원본 형식 역변환, 기본 썸네일 생성.
- 합성 Zstandard/Cap'n Proto 메시지와 잘린/잘못된 로그, gzip JSON 왕복, 사용 필드만 읽는 처리.
- 원본 `telemetry.py`의 실제 함수를 호출해 텔레메트리·주행 요약을 합성 입력으로 대조. 필드·배열 구조 일치, 숫자 허용 오차 `1e-6`. invalid sample·활성화 gating·enum·구형 carOutput fallback·CCNC 열·운전자 개입을 포함한다.
- 원본 openpilot `ccnc-hda1`의 고정 커밋 `client.py`로 Python/Rust 서버에 실제 HTTP 요청. 페어링, RV1 서명, streaming SHA, 조각 재전송, 재연결, 토큰 교체, finish 재시도, 배치 완료 영수증 복구 통과.
- Python 서버가 만든 인증 파일·완료 영수증을 오프라인 복사해 Rust로 가져온 뒤, **같은 장치 비밀값으로 재페어링 없이 새 배치 업로드 성공**. 실제 장치 대신 원본 클라이언트 함수와 합성 파일을 사용했다.
- amd64 multi-stage Docker build·실제 컨테이너 기동·신뢰 루트가 지정된 자체 서명 TLS·UI/장치 수신기 격리·Python 실행 파일 부재·컨테이너 FFmpeg의 네이티브 가변 간격 HEVC 처리 통과. 이후 일반 HTTP/경로 변경을 포함한 최종 컨테이너 검증은 CI에서 수행한다.
- ego-browser 실제 UI: 빈 목록, 파일 선택과 업로드, 등록 목록 반영, 검색 결과 없음/검색 해제, 구간 펼치기, 고정과 재시작 후 유지. 최종 UI 표시명과 탭 제목은 HARS이며 버전은 2.0.0-alpha.1로 제공한다.
- 원본 파일 286개의 Git 정규화 내용이 원본 커밋과 동일함을 확인. 원본 앱 config는 HA 재귀 탐색을 피하도록 `.reference` 확장자에 내용 그대로 보존했다.

## 검증 도구

`tests/`, `integration/ccnc_compat.py`, `integration/make_browser_fixture.py`, `scripts/container_smoke.py`, `scripts/verify_reference.py`, `scripts/check_release.py`에 재현 가능한 도구를 둔다. 성능의 별도 조건과 한계는 [PERFORMANCE.md](PERFORMANCE.md)에 기록한다.

GitHub Actions는 최신 커밋의 `HARS CI` 실행을 확인해야 한다. 로컬 결과를 최신 커밋 CI 통과로 대신하지 않는다. amd64/aarch64 두 컨테이너와 Rust 검사 전체가 성공해야 `HARS required`가 성공한다.

첫 실행 `5265050`에서는 Rust 검사 전체 27개와 원본 클라이언트 통합이 통과했지만 두 컨테이너의 TLS 기동 검사가 실패했다. 원인·재현·수정은 [FAILURES.md](FAILURES.md)에 기록한다. 이 실행을 전체 CI 성공이라고 부르지 않는다.

## 남은 검증

전체 API 오류 코드/메시지·응답 형식, 실제 rlog와 모든 분석 결과, 카메라/로그 동기화와 전체 재생 데이터, 실제 브라우저 MP4 재생/시킹, Drive/NAS 장애/복구/삭제 보호, 전체 UI 흐름, 저장 공간 자동 정리, 실제 장치 장시간/동시 업로드, HAOS 병행 설치·시작·재시작·Ingress·외부 도메인/TLS/포트 승계는 미완료다. 위 목록은 alpha.1 당시 상태다. 아래 alpha.2 실주행 코퍼스 검증을 추가했으며, HAOS 설치·실장치 운영 검증은 여전히 미완료다.

정식 Release, GHCR 이미지 게시, HAOS 설치·운영 포트 변경·Python 설정 변경·openpilot 전환을 수행하지 않았다.

## alpha.2 실제 재생 변환 (2026-11-15)

- 사용자 로컬 코퍼스 14구간(전방/qcamera), 추가47구간(qcamera47/전방44/와이드26)을 읽기 전용으로 사용했다. 각각16350/56399 모델 프레임이다.
- `integration/real_conversion_compare.py`가 보존된 Python decoder 전체를 영문 임시 사본에서 실행한다. 비교 대상은 전체 replayV2, 영상 메타·시각, 모델·조향·레이더·CCNC·overlay, telemetry, GPS track, wall clock·주행 summary다. 경로/mtime 기반 cache key만 제외하며, 배열 순서·키의 존재·bool/null·숫자를 재귀 비교한다. 저장 최소 정밀도인 1e-6을 절대오차 한도로 사용하며 큰 값에 상대 허용오차를 부여하지 않았다. 두 코퍼스 모두 차이0이다.
- 대표 실제 구간을 독립 Rust 서버에 브라우저 업로드 API로 전송했다. 3파일/4파일 → 자동 queued/processing/ready, 활성 업로드 중 자동변환 대기, 원본 SHA256 불변을 확인했다. 751/1201 모델 프레임과 2/3카메라가 실제 생성됐다.
- ego Chromium 실제 UI: 전방/qcamera/와이드 전환, 재생·일시정지,15/25/30/45초 탐색, 차량 그래프와 속도·조향·모델/레이더 overlay 표시, 지도와 GPS 경로를 확인했다. 브라우저 전체61구간을 재생한 것은 아니며 대표0번/6번 구간 검사다. 와이드45초 탐색은44.999989초,readyState4,오류없음이었다.
- CI용 `integration/pipeline_contract.py`는 별도 합성120모델/3카메라 TS/HEVC 입력을 만들어 Python/Rust 전체 출력 동등성과 실제 무손실 native remux를 검사한다. 합성을 실데이터로 표현하지 않는다.
- 신규 Rust 검사는 null overlay의 compact 멱등성, 최신 past/leading 시각 경계, GPS 우선 wall clock, 변환 publication 전/후 재시작 및 완료 직전 취소 복구를 포함한다. 최신 로컬 Clippy 통과, 기존30테스트 및 추가 복구2테스트가 통과했다. 최신 커밋의 CI 결과는 별도로 확인한다.

썸네일은 후보/점수/Y레벨·감마/JPEG/메타/실패 표식 정책을 이식했다. 실제14구간에서 score 일부±0.01, 동일FFmpeg로 JPEG를 RGB 디코딩해 비교한 평균 절대 채널차는0.7854~1.0617/255,최대9~14/255였다. Python PyAV wheel과 native FFmpeg 버전이 달라 픽셀·JPEG 바이트 동일성은 주장하지 않는다. 원인과 후보 동일성 검증은 추가로 필요하다.

관측된 수정: 방향지시등 선택필드 누락, PTS 표시문자열6자리 대신 정수PTS/time_base 사용, Python telemetry_origin 반올림 순서와 decimal round를 맞췄다. 원본 Python의 동작은 변경하지 않았다. raw HEVC/TS 경계·업로드 중 전원 장애·디스크 고갈·전체 UI·HAOS·장시간 실장치·다른 차량/지역 코퍼스는 추가 검증 대상이다.

결과: [실제14구간](measurements/replay-real14-2026-11-15.json), [실제47구간](measurements/replay-real47-2026-11-15.json), [썸네일 픽셀](measurements/thumbnail-pixels-2026-11-15.json). 좌표,영상,원본rlog,인증정보는 포함하지 않는다.
