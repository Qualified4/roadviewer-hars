# 개발·검증

<!-- toc -->
**목차**

- [진단](#진단)
- [CI/CD](#cicd)
- [웹 화면 검사 (브라우저, CI 밖)](#웹-화면-검사-브라우저-ci-밖)
- [재생 변환 비교](#재생-변환-비교)
- [변환 성능 재현](#변환-성능-재현)
<!-- /toc -->

빌드: Linux 또는 WSL에서 Rust stable, C 컴파일러(번들 zstd를 `cc`로 빌드), Cap'n Proto compiler(`capnp`)만 필요하다. 미디어 라이브러리·pkg-config·libclang은 필요 없다. 실행 파일은 libc·libm·libgcc_s만 링크한다.

시험: 위에 더해 `ffmpeg`/`ffprobe` CLI가 있어야 한다. **개발 전용**이다: 시험이 합성 입력(lavfi + x264/x265)을 만들고, 독립 디코더·판독기(oracle)로 결과를 대조한다. 제품 실행 파일과 배포 이미지에는 없다(CI의 `Rust checks` 작업만 설치한다. 컨테이너 작업은 이미지에 FFmpeg 실행 파일·라이브러리·링크가 있으면 실패한다). 배포 컨테이너에는 Python 런타임도 없다. 비교용 Python 코드는 개발/테스트에만 사용한다.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python3 scripts/verify_reference.py
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

실제 ccnc-hda1의 고정 커밋 `client.py`를 사용하는 비교 검사:

```sh
python3 -m venv /tmp/hars-test-env
/tmp/hars-test-env/bin/pip install -r reference/python/roadviewer/requirements.txt aiohttp
PATH=/tmp/hars-test-env/bin:$PATH /tmp/hars-test-env/bin/python integration/ccnc_compat.py \
  --rust "$PWD/target/debug/roadviewer-hars"
```

비교는 임시 데이터와 loopback 포트만 사용한다. 원본 Python의 실수신기 판별 규칙 때문에 로컬 8098 포트가 비어 있어야 하며, 사용 중이면 다른 프로세스를 중단하지 않고 검사를 실패시킨다. 기존 장치 인증은 Python 임시 서버에서 만든 뒤, 오프라인 스냅샷을 Rust로 가져와 재페어링 없이 검사한다. 출력 보고서에 비밀값을 넣지 않는다. 합성 업로드 페이로드의 프로토콜 검사이며, 실장치·실주행 로그 동등성 검사가 아니다.

## 진단

Rust 구조화 로그는 요청 ID, 경로, 서버 진입, 인증/파일 락 대기, 쓰기 시간, 응답 헤더 준비 시간을 다룬다. Authorization·서명·nonce·본문·장치 비밀값·TLS 키 본문을 출력하지 않는다. Nginx 완료 로그는 요청 전체·upstream connect/header/response 시간과 두 request ID를 연결한다. Rust의 헤더 준비 시간은 클라이언트가 응답을 모두 받았다는 뜻이 아니다. Nginx 수신 전에 발생한 장치 DNS/TLS/연결 오류는 장치 또는 네트워크 측 계측이 필요하다.

## CI/CD

`HARS CI`는 Rust 검사와 amd64/aarch64 네이티브 컨테이너 빌드·기동 검사를 실행한다. `HARS required`는 `always()`와 모든 의존 결과의 `success` 검사를 사용하므로 실패·취소·생략이 성공으로 바뀌지 않는다. 신규 저장소의 최초 조사에서는 ruleset이 없었고 main도 아직 생성 전이었다. 보호 규칙을 임의로 추가하지 않는다.

컨테이너 builder는 이미지에 들어 있는 Rust 1.100 stable compiler를 사용한다. root의 개발용 toolchain 설정은 builder에 복사하지 않아 별도 stable/rustfmt/clippy toolchain을 중복 다운로드하지 않는다. Rust CI는 최신 stable을 검증한다.

프리릴리스 workflow는 HARS 저장소의 `2.0.0-alpha.N`, `beta.N`, `rc.N` 태그만 처리한다. Cargo/HA 버전이 태그와 일치해야 하며 HARS 전용 GHCR 이미지를 게시한다. `2.0.0` 정식 태그는 이 workflow로 게시할 수 없다. 현재 작업에서 태그·Release·배포는 수행하지 않는다.

한글 경로에서 네이티브 Python 로더가 비정상 종료하는 문제를 피하도록 비교 서버는 영문 임시 경로에서 실행한다. Rust의 Cargo 산출물은 WSL 영문 임시 디렉터리에 두어 Windows 파일 시스템의 빌드 비용을 줄일 수 있다.

## 웹 화면 검사 (브라우저, CI 밖)

웹 파일은 `reference/python/roadviewer/app/web/`의 보존 원본을 그대로 쓴다. 원본을 새 릴리스로 올릴 때는 `scripts/verify_reference.py`의 `SOURCE`와 `scripts/inventory.py`의 기준을 함께 바꾼다. 페이지의 버전 치환은 브라우저 없이 `tests/page_version.rs`가 CI에서 지킨다(원본 릴리스 번호가 페이지에 남거나 치환 대상이 사라지면 실패).

아래 검사는 브라우저가 필요해 CI에 넣지 않았다. 새 임시 프로필의 headless Chromium 계열만 쓰고, 서버는 임시 데이터 폴더와 loopback 포트의 release 실행 파일이다. 대상 레코드는 영상과 GPS가 있고 같은 주행에 구간이 2개 이상 있어야 하므로 실주행 연속 구간 3개를 `/api/uploads`로 올려 변환한 뒤 쓴다(`integration/real_upload_smoke.py`와 같은 업로드 절차).

```sh
HARS_DATA=/tmp/hars-ui HARS_INGRESS_ONLY=0 HARS_BIND=127.0.0.1:18760 HARS_DEVICE_BIND=127.0.0.1:18761 target/release/roadviewer-hars &
# 1.3.17-1.3.18: 차량 아이콘 색상 9종(지도 옵션·표시 설정), 지도 탭과 영상 위 작은 지도에 함께 적용,
# 새로 고침 뒤 유지, 방향 화살표 색, 자동=기존 모습, 불투명 차체·창문, 구간 카드 밀어 올리기(끝에서 1.3배)
python3 integration/ui_sync_check.py --browser ~/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome \
  --profile "$(mktemp -d)" --port 18770 --server http://127.0.0.1:18760 --record ID --output sync.json
# 재생·카메라 전환·시킹·오버레이·지도·그래프
python3 integration/ui_view_check.py --browser EXE --profile NEW_DIR --port 18772 --server http://127.0.0.1:18760 --record ID --output view.json
```

`ui_sync_check.py`의 브라우저는 바깥 호스트 이름을 풀지 못하게 띄운다. 지도 타일을 받지 않으므로 실주행 위치가 밖으로 나가지 않는다. `ui_view_check.py`는 그렇게 하지 않으므로 실주행 데이터에서는 지도 타일 요청이 OpenFreeMap으로 나간다. 구간 카드 제스처는 원본 `tests/segment_peek.cjs`가 CDP로 보내는 터치 대신 같은 포인터·터치 이벤트를 스크립트로 보낸다.

보존 원본 브라우저 테스트(`reference/python/tests/*.cjs`, Node + Playwright)는 원본 파일을 고치지 않고 HARS에 붙일 수 있다. `segment_peek.cjs`는 API를 가짜로 채우고 화면 파일을 작업 폴더에서 읽으므로, `integration/upstream_cjs/mirror_web.py`로 HARS가 내보낸 페이지·자산을 영문 임시 폴더에 받아 그 폴더에서 실행한다. `maps.cjs`는 `RV_PYTHON`이 띄우는 서버를 쓰므로 `integration/upstream_cjs/hars_as_python.py`(HARS 기동 + `/api/hassio_ingress/test/` 접두사 제거 프록시)를 `RV_PYTHON`으로 준다. 이 테스트는 Python 형식 기록 파일을 데이터 폴더에 직접 써 넣는다.

```sh
cd /tmp/peek && NODE_PATH=PLAYWRIGHT_NODE_MODULES node "$REPO/reference/python/tests/segment_peek.cjs"
cd "$REPO/reference/python" && HARS_BIN=... HARS_WEB=... RV_PYTHON="$REPO/integration/upstream_cjs/hars_as_python.py" \
  RV_TEST_VIDEO=SYNTHETIC.mp4 NODE_PATH=PLAYWRIGHT_NODE_MODULES node tests/maps.cjs
```

2026-11-17 1.3.18 동기화(`feat/ui-sync-1.3.18`)에서 `segment_peek.cjs`는 통과했다. `maps.cjs`는 110행(목록 카드 경로 버튼 안의 경로 선 2개)에서 0개로 실패해 그 뒤(1.3.17 색상 단언 포함)는 실행되지 않았다. 같은 기록의 `/api/tracks`는 Python과 점이 같고 `distance_m` 표기(`900` 대 `900.0`)만 달랐다. 원인은 확인하지 않았다.

## 재생 변환 비교

`roadviewer-hars convert-log RLOG NEW_OUTPUT [LABEL]`은 원본을 수정하지 않고 새 출력 폴더에 replay/telemetry/track/summary/thumbnail을 생성한다. 서비스는 자식 프로세스를 낮은 CPU/I/O 우선순위로 실행하고 완료 staging을 저널로 게시한다. 자동 변환은 새 설치에서 기본 켜짐, 기존 저장 설정은 유지한다.

```sh
/tmp/hars-test-env/bin/python integration/pipeline_contract.py "$PWD/target/debug/roadviewer-hars"
/tmp/hars-test-env/bin/python integration/real_conversion_compare.py \
  /path/to/private-corpus "$PWD/target/release/roadviewer-hars" \
  /tmp/hars-oracle /tmp/hars-results --prepare-python --limit 47
```

실제 코퍼스는 `<route>--<segment>--rlog.zst`와카메라MP4 파일명으로 전달한다. 결과폴더는 새 경로를 사용하며 이미 끝난 기준 실행은 재사용한다. 비교 출력/좌표 원문을 Git에 넣지 않는다. `/api/settings/processing`, `/api/progress`, `/api/logs/<id>/convert`, `/prepared` DELETE, `/cancel` POST가 큐를 제어한다. `/cancel`은 HARS의 추가 API이며 기존UI의 queued 제거 경로도 유지한다.

## 변환 성능 재현

기준 커밋과 후보의 release 실행 파일을 각각 영문 로컬 경로에 보존한다. 별도 기준 측정용 PR이나 CI를 만들지 않는다. 같은 입력과 출력 파일 시스템에서 다음 도구가 구현별 워밍업 1회 후 순서를 교차해 측정한다.

```sh
python integration/benchmark_conversion.py /private/corpus/route--6--rlog.zst /private/new-results \
  --binary baseline=/private/baseline --binary final=/private/candidate --runs 5
```

측정 구간 밖에서 각 결과를 원본 Python 전체 출력과 `1e-6` 기준으로 비교한다. Rust 기준/후보의 썸네일 메타·JPEG 바이트, 입력 SHA-256 불변과 산출물 크기도 검사한다. CPU는 회수된 자식/후손을 포함하고 RSS는 20ms 프로세스 트리 표본이다. 공유 페이지 중복, 짧은 자식 누락, 캐시·라이브러리 버전 차이를 함께 기록한다. 원시 replay·GPS·로그·영상은 공개하지 않는다.

`HARS_PROFILE_FILE=/private/stages.jsonl roadviewer-hars convert-log ...`은 진단용 단계 wall/CPU/자식 CPU·호출 수·누적 RSS high water를 숫자로 기록한다. 기본은 꺼져 있다. 출력 실패는 변환을 실패시키지 않는다. 범위는 중첩되므로 합산하지 않으며, 단계 종료의 누적 RSS를 그 단계만의 Peak로 해석하지 않는다. 작은 zstd read마다 계측하므로 진단 실행의 시간은 정상 성능 표본으로 사용하지 않는다. CPU 함수 비용은 Linux `perf record -e cpu-clock -F 199 --call-graph dwarf`, 할당은 Valgrind DHAT로 별도 확인할 수 있다. 원시 perf/DHAT에는 메모리·개인 경로가 포함될 수 있어 저장소 밖에서 보관한다.

`integration/conversion_load_smoke.py`는 준비된 실주행 레코드가 있는 **전용 loopback 테스트 서버**에서만 사용한다. 해당 레코드의 파생 출력 제거·수동 재변환과 16MiB 합성 조각 업로드를 겹쳐 목록 API/조각 응답 표본을 기록한다. 자동 변환의 업로드 대기 정책을 바꾸지 않는다. 운영 서버에 실행하지 않는다.

`integration/benchmark_remux.py JOBS REFERENCE NEW_OUTPUT --binary baseline=/private/baseline --binary final=/private/final --runs 3`는 로그에서 미리 추출한 private camera clocks(`segment`, `kind`, `source`, `indices`, `times`와 별도 times JSON)를 사용해 영상만 측정한다. `--mode core`는 검사 밖 mux 경로용이며, 최종 HEVC처럼 입력 SHA를 mux에 합친 구현은 baseline 무해시 코어와 작업량이 다름을 기록해야 한다. `python_video_job.py`는 원본 HEVC 함수/TS inline adapter이며 제품에서 사용하지 않는다. `nal_inventory.py A B`는 모든 NAL 종류의 순서·횟수를 유지한 정규화 SHA를 내며 VCL+header set 검사와 구분한다.

`integration/real_device_media.py CORPUS BINARY JOBS REFERENCE NEW_OUTPUT --segment 3 [--reverse]`는 새 전용 loopback 서버에 고정 RV1 client로 실제 원본 파일을 보낸다. `--reverse`는 이 테스트 서버에 업로드한 사본만 비보존하도록 설정해 역변환 경로를 강제한다. 원본 폴더를 수정하지 않는다. 인증은 테스트 서버에서 새로 생성하며 결과에 비밀값을 넣지 않는다. 원본61/13 및 새 driver의 출력·사진·clock jobs는 private 경로에 두고, 커밋할 것은 숫자·스키마 비교·SHA 집계뿐이다.
