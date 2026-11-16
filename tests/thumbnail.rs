use std::{
    path::Path,
    process::{Command, Stdio},
};

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
        "testsrc2=size=526x330:rate=20",
        "-t",
        "7",
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
            "-tag:v",
            "hvc1",
            "-x265-params",
            "pools=1:frame-threads=1:bframes=0:log-level=error",
        ]);
    } else {
        cmd.args(["-c:v", "libx264", "-bf", "0"]);
    }
    assert!(
        cmd.args(["-movflags", "+faststart"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "fixture encoder failed"
    );
}

fn jpeg_size(jpeg: &[u8]) -> (u16, u16) {
    let sof = jpeg.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    let be = |i: usize| u16::from_be_bytes([jpeg[sof + i], jpeg[sof + i + 1]]);
    (be(7), be(5))
}

#[test]
fn h264_and_hevc_thumbnails_pick_a_later_frame_and_record_their_score() {
    for hevc in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mp4 = temp.path().join("camera.mp4");
        fixture(&mp4, hevc);
        let out = temp.path().join("thumbnail.jpg");
        let result = roadviewer_hars::thumbnail::make(&mp4, &out).unwrap();
        let second = result["second"].as_u64().unwrap();
        assert!((1..=5).contains(&second), "{result}");
        let jpeg = std::fs::read(&out).unwrap();
        assert!(jpeg.starts_with(&[0xff, 0xd8]) && jpeg.ends_with(&[0xff, 0xd9]));
        assert_eq!(jpeg_size(&jpeg), (320, 200));
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(temp.path().join("thumbnail.json")).unwrap())
                .unwrap();
        assert_eq!(meta["version"], 2);
        assert_eq!(meta["score"], result["score"]);
        assert!(!temp.path().join("thumbnail.jpg.tmp").exists());
    }
}

#[test]
fn a_file_cut_short_keeps_the_frames_before_its_end() {
    let temp = tempfile::tempdir().unwrap();
    let mp4 = temp.path().join("camera.mp4");
    fixture(&mp4, false);
    let bytes = std::fs::read(&mp4).unwrap();
    let cut = temp.path().join("cut.mp4");
    std::fs::write(&cut, &bytes[..bytes.len() / 4]).unwrap();
    let out = temp.path().join("thumbnail.jpg");
    let result = roadviewer_hars::thumbnail::make(&cut, &out).unwrap();
    assert!(result["second"].as_u64().unwrap() < 5, "{result}");
    assert!(out.exists());
}
