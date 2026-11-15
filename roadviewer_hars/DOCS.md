# 설치·운영 안내

이 alpha는 독립 개발·검증용입니다. 기존 Python 앱과 데이터·장치 인증·아카이브 경로를 공유하지 않습니다.

Home Assistant 앱 저장소에 `https://github.com/Qualified4/roadviewer-hars`를 등록하면 **Road Viewer HARS**가 별도 항목으로 표시됩니다. 실제 설치에는 해당 버전의 HARS 전용 GHCR 이미지가 먼저 게시되어 있어야 합니다. 현재 작업에서는 이미지를 게시하지 않았습니다.

설치 후에도 외부 HTTPS 포트는 기본 비활성(null)이며 자동 시작도 기본 비활성입니다. 테스트할 때만 기존 운영 포트와 다른 포트를 지정하세요. `device_https_enabled`를 켜려면 `/ssl`의 인증서·키 파일이 필요합니다. Ingress는 외부 업로드 포트와 별도로 동작합니다.

전체 기능·실제 로그·장치·HAOS 검증 전에는 운영 전환을 하지 않습니다. [전체 현황](https://github.com/Qualified4/roadviewer-hars/blob/main/docs/hars/STATUS.md)과 [전환·롤백 절차](https://github.com/Qualified4/roadviewer-hars/blob/main/docs/hars/CUTOVER.md)를 확인하세요.

원본 앱과 같은 정책으로 기록 폴더와 전송/내보내기 임시 파일은 HA 앱 백업에서 제외합니다. 백업에 영상·주행 기록까지 들어 있다고 가정하면 안 됩니다. 인증·설정 보존과 데이터 별도 백업/아카이빙을 구분해야 하며, HARS의 원격 아카이빙은 아직 구현 중입니다.
