# 개발·검증

Linux 또는 WSL에서 Rust stable, C/C++ 빌드 도구, Cap'n Proto compiler, FFmpeg/ffprobe, pkg-config, libclang-dev, libavformat-dev, libavcodec-dev, libavutil-dev, libswscale-dev가 필요하다. 배포 컨테이너에는 Python 런타임이 없다. 비교용 Python 코드는 개발/테스트에만 사용한다.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python3 scripts/verify_reference.py
python3 scripts/check_release.py
docker build -t roadviewer-hars:test .
python3 scripts/container_smoke.py roadviewer-hars:test
```

`cargo test`의 HMAC 비교 케이스는 개발 환경의 Python 표준 라이브러리를 사용한다. 제품 Rust 실행 파일은 Python을 호출하지 않는다. 테스트 영상 인코딩은 합성 입력 생성용이며, 제품 리먹싱은 스트림 복사만 수행한다.

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

## 재생 변환 비교

`roadviewer-hars convert-log RLOG NEW_OUTPUT [LABEL]`은 원본을 수정하지 않고 새 출력 폴더에 replay/telemetry/track/summary/thumbnail을 생성한다. 서비스는 자식 프로세스를 낮은 CPU/I/O 우선순위로 실행하고 완료 staging을 저널로 게시한다. 자동 변환은 새 설치에서 기본 켜짐, 기존 저장 설정은 유지한다.

```sh
/tmp/hars-test-env/bin/python integration/pipeline_contract.py "$PWD/target/debug/roadviewer-hars"
/tmp/hars-test-env/bin/python integration/real_conversion_compare.py \
  /path/to/private-corpus "$PWD/target/release/roadviewer-hars" \
  /tmp/hars-oracle /tmp/hars-results --prepare-python --limit 47
```

실제 코퍼스는 `<route>--<segment>--rlog.zst`와카메라MP4 파일명으로 전달한다. 결과폴더는 새 경로를 사용하며 이미 끝난 기준 실행은 재사용한다. 비교 출력/좌표 원문을 Git에 넣지 않는다. `/api/settings/processing`, `/api/progress`, `/api/logs/<id>/convert`, `/prepared` DELETE, `/cancel` POST가 큐를 제어한다. `/cancel`은 HARS의 추가 API이며 기존UI의 queued 제거 경로도 유지한다.
