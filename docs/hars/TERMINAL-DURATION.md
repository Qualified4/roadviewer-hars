# 원본 영상 마지막 프레임 길이 규칙

<!-- toc -->
**목차**

- [결론](#결론)
- [증거(13구간 실주행 원본)](#증거13구간-실주행-원본)
- [규칙](#규칙)
- [영상 길이와 주행 길이](#영상-길이와-주행-길이)
- [Python 데이터와의 호환](#python-데이터와의-호환)
- [시험](#시험)
<!-- /toc -->

## 결론

원시 원본(`qcamera.ts`, `fcamera.hevc`, `ecamera.hevc`)을 MP4로 옮길 때, 마지막 sample의 길이는 **같은 rlog에 기록된 다음 카메라 프레임의 노출 종료 시각까지**로 한다. 그 기록이 없거나 이상하면 그 영상 자신의 프레임 간격 중앙값을 쓴다. 두 리먹싱 엔진(순수 Rust, FFmpeg)이 같은 함수(`src/terminal_duration.rs`)의 결과를 받아 쓴다. 그래서 대체가 일어나도 결과가 같다.

Python 원본의 값(qcamera: libavformat probe의 `r_frame_rate` 추정, HEVC: 0.05초 고정)은 복제하지 않는다. 실제 다음 프레임까지의 간격과 비교하면 Python 값의 최대 오차는 TS 356 ticks(3.96 ms), HEVC 3,752 µs였다. 새 규칙은 13구간 모두에서 오차가 0~1 tick(0.011 ms 이하)이다.

## 증거(13구간 실주행 원본)

입력: 같은 route의 연속 13구간(읽기 전용). PyAV 16.1.0(FFmpeg 8.0.1)으로 TS를 demux하고, pycapnp로 rlog를 읽고, TS 패킷에서 PCR을 직접 읽었다. 숫자 집계는 [측정 JSON](measurements/terminal-duration-evidence-2026-11-16.json)에 있다. 단위: tick = 1/90000초.

관찰:

1. **TS 시각은 로그 시각에서 온다.** 13구간 모두 첫·마지막 프레임에서 `PTS = round(timestampEof × 90000 / 1e9)`가 0 tick 차이로 맞는다(프레임별로는 1,147~1,162/1,200개가 정확히 같고, 나머지는 1 tick 차이). PCR은 마지막 PES PTS와 같다(차이 0). SPS에는 VUI timing이 없다(PR #4 기록, PyAV `framerate` 없음). 즉 TS 안에는 마지막 프레임 길이에 대한 별도 정보가 없다.
2. **TS PTS는 구간 사이에서 이어진다.** 구간 N+1의 첫 PTS − 구간 N의 마지막 PTS = 4,163~4,604 ticks(리셋 없음). 12쌍 모두 frameId가 1씩 이어진다(`qRoadEncodeIdx`, `roadEncodeIdx` 둘 다).
3. **다음 프레임은 같은 구간의 rlog에 이미 있다.** 인코더는 두 프레임 사이에서 구간을 바꾸므로, 구간 N의 rlog에는 마지막 인코딩 프레임(frameId F)의 다음 프레임 F+1의 `roadCameraState`가 남는다. 13구간 모두 있었고, 12쌍 모두 그 `timestampEof` 차이가 구간 N+1 첫 프레임과의 실제 간격과 **ns 단위로 같았다**. 13번 구간도 같은 기록이 있어 같은 방식으로 정해진다.
4. 구간 안 프레임 간격: TS DTS 간격 평균 4,498.5~4,503.5, 중앙값 4,498~4,503, 범위 3,320~5,680 ticks, 표준편차 108~351. 로그 간격도 같은 분포다(평균 49.98~50.04 ms).

마지막 프레임 길이 후보와 실제 간격(다음 프레임 기록)의 차이(ticks, 후보 − 실제):

| 구간 | 실제 | Python probe | 공칭 4500 | 직전 간격 | 중앙값 | 평균 |
|---|---|---|---|---|---|---|
| 1 | 4496 | +4 | +4 | +4 | +2 | +2 |
| 2 | 4359 | +141 | +141 | −56 | +139 | +140 |
| 3 | 4514 | −33 (4481) | −14 | −12 | −15 | −15 |
| 4 | 4357 | +143 | +143 | +293 | +142 | +143 |
| 5 | 4502 | −2 | −2 | −2 | −3 | −3 |
| 6 | 4209 | +291 | +291 | +237 | +290 | +291 |
| 7 | 4604 | −104 | −104 | −253 | −105 | −105 |
| 8 | 4504 | −4 | −4 | −302 | −5 | −3 |
| 9 | 4162 | +356 (4518) | +338 | +678 | +341 | +342 |
| 10 | 4279 | +221 | +221 | +434 | +222 | +223 |
| 11 | 4499 | +1 | +1 | +7 | +1 | +2 |
| 12 | 4461 | +39 | +39 | +80 | +39 | +39 |
| 13 | 4499 | +1 | +1 | −3 | 0 | +1 |
| **최대 \|오차\|** | | 356 | 338 | 678 | 341 | 342 |
| **평균 \|오차\|** | | 103.1 | 100.2 | 181.6 | 100.3 | 100.7 |

"실제"는 로그 ns 간격을 tick으로 반올림한 값이다. TS 자체의 구간 간 PTS 차이와는 6·9번에서 1 tick 다르다(TS PTS 자체가 1 tick 단위로 반올림된 값이기 때문). 13번은 다음 구간 파일이 없으므로 같은 구간 rlog의 다음 프레임 기록만으로 정했다.

HEVC(`fcamera.hevc`, µs): 실제 46,248~51,155 µs. Python의 0.05초 고정은 최대 3,752 µs(평균 1,113), 중앙값은 최대 3,783 µs(평균 1,114), 직전 간격은 최대 7,535 µs 차이였다.

판정: Python의 21프레임 probe 추정은 공칭값보다 나을 때가 없다(3·9번은 공칭보다 나쁘다). 직전 간격은 가장 나쁘다(현재 순수 엔진 규칙, 폐기). 다음 프레임 기록은 정확하다.

## 규칙

`terminal_duration::decide(stamps, timescale, next_frame_ns)`(정수 연산만 사용):

1. `next_frame_ns`가 있고(마지막 인코딩 프레임의 frameId+1인 `roadCameraState`/`wideRoadCameraState`가 정확히 한 가지 시각으로 기록됨, 간격 > 0), tick으로 반올림한 값이 `0 < 값 ≤ 2 × 중앙값 간격`이면 그 값을 쓴다(`next_frame`).
2. 아니면 그 영상 DTS 간격의 아래쪽 중앙값을 쓴다(`median_interval`). 공칭 20 fps보다 최대 오차가 3 ticks 크지만(341 대 338), 20 fps가 아닌 입력에서도 그 입력 자신의 간격을 따른다.
3. 프레임이 하나뿐이고 1이 안 되면 공칭 20 fps(1/20초)를 쓴다(`nominal`). openpilot 도로·광각 카메라의 공칭 속도이고 코퍼스 중앙값(4,498~4,503 ticks)과 맞는다.

비정상 입력: 다음 프레임이 중앙값의 2배보다 멀면(정지, 연속 누락) 마지막 프레임이 그만큼 보였다고 보지 않고 2번을 쓴다. 다음 프레임 시각이 마지막보다 앞서거나 같으면 쓰지 않는다. DTS가 역행하면 두 엔진 모두 변환을 거부한다(기존과 같음). 가변 FPS는 1번이 정확한 값을 주고, 2번은 영상 전체의 중앙값이다.

반올림: ns → tick은 `(ns × timescale + 5e8) / 1e9`(정수, 반올림). TS는 90 kHz, HEVC는 1 MHz다. 부동소수 연산이 없어 아키텍처에 따라 달라지지 않는다.

적용 위치: `convert.rs`가 rlog에서 다음 프레임 시각을 찾아 `media::remux_probed`에 넘긴다. `media`가 규칙을 한 번 계산하고, 순수 엔진(`pure_remux::Job.terminal`)과 FFmpeg 엔진(`native_video::remux_ts`/`remux_hevc`의 마지막 패킷 duration)에 같은 값을 준다. 출력 검사는 composition offset이 없는 출력(순수 엔진, B-frame 없는 FFmpeg 사본)에서 `duration_ts − (마지막 DTS − 첫 DTS)`가 그 값인지 확인한다. B-frame FFmpeg 사본은 libavformat이 edit list를 따른 `duration_ts`를 보고해 이 식이 성립하지 않으므로(흔들리는 카메라 시각에서 첫·마지막 composition offset이 다르다), 독립 판독기(`pure_remux::readback`)가 읽은 stts의 마지막 값이 그 값인지 확인한다. 순수 엔진은 자기 출력의 stts 마지막 값도 같은 방식으로 확인한다. 로그 없이 부르는 `remux` CLI는 1번 정보가 없어 2번을 쓴다. 기존 MP4(`camera.mp4`, `fcamera.mp4`)를 재사용하는 경로는 바꾸지 않는다.

## 영상 길이와 주행 길이

| 필드 | 계산 위치(Python / HARS) | 마지막 duration 영향 |
|---|---|---|
| `videos.<camera>.duration` | `video_packets.inspect_mp4` stream duration / `convert::camera`의 `duration_ts × time_base` | 직접 |
| `video.duration` | 기본 영상(front→qcamera→wide) 복사 | 직접 |
| `duration`(replay, summary) | `timeline.py`: `max(logEnd, start+duration)` / `convert.rs` 같은 식 | 영상 끝이 로그 끝보다 늦으면(13구간 모두) 직접 |
| `telemetry.duration` | 위 `duration` | 같음. 이 값 + 0.15초 밖의 표본은 버린다 |
| `drive_summary.engaged_ratio` | `telemetry.drive_summary` / `telemetry::drive_summary` | 분모가 `duration`이므로 4자리 반올림이 바뀔 수 있다 |
| `logStart`, `logEnd`, `frames[].t`, `videos.*.start` | 로그 시간축 | 없음 |

UI(`app/web/app.js`): `data.duration`은 시크 막대 최대값·끝 시각·재생 정지 시점이다. `video.start + video.duration`은 영상 표시 구간(`videoAvailable`)과 마지막 프레임 유지(`finalVideoFrame`, 허용 오차는 `video.duration / video.frames` cadence에서 계산)에 쓴다. `syncVideo`는 `video.currentTime`을 `duration − 0.001`로 자른다. 구간을 이어 재생하는 기능은 없다(구간마다 따로 연다). 그래서 영상 끝이 실제 다음 프레임 시각에 맞으면, 마지막 프레임은 다음 구간이 시작될 시점까지 보이고 그 이상 늘지 않는다.

재생 확인(headless Edge, 13구간 26영상 × 두 엔진 × 3회): `video.duration`이 replay JSON의 `videos.*.duration`과 같고(최대 4.4e-7초), 마지막 0.3초를 재생한 `ended` 시점이 정확히 `duration`이었다(156/156). 13구간 모두 영상 끝이 로그 끝보다 늦어 `duration`(시크 막대 끝)은 영상 끝이다. 오버레이·그래프는 로그 시간축(`frames[].t`)을 쓰므로 이 규칙으로 바뀌지 않는다(replay JSON의 나머지 필드가 Python과 같음으로 확인).

## Python 데이터와의 호환

- Python이 만든 MP4(마지막 duration이 Python 규칙)는 재사용 경로에서 다시 쓰지 않는다. 검사는 프레임 수·시각·서명이고 마지막 duration을 보지 않는다(`tests/terminal_duration.rs`).
- 전체 JSON 비교(`integration/real_conversion_compare.py`, `integration/pipeline_contract.py`)는 `integration/terminal_rule.py`로 차이를 모델링한다. 입력(rlog, 원본 영상)만으로 Python 값(probe 추정식의 Python 이식, 0.05초)과 HARS 값(규칙의 독립 재구현)을 각각 계산하고, 두 출력이 **각자의 값과 1e-9 안에서 같아야** 한다. 그 뒤 위 표의 파생 필드만 맞추고, 나머지 모든 필드는 기존 1e-6 비교를 그대로 한다. `drive_summary`는 각 쪽의 telemetry로 Python 원본 함수를 다시 돌려 각 쪽 출력과 같은지 확인한 뒤 맞춘다.
- probe 추정식 이식은 `tests/ts_duration.rs`(Rust)와 `integration/terminal_rule.py`(Python) 두 곳에 있다. Rust 쪽은 libavformat의 실제 probe 값과 같음을, Python 쪽은 비교 때마다 Python 출력과 같음을 확인한다.

## 시험

- `src/terminal_duration.rs` 단위 시험, `tests/terminal_duration.rs`(합성): 일정 간격, 초기 흔들림, 중간 간격 변화, 말미 이상치, 다음 프레임 기록 있음/없음/이상, 짧은 스트림(1·2프레임, 21프레임 미만)에서 두 엔진의 마지막 duration·PTS/DTS가 같고 규칙대로인지 확인한다.
- `tests/ts_duration.rs`: Python 추정식 재현(libavformat probe와 같음)과, 다음 프레임 기록이 없을 때의 제품 규칙(중앙값)을 두 엔진에서 확인한다.
