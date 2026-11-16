//! Last-sample duration rule (docs/hars/TERMINAL-DURATION.md) on synthetic
//! clocks: the engine ends every clip at the rule-given time, as the host's
//! development `ffprobe` reads the MP4.
use roadviewer_hars::{
    media,
    terminal_duration::{Basis, decide},
};
use std::{
    path::Path,
    process::{Command, Stdio},
};

fn run(cmd: &mut Command) {
    assert!(
        cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "fixture command failed"
    );
}

fn hevc(path: &Path, frames: usize) {
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x96:rate=20")
        .args(["-frames:v", &frames.to_string(), "-an", "-c:v", "libx265"])
        .args([
            "-x265-params",
            "pools=1:frame-threads=1:bframes=0:keyint=20:log-level=error",
        ])
        .args(["-pix_fmt", "yuv420p", "-f", "hevc"])
        .arg(path));
}

/// H.264 TS whose PES clocks (90 kHz ticks) are exactly `ticks`.
fn ts(dir: &Path, name: &str, ticks: &[i64]) -> std::path::PathBuf {
    let raw = dir.join(format!("{name}.h264"));
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x96:rate=20")
        .args([
            "-frames:v",
            &ticks.len().to_string(),
            "-an",
            "-threads",
            "1",
        ])
        .args([
            "-c:v", "libx264", "-bf", "0", "-g", "20", "-pix_fmt", "yuv420p", "-f", "h264",
        ])
        .arg(&raw));
    let staged = dir.join(format!("{name}.staged.ts"));
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "h264", "-i"])
        .arg(&raw)
        .args(["-map", "0:v:0", "-c:v", "copy", "-f", "mpegts"])
        .arg(&staged));
    // From a 1/90000 input, setts expressions are ticks; a nested if() chain
    // maps the packet number N to its clock.
    let mut expr = format!("{}", ticks[ticks.len() - 1]);
    for (i, t) in ticks.iter().enumerate().rev().skip(1) {
        expr = format!("if(eq(N\\,{i})\\,{t}\\,{expr})");
    }
    let out = dir.join(format!("{name}.ts"));
    run(Command::new("ffmpeg")
        .args([
            "-v", "error", "-nostdin", "-y", "-copyts", "-f", "mpegts", "-i",
        ])
        .arg(&staged)
        .args(["-map", "0:v:0", "-c:v", "copy", "-bsf:v"])
        .arg(format!("setts=pts={expr}:dts={expr}"))
        .args(["-muxdelay", "0", "-f", "mpegts"])
        .arg(&out));
    out
}

struct Clip {
    pts: Vec<(i64, i64)>,
    last_duration: i64,
    duration_ts: i64,
}

/// Packet durations as the host's `ffprobe` reads them.
fn probed_durations(path: &Path) -> Vec<i64> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_packets", "-of", "json"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    info["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["duration"].as_i64().unwrap())
        .collect()
}

fn clip(path: &Path, info: &serde_json::Value) -> Clip {
    let durations = probed_durations(path);
    Clip {
        pts: info["packets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["pts"].as_i64().unwrap(), p["dts"].as_i64().unwrap()))
            .collect(),
        last_duration: *durations.last().unwrap(),
        duration_ts: info["streams"][0]["duration_ts"].as_i64().unwrap(),
    }
}

/// Runs the engine and returns the last duration as the host's `ffprobe` reads
/// it, after checking the stream duration ends with that sample.
async fn end(source: &Path, kind: &str, times: &[f64], next: Option<u64>) -> (i64, Basis) {
    let dir = source.parent().unwrap();
    let output = dir.join(format!(
        "{}-pure-{}.mp4",
        source.file_stem().unwrap().to_string_lossy(),
        next.unwrap_or(0)
    ));
    let done = media::remux_using(source, kind, &output, times, next)
        .await
        .unwrap();
    let c = clip(&output, &done.info);
    assert_eq!(c.duration_ts - c.pts.last().unwrap().1, c.last_duration);
    assert_eq!(c.pts.len(), times.len());
    (c.last_duration, done.terminal.unwrap().basis)
}

fn seconds(stamps: &[i64], scale: f64) -> Vec<f64> {
    stamps
        .iter()
        .map(|s| (s - stamps[0]) as f64 / scale)
        .collect()
}

#[tokio::test]
async fn hevc_clocks_end_at_the_logged_next_frame_or_the_median_interval() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("fcamera.hevc");
    hevc(&source, 30);
    let constant = (0..30).map(|i| i * 50_000).collect::<Vec<i64>>();
    let times = seconds(&constant, 1e6);
    assert_eq!(
        end(&source, "fcamera.hevc", &times, None).await,
        (50_000, Basis::MedianInterval)
    );
    assert_eq!(
        end(&source, "fcamera.hevc", &times, Some(47_541_184)).await,
        (47_541, Basis::NextFrame)
    );
    // A stall after the last frame is not one frame's length.
    assert_eq!(
        end(&source, "fcamera.hevc", &times, Some(100_600_000)).await,
        (50_000, Basis::MedianInterval)
    );
    assert_eq!(
        end(&source, "fcamera.hevc", &times, Some(100_000_000)).await,
        (100_000, Basis::NextFrame)
    );
    // Initial jitter (21 intervals of 50.12 ms, then 8 of 50 ms: the median is
    // the jittered one), a mid-clip rate change and a tail outlier.
    let jitter = (0..30)
        .map(|i| i * 50_000 + if i < 21 { i * 120 } else { 2520 })
        .collect::<Vec<i64>>();
    assert_eq!(
        end(&source, "fcamera.hevc", &seconds(&jitter, 1e6), None)
            .await
            .0,
        50_120
    );
    let change = (0..30)
        .map(|i| {
            if i < 12 {
                i * 50_000
            } else {
                600_000 + (i - 12) * 33_333
            }
        })
        .collect::<Vec<i64>>();
    assert_eq!(
        end(&source, "fcamera.hevc", &seconds(&change, 1e6), None)
            .await
            .0,
        33_333
    );
    let mut outlier = constant.clone();
    outlier[29] += 400_000;
    assert_eq!(
        end(&source, "fcamera.hevc", &seconds(&outlier, 1e6), None)
            .await
            .0,
        50_000
    );
}

#[tokio::test]
async fn hevc_short_clips_use_median_then_nominal() {
    let temp = tempfile::tempdir().unwrap();
    let two = temp.path().join("two.hevc");
    hevc(&two, 2);
    assert_eq!(
        end(&two, "fcamera.hevc", &[0., 0.04], None).await,
        (40_000, Basis::MedianInterval)
    );
    let one = temp.path().join("one.hevc");
    hevc(&one, 1);
    assert_eq!(
        end(&one, "fcamera.hevc", &[0.], None).await,
        (50_000, Basis::Nominal)
    );
    assert_eq!(
        end(&one, "fcamera.hevc", &[0.], Some(46_248_266)).await,
        (46_248, Basis::NextFrame)
    );
}

#[tokio::test]
async fn ts_clocks_follow_the_rule() {
    let temp = tempfile::tempdir().unwrap();
    let base = 900_000;
    type Case = (&'static str, Vec<i64>, Option<u64>, (i64, Basis));
    let cases: Vec<Case> = vec![
        (
            "constant",
            (0..25).map(|i| base + i * 4500).collect(),
            None,
            (4500, Basis::MedianInterval),
        ),
        (
            "logged",
            (0..25).map(|i| base + i * 4500).collect(),
            Some(46_771_877),
            (4209, Basis::NextFrame),
        ),
        (
            "jitter",
            (0..25)
                .map(|i| base + i * 4500 + if i <= 20 { i * 11 } else { 220 - (i - 20) * 30 })
                .collect(),
            None,
            (4511, Basis::MedianInterval),
        ),
        (
            "change",
            (0..25)
                .map(|i| {
                    if i < 10 {
                        base + i * 4500
                    } else {
                        base + 45_000 + (i - 10) * 3000
                    }
                })
                .collect(),
            None,
            (3000, Basis::MedianInterval),
        ),
        (
            "outlier",
            (0..25)
                .map(|i| base + i * 4500 + if i == 24 { 20_000 } else { 0 })
                .collect(),
            Some(500_000_000),
            (4500, Basis::MedianInterval),
        ),
        (
            "short",
            (0..5).map(|i| base + i * 4400).collect(),
            None,
            (4400, Basis::MedianInterval),
        ),
        (
            "pair",
            vec![base, base + 4600],
            None,
            (4600, Basis::MedianInterval),
        ),
        ("single", vec![base], None, (4500, Basis::Nominal)),
    ];
    for (name, ticks, next, expected) in cases {
        let source = ts(temp.path(), name, &ticks);
        let times = seconds(&ticks, 90_000.);
        let got = end(&source, "qcamera.ts", &times, next).await;
        assert_eq!(got, expected, "{name}");
        assert_eq!(
            decide(&ticks, 90_000, next).unwrap().ticks,
            expected.0 as u64,
            "{name}"
        );
    }
}

/// A Python-made MP4 keeps its own last duration: reuse neither rejects nor rewrites it.
#[tokio::test]
async fn python_shaped_mp4_is_reused_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let ticks = (0..25)
        .map(|i| 900_000 + i * 4500 + (i % 3) * 7)
        .collect::<Vec<i64>>();
    let source = ts(temp.path(), "camera", &ticks);
    let mp4 = temp.path().join("qcamera.mp4");
    // Same packet copy as the Python loop: library demux, MP4 mux, demuxer durations.
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&source)
        .args(["-map", "0:v:0", "-c:v", "copy", "-movflags", "+faststart"])
        .arg(&mp4));
    let before = std::fs::read(&mp4).unwrap();
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_packets",
            "-of",
            "json",
        ])
        .arg(&mp4)
        .output()
        .unwrap();
    assert!(probe.status.success());
    let probe: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let mut times = probe["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["pts_time"].as_str().unwrap().parse::<f64>().unwrap())
        .collect::<Vec<_>>();
    times.sort_by(f64::total_cmp);
    let output = temp.path().join("unused.mp4");
    let info = media::remux(&mp4, "camera.mp4", &output, &times)
        .await
        .unwrap();
    assert!(!output.exists());
    assert_eq!(std::fs::read(&mp4).unwrap(), before);
    assert_eq!(
        info["streams"][0]["duration_ts"],
        probe["streams"][0]["duration_ts"]
    );
}
