//! qcamera.ts terminal sample duration must follow the library stream probe
//! that the Python reference (PyAV) uses, not the ffmpeg CLI probe.
// The probe oracle keeps libavformat's index layout so it can be read against demux.c.
#![allow(clippy::needless_range_loop)]
use roadviewer_hars::media;
use std::{
    path::Path,
    process::{Command, Stdio},
};

fn bits(bytes: &[u8]) -> Vec<bool> {
    bytes
        .iter()
        .flat_map(|b| (0..8).rev().map(move |i| b >> i & 1 == 1))
        .collect()
}
struct Reader<'a> {
    bits: &'a [bool],
    at: usize,
}
impl Reader<'_> {
    fn u(&mut self, n: usize) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = v << 1 | u32::from(self.bits[self.at]);
            self.at += 1;
        }
        v
    }
    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while !self.bits[self.at] {
            zeros += 1;
            self.at += 1;
        }
        self.at += 1;
        (1 << zeros) - 1 + self.u(zeros)
    }
}

/// openpilot's qcamera encoder writes VUI without timing_info; libx264 always
/// writes it, and timing info would make FFmpeg derive durations from the SPS.
fn strip_vui_timing(nal: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::new();
    for &b in &nal[1..] {
        if b == 3 && rbsp.ends_with(&[0, 0]) {
            continue;
        }
        rbsp.push(b);
    }
    let all = bits(&rbsp);
    let mut r = Reader { bits: &all, at: 0 };
    let profile = r.u(8);
    r.u(16);
    r.ue();
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128) {
        if r.ue() == 3 {
            r.u(1);
        }
        r.ue();
        r.ue();
        r.u(1);
        assert_eq!(r.u(1), 0, "fixture uses no scaling matrix");
    }
    r.ue();
    match r.ue() {
        0 => {
            r.ue();
        }
        1 => panic!("fixture uses no POC type 1"),
        _ => {}
    }
    r.ue();
    r.u(1);
    r.ue();
    r.ue();
    if r.u(1) == 0 {
        r.u(1);
    }
    r.u(1);
    if r.u(1) == 1 {
        (0..4).for_each(|_| {
            r.ue();
        });
    }
    assert_eq!(r.u(1), 1, "fixture has VUI");
    if r.u(1) == 1 && r.u(8) == 255 {
        r.u(32);
    }
    if r.u(1) == 1 {
        r.u(1);
    }
    if r.u(1) == 1 {
        r.u(4);
        if r.u(1) == 1 {
            r.u(24);
        }
    }
    if r.u(1) == 1 {
        r.ue();
        r.ue();
    }
    let flag = r.at;
    assert!(all[flag], "fixture has VUI timing info");
    let stop = all.iter().rposition(|b| *b).unwrap();
    let mut out = all[..flag].to_vec();
    out.push(false);
    out.extend_from_slice(&all[flag + 66..stop]);
    out.push(true);
    while !out.len().is_multiple_of(8) {
        out.push(false);
    }
    let mut nal_out = vec![nal[0]];
    for byte in out.chunks(8) {
        let b = byte.iter().fold(0u8, |v, bit| v << 1 | u8::from(*bit));
        if b <= 3 && nal_out.len() > 2 && nal_out.ends_with(&[0, 0]) {
            nal_out.push(3);
        }
        nal_out.push(b);
    }
    nal_out
}

fn annex_b_units(bytes: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let mut end = starts.get(n + 1).map_or(bytes.len(), |e| e - 3);
            while end > s && bytes[end - 1] == 0 {
                end -= 1;
            }
            &bytes[s..end]
        })
        .collect()
}

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

/// 20 fps camera whose clock drifts +2.4 ms over the first 20 intervals and
/// back afterwards, as real qcamera TS jitter does within the 5 ms log check.
fn jittered_ts(dir: &Path) -> std::path::PathBuf {
    let raw = dir.join("x264.h264");
    run(Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x96:rate=20",
            "-frames:v",
            "60",
            "-an",
            "-threads",
            "1",
            "-c:v",
            "libx264",
            "-profile:v",
            "baseline",
            "-bf",
            "0",
            "-g",
            "20",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "h264",
        ])
        .arg(&raw));
    let bytes = std::fs::read(&raw).unwrap();
    let mut patched = Vec::new();
    let mut sps = 0;
    for unit in annex_b_units(&bytes) {
        patched.extend_from_slice(&[0, 0, 0, 1]);
        if unit[0] & 31 == 7 {
            patched.extend(strip_vui_timing(unit));
            sps += 1;
        } else {
            patched.extend_from_slice(unit);
        }
    }
    assert!(sps > 0);
    let stripped = dir.join("stripped.h264");
    std::fs::write(&stripped, patched).unwrap();
    let ts = dir.join("qcamera.ts");
    // Commas are escaped for the bitstream filter list parser.
    let clock = r"(N*0.05+0.00012*if(lte(N\,20)\,N\,max(40-N\,0)))/TB";
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "h264", "-i"])
        .arg(&stripped)
        .args([
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-bsf:v",
            &format!("setts=pts={clock}:dts={clock}"),
            "-f",
            "mpegts",
        ])
        .arg(&ts));
    ts
}

const fn std_rate(i: usize) -> i64 {
    let i = i as i64;
    if i < 360 {
        (i + 1) * 1001
    } else if i < 390 {
        (i - 360 + 31) * 1001 * 12
    } else if i < 393 {
        [80, 120, 240][(i - 390) as usize] * 1001 * 12
    } else {
        [24, 30, 60, 12, 15, 48][(i - 393) as usize] * 1000 * 12
    }
}
fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// libavformat ff_rfps_add_frame/ff_rfps_calculate (identical in FFmpeg 5.1
/// through 8.0) for an H.264 stream without SPS timing in a 1/90000 time
/// base. Stream probing stops once 20 DTS intervals are counted.
fn probe_rate(dts: &[i64]) -> (i64, i64) {
    const N: usize = 399;
    let tb = 1. / 90000.;
    let mut err = [[[0f64; N]; 2]; 2];
    let (mut count, mut sum, mut gcd_all) = (0i64, 0i64, 0i64);
    let mut last: Option<i64> = None;
    for &ts in dts {
        if count >= 20 {
            break;
        }
        if let Some(prev) = last.filter(|p| ts > *p) {
            let seconds = ts as f64 * tb;
            for i in 0..N {
                if err[0][1][i] < 1e10 {
                    let sdts = seconds * std_rate(i) as f64 / 12012.;
                    for j in 0..2 {
                        let shifted = sdts + j as f64 * 0.5;
                        let error = sdts - shifted.round_ties_even() + j as f64 * 0.5;
                        err[j][0][i] += error;
                        err[j][1][i] += error * error;
                    }
                }
            }
            count += 1;
            sum += ts - prev;
            if count % 10 == 0 {
                let n = count as f64;
                for i in 0..N {
                    if err[0][1][i] < 1e10 {
                        let e = |j: usize| {
                            let a = err[j][0][i] / n;
                            err[j][1][i] / n - a * a
                        };
                        if e(0) > 0.04 && e(1) > 0.04 {
                            err[0][1][i] = 2e10;
                            err[1][1][i] = 2e10;
                        }
                    }
                }
            }
            if count > 3 {
                gcd_all = gcd(gcd_all, ts - prev);
            }
        }
        last = Some(ts);
    }
    if count > 15 && gcd_all > 180 {
        let g = gcd(90000, gcd_all);
        return (90000 / g, gcd_all / g);
    }
    let mut best = 0.01;
    let mut num = 0;
    if count > 1 {
        for j in 0..N {
            if std_rate(j) < 12012
                || tb * sum as f64 / (count as f64) < 12012. * 0.8 / std_rate(j) as f64
            {
                continue;
            }
            for k in 0..2 {
                let n = count as f64;
                let a = err[k][0][j] / n;
                let error = err[k][1][j] / n - a * a;
                if error < best && best > 1e-9 {
                    best = error;
                    num = std_rate(j);
                }
            }
        }
    }
    if num == 0 {
        return (90000, 1);
    }
    let g = gcd(num, 12012);
    (num / g, 12012 / g)
}

/// The Python reference (PyAV) ends a qcamera.ts at 1/r_frame_rate of the
/// library probe. This keeps a model of that estimate (used by
/// integration/terminal_rule.py) honest against the host's development
/// `ffprobe` run with the library default; HARS no longer copies it
/// (docs/hars/TERMINAL-DURATION.md).
#[tokio::test]
async fn python_terminal_estimate_reproduces_library_rate_probe() {
    let temp = tempfile::tempdir().unwrap();
    let ts = jittered_ts(temp.path());
    let source = media::inspect(&ts, "qcamera.ts").await.unwrap();
    let packets = source.info["packets"].as_array().unwrap();
    let dts = packets
        .iter()
        .map(|p| p["dts"].as_i64().unwrap())
        .collect::<Vec<_>>();
    let mut pts = packets
        .iter()
        .map(|p| p["pts"].as_i64().unwrap())
        .collect::<Vec<_>>();
    pts.sort_unstable();
    let rate = probe_rate(&dts);
    assert_ne!(rate, (20, 1), "fixture must move the probe off 20 fps");
    let terminal = 90000 * rate.1 / rate.0;

    let cli = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate",
            "-of",
            "csv=p=0",
        ])
        .arg(&ts)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&cli.stdout).lines().next(),
        Some("20/1"),
        "the CLI probe (scan_all_pmts=1) reads ~5 s and settles on 20 fps"
    );
    // The library default (what PyAV opens with): the same CLI with `scan_all_pmts` left off.
    let library = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-scan_all_pmts",
            "0",
            "-select_streams",
            "v:0",
        ])
        .args(["-show_streams", "-show_packets", "-of", "json"])
        .arg(&ts)
        .output()
        .unwrap();
    assert!(library.status.success());
    let library: serde_json::Value = serde_json::from_slice(&library.stdout).unwrap();
    let (num, den) = library["streams"][0]["r_frame_rate"]
        .as_str()
        .unwrap()
        .split_once('/')
        .unwrap();
    assert_eq!(
        (num.parse::<i64>().unwrap(), den.parse::<i64>().unwrap()),
        rate
    );
    // What a plain PyAV demux + MP4 mux (the Python loop) stores as the last duration.
    let python_last = library["packets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["size"].as_str().is_none_or(|s| s != "0"))
        .last()
        .map(|p| p["duration"].as_i64().unwrap())
        .unwrap();
    assert_eq!(python_last, terminal);
}

/// The product rule on the same jittered clock, without a logged next frame:
/// the engine ends the last sample at the median frame interval.
#[tokio::test]
async fn product_terminal_duration_without_next_frame_is_median_interval() {
    let temp = tempfile::tempdir().unwrap();
    let ts = jittered_ts(temp.path());
    let source = media::inspect(&ts, "qcamera.ts").await.unwrap();
    let packets = source.info["packets"].as_array().unwrap();
    let dts = packets
        .iter()
        .map(|p| p["dts"].as_i64().unwrap())
        .collect::<Vec<_>>();
    let mut pts = packets
        .iter()
        .map(|p| p["pts"].as_i64().unwrap())
        .collect::<Vec<_>>();
    pts.sort_unstable();
    let mut intervals = dts.windows(2).map(|d| d[1] - d[0]).collect::<Vec<_>>();
    intervals.sort_unstable();
    let median = intervals[(intervals.len() - 1) / 2];
    let rate = probe_rate(&dts);
    assert_ne!(
        median,
        90000 * rate.1 / rate.0,
        "fixture must separate the rules"
    );
    let times = pts
        .iter()
        .map(|p| (p - pts[0]) as f64 / 90000.)
        .collect::<Vec<_>>();
    let last = pts[pts.len() - 1] - pts[0];
    let mp4 = temp.path().join("pure.mp4");
    let done = media::remux_using(&ts, "qcamera.ts", &mp4, &times, None)
        .await
        .unwrap();
    let info = done.info;
    assert_eq!(info["streams"][0]["time_base"], "1/90000");
    assert_eq!(info["streams"][0]["duration_ts"], last + median);
    // The host's development ffprobe reads the MP4 independently.
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_packets", "-of", "json"])
        .arg(&mp4)
        .output()
        .unwrap();
    assert!(out.status.success());
    let probed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let durations = probed["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["pts"].as_i64().unwrap(), p["duration"].as_i64().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(durations.len(), pts.len());
    assert_eq!(*durations.last().unwrap(), (last, median));
    let checked = media::inspect(&mp4, "camera.mp4").await.unwrap();
    assert_eq!(checked.signature, source.signature);
    assert_eq!(checked.info["streams"][0]["codec_tag_string"], "avc1");
}
