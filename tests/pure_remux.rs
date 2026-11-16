//! Pure Rust remux engine (the default) against the FFmpeg engine, and the
//! fallback contract between them. All media here is synthetic (lavfi + x264/
//! x265); real-corpus parity is checked by integration scripts.
use roadviewer_hars::{
    media::{self, Engine},
    pure_remux::{self, FailureClass, Job, readback},
};
use std::{
    path::{Path, PathBuf},
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

/// Packet payloads, durations and keyframe flags as libavformat demuxes them.
fn demuxed(path: &Path) -> Vec<(Vec<u8>, i64, bool)> {
    let mut input = ffmpeg_next::format::input(path).unwrap();
    input
        .packets()
        .map(|(_, p)| (p.data().unwrap().to_vec(), p.duration(), p.is_key()))
        .collect()
}

fn no_leftovers(dir: &Path, keep: &[&str]) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(keep.contains(&name.as_str()), "left behind {name}");
    }
}

/// TS clocks relative to the first PTS, as convert.rs builds them.
async fn ts_clock(source: &Path) -> (Vec<f64>, i64) {
    let raw = media::inspect(source, "qcamera.ts").await.unwrap();
    let mut pts = packets(&raw.info).iter().map(|p| p.0).collect::<Vec<_>>();
    pts.sort_unstable();
    (
        pts.iter().map(|p| (p - pts[0]) as f64 / 90000.).collect(),
        pts[0],
    )
}

#[tokio::test]
async fn hevc_pure_output_equals_ffmpeg_engine_except_container_extras() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("fcamera.hevc");
    fixture(&source, true, 0);
    let mut times = clock();
    times[5] += 0.01;
    let raw = media::inspect(&source, "fcamera.hevc").await.unwrap();
    let mut outputs = Vec::new();
    for engine in [Engine::Pure, Engine::Ffmpeg] {
        let output = temp.path().join(format!("{engine:?}.mp4"));
        let done = media::remux_using(
            engine,
            &source,
            "fcamera.hevc",
            &output,
            &times,
            Some(48_000_000),
        )
        .await
        .unwrap();
        assert_eq!(
            (done.engine, done.fallback.as_ref().map(|f| f.code.as_str())),
            (engine, None)
        );
        let independent = media::inspect(&output, "fcamera.mp4").await.unwrap();
        assert_eq!(independent.signature, raw.signature);
        assert_eq!(independent.info["streams"][0]["codec_tag_string"], "hvc1");
        outputs.push((output, done.info));
    }
    let a = media::probe(&outputs[0].0, "fcamera.mp4", true)
        .await
        .unwrap();
    let b = media::probe(&outputs[1].0, "fcamera.mp4", true)
        .await
        .unwrap();
    media::validate_times(&a, &times).unwrap();
    assert_eq!(packets(&a), packets(&b));
    for field in [
        "time_base",
        "duration_ts",
        "width",
        "height",
        "has_b_frames",
    ] {
        assert_eq!(a["streams"][0][field], b["streams"][0][field], "{field}");
    }
    // 0.9 s last stamp + the logged 48 ms gap.
    assert_eq!(a["streams"][0]["duration_ts"], 948_000);
    assert_eq!(demuxed(&outputs[0].0), demuxed(&outputs[1].0));
    let (_, pure) = readback::inspect(&outputs[0].0, true).unwrap();
    let (_, ffmpeg) = readback::inspect(&outputs[1].0, true).unwrap();
    assert_eq!(pure.decoder_configuration, ffmpeg.decoder_configuration);
    assert_eq!(pure.keyframes, vec![0]);
    assert_eq!(pure.top_level, ["ftyp", "moov", "mdat"]);
    assert_eq!(pure.durations, ffmpeg.durations);
    let (errors, frames) = decode_errors(&outputs[0].0);
    assert_eq!((errors.as_str(), frames), ("", 10));
    no_leftovers(temp.path(), &["fcamera.hevc", "Pure.mp4", "Ffmpeg.mp4"]);
}

#[tokio::test]
async fn h264_ts_pure_output_equals_ffmpeg_engine() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("qcamera.ts");
    fixture(&source, false, 0);
    let (times, _) = ts_clock(&source).await;
    let raw = media::inspect(&source, "qcamera.ts").await.unwrap();
    let mut outputs = Vec::new();
    for engine in [Engine::Pure, Engine::Ffmpeg] {
        let output = temp.path().join(format!("{engine:?}.mp4"));
        let done = media::remux_using(engine, &source, "qcamera.ts", &output, &times, None)
            .await
            .unwrap();
        assert_eq!(
            (done.engine, done.fallback.as_ref().map(|f| f.code.as_str())),
            (engine, None)
        );
        assert_eq!(
            media::inspect(&output, "camera.mp4")
                .await
                .unwrap()
                .signature,
            raw.signature
        );
        outputs.push((output, done.info));
    }
    assert_eq!(packets(&outputs[0].1), packets(&outputs[1].1));
    assert_eq!(
        outputs[0].1["streams"][0]["duration_ts"],
        outputs[1].1["streams"][0]["duration_ts"]
    );
    assert_eq!(demuxed(&outputs[0].0), demuxed(&outputs[1].0));
    let (_, pure) = readback::inspect(&outputs[0].0, false).unwrap();
    let (_, ffmpeg) = readback::inspect(&outputs[1].0, false).unwrap();
    assert_eq!(pure.decoder_configuration, ffmpeg.decoder_configuration);
    assert_eq!(pure.durations, ffmpeg.durations);
    let (errors, frames) = decode_errors(&outputs[0].0);
    assert_eq!((errors.as_str(), frames), ("", 10));
}

/// B-frame TS: the pure engine refuses picture reordering; FFmpeg writes the
/// file exactly as before (its ctts/edit list is what Python also produces).
#[tokio::test]
async fn b_frame_ts_falls_back_to_ffmpeg() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("qcamera.ts");
    fixture(&source, false, 2);
    let (times, _) = ts_clock(&source).await;
    let output = temp.path().join("camera.mp4");
    let done = media::remux_using(Engine::Pure, &source, "qcamera.ts", &output, &times, None)
        .await
        .unwrap();
    assert_eq!(done.engine, Engine::Ffmpeg);
    let failure = done.fallback.unwrap();
    assert_eq!(failure.class, FailureClass::Unsupported, "{failure:?}");
    assert!(
        failure.reason.contains("H.264 picture reordering"),
        "{failure:?}"
    );
    let (errors, frames) = decode_errors(&output);
    assert_eq!((errors.as_str(), frames), ("", 10));
    no_leftovers(temp.path(), &["qcamera.ts", "camera.mp4"]);
}

/// What the product path did with an input: the pure engine wrote it, or it
/// refused/failed with a reason and FFmpeg then produced `Ok` or `Err`.
#[derive(Debug, PartialEq)]
enum Path2 {
    Pure,
    Fallback(String, bool),
    Rejected,
}
async fn classify(source: &Path, kind: &str, times: &[f64]) -> Path2 {
    let before = media::remux_counts();
    let output = source.with_extension("out.mp4");
    let result = media::remux_using(Engine::Pure, source, kind, &output, times, None).await;
    let after = media::remux_counts();
    let dir = source.parent().unwrap();
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(!name.contains(".tmp"), "left behind {name}");
    }
    assert_eq!(output.exists(), result.is_ok());
    if let Ok(done) = &result {
        let (errors, frames) = decode_errors(&output);
        assert!(frames > 0, "{errors}");
        std::fs::remove_file(&output).unwrap();
        if done.engine == Engine::Pure {
            return Path2::Pure;
        }
        let failure = done.fallback.clone().unwrap();
        return Path2::Fallback(format!("{}: {}", failure.class.name(), failure.code), true);
    }
    if after[2] > before[2] {
        Path2::Fallback("failed".into(), false)
    } else {
        Path2::Rejected
    }
}

#[tokio::test]
async fn boundary_inputs_take_the_pure_engine_or_an_explicit_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let ts = temp.path().join("qcamera.ts");
    fixture(&ts, false, 0);
    let hevc = temp.path().join("fcamera.hevc");
    fixture(&hevc, true, 0);
    let (times, _) = ts_clock(&ts).await;
    let raw_ts = std::fs::read(&ts).unwrap();
    let case = |name: &str, bytes: Vec<u8>| {
        let path = temp.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    };
    let video_packets = |f: &mut dyn FnMut(usize, &mut [u8]) -> bool| {
        let mut out = Vec::new();
        let mut seen = 0;
        for packet in raw_ts.chunks(188) {
            let mut packet = packet.to_vec();
            let pid = u16::from_be_bytes([packet[1] & 0x1f, packet[2]]);
            let keep = pid != 0x100 || {
                seen += 1;
                f(seen, &mut packet)
            };
            if keep {
                out.extend_from_slice(&packet);
            }
        }
        out
    };
    assert_eq!(classify(&ts, "qcamera.ts", &times).await, Path2::Pure);
    assert_eq!(classify(&hevc, "fcamera.hevc", &clock()).await, Path2::Pure);

    // A lost TS packet: libavformat marks the packet corrupt, so the source
    // scan rejects it before either engine (Python refuses corrupt packets too).
    let lost = case("lost.ts", video_packets(&mut |n, _| n != 20));
    assert_eq!(classify(&lost, "qcamera.ts", &times).await, Path2::Rejected);
    // Scrambling flag on one video packet.
    let scrambled = case(
        "scrambled.ts",
        video_packets(&mut |n, p| {
            if n == 20 {
                p[3] |= 0x80;
            }
            true
        }),
    );
    // The pure engine refuses it; libavformat ignores the flag (the payload is
    // clear here) and its copy passes every check.
    assert_eq!(
        classify(&scrambled, "qcamera.ts", &times).await,
        Path2::Fallback("unsupported: unsupported".into(), true)
    );
    // Transport error indicator on one video packet.
    let errored = case(
        "terror.ts",
        video_packets(&mut |n, p| {
            if n == 20 {
                p[1] |= 0x80;
            }
            true
        }),
    );
    // libavformat marks the packet corrupt: rejected by the source scan.
    assert_eq!(
        classify(&errored, "qcamera.ts", &times).await,
        Path2::Rejected
    );
    // The first PES loses its PTS/DTS flags: rejected by the source scan
    // before either engine, as the clock check needs every picture's PTS.
    let no_pts = case(
        "nopts.ts",
        video_packets(&mut |n, p| {
            if n == 1 && p[1] & 0x40 != 0 {
                let at = if p[3] & 0x20 != 0 {
                    5 + p[4] as usize
                } else {
                    4
                };
                p[at + 7] &= 0x3f;
            }
            true
        }),
    );
    assert_eq!(
        classify(&no_pts, "qcamera.ts", &times).await,
        Path2::Rejected
    );
    // Parameter sets that change mid-stream: hvc1 keeps one set, so both
    // engines refuse what would no longer be the same compressed stream.
    let other = temp.path().join("other.hevc");
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=176x96:rate=10",
        "-t",
        "1",
        "-an",
        "-c:v",
        "libx265",
        "-x265-params",
        "pools=1:frame-threads=1:bframes=0:keyint=10:log-level=error",
        "-f",
        "hevc",
    ])
    .arg(&other)
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    assert!(cmd.status().unwrap().success());
    let mut joined = std::fs::read(&hevc).unwrap();
    joined.extend(std::fs::read(&other).unwrap());
    let joined = case("joined.hevc", joined);
    let twenty = (0..20).map(|i| i as f64 / 10.).collect::<Vec<_>>();
    assert_eq!(
        classify(&joined, "fcamera.hevc", &twenty).await,
        Path2::Fallback("failed".into(), false)
    );
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

fn job(kind: &str, times: &[f64], offset: i64) -> Job {
    Job {
        kind: kind.into(),
        times: times.to_vec(),
        terminal: if kind.ends_with(".ts") { 9000 } else { 100_000 },
        offset,
    }
}

fn rejects(source: &Path, kind: &str, times: &[f64], offset: i64) -> String {
    let dir = source.parent().unwrap();
    let output = dir.join("out.mp4");
    let error = format!(
        "{:#}",
        pure_remux::remux(source, &output, &job(kind, times, offset))
            .err()
            .unwrap_or_else(|| panic!("accepted {}", source.display()))
    );
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

#[tokio::test]
async fn damaged_and_boundary_inputs_fail_without_panic_or_output() {
    let temp = tempfile::tempdir().unwrap();
    let hevc = temp.path().join("fcamera.hevc");
    fixture(&hevc, true, 0);
    let ts = temp.path().join("qcamera.ts");
    fixture(&ts, false, 0);
    let (ts_times, offset) = ts_clock(&ts).await;
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
    assert!(rejects(&cut, "qcamera.ts", &ts_times, offset).contains("MPEG-TS"));

    let whole = raw_ts.len() / 188 / 2 * 188;
    let half = case("half.ts", &raw_ts[..whole]);
    rejects(&half, "qcamera.ts", &ts_times, offset);

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
    let error = rejects(&lost, "qcamera.ts", &ts_times, offset);
    assert!(error.contains("continuity"), "{error}");
    // A lost packet is damage, not a range limit of the engine.
    let failure = pure_remux::Failure::classify(
        &pure_remux::remux(
            &lost,
            &dir.join("out.mp4"),
            &job("qcamera.ts", &ts_times, offset),
        )
        .unwrap_err(),
    );
    assert_eq!(failure.class, FailureClass::Invalid, "{failure:?}");

    let mut stripped = Vec::new();
    for unit in units(&raw_hevc) {
        if !matches!((unit[0] >> 1) & 63, 32..=34) {
            stripped.extend_from_slice(&[0, 0, 0, 1]);
            stripped.extend_from_slice(unit);
        }
    }
    let stripped = case("noparams.hevc", &stripped);
    assert!(rejects(&stripped, "fcamera.hevc", &times, 0).contains("decoder configuration"));

    let mut huge = vec![0, 0, 0, 1, 0x02, 0x01];
    huge.resize(33 * 1024 * 1024, 0x55);
    let huge = case("huge.hevc", &huge);
    assert!(rejects(&huge, "fcamera.hevc", &times, 0).contains("bounded memory"));

    let empty = case("empty.hevc", b"");
    rejects(&empty, "fcamera.hevc", &times, 0);
    let empty_ts = case("empty.ts", b"");
    rejects(&empty_ts, "qcamera.ts", &ts_times, offset);
    let garbage = case("garbage.hevc", b"#EXTM3U\nhttps://example.com/private\n");
    assert!(rejects(&garbage, "fcamera.hevc", &times, 0).contains("Annex B"));

    let hevc_copy = case("count.hevc", &raw_hevc);
    assert!(rejects(&hevc_copy, "fcamera.hevc", &times[..9], 0).contains("count"));
    let mut late = times.clone();
    late.iter_mut().for_each(|t| *t += 0.5);
    assert!(rejects(&hevc_copy, "fcamera.hevc", &late, 0).contains("does not start at zero"));
    let ts_copy = case("count.ts", &raw_ts);
    assert!(rejects(&ts_copy, "qcamera.ts", &ts_times, offset + 1).contains("earliest"));
    assert!(rejects(&ts_copy, "fcamera.hevc", &times, 0).contains("Annex B"));
    rejects(&hevc_copy, "qcamera.ts", &ts_times, offset);
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
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut outcomes = [0usize; 3];
    for (kind, hevc) in [("qcamera.ts", false), ("fcamera.hevc", true)] {
        let source = temp.path().join(kind);
        fixture(&source, hevc, 0);
        let raw = std::fs::read(&source).unwrap();
        let (times, offset) = if hevc {
            (clock(), 0)
        } else {
            runtime.block_on(ts_clock(&source))
        };
        for round in 0..160 {
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
            let job = job(kind, &times, offset);
            let started = std::time::Instant::now();
            let result = std::panic::catch_unwind(|| pure_remux::remux(&input, &output, &job));
            assert!(result.is_ok(), "panic on round {round} of {kind}");
            // No parser loops on damaged input: far inside the worker deadline.
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "round {round} of {kind} took {:?}",
                started.elapsed()
            );
            match result.unwrap() {
                Ok(_) => {
                    outcomes[0] += 1;
                    let (errors, frames) = decode_errors(&output);
                    assert!(frames > 0, "{errors}");
                }
                Err(e) if e.downcast_ref::<pure_remux::Unsupported>().is_some() => {
                    outcomes[1] += 1;
                    assert!(!output.exists());
                }
                Err(_) => {
                    outcomes[2] += 1;
                    assert!(!output.exists());
                }
            }
            let _ = std::fs::remove_file(&output);
        }
    }
    assert_eq!(outcomes.iter().sum::<usize>(), 320, "{outcomes:?}");
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

/// Raw HEVC with `keyint` and its Annex B units with their 4-byte start codes.
fn hevc_units(path: &Path, keyint: u32) -> Vec<Vec<u8>> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x96:rate=10")
        .args(["-t", "1", "-an", "-threads", "1", "-c:v", "libx265"])
        .arg("-x265-params")
        .arg(format!(
            "pools=1:frame-threads=1:bframes=0:keyint={keyint}:min-keyint={keyint}:log-level=error"
        ))
        .args(["-pix_fmt", "yuv420p", "-f", "hevc"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    assert!(cmd.status().unwrap().success());
    units(&std::fs::read(path).unwrap())
        .into_iter()
        .map(|u| u.to_vec())
        .collect()
}

/// Units joined with 4-byte start codes and no trailing zeros: the FFmpeg
/// engine rewrites HEVC samples through `filter_units` (CBS), which drops a
/// unit's own trailing zero bytes, while the pure engine keeps them as
/// libavformat's raw packet copy does; both hash the same pictures.
fn annex_b(units: &[Vec<u8>]) -> Vec<u8> {
    units
        .iter()
        .flat_map(|u| {
            let end = u.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
            [0, 0, 0, 1].into_iter().chain(u[..end].iter().copied())
        })
        .collect()
}

/// An end-of-sequence NAL before the second IDR: FFmpeg's HEVC parser starts
/// the next packet at it (VPS..EOB end a picture), so both engines must put it
/// at the start of the same sample.
#[tokio::test]
async fn end_of_sequence_units_split_like_ffmpeg() {
    let temp = tempfile::tempdir().unwrap();
    let mut units = hevc_units(&temp.path().join("raw.hevc"), 5);
    let second_vps = units
        .iter()
        .enumerate()
        .filter(|(_, u)| (u[0] >> 1) & 63 == 32)
        .nth(1)
        .unwrap()
        .0;
    units.insert(second_vps, vec![36 << 1, 1]);
    let source = temp.path().join("fcamera.hevc");
    std::fs::write(&source, annex_b(&units)).unwrap();
    let mut outputs = Vec::new();
    for engine in [Engine::Pure, Engine::Ffmpeg] {
        let output = temp.path().join(format!("{engine:?}.mp4"));
        let done = media::remux_using(engine, &source, "fcamera.hevc", &output, &clock(), None)
            .await
            .unwrap();
        assert_eq!(done.engine, engine, "{:?}", done.fallback);
        outputs.push(output);
    }
    let (pure, ffmpeg) = (demuxed(&outputs[0]), demuxed(&outputs[1]));
    assert_eq!(pure, ffmpeg);
    // The EOS unit opens sample 5 (after its 4-byte length).
    assert_eq!(&pure[5].0[4..6], &[36 << 1, 1]);
    let (errors, frames) = decode_errors(&outputs[0]);
    assert_eq!((errors.as_str(), frames), ("", 10));
}

/// Inputs built to exhaust memory or time end quickly with a bounded,
/// classified refusal: a stream of parameter sets alone (no picture ever
/// closes the first unit) and a stream of 5-byte SEI units (one hash pass,
/// not a buffer move per unit).
#[test]
fn hostile_hevc_inputs_fail_fast_and_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let job = job("fcamera.hevc", &clock(), 0);
    for (name, unit, bytes) in [
        ("sets.hevc", [0u8, 0, 1, 0x40, 1], 20 << 20),
        ("sei.hevc", [0u8, 0, 1, 0x4e, 1], 4 << 20),
    ] {
        let source = temp.path().join(name);
        std::fs::write(&source, unit.repeat(bytes / unit.len())).unwrap();
        let started = std::time::Instant::now();
        let error = pure_remux::remux(&source, &temp.path().join("out.mp4"), &job).unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{name} took {:?}",
            started.elapsed()
        );
        let failure = pure_remux::Failure::classify(&error);
        assert_eq!(failure.class, FailureClass::Invalid, "{name}: {failure:?}");
        no_leftovers(temp.path(), &["sets.hevc", "sei.hevc"]);
    }
}

/// Where a damaged input ended: written by the pure engine, refused by the
/// source scan before either engine, or refused by the pure engine with a
/// class, after which FFmpeg's copy either passed the same checks or failed.
#[derive(Debug, PartialEq)]
enum End {
    Pure,
    SourceScan,
    Fallback(FailureClass, bool),
}

async fn end(source: &Path, kind: &str, times: &[f64], offset: i64) -> End {
    let output = source.with_extension("out.mp4");
    let result = media::remux_using(Engine::Pure, source, kind, &output, times, None).await;
    let dir = source.parent().unwrap();
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(!name.contains(".tmp"), "left behind {name}");
    }
    assert_eq!(output.exists(), result.is_ok());
    let direct = pure_remux::remux(source, &dir.join("direct.mp4"), &job(kind, times, offset));
    let _ = std::fs::remove_file(dir.join("direct.mp4"));
    let class = direct
        .as_ref()
        .err()
        .map(|e| pure_remux::Failure::classify(e).class);
    match result {
        Ok(done) => {
            // A published file decodes completely and holds the input's pictures.
            let (errors, frames) = decode_errors(&output);
            assert_eq!(frames, times.len(), "{errors}");
            let read = media::inspect(
                &output,
                if kind.ends_with(".ts") {
                    "camera.mp4"
                } else {
                    "fcamera.mp4"
                },
            )
            .await
            .unwrap();
            if kind.ends_with(".hevc") {
                let raw = media::annex_b_signature(&std::fs::read(source).unwrap(), true).unwrap();
                assert_eq!(read.signature, raw);
            } else {
                let raw = media::inspect(source, kind).await.unwrap();
                assert_eq!(read.signature, raw.signature);
            }
            std::fs::remove_file(&output).unwrap();
            match done.fallback {
                None => End::Pure,
                Some(f) => End::Fallback(f.class, true),
            }
        }
        // Raw HEVC has no source scan; a TS input must pass FFmpeg's first.
        Err(_) if kind.ends_with(".ts") && media::inspect(source, kind).await.is_err() => {
            End::SourceScan
        }
        Err(e) => End::Fallback(
            class.unwrap_or_else(|| panic!("pure accepted: {e:#}")),
            false,
        ),
    }
}

/// Inputs the pure engine judges damaged end as a checked file or a clear
/// failure, never as a copy that lost data; FFmpeg's copy passes the same
/// independent checks (raw `NalHasher` signature, frame count, read-back).
#[tokio::test]
async fn damaged_inputs_end_verified_or_failed() {
    let temp = tempfile::tempdir().unwrap();
    let hevc = temp.path().join("fcamera.hevc");
    fixture(&hevc, true, 0);
    let ts = temp.path().join("qcamera.ts");
    fixture(&ts, false, 0);
    let (ts_times, offset) = ts_clock(&ts).await;
    let raw_hevc = std::fs::read(&hevc).unwrap();
    let raw_ts = std::fs::read(&ts).unwrap();
    let units = units(&raw_hevc);
    type Edit<'a> = &'a dyn Fn(usize, &[u8]) -> Option<Vec<u8>>;
    let rebuilt = |f: Edit| -> Vec<u8> {
        units
            .iter()
            .enumerate()
            .filter_map(|(i, u)| f(i, u))
            .flat_map(|u| [0, 0, 0, 1].into_iter().chain(u))
            .collect()
    };
    let first_slice = units.iter().position(|u| (u[0] >> 1) & 63 < 32).unwrap();
    let mut cases: Vec<(&str, PathBuf, &str, End)> = Vec::new();
    let mut add = |name: &'static str, bytes: Vec<u8>, kind: &'static str, expected: End| {
        let dir = temp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join(kind);
        std::fs::write(&path, bytes).unwrap();
        cases.push((name, path, kind, expected));
    };
    add(
        "forbidden-bit",
        rebuilt(&|i, u| {
            let mut u = u.to_vec();
            if i == first_slice + 2 {
                u[0] |= 0x80;
            }
            Some(u)
        }),
        "fcamera.hevc",
        End::Fallback(FailureClass::Invalid, false),
    );
    add(
        "no-parameter-sets",
        rebuilt(&|_, u| (!matches!((u[0] >> 1) & 63, 32..=34)).then(|| u.to_vec())),
        "fcamera.hevc",
        End::Fallback(FailureClass::Invalid, false),
    );
    let mut garbage = b"JUNK".to_vec();
    garbage.extend(&raw_hevc);
    add(
        "leading-garbage",
        garbage,
        "fcamera.hevc",
        End::Fallback(FailureClass::Invalid, false),
    );
    let mut huge = raw_hevc.clone();
    huge.extend([0, 0, 1, 0x02, 0x01]);
    huge.resize(huge.len() + 33 * 1024 * 1024, 0x55);
    add(
        "oversize-nal",
        huge,
        "fcamera.hevc",
        End::Fallback(FailureClass::Invalid, false),
    );
    add(
        "dropped-picture",
        rebuilt(&|i, u| (i != first_slice + 3).then(|| u.to_vec())),
        "fcamera.hevc",
        End::Fallback(FailureClass::Invalid, false),
    );
    let video = |f: &dyn Fn(usize, &mut [u8]) -> bool| -> Vec<u8> {
        let mut out = Vec::new();
        let mut seen = 0;
        for packet in raw_ts.chunks(188) {
            let mut packet = packet.to_vec();
            let pid = u16::from_be_bytes([packet[1] & 0x1f, packet[2]]);
            let keep = pid != 0x100 || {
                seen += 1;
                f(seen, &mut packet)
            };
            if keep {
                out.extend_from_slice(&packet);
            }
        }
        out
    };
    add(
        "ts-lost-packet",
        video(&|n, _| n != 20),
        "qcamera.ts",
        End::SourceScan,
    );
    add(
        "ts-transport-error",
        video(&|n, p| {
            if n == 20 {
                p[1] |= 0x80;
            }
            true
        }),
        "qcamera.ts",
        End::SourceScan,
    );
    add(
        "ts-cut-mid-packet",
        raw_ts[..raw_ts.len() - 100].to_vec(),
        "qcamera.ts",
        // libavformat (and so Python) drops the incomplete last TS packet; every
        // logged frame is present and the copy equals FFmpeg's own input scan.
        End::Fallback(FailureClass::Invalid, true),
    );
    let mut table = Vec::new();
    for (name, path, kind, expected) in cases {
        let times = if kind.ends_with(".ts") {
            ts_times.clone()
        } else {
            clock()
        };
        let got = end(&path, kind, &times, offset).await;
        table.push(format!("{name}: {got:?}"));
        assert_eq!(got, expected, "{name}");
    }
    println!("{}", table.join("\n"));
}
