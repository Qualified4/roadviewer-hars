# 재현된 실패와 수정

## Nginx TLS 준비 전 HTTP health가 성공한 기동 경합

기준: `5265050`, [첫 HARS CI](https://github.com/Qualified4/roadviewer-hars/actions/runs/43701810253).

Rust 검사 27개와 원본 ccnc 클라이언트/인증 이전 검사는 성공했다. amd64/aarch64 이미지 빌드도 성공했으나, 컨테이너 기동 직후 TLS 검사에서 connection reset/SSL EOF가 발생했다. `HARS required`는 이를 실패로 반영했다.

원인은 Nginx 자식 프로세스의 spawn을 TLS 수신 준비 완료로 간주한 것이다. 빠른 Rust HTTP 서버가 먼저 health 요청에 성공해도 Nginx는 아직 listen을 시작하지 않을 수 있었다. 로컬 Docker 환경의 상대적인 지연이 이 순서를 가린 상태였다.

재현: 검사 전용 Nginx launcher에서 실제 daemon 시작만 0.3초 늦춘다. 기존 이미지는 같은 TLS EOF로 실패했다. 수정 후 동일한 fixture에서 컨테이너 기동·인증서 검증·API 격리·네이티브 HEVC 처리가 통과했다.

수정: 활성화된 Nginx의 생존과 loopback TLS 포트의 준비를 확인한 다음 HTTP 요청 처리를 시작한다. 준비 확인은 5초로 제한하고 실패 시 앱 기동을 실패시킨다. 준비 시간을 비밀값 없는 `nginx_tls_ready` 이벤트로 기록한다. 업로더 재시도·네트워크 타임아웃은 바꾸지 않는다.

회귀 검증: CI의 컨테이너 기동 검사는 항상 이 초기화 지연 fixture를 사용하며 health 성공 직후 첫 TLS 요청이 성공해야 한다. TLS 요청 자체를 반복해서 통과시키지 않는다. 실패 시 제한된 컨테이너 상태와 구조화 이벤트명만 출력한다.

## Drive로 옮긴 구간에 와이드 영상을 더하면 등록이 409로 거부됨

기준: 2.0.3, 2026-11-19 사용자 보고. 고화질 영상만 올린 구간을 Drive로 옮긴(rlog 포함) 뒤 장치에서 와이드(`ecamera.hevc`)를 올리면 19개 구간 모두 등록 단계에서 실패했다. 서버 응답은 `409 storage_unavailable`, 장치 클라이언트는 이 코드를 모르므로 `upload_failed`(“Road Viewer 요청에 실패했습니다”)로 보였다.

원인: 등록 전 `archived_matches`는 Drive의 `read_state`로 파일을 확인하는데, 이 경로는 “연결됨” 결과를 300초 동안 믿고 다시 확인하지 않는다(`READ_TRUST_SECONDS`). 반면 `plan`의 `reachable`은 10초 캐시만 읽는 `last_state()`였다. 최근 10초 안에 Drive 확인이 없었으면 `None`이 되어 보관된 구간의 카메라 추가가 항상 거부됐다. 재시도해도 같은 이유로 다시 실패한다. Python 원본도 Drive에서는 `archive_state()`가 백그라운드 갱신만 해서 같은 구조였다.

수정: `finish`가 등록 잠금 전에 `archive_reachable`로 연결 여부를 정한다. 보관된 구간과 일치하는 업로드는 `probe(true)`로 실제 확인하고(캐시가 신선하면 재사용), 그 결과를 `plan`에 넘긴다. 일치하는 보관 구간이 없으면 기존처럼 `last_state()`만 읽는다. 업로드 입장 검사(`eviction_guards`)의 보수적 판단은 바꾸지 않았다.

회귀 검증: `tests/archive_reupload.rs`가 조절 가능한 시계로 Drive 확인을 60초 전 결과로 만든 뒤 rlog+와이드 영상을 올린다. 수정 전 `409 storage_unavailable`, 수정 후 등록 성공과 `ecamera.hevc` 추가를 확인한다.
