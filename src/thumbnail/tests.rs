use super::scale::{Image, Plane, small};

fn flat(width: usize, height: usize, y: u8, c: u8) -> Image {
    let plane = |w: usize, h: usize, v: u8| Plane {
        width: w,
        height: h,
        data: vec![v; w * h],
    };
    Image {
        y: plane(width, height, y),
        u: plane(width.div_ceil(2), height.div_ceil(2), c),
        v: plane(width.div_ceil(2), height.div_ceil(2), c),
    }
}

#[test]
fn limited_range_is_stretched_to_full_range() {
    let black = small(&flat(526, 330, 16, 128), 320, false).unwrap();
    assert_eq!((black.y.width, black.y.height), (320, 200));
    assert_eq!((black.u.width, black.u.height), (160, 100));
    assert!(black.y.data.iter().all(|v| *v == 0));
    assert!(black.u.data.iter().chain(&black.v.data).all(|v| *v == 128));
    let white = small(&flat(1344, 760, 235, 128), 320, false).unwrap();
    assert_eq!((white.y.width, white.y.height), (320, 180));
    assert!(white.y.data.iter().all(|v| *v == 255));
}

#[test]
fn full_range_source_keeps_its_levels() {
    let grey = small(&flat(640, 400, 100, 60), 320, true).unwrap();
    assert!(grey.y.data.iter().all(|v| *v == 100));
    assert!(grey.u.data.iter().all(|v| *v == 60));
}

#[test]
fn jpeg_has_ffmpeg_quantiser_and_picture_size() {
    let jpeg = super::jpeg::encode(&small(&flat(526, 330, 90, 120), 320, false).unwrap()).unwrap();
    assert!(jpeg.starts_with(&[0xff, 0xd8]) && jpeg.ends_with(&[0xff, 0xd9]));
    let dqt = jpeg.windows(2).position(|w| w == [0xff, 0xdb]).unwrap();
    // FFmpeg mjpeg at qscale 4: DC 8, then the MPEG-1 intra matrix halved, in zigzag order.
    assert_eq!(&jpeg[dqt + 5..dqt + 13], &[8, 8, 8, 9, 8, 9, 11, 11]);
    let sof = jpeg.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    assert_eq!(&jpeg[sof + 5..sof + 9], &[0, 200, 1, 64]);
}

/// A camera-like stream: no B-frames and an IDR picture every `gop` frames (openpilot's qcamera
/// has 15, its HEVC cameras 20).
fn keyframed(path: &std::path::Path, hevc: bool, gop: u32) {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .args([
            "testsrc2=size=526x330:rate=20",
            "-t",
            "7",
            "-an",
            "-threads",
            "1",
        ])
        .args(["-pix_fmt", "yuv420p"]);
    if hevc {
        let params = format!("pools=1:frame-threads=1:bframes=0:keyint={gop}:min-keyint={gop}");
        cmd.args(["-c:v", "libx265", "-tag:v", "hvc1", "-x265-params"])
            .arg(params + ":scenecut=0:open-gop=0:log-level=error");
    } else {
        let gop = gop.to_string();
        cmd.args([
            "-c:v",
            "libx264",
            "-bf",
            "0",
            "-g",
            &gop,
            "-keyint_min",
            &gop,
        ])
        .args(["-sc_threshold", "0", "-forced-idr", "1"]);
    }
    let status = cmd
        .args(["-movflags", "+faststart"])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "fixture encoder failed");
}

#[test]
fn skipping_to_an_idr_picture_gives_the_pictures_of_decoding_every_sample() {
    for (hevc, gop) in [(false, 15), (true, 20)] {
        let temp = tempfile::tempdir().unwrap();
        let mp4 = temp.path().join("camera.mp4");
        keyframed(&mp4, hevc, gop);
        let mut all = Vec::new();
        super::video::decode(&mp4, 140, |f| {
            all.push((f.time, f.image));
            Ok(Some(f64::NEG_INFINITY))
        })
        .unwrap();
        // Between two IDR pictures, exactly on one, and the last one of the window.
        let wanted = [1.3, 2.0, 3.65, 6.0];
        let mut some = Vec::new();
        let mut met = 0;
        super::video::decode(&mp4, 140, |f| {
            while met < wanted.len() && f.time >= wanted[met] {
                met += 1;
            }
            some.push((f.time, f.image));
            Ok(wanted.get(met).copied())
        })
        .unwrap();
        assert!(
            some.len() < all.len() / 2,
            "{} of {}",
            some.len(),
            all.len()
        );
        for (time, image) in &some {
            assert!(all.iter().any(|(t, i)| t == time && i == image), "{time}");
        }
        for w in &wanted {
            let first = |v: &[(f64, super::scale::Image)]| v.iter().position(|(t, _)| t >= w);
            let (a, s) = (first(&all).unwrap(), first(&some).unwrap());
            assert!(all[a] == some[s], "hevc {hevc}, wanted {w}");
        }
    }
}

#[test]
fn unreadable_inputs_fail_without_a_picture() {
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join("thumbnail.jpg");
    for bytes in [
        &b""[..],
        b"\0\0\0\x08ftyp",
        b"\0\0\0\x10moovgarbage!",
        &[0xffu8; 4096],
    ] {
        let source = temp.path().join("camera.mp4");
        std::fs::write(&source, bytes).unwrap();
        assert!(super::make(&source, &out).is_err());
        assert!(!out.exists());
    }
}
