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
