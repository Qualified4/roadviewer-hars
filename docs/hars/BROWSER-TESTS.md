# 보존 원본 브라우저 테스트를 HARS에 붙이기

<!-- toc -->
**목차**

- [요약](#요약)
- [실행](#실행)
- [실행기 구조](#실행기-구조)
- [대상과 세는 단위](#대상과-세는-단위)
- [분류표](#분류표)
- [대체 목록](#대체-목록)
- [C · D · F 상세](#c--d--f-상세)
- [실행기에서 찾아 고친 결함](#실행기에서-찾아-고친-결함)
- [자원 정리 확인](#자원-정리-확인)
- [시간·용량](#시간용량)
- [CI 구성 제안](#ci-구성-제안)
- [미검증과 남은 위험](#미검증과-남은-위험)
<!-- /toc -->

## 요약

원본 Python Road Viewer 1.3.18의 브라우저 테스트(`reference/python/tests/*.cjs`)를 원본 파일 수정 없이 HARS 실행 파일과 HARS가 내보내는 웹 파일에 붙여 돌린다. 실행기는 `integration/upstream_cjs/run_browser_tests.py`다. 2026-11-17 `5a2426f` 기준(2.0.0-alpha.4, release 빌드) 결과는 다음과 같다.

| 분류 | 뜻 | 수 |
|---|---|---|
| A | 표준 구성만으로 통과 | 43 |
| B | 테스트별 fixture·어댑터 대체로 통과 | 5 |
| C | 실패, HARS 제품 결함 | 1 (`library_tabs`) |
| D | 실패, 테스트 환경 차이 | 1 (`replay_layout`) |
| E | 원본 테스트 결함 | 0 |
| F | Python 전용 의존 | 1 (`replay_format2`) |

"51개"는 `browser-groups.json`의 항목 수다. 테스트 파일은 49개이고 `maps.cjs`가 환경변수만 다르게 세 항목으로 들어 있다. `browser-helpers.cjs`와 `run_group.cjs`는 테스트가 아니다.

## 실행

```sh
cd integration/upstream_cjs && npm ci && cd -        # playwright 1.64.1 (package-lock.json)
npx --prefix integration/upstream_cjs playwright install --only-shell chromium
cargo build --locked --release
python3 integration/upstream_cjs/run_browser_tests.py \
  --hars-bin "$CARGO_TARGET_DIR/release/roadviewer-hars" --out /tmp/rv-browser [--jobs 4] [--repeat 5] [--only maps.cjs tags]
# Python 기준 실행(E 판정용): Flask가 있는 인터프리터
python3 integration/upstream_cjs/run_browser_tests.py --python-baseline ~/venv/bin/python --out /tmp/rv-py --only gdrive_panel.cjs
```

Node 22가 PATH에 있어야 한다(`--node`로 지정 가능). 결과는 `--out` 아래 `summary.json`(항목별 결과·초·단언 수·분류·남은 프로세스·남은 임시 항목)과 `run<N>/<항목>/{test.log,hars.log,result.json}`이다. `hars.log`에는 HARS 출력과 어댑터 사건(`[shim ...] proxy`, `restart`, `filled id`)이 시간순으로 들어 있다. `--pw-debug`는 `DEBUG=pw:api`로 Playwright 호출·요청·응답 헤더를 `test.log`에 남긴다.

실패 시 스크린샷·trace는 남기지 않는다. 원본 helper에 그 기능이 없고, 넣으려면 테스트의 `browser.newContext`를 가로채야 하므로 이번 범위에서 하지 않았다. 일부 테스트는 자체 스크린샷 변수(`RV_MAP_SHOTS` 등)를 지원하지만 켜면 테스트가 다른 단계를 더 실행하므로 기본으로 켜지 않는다.

실행 언어는 Python이다. 서버 어댑터가 Python이고, 프로세스 감독에 `prctl`과 `/proc`이 필요하다.

## 실행기 구조

- **미러.** 실행 시작에 HARS를 임시 데이터 폴더(기록 1개)와 loopback 19180/19181로 한 번 띄워, 보존 웹 파일마다 HARS가 Python과 같은 URL에서 내보내는 내용을 받는다(`mirror_web.py`, 43개). `library.html`은 `/?fragment=1`, `index.html`은 `/view/<id>/?fragment=1`, 나머지는 `/assets/<이름>`이다. 그래서 HARS의 버전 치환·제목 치환이 들어간 페이지가 테스트에 간다.
- **작업 폴더(테스트마다 새로).** `roadviewer/app/web`(미러), `roadviewer/config.yaml`(HARS의 `roadviewer_hars/config.yaml`), `tests/`(보존 테스트 폴더 항목마다 심볼릭 링크), `tmp/`(`TMPDIR`). 테스트는 원본 경로(`reference/python/tests/X.cjs`)로 실행하고 작업 폴더를 현재 폴더로 준다. `roadviewer/app`에 Python 모듈이 없으므로 실수로 Python 서버가 뜰 수 없다.
- **Python 서버 자리(`hars_as_python.py` + `pyshim/`).** 테스트는 `RV_PYTHON -u -c PROGRAM`으로 서버를 띄운다. `hars_as_python.py`는 `PROGRAM`을 그대로 실행하되 `pyshim/`를 모듈 경로 맨 앞에 둔다. `sitecustomize.py`의 finder가 `server`, `werkzeug.serving`, `werkzeug.middleware.dispatcher`를 shim으로 돌린다(테스트가 `sys.path.insert(0,'roadviewer/app')`을 해도). `make_server`는 RV_DATA로 HARS를 띄우고 `/`와 `/api/hassio_ingress/test/` 둘 다에서 답하는 HTTP/1.0 프록시를 돌려준다. 그래서 `fake_drive.py`와 그 환경변수(`RV_GDRIVE_*`), 표준 입력 `grant` 명령 같은 테스트 자체의 fixture는 쓰인 그대로 돈다. HARS가 Python과 같은 이름의 시험용 변수(`RV_ARCHIVE_OPTIONS_JSON`, `RV_ARCHIVE_ALLOW_ANY_PATH`, `RV_GDRIVE_*`)를 읽는다.
- **포트.** loopback 19000~19199만 쓴다. 병렬 슬롯마다 40개(19000+40k), 미러는 19180/19181. 한 테스트에 서버가 둘이면(`archive_status`, `gdrive_panel`) 포트 선택과 HARS listen 대기를 파일 잠금 하나로 묶는다.
- **감독 프로세스.** 테스트마다 `run_browser_tests.py --supervise`(실행 시작 때 만든 사본)가 child subreaper가 되어 Node를 띄운다. Playwright는 브라우저를 새 세션(`detached`)으로 띄우므로 프로세스 그룹만으로는 브라우저가 빠진다. subreaper 아래에서는 어떻게 떨어져 나가도 감독의 자손으로 남으므로, 끝·실패·시간 초과·중단 때 `/proc`에서 자손 전체를 찾아 SIGKILL하고 거둔다. 정상 종료 뒤 3초 안에 남은 프로세스와 테스트가 지우지 않은 임시 항목을 `result.json`에 적는다.
- **외부 차단.** 기본(`--isolate on`)으로 감독을 `unshare -rn`(새 사용자+네트워크 이름공간, loopback만)에서 띄운다. 감독이 `ip link set lo up`을 한다. Playwright는 Chromium에 `--no-sandbox`를 스스로 붙인다. `unshare`가 거부되면 실행기는 실패한다. `--isolate off`는 다른 방법으로 바깥이 막힌 곳에서만 쓴다.

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

`5회`는 직렬 1회에 통과한 48개 항목을 직렬로 5번 반복한 결과다(240/240 통과, 2,756초). `4병렬`은 `--jobs 4` 전체 실행 3회의 통과 수다. `초`는 직렬 전체 실행 1회의 항목별 소요다. 숫자 원본은 `measurements/browser-tests.json`.

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
| `library_tabs` | 라이브러리 탭, 헤더, 경로 카드, 좁은 화면 앱바 | 94 | Python 서버→shim | C | - | 0/3 | 15.4 |
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
- 고칠 곳(제안, 이번 단계에서 고치지 않음): 워드마크 `h1`은 원본 글자를 유지하거나, HARS 표기를 넣더라도 320px 앱바에서 60px 넘게 보이게 한다.

**D `replay_layout` — 프리릴리스 버전 표기.** 214행이 ⋯ 메뉴의 버전 줄을 `^v[0-9]+[.][0-9]+[.][0-9]+$`로 검사한다. HARS는 의도대로 자기 버전 `v2.0.0-alpha.4`를 내보낸다(`page_html`의 `>v{upstream}<` 치환). Python에서는 `v1.3.18`이라 통과한다. 원본 단언을 약화하지 않고 해소하는 길은 정식 버전(`2.0.0`)에서 다시 돌리는 것이다. 진단 실행에서 미러 `index.html`의 버전 줄만 `v2.0.0`으로 바꾸면 HARS에서 파일 전체가 통과했다. 214행 외의 차이는 없다.

**F `replay_format2` — Python 내부 모듈.** `from compact import compact_data,expand_frames`로 Python의 재생 형식 2 압축·확장 결과를 만들고 app.js의 `expandFlat`과 대조한다. HARS에는 이 Python 모듈이 없다(`ModuleNotFoundError: No module named 'compact'`). 서버도 브라우저도 쓰지 않는다. HARS의 Rust 변환기 출력과 app.js를 대조하는 검사는 이 파일로는 할 수 없다.

E는 없다. HARS 첫 전체 실행에서 실패한 6개(navigation, replay_layout, replay_format2, library_tabs, gdrive_panel, maps many·leak)를 Python 서버로 돌려 6/6 통과했다. 그중 3개는 실행기 결함(아래)과 `id` 대체를 고친 뒤 HARS에서도 통과했다.

**병렬·부하에서의 간헐 실패(원본 테스트 쪽).** `video_overlay` 77행(`closing hands focus to nothing`, Escape로 대화상자를 닫은 직후 포커스가 `body`인지)은 직렬 6/6 통과, 4병렬 3회 중 2회 실패했다. 서버를 쓰지 않는 모킹형이다. 바쁜 루프 14개(16 vCPU)를 걸고 직렬로 돌리면 HARS 미러에서 2/8, Python 원본 웹 파일(`--python-baseline`)에서 3/8로 같은 행에서 실패했다. HARS와 무관한 원본 테스트의 부하 민감성이다. 직렬 분류는 A로 두고, 재시도로 숨기지 않는다.

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

## CI 구성 제안

- **버전 고정.** Node 22(원본 CI `actions/setup-node` `node-version: '22'`, 로컬 22.13.1). Playwright는 원본 CI가 고정하지 않았다(`npm install --no-save playwright`, 실행 시점 최신). 여기서는 1.64.1을 `integration/upstream_cjs/package.json`·`package-lock.json`으로 고정하고 `npm ci`를 쓴다. 이 버전은 이전 HARS 검사가 쓴 로컬 설치와 같다. 원본 CI가 1.3.18 시점에 실제로 받은 버전은 확인하지 않았다.
- **캐시.** `~/.cache/ms-playwright`(키: OS·아키텍처·`package-lock.json` 해시, `--only-shell chromium`), npm 캐시(`package-lock.json`), Cargo(HARS debug 빌드). 원본 CI처럼 `--with-deps`는 쓰지 않는다(호스트 러너에 Chrome 공유 라이브러리가 있다는 원본 CI의 주석, 이번에 러너에서 확인하지 않음).
- **분할.** 4병렬 대신 원본처럼 그룹 matrix(러너마다 직렬)를 권한다. `--only`로 묶음을 고를 수 있다. `browser-groups.json`의 9묶음을 그대로 쓰면 테스트 시간 합이 묶음마다 48~71초다(g1 61, g2 59, g3 69, g4 68, g5 48, g6 55, g7 71, g8 71, g9 60초, 직렬 1회 기준, 미러·설치 제외).
- **외부 차단.** 1순위 `unshare -rn`. Ubuntu 24.04는 AppArmor의 `kernel.apparmor_restrict_unprivileged_userns`로 비특권 사용자 이름공간을 막을 수 있다. GitHub 러너의 실제 값은 확인하지 않았다(2단계). 안 되면: (1) `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` 후 `unshare -rn`, (2) `docker run --network none`으로 실행기 전체를 컨테이너 안에서(같은 이미지에 Node·Chromium 의존 라이브러리 필요), (3) `sudo unshare -n` 뒤 원래 사용자로 실행. Playwright 라우팅으로 막는 방법은 테스트 코드에 손대야 하므로 쓸 수 없고, 프록시 환경변수는 테스트의 `launch()`가 `proxy`를 넘기지 않으므로 Chromium이 따른다는 보장이 없어(확인하지 않음) 차단 수단으로 쓰지 않는다.
- **게이트.** C·D·F 3개가 지금 실패하므로 `Browser checks`를 필수로 만들기 전에 C를 고치거나, 실패가 예상된 항목 목록(`classification.json`)과 결과를 대조해 "예상 밖 실패"만 실패로 보는 판정이 필요하다. 후자는 단언을 약화하지 않지만 F·D를 계속 실패로 기록한다.

## 미검증과 남은 위험

- GitHub 러너에서 실행하지 않았다(`unshare`, 공유 라이브러리, 시간).
- 모킹형 39개는 HARS의 API를 검사하지 않는다. HARS가 내보내는 화면 파일만 검사한다. HARS API는 Python 서버형 8개 파일만 닿는다.
- 미러는 HARS가 같은 URL에서 내보내는 내용이지만, Python이 `/view/<id>/` 등에서 붙이는 응답 헤더(CSP 등)는 모킹형 테스트에 가지 않는다(테스트가 자기 헤더로 `fulfill`한다). Python에서도 같다.
- `maps[many,leak]`·`navigation`은 대체(재기동, `id` 채움)에 기대 통과한다. 재기동 뒤 HARS가 기록을 다시 읽는 시간은 테스트 시간에 들어간다.
- 측정은 모두 1회(병렬 3회)다. 다른 작업자의 부하가 섞였을 수 있다.
- Playwright 1.64.1이 원본 1.3.18 CI의 버전과 같은지 확인하지 않았다.
