//! The pure original-format export against the host's development `ffmpeg` CLI doing the export
//! the product once ran (an independent oracle), and its failures on damaged MP4s. Fixtures are
//! made with the same CLI (development only).
use roadviewer_hars::{
    media,
    pure_remux::FailureClass,
    pure_reverse::{self, Job, Reply},
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn encode(path: &Path, hevc: bool, extra: &[&str]) {
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
        "2",
        "-an",
        "-threads",
        "1",
        "-pix_fmt",
        "yuv420p",
    ]);
    if hevc {
        cmd.args([
            "-c:v",
            "libx265",
            "-x265-params",
            "pools=1:frame-threads=1:bframes=0:keyint=10:log-level=error",
            "-f",
            "hevc",
        ]);
    } else {
        cmd.args(["-c:v", "libx264", "-g", "10"]).args(extra);
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

/// A converted camera MP4 as the product makes it: `(mp4, original kind)`.
async fn converted(dir: &Path, hevc: bool) -> (PathBuf, &'static str) {
    let (kind, mp4) = if hevc {
        ("fcamera.hevc", "fcamera.mp4")
    } else {
        ("qcamera.ts", "camera.mp4")
    };
    let source = dir.join(kind);
    encode(&source, hevc, &["-bf", "0", "-f", "mpegts"]);
    let times = (0..20).map(|i| i as f64 / 10.).collect::<Vec<_>>();
    media::remux(&source, kind, &dir.join(mp4), &times)
        .await
        .unwrap();
    (dir.join(mp4), kind)
}

/// The stream copy the product ran before the pure export: `ffmpeg -c:v copy` with the
/// `*_mp4toannexb` filter and, for TS, the source clock offset.
fn cli_export(mp4: &Path, kind: &str, output: &Path, offset: f64) {
    let ts = kind == "qcamera.ts";
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-v", "error", "-nostdin", "-y", "-f", "mov", "-copyts", "-i",
    ])
    .arg(mp4)
    .args(["-map", "0:v:0", "-an", "-c:v", "copy", "-bsf:v"])
    .arg(if ts {
        "h264_mp4toannexb"
    } else {
        "hevc_mp4toannexb"
    });
    if ts {
        cmd.args(["-mpegts_copyts", "1", "-muxdelay", "0", "-output_ts_offset"])
            .arg(offset.to_string());
    }
    assert!(
        cmd.args(["-f", if ts { "mpegts" } else { "hevc" }])
            .arg(output)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn pure_export_holds_what_the_cli_export_holds() {
    for hevc in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (mp4, kind) = converted(temp.path(), hevc).await;
        let (pure, ffmpeg) = (temp.path().join("pure"), temp.path().join("ffmpeg"));
        let offset = 118.155_244_444_444_45;
        media::reverse(&mp4, kind, &pure, offset).await.unwrap();
        cli_export(&mp4, kind, &ffmpeg, offset);
        let (a, b) = (
            std::fs::read(&pure).unwrap(),
            std::fs::read(&ffmpeg).unwrap(),
        );
        if hevc {
            assert_eq!(a, b, "raw HEVC exports differ");
        } else {
            let (a, b) = (
                media::inspect(&pure, kind).await.unwrap(),
                media::inspect(&ffmpeg, kind).await.unwrap(),
            );
            assert_eq!(a.signature, b.signature);
            let clocks = |v: &serde_json::Value| {
                v["packets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| (p["pts"].as_i64().unwrap(), p["dts"].as_i64().unwrap()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(clocks(&a.info), clocks(&b.info));
            assert_eq!(clocks(&a.info)[0].0, 10_633_972);
            assert_eq!(clocks(&a.info).len(), 20);
        }
        // The export of an export is the same file: nothing in it depends on the run.
        let again = temp.path().join("again");
        media::reverse(&mp4, kind, &again, offset).await.unwrap();
        assert_eq!(std::fs::read(&again).unwrap(), a);
    }
}

#[tokio::test]
async fn an_edit_list_the_engine_does_not_copy_is_refused_as_unsupported() {
    let temp = tempfile::tempdir().unwrap();
    let (ts, mp4) = (temp.path().join("b.ts"), temp.path().join("camera.mp4"));
    encode(&ts, false, &["-bf", "2", "-f", "mpegts"]);
    // libavformat's copy of B-frames into MP4 starts its edit list at the first composition offset.
    assert!(
        Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-i"])
            .arg(&ts)
            .args(["-c", "copy", "-movflags", "+faststart", "-f", "mp4"])
            .arg(&mp4)
            .status()
            .unwrap()
            .success()
    );
    // The export the product ran before refused such a file too (its clock check did not follow
    // the edit list); now the product's export says why, and writes nothing.
    let output = temp.path().join("export.ts");
    let error = media::reverse(&mp4, "qcamera.ts", &output, 0.)
        .await
        .unwrap_err();
    assert_eq!(media::failure(&error).class, FailureClass::Unsupported);
    assert!(!output.exists());
    let direct = pure_reverse::reverse(
        &mp4,
        &temp.path().join("direct.ts"),
        &Job {
            kind: "qcamera.ts".into(),
            offset: 0,
        },
    );
    let Reply::Failed(failure) = Reply::from(direct) else {
        panic!("an edit list must not be copied")
    };
    assert_eq!(failure.class, FailureClass::Unsupported);
    assert!(!temp.path().join("direct.ts").exists());
}

fn find(data: &[u8], tag: &[u8; 4]) -> usize {
    data.windows(4).position(|w| w == tag).expect("box") - 4
}

/// Damaged copies of a real export input: each fails with a class, leaves no output and panics never.
#[tokio::test]
async fn damaged_mp4s_fail_classified_without_output() {
    let temp = tempfile::tempdir().unwrap();
    for hevc in [false, true] {
        let (mp4, kind) = converted(temp.path(), hevc).await;
        let good = std::fs::read(&mp4).unwrap();
        let mut cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("garbage", b"#EXTM3U\nhttps://example.com/x\n".to_vec()),
            ("half", good[..good.len() / 2].to_vec()),
            ("moov only", good[..find(&good, b"mdat")].to_vec()),
        ];
        let mut sized = good.clone();
        let at = find(&sized, b"moov");
        sized[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        cases.push(("moov size past the file", sized));
        let mut counted = good.clone();
        let at = find(&counted, b"stsz") + 16;
        counted[at..at + 4].copy_from_slice(&0x7fff_ffffu32.to_be_bytes());
        cases.push(("sample count past the table", counted));
        let mut large = good.clone();
        let at = find(&large, b"stsz") + 20;
        large[at..at + 4].copy_from_slice(&0x7fff_0000u32.to_be_bytes());
        cases.push(("sample size past the file", large));
        let mut none = good.clone();
        let at = find(&none, b"stsz") + 16;
        none[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
        cases.push(("no samples", none));
        let mut offsets = good.clone();
        let at = find(&offsets, b"stco") + 16;
        offsets[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
        cases.push(("chunk offset before mdat", offsets));
        let mut lengths = good.clone();
        let at = find(&lengths, b"mdat") + 8;
        lengths[at..at + 4].copy_from_slice(&0x00ff_ffffu32.to_be_bytes());
        cases.push(("NAL length past the sample", lengths));
        cases.push(("other codec", Vec::new()));
        for (name, bytes) in cases {
            let input = temp.path().join("damaged.mp4");
            let kind = if name == "other codec" {
                std::fs::copy(&mp4, &input).unwrap();
                if hevc { "qcamera.ts" } else { "fcamera.hevc" }
            } else {
                std::fs::write(&input, bytes).unwrap();
                kind
            };
            let output = temp.path().join("restored");
            let result = std::panic::catch_unwind(|| {
                Reply::from(pure_reverse::reverse(
                    &input,
                    &output,
                    &Job {
                        kind: kind.into(),
                        offset: 0,
                    },
                ))
            });
            let Ok(Reply::Failed(failure)) = result else {
                panic!("{name} ({kind}) did not fail cleanly");
            };
            assert!(
                matches!(
                    failure.class,
                    FailureClass::Invalid | FailureClass::Unsupported
                ),
                "{name}: {failure:?}"
            );
            assert!(!output.exists(), "{name} left an output");
        }
    }
}

#[tokio::test]
async fn offsets_outside_the_ts_clock_are_refused_and_the_export_kind_is_checked() {
    assert_eq!(pure_reverse::offset_ticks(1.5).unwrap(), 135_000);
    for bad in [-0.5, f64::NAN, f64::INFINITY, 1e9] {
        assert_eq!(
            pure_reverse::offset_ticks(bad).unwrap_err().class,
            FailureClass::Unsupported
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let (mp4, _) = converted(temp.path(), false).await;
    assert!(
        media::reverse(&mp4, "rlog.zst", &temp.path().join("x"), 0.)
            .await
            .is_err()
    );
}
