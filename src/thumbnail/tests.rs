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
    encoded(path, hevc, gop, "");
}

/// `keyframed` with more x264 or x265 options, which win over the camera-like ones.
fn encoded(path: &std::path::Path, hevc: bool, gop: u32, extra: &str) {
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
        let extra = if extra.is_empty() {
            String::new()
        } else {
            format!(":{extra}")
        };
        cmd.args(["-c:v", "libx265", "-tag:v", "hvc1", "-x265-params"])
            .arg(params + ":scenecut=0:open-gop=0:log-level=error" + &extra);
    } else {
        if !extra.is_empty() {
            cmd.args(["-x264-params", extra]);
        }
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
        super::video::decode(&mp4, 140, true, |f| {
            all.push((f.time, f.image));
            Ok(Some(f64::NEG_INFINITY))
        })
        .unwrap();
        // Between two IDR pictures, exactly on one, and the last one of the window.
        let wanted = [1.3, 2.0, 3.65, 6.0];
        let mut some = Vec::new();
        let mut met = 0;
        super::video::decode(&mp4, 140, true, |f| {
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

/// Payload range of the box at `path` (32-bit sizes, as FFmpeg writes these files).
fn find(mp4: &[u8], path: &[&[u8; 4]]) -> (usize, usize) {
    let (mut start, mut end) = (0, mp4.len());
    for kind in path {
        let mut at = start;
        loop {
            assert!(at + 8 <= end, "no {} box", String::from_utf8_lossy(*kind));
            let size = u32::from_be_bytes(mp4[at..at + 4].try_into().unwrap()) as usize;
            if &mp4[at + 4..at + 8] == *kind {
                end = at + size;
                start = at
                    + 8
                    + match *kind {
                        b"stsd" => 8,
                        b"avc1" | b"hvc1" => 78,
                        _ => 0,
                    };
                break;
            }
            at += size;
        }
    }
    (start, end)
}

const STBL: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];

fn table(mp4: &[u8], kind: &[u8; 4], skip: usize, width: usize) -> (usize, Vec<u32>) {
    let (start, _) = find(mp4, &[STBL.as_slice(), &[kind]].concat());
    let word = |at: usize| u32::from_be_bytes(mp4[at..at + 4].try_into().unwrap());
    let at = start + 4 + skip;
    let entries = (0..word(at) as usize * width)
        .map(|i| word(at + 4 + 4 * i))
        .collect();
    (at + 4, entries)
}

/// The file with each sample's NAL units replaced by `edit(index, units)`; the samples move
/// to a new `mdat` at the end, the sample sizes and chunk offsets are rewritten in place.
fn rewrite(mp4: &[u8], mut edit: impl FnMut(usize, Vec<Vec<u8>>) -> Vec<Vec<u8>>) -> Vec<u8> {
    let (sizes_at, sizes) = table(mp4, b"stsz", 4, 1);
    assert!(!sizes.is_empty(), "constant sample size");
    let (chunks_at, chunks) = table(mp4, b"stco", 0, 1);
    let (_, runs) = table(mp4, b"stsc", 0, 3);
    let mut out = mp4.to_vec();
    let mut data = Vec::new();
    let base = mp4.len() + 8;
    let mut index = 0;
    for (c, &offset) in chunks.iter().enumerate() {
        let per = runs.chunks(3).rfind(|r| r[0] as usize <= c + 1).unwrap()[1];
        out[chunks_at + 4 * c..chunks_at + 4 * c + 4]
            .copy_from_slice(&((base + data.len()) as u32).to_be_bytes());
        let mut at = offset as usize;
        for _ in 0..per {
            let Some(&size) = sizes.get(index) else {
                break;
            };
            let mut rest = &mp4[at..at + size as usize];
            at += size as usize;
            let mut nals = Vec::new();
            while !rest.is_empty() {
                let n = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
                nals.push(rest[4..4 + n].to_vec());
                rest = &rest[4 + n..];
            }
            let before = data.len();
            for nal in edit(index, nals) {
                data.extend((nal.len() as u32).to_be_bytes());
                data.extend(nal);
            }
            out[sizes_at + 4 * index..sizes_at + 4 * index + 4]
                .copy_from_slice(&((data.len() - before) as u32).to_be_bytes());
            index += 1;
        }
    }
    out.extend(((data.len() + 8) as u32).to_be_bytes());
    out.extend(b"mdat");
    out.extend(data);
    out
}

/// What a caller is given with and without skipping, which must not differ: the thumbnail (its
/// result or error, JPEG and JSON) and the pictures a caller says it needs when it keeps wanting
/// a time for a few pictures after reaching it (`make` moves on at once). Returns how the
/// skipping thumbnail went.
fn same(mp4: &std::path::Path) -> super::video::Work {
    let temp = tempfile::tempdir().unwrap();
    let mut work = None;
    let mut thumbnails = Vec::new();
    for skip in [true, false] {
        let out = temp.path().join(format!("{skip}.jpg"));
        let result = super::make_with(mp4, &out, skip).map(|(value, w)| {
            if skip {
                work = Some(w);
            }
            value
        });
        let read = |p: std::path::PathBuf| std::fs::read(p).ok();
        thumbnails.push((
            result.map_err(|e| format!("{e:#}")),
            read(out.clone()),
            read(out.with_extension("json")),
        ));
    }
    assert!(thumbnails[0] == thumbnails[1], "thumbnail differs");
    let wanted = [1.3, 2.0, 3.65, 6.0];
    let needed = |skip| {
        let mut seen = Vec::new();
        let mut met = 0;
        let result = super::video::decode(mp4, 140, skip, |f| {
            while met < wanted.len() && f.time >= wanted[met] + 0.12 {
                met += 1;
            }
            if met < wanted.len() && f.time >= wanted[met] {
                seen.push((f.time, f.image));
            }
            Ok(wanted.get(met).copied())
        })
        .map(|_| ())
        .map_err(|e| format!("{e:#}"));
        (result, seen)
    };
    assert!(needed(true) == needed(false), "needed pictures differ");
    work.unwrap_or_default()
}

fn h264_pps(mp4: &[u8]) -> Vec<u8> {
    let (at, _) = find(
        mp4,
        &[STBL.as_slice(), &[b"stsd", b"avc1", b"avcC"]].concat(),
    );
    let mut p = at + 6;
    for _ in 0..mp4[at + 5] & 31 {
        p += 2 + u16::from_be_bytes([mp4[p], mp4[p + 1]]) as usize;
    }
    assert_eq!(mp4[p], 1);
    let n = u16::from_be_bytes([mp4[p + 1], mp4[p + 2]]) as usize;
    mp4[p + 3..p + 3 + n].to_vec()
}

#[test]
fn skipping_matches_decoding_every_sample() {
    let temp = tempfile::tempdir().unwrap();
    let mp4 = temp.path().join("camera.mp4");
    // HEVC with an IDR picture every 21 or 30 frames: a candidate right before an IDR picture,
    // and a flushed stretch with no IDR picture before the next wanted time, decoded again.
    for (hevc, gop) in [(false, 15), (true, 20), (true, 21), (true, 30)] {
        keyframed(&mp4, hevc, gop);
        let work = same(&mp4);
        assert!(!work.restarts.is_empty(), "{hevc} {gop}: {work:?}");
        assert_eq!(work.flushes > 0, hevc, "{gop}: {work:?}");
        if gop > 20 {
            assert!(work.replays > 0, "{gop}: {work:?}");
        }
    }
}

#[test]
fn an_in_band_parameter_set_other_than_the_tracks_turns_skipping_off() {
    let temp = tempfile::tempdir().unwrap();
    let (base, other) = (temp.path().join("a.mp4"), temp.path().join("b.mp4"));
    keyframed(&base, false, 15);
    encoded(&other, false, 15, "chroma-qp-offset=10");
    let pps = h264_pps(&std::fs::read(&other).unwrap());
    let base = std::fs::read(&base).unwrap();
    assert_ne!(pps, h264_pps(&base));
    let mp4 = temp.path().join("camera.mp4");
    std::fs::write(
        &mp4,
        rewrite(&base, |i, mut nals| {
            if i == 0 {
                nals.insert(0, pps.clone());
            }
            nals
        }),
    )
    .unwrap();
    let work = same(&mp4);
    assert!(work.restarts.is_empty(), "{work:?}");
}

#[test]
fn h264_restarts_only_after_a_finished_picture_at_a_reference_i_picture() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("a.mp4");
    encoded(&base, false, 15, "slices=2");
    let base = std::fs::read(&base).unwrap();
    let mp4 = temp.path().join("camera.mp4");
    std::fs::write(&mp4, rewrite(&base, |_, nals| nals)).unwrap();
    assert!(same(&mp4).restarts.contains(&30));
    // The second slice of the picture before the IDR picture at 1.5 s lost: `rusty_h264` then
    // refuses that IDR picture when every sample is decoded.
    std::fs::write(
        &mp4,
        rewrite(&base, |i, nals| {
            let mut slices = 0;
            nals.into_iter()
                .filter(|n| {
                    slices += usize::from(n[0] & 31 == 1);
                    !(i == 29 && n[0] & 31 == 1 && slices == 2)
                })
                .collect()
        }),
    )
    .unwrap();
    let work = same(&mp4);
    assert!(!work.restarts.contains(&30), "{work:?}");
    // An IDR slice that is no reference, or that is no I slice.
    for change in [0x9f, 0xff] {
        std::fs::write(
            &mp4,
            rewrite(&base, |i, nals| {
                nals.into_iter()
                    .map(|mut n| {
                        if i == 30 && n[0] & 31 == 5 && (change == 0x9f || n[1] & 0x80 != 0) {
                            if change == 0x9f {
                                n[0] &= 0x9f;
                            } else {
                                // first_mb_in_slice 0, then slice_type 7 (I) becomes 8 (SP).
                                assert_eq!(n[1], 0b1000_1000);
                                n[1] |= 1;
                            }
                        }
                        n
                    })
                    .collect()
            }),
        )
        .unwrap();
        let work = same(&mp4);
        assert!(!work.restarts.contains(&30), "{change:x}: {work:?}");
    }
}

#[test]
fn hevc_skipping_needs_parsable_nal_units_and_no_reordering() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("a.mp4");
    let mp4 = temp.path().join("camera.mp4");
    keyframed(&base, true, 20);
    // A NAL unit whose header does not parse leaves its picture without a time.
    let bytes = std::fs::read(&base).unwrap();
    std::fs::write(
        &mp4,
        rewrite(&bytes, |i, mut nals| {
            if i == 19 {
                nals.insert(0, vec![0x80, 0x01]);
            }
            nals
        }),
    )
    .unwrap();
    let work = same(&mp4);
    assert!(work.restarts.is_empty() && work.flushes == 0, "{work:?}");
    // B-frames with their composition offsets, then with the `ctts` box hidden.
    encoded(&base, true, 20, "bframes=3:b-pyramid=1");
    let mut bytes = std::fs::read(&base).unwrap();
    let work = same(&base);
    assert!(work.restarts.is_empty() && work.flushes == 0, "{work:?}");
    let (at, _) = find(&bytes, &[STBL.as_slice(), &[b"ctts"]].concat());
    bytes[at - 4..at].copy_from_slice(b"free");
    std::fs::write(&mp4, bytes).unwrap();
    let work = same(&mp4);
    assert!(work.restarts.is_empty() && work.flushes == 0, "{work:?}");
    // Open GOPs: CRA pictures, which a fresh decoder does not start at.
    encoded(&base, true, 20, "open-gop=1");
    let work = same(&base);
    assert!(work.restarts.is_empty(), "{work:?}");
}
