# openpilot RV1 클라이언트 테스트

출처: https://github.com/Qualified4/openpilot/tree/5c2a8c7f6e14dac82678a390e408ef7a9c73eefa/openpilot/selfdrive/carrot/server/road_viewer

- 기준 브랜치: `ccnc-hda1`
- 기준 커밋: `5c2a8c7f6e14dac82678a390e408ef7a9c73eefa`
- 원본 바이트를 보존한 `client.py`를 [RV1 호환 검사](../../integration/ccnc_compat.py)와 [업로드 장애 검사](../../integration/uploader_faults.py)가 실행한다.
- 루트 라이선스도 함께 보존한다.
- HARS 제품 이미지와 Rust 런타임에서 Python 코드를 실행하지 않는다.
- 업로더는 실제 body 바이트의 SHA-256을 RV1 문자열에 넣고, ASCII hex 장치 비밀값으로 HMAC을 만든다.
- 스트리밍 체크섬, 최대 8 MiB 조각 협상, offset 재조회·배치 복구, 완료 결과의 logs/duplicates/updated를 사용한다.
