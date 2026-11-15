# Road Viewer HARS

Rust 기반 Road Viewer 2.x의 독립 개발 저장소입니다. 현재 버전은 **2.0.0-alpha.1**이며, 전체 기능 이식과 운영 전환 검증은 진행 중입니다. 정식 릴리스와 운영 전환을 수행하지 않았습니다.

Python [Road Viewer 1.x](https://github.com/Qualified4/roadviewer-ha)는 별도 제품으로 계속 유지됩니다. 해당 저장소를 수정하지 않습니다. 원본 이력과 MIT 라이선스를 유지하고, 기준 코드·테스트·스키마·UI는 `reference/python`에 보존합니다. 제품 컨테이너는 Python을 설치하지 않습니다.

- [기능·API 인벤토리](docs/hars/inventory.json)
- [구현·검증 현황](docs/hars/STATUS.md)
- [설계와 이식 기준](docs/hars/ARCHITECTURE.md)
- [HAOS 병행 설치와 전환·롤백](docs/hars/CUTOVER.md)
- [실행 및 테스트](docs/hars/DEVELOPMENT.md)

개발 실행(Linux/WSL, Rust stable 및 `capnproto`, `ffmpeg` 필요):

```sh
cargo test --locked
HARS_DATA=/tmp/hars-dev HARS_INGRESS_ONLY=0 cargo run --locked
```

UI는 `http://127.0.0.1:8099`, 장치 API 내부 수신기는 `127.0.0.1:8098`입니다. 개발 모드 UI의 변경 요청도 `X-RoadViewer-Request: 1`이 필요합니다. 외부 HTTPS는 별도 Nginx 수신기를 사용합니다. 원본 장치 API는 UI 수신기에 노출하지 않습니다.

미이식 기능은 성공으로 응답하지 않습니다. 현재 빌드를 Python 1.x의 완전한 대체 제품으로 설치하거나 운영 업로드를 전환하면 안 됩니다. 모든 실제 검증 결과와 남은 항목은 현황 문서에 구분해 기록합니다.
