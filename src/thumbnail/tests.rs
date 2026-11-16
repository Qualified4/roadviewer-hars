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
