# HARS 설계 및 기준

## 원본과 범위

- Python Road Viewer `main`: `d3de13472ea242dab9866d485b5777ad67d5b1dd`, 버전 1.3.18. 프런트엔드 기능 기준: Python Road Viewer 1.3.18(`d3de134`). 서버 API·분석 코드는 이전 보존 기준 1.3.16(`71add1a0`)과 같다(`git diff 71add1a d3de134 -- 'roadviewer/app/*.py'` 변경 없음). 1.3.16 → 1.3.18의 변경은 웹 파일(`roadviewer/app/web/`), 그 브라우저 테스트, 버전·문서뿐이다.
- 웹 파일은 보존 원본 그대로 제공한다. HARS가 페이지를 내보낼 때만 원본 릴리스 번호(`library.html`의 `assets/style.css?v=` 값, Python `server.page_response`가 읽는 곳)를 자산 주소의 `?v=`와 버전 표시에서 HARS 버전으로 바꾸고 `Road Viewer`를 `Road Viewer HARS`로 바꾼다(`src/http.rs` `page_html`, 검사 `tests/page_version.rs`).
- openpilot `Qualified4/openpilot`, `ccnc-hda1`: `5c2a8c7f6e14dac82678a390e408ef7a9c73eefa`.
- 클라이언트 계약: `openpilot/selfdrive/carrot/server/road_viewer/client.py`, `uploader.py`.
- Git 이력은 원본 커밋의 자손으로 유지한다. 원본 태그·브랜치·릴리스·Actions·이미지는 변경하거나 복제 게시하지 않는다.
- 원본 코드와 스키마 라이선스는 `LICENSE`, `reference/python/roadviewer/schema/OPENPILOT-LICENSE`에 보존한다.
- 코드에서 추출한 전체 모듈·라우트·테스트 목록은 `inventory.json`에 있다. 정적 추출 도구: `scripts/inventory.py`.
- Supervisor는 하위 폴더의 앱 설정도 재귀 탐색하므로 원본 앱 설정은 내용 그대로 `reference/python/roadviewer/config.yaml.reference`에 보존한다. HARS 저장소에서 발견되는 앱 설정은 `roadviewer_hars/config.yaml` 하나뿐인지 검사한다. 근거: [공식 Supervisor 탐색 구현](https://github.com/home-assistant/supervisor/blob/main/supervisor/store/data.py).

## 실행 경계

Tokio와 Axum의 서로 다른 Router와 TCP 수신기를 사용한다. Ingress는 실제 TCP peer가 `172.30.32.2`인지 검사한다. 프록시 전달 헤더로 Ingress 인증을 우회할 수 없다. 변경 요청은 기존 CSRF 헤더를 요구한다. 장치 API 백엔드는 loopback에만 바인딩하며 외부 TLS Nginx는 `/api/device/`만 전달한다.

앱별 단일 데이터 디렉터리에 프로세스 잠금을 잡아 여러 쓰기 프로세스의 실행을 거부한다. Python의 `/data/roadviewer`를 HARS 데이터 경로로 사용하는 것도 거부한다. 동일 볼륨을 두 앱에서 공유하지 않는다.

## 락과 비동기 작업

- 장치 상태 락: HMAC·nonce·장치·영수증의 작은 상태 변경과 원자적 저장에만 사용.
- 업로드 입장 락: 배치 복구·공간 예약·세션 생성 직렬화. 영상 검증이나 전체 파일 해시를 수행하지 않음.
- 세션 생명주기 RwLock: 서로 다른 파일의 조각 쓰기는 동시 실행, 완료·취소는 배타 실행.
- 파일별 Mutex: offset 비교·재전송 비교·쓰기·동기화를 파일 하나에 대해서만 직렬화.
- 장치 폐기 상태와 수신 바이트/진행 시각은 별도 메모리 스냅샷으로 조회한다. nonce·영수증 파일 저장이나 다른 파일의 stat 순회 때문에 각 조각의 인증/offset 조회가 대기하지 않는다. 본문 수신 전 전송 슬롯 8개로 메모리 사용을 제한하며 UI 조회는 이 제한을 사용하지 않는다.
- UI 메타데이터 인덱스: 디스크 순회 없이 메모리 스냅샷 조회. 디스크 사용량은 백그라운드에서 측정하므로 표시값에 최대 약 10초 지연이 있다.
- 로그 파싱과 gzip 해제: blocking worker에서 실행. 메시지별 Zstandard 읽기, 압축 해제 512 MiB·Cap'n Proto 탐색/중첩 제한.
- FFmpeg/ffprobe: 비동기 자식 프로세스, 동시 실행 2개, 시간·출력 상한. 명시적인 입력 demuxer와 `file,pipe` 프로토콜만 허용.

## 업로드 영속성

원본 `RV1` 정규화 문자열, 장치 비밀값의 ASCII hex HMAC 키, nonce 키 해시와 만료 시각을 유지한다. 단순한 JWT로 교체하지 않는다. 서명은 body의 실제 수신 바이트를 해시한다. 배치 비교는 정렬된 JSON 직렬화를 사용한다.

조각 성공은 파일 데이터 동기화 후 응답한다. 길이는 별도 offset 상태가 아닌 파일 실제 길이에서 복구한다. 동일 offset의 재전송은 저장된 바이트와 일치할 때만 성공한다. 토큰은 상태 파일에 해시만 저장하며, 갱신은 이전 토큰을 무효화한다. 상태 폴링·토큰 갱신을 업로드 진행으로 간주하지 않는다.

등록은 `commit.json` 저널을 먼저 저장하고 원본 파일을 이동한다. 영수증을 원자적으로 저장한 뒤에만 세션과 예약을 제거한다. 미완료 등록 저널은 만료 정리에서 삭제하지 않는다. 장애 주입 및 모든 등록 중복·카메라 추가 정책은 아직 전체 검증이 필요하다.

## 영상 정책

재인코딩 없이 `-c:v copy`를 사용한다. MP4의 `avcC`/`hvcC`, `hvc1`, faststart, 프레임 수와 로그 시간 오차 2 ms, Annex B로 정규화한 압축 NAL 및 디코더 헤더 해시를 검사한다. 썸네일만 디코딩한다.

raw HEVC는 `ffmpeg-next`의 libavformat 패킷 경로에서 프레임별 로그 시각을 PTS/DTS에 직접 지정한다. 가변 간격도 보존하며, 원본 Python과 동일하게 B 프레임은 거부한다. 서버 실행 파일이 별도 자식 프로세스로 네이티브 작업을 수행하므로 실패·취소·시간 제한을 웹 수신기와 분리한다. 라이브러리 단독 호출 테스트는 동시 실행이 제한된 blocking worker를 사용한다. NAL 검사는 스트리밍하고 MP4 검증은 미디어 본문을 건너뛰며 메타데이터만 읽는다. 원본 형식 역변환도 스트림 복사 후 압축 영상 해시를 검증한다.

## 정식 릴리스 게이트

전체 기능·API·JSON·실제 업로더·실제 로그·브라우저 재생/시킹·저장소 장애 복구·HAOS 동시 설치가 검증되기 전 `2.0.0` 태그 또는 정식 Release를 만들지 않는다. 초기 게시 자동화는 `2.0.0-alpha.N`, `beta.N`, `rc.N`만 허용한다.
