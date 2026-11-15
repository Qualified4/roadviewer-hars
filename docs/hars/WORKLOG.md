# 작업 재개 기록

## 현재 목표와 경계

사용자가 요청한 목표는 전체 Rust 이식·기능 동등성·독립 HA 앱·CI/CD·안전한 최종 포트 승계다. **목표는 아직 완료되지 않았다.** 현재 산출물을 완성된 Road Viewer 2.0 제품으로 취급하거나 남은 기능을 목표에서 제외하지 않는다.

기존 Python GitHub 저장소/브랜치/태그/Release/Actions/GHCR, 실제 Python 앱 설정, 운영 포트, openpilot 업로드 설정을 변경하지 않았다. 운영 전환은 별도 명시적 승인 전까지 실행하지 않는다. 신규 HARS 저장소에서만 작업한다. 원본 remote의 push URL은 `DISABLED`로 설정했다.

## 보존된 기준과 커밋

- Python 원본: `71add1a04ed505fc2cb6982052db4a9945d0808f` (1.3.16).
- openpilot ccnc-hda1: `5c2a8c7f6e14dac82678a390e408ef7a9c73eefa`.
- `b1cc7e4`: 이력·라이선스·원본 코드·실제 업로더 참고 파일 및 인벤토리 보존.
- `8b30ab7`: Rust 서버·인증/업로드·로그 읽기/텔레메트리·무손실 영상 엔진. API 예비 측정은 이 커밋의 release 빌드를 사용.
- `f67ae62`: Nginx→Rust request ID 전파 및 원본 progress JSON 형식.
- `45a53a8`: 스냅샷/대상/기존 Python 경로의 별칭·중첩을 실제 파일 쓰기 전에 검사.
- `5265050`: 독립 HA 앱/두 아키텍처 CI/검증 도구 게시. 첫 CI의 Rust 검사와 이미지 빌드는 성공했으나 TLS 기동 검사는 실패했다. `FAILURES.md`의 준비 상태 수정과 이후 최신 실행 결과를 함께 확인한다.

최종 배포 구성/검증 도구 커밋과 원격 최신 상태는 `git log`, `git status`, 해당 HEAD의 GitHub Actions에서 확인한다. 기존 성공을 새 HEAD의 성공으로 간주하지 않는다.

## 실행 환경과 검증 자료

Windows 작업 폴더 안의 소스를 WSL Ubuntu 24.04에서 빌드했다. Rust stable 1.100.0, Cargo 산출물은 `/tmp/hars-target`, 개발 비교용 Python venv는 `/tmp/hars-venv`에 있다. FFmpeg/Cap'n Proto compiler/libclang/libavformat 개발 패키지를 설치했다. 제품 컨테이너에는 Python이 없다.

원본 `client.py`를 통한 실제 HTTP 호환성 및 재페어링 없는 인증 이전, 합성 영상 검증, 실제 브라우저 업로드/검색/펼치기/고정을 실행했다. `VALIDATION.md`와 `PERFORMANCE.md`에 범위와 한계를 구분했다. UI 합성 자료는 git에서 제외한 `artifacts/smoke`에 두었다. 브라우저 TaskSpace는 종료했고, 직접 띄운 테스트 서버도 종료했다. 실제 HAOS·openpilot 기기·실주행 원본 코퍼스에는 접근하지 않았다.

## 바로 다음 구현

1. `STATUS.md`, `inventory.json`, 원본 `decoder.py`와 관련 모듈을 이어서 읽고 전체 분석/재생 변환을 이식한다. 현재 새 업로드는 의도적으로 `unconverted`로 보존한다.
2. 모델/레이더/조향/CCNC 기하·GPS·wall clock·카메라 시간축·replayV2 압축 출력과 변환 큐를 완성한다. `telemetry.rs`는 원본 함수 대조가 있지만 아직 전체 변환 파이프라인에 연결되지 않았다.
3. 기존 구간에 카메라 추가, 원격 상태별 중복 정책, ZIP/선택 내보내기/가져오기, 보호된 삭제와 오래된 데이터 정리를 이식한다.
4. Drive/NAS와 삭제 저널·복구·무결성·보충 변환을 이식한다. 독립 HARS 루트/소유권 표식 없이 기존 아카이브를 쓰거나 삭제하면 안 된다.
5. 전체 API 오류 조건/메시지·입력 경계·모든 UI 흐름을 원본과 대조한다. 현재 정상 경로의 RV1 호환성을 전체 API 호환성 완료로 확대 해석하지 않는다.
6. 실제 영상/로그·브라우저 재생/시킹·실장치 안정성·HAOS·역방향 인증 복원·TLS 인증서 자동 갱신을 검증한다. 이후 같은 기능/데이터 조건으로 전체 성능을 측정한다.

추가 CI는 변경 사항의 검증에 필요한 범위로 한정하고, 기존 실행·로그를 먼저 확인한다. `2.0.0` 정식 태그/Release를 만들지 않는다.


## 2026-11-15 alpha.2 재개 — 실제 코퍼스 검증 진행 중

목표는 전체 재생 변환, 업로드 후 자동 큐, Python 동등성, 기존 UI 재생이며 아직 alpha.2 완료/릴리스 아님. Drive/NAS 이식과 운영 전환은 이번 범위 제외. 작업 트리의 신규 src/replay.rs, track.rs, thumbnail.rs, convert.rs, jobs.rs 및 HTTP 연결은 현재 미커밋 개발 중이다.

- 사용자 지정 실제 코퍼스: `F:/davin/다운로드/WhaleDownload/roadviewer-14-segments-mp4` (14 rlog + 전방/qcamera 각14, 736950554 bytes). 원본은 읽기 전용.
- `integration/real_replay_compare.py`: 14구간 모델총수 등 `/tmp/hars-alpha2-real14/frame-comparison.json`, 전 프레임/CCNC/조향/오버레이/replayV2 차이 없음 (첫 모델 프레임 상대 시간축).
- `integration/real_conversion_compare.py`: `/tmp/hars-alpha2-real14-full/conversion-comparison.json`. 캐시 key만 제외한 전체 재생JSON/텔레메트리/summary/GPS, 14구간 모두 1e-6 초과 차이 없음. 썸네일 score 일부 ±0.01, 픽셀 동등성은 아직 미확인. Python wheel과 네이티브 FFmpeg 버전 차이 가능성이며 아직 원인 확정 아님.
- 실제 데이터로 방향지시등 선택필드 누락, ffprobe pts_time 6자리 반올림 사용, telemetry_origin 반올림 순서, Python decimal rounding 차이를 찾아 수정했다. Python 원본은 수정하지 않았다.
- `integration/real_upload_smoke.py`: `/tmp/hars-alpha2-upload1/result.json`. 첫 실제구간3파일 업로드 → queued/processing/ready, 업로드 수신 중 자동변환 대기 통과,751모델/전방/qcamera,원본해시 불변. 목록API22회max5.1275ms(단일 소규모 측정).
- 실브라우저 ego TaskSpace23: 0번구간 전방 HEVC→브라우저 blob 및 qcamera 모두 readyState4, 재생/일시정지/30→45초 탐색/카메라 전환/실제 그래프·오버레이 확인. GPS API는1segment58points이나 지도 스크린샷이 빈 영역이어서 지도 렌더링 성공으로 보고하지 말 것. 스페이스는 종료했다. private screenshot `C:/Users/dabin/ego/hars-alpha2-real14/map.png`.
- 독립 브라우저용 서버 실행 중(exec session84350): root `/tmp/hars-alpha2-upload1/data`, UI127.0.0.1:18199, device127.0.0.1:18198. 원래 운영 HAOS 아님. 재시작/종료 전 진행 중 변환 없는지 확인할 것.
- 최신 로컬 Clippy 통과 `/tmp/hars-alpha2-clippy8.log`. 직전 cargo test 전체통과 `/tmp/hars-alpha2-tests2.log` (HTTP map 변경 이전; 최종 재검증 필요). 현재 releasebinary `/tmp/hars-target/release/roadviewer-hars`는최신 map API까지 포함(빌드로그 release6).
- 실제 HA MCP 도구는 세션에 노출되지 않았지만 `.codex/config.toml`의 등록 `roadviewer-mcp` stdio 경로로 MCP SDK 연결 성공. 인증env값 출력하지 않는다. MCP756 ready,1.3.17,Drive보관. 실제47번구간6파일92.2MB를 별도 `F:/davin/Documents/roadviewer-downloads/hars-alpha2-validation`에 다운로드했고manifest SHA256확인. 보조fixture이며 이제 사용자 로컬 코퍼스가 우선.

### 최신 사용자 추가 코퍼스

`F:/davin/다운로드/WhaleDownload/roadviewer-47-segments-mp4`: 별도주행00000468--c7459c0c5a,47구간(3..49), rlog47/qcamera47/front44/wide26,164파일3315376538bytes. 3/4/5는q만,20..37은wide없음; 누락카메라는코퍼스구성으로분류하고변환실패로추정하지 않는다. 첫 와이드 대표6번구간을 비교중(exec45175); 결과 `/tmp/hars-alpha2-full47-first.log`, outputs `/tmp/hars-alpha2-real47-full`, Pythonoracle `/tmp/hars-alpha2-real47-oracle`. 비교스크립트 `--prepare-python --offset --limit` 추가, wide media mapping 포함. 첫 결과 확인후 나머지46개로 확대(기존14개는 반복안함). 보고JSON 좌표없음; raw replay/GPS는 Git에 넣지 않음.

### 남은 구현/검증 주의점

큐는 nice10/ionice2:7 자식, 동시1..4,업로드대기,수동/취소/재시작큐복구,staging/journal publication까지 연결. 아직 작업 중이며 오류/취소/재시작/경합 주입테스트 필요. keep_original_video=false 저장은가능하나현재원본은항상보존하므로정책불일치 해결필요(임시비활성으로숨기지말것). auto_convert 기본값은아직false,새설정정책결정필요. 성능단계CPU/RSS 및동등조건전환성능미측정. 이미지native지원위해software-scaling feature와libswscale-dev를Docker/CI추가(WSLlocalapt설치됨). 기존CI job이름은변경안함. 아직신규CI/푸시/태그/Release없고버전은alpha.1유지. STATUS/VALIDATION/PERFORMANCE최종갱신전이며이WORKLOG와ALPHA2-PIPELINE을우선참고.


## alpha.2 追加 검증·변경 (위 진행 메모 갱신)

- 47구간 전체 완료: 모델56399,와이드26구간. 기존14구간과합계61구간72749모델. cache key를제외한 replay/telemetry/track/summary 차이0. `/tmp/hars-alpha2-real47-full/conversion-comparison.json`,안전한보고서는docs/hars/measurements에복사됨.
- 와이드6번구간4파일 실제업로드자동변환PASS `/tmp/hars-alpha2-upload-wide/result.json`1201모델·3카메라,upload waitPASS,API23샘플max1.0114ms,원본SHA불변.
- egoTaskSpace24에서와이드실재생15→16초,일시정지,45초시킹44.999989ready4오류없음. 지도타일/GPS경로/차량표식/오버레이스크린샷직접확인성공.초기0번구간빈지도는비동기로딩중찍은것으로추정(원인확정아님). 24종료. private screenshot wide-map.png.
- 독립서버84350은진행작업없음을확인후종료. 현재browser용서버exec19369 root `/tmp/hars-alpha2-upload-wide/data`,18199/18198실행중,production아님.
- 2.0.0-alpha.2로Cargo/config/Docker/README버전맞춤. 새설치자동변환기본true,저장된partialprocessing설정은default와merge/validation. keep_original_video=false일때검증완료후MP4를recordroot로이동하고raw만제거하는publication분기추가(실제데이터는MP4입력으로raw삭제분기미검증). cancel요청generation지속하여publish직전/재시작race보호. ENOSPC로활성meta저장실패시active entry정리.
- syntheticCI fullpipelineoracle `integration/pipeline_contract.py`: 원본Python+Rust,120모델/TS+rawHEVC전방+와이드,전체JSON차이0,무손실remuxPASS(로컬~3.4초). CI Rustjob의기존venv재사용step으로추가.
- 로컬alpha.2fmt/clippy/전체Rust32테스트/Python synthetic oracle/실제ccnc client+auth migration PASS. 원본보존검사는WSLgit core.autocrlf unset과Windows checkout CRLF때문에false mismatch발생. WindowsnativeGit으로기존verify_reference.py재실행286파일PASS,check_release.pyalpha.2PASS. source파일gitdiff없음. 검사약화/원본수정없음.
- 성능47중앙Python3.733s/Rust4.782s;Rust더느림(보고서doc반영). 대표실제6번구간프로파일추가 `integration/profile_conversion.py`: `/tmp/hars-alpha2-profile-wide/profile.json` Python3.78084s/CPU3.30738s/meanRSS233443KiB/peak326808KiB;Rust4.96637s/CPU4.45827s/meanRSS319542KiB/peak434112KiB. RSS프로세스트리합계,20ms관측. per-stageCPU는짧은자식누락가능한하한이며단계분할이다름.
- 원인발견: json!(frames)가전체Value배열복제+원시model/radarstream과telemetry트리를scope끝까지보관. `convert.rs` 수정해frames를Value::Array로이동,frames생성후남은단계에필요없는streams제거,telemetry쓰기후drop,track/starts/counts추출후decoded drop. 수치개선은아직미보고. exec22005현재fmt/clippy/releasebuild중;완료후같은구간Rust만profile재측정(--baseline /tmp/hars-alpha2-profile-wide/profile.json)하고대표전체JSON동등성을확인. Python기준반복하지않음.
- git아직미커밋/main8c12894. GitHub최신기존CI43702875850green,진행CI없음,rulesets조회빈배열. 사용자원래커밋/푸시요청되어phasecommit가능. 아직푸시/태그/Release없음. STATUS/VALIDATION/PERFORMANCE/DEVELOPMENT갱신했으나최종CI/프로파일결과추가필요.

- 복사제거후대표6번Rust만재측정완료(기존Python재사용):4.526059s/CPU4.031083s/meanRSS280495.8KiB/peak348612KiB. Python보다여전히느리고무거움. cache key제외전체4출력다시차이0. before/after메트릭안전JSON을docs/hars/measurements에보존.
- 이제최종소스커밋과CI를진행한다. 원본소스검사WindowsGit286PASS이며WSLgit설정차이에따른CRLFfalse mismatch만발생했다.

- 커밋095a922(변환/큐),f4b124a(CI/실제코퍼스증거)를main에푸시. f4b124a의CI43708871726 4개job 모두success.
- CI후최종UI점검에서다운로드목록메타/단일영상alias/preparedMP4 original경로누락발견. src/jobs.rs/http.rs+tests/contract.rs 보완. 기존ready레코드는시작시download메타보충(재변환안함). 지도private no-cache추가. 범위Clippy·contract13개/recovery4개통과. 최신src조금수정후재검증은이미끝남(다운로드prefix fallback+cachepolicy포함).
- 독립서버exec19369종료전진행작업없음확인,현재exec9050에서최신debugbinary로root `/tmp/hars-alpha2-upload-wide/data`18199/18198실행중. 테스트끝나면같은root/pid확인후종료할것. 원래HAOS아님.
- 실제3카메라 MP4다운로드 SHA256입력과일치,Range206바이트일치,원본TS/HEVC역변환API완료(nativeNAL검사내장). docs측정download-real-wide JSON에안전한수치보존.
- 아직추가수정미커밋,다음작업은해당fix커밋/푸시→최신커밋CI확인. 이전f4CIgreen을새fix커밋CIgreen으로보고하면안됨. 전체61구간비교·CPU측정은반복안함. release노트준비됐으나tag/GHCR/Release게시없음,운영변경없음.
