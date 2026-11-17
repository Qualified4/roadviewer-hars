# 보존 원본 브라우저 테스트를 HARS에 붙이기

<!-- toc -->
**목차**

- [요약](#요약)
- [실행](#실행)
- [게이트](#게이트)
- [실패 산출물](#실패-산출물)
- [video_overlay 77행](#video_overlay-77행)
  - [게이트 처리(알려진 원본 테스트 결함)](#게이트-처리알려진-원본-테스트-결함)
- [실행기 구조](#실행기-구조)
- [대상과 세는 단위](#대상과-세는-단위)
- [분류표](#분류표)
- [대체 목록](#대체-목록)
- [C · D · F 상세](#c--d--f-상세)
- [실행기에서 찾아 고친 결함](#실행기에서-찾아-고친-결함)
- [자원 정리 확인](#자원-정리-확인)
- [시간·용량](#시간용량)
- [CI 구성](#ci-구성)
- [검증](#검증)
  - [알려진 원본 테스트 결함 규칙(2026-11-17)](#알려진-원본-테스트-결함-규칙2026-11-17)
- [미검증과 남은 위험](#미검증과-남은-위험)
<!-- /toc -->

## 요약

원본 Python Road Viewer 1.3.18의 브라우저 테스트(`reference/python/tests/*.cjs`)를 원본 파일 수정 없이 HARS 실행 파일과 HARS가 내보내는 웹 파일에 붙여 돌린다. 실행기는 `integration/upstream_cjs/run_browser_tests.py`이고, GitHub Actions의 `Browser checks`(필수, `HARS required`가 요구)가 같은 실행기로 돈다.

2026-11-17 최종 상태(`ci/browser-checks`, 2.0.0-alpha.4, debug 빌드 = CI와 같은 빌드):

| 판정 | 수 | 항목 |
|---|---|---|
| 통과 | 49 | 아래 둘을 뺀 전부(1단계의 C `library_tabs`는 제품 결함을 고쳐 통과). 이 중 `video_overlay`는 알려진 원본 테스트 결함 규칙 적용(아래) |
| 엄격한 예상 실패 | 1 | `replay_layout`(214행 그 단언에서만 실패해야 함) + 버전 줄만 바꾼 사본으로 한 번 더: `replay_layout[release_version]` 통과 필수 |
| 제외 | 1 | `replay_format2`(Python `compact.py` import) |
| HARS 검사 | 1 | `video_overlay[hars_focus_check]`(`integration/upstream_cjs/video_overlay_focus_check.cjs`): `video_overlay`가 돌 때마다 함께 돌고 통과 필수 |
| 예상 밖 실패 | 0 | 직렬 전체 5회, CI 묶음 구성 3회(아래 [검증](#검증)) |

분류(1단계 A~F)는 바뀌지 않았고 `library_tabs`만 C에서 A가 됐다. `video_overlay` 77행은 원본 테스트의 대기 조건 결함으로 간헐 실패한다([아래](#video_overlay-77행)). 77행이 지키는 동작은 HARS 검사가 매번 확인하고, 원본 항목은 정확히 77행 그 서명으로 실패할 때만 최대 3회까지 시도해 뒤쪽 단언을 확인한다. 서명 실패 수는 `known_upstream_defect_hits`로 항상 남는다.

"51개"는 `browser-groups.json`의 항목 수다. 테스트 파일은 49개이고 `maps.cjs`가 환경변수만 다르게 세 항목으로 들어 있다. `browser-helpers.cjs`와 `run_group.cjs`는 테스트가 아니다.

## 실행

```sh
cd integration/upstream_cjs && npm ci && cd -        # playwright 1.64.1 (package-lock.json)
npx --prefix integration/upstream_cjs playwright install --only-shell chromium
cargo build --locked
python3 integration/upstream_cjs/run_browser_tests.py \
  --hars-bin "$CARGO_TARGET_DIR/debug/roadviewer-hars" --out /tmp/rv-browser [--groups g1 g2] [--only maps.cjs tags] [--repeat 5]
# CI 묶음을 모아 판정(각 폴더: summary.json + exit-code)
python3 integration/upstream_cjs/run_browser_tests.py --aggregate /tmp/shard1 /tmp/shard2 ... --out /tmp/rv-gate
# Python 기준 실행(E 판정용): Flask가 있는 인터프리터. 예상 실패·제외 없이 선택한 것 전부가 통과해야 한다
python3 integration/upstream_cjs/run_browser_tests.py --python-baseline ~/venv/bin/python --out /tmp/rv-py --only gdrive_panel.cjs
```

Node 22가 PATH에 있어야 한다(`--node`로 지정 가능). 종료 코드는 게이트 판정이다(0 통과, 1 게이트 실패, 2 HARS 실행 파일 없음, 130 중단). 결과는 `--out` 아래 `summary.json`(게이트 판정·사유, 예상 실패·제외 목록, 항목별 결과·초·단언 수·분류·파싱한 실패·남은 프로세스·남은 임시 항목, `exit_code`)과 `run<N>/<항목>/{test.log,hars.log,result.json,screenshots/,collect/}`이다. `hars.log`에는 HARS 출력과 어댑터 사건(`[shim ...] proxy`, `restart`, `filled id`)이 시간순으로 들어 있다(서버형 항목만). `--pw-debug`는 `DEBUG=pw:api`로 Playwright 호출·요청·응답 헤더를 `test.log`에 남긴다. `--no-failure-shots`는 사전 로드 없이, `--no-collect`는 수집용 재실행 없이 돈다. `--jobs N`(병렬)은 남아 있지만 `video_overlay` 때문에 게이트용으로 쓰지 않는다.

실행 언어는 Python이다. 서버 어댑터가 Python이고, 프로세스 감독에 `prctl`과 `/proc`이 필요하다.

## 게이트

데이터는 `integration/upstream_cjs/expectations.json` 한 곳이다. `entries: 51`, 예상 실패 1개(`replay_layout`), 알려진 원본 테스트 결함 1개(`video_overlay`), HARS 검사 1개(`video_overlay[hars_focus_check]`), 제외 1개(`replay_format2`). 실행기는 요약 첫머리에 이 목록과 사유를 그대로 찍는다. 목록에 항목을 더하면 게이트가 받아들이는 것이 바뀌므로, 이 문서의 이 표와 함께 고친다.

| 목록 | 항목 | 판정 조건 | 사유 |
|---|---|---|---|
| 예상 실패 | `replay_layout` | `test.log`에 Node가 찍은 오류 머리줄(`이름 [코드]: 메시지`)이 **정확히 1개**, 이름 `AssertionError`, 메시지 `version is an info line in ⋯`, 그 아래 스택에서 처음 나오는 `tests/replay_layout.cjs:<줄>`이 **214**, 종료 코드 1(outcome `fail`). 통과하면 `unexpected-pass`로 게이트 실패(정식 버전이 되면 목록에서 빼야 한다는 신호). 다른 줄·다른 메시지·시간 초과는 `fail` | 214행이 버전 줄을 `^v숫자.숫자.숫자$`로 검사하고 HARS는 의도대로 `v2.0.0-alpha.4`를 낸다 |
| 예상 실패의 짝 | `replay_layout[release_version]` | 통과 필수. 미러 사본에서 `index.html`의 `<span class="app-version">vX.Y.Z-접미사</span>` 한 곳(정확히 1회 일치해야 함)만 `vX.Y.Z`로 바꾸고 같은 원본 파일을 돌린다. 테스트 파일은 그대로다. 일치가 0회(접미사 없음)면 `not-run`으로 게이트 실패 | 214행 뒤의 단언이 가려지지 않게 |
| 알려진 원본 테스트 결함 | `video_overlay` | 통과하면 통과. 실패가 정확히 77행 서명(오류 머리줄 1개, `AssertionError`, 메시지 `closing hands focus to nothing`, 스택에서 처음 나오는 `tests/video_overlay.cjs:<줄>`이 77, outcome `fail`)일 때만 같은 항목을 다시 돌린다. 시도 상한 3회(재실행 최대 2회). 한 번이라도 통과하면 통과, 3회 모두 그 서명이면 게이트 실패, 서명이 조금이라도 다르면(다른 줄·메시지, 머리줄 2개 이상, 시간 초과) 재실행 없이 게이트 실패. 서명 실패 수를 `known_upstream_defect_hits`로 요약(`summary.json`, 0이어도)과 표준 출력 요약 줄에 찍는다 | 원본 테스트의 대기 조건 결함([video_overlay 77행](#video_overlay-77행)) |
| HARS 검사 | `video_overlay[hars_focus_check]` | `video_overlay`를 돌릴 때마다(반복·묶음마다) 정확히 1회, 통과 필수. 재실행 없음 | 77행이 지키는 동작을 결함 없이 확인 |
| 제외 | `replay_format2` | 실행하지 않고 요약에 `excluded`로 사유와 함께 남는다. 실행되면 게이트 실패 | Python `compact.py`의 `compact_data`·`expand_frames`를 import한다. HARS에는 Python 변환기가 없다 |

알려진 원본 테스트 결함 항목의 제한(실행기가 강제): `known_upstream_defects`가 1개를 넘거나, 항목에 `kind`·`reason`·서명(`error`·`message`·`line`)·`max_attempts`(1~3)·`upstream_fix`·`guarded_by`(그 항목과 함께 도는 `hars_checks` 항목) 중 하나라도 없거나 어긋나거나, 같은 이름이 예상 실패·제외에도 있으면 실행기가 `expectations.json refused: ...`로 종료 1(실행·`--aggregate` 모두). 실행기에 일반 재시도 옵션은 없다. 재실행 판단은 `another_attempt`, 판정은 `judge_defect`(`run_browser_tests.py`)이고 `--aggregate`도 요약의 결과(`earlier_attempts`에 이전 시도)에서 다시 계산한다. 시도별 산출물은 `run<N>/video_overlay/`(1회차), `attempt-2/`, `attempt-3/`이다. `--python-baseline`에서는 이 규칙도 끈다(선택한 것 전부 1회에 통과해야 함). 규칙 자체의 단위 검사: `python3 integration/upstream_cjs/test_gate_rules.py`(브라우저 없음, CI에는 넣지 않음).

`replay_format2`가 지키던 것(Python `compact.py` 인코더와 `app.js` 디코더의 일치)의 HARS 쪽 대체: 인코더 쪽은 `integration/pipeline_contract.py`(CI, 합성 입력)와 `integration/real_conversion_compare.py`(13·14·47 실주행 구간, 로컬)가 HARS의 replayV2 전체가 Python 출력과 같음을 요구하고, `scripts/verify_reference.py`가 `app.js`가 보존 원본 그대로임을 지킨다. **공백:** HARS 출력에 `app.js`의 형식 2 확장(`expandFlat`)을 실제로 돌려 보는 검사는 없다. HARS 출력 = Python 출력이고, Python 출력과 `app.js`의 일치는 원본 저장소의 이 테스트가 확인했다는 간접 근거뿐이다.

그 밖의 게이트 실패 조건(각각 일부러 일으켜 확인, [검증](#검증)):

- 결과가 `pass`(또는 위 조건을 만족하는 예상 실패)가 아닌 항목이 하나라도 있음: `fail`, `timeout`, `supervisor-failed`, `isolation-failed`, `interrupted`.
- 실행된 테스트 0개. 반복마다 실행 + 제외 ≠ 선택한 항목 수, 같은 항목 중복, 제외 항목 실행.
- 실행기가 남긴 프로세스, 중단(Ctrl-C).
- `--aggregate`: 묶음 합이 manifest 51개와 정확히 같지 않음(빠짐·중복), `expectations.json`의 `entries`와 manifest 수가 다름, 묶음의 실제 종료 코드(`exit-code` 파일) ≠ 요약의 `exit_code` ≠ 그 묶음 결과로 다시 계산한 판정, 요약이나 `exit-code`가 없음. 판정은 요약에 적힌 `gate`를 믿지 않고 결과(outcome·파싱한 실패)에서 다시 계산한다.

## 실패 산출물

- **스크린샷.** Node를 `--require integration/upstream_cjs/failure_capture.cjs`로 띄운다. 모든 테스트가 `finally`에서 `browser.close()`를 부르므로, 실패한 단언 직후 그 close가 먼저 열린 페이지마다 `screenshots/page-N.png`과 `failure.txt`(첫 실패 메시지·URL)를 남긴다. 실패는 오류 객체가 만들어질 때 안다: `AssertionError.prototype.generatedMessage`와 Playwright `errors.TimeoutError.prototype.name`에 접근자를 두면 생성자가 그 이름에 값을 넣을 때 setter가 불린다. setter는 원래의 일반 데이터 속성을 그대로 만들어 준다. 테스트의 호출을 감싸지 않으므로 스택·`assert(x)`의 자동 메시지·통과 경로의 타이밍이 그대로다(감싸기 방식은 `assert(x)`의 자동 메시지가 래퍼 줄을 가리키게 되어 버렸다). 확인: 합성 스크립트로 통과·단언 실패·시간 초과·잡힌 실패 4경우의 `test.log`가 사전 로드 유무로 바이트 단위 같음, 전체 1회를 사전 로드 없이 돌린 결과(`--no-failure-shots`)와 5회 결과의 항목별 판정이 같음, 통과한 255회 실행에서 스크린샷이 찍힌 적 0회(`screenshots_discarded_on_pass`). 한계: Playwright의 시간 초과가 아닌 오류(대상 닫힘 등)만으로 실패하면 스크린샷이 없고, 테스트가 단언 실패를 스스로 잡고 통과하면 close에서 스크린샷을 찍은 뒤 버린다(255회 중 0회).
- **trace.** 상시로 켜지 않는다. 판정이 끝난 뒤, 게이트를 실패시킨 항목(예상 실패 제외)만 같은 조건으로 한 번 더 돌리며 `HARS_TRACE_DIR`로 모든 context에 trace를 켠다(`collect/trace/trace-N.zip`, `collect/test.log`, `collect/screenshots/`). 이 재실행의 결과는 `collection_reruns`에만 적고 판정에 쓰지 않는다. 요약에 `collection rerun (artifacts only, not judged)`로 찍힌다.
- **CI 아티팩트.** 묶음마다 항상 `browser-summary-N`(`summary.json` + `exit-code`), 실패하면 `browser-artifacts-N`(산출물 폴더 전체: 항목별 `test.log`, `hars.log`, 스크린샷, trace)을 7일 보존한다.
- **비밀값·좌표.** fixture는 합성 데이터뿐이다(`tests/fixtures`: 2초 검은 영상 `black.mp4`, 합성 색 영상, AppArmor 프로필 원문. 테스트의 기록·GPS는 코드 안의 작은 합성 값). 서버형 테스트의 Drive는 `fake_drive.py`, 토큰은 테스트가 만든 가짜 값이다. HARS 데이터 폴더는 테스트마다 새 임시 폴더다.

## video_overlay 77행

77행 `closing hands focus to nothing`: 겹쳐 보기 설정 대화상자에서 `초기화`를 누른 뒤(포커스가 대화상자 안 `#overlayHeightReset`) Escape를 누르고 `dialog.waitFor({state:'hidden'})` 뒤 `document.activeElement===document.body`를 본다.

- 앱(`video-overlay.js` 617~618행): `cancel`에서 `dialog.close()`를 부르고, 포커스는 `close` **이벤트** 처리기에서 설정 버튼으로 넘긴 뒤 `ripple.js`의 focusin 처리가 마이크로태스크로 놓는다(그래서 body). `close()`가 `open`을 바로 지우므로 대화상자는 그 순간 `hidden`이지만, `close` 이벤트는 나중에 따로 온다.
- 테스트는 `hidden`만 기다린다. 그 사이에 `activeElement`를 읽으면 숨은 `#overlayHeightReset`이 아직 포커스를 갖고 있다.
- 증거(`integration/upstream_cjs/probes/video_overlay_focus.cjs`, 게이트 밖 별도 스크립트, 같은 페이지·모킹·단계를 30회 반복하며 페이지 안에서 `close` 시각 기록): 무부하 2/30, 2코어+바쁜 루프 2개 4/30 실패. **실패 6회 모두** 읽는 시점에 `close`가 아직 안 왔고 `activeElement`가 `overlayHeightReset`, **통과 54회 모두** `close`가 먼저 왔다. `close`는 keydown 뒤 무부하 2.2~4.9 ms(중앙값 2.8), 부하 2.9~22.1 ms. 실패한 읽기는 keydown 뒤 5.0~13.9 ms.
- 결론 **(b) 원본 테스트의 대기 조건 결함.** 테스트가 확인하려는 상태는 `close` 이벤트 뒤에야 성립하는데 테스트는 그보다 먼저 참이 되는 `hidden`만 기다린다. 웹 파일은 HARS가 그대로 내보내므로 Python과 공통이다(1단계에서 Python 원본으로도 부하 시 3/8 실패). 사용자에게 보이는 결함은 아니다(숨은 버튼이 몇 ms 포커스를 가짐). 원본을 고치지 않고는 못 고친다. 실행기 조건(미러, shim, 사전 로드)은 원인이 아니다: 서버를 쓰지 않는 모킹형이고, 사전 로드 유무 교차 실행에서 차이가 없다(아래).
- 반복(원본 테스트 그대로, `--only video_overlay.cjs`, 직렬):

| 조건 | 실행기 | 통과 | 부하 평균(시작→끝) |
|---|---|---|---|
| 직렬 무부하 1차 | 이전 실행기(사전 로드 없음) | 25/30 (실패 5, 이 중 1회는 내 다른 브라우저 시험과 겹침) | 0.60 → 1.34 |
| 직렬 무부하 2차 | 최종 실행기 | 30/30 | 1.54 → 1.19 |
| `taskset -c 0-1` 1차 | 이전 실행기 | 23/30 | 1.34 → 1.68 |
| `taskset -c 0-1` 교차, 사전 로드 켬 | 최종 실행기 | 27/30 | 0.68 → 1.82 |
| `taskset -c 0-1` 교차, 사전 로드 끔 | 최종 실행기 `--no-failure-shots` | 28/30 | (위와 같은 시간대, 번갈아) |
| 직렬 전체 5회 + 사전 로드 없는 전체 1회 + CI 묶음 3회 | 최종 | 9/9 | 1.3~3.0 |

실패 17회 모두 77행이다. 직렬 무부하에서도 0~17 %로 흔들리므로 "부하 없는 직렬에서만 안정적"이라고 할 수 없다(위 표는 이 정책 전 측정이다).

### 게이트 처리(알려진 원본 테스트 결함)

- **무엇이 결함인가.** 앱 동작이 아니라 원본 테스트의 대기 조건이다. 77행이 확인하려는 상태(포커스가 아무 것에도 없음)는 `close` 이벤트 뒤에 성립하는데 테스트는 그보다 먼저 참이 되는 `hidden`만 기다린다. Python 서버에서도 같다. 원본에서 고칠 한 줄: `reference/python/tests/video_overlay.cjs` 77행 앞에 `await page.waitForFunction(()=>document.activeElement===document.body);`(또는 `close` 이벤트 대기). HARS는 `reference/`를 고치지 않는다.
- **동작은 HARS 검사가 지킨다.** `integration/upstream_cjs/video_overlay_focus_check.cjs`는 원본과 같은 페이지·모킹·조작(설정 버튼으로 대화상자 열기, `초기화`로 포커스를 대화상자 안에 두기, 높이 75, Escape, `hidden` 대기)을 5회 반복하며, 매번 Escape 전에 포커스가 `#overlayHeightReset`에 있는지 보고 `close` 이벤트를 기다린 뒤 77행과 같은 조건 `document.activeElement===document.body`를 같은 메시지로 단언한다. `video_overlay`가 돌 때마다 함께 돌고 실패하면 게이트 실패다(재실행 없음).
- **원본 항목은 서명이 정확할 때만 다시 돈다.** 재실행을 77행 서명 하나로 묶은 이유: 그 실패의 원인은 확인됐고(위 probe: 실패 6회 모두 `close` 전 읽기) 그 동작은 HARS 검사가 따로 확인하므로, 77행 실패 자체는 새 정보가 없다. 다시 도는 목적은 77행 뒤의 단언(파일 끝까지)을 가리지 않는 것이다. 다른 줄·다른 메시지·시간 초과는 원인을 모르는 실패이므로 재실행으로 숨기지 않고 즉시 게이트 실패다. 3회 모두 77행이면 우연으로 보기 어려우므로(2코어 기준 실패율 약 10 %면 0.1 %) 게이트 실패다. 서명 실패 수(`known_upstream_defect_hits`)는 통과할 때도 남으므로 비율이 바뀌면 보인다.
- **지울 때.** 원본 저장소가 77행을 고쳐 다시 들여오면 `expectations.json`의 `known_upstream_defects.video_overlay`를 지운다(HARS 검사는 남겨도 된다). 원본이 고쳐졌는데 항목이 남아 있어도 판정은 같지만(통과하면 통과), 남은 항목은 우회 통로이므로 지운다.

## 실행기 구조

- **미러.** 실행 시작에 HARS를 임시 데이터 폴더(기록 1개)와 loopback 19180/19181로 한 번 띄워, 보존 웹 파일마다 HARS가 Python과 같은 URL에서 내보내는 내용을 받는다(`mirror_web.py`, 43개). `library.html`은 `/?fragment=1`, `index.html`은 `/view/<id>/?fragment=1`, 나머지는 `/assets/<이름>`이다. 그래서 HARS의 버전 치환·제목 치환이 들어간 페이지가 테스트에 간다.
- **작업 폴더(테스트마다 새로).** `roadviewer/app/web`(미러), `roadviewer/config.yaml`(HARS의 `roadviewer_hars/config.yaml`), `tests/`(보존 테스트 폴더 항목마다 심볼릭 링크), `tmp/`(`TMPDIR`). 테스트는 원본 경로(`reference/python/tests/X.cjs`)로 실행하고 작업 폴더를 현재 폴더로 준다. `roadviewer/app`에 Python 모듈이 없으므로 실수로 Python 서버가 뜰 수 없다.
- **Python 서버 자리(`hars_as_python.py` + `pyshim/`).** 테스트는 `RV_PYTHON -u -c PROGRAM`으로 서버를 띄운다. `hars_as_python.py`는 `PROGRAM`을 그대로 실행하되 `pyshim/`를 모듈 경로 맨 앞에 둔다. `sitecustomize.py`의 finder가 `server`, `werkzeug.serving`, `werkzeug.middleware.dispatcher`를 shim으로 돌린다(테스트가 `sys.path.insert(0,'roadviewer/app')`을 해도). `make_server`는 RV_DATA로 HARS를 띄우고 `/`와 `/api/hassio_ingress/test/` 둘 다에서 답하는 HTTP/1.0 프록시를 돌려준다. 그래서 `fake_drive.py`와 그 환경변수(`RV_GDRIVE_*`), 표준 입력 `grant` 명령 같은 테스트 자체의 fixture는 쓰인 그대로 돈다. HARS가 Python과 같은 이름의 시험용 변수(`RV_ARCHIVE_OPTIONS_JSON`, `RV_ARCHIVE_ALLOW_ANY_PATH`, `RV_GDRIVE_*`)를 읽는다.
- **포트.** loopback 19000~19199만 쓴다. 병렬 슬롯마다 40개(19000+40k), 미러는 19180/19181. 한 테스트에 서버가 둘이면(`archive_status`, `gdrive_panel`) 포트 선택과 HARS listen 대기를 파일 잠금 하나로 묶는다.
- **감독 프로세스.** 테스트마다 `run_browser_tests.py --supervise`(실행 시작 때 만든 사본)가 child subreaper가 되어 Node를 띄운다. Playwright는 브라우저를 새 세션(`detached`)으로 띄우므로 프로세스 그룹만으로는 브라우저가 빠진다. subreaper 아래에서는 어떻게 떨어져 나가도 감독의 자손으로 남으므로, 끝·실패·시간 초과·중단 때 `/proc`에서 자손 전체를 찾아 SIGKILL하고 거둔다. 정상 종료 뒤 3초 안에 남은 프로세스와 테스트가 지우지 않은 임시 항목을 `result.json`에 적는다.
- **외부 차단.** 기본(`--isolate on`)으로 감독을 `unshare -rn`(새 사용자+네트워크 이름공간, loopback만)에서 띄운다. 감독이 `ip link set lo up`을 한 뒤 테스트 전에 스스로 검사한다: 네트워크 장치가 `lo` 하나뿐이고 바깥 주소(`1.1.1.1:443`) 연결이 `ENETUNREACH`로 실패해야 한다. 아니면 그 항목은 `isolation-failed`(게이트 실패). `--isolation-check`는 같은 검사만 하고 끝낸다(CI 사전 단계). Playwright는 Chromium에 `--no-sandbox`를 스스로 붙인다. `unshare`가 거부되면 실행기는 실패한다. `--isolate off`는 다른 방법으로 바깥이 막힌 곳에서만 쓴다.

## 대상과 세는 단위

세는 단위는 **정적 단언 수**다: 파일 안의 `assert(...)`·`assert.<이름>(...)` 호출 지점 수(루프 안의 단언도 1로 센다). 49개 파일 합계 2,509개다. 원본은 시나리오마다 `PASS:` 줄을 찍지만 `maps.cjs`·`lazy_rows.cjs`·`log_export.cjs`·`storage_devices.cjs`는 변수로 찍어 정적으로 셀 수 없어 보조 지표로만 둔다. 계산은 `integration/upstream_cjs/survey_tests.py`(실행 없이 파일 본문만 읽음)와 `summary.json`의 `assertions`에 있다. `maps.cjs`의 302는 세 항목 각각에 같은 수로 적었다. 한 항목은 그 일부(main 또는 many·leak)만 실행한다.

서버 획득 방식:

| 방식 | 파일 |
|---|---|
| Python 서버(`RV_PYTHON`, werkzeug) → shim으로 HARS | archive_settings, archive_status, bookmarks, gdrive_panel, library_tabs(모킹 부분도 있음), maps, navigation, tags |
| 디스크 웹 파일 + `page.route` API 모킹(서버 없음) | 나머지 39개 |
| Node만(브라우저 없음) | upload_timeout(library.js 일부 평가), replay_format2(app.js 일부 + Python compact) |

모킹형은 HARS의 API를 부르지 않는다. HARS가 내보내는 화면 파일(보존 원본 + 버전·제목 치환)만 검사한다.

fixture와 Python 의존:

| 의존 | 파일 |
|---|---|
| `RV_TEST_VIDEO`(`tests/fixtures/black.mp4`, 원본 CI는 `/tmp/roadviewer-test.mp4`로 복사) | archive_status, camera_info, fullscreen_landscape, lane_marking, maps, navigation, replay_coverage, replay_end_seek, replay_layout, replay_play_request, replay_ready, replay_rendering, telemetry, video_overlay, video_sources, whole_video |
| `tests/fake_drive.py`(모의 Google Drive, 서버 프로그램 안) | archive_settings, gdrive_panel |
| `tests/overlay_geometry_fixture.py`(보존 Python `overlay.py`·`compact.py`로 fixture 생성) | lane_marking, replay_gzip |
| Python `compact.py` 직접 import | replay_format2 |
| 데이터 폴더에 Python 형식 기록 직접 쓰기 | archive_settings, archive_status, bookmarks, gdrive_panel, maps, navigation, tags |
| 데이터 폴더 `meta.json` 읽어 서버가 쓴 값 확인 | bookmarks, gdrive_panel, tags |
| `tests/fixtures/ui-review-colour.mp4`(없으면 PyAV로 생성) | replay_glass_capture |
| `roadviewer/config.yaml` | replay_downloads |
| `git show HEAD:...`(`RV_AUDIT_BASELINE`일 때만) | replay_rendering |

## 분류표

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

## 대체 목록

| 대체 | 이유 | 적용 항목(실행 증거) |
|---|---|---|
| Python 서버 → HARS(shim), HTTP/1.0 연결 닫기, Ingress 접두사 제거 | 테스트의 werkzeug 서버와 같은 연결 조건(이전 `maps.cjs` 조사) | Python 서버 방식 8개 파일 전부 |
| 폴더 목록이 바뀌면 다음 요청 전에 HARS 재기동 | Python은 요청마다 데이터 폴더를 다시 읽고 HARS는 기동 때 한 번 읽는다 | `maps[map_parts=many,leak]`(재기동 1회). 다른 항목은 `hars.log`에 `restart` 없음 |
| `meta.json`에 없는 `id`를 폴더 이름으로 채움 | HARS는 기동 때 모든 기록의 `id`가 폴더 이름과 같은지 검사하고 다르면 기동하지 않는다 | `navigation`(2개). 다른 항목은 `filled id` 없음 |
| `roadviewer/config.yaml` = HARS `roadviewer_hars/config.yaml` | `reference/python`에는 `config.yaml.reference`만 있다. 페이지 버전 줄이 HARS 버전이므로 같은 출처를 쓴다 | `replay_downloads` |
| 보존 Python 코드로 fixture 생성 | 테스트가 `python3 tests/overlay_geometry_fixture.py`를 부른다. 서버가 아니라 입력 데이터 생성이다 | `lane_marking`, `replay_gzip` |

## C · D · F 상세

**C `library_tabs` — 좁은 화면에서 워드마크가 사라짐.**
- 재현: `run_browser_tests.py --only library_tabs.cjs`. 183행 `wordmark visible at 320 with the upload pill`에서 실패한다(폭 320px, 업로드 표시가 보일 때 `.lib-appbar h1` 폭이 60px 이하).
- 기대·실제: Python은 같은 테스트를 통과한다(`--python-baseline`, 1/1). HARS는 1/1 실패(직렬 전체 실행 2회 모두).
- 원인 근거: `src/http.rs` `page_html`의 `html.replace("Road Viewer", "Road Viewer HARS")`가 `<h1>Road Viewer</h1>`도 바꾼다. 진단 실행에서 Python 서버 + 보존 원본 `library.html`의 `h1`만 `Road Viewer HARS`로 바꾼 임시 사본으로 돌리면 Python에서도 같은 183행에서 실패했다. 다른 차이는 없었다.
- 183행 뒤의 단언: 진단 실행에서 HARS 미러의 `h1`만 `Road Viewer`로 되돌리면 HARS에서 파일 전체가 통과했다(`PASS: library tabs ...`, 서버 부분 포함). 이 결함 하나만 있다.
- **고침(2단계, `af00194`).** `page_html`의 넓은 치환을 대상을 명시한 두 치환(`<title>Road Viewer ` → `<title>Road Viewer HARS `, `>Road Viewer <span class="app-version">` → `>Road Viewer HARS <span class="app-version">`)으로 바꿨다. 원본 CSS·웹 파일은 그대로다. 이전 치환이 닿던 곳과 판단:

| 페이지(내보내는 경로) | 위치 | 이전 | 지금 | 이유 |
|---|---|---|---|---|
| `shell.html`(`/`, `/view/<id>/`) | `<title>` | HARS | HARS | 레이아웃 영향 없음, HARS 표시 |
| `library.html`(`/?fragment=1`) | `<title>` | HARS | HARS | 같음 |
| `library.html` | 워드마크 `<h1>Road Viewer</h1>` | HARS | **원본** | 결함. 320 px 앱바에 맞춘 글자 |
| `library.html` | 저장공간 미터 `title` 툴팁 | HARS | 원본 | 설명 문구. `library.js`가 실행 중 원본 문구로 다시 쓴다 |
| `library.html` | 설정 탭 버전 줄 `Road Viewer <span class="app-version">` | HARS | HARS | 버전 표시(`v2.0.0-alpha.4`) 옆 |
| `library.html` | 장치 연결 안내 `장치의 Road Viewer 연결 화면` | HARS | 원본 | 장치(openpilot) 쪽 화면 이름. `storage-devices.js`도 원본 문구로 다시 쓴다 |
| `index.html`(`/view/<id>/?fragment=1`) | `<title>` | HARS | HARS | 레이아웃 영향 없음 |
| `index.html` | ⋯ 메뉴 버전 줄 | HARS | HARS | 버전 표시 옆 |

`device-test.html`과 `/assets/*`는 `page_html`을 거치지 않아 원래부터 원본 그대로다. 재현 테스트 `tests/page_version.rs`(`pages_name_the_hars_version_only`)가 네 경로 모두에서 제목이 `Road Viewer HARS · Home Assistant`, `Road Viewer HARS` 등장 횟수(셸 1, 조각 2), 조각의 버전 줄, `/?fragment=1`의 `<h1>Road Viewer</h1>`를 고정한다. 원본 갱신으로 대상 문구가 사라지면 치환이 일어나지 않아 이 테스트가 실패한다. 수정 전 코드에서 `/?fragment=1: the wordmark keeps the preserved letters`로 실패했고, 수정 뒤 통과한다. `library_tabs.cjs`는 수정 뒤 표준 구성으로 통과한다(최종 직렬 5회 5/5).

**D `replay_layout` — 프리릴리스 버전 표기.** 214행이 ⋯ 메뉴의 버전 줄을 `^v[0-9]+[.][0-9]+[.][0-9]+$`로 검사한다. HARS는 의도대로 자기 버전 `v2.0.0-alpha.4`를 내보낸다(`page_html`의 `>v{upstream}<` 치환). Python에서는 `v1.3.18`이라 통과한다. 원본 단언을 약화하지 않고 해소하는 길은 정식 버전(`2.0.0`)에서 다시 돌리는 것이다. 진단 실행에서 미러 `index.html`의 버전 줄만 `v2.0.0`으로 바꾸면 HARS에서 파일 전체가 통과했다. 214행 외의 차이는 없다. 2단계에서 이 둘을 게이트로 만들었다([게이트](#게이트)): 엄격한 예상 실패 + `replay_layout[release_version]`.

**F `replay_format2` — Python 내부 모듈.** `from compact import compact_data,expand_frames`로 Python의 재생 형식 2 압축·확장 결과를 만들고 app.js의 `expandFlat`과 대조한다. HARS에는 이 Python 모듈이 없다(`ModuleNotFoundError: No module named 'compact'`). 서버도 브라우저도 쓰지 않는다. HARS의 Rust 변환기 출력과 app.js를 대조하는 검사는 이 파일로는 할 수 없다. 2단계에서 제외로 두고 대체 검증과 공백을 [게이트](#게이트)에 적었다.

E는 없다. HARS 첫 전체 실행에서 실패한 6개(navigation, replay_layout, replay_format2, library_tabs, gdrive_panel, maps many·leak)를 Python 서버로 돌려 6/6 통과했다. 그중 3개는 실행기 결함(아래)과 `id` 대체를 고친 뒤 HARS에서도 통과했다.

**병렬·부하에서의 간헐 실패(원본 테스트 쪽).** `video_overlay` 77행(`closing hands focus to nothing`, Escape로 대화상자를 닫은 직후 포커스가 `body`인지)은 직렬 6/6 통과, 4병렬 3회 중 2회 실패했다. 서버를 쓰지 않는 모킹형이다. 바쁜 루프 14개(16 vCPU)를 걸고 직렬로 돌리면 HARS 미러에서 2/8, Python 원본 웹 파일(`--python-baseline`)에서 3/8로 같은 행에서 실패했다. 2단계에서 원인을 밝혔다: [video_overlay 77행](#video_overlay-77행)(원본 테스트의 대기 조건 결함, 직렬에서도 간헐). 분류는 A로 두고, 77행 서명에만 한정한 재실행과 HARS 검사로 처리한다(같은 절).

## 실행기에서 찾아 고친 결함

| 증상 | 원인 | 조치 |
|---|---|---|
| `maps[many,leak]` 754행 `#logView` 시간 초과. HARS 로그에 재기동 뒤 `GET /` 한 건만 있음 | shim이 HARS에 `PR_SET_PDEATHSIG`를 걸었다. 이 신호는 HARS를 만든 **스레드**가 끝날 때 오는데 재기동은 요청 스레드에서 일어나므로, 재기동된 HARS가 첫 응답 직후 죽었다 | PDEATHSIG 제거. 남은 HARS는 감독(subreaper)이 끝낸다 |
| `gdrive_panel` 210행 `route.fulfill: Route is already handled!`(HARS 1/1 실패, Python 1/1 통과) | 프록시가 `send_response`로 Date·Server를 덧붙여 Date 헤더가 두 번 갔다. 이 테스트는 Date 헤더로 서버 시계를 읽는다 | `send_response_only`로 HARS의 헤더만 전달. 이후 HARS 5/5, Python 5/5 |
| 실행 도중 실행기를 고치자 남은 항목이 모두 `supervisor-failed` | 감독이 매 테스트마다 실행기 파일을 다시 읽었다 | 실행 시작 때 실행기 사본을 임시 폴더에 두고 감독은 사본으로 띄운다 |

## 자원 정리 확인

실제로 일으켜 확인했다. "남은 프로세스"는 HARS 바이너리, `chrome-headless-shell`, 원본 테스트 경로의 `node`, `hars_as_python`, 감독을 `ps`로 찾은 결과다. 다른 작업자의 프로세스는 건드리지 않는다(이름으로 찾기만 하고 끝내는 것은 감독의 자손뿐).

| 경우 | 방법 | 감독이 끝낸 프로세스 | 끝난 뒤 남은 프로세스 | 작업 폴더·임시 폴더 | `/tmp` 새 항목 |
|---|---|---|---|---|---|
| 시간 초과 | `maps[dark,main]`, `--timeout 12` | 9개(node, shim python3, Chromium 6, HARS) | 0 | 삭제됨 | 0 |
| 실패 | `library_tabs`(C) | 0(테스트가 스스로 정리) | 0 | 삭제됨 | 0 |
| Ctrl-C | `tags` 실행 9초 뒤 실행기에 SIGINT | 8개 | 0 | 삭제됨, 실행기 종료 코드 130, 남은 항목은 시작 안 함 | 0 |
| 정상 | 전체 직렬 2회 + 5회 반복 + 4병렬 3회(495회 실행) | 각 `result.json`의 `leftover_processes` 모두 빈 목록 | 0 | 삭제됨 | 0 |

실행 중 `ps -o pid,ppid,pgid,sid`로 보면 Chromium은 Node와 다른 세션(Playwright `detached`)이다. 그래서 프로세스 그룹 종료만으로는 브라우저가 남을 수 있고, subreaper로 자손 전체를 잡는다. 정상 종료에서 테스트가 지우지 않은 임시 항목은 `replay_glass_capture`의 스크린샷 폴더(`roadviewer-glass-shots`, 테스트가 의도한 출력) 하나였고 작업 폴더와 함께 지워진다.

실행기 자체가 SIGKILL되면 감독들은 각자의 시간 상한에서 스스로 정리하지만, 그 사이 남는다. 이 경우는 확인하지 않았다.

## 시간·용량

측정 환경: WSL2 Ubuntu 24.04, 16 vCPU, 31 GiB, 시작 부하 평균 0.6~1.9(다른 작업자가 같이 돌 수 있음). 각 1회 측정이라 범위가 아니라 단일 값이다.

| 항목 | 값 |
|---|---|
| 직렬 전체 1회(51항목, 미러 포함) | 573초. 항목 합 약 560초, 가장 긴 것 `maps` main 61~64초 ×2, many·leak 46초 |
| 4병렬 전체 | 164초, 161초(2회 반복 323초). `video_overlay` 간헐 실패(위) 때문에 그대로는 안전하지 않다 |
| HARS debug 대 release | 서버형 10항목 항목별 차이 1초 이내(예: maps dark 63.8 대 63.4초), 결과 같음. 모킹형 39개는 미러 한 번 외에 HARS를 쓰지 않는다 |
| 빌드(새 target 폴더) | release 59초, debug 42초(이 순서로 이어서, 같은 캐시). 실행 파일 release 27 MB, debug 258 MB |
| 브라우저 | `chromium_headless_shell-1248`만 필요(273 MB 설치, Chromium 156.0.8078.4). 이것만 둔 `PLAYWRIGHT_BROWSERS_PATH`로 `segment_peek`, `replay_glass_capture` 통과. `chromium-1248`(403 MB)·`ffmpeg-1013`은 쓰지 않는다 |
| Node 모듈 | 19 MB(playwright 1.64.1 + playwright-core) |

빌드는 **debug**를 권한다. 테스트 시간이 같고 결과가 같으며 빌드가 짧다. HARS는 어느 테스트에서도 병목이 아니었다(서버형 항목 시간의 대부분은 브라우저 대기다).

## CI 구성

`.github/workflows/ci.yml`. 기존 `Rust checks`·`Container (amd64)`·`Container (aarch64)`와 병렬로 시작하고 `HARS required`가 `Browser checks`의 `success`를 요구한다.

| 작업 | 하는 일 |
|---|---|
| `Browser build` | `Rust checks`와 같은 캐시 키·경로(`hars-rust-v2-…`)를 `actions/cache/restore`로 **복원만** 하고(저장하지 않으므로 캐시 항목은 하나), 같은 환경변수(`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=line-tables-only`)로 `cargo build --locked --bin roadviewer-hars` 한 번. 실행 파일을 아티팩트 `hars-debug-bin`(1일)으로 넘긴다 |
| `Browser shard 1~5` | matrix, 묶음마다 직렬. Node `22.13.1`(setup-node, npm 캐시), `npm ci`(Playwright 1.64.1, `package-lock.json`), `~/.cache/ms-playwright` 캐시(키: OS·아키텍처·`package-lock.json` 해시) + `playwright install --only-shell chromium`(chromium-headless-shell 1248 = Chromium 156.0.8078.4만). 공유 라이브러리는 설치하지 않고 `ldd`로 빠진 것이 없는지만 본다. `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` 뒤 `unshare -rn` 안에서 `--isolation-check`가 통과해야 진행. 실행기 종료 코드를 `exit-code`에 적는다 |
| `Browser checks` | 묶음 요약을 모두 받아 `--aggregate`로 다시 판정. 필수 검사 이름은 이것 하나다 |

묶음: `1` g7 g6, `2` g8 g5, `3` g3 g9, `4` g4 g2, `5` g1. 묶음 안 테스트 시간 합(직렬 5회 중앙값 합, 16 vCPU): 131, 141, 127, 127, 61초. CI 묶음 구성 3회의 묶음별 실행기 전체 시간: 133~135, 144~145, 129~131, 132~137, 62~65초.

| 근거 | 값 |
|---|---|
| 기존 최장 작업 `Rust checks`(GitHub, 최근 3회) | 3분 37초 ~ 3분 57초 |
| 같은 조건 debug 빌드(로컬 16 vCPU, 새 target) | 의존성 포함 38.6초, 의존성 캐시 뒤 크레이트만 22.4초. 실행 파일 99.5 MB(gzip 22.5 MB), target 957 MB |
| 묶음 분할 수 | 3묶음이면 묶음당 약 190초, 9묶음(원본처럼 그룹마다)이면 최장 71초지만 작업마다 준비 시간(체크아웃, npm, 브라우저 캐시, 바이너리 받기: 추정 1~1.5분)이 9번 든다. 5묶음은 최장 약 145초 |
| 예상 총 시간(추정, 러너 미측정) | `Browser build` 약 2~3.5분(캐시 복원 + 4 vCPU 빌드) + 묶음 약 3.5~4분 = 6~7.5분. `Rust checks`보다 2~3.5분 길다. 빌드를 묶음마다 하면 기다림은 없어지지만 빌드가 5번 중복되고, `Rust checks`가 만든 실행 파일을 쓰면 `Rust checks` 전체가 끝날 때까지 기다린다(약 4분 + 묶음) |

아티팩트: 묶음마다 항상 `browser-summary-N`, 실패하면 `browser-artifacts-N`(7일), 집계 `browser-gate`(7일). 워크플로 문법은 `actionlint` 1.7.12로 통과.

외부 차단은 `unshare -rn` 하나로 정했다. Ubuntu 24.04의 AppArmor가 비특권 사용자 이름공간을 제한하면 이름공간 안에서 `lo`를 올릴 권한이 없으므로 sysctl로 그 제한을 이 작업에서만 끈다. 러너에서 돌려 보지 않았다. 안 되면 대안은 `docker run --network none`으로 실행기 전체를 컨테이너 안에서 돌리는 것이다(같은 이미지에 Node·Chromium 의존 라이브러리 필요). Playwright 라우팅은 테스트 코드를 고쳐야 하고, 프록시 환경변수는 Chromium이 따른다는 보장이 없어 쓰지 않는다.

## 검증

2026-11-17, WSL2 Ubuntu 24.04, 16 vCPU, 부하 평균 1.2~3.0(다른 작업자가 같이 돌 수 있음). HARS debug 빌드, Node 22.13.1, Playwright 1.64.1, chromium-headless-shell 1248.

| 실행 | 결과 |
|---|---|
| 직렬 전체 5회(`--repeat 5`) | 게이트 통과. 통과 245, 예상 실패 5(모두 214행), 제외 5, `replay_layout[release_version]` 5/5, 예상 밖 실패 0. 2,994초(반복당 항목 합 586~589초). 남은 프로세스 0, 모든 항목 `loopback only; outside address unreachable` |
| 사전 로드 없는 전체 1회(`--no-failure-shots`) | 게이트 통과, 598초. 항목별 판정이 5회와 같음 |
| CI 묶음 구성 3회(5묶음 각각 직렬, `--aggregate`) | 3/3 게이트 통과(매회 49 + 1 + 1 + 1) |
| `video_overlay` | 위 실행 9회 모두 통과. 따로 30회 반복은 [video_overlay 77행](#video_overlay-77행) |

게이트 우회 점검(각 1회, 모두 게이트가 실패해야 정상):

| 일으킨 것 | 방법 | 결과 |
|---|---|---|
| 단언 하나가 실패하는 화면 | `--web`에 `library.html`의 워드마크만 `Road Viewer HARS`로 바꾼 사본 | 종료 1, `library_tabs fail`(183행). 수집용 재실행이 `collect/trace/trace-1.zip`·스크린샷을 남김 |
| HARS 실행 파일 없음 | `--hars-bin /nonexistent/...` | 종료 2, 요약에 사유 |
| 차단 실패 | `unshare`를 옵션을 버리고 그대로 실행하는 가짜로 바꿈 | 종료 1, `isolation-failed`(`lo, eth0, docker0`). 이름공간 밖 `--isolation-check` 종료 1, 안에서는 0 |
| 테스트 0개 | `--only no_such_test.cjs`, `--only replay_format2.cjs`(제외만) | 둘 다 종료 1, `no test ran` |
| 묶음 빠짐 | 5묶음 중 4개만 `--aggregate` | 종료 1, 7개 항목 누락 |
| 묶음 중복 | 묶음 5를 두 번 | 종료 1, 중복 |
| 결과 하나 삭제 | 묶음 5 요약에서 `upload_stall` 결과를 지움(요약의 gate·exit_code는 그대로) | 종료 1, 다시 계산한 판정과 요약 불일치 + 누락 |
| 종료 코드 불일치 | 묶음 3의 `exit-code`를 1로(요약은 0) | 종료 1, 불일치 |

### 알려진 원본 테스트 결함 규칙(2026-11-17)

우회 점검(각 1회). 화면 사본은 `--web`에 보존 웹 폴더 사본을 주고 한 곳만 바꿨다. 원본 테스트 파일은 그대로다.

| 일으킨 것 | 방법 | 결과 |
|---|---|---|
| (a) 77행이 아닌 줄의 실패 | `index.html`의 `#ccncBoxHelp` 문구만 바꾼 사본, `--only video_overlay.cjs` | 종료 1, `video_overlay fail`(32행, `Expected values to be strictly equal:`). `attempt-2/` 없음(재실행 없음), hits 0 |
| (b) 77행 서명으로 3번 실패 | 실제 실행: `ripple.js`의 focusin 해제 한 줄을 지운 사본. 단위 검사: `test_gate_rules.py` 16개 | 실제 실행: 1·2·3회차 모두 77행 서명, 종료 1, `attempts 3`, hits 3. 단위 검사 16/16 통과(서명 일치→재실행, 3회 모두 서명→실패, 서명 뒤 통과→통과, 다른 줄·메시지·머리줄 2개·`TimeoutError`·시간 초과·`supervisor-failed`→재실행 없이 실패, 기록된 시도 수 초과·다른 실패 뒤 재실행·다른 항목의 `earlier_attempts`→실패) |
| (c) 결함 항목 2개 | `known_upstream_defects`에 항목을 하나 더한 `expectations.json`(`HARS_BROWSER_RUNNER_HOME`으로 지정) | 실행·`--aggregate` 둘 다 `expectations.json refused: known_upstream_defects has 2 entries, at most 1 is accepted`, 종료 1, 테스트 실행 없음 |
| (d) HARS 검사 실패 | (b)의 사본 | `video_overlay[hars_focus_check] fail`(검사 파일 40행 `closing hands focus to nothing`), 게이트 실패 사유에 따로 찍힘 |
| HARS 검사의 검출력 | `video-overlay.js`의 `close` 처리기에서 `settings.focus(...)`만 지운 사본 | HARS 검사·원본 77행 모두 통과(포커스가 그래도 body로 감). 검출 확인은 위 `ripple.js` 사본으로만 됨 |

`video_overlay` 단독 40회(`taskset -c 0-1`, 실행마다 실행기 1회, 시작·끝 부하 평균 14.6 → 18.4, 다른 작업자의 프로세스가 같이 돎): 게이트 통과 37/40. hits 분포: 0이 38회, 1이 1회, 2가 1회(합 3, 둘 다 다음 시도에서 통과). 게이트 실패 3회는 모두 77행이 아니라 **529행** `height reversal starts from the current position`(오류 1개, 재실행 없음, hits 0)이었다. HARS 검사는 40/40 통과. 529행은 높이 애니메이션을 90 ms 기다린 뒤 되돌릴 때 마지막 그린 위치와 바로 다음 위치의 차가 10 px 미만인지 보는 시간 민감 단언이다. 이 정책 전 측정(무부하·2코어, 실패 17회 모두 77행)에서는 나오지 않았다. 원인과 Python 서버에서의 재현은 확인하지 않았다. 규칙대로 재실행하지 않고 게이트 실패로 둔다.

전체 직렬(이 규칙 적용 후, debug 빌드):

| 실행 | 부하 평균(시작→끝) | 결과 |
|---|---|---|
| 전체 1회, 실행기 1번 | 17.6 → 9.4(다른 작업자의 분석 프로세스가 같이 돎) | **게이트 실패**: `conversion_controls`(Playwright 오류, 머리줄 없음), `replay_gzip` 57행 `9 redraws for 10 frames`, `maps[light][main]` 크기 고정 단언. 셋 다 수집용 재실행에서 통과. `video_overlay` 1회 통과, HARS 검사 통과, hits 0. 이전 무부하 5회에서는 없던 실패로, 높은 부하의 시간 민감 단언으로 보이나 원인 미확인 |
| CI 묶음 구성 1회(5묶음 각각 직렬 + `--aggregate`) | 9.4 → 2.1 | 게이트 통과(49 + 예상 실패 1 + 제외 1 + `release_version` 1 + HARS 검사 1). 집계 `known_upstream_defect_hits: 1`(`video_overlay` 1회차 77행, 2회차 통과) |
| 전체 1회, 실행기 1번 | 2.2 → 2.2 | 게이트 통과, 632초. `video_overlay` 1회차 77행 → 2회차 통과, hits 1 |

## 미검증과 남은 위험

- GitHub 러너에서 실행하지 않았다: `unshare`+sysctl, 공유 라이브러리(`ldd`), `actions/cache/restore` 키 공유, 시간. 위 시간은 추정이다.
- `video_overlay` 529행(`height reversal starts from the current position`)이 높은 부하에서 간헐 실패했다(부하 평균 15~18, 2코어 고정 40회 중 3회). 원인 미확인, 재실행하지 않으므로 그만큼 게이트가 실패할 수 있다.
- `video_overlay` 77행 간헐 실패(원본 테스트 결함)는 3회 모두 그 서명일 때만 게이트를 실패시킨다. 4 vCPU 러너에서의 서명 실패 비율은 모른다(`known_upstream_defect_hits`로 보인다).
- HARS 검사의 결함 검출은 `ripple.js`의 focusin 해제를 없앤 사본으로만 확인했다. `video-overlay.js`의 `close` 처리기에서 `settings.focus(...)`만 지운 사본은 HARS 검사와 원본 77행 둘 다 통과했다(그 경우에도 포커스가 body로 간다. 관찰 결과이며 Chromium 내부 경로는 확인하지 않음).
- 모킹형 39개는 HARS의 API를 부르지 않는다. HARS가 내보내는 화면 파일(보존 원본 + 버전·제목 치환)만 검사한다. HARS API는 Python 서버형 8개 파일만 닿는다.
- `replay_format2`의 공백: HARS 출력에 `app.js`의 형식 2 확장을 직접 돌리는 검사가 없다.
- 미러는 HARS가 같은 URL에서 내보내는 내용이지만, Python이 `/view/<id>/` 등에서 붙이는 응답 헤더(CSP 등)는 모킹형 테스트에 가지 않는다(테스트가 자기 헤더로 `fulfill`한다). Python에서도 같다.
- `maps[many,leak]`·`navigation`은 대체(재기동, `id` 채움)에 기대 통과한다.
- 스크린샷은 단언 실패와 Playwright 시간 초과에만 찍힌다. 다른 오류만으로 실패하면 수집용 재실행의 trace만 남는다.
- Playwright 1.64.1이 원본 1.3.18 CI의 버전과 같은지 확인하지 않았다.
