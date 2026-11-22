# HARS 브라우저 테스트

<!-- toc -->
**목차**

- [요약](#요약)
- [출처와 원본 대비 차이](#출처와-원본-대비-차이)
- [HARS가 고친 지점](#hars가-고친-지점)
- [가져오지 않은 항목](#가져오지-않은-항목)
- [실행](#실행)
- [게이트](#게이트)
- [실패 산출물](#실패-산출물)
- [실행기 구조](#실행기-구조)
- [범위와 한계](#범위와-한계)
- [CI 구성](#ci-구성)
- [시간](#시간)
- [검증(2026-11-17)](#검증2026-11-17)
- [분류표(1단계 측정)](#분류표1단계-측정)
- [미검증과 남은 위험](#미검증과-남은-위험)
<!-- /toc -->

## 요약

HARS는 브라우저 테스트를 직접 소유한다. `tests/browser/`의 테스트(Node + Playwright)는 Python Road Viewer 1.3.18(`d3de13472ea242dab9866d485b5777ad67d5b1dd`)의 `tests/`에서 복사해 왔다. 이후에는 원본 Python 저장소와 따로 관리한다. 실행기 `tests/browser/run_browser_tests.py`가 이 테스트를 HARS 실행 파일과 HARS가 내보내는 웹 파일에 붙여 돌린다. GitHub Actions의 `Browser checks`(`HARS required`가 결과 확인)도 같은 실행기를 쓴다.

`browser-groups.json`의 50개 항목은 모두 통과해야 한다. 테스트 파일은 48개이며 `maps.cjs`는 환경변수만 다른 세 항목으로 실행한다. `browser-helpers.cjs`는 테스트 보조 모듈이다.

**관리 원칙.** 원본 Python 저장소와 독립적으로 발전시킨다. 원본이 바뀌어도 자동으로 따라가지 않는다. 가져올 변경은 HARS 커밋으로 하나씩 들인다. 새 테스트와 수정은 시간 대기(`waitForTimeout`, 고정 지연)가 아니라 확인하려는 상태 변화(이벤트, DOM 상태, 그리기 기록)를 기다린다. 기존 단언은 삭제하거나 완화하지 않고, 같은 동작을 같은 수준이나 더 엄격하게 확인한다. 게이트에는 재시도와 예상 실패가 없다. 고칠 수 없는 실패는 고치거나 보고할 대상이다.

## 출처와 원본 대비 차이

- 복사 커밋 `c977863`(`test: copy the browser tests into tests/browser, unchanged, ...`)은 내용을 바꾸지 않았다. 복사한 55개 파일은 원본의 대응 파일과 바이트 단위로 같았다(`cmp`, `sha256sum -c`). 다만 `.gitignore`의 `*.mp4` 때문에 영상 2개가 커밋에서 빠졌다(53개만 추적). `2db16d3`이 같은 바이트로 추가하고 `.gitignore` 예외를 넣었다.
- 현재 테스트는 `.cjs` 48개, 브라우저 보조 모듈, `fake_drive.py`, 합성 오버레이 JSON과 합성 영상 두 개를 사용한다.
- 가져오지 않은 것: `replay_format2.cjs`([아래](#가져오지-않은-항목)), `run_group.cjs`(원본 CI의 묶음 실행기. HARS 실행기가 대신한다), `tests/*.py` 단위 테스트, AppArmor fixture.
- 원본 대비 HARS 수정만 보기:

```sh
git log --oneline c977863..HEAD -- tests/browser
git diff c977863 -- tests/browser
```

`replay_gzip.cjs`와 `lane_marking.cjs`는 [합성 오버레이 데이터](../../tests/browser/fixtures/overlay-geometry.json)를 읽는다. 기존 9개 시나리오의 프레임과 예상 투영 좌표를 고정해 런타임 투영, 캐시와 표시를 확인한다.

## HARS가 고친 지점

부하는 `taskset -c 0-1`에 같은 코어의 바쁜 루프 3개다. "깨뜨림"은 단언이 지키는 동작을 화면 사본(`--web`)에서 깨뜨렸을 때의 결과다.

| 파일·줄(복사본 기준) | 경쟁의 원인 | 이전 대기 → 새 대기 | 단언 변화 | 부하 8회 | 깨뜨림 | 커밋 |
|---|---|---|---|---|---|---|
| `video_overlay.cjs` 77행 `closing hands focus to nothing` | `close()`가 대화상자를 바로 숨기고, 앱은 몇 ms 뒤의 `close` 이벤트에서 포커스를 넘긴다. 그 사이에 읽으면 숨은 `초기화` 버튼이 아직 포커스를 가진다 | `hidden`만 대기 → `hidden` 뒤, 앱 다음에 등록한 `close` 리스너가 불릴 때까지 대기, 그 뒤 한 번 읽음 | 같음 + Escape 전 포커스가 `#overlayHeightReset`인지 단언(더 엄격) | 8/8(작업자 VO 확인) | `ripple.js` focusin 해제 한 줄 제거 → 83행 그 메시지로 실패 | `4fa6c34` |
| `video_overlay.cjs` 529행 `height reversal starts from the current position` | 이동 시작 90 ms 뒤 마지막 rAF 그리기를 현재 위치로 썼다. 부하에서는 그 그리기가 오래됐거나 이동이 끝나 있었다 | `waitForTimeout(90)` → 같은 작업 안에서 `render()`로 현재 위치를 그려 보고, 바닥까지 1/4~3/4 지점에서 바로 되돌림 | 같음 + 이동 도중을 관측하지 못하면 실패, 이동 폭의 1/4 ≥ 10 px(점프가 보일 크기) 단언(더 엄격) | 8/8(작업자 VO 확인) | 되돌리기가 처음 위치에서 시작하는 `video-overlay.js` 사본 → 547행 그 메시지로 실패 | `dd76cd0` |
| `conversion_controls.cjs` 99행 | `locator.check()`는 클릭 뒤 상태를 다시 읽는다. 저장 실패(500)로 앱이 먼저 되돌리면 `Clicking the checkbox did not change its state`(수정 전 부하 4/8 재현) | `check()` → `click()` + `change` 이벤트의 `checked` 기록, 오류 문구 대기(기존) 뒤 한 번 읽음 | 같음(되돌림 뒤 꺼짐) + 클릭이 정확히 한 번 켰는지 단언(더 엄격) | 8/8(조정자 확인) | 미확인(시도한 사본은 97행에서 먼저 실패해 99행을 검사하지 못함) | `f90dee6` |
| `replay_gzip.cjs` 57행 `N redraws for M frames` | 고정 500 ms 재생 뒤 `idx`(도달한 프레임 번호)를 프레임 변화 횟수로 썼다. 틱이 20 Hz보다 느리면 앱은 프레임을 건너뛰며 한 번만 다시 그리므로 다시 그린 수가 `idx`보다 작다(수정 전 부하 4/8 재현) | `waitForTimeout(500)` → `waitForFunction(()=>idx>=10)` 뒤 멈추고 한 번 읽음. 기준값은 rAF마다 센 실제 프레임 변화 횟수 | 범위 그대로(변화 수 ≤ 다시 그린 수 ≤ 변화 수 + 4) | 8/8(작업자 OVL, 수정 전 2/8) | 앱이 그리기를 빠뜨리게 한 사본 → 62행 실패(작업자 OVL) | `e6331a8`(OVL `d1b9bfb` cherry-pick) |
| `replay_layout.cjs` 214행 `version is an info line in ⋯` | 경쟁 아님. 정규식 `^v\d+\.\d+\.\d+$`가 HARS 프리릴리스 `v2.0.0-alpha.4`를 받지 않았다 | 엄격한 예상 실패 + 버전 줄을 바꾼 화면 사본 짝 실행 → 없음 | 정규식 → `'v' + HARS_EXPECTED_VERSION`과 정확히 같음. 실행기가 `Cargo.toml`에서 읽는다(화면 값이 아니므로 순환이 아님). 변수가 없으면 실패(더 엄격) | 8/8 | 웹 사본의 버전 줄 `v1.3.18` → `v9.9.9` → 216행 그 메시지로 실패, 수집용 재실행이 trace 2개·스크린샷을 남김 | `1bbdadd` |
| `navigation.cjs` 4행(fixture) | 경쟁 아님. fixture의 `meta.json`에 `id`가 없어 HARS가 시작을 거부했다. 어댑터가 채우던 값을 fixture에 썼다 | - | 없음 | 8/8 | - | `066284d` |
| `maps.cjs` 110·113행 지도 버튼이 재생 옆 한 줄에 있는지(+116·117행 접기 탭) | 버튼이 보이자마자 쟀다. 버튼은 `/api/tracks` 응답 전에 먼저 나오고, 두 상자를 따로 두 번 읽었다. 새 clone 실행 1회에서 두 읽기 사이에 목록이 움직여 버튼이 재생보다 56 px 아래로 읽혔다. 수정 전 부하 4회 중 2회는 같은 측정 직후 116·117행(앞서 잰 줄 왼쪽 탭이 카드를 접지 못함, 30초 초과)에서 실패 | 버튼 대기 → 경로 선(`.route-map-button svg .shape-line`) 대기(maps.js가 모든 카드를 한 번에 그림), `settleLayout`, 버튼·재생의 `waitForSteadyLayout`, 그 뒤 한 번의 `evaluate`에서 두 상자를 함께 읽음 | 같음(개수 2·2, 배치 조건 그대로) | 수정 뒤 dark main 6/6(수정 전 4회 중 2회 실패) | 버튼을 재생 다음 줄로 넘긴 `library.css` 사본 → 118행(이전 113행) 그 단언으로 실패(버튼 y 803.5, 재생 y 855.5) | `9b24b6f` |
| `maps.cjs` 686행 영상 위 지도 크기 전환 중 고정 | 앱은 탭할 때 크기를 고정하고 280 ms 타이머로 푼다. 테스트는 프레임이 움직인 첫 rAF에서 표본을 떴다. 부하로 그 rAF가 280 ms보다 늦으면 이미 풀린 상태를 봤다 | 첫 rAF 대기 → 탭과 같은 작업 안에서 지도 틀의 width/height CSS 전환을 찾아 그 길이의 절반으로 옮기고 바로 표본을 뜸(타이머는 이 작업이 끝나기 전에 돌 수 없다) | 같음 + 전환이 없으면 실패(더 엄격) | 수정 뒤 light main 6/6 | `holdCanvas`를 바로 돌아가게 한 `maps.js` 사본 → 697행(이전 686행) 그 단언으로 실패(`held:false`, 틀은 이동 중간 320×180) | `b4820e5` |

위 지점 밖의 `waitForTimeout`은 건드리지 않았다.

`maps.cjs` 두 행의 부하는 `taskset -c 12-13`(light main은 `14-15`)에 같은 코어의 바쁜 루프 3개이고 횟수는 6회다. many·leak도 같은 부하에서 6/6, 무부하 1회 3항목 통과, 모두 `refused_restarts` 0.

**(기록) 686행을 고치기 전의 분석: `maps[map_schemes=light][map_parts=main]` 686·688행.** 부하에서 수정 전 실행기로 2회 중 1회 재현됐다(686행: 첫 표본에서 이미 `held:false`, 크기가 끝난 상태). 앞선 실패는 688행이었다(이동 중 붙든 크기가 최종 크기보다 14 px 큼, 허용 12 px). 원인은 테스트의 시간 가정으로 보인다. 앱(`maps.js` 1057행)은 탭 뒤 `setTimeout` 280 ms에 붙든 크기를 놓는다. 테스트는 프레임이 움직인 첫 rAF에서 표본을 뜬다. 부하로 rAF가 280 ms 넘게 밀리면, 첫 표본이 이미 놓인 상태를 본다. 실패 지점은 페이지 안의 배치 측정이므로 프록시 대기열(5 → 128) 가설과는 맞지 않는다. 대기열은 그대로 128로 올렸다. 상태 대기로 고치려면 표본을 rAF가 아닌 시점(예: 크기 고정이 걸릴 때의 변화 관찰)에서 떠야 한다. 단언 의미를 바꾸지 않는 방법을 정하지 못해 고치지 않았다. 남은 위험이다.

## 가져오지 않은 항목

`replay_format2`는 Python 변환기를 직접 불러서 형식 2 확장과 대조한 이식용 검사였다. HARS 브라우저 CI에서는 실행한 적이 없어 manifest에서도 제거했다. `replay_gzip.cjs`는 gzip과 일반 JSON 로드, 압축 프레임 확장 및 재생 화면을 검사한다.

## 실행

```sh
npm ci --prefix tests/browser                # playwright 1.64.1 (package-lock.json)
npx --prefix tests/browser playwright install --only-shell chromium
cargo build --locked
python3 tests/browser/run_browser_tests.py \
  --hars-bin "${CARGO_TARGET_DIR:-target}/debug/roadviewer-hars" --out /tmp/rv-browser [--groups g1 g2] [--only maps.cjs tags]
python3 tests/browser/run_browser_tests.py --aggregate /tmp/shard1 /tmp/shard2 ... --out /tmp/rv-gate
python3 tests/browser/test_gate_rules.py     # 게이트 데이터·판정 규칙(브라우저 없음, CI에서 돈다)
```

Node 22가 PATH에 있어야 한다(`--node`로 지정할 수도 있다). 종료 코드는 게이트 판정이다: 0 통과, 1 게이트 실패, 2 HARS 실행 파일 없음, 130 중단. 결과는 `--out` 아래에 남는다: `summary.json`(판정·사유, `expected_version`, 항목별 결과·초·단언 수·파싱한 실패·남은 프로세스·남은 임시 항목·어댑터 사건, `exit_code`)과 `<항목>/{test.log,hars.log,result.json,screenshots/,collect/}`. 테스트당 시간 상한은 기본 300초다. 가장 느린 항목인 `maps` main이 61~75초 걸리므로 여유는 4배다. 이 상한은 멈춘 테스트를 CI 작업 상한(20분) 안에서 실패시키기 위한 것이다. `--repeat`와 재시도 옵션은 없다.

## 게이트

게이트는 `tests/browser/expectations.json`의 `entries`(50)와 `restart_allowed`(`maps[map_parts=many,leak]`)를 사용한다. 다른 키(예상 실패, 재시도)가 있으면 실행기가 `expectations.json refused`로 끝난다.

게이트 실패 조건:

- 결과가 `pass`가 아닌 항목: `fail`, `timeout`, `supervisor-failed`, `isolation-failed`, `interrupted`.
- 통과한 항목이 프로세스를 남김(`leftover_processes`).
- 어댑터가 `restart_allowed` 밖의 항목에서 HARS를 재기동했거나, 기록 폴더 변화를 보고 재기동을 거부함(`restart refused`).
- 실행된 테스트가 0개이거나, 선택한 항목이 빠지거나, 중복되거나, 선택하지 않은 항목이 실행됨.
- 실행기가 남긴 프로세스, 중단.
- `--aggregate`: 묶음 합이 manifest 50개와 정확히 같지 않음(빠짐·중복), `entries`와 manifest 수가 다름, 묶음의 실제 종료 코드(`exit-code`) ≠ 요약의 `exit_code` ≠ 그 묶음 결과로 다시 계산한 판정, 요약이나 `exit-code`가 없음. 요약에 적힌 `gate`는 믿지 않고 결과에서 다시 계산한다.

`test_gate_rules.py`는 브라우저 없이 재기동 허용 목록, manifest 항목 수, CI matrix의 그룹 배정, `judge`·`verdict` 규칙을 검사한다.

## 실패 산출물

- **스크린샷.** Node를 `--require tests/browser/failure_capture.cjs`로 띄운다. 단언 실패나 Playwright 시간 초과가 생기면, 테스트의 `finally`가 `browser.close()`를 부를 때 열린 페이지마다 `screenshots/page-N.png`과 `failure.txt`를 남긴다. 오류 객체의 생성에 접근자를 걸 뿐 테스트의 호출을 감싸지 않으므로 스택과 타이밍은 그대로다. 다른 오류만으로 실패하면 스크린샷이 없다.
- **trace.** 판정이 끝난 뒤 게이트를 실패시킨 항목만 같은 조건으로 한 번 더 돌린다. 이때 `HARS_TRACE_DIR`로 trace를 켠다(`collect/trace/trace-N.zip`). 이 재실행은 `collection_reruns`에만 적고 판정에 쓰지 않는다.
- **CI 아티팩트.** 묶음마다 `browser-summary-N-attempt-M`(`summary.json` + `exit-code`)을 남긴다. M은 GitHub Actions 실행 시도 번호다. 묶음이 성공하지 못하면(실패·시간 초과·취소) `browser-artifacts-N`(산출물 폴더 전체)을 남긴다. 보존 기간은 7일이다. 집계 결과는 `browser-gate`다.
- **비밀값·좌표.** fixture는 합성 데이터뿐이다. 서버형 테스트의 Drive는 `fake_drive.py`이고, 토큰은 테스트가 만든 가짜 값이다. HARS 데이터 폴더는 테스트마다 새 임시 폴더다.

## 실행기 구조

- **미러.** 실행을 시작할 때 HARS를 임시 데이터 폴더(기록 1개)로 한 번 띄운다. 포트는 19180~19199 중 빈 두 개를 쓴다. 준비 판정은 연결 성공이 아니다. 띄운 프로세스가 살아 있고 그 포트의 listen 소켓을 가진 프로세스가 그 pid여야 한다(`/proc/net/tcp`와 `/proc/<pid>/fd`). 그래야 같은 PC의 다른 실행기가 띄운 HARS 화면을 받지 않는다. 그다음 HARS 소유 `web/`의 파일마다 HARS가 Python과 같은 URL에서 내보내는 내용을 받는다(`mirror_web.py`). 미러 단계에서 예외가 나도 임시 폴더는 지워진다.
- **작업 폴더(테스트마다 새로).** `roadviewer/app/web`(HARS `web/`의 서빙 결과 미러), `roadviewer/config.yaml`(HARS의 `roadviewer_hars/config.yaml`), `tests/`(`tests/browser` 항목마다 심볼릭 링크), `tmp/`(`TMPDIR`). 환경변수 `HARS_EXPECTED_VERSION` = `Cargo.toml`의 패키지 버전이다.
- **Python 서버 자리(`hars_as_python.py` + `pyshim/`).** 테스트는 `RV_PYTHON -u -c PROGRAM`으로 서버를 띄운다. shim이 `server`·`werkzeug.serving`을 대신해 HARS를 띄운다. 그리고 `/`와 `/api/hassio_ingress/test/` 둘 다에서 답하는 HTTP/1.0 프록시를 돌려준다(연결 대기열 128, werkzeug와 같음). HARS 준비 판정은 미러와 같다(살아 있음 + 소켓 소유). 기록 폴더가 바뀌면 HARS를 재기동하는 동작은 `HARS_SHIM_RESTART=1`일 때만 한다. 실행기는 이 값을 `restart_allowed` 항목에만 준다. `meta.json`의 `id`를 채우던 코드는 없앴다.
- **감독 프로세스.** 테스트마다 `run_browser_tests.py --supervise`(실행 시작 때 만든 사본)가 child subreaper가 되어 Node를 띄운다. 테스트가 끝나거나, 실패하거나, 시간을 넘기거나, 중단되면 `/proc`에서 자손 전체를 찾아 SIGKILL한다. 감독 자체가 상한을 넘기면 실행기가 그 감독의 프로세스 트리와 작업 폴더를 그 자리에서 지운다.
- **외부 차단.** 감독을 `unshare -rn`(loopback만) 안에서 띄우고, 테스트 전에 장치가 `lo` 하나뿐이고 `1.1.1.1:443` 연결이 `ENETUNREACH`인지 확인한다. 아니면 `isolation-failed`다.
- **포트.** loopback 19000~19199만 쓴다. 병렬 슬롯마다 40개, 미러는 19180~19199다.

## 범위와 한계

- 모킹형 39개 파일은 HARS의 API를 부르지 않는다. HARS가 내보내는 화면 파일(`web/` + 버전·제목 치환)만 검사한다. HARS API에 닿는 것은 Python 서버형 8개 파일이다(archive_settings, archive_status, bookmarks, gdrive_panel, library_tabs, maps, navigation, tags).
- `maps[map_parts=many,leak]`는 어댑터의 재기동에 기대어 통과한다. HARS는 기록을 시작할 때 한 번 읽고, 이 테스트는 서버가 도는 중에 폴더에 기록을 직접 써 넣는다.
- 미러는 HARS가 같은 URL에서 내보내는 본문이다. HARS의 응답 헤더(CSP 등)는 모킹형 테스트에 가지 않는다.
- `replay_format2`의 공백([위](#가져오지-않은-항목)).

## CI 구성

`.github/workflows/ci.yml`. 브라우저 작업은 PR과 수동 실행에서만 돈다(`main` 푸시에서는 `Cache refresh (browser)`가 npm·Playwright 캐시만 저장한다. [DEVELOPMENT.md](DEVELOPMENT.md#cicd)). `Browser build`는 `Rust tests`와 같은 캐시(`hars-rust-test-v1-*`, `main`의 `Cache refresh (Rust tests)`가 저장)를 복원만 하고, `cargo build --locked --bin roadviewer-hars`를 한 번 돌려 실행 파일을 넘긴다. 그다음 `Browser shard 1~9`가 돌고, `Browser checks`와 `HARS required`가 이어진다. 같은 실행 파일을 `Compatibility`도 받아 쓴다.

| 작업 | 하는 일 |
|---|---|
| `Browser shard N`(N = 1~9) | `browser-groups.json`의 `gN` 하나를 직렬로 돈다. matrix는 `fail-fast: false`, 작업 상한은 20분이다. Node 22.13.1과 `npm ci`(Playwright 1.64.1)를 쓰고, `chromium-headless-shell`만 설치한다. 공유 라이브러리는 `ldd`로 확인한다. sysctl로 AppArmor 제한을 푼 뒤 `unshare -rn` 안에서 `--isolation-check`를 돌린다. 실행기 종료 코드는 `exit-code`에 적는다 |
| `Browser checks` | `test_gate_rules.py`(게이트 데이터·판정 규칙)를 돌린다. 그다음 9개 묶음 각각의 최신 시도 요약을 받아 `--aggregate`로 다시 판정한다. 이 결과는 `HARS required`(필수 검사 이름은 이것 하나다)가 요구한다 |

묶음은 이전에 5개였다(두 그룹씩). 러너에서 84/164/168/169/174초로 고르지 않았고, 가장 긴 경로가 build 62초 + 174초였다. 그래서 한 그룹에 묶음 하나로 바꿨다. `test_gate_rules.py`는 matrix가 manifest 그룹을 각각 정확히 한 번 부르는지 확인한다.

러너 실측(9묶음): `e6331a8`에서 전 작업 통과. `Browser build` 1분 14초, 묶음 9개 각 1분 26초~2분 3초, 집계 9초. 같은 실행에서 `Rust checks` 3분 43초, `Container` 2분 57초·3분 7초. (`Rust checks`는 이후 `Rust lint`·`Rust tests`·`Compatibility`·`Repository checks`로 나뉘었다.)

## 시간

로컬 측정 환경은 WSL2 Ubuntu 24.04, 16 vCPU, HARS debug 빌드다. 다른 작업자의 부하 재현이 같이 돌았으므로 단일 값으로 본다.

| 항목 | 값 |
|---|---|
| 직렬 전체 1회(50항목 + 미러) | 작업 사본 619초(부하 평균 4.4 → 2.5), 새 clone 608초(부하 평균 2.4 → 2.5) |
| 가장 느린 항목 | `maps` light main 62.2초, `maps` many·leak 47.7초, `motion` 32.1초(새 clone 실행). 테스트당 상한 300초 |

## 검증(2026-11-17)

| 실행 | 결과 |
|---|---|
| 직렬 전체 1회(작업 사본, `a955b80` 기준 + 9묶음 CI) | 게이트 통과: 통과 50, 제외 1, 실패 0. 남은 프로세스 0, 임시 폴더 삭제 |
| 직렬 전체 1회(깨끗한 새 clone, 영상 fixture 추가 뒤) | 통과 49, 제외 1, 실패 1: `maps[map_schemes=dark][map_parts=main]` 113행(0.8초, 수집용 재실행은 통과. [미검증과 남은 위험](#미검증과-남은-위험)). clone에 `tests/browser` 55개 파일이 추적되고, `test_gate_rules.py` 통과 |
| 전체 결과를 두 묶음 요약으로 나눠 `--aggregate` | 둘 다: 통과. 하나만: 종료 1, `entries run 30 + excluded 0 != 51`(빠진 항목 이름 표시) |
| `test_gate_rules.py` | 14개 통과. 사본에서 `excluded`에 `tags`를 더하면 `test_excluded_is_exactly_replay_format2` 실패 |
| Rust·저장소 | `cargo fmt --all --check` 0, `cargo clippy --locked --all-targets -D warnings` 0, `cargo nextest run --locked --all-targets` 312/312, 옛 원본 무결성 검사 286개 차이 없음, `scripts/check_release.py` 통과(2.0.0-alpha.4), `actionlint` 통과 |

게이트 무결성 점검(각 1회, 모두 게이트가 실패해야 정상):

| 일으킨 것 | 방법 | 결과 |
|---|---|---|
| 단언 하나가 실패하는 화면 | `--web` 사본의 버전 줄을 `v9.9.9`로 바꿈 | 종료 1, `replay_layout` 216행. 수집용 재실행이 `collect/trace/trace-1.zip`, `trace-2.zip`, 스크린샷을 남김 |
| 묶음 누락 | 위 두 묶음 중 하나만 `--aggregate` | 종료 1, 21개 항목 누락 |
| HARS 실행 파일 없음 | `--hars-bin /nonexistent/...` | 종료 2 |
| 차단 실패 | 옵션을 버리고 명령만 실행하는 가짜 `unshare`를 PATH 앞에 둠 | 종료 1, `isolation-failed`(`lo, eth0, docker0`) |
| 테스트 0개 | `--only no_such_test.cjs` | 종료 1, `no test ran` |
| 미러 포트의 listener가 띄운 프로세스가 아님 | HARS를 자식으로 띄우는 래퍼를 `--hars-bin`으로 줌(listen 소켓 소유자 ≠ 띄운 pid) | 종료 1, `mirror HARS (pid …) is not listening on 127.0.0.1:19186`, 테스트 시작 전 |
| 허용되지 않은 재기동 | 실행기 폴더 사본의 `restart_allowed`를 비우고 `maps[map_parts=many,leak]` 실행 | 종료 1, 사유에 `adapter {'restarts': 0, 'refused_restarts': 1}` |

과부하 재현(수정 전 실행기와 원본 사본, 부하 조건은 위와 같음, 항목당 8회): `conversion_controls` 4/8 실패(모두 99행 `check()`), `replay_gzip` 4/8 실패(57행), `maps` light main 2회 중 1회 실패(686행. 나머지 6회는 실행기를 고치는 중이라 무효).

## 분류표(1단계 측정)

이 표는 1단계(`5a2426f`, release 빌드, 2단계 수정 전) 측정이다. 분류는 `library_tabs`만 C → A로 바뀌었다(제품 수정 뒤 통과). 최종 수치는 [검증](#검증). `5회`는 직렬 1회에 통과한 48개 항목을 직렬로 5번 반복한 결과다(240/240 통과, 2,756초). `4병렬`은 `--jobs 4` 전체 실행 3회의 통과 수다. `초`는 직렬 전체 실행 1회의 항목별 소요다. 숫자 원본은 `measurements/browser-tests.json`.

| 항목 | 기능 영역 | 단언 수 | 서버 획득 | 분류 | 5회(직렬) | 4병렬 | 초(직렬) |
|---|---|---|---|---|---|---|---|
| `replay_coverage` | 긴 영상·로그, 영상 없는 구간, 시킹 | 56 | 웹 파일+모킹 | A | 5/5 | 3/3 | 30.2 |
| `display_preferences` | 표시 설정 저장·복원 | 51 | 웹 파일+모킹 | A | 5/5 | 3/3 | 3.9 |
| `concurrency` | 변환 동시 실행 수 설정 | 6 | 웹 파일+모킹 | A | 5/5 | 3/3 | 5.0 |
| `info_panels` | 저장공간·카메라 정보 패널 | 30 | 웹 파일+모킹 | A | 5/5 | 3/3 | 3.0 |
| `archive_status` | 미디어 상태 배지, NAS 상태 패널, 전체 이동 실행 | 43 | Python 서버→shim | A | 5/5 | 3/3 | 6.5 |
| `video_overlay` | 영상 오버레이 그리기·배치 | 240 | 웹 파일+모킹 | A | 5/5 | 1/3 | 11.1 |
| `upload_stall` | 멈춘 업로드 재시도 | 13 | 웹 파일+모킹 | A | 5/5 | 3/3 | 1.3 |
| `navigation` | 단일 문서 내비게이션, Ingress 접두사, 기록 | 47 | Python 서버→shim | B | 5/5 | 3/3 | 25.6 |
| `replay_play_request` | 재생 요청 취소·지연 | 9 | 웹 파일+모킹 | A | 5/5 | 3/3 | 1.0 |
| `recording_names` | 주행 묶음, 업로드 날짜, 접힘 기본값 | 81 | 웹 파일+모킹 | A | 5/5 | 3/3 | 8.2 |
| `archive_settings` | 아카이브 설정 폼(NAS/Drive, COPY/MOVE 경고, 저장·재적용) | 72 | Python 서버→shim | A | 5/5 | 3/3 | 2.7 |
| `fullscreen_landscape` | 터치 화면 전체 화면·가로 고정 | 26 | 웹 파일+모킹 | A | 5/5 | 3/3 | 3.7 |
| `long_drive` | 180구간 주행 스크롤 띠 | 47 | 웹 파일+모킹 | A | 5/5 | 3/3 | 9.8 |
| `lazy_rows` | 목록 행 지연 렌더링 | 22 | 웹 파일+모킹 | A | 5/5 | 3/3 | 2.3 |
| `upload_drop` | 끌어다 놓기 업로드 | 30 | 웹 파일+모킹 | A | 5/5 | 3/3 | 0.9 |
| `upload_parallel` | 4개 동시 업로드 | 13 | 웹 파일+모킹 | A | 5/5 | 3/3 | 4.7 |
| `video_sources` | 영상 소스·화질 선택 | 41 | 웹 파일+모킹 | A | 5/5 | 3/3 | 10.5 |
| `choices` | 선택 시트·메뉴(속도·범위·구간 이동) | 42 | 웹 파일+모킹 | A | 5/5 | 3/3 | 17.6 |
| `conversion_controls` | 자동 변환 토글, 변환·제거·완전 삭제 상태 | 47 | 웹 파일+모킹 | A | 5/5 | 3/3 | 3.8 |
| `whole_video` | 전체 영상 사본 전환 | 5 | 웹 파일+모킹 | A | 5/5 | 3/3 | 2.6 |
| `motion` | 전환·목록 애니메이션 | 73 | 웹 파일+모킹 | A | 5/5 | 3/3 | 32.8 |
| `log_export` | 로그 내보내기 | 28 | 웹 파일+모킹 | A | 5/5 | 3/3 | 1.4 |
| `storage_devices` | 저장공간·장치 탭 | 36 | 웹 파일+모킹 | A | 5/5 | 3/3 | 13.2 |
| `pairing_refresh` | 페어링 코드 갱신 | 20 | 웹 파일+모킹 | A | 5/5 | 3/3 | 6.1 |
| `segment_peek` | 구간 미리보기(호버·터치 길게 누름) | 77 | 웹 파일+모킹 | A | 5/5 | 3/3 | 16.4 |
| `replay_rendering` | 지도 라벨·표 렌더링 | 15 | 웹 파일+모킹 | A | 5/5 | 3/3 | 2.7 |
| `replay_glass_capture` | 재생 화면 테마·배지·탭 화면 | 24 | 웹 파일+모킹 | A | 5/5 | 3/3 | 23.5 |
| `download_dialog` | 다운로드 대화상자(형식·ZIP 범위) | 45 | 웹 파일+모킹 | A | 5/5 | 3/3 | 4.1 |
| `upload_timeout` | 업로드 시간 초과 판정(브라우저 없음) | 33 | Node만 | A | 5/5 | 3/3 | 0.0 |
| `tag_filter_fold` | 태그 필터 접기 | 20 | 웹 파일+모킹 | A | 5/5 | 3/3 | 2.0 |
| `replay_end_seek` | 끝 위치 시킹 | 10 | 웹 파일+모킹 | A | 5/5 | 3/3 | 0.6 |
| `tags` | 태그 편집·필터·일괄(실서버 304 폴링 포함) | 74 | Python 서버→shim | A | 5/5 | 3/3 | 22.5 |
| `bookmarks` | 북마크 기록·목록·편집(모바일) | 35 | Python 서버→shim | A | 5/5 | 3/3 | 2.9 |
| `camera_info` | 카메라 정보 대화상자 | 15 | 웹 파일+모킹 | A | 5/5 | 3/3 | 7.7 |
| `replay_layout` | 재생 화면 배치, 앱바, 고정, 버전 줄 | 146 | 웹 파일+모킹 | D | - | 0/3 | 8.0 |
| `disclosure_animation` | 접기·펼치기 애니메이션 | 32 | 웹 파일+모킹 | A | 5/5 | 3/3 | 5.9 |
| `lane_marking` | 차선 표시 코드(영상·도로 보기) | 35 | 웹 파일+모킹 | B | 5/5 | 3/3 | 0.9 |
| `replay_ready` | 변환 완료 전후 재생 링크 | 37 | 웹 파일+모킹 | A | 5/5 | 3/3 | 5.9 |
| `bulk_actions` | 목록 다중 선택, 일괄 작업, ZIP 크기 제한 | 110 | 웹 파일+모킹 | A | 5/5 | 3/3 | 19.4 |
| `device_test_page` | 장치 시험 페이지(HMAC, 조각 업로드) | 23 | 웹 파일+모킹 | A | 5/5 | 3/3 | 1.3 |
| `replay_gzip` | gzip 재생 데이터와 대체 경로 | 13 | 웹 파일+모킹 | B | 5/5 | 3/3 | 2.7 |
| `replay_format2` | 재생 데이터 형식 2 확장(Python compact 대조) | 6 | Node만 | F | - | 0/3 | 0.1 |
| `library_tabs` | 라이브러리 탭, 헤더, 경로 카드, 좁은 화면 앱바 | 94 | Python 서버→shim | C → A | - | 0/3 | 15.4 |
| `replay_downloads` | 재생 화면 메뉴(다운로드·정보·삭제) | 32 | 웹 파일+모킹 | B | 5/5 | 3/3 | 5.4 |
| `upload_mobile` | 모바일 업로드·정리 | 26 | 웹 파일+모킹 | A | 5/5 | 3/3 | 4.6 |
| `maps[map_schemes=dark][map_parts=main]` | 지도(목록 카드 경로, 주행 지도, 영상 위 지도, 다수 주행, 누수) | 302 | Python 서버→shim | A | 5/5 | 3/3 | 63.4 |
| `ripple` | 누름 물결 효과 | 16 | 웹 파일+모킹 | A | 5/5 | 3/3 | 7.9 |
| `maps[map_schemes=light][map_parts=main]` | 지도(목록 카드 경로, 주행 지도, 영상 위 지도, 다수 주행, 누수) | 302 | Python 서버→shim | A | 5/5 | 3/3 | 61.1 |
| `gdrive_panel` | Google Drive 패널(장치 코드 로그인, 해제, 재시도) | 113 | Python 서버→shim | A | 5/5 | 3/3 | 9.5 |
| `maps[map_parts=many,leak]` | 지도(목록 카드 경로, 주행 지도, 영상 위 지도, 다수 주행, 누수) | 302 | Python 서버→shim | B | 5/5 | 3/3 | 46.1 |
| `telemetry` | 텔레메트리 탭·그래프 동기화 | 98 | 웹 파일+모킹 | A | 5/5 | 3/3 | 14.4 |

분류 D(`replay_layout`)·F(`replay_format2`)의 처리는 지금은 [HARS가 고친 지점](#hars가-고친-지점)과 [가져오지 않은 항목](#가져오지-않은-항목)을 따른다.

## 미검증과 남은 위험

- 고정 시간 대기(`waitForTimeout`)가 18개 파일에 170곳 남아 있다. 실패가 확인된 곳만 고쳤다. 새로 실패가 나오면 그 지점을 상태 대기로 고친다(관리 원칙).
- 재기동 거부 규칙은 정상 동작에서도 드물게 게이트를 실패시킬 수 있다. 예를 들어 HARS가 자기 API로 기록을 만들거나 지우는 POST가 끝나기 전에 GET이 폴더 변화를 보는 경우다. 틀리는 쪽은 실패뿐이다(통과로 숨기지 않는다).
- `replay_gzip`은 프레임 변화 횟수에 하한을 두지 않는다. 범위는 변화 수 ≤ 다시 그린 수 ≤ 변화 수 + 4다. 재생이 `idx>=10`까지 진행했는지는 확인하지만, 변화 횟수 자체가 몇 회 이상인지는 단언하지 않는다.
- `maps` light main 686행(이동 중 고정)은 탭과 같은 작업 안의 표본으로 고쳤다([위](#hars가-고친-지점)). 앞서 한 번 나온 688행 실패(커질 때 붙든 크기가 최종 크기보다 14 px 큼, 허용 12 px)는 재현하지 못했고 원인을 모른다. 붙든 크기는 탭 때 잰 값이므로 이 수정과 관계가 없다.
- `maps` 113행(지도 버튼과 재생 버튼이 한 줄인지)은 두 번 따로 읽던 상자를 배치가 멈춘 뒤 한 번에 읽도록 고쳤다([위](#hars가-고친-지점)). 실패 때 무엇이 목록을 56 px 움직였는지는 확인하지 못했다.
- `conversion_controls` 99행의 "깨뜨림" 확인은 하지 못했다.
