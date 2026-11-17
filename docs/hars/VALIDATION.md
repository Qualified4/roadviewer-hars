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

### 다운로드 연결 보완

최초 alpha.2 후보 `f4b124a`의 CI43708871726은 Rust/amd64/aarch64/HARS required 모두 성공했다. 이후 UI 다운로드 점검에서 `video_downloads` 목록 누락, 준비폴더 MP4를 찾지 못하는 original 경로와 단일영상 alias 누락을 발견해 별도 수정했다. 준비된 기존 레코드도 재변환 없이 시작 시 다운로드 메타를 채운다. 수정 범위의 Range206·Content-Range·파일명과 기존 레코드 복구 테스트를 추가했으며 최신 커밋 CI는 별도로 확인한다.

실제 와이드6번구간의3카메라를 새서버에서 다운로드했다. 각 MP4 전체 SHA256이 입력 MP4와 일치하고206부분응답도 원본바이트와 일치했다. 원본형식TS/HEVC 역변환을 실제 API로 완료했고 서버의 native 압축비트스트림 검사를 통과했다. [실제 다운로드 결과](measurements/download-real-wide-2026-11-15.json). 위치정보는 지도 API에만 제공하고 `private, no-cache`를 적용한다.

## 성능 최적화 회귀 검증 (2026-11-15)

최적화 소스 `3a645e9`에 다음을 수행했다. Python/openpilot 원본·운영 HAOS·업로드 설정은 변경하지 않았다.

- `cargo fmt --all --check`, Clippy `--all-targets -D warnings`, 전체35테스트 통과. 기존34개에 origin의 camera→model→event fallback/0시각/모델 없음 검사를 추가했다. gzip은64KiB 경계를 넘는 UTF-8·escape roundtrip과 mtime0을 검사한다. 기존 보안/손상/단절/복구/미디어 검사를 약화하지 않았다.
- 실제14+47구간/72749모델 프레임 전체 replayV2/telemetry/GPS/summary를 새 Python oracle과 다시 비교했다. cache key만 제외하고 `1e-6` 절대오차·필드 존재·null/bool·순서 기준을 유지해 차이0이다. [14구간](measurements/conversion-optimized-real14-2026-11-15.json), [47구간](measurements/conversion-optimized-real47-2026-11-15.json). 이 전체 실행은 기능 검증용 각1회이며 대표 교차 반복 성능 측정과 구분한다.
- 대표 MP4 2종과 실제 MP4에서 무손실 추출한 TS/HEVC 1종은 구현별 warmup1/교차5회로 매 결과를 비교했다. 모든 카메라가 경고·실패 fallback 없이 생성됐으며 Rust 전후 thumbnail JSON/JPEG 바이트 동일, 입력 SHA 불변이다. 파생 raw는 센서 원본 raw가 아니다. Python/Rust 사이의 기존 thumbnail score±0.01/FFmpeg 픽셀 차이는 그대로 별도 범위로 남겨 두며 출력 허용오차를 넓히지 않았다.
- `integration/pipeline_contract.py`의 합성120모델·H.264 TS/2개 HEVC 전체 oracle 통과. 제품 경로가 frame 수·PTS/DTS·정규화 압축 그림 SHA·decoder header·hvc1·faststart 검사를 유지한다. 합성 시험을 실제 센서 raw 검증으로 표현하지 않는다.
- 원본 ccnc 클라이언트 HTTP lifecycle·재페어링 없는 오프라인 인증/완료 영수증 이전을 최종 release로 재검증했다. 실제 와이드 구간4파일 업로드→활성 업로드 중 자동 변환 대기→queued/processing/ready·1201프레임/3카메라·원본 SHA 불변도 통과했다.
- 전용 서버의 명시적 재변환+16MiB 합성 조각 업로드가 겹친 동안 목록 API165개/조각64개가 실패 없이 완료됐다. 결과와 표본 한계는 [성능 문서](PERFORMANCE.md) 및 [측정 JSON](measurements/conversion-upload-load-2026-11-15.json)에 기록했다.
- ego Chromium 실제 UI에서 전방 재생/일시정지·15초 탐색, 와이드 전환/44.999989초 탐색, qcamera 전환/45초 동기화·readyState4/오류없음, 지도 경로·영상 overlay·실제 차량 그래프를 확인했다. [수치 기록](measurements/conversion-optimized-browser-2026-11-15.json). 대표1구간의 검증으로, 전체61구간 브라우저/HAOS 검증은 아니다. 사진·지도·좌표는 공개하지 않았다. 테스트 서버는 모든 작업 완료를 확인하고 종료했다.
- 원본286파일 정규화 내용과 HARS 버전/slug/포트/정식 릴리스 차단 검사를 통과했다. Rust release 설정과 API·출력 스키마·프론트엔드를 유지했다.

CI는 기존 `Rust checks`, `Container (amd64)`, `Container (aarch64)`, `HARS required` 이름과 모든 의존 결과 success 검사 규칙을 유지한다. 저장소 ruleset은 없고 main 보호 API는 `Branch not protected`였다. 로컬 결과를 CI 성공으로 대신하지 않으며 최종 PR/main 커밋과 연결된 실행 상태를 확인한다. HAOS 실기기·추가 차량·장시간 실장치·순수 단계별 비용의 완전한 분리는 미검증이다.

## 원본 형식 13구간 (2026-11-15)

기준 main `0dd87af`, 최적화 소스 `314ed18`이다. 사용자 코퍼스39파일을 읽기 전용으로 조사하고 모두 SHA-256 불변을 확인했다. 13 rlog, 13 H.264 TS(526×330), 13 HEVC(1344×760)이며 와이드/별도 메타데이터는 없다. 카메라별1200패킷/디코딩1200프레임, 원본26영상 디코딩 오류0·중복 파일 내용 없음이다. 로그의 모델은 총15600프레임이다. 이 파일들은 Road Viewer 원본 형식 내보내기이므로 센서 최초 파일·컨테이너·시각 동일성을 가정하지 않는다. [파일별 크기/코덱/시각/누락 조사](measurements/original13-inventory-2026-11-15.json).

- 최종 검증 포함 영상 전용 측정은26개 영상 각각 Python/기존Rust/최적화Rust warmup1+교차3회다. 모든 산출물에 원본의 압축 그림 SHA·decoder header set·패킷 수·PTS/DTS `1e-6`·키프레임 인덱스·avcC/hvcC/hvc1·faststart 검사를 수행했다. 입력·출력 파일 전체 SHA가 다르다는 이유로 손실로 판정하지 않았다.
- 별도 전체 NAL 종류별 감사와 최종26 MP4 전체 디코딩도 수행했다. 정규화(start code 길이/Annex B trailing zeros 제외) 전체 NAL SHA가 입력과 모두 같고, 디코딩1200프레임/오류0이다. HEVC는2 slices/frame으로 VCL2400개와 frame1200개를 구분한다. `integration/nal_inventory.py`로 사진·payload를 출력하지 않고 재현할 수 있다.
- 기존 HEVC의 in-band VPS/SPS/PPS 중복을 확인해 신규 hvc1 mux에서는 입력 해시 후 parameter set32/33/34만 sample에서 제거한다. hvcC의 동일 decoder config를 보존하며 입력/출력 VCL/헤더를 독립 재검증한다. Native FFmpeg BSF는 RAII로 소유권을 관리하고 worker timeout/출력 상한·형식/프로토콜 whitelist·corrupt packet/단일 video/크기/clock/B-frame/32MiB NAL 한도를 유지한다.
- 로컬 rustfmt/Clippy/전체36테스트 통과. 추가 검사는 native 검사·통합 mux 입력 해시를 FFmpeg CLI 독립 추출과 대조하고, timestamp/stream metadata 일치·hvc1 sample 내 PS 부재·잘못된 codec/playlist 거부를 확인한다. 합성120모델/3cam raw 전체 oracle도 통과했다. 합성을 실제 와이드 원본 검증으로 표현하지 않는다.
- 최종13구간 Rust 전체 출력은 기존Rust와 cache key 제외 `1e-6` 기준13/13 동일하다. **Python과 전체 JSON은11/13 동일**하다. 3/9번의 qcamera 마지막 sample duration 차이(각+0.211/-0.200ms)가 duration 필드에 전달된다. baseline에도 같은 차이가 있었으며 허용오차를 확대하거나 결과를 성공으로 숨기지 않았다. 입력 탐색의 FPS 추정 차이로 인한 미해결 메타데이터 호환 차이이고, VCL·프레임·PTS/DTS 손실은 아니다. 원인과 영향은 PERFORMANCE.md를 따른다.
- 기존14+47구간 **72749모델 프레임**은 캐시된 불변 Python oracle을 재사용해 최종 Rust로 전부 재검증했다. replayV2/telemetry/GPS/summary·모델/레이더/조향/CCNC/overlay·단위/기본값/정렬에서1e-6 초과 차이0이다. [13구간·NAL·기존61 회귀 결과](measurements/original13-validation-2026-11-15.json).
- 보존된 원본 `ccnc-hda1` client 함수로 실제3번구간3파일을 전용 Rust loopback에 전송했다. 새 테스트용 페어링/HMAC, streaming SHA·조각 재전송·재연결·완료 영수증 재사용, 업로드 완료 후 자동 변환·ready·replay/telemetry 생성을 확인했다. 별도 기존 인증/receipt 이전 회귀도 통과했다. 실제 차량 장치나 운영 인증을 사용하지 않았다.
- 원본 보존 모드의 rlog/TS/HEVC 다운로드 파일 SHA는 입력과 같았다. 별도 테스트 서버의 원본 비보존 정책으로 **업로드된 사본만** 제거한 뒤 TS/HEVC 역변환·MP4 다운로드·Range206 바이트 검사를 통과했다. 재생성 파일 SHA는 달랐지만 정규화 전체 NAL과 decoder header는 같고 TS PTS 차이0이다. 사용자 원본 폴더는 변경하지 않았다.
- 업로드/finish와 자동 변환 단계의 API 지연·server process-tree RSS를 독립 관측 thread로 측정했다. 업로더 asyncio loop의 동기 파일 읽기/해시가 관측 타이머를 지연시키지 않게 분리했다. 표본 수·최대값·percentile을 공개하고 운영 TLS/Ingress·장시간 tail latency로 확대하지 않는다.
- ego Chromium 실제 UI에서 원본 HEVC 재생/일시정지·30초 탐색, qcamera 전환/59.9초 말미 탐색(readyState4/오류0), 지도·GPS 경로·실제 차량 그래프·overlay를 확인했다. 기존47코퍼스는 별도3cam 업로드 후 와이드44.999989초 탐색도 통과했다. 새13코퍼스에 실제 ecamera 원본이 있는 것처럼 보고하지 않는다. [업로드·다운로드·관측·브라우저 결과](measurements/original13-upload-browser-2026-11-15.json).

전용 브라우저 Space와 테스트 서버는 모든 레코드 ready를 확인하고 종료했다. 사진/원본 로그·영상/GPS 좌표/인증정보/원시 perf 메모리/strace payload를 Git에 넣지 않았다. Python/openpilot 원본·HAOS·Ingress·운영 포트/도메인/업로드 설정·GHCR 운영 이미지·릴리스는 변경하지 않았다. 실제 raw ecamera, HAOS/실장치/TLS 장시간 운영, ISO 전용 conformance suite, 서로 다른 FFmpeg/PyAV 탐색 설정의 완전한 분리는 미검증이다.

## alpha.3 기능 완성·안정화 (2026-11-16)

기준 `a76c0c5`에서 PR #3~#13을 거쳤다. 각 PR은 `Rust checks`, `Container (amd64)`, `Container (aarch64)`, `HARS required`가 모두 성공한 뒤 병합했다. 아래에서 "모의"는 테스트 프로세스 안의 모의 서버나 임시 폴더를 뜻한다. 실제 Google Drive, 실제 NAS, 실제 HAOS, 실제 차량 장치에는 접속하지 않았다.

### 영상 시간축

- 원본 13구간의 3·9번 qcamera 마지막 sample duration 차이(Python 4481/4518 ticks, HARS 4500)의 원인은 `ffmpeg`/`ffprobe` CLI가 강제하는 `scan_all_pmts=1`이었다. 같은 FFmpeg에서 이 옵션만 바꿔 양쪽 값을 재현했다. 이전 문서의 "FFmpeg 6과 8의 버전 차이"는 틀린 가설이다.
- 배포 이미지(bookworm, FFmpeg 5.1.9)에서 3·9·5번 구간을 직접 변환해 4481/4518/4500을 확인했다(libavformat 기본 옵션 경로, PR #4 시점).
- 이후 마지막 프레임 길이의 기준을 Python 추정에서 rlog의 실제 타이밍으로 바꿨다. 근거와 규칙은 [TERMINAL-DURATION.md](TERMINAL-DURATION.md)에 있다. Python 대비 비교는 허용오차를 넓히지 않고, 입력에서 Python 값과 HARS 값을 각각 계산해 양쪽 출력이 자기 값과 1e-9 안에서 같은지 단언한다. 나머지 필드는 1e-6 비교 그대로다.
- `tests/ts_duration.rs`의 합성 입력 전제 조건은 FFmpeg 5.1의 도구로 만든 스트림에서 성립하지 않는다. CI는 ubuntu-24.04(FFmpeg 6.1)에서 돈다.

### 저장 공간과 삭제

- `tests/storage.rs`: `admit` 계산식 경계, 507 코드 3종, 사전 점검에서 부족하면 무삭제, 전체·부분·미고정 혼합 주행의 고정 보호, 업로드 중 주행과 변환 대기·변환 중 기록의 보호, `pinned_routes` 이전, 삭제 저널의 중단 지점별 복구, 권한 오류 주입, 세션 생성과 정리의 경합, 계획 후 변환 대기로 바뀐 주행의 전체 건너뜀.
- 삭제의 확정 시점은 숨김 rename이다. 재시작 복구는 숨겨진 폴더만 마무리하고, 저널에만 있고 온전한 기록은 지우지 않는다.
- 독립 검토에서 나온 경합 3건은 수정 전 코드에서 실패하는 테스트로 재현한 뒤 고쳤다.

### 업로드 장애

- `tests/upload_faults.rs`, `tests/upload_crash.rs`, `integration/uploader_faults.py`: 청크 전송 중 TCP 종료, 재전송과 offset 충돌, SIGKILL(청크 fsync 전후, finish 5개 지점, 카메라 추가 6개 지점), finish 응답 유실, 중복 배치, 업로드 중 자동 변환 대기, `/dev/full`과 `RLIMIT_FSIZE`의 ENOSPC, 정체된 본문과 슬롯 고갈, 만료 토큰·폐기 장치·nonce 재사용.
- 각 사례 뒤에 세션·예약·임시 파일·저널, 등록 파일 SHA-256, 목록 중복 여부를 재시작 전후로 검사한다.
- 드러난 결함: 실패한 쓰기를 성공으로 응답(fsync 전 flush 누락), 장치 삭제 뒤 재시작 불가, 세션 폴더 삭제 중 크래시 뒤 재시작 불가, 정체된 본문의 슬롯 영구 점유. 모두 수정했다.
- finish 검증 실패 뒤 세션을 유지하면 업로더의 재시도가 유휴 만료까지 같은 422를 받는 것을 3회 재현했고, Python처럼 세션을 해제하도록 바꿨다.
- 실제 tmpfs ENOSPC는 root가 필요해 `integration/upload_enospc.py`로 로컬에서 1회만 실행했다. 전원 차단과 OS 크래시는 검증하지 않았다.

### API 동등성

- `integration/api_contract_compare.py`가 실제 Python 서버와 HARS를 loopback에 나란히 띄워 비교한다. 53개 요청에서 상태 코드 53개, JSON 키 집합 46개가 같다. 차이 7개는 오류 형식(HARS는 `error`에 코드, `message`에 Python 문장)과 `video_downloads` 키 하나다.
- Python이 만든 tar를 HARS가 가져오고 HARS가 만든 tar를 Python이 가져온다. rlog SHA와 태그가 보존된다.
- 네트워크 설정·재시작은 모의 Supervisor로만 확인했다.

### 아카이빙

- `tests/archive_backends.rs`, `tests/archiver*.rs`, `tests/archive_*.rs`, `integration/archive_e2e.py`: OAuth 장치 흐름과 토큰 갱신, resumable 업로드의 끊김 후 재개와 세션 만료 후 재시작, size·md5·sha256 검증, copy·move, 단계별 크래시 뒤 복구, 원격 장애 중 로컬 보존, NAS 미마운트·끊김, 수동 실행과 `archive_running`, 기록 삭제에서 원격 삭제 저널까지, 원격 Range 재생과 다운로드, 아카이브 입력 재변환, 지도 보충.
- Python 구조의 폴더와 파일을 모의 Drive에 두고 아카이브·정리·삭제·연결 해제를 모두 실행한 뒤 바이트와 메타가 그대로임을 단언한다.
- 독립 검토 지적: 언마운트 직후 로컬 디스크에 보관, 포기한 확인 작업의 뒤늦은 삭제, 다른 소유자 폴더에서의 삭제, 끊긴 요청의 임시 사본, 공유 폴더의 심볼릭 링크. 모두 재현 테스트와 함께 수정했다.
- 중단된 NAS 복사가 다음 주기의 임시 파일을 rename하던 경합은 병렬 부하에서 100회 중 8회 실패했고, 대상별 배타 락과 취소 확인을 넣은 뒤 100회 중 0회다.

### Python 원격 데이터 읽기 전용 가져오기

- `tests/legacy_archive.rs`, `integration/legacy_import_e2e.py`: 스캔, 가져오기 성공, 해시 불일치, 중복, 카메라 추가, 용량 부족, 5xx, 다운로드 중간 끊김, 취소, SIGKILL 3개 지점에서 Python 폴더·파일을 대상으로 한 요청이 GET뿐임을 요청 로그로 단언한다.
- NAS 원본은 읽기 전용 권한(0555, 0444)으로 두고 전후 상태가 같음을 단언한다.

### 실제 코퍼스 회귀

`d849d10`(PR #9까지)에서 캐시된 Python 기준 결과와 1e-6 절대오차로 비교했다.

| 코퍼스 | 구간 | 동등 | 모델 프레임 |
|---|---:|---:|---:|
| 원본 형식 | 13 | 13 | 15,600 |
| 기존 14구간 | 14 | 14 | 16,350 |
| 기존 47구간 | 47 | 47 | 56,399 |

최종 커밋의 결과는 이 절 끝의 "최종 커밋 검증"에 적는다.

### HAOS 병행 설치(구성 수준)

- `scripts/check_coexistence.py`가 slug, 이미지, 패널명, 데이터 루트, 미게시 호스트 포트, `boot: manual`, Supervisor API 권한을 Python 앱 구성과 대조한다. CI에서 실행한다.
- 컨테이너 기동 검사는 같은 이미지의 nginx로 Supervisor의 포트 응답을 흉내 낸다. TLS 프록시는 Python처럼 Supervisor가 알려 준 포트가 있을 때만 뜬다.
- 실제 HAOS에서의 설치·시작·종료·업데이트·재시작·Ingress는 검증하지 않았다.

### 순수 Rust 리먹싱 엔진(기본 경로)

- 원본 13구간 26영상: 패킷 수 1,200, PTS/DTS, 키프레임, sample payload SHA, decoder configuration 바이트, 정규화 전체 NAL SHA(원본 대비), 디코딩 프레임 MD5(Python 대비)가 모두 같다. 순수 엔진과 FFmpeg 엔진의 출력 스트림은 26개 모두 같고, 기본 실행에서 대체는 0회였다.
- 실패는 `unsupported`, `invalid`, `internal`, `verification`, `resource`로 구분한다. `resource`(디스크 부족, 쓰기 한도, 180초 초과, 강제 종료, 메모리 상한)는 FFmpeg으로 대체하지 않는다. 나머지는 카메라 파일당 최대 1회 대체하고, 대체 출력도 입력 서명 검증과 독립 판독 검사를 통과해야 게시된다. 분류별 주입 테스트가 로그 이벤트와 최종 디스크 상태를 단언한다(`tests/pure_remux_faults.rs`, `tests/conversion_resource.rs`).
- 게시된 결과에 그 카메라의 영상이 이미 있으면 `resource` 실패는 변환을 실패로 끝내고 이전 결과를 그대로 둔다. 실제 서버 실행 파일로 worker 강제 종료, 쓰기 한도, 시간 초과를 주입해 `prepared` 바이트와 메타가 그대로임을 확인한다(`tests/conversion_keep_previous.rs`).
- MP4와 폴더를 fsync한 뒤 게시하고, 원본 비보존 설정에서는 그 뒤에만 원본을 지운다. MP4를 옮긴 직후 서버를 죽이면 원본이 남아 있음을 테스트한다. 정전 자체는 모의하지 못했다.
- 손상 입력: forbidden bit가 선 slice·SEI는 Python처럼 바이트 그대로 게시한다. 첫 start code 앞의 junk는 버리고 경고 이벤트를 남긴다. TS 마지막 패킷이 영상 payload 안에서 잘린 입력은 거부한다. Python은 그 payload를 버린 채 게시한다. 패킷 경계에서 잘린 입력은 버려지는 바이트가 없어 게시한다.
- 합성 경계 입력: 무작위 변형 320건(패닉 0, 시간 상한 안에 종료), B-frame TS(FFmpeg 대체 후 게시), 파라미터 세트만 100 MB(34.7 MiB에서 거부), 33 MiB NAL, 0프레임, EOS/EOB 경계. 실주행 B-frame·interlaced TS는 코퍼스에 없어 합성 입력으로만 확인했다. 실제 PAFF 파일은 만들지 못해 규칙 단위 테스트만 있다.
- 실제 서버 실행 파일: 브라우저 업로드와 RV1 장치 업로드에서 자동 변환, Range 재생, 원본 보존 모드의 SHA 일치, 비보존 모드의 역변환 NAL·TS PTS 일치, `/api/logs/<id>/cancel` 취소, worker·변환 프로세스·서버 SIGKILL 뒤 재시작, 쓰기 한도와 메모리 상한, 동시 업로드 4개의 변환 대기, B-frame TS 대체 뒤 재생, `HARS_REMUX_ENGINE=ffmpeg` 서버.
- 실제 재생 페이지(`/view/<id>/`)를 격리된 headless Edge로 구동했다. 카메라 전환, 재생·일시정지, 중간·말미 시킹, 오버레이 캔버스, 지도 요소, 차량 그래프의 9개 항목이 3회 모두 통과했다. 화질과 오버레이 정합의 눈 확인, 지도 타일 로드, 키보드·터치는 자동화하지 못했다.
- 사용자 권한 tmpfs에서 실제 ENOSPC를 확인했다(`integration/low_space_tmpfs.sh`). 여유가 입력의 1.5배이면 FFmpeg 엔진이 선택되어 성공하고, 0.5배이면 `resource`로 끝난다.
- 컨테이너 기동 검사(bookworm, FFmpeg 5.1)가 amd64와 aarch64에서 HEVC와 H.264 TS를 순수 엔진으로 대체 없이 변환하는지 확인한다.

### 최종 독립 검토 반영 (PR #15)

병합 전에 독립 검토를 받지 못했던 세 범위(읽기 전용 가져오기, 아카이버 2차 수정, 리먹싱 재검토 수정)를 다시 검토했다. 차단 사유는 없었고 아래를 수정 전 코드에서 실패하는 테스트로 재현한 뒤 고쳤다.

- "이전 결과 유지"가 유지할 결과가 없는데 발동해 기록이 `error`에 고착되던 문제. 판정은 디스크만 본다(게시된 summary에 그 카메라가 있고 MP4가 실제로 존재). 결과가 있으면 그 `ready` 결과로 되돌리고 경고와 오류 수준 이벤트 `conversion_kept_previous`를 남긴다. 없으면 Python처럼 그 카메라 없이 `ready`와 경고다. 같은 기록에 실패를 3회 주입해도 `ready`로 수렴한다.
- 자원 실패의 범위를 ENOSPC, EFBIG, EDQUOT, ENOMEM, 시간 초과, 강제 종료, 메모리 상한으로 좁혔다. 그 밖의 errno는 읽기 단계면 `invalid`, 쓰기 단계면 `internal`이다.
- NAS의 모든 unlink와 rmdir이 writer 가드 아래에서 sentinel의 owner와 storage id를 다시 읽는다. probe 캐시가 connected인 상태에서 sentinel을 Python 소유로 바꾼 뒤 삭제·정리·대기 삭제를 돌려 Python 구조 파일이 그대로임을 단언한다.
- 원본 비보존 설정에서 원본 삭제를 메타 저장 뒤로 옮겼다. 중단 지점 3곳에서 재시작해도 중단 없이 끝난 경우와 같은 끝 상태가 된다.
- 가져오기 다운로드는 목록의 크기를 넘으면 즉시 멈추고, 파일이 20만 개를 넘는 원격 목록은 거부한다.
- 전체 테스트 283개가 3회 연속 통과했다. 실제 실행 파일로 재변환 중 쓰기 한도와 worker 강제 종료를 주입해 `ready`와 경고, 남은 카메라의 Range 206 재생, 원본 보존, 이후 재변환 성공을 확인했다.
- 남은 한계: 서로 다른 변환 프로세스가 같은 여유 공간을 보고 동시에 순수 엔진을 고르는 경쟁, NAS에서 확인과 열기 사이의 심볼릭 링크 바꿔치기, sentinel 최초 생성 시의 밀리초 단위 틈.

### 최종 커밋 검증

PR #15까지 병합한 `main`(`2a8474f`)을 포함한 통합 브랜치의 release 실행 파일로 캐시된 Python 기준 결과와 비교했다. 그 전 단계(`d849d10`, `3e2338d`)에서도 같은 결과였다. 영상 길이 필드는 [TERMINAL-DURATION.md](TERMINAL-DURATION.md)의 방식대로 양쪽 값을 각각 계산해 대조하고, 나머지 필드는 1e-6 절대오차다.

| 코퍼스 | 구간 | 동등 | 모델 프레임 |
|---|---:|---:|---:|
| 원본 형식 | 13 | 13 | 15,600 |
| 기존 14구간 | 14 | 14 | 16,350 |
| 기존 47구간 | 47 | 47 | 56,399 |

기존 61구간·72,749프레임이 유지된다. PR #15 최종 커밋에서 전체 테스트 283개가 3회 연속 통과했고 `integration/pipeline_contract.py`가 통과했다. 컨테이너 검사는 CI의 amd64·aarch64에서 통과했다.

### 미검증 요약

실제 HAOS(설치, Ingress, 재시작, 포트 승계, aarch64 실기기 재생), 실제 Google Drive와 SMB 마운트, 실제 Supervisor, 실제 차량 장치의 장시간·동시 업로드, 전원 차단과 OS 크래시, Safari·Firefox, 실주행 B-frame·interlaced 영상, 4 GiB 초과 ZIP.

## alpha.4 FFmpeg 완전 제거 (2026-11-17)

기준은 `main` `d846fc6`(2.0.0-alpha.3)이다. 설계와 세부 수치는 [FFMPEG-REMOVAL.md](FFMPEG-REMOVAL.md), [THUMBNAIL-DECODER.md](THUMBNAIL-DECODER.md), 측정 원본은 `measurements/pure-reverse-2026-11-17.json`, `thumbnail-decoder-2026-11-17.json`, `upload-check-compare-2026-11-17.json`, `ffmpeg-image-amd64-2026-11-17.json`, `no-ffmpeg-*.json`에 있다. 호스트의 FFmpeg 6.1.1은 대조 도구로만 썼다.

### 잔존 검사

- `Cargo.toml`, `Cargo.lock`, `Dockerfile`에 FFmpeg 참조 0건. `cargo tree`에 `ffmpeg-next`·`ffmpeg-sys-next` 없음. release 실행 파일의 `ldd`에 libav*·libsw* 0줄.
- `src/`에 남은 문자열은 세 종류다: 옛 설정값 `HARS_REMUX_ENGINE=ffmpeg`을 알아보는 코드(경고 후 순수 엔진), 원본 TS에 들어 있어 내보내기가 재현해야 하는 서비스 제공자 이름 `FFmpeg`, 바이트 단위로 맞춘 FFmpeg 규칙을 가리키는 주석.
- `PATH`에 `ffmpeg`·`ffprobe`가 없는 환경에서 서버 기동, 변환, 썸네일, 원본 형식 내보내기, ZIP을 확인했다. CI의 두 컨테이너 작업은 이미지에 FFmpeg 실행 파일·공유 라이브러리·동적 링크가 있으면 실패한다.
- 테스트의 합성 입력 생성과 독립 대조에 쓰는 `ffmpeg` CLI는 개발 전용으로 CI의 `Rust checks`에만 설치한다.

### 원본 형식 내보내기(순수 Rust)

- 계약 확인: 코드·테스트·문서·Python 참조 어디에도 "원본과 바이트 동일" 계약은 없다. alpha.3이 확인하던 것은 정규화 서명 일치와 TS PTS ±2 ms였고, 새 구현은 PES별 PTS/DTS 정확 일치, 키프레임, 프레임 수, 입력 구조를 확인한다.
- 원본 13구간 26영상 왕복(원본 → MP4 → 내보내기): 정규화 NAL SHA-256, 프레임 수, 키프레임 위치, 파라미터 세트, PES별 PTS/DTS, 디코딩 프레임 MD5가 26/26 원본과 같다. qcamera TS 13개는 파일 바이트까지 같다. fcamera HEVC 13개는 PPS 뒤 0 바이트 60개(영상당 +60 B)만 다르고, 이는 MP4에 남지 않는 정보라 alpha.3의 FFmpeg 내보내기 결과와는 바이트가 같다.
- 14·47구간 MP4 표본 12개(qcamera 5, fcamera 5, ecamera 2): alpha.3 FFmpeg 내보내기와 비교 항목·디코딩 MD5 12/12 동일. 바이트는 HEVC 7/7 동일, TS는 1개 동일하고 4개는 PCR 배치만 다르다.
- 손상·무작위 변형 300건(시드 고정, 2 GiB 상한): panic 0, 비정상 종료 0, 실패 뒤 남은 출력 0. 최고 RSS 37.3 MiB.
- 제품 경로: `/original/<kind>` 전체 4/4, Range(앞·중간·끝) 12/12가 206, ZIP 안 원본 형식 파일 4/4가 같은 결과.

### 썸네일(순수 Rust 디코더)

- 디코딩 비트 정확성: 36개 영상(13구간 camera.mp4, 14·47구간 표본의 H.264·HEVC)의 5,040/5,040 프레임이 호스트 FFmpeg 디코딩 MD5와 같다.
- 동등성: 85개 입력(qcamera 74 + HEVC 대체 경로 11)에서 고른 초, 점수, `thumbnail.json`, 해상도, 후보 5장의 축소 결과, 밝기 보정 이미지가 85/85 alpha.3과 같다. JPEG 바이트는 0/85 같다(DCT 구현 차이). alpha.3 JPEG 대비 PSNR 52.1/55.0/58.7 dB(최소/중앙/최대), 원본 대비 충실도는 42.34 dB → 42.40 dB(중앙값).
- 실패 사례 40건(2 GiB 상한): panic 0, 자원 초과 0, 남은 파일 0. alpha.3과 성공/실패가 같은 것은 31/40이고, 갈린 9건은 STATUS의 지원 범위 표에 있는 픽셀 형식 6종과 메타데이터 3종(moov 16 MiB 초과, 부모보다 큰 box, timescale 0)이다.

### 업로드·원본 검사(순수 Rust)

- alpha.3의 `ffprobe` 검사와 138건(실주행 38, 합성·손상 100) 대조: 업로드 검사는 둘 다 수락 78, 둘 다 거부 20, 새 검사만 수락 0, 새 검사만 거부 4. 원본 검사는 85/43/0/10. 새 검사만 거부한 것은 손상 합성 파일과 B-frame MP4다. 실주행 38개는 모두 양쪽 수락.
- MP4의 영상 크기는 `stsd`가 아니라 SPS에서 읽는다(`stsd`만 고친 과대 해상도 H.264·HEVC 거부 시험). 실주행 MP4 40개에서 두 값은 모두 같았다.
- 검사는 `__probe` worker 프로세스(주소 공간 상한, 180초, 부모 종료 시 함께 종료)에서 돈다. worker를 SIGKILL로 죽이면 그 파일만 거부되고 서버는 계속 도는 것을 시험으로 확인한다.

### 실패 처리와 데이터 보호

- 대체 엔진이 없으므로 모든 실패 분류가 재시도 없이 끝난다. 자원 실패는 alpha.3과 같다(게시 결과 유지, 변환 중단). 자원이 아닌 실패는 `prepared/`의 게시 결과가 그 카메라를 재생하면 그 카메라만 이어받고 나머지는 변환·게시한다(원본 보존 켬·끔, Range 206, 2회 수렴 시험). 이어받은 카메라 목록을 읽지 못하면 게시를 중단한다.
- 여유 공간이 입력의 2배 + 100 MiB 미만이면 쓰기 전에 자원 실패로 끝난다. 검사를 우회하면 순수 엔진이 그 공간에서 끝내지 못하는 것을 실측했다.
- 불안정 테스트 2건의 원인: `boundary_inputs_…`는 프로세스 전역 카운터로 판정하던 테스트 결함(카운터 제거, 50/50). `a_resource_failure_keeps_…`는 서버가 `meta.json`을 쓴 뒤 메모리 기록을 갱신하는 사이를 테스트가 파일로 기다리던 경합이며 alpha.3에서도 20회 중 2회 실패했다(서버가 `ready`를 답할 때까지 기다리도록 수정, 조정자 50/50). API로 `ready`를 본 클라이언트는 이 사이에 409를 받지 않는다.

### 독립 검토

2회(전체 변경, 수정분). 차단 사유 없음. 반영한 지적: 자원이 아닌 실패에서 변환 전체를 멈추던 정책, MP4 크기를 `stsd`로 판정하던 검사 약화(재현됨), 업로드 검사의 프로세스 격리, TS 오류 문자열의 빌드 경로, 이어받기 목록 읽기 실패 처리. 반영하지 않고 기록만 한 것: 첫 SPS만 판정(이전 `stsd`도 입력이 정하는 값이었다), `internal` 분류에 일시적 원인이 섞임, TS 원본 검사와 엔진이 같은 demuxer를 씀.

### 최종 커밋 검증

통합 커밋 `a5b1ce0`(버전 2.0.0-alpha.4)에서 조정자가 직접 실행했다. 이후 커밋은 형식 지원 문서, 썸네일 격리 테스트 1개, 경고 문구 1곳이다.

| 항목 | 결과 |
|---|---|
| `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings` | 통과 |
| `cargo test --locked --all-targets` | 302 통과, 0 실패(이후 썸네일 격리 테스트 추가로 303) |
| `scripts/check_release.py`, `scripts/check_coexistence.py` | 통과 |
| release 실행 파일 `ldd`의 libav·libsw | 0줄 |
| 원본 13구간 | 13/13, 15,600프레임 |
| 14구간 | 14/14, 16,350프레임 |
| 47구간 | 47/47, 56,399프레임 |
| `conversion_keep_previous` 50회 반복 | 0 실패(`1318f94`에서도 50/50) |
| CI(PR #16) `Rust checks`, `Container (amd64)`, `Container (aarch64)`, `HARS required` | 통과 |

CI의 두 컨테이너 작업은 FFmpeg 없는 이미지 안에서 기동, TLS, VFR HEVC·H.264 TS 변환, 원본 형식 내보내기, 썸네일을 확인하고 FFmpeg 잔존 검사(`image_report.py --require-no-ffmpeg`, FFmpeg 관련 파일 0바이트)를 통과했다. 이미지 크기는 amd64 120.7 MB(압축 46.1 MB), aarch64 142.1 MB(압축 45.3 MB), 설치 패키지는 둘 다 107개다.

### openpilot 형식 대조와 실패 격리

근거와 표는 [FORMAT-SUPPORT.md](FORMAT-SUPPORT.md), 측정은 `measurements/format-support-2026-11-17.json`이다.

- 기준 소스: 로컬 openpilot fork `5c2a8c7f`(읽기 전용)이고, 이미 fetch된 upstream `070de2aef`와 녹화 파일 이름·인코더 설정·V4L2 제어값이 같음을 대조했다. 장치가 기록하는 형식은 HEVC Main level 5 8비트 4:2:0(B-frame 0, 1928×1208 또는 1344×760)과 H.264 High 526×330(CABAC, slice 1, 오디오 선택)이다.
- 코퍼스 24파일은 모두 1344×760 HEVC와 526×330 H.264였다. 코퍼스가 덮지 않는 형식(1928×1208 fcamera·ecamera, 장치 바이트 그대로의 raw ecamera, AAC가 든 qcamera.ts)은 실제 서버(FFmpeg 없는 PATH)에서 업로드 수락, `ready`, Range 206, 썸네일, 내보내기 NAL 일치를 확인했다. 1928×1208과 오디오 TS는 소프트웨어 인코더로 만든 합성 입력이며 하드웨어 인코더의 tile 구성·rate control·SEI는 재현하지 못했다.
- 미지원·손상 입력 12개 사례: 기록이 `error`가 된 경우 0, 서버 종료 0, 거부된 카메라의 원본 SHA-256 변화 0. 이전 게시 결과가 있던 8건은 MP4가 바이트 그대로 남았다.
- 썸네일 격리: Main10 HEVC만 있는 구간이 `ready`, Range 206, `data`·`telemetry`·`track` 200, 목록·ZIP·내보내기 정상, 썸네일 API만 404. `tests/thumbnail_isolation.rs`가 이 성질을 지킨다(20회 반복 0 실패).
- 남은 표시 문제: 업로드 거부는 `{"error":"invalid_file"}`만 오고 `message`가 없다(alpha.3과 같음). 원본 형식 ZIP은 내보낼 수 없는 영상이 하나 있으면 전체가 실패한다.

### 미검증

aarch64에서의 실제 디코딩·속도·메모리(CI는 컨테이너 기동·변환·크기만 확인), 실제 HAOS, 실주행 B-frame·interlaced 입력, 4 GiB 초과 내보내기, 카메라가 처음 쓴 TS(다른 muxer)와의 바이트 비교, 썸네일 디코더의 퍼징·적합성 시험 재현, Python 시절 `prepared/`의 이어받기.

## 썸네일 최적화 (2026-11-17)

브랜치 `perf/thumbnail`(기준 `b94d8a4`, 버전 2.0.0-alpha.4 그대로). 비교 기준은 `b94d8a4`의 release 실행 파일이다. 세부는 [THUMBNAIL-DECODER.md](THUMBNAIL-DECODER.md#speed-up-2026-11-17), 수치는 `measurements/thumbnail-performance-2026-11-17.json`.

| 검증 | 결과 |
|---|---|
| 썸네일 85개(qcamera 74 + HEVC 대체 경로 11) | 종료 코드·stdout(고른 초, 점수)·`thumbnail.json`·해상도·JPEG 바이트 85/85 동일. 측정 빌드로 본 후보 5장과 밝기 보정 후 픽셀 85/85 동일. 새 worker가 내보낸 그림 2,918장(`8800338`: 4,052장) 모두 같은 시각의 alpha.4 그림과 같다 |
| 디코딩 대조 36개 영상, 140패킷 창 | 내보낸 그림 5,004/5,004가 호스트 FFmpeg 6.1.1 `-threads 1` framemd5와 같다(영상당 139장: 제품 경로는 140번째 샘플의 그림을 원래 내보내지 않는다. 이전 기록의 5,040은 별도 harness로 그것까지 셌다) |
| 줄인 범위에서 실제로 내보낸 그림 | 1,040/1,040 FFmpeg와 같다(`8800338`: 1,265/1,265). 건너뛰거나 flush로 버린 디코더에서 디코딩만 된 그림은 출력되지 않아 대조 대상이 아니다 |
| 손상·잘림·비정상 해상도 40건(`integration/thumbnail_faults.py`, 시드 고정, `prlimit --as=2147483648`) | 성공/실패 40/40, 결과·JPEG까지 40/40 alpha.4와 같다. 만든 것 18/18, panic 0, 시한·메모리 초과 0, 실패 뒤 남은 파일 0 |
| 독립 검토가 찾은 입력 4건(`8800338`에서 결과가 달랐다: 샘플 0의 다른 in-band PPS, IDR 직전 슬라이스 손실, HEVC 해석 불가 NAL 헤더, `ctts`를 숨긴 HEVC B-frame) | 4/4 `b94d8a4`와 고른 초·점수·JSON·JPEG 바이트 동일(검토자의 대조 입력 포함 9/9) |
| 차등 퍼징(`integration/thumbnail_differential.py`, 시드 20261117, `prlimit --as=2147483648`): 합성 2 + 실주행 2개씩에서 만든 손상 입력 H.264 300 + HEVC 300 | 600/600 종료 코드·출력·`thumbnail.json`·JPEG 바이트가 `b94d8a4`와 같다(500건은 둘 다 썸네일 생성, 100건은 둘 다 실패) |
| 두 번째 독립 검토(`d2637b2`): 4096x2160 H.264 `ref=16` 2건에서 확인용 디코더가 메모리를 두 배 가까이 쓰거나(251 → 477 MiB) 메모리 부족으로 abort(`b94d8a4`는 성공) | 수정(`bdd9d7d`) 뒤 검토 입력 5건 + 첫 검토 입력 12건 + 4096x2160 HEVC `ref=4` 1건 18/18 `b94d8a4`와 결과·JSON·JPEG 바이트 동일. Peak RSS 250.5 → 253.3 MiB, 306.3 → 339.6 MiB(1.11배, 차이는 glibc mmap 임계값: 고정하면 299.6 / 301.1 MiB), HEVC 4K 171.6 → 69.2 MiB |
| 차등 퍼징 보강 뒤(`df2ed8e`): 합성 시드 추가(H.264 `ref=4`·`ref=16`·`slices=4`·1920x1080, HEVC `slices=4`)와 금지 비트가 켜진 비슬라이스 NAL 삽입 변형 | 시드 20261117 600/600(486건 둘 다 생성), 시드 20261118 300/300(249건) `b94d8a4`와 같다 |
| 두 번째 검토 뒤 기존 검증 재실행(`df2ed8e`) | 썸네일 85/85(JPEG 바이트 포함), FFmpeg 대조 5,004/5,004·1,040/1,040, 손상 입력 40/40(panic 0), 원본 13구간 13/13 차이 없음 |
| 단위 테스트(추가) | 건너뛰는 경로와 전부 디코딩하는 경로(`video::decode(.., skip: false, ..)`)의 썸네일과 필요한 그림이 같은지: 카메라형 H.264 GOP 15, HEVC GOP 20·21·30(flush 뒤 다시 디코딩), 위 4건, IDR 슬라이스의 비참조·SP 변형, CRA. 재시작이 거부되어야 하는 곳에서 재시작하지 않았는지도 확인. 변이 3건(IDR 판정 완화, `shown` 필터 제거, 파라미터 세트 검사 제거) 각각 시험이 실패로 잡았다(변이는 커밋하지 않음). 두 번째 검토 뒤 추가: 샘플 0 앞의 금지 비트 SEI(`ce 01 05 05 80`), 확인용 SPS의 참조 장수 상한(4096x2160 1, 1920x1080 5, 526x330 원래 값). 변이 2건(HEVC 금지 비트 검사 제거, 참조 장수 상한 제거)도 각각 잡혔다 |
| worker 격리 | `main.rs`·`media.rs`·`pure_remux.rs` 변경 없음(메모리 상한 자기 크기 + 512 MiB, 180초, `PR_SET_PDEATHSIG`). 기존 worker 강제 종료 시험들 통과 |
| aarch64 | 썸네일 모듈 3개 + 두 디코더의 `cargo check --target aarch64-unknown-linux-gnu` 통과(기본·SIMD 빌드 둘 다). 전체 크레이트는 aarch64 C 컴파일러가 없어 check 불가(`ring`, `zstd-sys`). qemu-aarch64가 없어 실행은 미검증 |
| 원본 13구간 `real_conversion_compare.py --limit 13` | 13/13 차이 없음(검토 수정 뒤 다시 실행), 15,600프레임. 썸네일 메타데이터는 13/13 alpha.4와 같고, Python과는 10/13(5·6·7구간 점수 0.01 차이, alpha.4도 같음) |
| 전체 변환 벤치마크 3구간 × 6회 | 매 회 Python과 동등, `thumbnail.json`·`thumbnail.jpg`가 alpha.4와 바이트 동일(스크립트 assert) |
| 제품 경로(`8800338`에서 확인, 검토 수정 뒤 다시 하지 않음) | release 실행 파일, 임시 `HARS_DATA`, 포트 18724~18727, PATH에 ffmpeg·ffprobe 없음. 실제 2구간(3, 7) 업로드·자동 변환: 썸네일 API JPEG SHA-256이 alpha.4와 2/2 같고, 두 카메라 Range 206, `/original/<kind>`가 업로드와 같다 |
| `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked --all-targets` | 통과. 전체 테스트 2회 연속 308 통과, 0 실패(검토 수정 뒤, `8800338`에서는 304) |
| amd64 이미지(`hars-tp-thumb:new`, push 없음) | `image_report.py --require-no-ffmpeg` 통과, 120.74 MB / gzip 46.11 MB, 패키지 107 |

스레드를 도입하지 않아 썸네일 테스트 50회 반복은 하지 않았다. `integration/no_ffmpeg_product.py`는 alpha.4와 새 실행 파일 모두에서 `/api/logs/{id}/download?source=...&format=mp4`가 404라 첫 구간에서 멈춘다(이 변경과 무관, 고치지 않음). 제품 경로 확인은 같은 서버 도우미(`pure_default_product.py`)로 따로 했다.

미검증: aarch64 실행, 썸네일 worker 자체를 SIGKILL하는 시험, IDR 간격이 1초보다 길거나 CRA만 있는 HEVC 실주행 입력, 14·47구간 전체 회귀(조정자). 건너뛴 샘플에서 디코더가 panic하는 입력(전부 디코딩하면 실패할 썸네일이 건너뛰면 만들어진다)은 구조 검사로 막지 못하며 차등 퍼징 1,500건(600 + 보강 뒤 600 + 300)에서는 나오지 않았다. 따라서 계약은 "디코더가 panic하지 않는 입력에서 `b94d8a4`와 같다"이다(THUMBNAIL-DECODER.md "What the equality covers"). 4096x2160 H.264의 메모리는 `cargo test`로 직접 시험하지 않고 고정 입력으로만 확인했다.

## 브라우저 테스트 CI와 2.0.0-alpha.5 (2026-11-18)

브랜치 `ci/browser-checks`(PR #22, `main` `46b3c1a`). 세부는 [BROWSER-TESTS.md](BROWSER-TESTS.md).

| 검사 | 결과 |
|---|---|
| GitHub 러너, 최종 커밋 `e238440` | 전 작업 통과. `Browser build` 1분 14초, 묶음 9개 1분 28초~1분 58초, `Rust checks` 3분 47초, 컨테이너 2분 54초·2분 57초 |
| 상태 대기로 고친 4곳(`video_overlay` 77·529행, `conversion_controls` 99행, `replay_gzip` 57행) | CPU 경쟁 부하에서 각 8/8. 수정 전 `conversion_controls` 4/8, `replay_gzip` 2/8~4/8 통과 |
| 고친 단언의 검출력 | 지키는 동작을 화면 사본에서 깨뜨리면 실패(`video_overlay` 두 곳, `replay_gzip`, `replay_layout`). `conversion_controls`는 확인하지 못함 |
| 게이트 무결성(각 1회, 모두 게이트 실패로 판정) | 단언 하나가 실패하는 화면 사본, 묶음 누락, 실행 파일 없음, 네트워크 차단 실패, 테스트 0개, 남의 listener, 허용되지 않은 재기동, 제외 목록 추가 |
| 워드마크 수정(`src/http.rs`) | `tests/page_version.rs`가 수정 전 실패·수정 후 통과, `library_tabs` 183행 통과 |
| `cargo nextest run --locked --all-targets` | 312/312 |
| 실주행 회귀(`f9ad0cb`, 이후 `src/` 변경 없음) | 원본 13구간 13/13, 14구간 14/14, 47구간 47/47 차이 없음(15,600·16,350·56,399프레임) |
| 독립 검토 2회 | 차단 사유 없음. 지적(미러 포트, 제외 집합 고정, 어댑터 대기열, 재기동 범위)은 같은 PR에서 반영 |

미검증: `maps.cjs` 113행(새 clone 실행에서 1회 실패)과 686행(과부하에서 실패)의 시간 의존은 이 시점에 고치지 않았다. 모킹형 39개 항목은 HARS API를 부르지 않는다. alpha.5 이미지의 실제 HAOS 업데이트는 수행하지 않았다.
