# 개발·검증

<!-- toc -->
**목차**

- [진단](#진단)
- [CI/CD](#cicd)
- [웹 화면 검사 (브라우저)](#웹-화면-검사-브라우저)
- [재생 변환](#재생-변환)
- [변환 성능 재현](#변환-성능-재현)
<!-- /toc -->

빌드: Linux 또는 WSL에서 Rust stable, C 컴파일러(번들 zstd를 `cc`로 빌드), Cap'n Proto compiler(`capnp`)만 필요하다. 미디어 라이브러리·pkg-config·libclang은 필요 없다. 실행 파일은 libc·libm·libgcc_s만 링크한다.

시험: 위에 더해 `ffmpeg`/`ffprobe` CLI가 있어야 한다. **개발 전용**이다: 시험이 합성 입력(lavfi + x264/x265)을 만들고, 독립 디코더·판독기(oracle)로 결과를 대조한다. 제품 실행 파일과 배포 이미지에는 없다(CI의 `Rust tests`만 설치한다. 컨테이너 작업은 이미지에 FFmpeg 실행 파일·라이브러리·링크가 있으면 실패한다). 배포 컨테이너에는 Python 런타임도 없다. Python은 CI 도구, 브라우저 어댑터와 openpilot RV1 클라이언트 검사에 사용한다.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets   # CI는 같은 테스트를 cargo nextest run --locked --all-targets 로 병렬 실행한다
python3 scripts/check_coexistence.py
python3 scripts/check_release.py
docker build -t roadviewer-hars:test .
python3 scripts/image_report.py roadviewer-hars:test --require-no-ffmpeg > image-report.json
python3 scripts/container_smoke.py roadviewer-hars:test
```

`cargo test`의 HMAC 비교 케이스는 개발 환경의 Python 표준 라이브러리를 사용한다. 제품 Rust 실행 파일은 Python도, 외부 미디어 도구도 호출하지 않는다. 테스트 영상 인코딩은 합성 입력 생성용이며, 제품 리먹싱은 스트림 복사만 수행한다. `scripts/container_smoke.py`는 저장소의 합성 클립(`scripts/smoke/`, 10프레임 160x96, 만든 명령은 스크립트 머리말)으로 FFmpeg 없는 이미지 안에서 변환·원본 형식 내보내기·썸네일을 확인한다.

배포 이미지의 실행 파일은 `strip`한다(약 27.2 MB → 20.7 MB). 심볼 표가 없어도 panic은 메시지와 소스 위치(`파일:줄:열`)를 그대로 출력한다. `RUST_BACKTRACE=1`의 backtrace에는 함수 이름 대신 주소만 남으므로, 함수 이름이 필요하면 같은 커밋을 strip 없이 빌드해 주소를 대조한다.

개발 서버:

```sh
HARS_DATA=/tmp/hars-dev HARS_INGRESS_ONLY=0 cargo run --locked
```

이때도 UI 변경 API에는 `X-RoadViewer-Request: 1`이 필요하다. 기본 서버는 loopback에만 바인딩한다. HA 실행은 `roadviewer-hars ha-start`이며 `/data/options.json`, `/app/web`, HA Ingress peer 제한을 사용한다.

고정 커밋의 ccnc-hda1 `client.py`를 쓰는 RV1 검사:

```sh
python3 -m venv /tmp/hars-test-env
/tmp/hars-test-env/bin/pip install aiohttp
/tmp/hars-test-env/bin/python integration/ccnc_compat.py \
  --rust "$PWD/target/debug/roadviewer-hars"
```

검사는 임시 데이터와 loopback 포트를 사용한다. 합성 1.x 스냅샷을 HARS로 가져온 뒤 기존 장치 인증과 완료 영수증 복구, 재페어링 없는 업로드를 확인한다. 새 장치의 페어링과 업로드도 확인한다. 장애 주입은 같은 인터프리터로 `integration/uploader_faults.py --rust …`를 실행한다.

## 진단

Rust 구조화 로그는 요청 ID, 경로, 서버 진입, 인증/파일 락 대기, 쓰기 시간, 응답 헤더 준비 시간을 다룬다. Authorization·서명·nonce·본문·장치 비밀값·TLS 키 본문을 출력하지 않는다. Nginx 완료 로그는 요청 전체·upstream connect/header/response 시간과 두 request ID를 연결한다. Rust의 헤더 준비 시간은 클라이언트가 응답을 모두 받았다는 뜻이 아니다. Nginx 수신 전에 발생한 장치 DNS/TLS/연결 오류는 장치 또는 네트워크 측 계측이 필요하다.

## CI/CD

`HARS CI`는 PR과 수동 실행(`workflow_dispatch`)에서 다음 작업을 병렬로 돌린다: `Rust lint`(fmt, clippy), `Rust tests`(nextest), `Compatibility`(`Browser build`의 debug 실행 파일로 `ccnc_compat.py`·`uploader_faults.py`), `Repository checks`(`check_release.py`·`check_coexistence.py`), amd64/aarch64 네이티브 컨테이너 빌드·기동 검사, 브라우저 테스트(`Browser build` → `Browser shard 1~9` → `Browser checks`). `HARS required`는 `always()`와 모든 의존 결과의 `success` 검사를 사용하므로 실패·취소·생략이 성공으로 바뀌지 않는다. 이 작업은 `push`가 아닌 모든 이벤트에서 돈다.

`main` 푸시에서는 검사를 다시 돌리지 않는다(병합되는 트리는 PR에서 `HARS required`를 통과한 트리다). 대신 `Cache refresh (Rust lint)`·`Cache refresh (Rust tests)`·`Cache refresh (browser)`가 PR 작업이 복원하는 캐시를 같은 키·경로로 저장한다. PR은 자기 브랜치와 `main`의 캐시만 읽을 수 있기 때문이다. Rust 캐시는 `hars-rust-lint-v1-*`(clippy 산출물)와 `hars-rust-test-v1-*`(nextest 테스트 프로그램과 `Browser build`의 실행 파일) 두 개이고, 키는 `Cargo.lock`·`Cargo.toml`·`rust-toolchain.toml`의 해시다. 키가 이미 있으면 refresh 작업은 내려받지 않고 확인만 한 뒤 끝난다. 테스트용 패키지(capnproto, ffmpeg CLI)는 `.github/actions/test-packages`가 설치하며, 받은 .deb 파일을 러너 이미지별 캐시(`hars-apt-v1-*`)에 두고 `Cache refresh (packages)`가 main 푸시에서 저장한다. `Rust tests`는 읽기만 한다. 2026-11-18부터 러너의 Ubuntu 미러가 ffmpeg 65 MB를 약 100 kB/s로 내려 이 단계가 10~16분 걸렸기 때문이다(캐시 후에는 패키지 목록만 미러에서 받는다). PR과 수동 실행은 Rust 캐시를 복원만 하고 저장하지 않는다. `Cargo.lock`이나 `Cargo.toml`을 바꾼 PR(버전을 올리는 릴리스 PR 포함)은 같은 키가 없으므로 `restore-keys`로 main의 가장 최근 캐시를 복원해, 받아 둔 crate와 의존 라이브러리 빌드를 재사용하고 바뀐 부분만 다시 빌드한다. 컨테이너 작업은 캐시를 쓰지 않는다. main 보호 규칙은 `HARS required`를 요구한다.

컨테이너 builder는 이미지에 들어 있는 Rust 1.100 stable compiler를 사용한다. root의 개발용 toolchain 설정은 builder에 복사하지 않아 별도 stable/rustfmt/clippy toolchain을 중복 다운로드하지 않는다. Rust CI는 최신 stable을 검증한다.

프리릴리스 workflow는 HARS 저장소의 `2.0.0-alpha.N`, `beta.N`, `rc.N` 태그만 처리한다. Cargo/HA 버전이 태그와 일치해야 하며 HARS 전용 GHCR 이미지를 게시한다. `2.0.0` 정식 태그는 이 workflow로 게시할 수 없다. 현재 작업에서 태그·Release·배포는 수행하지 않는다.

## 웹 화면 검사 (브라우저)

웹 파일은 `web/`에서 관리한다. Python Road Viewer 1.3.18에서 복사한 이후의 변경은 `git diff a3687c9 -- web`로 확인한다. `tests/page_version.rs`는 현재 웹 파일을 HARS로 열어 자산 주소의 버전, 제품명, 제목과 캐시 헤더를 검사한다.

CI는 브라우저 테스트 50개 항목을 9개 묶음으로 실행한다. 로컬 실행, 격리, 게이트와 실패 산출물은 [BROWSER-TESTS.md](BROWSER-TESTS.md)를 참고한다.

어댑터는 Python 서버와 같은 조건을 두 가지 만든다. 둘째(재기동)는 `HARS_SHIM_RESTART=1`일 때만 하고, 실행기는 그것을 `maps[map_parts=many,leak]`에만 준다(다른 항목에서 폴더가 바뀌면 `restart refused`로 기록되고 게이트 실패). 연결 대기열은 werkzeug와 같은 128이다.

- 요청마다 연결을 닫는다(HTTP/1.0). 테스트의 werkzeug 서버가 `Connection: close`로 답하기 때문이다. 목록 카드의 지도 버튼은 `/api/tracks` 응답 전에 먼저 나타나고(maps.js의 의도된 대기 상태), 경로 선은 응답을 받은 뒤 그려진다. 110행은 버튼이 보이자마자 선을 센다. keep-alive 프록시에서는 그 사이에 세는 경우가 30회 페이지 열기 중 2~5회 나왔다. Python 서버를 같은 프록시 뒤에 두어도 3/30이었고, Python 직접 연결과 HTTP/1.0 프록시(Python·HARS 모두)에서는 0/30이었다.
- 데이터 폴더의 기록 폴더 목록이 바뀌면 다음 요청 전에 HARS를 다시 띄운다. HARS는 기록을 시작할 때 한 번 읽고 그 뒤에는 자기 API로만 새 기록을 알게 되는데, 테스트의 `many` 부분은 서버가 도는 중에 기록 100개를 폴더에 직접 써 넣는다(Python은 요청마다 폴더를 읽는다).

실행기는 각 브라우저 테스트를 루프백만 있는 네트워크 이름공간(`unshare -rn`)에서 실행한다.

## 재생 변환

`roadviewer-hars convert-log RLOG NEW_OUTPUT [LABEL]`은 원본을 수정하지 않고 새 출력 폴더에 replay/telemetry/track/summary/thumbnail을 생성한다. 서비스는 자식 프로세스를 낮은 CPU/I/O 우선순위로 실행하고 완료 staging을 저널로 게시한다. 자동 변환은 새 설치에서 기본 켜짐, 기존 저장 설정은 유지한다.

`/api/settings/processing`, `/api/progress`, `/api/logs/<id>/convert`, `/prepared` DELETE, `/cancel` POST가 큐를 제어한다. `/cancel`은 HARS의 추가 API이며 기존 UI의 queued 제거 경로도 유지한다.

## 변환 성능 재현

기존 측정의 조건·결과는 [PERFORMANCE.md](PERFORMANCE.md)를 참고한다. 이식 당시 측정·재생 비교 도구는 [정리 전 Git 트리](https://github.com/Qualified4/roadviewer-hars/tree/284efa5b0932c918f8f52bee616f216650b23c52/integration)에 있다.

`HARS_PROFILE_FILE=/private/stages.jsonl roadviewer-hars convert-log ...`은 진단용 단계 wall/CPU/자식 CPU·호출 수·누적 RSS high water를 숫자로 기록한다. 기본은 꺼져 있다. 출력 실패는 변환을 실패시키지 않는다. 범위는 중첩되므로 합산하지 않으며, 단계 종료의 누적 RSS를 그 단계만의 Peak로 해석하지 않는다. 작은 zstd read마다 계측하므로 진단 실행의 시간은 정상 성능 표본으로 사용하지 않는다. CPU 함수 비용은 Linux `perf record -e cpu-clock -F 199 --call-graph dwarf`, 할당은 Valgrind DHAT로 별도 확인할 수 있다. 원시 perf/DHAT에는 메모리·개인 경로가 포함될 수 있어 저장소 밖에서 보관한다.
