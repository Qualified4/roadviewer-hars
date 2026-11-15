#![cfg(feature = "pure-remux")]
use roadviewer_hars::{experimental::pure_remux, media};
use std::{
    path::Path,
    process::{Command, Stdio},
};

fn fixture(path: &Path, hevc: bool, bframes: u32) {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x96:rate=10",
        "-t",
        "1",
        "-an",
        "-threads",
        "1",
        "-c:v",
        if hevc { "libx265" } else { "libx264" },
        "-pix_fmt",
        "yuv420p",
    ]);
    if hevc {
        cmd.args([
            "-x265-params",
            "pools=1:frame-threads=1:bframes=0:keyint=10:log-level=error",
            "-f",
            "hevc",
        ]);
    } else {
        cmd.args(["-bf", &bframes.to_string(), "-g", "10", "-f", "mpegts"]);
    }
    assert!(
        cmd.arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "fixture encoder failed"
    );
}

fn clock() -> Vec<f64> {
    (0..10).map(|i| i as f64 / 10.).collect()
}

fn decode_errors(path: &Path) -> (String, usize) {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-f", "framemd5", "-"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let frames = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .count();
    (String::from_utf8_lossy(&out.stderr).into_owned(), frames)
}

fn packets(info: &serde_json::Value) -> Vec<(i64, i64)> {
    info["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["pts"].as_i64().unwrap(), p["dts"].as_i64().unwrap()))
        .collect()
}

fn no_leftovers(dir: &Path, keep: &[&str]) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(keep.contains(&name.as_str()), "left behind {name}");
    }
}

#[tokio::test]
async fn hevc_matches_ffmpeg_engine_pictures_clock_and_layout() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("fcamera.hevc");
    fixture(&source, true, 0);
    let mut times = clock();
    times[5] += 0.01;
    let pure = temp.path().join("pure.mp4");
    let report = pure_remux::remux(&source, "fcamera.hevc", &pure, &times, true).unwrap();
    let raw = media::inspect(&source, "fcamera.hevc").await.unwrap();
    assert_eq!(report.signature, raw.signature);
    assert_eq!(report.frames, 10);
    let independent = media::inspect(&pure, "fcamera.mp4").await.unwrap();
    assert_eq!(independent.signature, raw.signature);
    assert_eq!(independent.info["streams"][0]["codec_tag_string"], "hvc1");
    let reference = temp.path().join("ffmpeg.mp4");
    media::remux(&source, "fcamera.hevc", &reference, &times)
        .await
        .unwrap();
    let a = media::probe(&pure, "fcamera.mp4", true).await.unwrap();
    let b = media::probe(&reference, "fcamera.mp4", true).await.unwrap();
    media::validate_times(&a, &times).unwrap();
    assert_eq!(packets(&a), packets(&b));
    assert_eq!(a["streams"][0]["time_base"], b["streams"][0]["time_base"]);
    assert_eq!(
        a["streams"][0]["duration_ts"],
        b["streams"][0]["duration_ts"]
    );
    let checked = pure_remux::inspect(&pure, true).unwrap();
    assert_eq!(checked.keyframes, vec![0]);
    let (errors, frames) = decode_errors(&pure);
    assert_eq!((errors.as_str(), frames), ("", 10));
    no_leftovers(temp.path(), &["fcamera.hevc", "pure.mp4", "ffmpeg.mp4"]);
}

#[tokio::test]
async fn h264_ts_matches_ffmpeg_engine_with_and_without_b_frames() {
    for bframes in [0, 2] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("qcamera.ts");
        fixture(&source, false, bframes);
        let raw = media::inspect(&source, "qcamera.ts").await.unwrap();
        let scale = 90000.;
        let mut pts = packets(&raw.info)
            .iter()
            .map(|p| p.0 as f64 / scale)
            .collect::<Vec<_>>();
        pts.sort_by(f64::total_cmp);
        let times = pts.iter().map(|p| p - pts[0]).collect::<Vec<_>>();
        let pure = temp.path().join("pure.mp4");
        let report = pure_remux::remux(&source, "qcamera.ts", &pure, &times, true).unwrap();
        assert_eq!(report.signature, raw.signature, "bframes={bframes}");
        let independent = media::inspect(&pure, "camera.mp4").await.unwrap();
        assert_eq!(independent.signature, raw.signature);
        let a = media::probe(&pure, "camera.mp4", true).await.unwrap();
        media::validate_times(&a, &times).unwrap();
        let (errors, frames) = decode_errors(&pure);
        assert_eq!((errors.as_str(), frames), ("", 10), "bframes={bframes}");
        let checked = pure_remux::inspect(&pure, false).unwrap();
        if bframes == 0 {
            let reference = temp.path().join("ffmpeg.mp4");
            media::remux(&source, "qcamera.ts", &reference, &times)
                .await
                .unwrap();
            let b = media::probe(&reference, "camera.mp4", true).await.unwrap();
            assert_eq!(packets(&a), packets(&b));
            assert!(
                checked
                    .pts
                    .iter()
                    .zip(&checked.dts)
                    .all(|(p, d)| *p == *d as i64)
            );
        } else {
            assert_eq!(checked.pts[0], 0);
            assert!(
                checked
                    .pts
                    .iter()
                    .zip(&checked.dts)
                    .any(|(p, d)| *p < *d as i64),
                "negative ctts expected"
            );
        }
    }
}

fn units(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut begin = None;
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i..i + 3] == [0, 0, 1] {
            if let Some(b) = begin {
                out.push(&bytes[b..i]);
            }
            begin = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(b) = begin {
        out.push(&bytes[b..]);
    }
    out
}

fn rejects(source: &Path, kind: &str, times: &[f64]) -> String {
    let dir = source.parent().unwrap();
    let output = dir.join("out.mp4");
    let error = pure_remux::remux(source, kind, &output, times, true)
        .unwrap_err()
        .to_string();
    assert!(!output.exists(), "{error}");
    let leftovers = std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("tmp")
        })
        .count();
    assert_eq!(leftovers, 0, "{error}");
    error
}

#[test]
fn damaged_and_boundary_inputs_fail_without_panic_or_output() {
    let temp = tempfile::tempdir().unwrap();
    let hevc = temp.path().join("fcamera.hevc");
    fixture(&hevc, true, 0);
    let ts = temp.path().join("qcamera.ts");
    fixture(&ts, false, 0);
    let raw_hevc = std::fs::read(&hevc).unwrap();
    let raw_ts = std::fs::read(&ts).unwrap();
    let dir = temp.path().join("cases");
    std::fs::create_dir(&dir).unwrap();
    let case = |name: &str, bytes: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    };
    let times = clock();

    let cut = case("cut.ts", &raw_ts[..raw_ts.len() - 100]);
    assert!(rejects(&cut, "qcamera.ts", &times).contains("MPEG-TS"));

    let whole = raw_ts.len() / 188 / 2 * 188;
    let half = case("half.ts", &raw_ts[..whole]);
    rejects(&half, "qcamera.ts", &times);

    let mut lost = Vec::new();
    let mut seen = 0;
    for packet in raw_ts.chunks(188) {
        let pid = u16::from_be_bytes([packet[1] & 0x1f, packet[2]]);
        if pid == 0x100 {
            seen += 1;
            if seen == 20 {
                continue;
            }
        }
        lost.extend_from_slice(packet);
    }
    let lost = case("lost.ts", &lost);
    assert!(rejects(&lost, "qcamera.ts", &times).contains("continuity"));

    let mut stripped = Vec::new();
    for unit in units(&raw_hevc) {
        if !matches!((unit[0] >> 1) & 63, 32..=34) {
            stripped.extend_from_slice(&[0, 0, 0, 1]);
            stripped.extend_from_slice(unit);
        }
    }
    let stripped = case("noparams.hevc", &stripped);
    assert!(rejects(&stripped, "fcamera.hevc", &times).contains("decoder configuration"));

    let mut huge = vec![0, 0, 0, 1, 0x02, 0x01];
    huge.resize(33 * 1024 * 1024, 0x55);
    let huge = case("huge.hevc", &huge);
    assert!(rejects(&huge, "fcamera.hevc", &times).contains("bounded memory"));

    let empty = case("empty.hevc", b"");
    rejects(&empty, "fcamera.hevc", &times);
    let empty_ts = case("empty.ts", b"");
    rejects(&empty_ts, "qcamera.ts", &times);
    let garbage = case("garbage.hevc", b"#EXTM3U\nhttps://example.com/private\n");
    assert!(rejects(&garbage, "fcamera.hevc", &times).contains("Annex B"));

    let hevc_copy = case("count.hevc", &raw_hevc);
    assert!(rejects(&hevc_copy, "fcamera.hevc", &times[..9]).contains("count"));
    let ts_copy = case("count.ts", &raw_ts);
    rejects(&ts_copy, "qcamera.ts", &times[..9]);
    let mut shifted = times.clone();
    shifted[4] += 0.01;
    assert!(rejects(&ts_copy, "qcamera.ts", &shifted).contains("timestamps"));
    assert!(rejects(&ts_copy, "fcamera.hevc", &times).contains("Annex B"));
    rejects(&hevc_copy, "qcamera.ts", &times);
}

#[test]
fn mutated_inputs_never_panic() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for (kind, hevc) in [("qcamera.ts", false), ("fcamera.hevc", true)] {
        let source = temp.path().join(kind);
        fixture(&source, hevc, 0);
        let raw = std::fs::read(&source).unwrap();
        let times = clock();
        for round in 0..150 {
            let mut bytes = raw.clone();
            // Header-heavy prefix mutations hit PAT/PMT/PES/parameter-set parsers.
            let span = if round % 2 == 0 {
                4096.min(bytes.len())
            } else {
                bytes.len()
            };
            for _ in 0..1 + next() % 8 {
                let at = (next() as usize) % span;
                bytes[at] = next() as u8;
            }
            if round % 5 == 0 {
                bytes.truncate((next() as usize) % bytes.len());
            }
            let input = temp.path().join(format!("m{round}-{kind}"));
            std::fs::write(&input, &bytes).unwrap();
            let output = temp.path().join(format!("m{round}.mp4"));
            let result =
                std::panic::catch_unwind(|| pure_remux::remux(&input, kind, &output, &times, true));
            assert!(result.is_ok(), "panic on round {round} of {kind}");
            if let Ok(Ok(_)) = result {
                let (errors, frames) = decode_errors(&output);
                assert!(frames > 0, "{errors}");
            }
        }
    }
}

#[test]
fn muxer_switches_to_co64_beyond_4_gib() {
    use shiguredo_mp4::{
        TrackKind,
        bitstream::h265::{
            H265ConstantFrameRate, H265SampleEntryConfig, LengthSize, build_hvc1_box,
        },
        boxes::SampleEntry,
        mux::{Mp4FileMuxer, Sample},
    };
    let temp = tempfile::tempdir().unwrap();
    let hevc = temp.path().join("fcamera.hevc");
    fixture(&hevc, true, 0);
    let raw = std::fs::read(&hevc).unwrap();
    let set = |kind: u8| {
        units(&raw)
            .into_iter()
            .filter(|u| (u[0] >> 1) & 63 == kind)
            .take(1)
            .map(|u| u.to_vec())
            .collect::<Vec<_>>()
    };
    let entry = build_hvc1_box(
        &set(32),
        &set(33),
        &set(34),
        &H265SampleEntryConfig {
            length_size: LengthSize::FourBytes,
            avg_frame_rate: 0,
            constant_frame_rate: H265ConstantFrameRate::Unknown,
        },
    )
    .unwrap();
    let mut muxer = Mp4FileMuxer::new().unwrap();
    let head = muxer.initial_boxes_bytes().len() as u64;
    muxer.advance_position(5 << 30).unwrap();
    muxer
        .append_sample(&Sample {
            track_kind: TrackKind::Video,
            sample_entry: Some(SampleEntry::Hvc1(entry)),
            keyframe: true,
            timescale: std::num::NonZeroU32::new(90000).unwrap(),
            duration: 4500,
            composition_time_offset: None,
            data_offset: head + (5 << 30),
            data_size: 16,
        })
        .unwrap();
    let boxes = muxer.finalize().unwrap();
    let bytes = boxes
        .offset_and_bytes_pairs()
        .flat_map(|(_, b)| b.to_vec())
        .collect::<Vec<_>>();
    assert!(bytes.windows(4).any(|w| w == b"co64"));
    assert!(!bytes.windows(4).any(|w| w == b"stco"));
}
