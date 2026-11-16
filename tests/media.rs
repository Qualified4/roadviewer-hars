use roadviewer_hars::media;
use std::{
    path::Path,
    process::{Command, Stdio},
};
/// The host's development `ffprobe`, an independent reader of the product's MP4s.
fn ffprobe(path: &Path) -> serde_json::Value {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_packets",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}
/// Sample payloads as `shiguredo_mp4`'s demuxer reads them.
fn samples(path: &Path) -> Vec<Vec<u8>> {
    use shiguredo_mp4::demux::{Input, Mp4FileDemuxer};
    let data = std::fs::read(path).unwrap();
    let mut demuxer = Mp4FileDemuxer::new();
    demuxer.handle_input(Input {
        position: 0,
        data: &data,
    });
    demuxer.tracks().unwrap();
    let mut out = Vec::new();
    while let Some(s) = demuxer.next_sample().unwrap() {
        let at = s.data_offset as usize;
        out.push(data[at..at + s.data_size].to_vec());
    }
    out
}
fn fixture(path: &Path, hevc: bool) {
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
        cmd.args(["-bf", "0", "-g", "10", "-f", "mpegts"]);
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
#[tokio::test]
async fn h264_stream_copy_preserves_pictures_and_is_faststart() {
    let temp = tempfile::tempdir().unwrap();
    let ts = temp.path().join("qcamera.ts");
    let mp4 = temp.path().join("camera.mp4");
    fixture(&ts, false);
    let times = (0..10).map(|i| i as f64 / 10.).collect::<Vec<_>>();
    let info = media::remux(&ts, "qcamera.ts", &mp4, &times).await.unwrap();
    assert_eq!(info["streams"][0]["codec_name"], "h264");
    let info = ffprobe(&mp4);
    media::validate_times(&info, &times).unwrap();
    let restored = temp.path().join("restored.ts");
    media::reverse(&mp4, "qcamera.ts", &restored, 1.4)
        .await
        .unwrap();
    let jpeg = temp.path().join("thumbnail.jpg");
    media::thumbnail(&mp4, "camera.mp4", &jpeg).await.unwrap();
    assert!(std::fs::metadata(jpeg).unwrap().len() > 100);
}
#[tokio::test]
async fn hevc_stream_copy_uses_hvc1_and_checks_logged_clock() {
    let temp = tempfile::tempdir().unwrap();
    let hevc = temp.path().join("fcamera.hevc");
    let mp4 = temp.path().join("fcamera.mp4");
    fixture(&hevc, true);
    let times = (0..10).map(|i| i as f64 / 10.).collect::<Vec<_>>();
    let info = media::remux(&hevc, "fcamera.hevc", &mp4, &times)
        .await
        .unwrap();
    assert_eq!(info["streams"][0]["codec_tag_string"], "hvc1");
    let restored = temp.path().join("restored.hevc");
    media::reverse(&mp4, "fcamera.hevc", &restored, 0.)
        .await
        .unwrap();
    let mut wrong = times;
    wrong[5] += 0.01;
    let variable = temp.path().join("variable.mp4");
    media::remux(&hevc, "fcamera.hevc", &variable, &wrong)
        .await
        .unwrap();
    let info = ffprobe(&variable);
    media::validate_times(&info, &wrong).unwrap();
    assert!(
        media::validate_times(&info, &(0..10).map(|i| i as f64 / 10.).collect::<Vec<_>>()).is_err()
    );
}
#[tokio::test]
async fn camera_codec_mismatch_and_malformed_media_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let ts = temp.path().join("qcamera.ts");
    fixture(&ts, false);
    assert!(media::validate_upload(&ts, "fcamera.hevc").await.is_err());
    let playlist = temp.path().join("bad.mp4");
    std::fs::write(&playlist, b"#EXTM3U\nhttps://example.com/private\n").unwrap();
    assert!(
        media::validate_upload(&playlist, "camera.mp4")
            .await
            .is_err()
    );
    assert!(media::inspect(&playlist, "camera.mp4").await.is_err());
    assert!(media::inspect(&ts, "fcamera.hevc").await.is_err());
}

/// MP4 inputs another muxer wrote, as a conversion reads them: `moov` after `mdat`, an extra
/// audio track and hev1 (parameter sets also in the samples) are read with the clock the host's
/// ffprobe gives; an edit list that shifts the media is refused as unsupported, not misread.
#[tokio::test]
async fn other_muxers_mp4_inputs_are_read_or_refused_explicitly() {
    let temp = tempfile::tempdir().unwrap();
    let encode = |name: &str, args: &[&str]| {
        let path = temp.path().join(name);
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=160x96:rate=10")
            .args(args)
            .args(["-t", "1", "-threads", "1", "-pix_fmt", "yuv420p"]);
        assert!(
            cmd.arg(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        path
    };
    let h264 = ["-c:v", "libx264", "-bf", "0", "-g", "10"];
    let x265 = "pools=1:frame-threads=1:bframes=0:keyint=10:log-level=error";
    let cases = [
        (
            "moov-last.mp4",
            "camera.mp4",
            encode("moov-last.mp4", &[&h264[..], &["-f", "mp4"]].concat()),
        ),
        (
            "audio.mp4",
            "camera.mp4",
            encode(
                "audio.mp4",
                &[
                    &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=8000"][..],
                    &h264,
                    &["-c:a", "aac", "-shortest", "-movflags", "+faststart"],
                ]
                .concat(),
            ),
        ),
        (
            "hev1.mp4",
            "fcamera.mp4",
            encode(
                "hev1.mp4",
                &[
                    "-an",
                    "-c:v",
                    "libx265",
                    "-x265-params",
                    x265,
                    "-tag:v",
                    "hev1",
                    "-movflags",
                    "+faststart",
                ],
            ),
        ),
    ];
    for (name, kind, path) in cases {
        let read = media::inspect(&path, kind)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let probed = ffprobe(&path);
        let times = probed["packets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["codec_type"] == "video")
            .map(|p| p["pts_time"].as_str().unwrap().parse::<f64>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(times.len(), 10, "{name}");
        media::validate_times(&read.info, &times).unwrap();
        let output = temp.path().join(format!("unused-{name}"));
        media::remux(&path, kind, &output, &times).await.unwrap();
        assert!(!output.exists(), "{name}: an MP4 input is used as it is");
    }
    let bframes = encode(
        "bframes.mp4",
        &[
            "-an",
            "-c:v",
            "libx264",
            "-bf",
            "2",
            "-g",
            "10",
            "-movflags",
            "+faststart",
        ],
    );
    let error = media::inspect(&bframes, "camera.mp4").await.unwrap_err();
    assert_eq!(
        media::failure(&error).class,
        roadviewer_hars::pure_remux::FailureClass::Unsupported,
        "{error:#}"
    );
}

#[tokio::test]
async fn inspection_and_mux_hash_match_independent_ffmpeg_extraction() {
    for hevc in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (kind, format, filter) = if hevc {
            ("fcamera.hevc", "hevc", "hevc_mp4toannexb")
        } else {
            ("qcamera.ts", "h264", "h264_mp4toannexb")
        };
        let source = temp.path().join(kind);
        fixture(&source, hevc);
        let raw = media::inspect(&source, kind).await.unwrap();
        let extracted = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(&source)
            .args([
                "-map", "0:v:0", "-c:v", "copy", "-bsf:v", filter, "-f", format, "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(extracted.status.success());
        assert_eq!(
            raw.signature,
            media::annex_b_signature(&extracted.stdout, hevc).unwrap()
        );
        let times = (0..10).map(|i| i as f64 / 10.).collect::<Vec<_>>();
        let output = temp.path().join("checked.mp4");
        let info = media::remux(&source, kind, &output, &times).await.unwrap();
        let checked = media::inspect(&output, if hevc { "fcamera.mp4" } else { "camera.mp4" })
            .await
            .unwrap();
        assert_eq!(raw.signature, checked.signature);
        let probe = ffprobe(&output);
        for field in [
            "codec_name",
            "codec_tag_string",
            "width",
            "height",
            "has_b_frames",
            "time_base",
            "duration_ts",
        ] {
            assert_eq!(
                info["streams"][0][field], probe["streams"][0][field],
                "{field}"
            );
        }
        let a = info["packets"].as_array().unwrap();
        let b = probe["packets"].as_array().unwrap();
        assert_eq!(a.len(), b.len());
        for (a, b) in a.iter().zip(b) {
            assert_eq!(a["pts"], b["pts"]);
            assert_eq!(a["dts"], b["dts"]);
        }
        if hevc {
            for bytes in samples(&output) {
                let mut offset = 0;
                while offset < bytes.len() {
                    let size =
                        u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    assert!(size >= 2 && offset + size <= bytes.len());
                    assert!(
                        !matches!((bytes[offset] >> 1) & 63, 32..=34),
                        "hvc1 parameter sets must be out of band"
                    );
                    offset += size;
                }
            }
            let direct = temp.path().join("direct.mp4");
            let job = roadviewer_hars::pure_remux::Job {
                kind: kind.into(),
                times: times.clone(),
                terminal: 100_000,
                offset: 0,
            };
            assert_eq!(
                roadviewer_hars::pure_remux::remux(&source, &direct, &job)
                    .unwrap()
                    .input,
                raw.signature
            );
        }
    }
}
