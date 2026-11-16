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


## 2026-11-15 변환 성능 최적화 (perf/conversion-pipeline)

- 목표: 프로파일에 근거한 실제 속도·메모리 개선, 전체 출력 1e-6·영상 무손실·보안 유지. Python/openpilot/HAOS/운영 업로드 설정은 변경하지 않는다. 사용자 요청에 PR·커밋·병합이 포함되어 있다. 에이전트 위임은 하지 않았다.
- 기준은 main `98ca2997be1e7c85895e60018871511b77bad5a0`, 기존 CI43709708605 전체 성공. 환경 재부팅으로 이전 /tmp 산출물이 없어 불변 기준 release를 다시 빌드했다. 영구 실험 경로 `/home/dabin/hars-performance`; baseline SHA256 `4a9197e22300ce9b49d33f181601eabdd3cf1a76ee5ee59f1099ad3416b6d375`.
- WSL Ubuntu24.04/16 logical CPU/Rust1.100 stable. 입력 NTFS/출력 ext4. Rust system FFmpeg6.1.1, 원본 Python PyAV16.1 wheel의 FFmpeg8 계열로 라이브러리 버전은 다르다. venv는 원본 requirements로 준비했다.
- CPU perf 914표본/199Hz/lost0: memset19.80%(GzEncoder 아래), miniz compress_inner13.24%, float format2.95%, malloc2.63%. DHAT 기준 누적970198580 bytes/8480770할당, live heap peak232062491 bytes. NAL 복사77.3MB, compact 임시 배열28.6MB가 상위 할당이다. 원시 perf/DHAT/실제 출력에는 개인 데이터가 있을 수 있어 Git 밖에 둔다.
- 유지: Serde→64KiB BufWriter→Gzip9(기존 finish/fsync 유지), DynamicList/telemetry 소유권 이동 및 열 용량 예약, NAL slice borrow, compact 직접 비교, 동일 작업의 ffprobe 결과 재사용, 영상 검사 후 frame geometry 구성. 모든 프레임·시각·헤더·비트스트림 검증/제한은 유지했다.
- 제외: 렌더링 배열 owner 이동은 RSS 회귀, telemetry 선계산은 CPU/시간 회귀, native zlib는 5회 중앙값 시간2.6% 회귀로 되돌렸다. 기존 테스트의 assertion은 삭제하지 않았다. codegen-units=1은 시간0.2% 차이로 채택하지 않았고 LTO 해제는 CPU가 늘었다. thin LTO/default CGU/generic CPU를 유지하며 PGO는 적용하지 않았다.
- 각 실험: 구현별 warmup1+교차5회, 프로파일 OFF, 모든 출력 Python1e-6 비교, Rust 전후 thumbnail JSON/JPEG 바이트 동일, 입력 SHA 불변. baseline/buffered/allocation/candidate/reserved/folded/ordered/zlib/final/cgu1/nolto 불변 실행 파일과 각 bench-*/benchmark.json 보존.
- 최종 대표 wide6: Python3.898547s/CPU3.333185/meanRSS237279/peak327404KiB; 기존Rust4.750616s/CPU4.153492/mean281378/peak348944; 최종Rust3.494705s/CPU2.986633/mean234555/peak341944. 시간26.4%, 평균RSS16.6%, Peak2.0% 감소. Python 대비 Peak는 여전히 높다.
- 대표 14코퍼스0: Python2.908867s/CPU2.583351/mean195773/peak274132; 기존Rust3.429724s/CPU3.090948/mean239429/peak310216; 최종2.413979s/CPU2.117385/mean206624/peak300388. 서로 다른 실험의 절대시간을 직접 개선율로 사용하지 않는다.
- 최종 DHAT: 누적671629886 bytes/7424303할당, live heap peak232067694 bytes. 누적할당은30.8% 감소하지만 live heap peak는 거의 같다. opt-in Scope 진단은 중첩/계측 overhead/누적RSS이므로 정상 벤치와 분리한다. 최종 gzip JSON 저장 wall0.553s(기준1.523), probe4회(기준8), signature3회 유지. 총 native 프로세스12→8.
- core 최적화 커밋 `3a645e9`. 로컬 fmt/clippy/전체35테스트 성공, 원본286파일·release gate 성공. 전체61 oracle/합성 파이프라인/ccnc/실제 업로드를 순차 실행 중인 세션92335. 14구간 완료·차이0, 47구간 진행 중. 추가 raw는 실제 MP4를 무손실 추출한 파생 입력이며 센서 원본 raw로 표현하지 않는다.
- 전체61구간/72749모델 프레임 차이0, 합성3cam raw oracle, 원본 ccnc lifecycle/인증 이전, 실제4파일 업로드/자동 변환 모두 통과했다. 파생 raw 교차5회는 기존Rust4.959281→최종3.515262초, Python3.430890초로 Rust가 여전히 약2.5% 느리다. 원본센서 raw가 아닌 실제 MP4 무손실 추출 입력이다.
- 최종 perf647표본/lost0: miniz14.99%, memset1%미만. strace 자식12→8/openat2841→1921/read13202→11775/write9684→8474, fsync5유지. 동시 스레드 wait 합계를 전체 경과로 보지 않는다.
- 전용 loopback 재변환+16MiB 업로드: API165표본 p95 0.820ms/p99 0.988ms, 조각64개 실패0, processing/upload 겹침43표본. ego Space13에서 실제 전방/와이드/qcamera·재생/일시정지/15·45초 시킹/지도/overlay/그래프를 확인하고 Space를 닫았다. 모든 로그 ready를 확인해 소유한 테스트 서버만 종료했다.
- PRIVATE raw/perf/DHAT/영상/좌표는 `/home/dabin/hars-performance` 및 사용자 ego 임시 폴더에만 있다. 공개할 숫자 집계/비교 결과는 docs/hars/measurements에 저장했다. STATUS/VALIDATION/PERFORMANCE/DEVELOPMENT를 갱신했다.
- 다음: 도구·문서 커밋→PR1개 생성→amd64/aarch64/필수 CI 확인→병합 후 최신 main CI 확인. 실행 링크·최종 커밋·완료 여부는 PR 및 최종 사용자 보고에 기록한다. 운영 전환/릴리스/배포는 하지 않는다.

## 2026-11-15 원본 형식 13구간 검증 (perf/original13-remux)

- 최신 main/origin main `0dd87af` 동기화·clean 확인 후 분기. 원본 폴더 읽기 전용, 결과 `/home/dabin/hars-original13`, 기존 venv/target/perf/strace 재사용. 원본검증 요구라 별도 기준 PR/CI 없이 기존0dd코드 release `/home/dabin/hars-performance/final`을 실행권한 보존해 baseline 복사. CORE는 같은 컨텍스트에서 이미 읽어 적용 중, Ponytail 사용. 위임 없음. 예상2–4h 알림.
- 파일39개:13rlog+13qcamera.ts+13fcamera.hevc. ecamera/메타 없음. 모두 SHA256 private inventory로 보존.26영상 디코딩 오류0, 각각1200프레임. fcam HEVC1344×760/1200packet/60keys/B0/원시PTS없음. q H264526×330/1200packet/80keys/90kHz, 원래 TS offset/jitter 존재. sensor 최초원본/컨테이너 동일성은 가정하지 않는다.
- baseline-full13 Python비교 완료:3·9번 qcamera terminal sample duration에서 약0.2ms 차이로 duration파생필드가1e-6초과. 나머지11구간 차이0. 처음 진행로그7·8만 보고8구간차이0으로 말한 오류를 사용자에게 정정했다. PyAV FFmpeg8의 소스 q3 inferred rate241/12→last4481 ticks, q9 239/12→last4518ticks; Rust FFmpeg6 CLI 마지막4500 ticks. 실제 PTS/DTS와 NAL 무손실은 별도검증 중. 허용오차 확대/기존Python수정 금지, 차이는 조용히 숨기지 않는다.
- 새 integration/python_video_job.py: 원본HEVC prepare_video 사용, inline qTS블록만 로그와 분리한adapter. core mux-only는 복사루프(encoder 없음), 출력은 시간밖에서공통감사. benchmark_remux.py는warmup1/교차3회persegment/camera/variant, samplerCPU/RSS와정규화SHA/headers/PTS-DTS/keyframe/config/faststart 검증. initial audit에서PyAV BSF가packet을consume하므로해시전에timestamp보관하도록수정. 원본14/47 및3cam 도구를 raw파일명도 인식하도록 확장했다.
- pilot segment1 validated-video:PythonHEVC0.593s/CPU0.316,기존Rust1.154s/CPU0.939; q0.211/0.127 vs0.405/0.450. purevideo이고로그/JSON/thumbnail제외. baselineperf442samples/499Hz/lost0: NALfeed7.47%,SHA3.39%,ffmpeg/ffprobe dynamiclookup각2.04%,pipe/kernel wakeups다수. private -s0 strace IO payload미출력으로 read/write 추적보존. source read 재검사는필요.
- 후보 native inspector 구현(src/native_video.rs): 기존FFmpeg BSF를작은RAII wrapper로사용해한demuxpass에서packet시각과기존NAL SHA/header정책동시검사. 별도worker/180초/64MiB IPC/프로토콜·포맷화이트리스트/코덱·단일video·크기·패킷corrupt·boundedNAL유지. 입력/출력 각각동일검증. ffprobe+ffmpeg stream-pipe중복을줄이며 qcamera사전검사도같은job 내Inspection재사용. uploadheader검사/역변환/thumbnail기존경로는아직유지. 새내부CLI __inspect. publicrestschema변경없음.
- fmt/Clippy통과,releaseartifact inspected 생성. 현재 pilot-inspected3구간1/3/9 Python+baseline+후보 교차3회실행중. 아직성능효과/전체회귀/새CI/커밋/PR미확인. 효과없으면후보원복(다른작업자변경건드리지않음).
- 다음:pilot효과/무결성→13구간cam별반복/순수mux/fullconvert분리→nativeFFBSF경계test+35전체+합성3cam→기존61구간oracle재사용Rust회귀→실제원본업로더/2cam원본업로드/download역변환/브라우저→측정안전JSON·3문서갱신→커밋/PR/amd64+aarch64CI. 13코퍼스에는와이드없어실제3cam으로보고하지않는다. HAOS/기존repo/운영설정/릴리스이미지변경금지.
### 원본13 최적화·검증 추가 현황

- 후보 inspected 대표1/3/9 비교: q0.398→0.208s 수준, HEVC합계3.255→2.743s. HEVC 입력해시를mux순회와합친combined는다음실험에서2.821→1.639s(Python1.668). 빈루프가아닌동일검증:PacketClock/SHA/header/count/keyframes 유지. baseline binary immutable.
- `video-all13`교차3회/warmup1/percamera/persegment:HEVC Python7.432s/baseline14.493/combined7.334(총488896405bytes),q2.680/5.189/2.694(30132264bytes). rootCPU/평균/관측Peak RSS 저장. 원본PythonCLI와동일하게gc.disable설정한최종어댑터로새quality후보재측정중이며이전숫자를최종으로승격하지않는다.
- combined기준13전체JSON before/after1e-6차이0,thumbnail JSON/JPEG바이트일치. Python대비3/9 qterminalduration차이는기존과동일. 기존14+47/72749프레임모두차이0. 로컬36테스트(fmt/clippy)+합성3cam full oracle 통과.
- 별도 -s0 strace논리I/O(대표HEVC):source_read기준77282900→37527338bytes, output_read112633578→75089060, pipe75.5MB→124729bytes. 파이프/파일logicalread와디스크physicalIO혼동금지. baseline CPUperf442samples/499Hz/lost0보존.
- sourceNAL全종류별추가검사:HEVC VCL2slices/frame(2400NAL/1200packets),VCL SHA모두동일. 그러나기준/combined HEVC 샘플에VPS/SPS/PPS각60개가남아hvcC분60개와합쳐AnnexB확장시120개로중복. PythonFF8은샘플에없고확장60개. FFmpeg n6.1 movenc filter_ps=0, n8.0 hvc1에서filter_ps적용을공식소스로확인함. 이는기준부터있던품질문제이며새최적화회귀는아님.
- 최종314ed18 corecommit:네이티브FFmpeg filter_units로mux전에32/33/34만제거(그전입력hash, hvcC헤더보존),VCL/SEI/시간/키프레임검증유지. 신규test는FFmpeg독립추출SHA/메타비교및hvc1샘플내PS부재를assert. 전체36+합성3cam다시통과. sourceartifact `/home/dabin/hars-original13/final`.
- 최종순차세션6277:video-final13(all26video×Py/baseline/final×warmup+3repeat)→full-final-1(quality효과분리 combined포함)→quality-full13 기존차이정확히3/9인지→final-regression14/47. 성능벤치와다른CPU작업겹치지않게실행중.
- 실제원본client도구추가(real_device_media.py):RV1 페어링/HMAC/조각재전송/재연결/완료receipt·실제원본3파일53MB업로드·자동변환·입력보존및원본삭제정책으로강제역변환·MP4/원본다운로드SHA/Range206/TSPTS복원. 첫실험도구가finish.logs항목을문자열로잘못가정해실패,원래API는메타객체라id를선택하도록수정(서버스키마변경없음). monitor정리보완. combined로retained/reverse모두성공,원본qPTSdelta0;원본업로드중API p99약1.31ms/RSSpeak116344KiB(transfer+finish포함),변환시관측peak413212KiB. 전용루프백1회표본이고최종후보다시검증예정.
- 기존47코퍼스3cam4파일업로드regression(combined)성공,원본13에와이드없음을구분. 새로운원본13실제3cam검증으로표현하지않음. 최신hvc1후보로원본26영상NAL/decodedframe재감사·signed업로드두모드·브라우저·최종61·CI필요. 아직푸시/PR없음;source314ed18만로컬커밋. 운영설정/원본파일/기존repo변경없음.
### 최종 검증 완료·공개 준비 (2026-11-16)

- Source314ed18최종video-final13: Python/기존/최종 각각warmup1+교차3회/26영상. HEVC 합계7.894787/15.804578/8.676774초,CPU4.343683/13.105135/5.658285; TS2.925770/5.836634/2.981046초,CPU1.736495/6.445157/2.617669. 카메라별 전체입력/합계시간 weighted MB/s사용. 최종은Py보다H9.9%/TS1.9%느리고RSS도높다고명시했다.
- 전체1번구간교차3회 baseline4.165559→final3.243523초(Py4.192819),CPU3.828534→2.776158. hvc1전combined3.205762와같은실험에서품질보정효과분리. 다른시리즈절대시간합산안함.
- 최종13모델15600프레임. 기존Rust대비13/13JSON동일. Python대비11/13,3/9 qduration+0.211/-0.200ms잔여호환차이를그대로공개(1e-6확대없음). 최종61/72749 차이0재확인.
- 최종26MP4전체decoded1200/오류0,정규화전체NAL SHA원본동일(allNAL types/count/order보존한독립PyAV감사). HEVC각60PS중복문제해결,2slices/frame구분. 최종명시적원본삭제서버역변환도원본과정규화전체NAL동일,TS PTSdelta0. 파일SHA는다를수있고실제로재생성파일은달랐다.
- Pinned RV1 client 실제3파일upload/재전송/재연결/완료receipt/자동변환/다운로드보존sha/강제reverse+Range206두모드최종통과. 독립thread관측최종upload API p95 0.969/0.794ms,p99 16.161/19.118ms(표본36/39),conversionp99<0.9ms. transfer+finish포함RSSpeak121096/112048KiB,converter포함420204KiB. 관측기자체async업로더와loop공유한초기시리즈는backend최종성능으로쓰지않음. 최신관측은새HTTP연결/OS스케줄링포함end2end이며순수handler시간아님.
- 최종perf215samples/499Hz/lost0,관측CPU.431초(기준.886). strace sourceRead77.283→37.527MB,pipe75.518MB→124729bytes,outputRead112.634→75.076MB. 논리IO(재읽기포함),디스크물리bytes아님. 무미디어startup5회control/median보존,성공한mux순수init와직접감산안함.
- ego Space15:새13/segment3 HEVC1344×760 play/pause/30s; q59.9s/duration59.989089/ready4/error0/Frame4800; 실제지도/overlay/차량그래프확인. 별도기존47 3cam/segment6 wide44.999989s/ready4/error0. 사진private유지. Spacefinish및두ownedloopback서버모든ready확인후종료.
- 원본39파일 총691661569bytes SHA불변. 공개숫자5JSON원본파일/GPS/인증/perf메모리/payload없음확인. STATUS/PERFORMANCE/VALIDATION/DEVELOPMENT갱신. source314ed18외도구·문서아직미커밋. 다음doc/tool커밋→PR1개→Rust36+amd64/aarch64필수CI→요청된기존개발절차대로결과확인/병합. 실제HAOS/포트/설정/원본Python/openpilot/운영GHCR/릴리스는변경안함.
## 2026-11-16 alpha.3 기능 완성·안정화 (조정 기록)

- 기준 `a76c0c5`. 저장소에 AGENTS.md/CLAUDE.md 없음, DEVELOPMENT.md와 사용자 전역 지침 적용. 병렬 작업자는 WSL `~/hars-wt/<작업명>` 사본과 `~/hars-targets/<작업명>`에서 일하고 브랜치만 올림. 조정자가 검토·PR·CI·병합.
- 병합(각 PR CI 4개 검사 통과 확인 후): #4 qcamera 마지막 duration(`ee2fa7d`), #3 저장 공간 보호·삭제 저널(`8fee5d4`), #5 Drive/NAS 백엔드(`3003dac`), #6 업로드 장애 주입·결함 수정, #7 API 동등성, #8 순수 Rust 리먹싱 실험(`f74d95d`).
- #4 원인 정정: 이전 메모의 "FFmpeg 6/8 버전 차이"는 틀림. `ffmpeg`/`ffprobe` CLI가 `scan_all_pmts=1`을 강제해 TS 탐색이 약 103프레임까지 이어지고 20/1로 추정. PyAV는 라이브러리 기본값으로 21프레임에서 241/12·239/12 추정. 같은 FFmpeg에서 옵션만 바꿔 양쪽 재현. qcamera 리먹싱을 libavformat 기본값 경로(`native_video::remux_ts`)로 변경. 13/13, 14/14, 47/47(72,749프레임) 동등.
- #3 독립 검토 지적 3건 수정: 세션 생성 전체를 registration 락 안에서, 자동 정리는 route 전체 락 후 재확인, 삭제 커밋 시점은 숨김 rename(복구는 `.deleting-<id>`만 완료, 숨겨지지 않은 저널 항목은 폐기). 기존 테스트 2개 기대값을 "재시작 시 삭제 완료"→"기록 보존"으로 변경(보존 쪽 정책 변경).
- 결정: 변환 중 기록의 수동 삭제는 Python처럼 변환 취소 후 삭제(자동 정리는 route 전체 보호). UI 오류 형식은 `error`=코드, `message`=Python 문장 유지(화면 표시 문장 동일, 장치 업로더는 코드로 재시도 판단). 순수 Rust 리먹싱은 채택 보류, FFmpeg 엔진 기본 유지.
- #6/#7 독립 검토: 차단 사유 없음. 3자 잠금 순환(가져오기 begin/장치 begin/finish)과 레거시 업로드 사전 입장은 통합 merge에서 수정. 남은 지적(408 재시도 불가, 슬롯 순서, flush, ZIP 임시 파일·fd, 부팅 문구)은 `fix/upload-parity-followup`에서 처리 중.
- 진행 중: `feat/archiver`(아카이버 본체·설정 라우트, 기준 `3003dac`), `fix/upload-parity-followup`(검토 잔여, 기존 구간 카메라 추가, finish 실패 세션 정책; 기준 `f74d95d`).
- 남음: 원격 파일 재생·다운로드, 보충 변환(backfill/gap_backfill), 지도 보충(tracks/run·stop), Python 원격 데이터 읽기 전용 조회, 버전 alpha.3, 문서·인벤토리 갱신, `scripts/check_coexistence.py` CI 연결, 컨테이너 기동 검사에 TS 사례 추가, 실제 코퍼스 전체 재검증과 브라우저 확인.
- 운영 주의: `drive.file` 범위라 Python이 올린 파일을 읽으려면 같은 OAuth client ID 필요(미확인). Python `.archive.json`을 HARS 데이터 폴더에 복사 금지.
