# alpha.2 이식 기준

대상은 업로드 완료 → 대기/변환 큐 → 독립 저우선순위 Rust 작업 프로세스 → 로그 분석/카메라 검증 → 임시 prepared 세대 → 원자적 게시 → 기존 UI 재생이다. 기존 인증/업로드 엔진과 영상 검증을 재사용한다. Drive/NAS 및 운영 전환은 이번 범위에서 제외한다.

Python 최신 main `7bc3b4f`(1.3.17)와 보존 기준 `71add1a0`(1.3.16)를 비교했다. 변경은 지도 색상 등 웹 리소스이며 이번 대상 Python 분석 모듈·스키마는 동일하다. 따라서 현재 HARS에 보존된 1.3.16 UI/분석 코드로 출력 동등성을 검증한다.

데이터 흐름과 주의점:

- `decoder.pick`의 선택 필드 외에도 `Starts`, GPS collector, gap, 도로 신호가 이벤트를 직접 읽는다. 이 필드를 Rust 선택 스펙에 추가해야 한다. first_mono는 initData를 제외한 모든 알려진 이벤트의 최소 시각이다.
- 모델의 카메라 시각은 qRoadEncodeIdx의 frameId 우선, 없으면 model timestampEof, 마지막으로 logMonoTime이다. radar/liveTracks의 nearest 검색은 원본 모델 logMonoTime 기준이며 steering/CCNC/gap/GPS는 카메라 시각 기준이다.
- CCNC는 Hyundai brand일 때만 전방/도로 명령을 사용한다. 150 ms freshest-past와 nearest 검색을 서로 대체하지 않는다. steering 토크 필터는 모델의 정렬된 순서에 따라 상태를 유지한다.
- overlay는 보정된 homogeneous basis와 원본 geometry만 저장한다. compact는 cameraInfo 중복 제거, geometry 반올림, 동일 flat 좌표 제거와 laneX grid 축약으로 replayV2를 만든다.
- GPS는 수신기 → 차량 GPS → carrot 순서로 소스를 고르고 1 Hz·90 m/s 점프 제한을 적용한다. 좌표는 track 응답에만 제공한다. wall clock은 GPS → 최신 clocks → initData 순서다.
- 카메라마다 연속 segmentId·증가하는 timestampEof·압축 NAL·프레임 수·PTS/DTS를 검사한다. qcamera는 source PTS spread 5 ms와 원점 보존이 추가로 필요하다. 시간축은 모델과 모든 성공한 영상의 coverage 합집합이다.
- thumbnail은 한 소스의 처음 140 packets에서 1..5초 후보를 고른다. luma histogram의 점수와 1/99 percentile·gamma 보정을 사용하고 JPEG/metadata/실패 상태를 관리한다.
- 자동 변환은 최근 업로드가 수신 중이면 대기한다. 작업 프로세스는 nice +10, 가능한 경우 ionice best-effort 7을 사용한다. Tokio 수신 스레드에서 분석/전체 디스크 순회를 수행하지 않는다.
- 재변환/취소/재시작 중에도 원본은 보존한다. 새 prepared는 전체 성공 후 게시하며 이전 재생 결과를 먼저 지우지 않는다. 원본 Python의 일부 파괴적 재변환 동작과 다른 이유는 데이터 보호이며 별도로 테스트한다.

자동 비교는 원본 Python 함수를 기준으로 schema/배열 순서/결측/null/단위/수치를 확인한다. 모델/좌표의 원본 반올림 자릿수와 2 ms 영상 시각 검증을 유지한다. 실주행 코퍼스는 아직 확인되지 않았으며 합성 결과로 대체했다고 표시하지 않는다.
