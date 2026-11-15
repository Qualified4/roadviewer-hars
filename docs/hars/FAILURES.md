# 재현된 실패와 수정

## Nginx TLS 준비 전 HTTP health가 성공한 기동 경합

기준: `5265050`, [첫 HARS CI](https://github.com/Qualified4/roadviewer-hars/actions/runs/43701810253).

Rust 검사 27개와 원본 ccnc 클라이언트/인증 이전 검사는 성공했다. amd64/aarch64 이미지 빌드도 성공했으나, 컨테이너 기동 직후 TLS 검사에서 connection reset/SSL EOF가 발생했다. `HARS required`는 이를 실패로 반영했다.

원인은 Nginx 자식 프로세스의 spawn을 TLS 수신 준비 완료로 간주한 것이다. 빠른 Rust HTTP 서버가 먼저 health 요청에 성공해도 Nginx는 아직 listen을 시작하지 않을 수 있었다. 로컬 Docker 환경의 상대적인 지연이 이 순서를 가린 상태였다.

재현: 검사 전용 Nginx launcher에서 실제 daemon 시작만 0.3초 늦춘다. 기존 이미지는 같은 TLS EOF로 실패했다. 수정 후 동일한 fixture에서 컨테이너 기동·인증서 검증·API 격리·네이티브 HEVC 처리가 통과했다.

수정: 활성화된 Nginx의 생존과 loopback TLS 포트의 준비를 확인한 다음 HTTP 요청 처리를 시작한다. 준비 확인은 5초로 제한하고 실패 시 앱 기동을 실패시킨다. 준비 시간을 비밀값 없는 `nginx_tls_ready` 이벤트로 기록한다. 업로더 재시도·네트워크 타임아웃은 바꾸지 않는다.

회귀 검증: CI의 컨테이너 기동 검사는 항상 이 초기화 지연 fixture를 사용하며 health 성공 직후 첫 TLS 요청이 성공해야 한다. TLS 요청 자체를 반복해서 통과시키지 않는다. 실패 시 제한된 컨테이너 상태와 구조화 이벤트명만 출력한다.
