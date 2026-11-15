//! Lossless FFmpeg stream copy orchestrated by Rust; no Python runtime.
use anyhow::{Context, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, OnceLock},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore};

static WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));
static WORKER_EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
pub fn set_worker_executable(path: PathBuf) -> anyhow::Result<()> {
    WORKER_EXECUTABLE
        .set(path)
        .map_err(|_| anyhow::anyhow!("media worker already configured"))
}
const OUTPUT_LIMIT: u64 = 64 * 1024 * 1024;
async fn run(command: &mut Command) -> anyhow::Result<Vec<u8>> {
    let _profile = crate::profile::Scope::new("native_process_ipc");
    use std::process::Stdio;
    let _permit = WORKERS.acquire().await?;
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().context("start native media tool")?;
    let mut stdout = child.stdout.take().unwrap();
    let work = async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(OUTPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() as u64 > OUTPUT_LIMIT {
            child.kill().await?;
            bail!("media output limit exceeded");
        }
        anyhow::ensure!(
            child.wait().await?.success(),
            "native media validation failed"
        );
        Ok(bytes)
    };
    tokio::time::timeout(Duration::from_secs(180), work)
        .await
        .context("media processing deadline exceeded")?
}
fn input_format(kind: &str) -> &'static str {
    if kind.ends_with(".mp4") {
        "mov"
    } else if kind.ends_with(".hevc") {
        "hevc"
    } else {
        "mpegts"
    }
}
pub async fn probe(path: &Path, kind: &str, packets: bool) -> anyhow::Result<Value> {
    let _profile = crate::profile::Scope::new("video_probe");
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-protocol_whitelist",
        "file,pipe",
        "-f",
        input_format(kind),
        "-show_streams",
        "-show_format",
    ]);
    if packets {
        cmd.arg("-show_packets");
    }
    cmd.args(["-of", "json"]).arg(path);
    Ok(serde_json::from_slice(&run(&mut cmd).await?)?)
}
pub async fn validate_upload(path: &Path, kind: &str) -> anyhow::Result<Value> {
    let info = probe(path, kind, false).await?;
    validate_info(&info, kind)?;
    Ok(info)
}
fn validate_info(info: &Value, kind: &str) -> anyhow::Result<()> {
    let streams = info["streams"].as_array().context("missing media stream")?;
    let video = streams
        .iter()
        .filter(|v| v["codec_type"] == "video")
        .collect::<Vec<_>>();
    anyhow::ensure!(video.len() == 1, "expected one video stream");
    let stream = video[0];
    anyhow::ensure!(
        stream["codec_name"]
            == if kind == "camera.mp4" || kind == "qcamera.ts" || kind == "qcamera.mp4" {
                "h264"
            } else {
                "hevc"
            },
        "camera codec mismatch"
    );
    let w = stream["width"].as_u64().unwrap_or(0);
    let h = stream["height"].as_u64().unwrap_or(0);
    anyhow::ensure!(
        w > 0 && w <= 4096 && h > 0 && h <= 2160 && w % 2 == 0 && h % 2 == 0,
        "unsupported camera dimensions"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub struct Signature {
    pub pictures: String,
    pub headers: BTreeSet<(u8, String)>,
}
pub fn annex_b_signature(bytes: &[u8], hevc: bool) -> anyhow::Result<Signature> {
    let mut hash = NalHasher::new(hevc);
    hash.feed(bytes)?;
    hash.finish()
}
pub struct NalHasher {
    buffer: Vec<u8>,
    hevc: bool,
    hash: Sha256,
    headers: BTreeSet<(u8, String)>,
    pictures: u64,
    searched: usize,
}
fn start_code(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    for i in from..bytes.len().saturating_sub(2) {
        if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            return Some((i, i + 4));
        }
        if bytes[i..].starts_with(&[0, 0, 1]) {
            return Some((i, i + 3));
        }
    }
    None
}
impl NalHasher {
    pub fn new(hevc: bool) -> Self {
        Self {
            buffer: Vec::new(),
            hevc,
            hash: Sha256::new(),
            headers: BTreeSet::new(),
            pictures: 0,
            searched: 0,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.buffer.extend_from_slice(bytes);
        while let Some((prefix, begin)) = start_code(&self.buffer, 0) {
            anyhow::ensure!(
                self.buffer[..prefix].iter().all(|b| *b == 0),
                "non Annex B prefix"
            );
            let Some((end, _)) = start_code(&self.buffer, begin.max(self.searched)) else {
                self.searched = self.buffer.len().saturating_sub(3).max(begin);
                break;
            };
            Self::add(
                self.hevc,
                &mut self.hash,
                &mut self.headers,
                &mut self.pictures,
                &self.buffer[begin..end],
            )?;
            self.buffer.drain(..end);
            self.searched = 0;
        }
        anyhow::ensure!(
            self.buffer.len() <= 32 * 1024 * 1024,
            "NAL unit exceeds bounded memory limit"
        );
        Ok(())
    }
    fn add(
        hevc: bool,
        hash: &mut Sha256,
        headers: &mut BTreeSet<(u8, String)>,
        pictures: &mut u64,
        mut unit: &[u8],
    ) -> anyhow::Result<()> {
        while unit.last() == Some(&0) {
            unit = &unit[..unit.len() - 1];
        }
        anyhow::ensure!(unit.len() >= if hevc { 2 } else { 1 }, "truncated NAL unit");
        let kind = if hevc {
            (unit[0] >> 1) & 63
        } else {
            unit[0] & 31
        };
        if (hevc && matches!(kind, 32..=34)) || (!hevc && matches!(kind, 7 | 8)) {
            headers.insert((kind, hex::encode(Sha256::digest(unit))));
        }
        if (hevc && kind < 32) || (!hevc && (1..=5).contains(&kind)) {
            hash.update((unit.len() as u32).to_be_bytes());
            hash.update(unit);
            *pictures += 1;
        }
        Ok(())
    }
    pub fn finish(mut self) -> anyhow::Result<Signature> {
        if let Some((prefix, begin)) = start_code(&self.buffer, 0) {
            anyhow::ensure!(
                self.buffer[..prefix].iter().all(|b| *b == 0),
                "non Annex B prefix"
            );
            Self::add(
                self.hevc,
                &mut self.hash,
                &mut self.headers,
                &mut self.pictures,
                &self.buffer[begin..],
            )?;
        }
        let required: &[u8] = if self.hevc { &[32, 33, 34] } else { &[7, 8] };
        anyhow::ensure!(
            self.pictures > 0
                && required
                    .iter()
                    .all(|k| self.headers.iter().any(|(kind, _)| kind == k)),
            "missing pictures or decoder configuration"
        );
        Ok(Signature {
            pictures: hex::encode(self.hash.finalize()),
            headers: self.headers,
        })
    }
}
async fn signature(path: &Path, kind: &str) -> anyhow::Result<Signature> {
    let _profile = crate::profile::Scope::new("video_bitstream_hash");
    let hevc = kind != "qcamera.ts" && kind != "camera.mp4" && kind != "qcamera.mp4";
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-v",
        "error",
        "-nostdin",
        "-protocol_whitelist",
        "file,pipe",
        "-f",
        input_format(kind),
        "-i",
    ])
    .arg(path)
    .args([
        "-map",
        "0:v:0",
        "-an",
        "-c:v",
        "copy",
        "-bsf:v",
        if hevc {
            "hevc_mp4toannexb"
        } else {
            "h264_mp4toannexb"
        },
        "-f",
        if hevc { "hevc" } else { "h264" },
        "pipe:1",
    ]);
    use std::process::Stdio;
    let _permit = WORKERS.acquire().await?;
    let mut child = cmd
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let work = async {
        let mut hash = NalHasher::new(hevc);
        let mut buffer = vec![0u8; 128 * 1024];
        loop {
            let n = stdout.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            hash.feed(&buffer[..n])?;
        }
        anyhow::ensure!(
            child.wait().await?.success(),
            "native bitstream extraction failed"
        );
        hash.finish()
    };
    tokio::time::timeout(Duration::from_secs(180), work)
        .await
        .context("bitstream inspection deadline exceeded")?
}
pub fn validate_times(info: &Value, times: &[f64]) -> anyhow::Result<()> {
    let packets = info["packets"]
        .as_array()
        .context("missing packet timestamps")?;
    let mut stamps = Vec::new();
    let mut last_dts = None;
    for packet in packets.iter().filter(|p| p["codec_type"] == "video") {
        let pts = packet["pts_time"]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .context("missing PTS")?;
        let dts = packet["dts_time"]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .context("missing DTS")?;
        anyhow::ensure!(
            pts.is_finite() && dts.is_finite() && last_dts.is_none_or(|last| dts > last),
            "invalid PTS/DTS"
        );
        last_dts = Some(dts);
        stamps.push(pts);
    }
    stamps.sort_by(f64::total_cmp);
    anyhow::ensure!(
        stamps.len() == times.len()
            && !times.is_empty()
            && stamps
                .iter()
                .zip(times)
                .all(|(a, b)| b.is_finite() && (a - b).abs() <= 0.002),
        "video frame count or timestamps disagree with log"
    );
    Ok(())
}
pub async fn remux(
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
) -> anyhow::Result<Value> {
    remux_probed(source, kind, output, times, None).await
}

// A scan may be shared only inside the same conversion, never across jobs or requests.
pub(crate) async fn remux_probed(
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
    provided: Option<Value>,
) -> anyhow::Result<Value> {
    let input_info = if let Some(info) = provided {
        info
    } else {
        probe(source, kind, kind.ends_with(".mp4")).await?
    };
    validate_info(&input_info, kind)?;
    anyhow::ensure!(source != output, "source and output must differ");
    anyhow::ensure!(
        !times.is_empty()
            && times.iter().all(|v| v.is_finite())
            && times.windows(2).all(|v| v[1] > v[0]),
        "invalid camera clock"
    );
    if kind.ends_with(".mp4") {
        validate_times(&input_info, times)?;
        signature(source, kind).await?;
        return Ok(input_info);
    }
    let before = signature(source, kind).await?;
    let temp = output.with_extension(format!("{}.tmp.mp4", crate::store::id()));
    let result = async {
        if kind.ends_with(".hevc") {
            anyhow::ensure!(
                input_info["streams"][0]["has_b_frames"] == 0,
                "unsupported HEVC B frames"
            );
            if let Some(worker) = WORKER_EXECUTABLE.get() {
                let clock_path = temp.with_extension("clock.json");
                crate::store::atomic_json(&clock_path, &times).await?;
                let mut command = Command::new(worker);
                command
                    .arg("__native-hevc")
                    .arg(source)
                    .arg(&clock_path)
                    .arg(&temp);
                let result = run(&mut command).await;
                let _ = tokio::fs::remove_file(&clock_path).await;
                result?;
            } else {
                // Library embedding/tests: keep the permit in the blocking job even if its caller cancels.
                let permit = WORKERS.clone().acquire_owned().await?;
                let input = source.to_path_buf();
                let output = temp.clone();
                let clock = times.to_vec();
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    crate::native_video::remux_hevc(&input, &output, &clock)
                })
                .await??;
            }
        } else {
            let mut cmd = Command::new("ffmpeg");
            cmd.args([
                "-v",
                "error",
                "-nostdin",
                "-n",
                "-protocol_whitelist",
                "file,pipe",
                "-f",
                input_format(kind),
            ]);
            cmd.arg("-i").arg(source).args([
                "-map",
                "0:v:0",
                "-an",
                "-c:v",
                "copy",
                "-map_metadata",
                "-1",
                "-movflags",
                "+faststart",
            ]);
            cmd.args(["-f", "mp4"]).arg(&temp);
            run(&mut cmd).await?;
        }
        let target_kind = if kind == "qcamera.ts" {
            "camera.mp4"
        } else {
            "fcamera.mp4"
        };
        let info = probe(&temp, target_kind, true).await?;
        validate_info(&info, target_kind)?;
        validate_times(&info, times)?;
        anyhow::ensure!(
            before == signature(&temp, target_kind).await?,
            "compressed pictures changed during remux"
        );
        verify_mp4_boxes(&temp, kind != "qcamera.ts").await?;
        tokio::fs::rename(&temp, output).await?;
        Ok(info)
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}
async fn verify_mp4_boxes(path: &Path, hevc: bool) -> anyhow::Result<()> {
    use tokio::io::AsyncSeekExt;
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let mut at = 0;
    let mut moov = None;
    let mut mdat = None;
    let mut config = false;
    while at < len {
        anyhow::ensure!(len - at >= 8, "truncated MP4 box");
        let mut header = [0u8; 8];
        file.read_exact(&mut header).await?;
        let small = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (size, prefix) = if small == 1 {
            let mut extended = [0u8; 8];
            file.read_exact(&mut extended).await?;
            (u64::from_be_bytes(extended), 16)
        } else if small == 0 {
            (len - at, 8)
        } else {
            (small as u64, 8)
        };
        anyhow::ensure!(size >= prefix && size <= len - at, "invalid MP4 box size");
        match &header[4..] {
            b"moov" => {
                moov = Some(at);
                anyhow::ensure!(size <= 16 * 1024 * 1024, "MP4 metadata exceeds limit");
                let mut bytes = vec![0u8; (size - prefix) as usize];
                file.read_exact(&mut bytes).await?;
                config = bytes
                    .windows(4)
                    .any(|v| v == if hevc { b"hvcC" } else { b"avcC" });
            }
            b"mdat" => {
                mdat = Some(at);
            }
            _ => {}
        }
        at += size;
        file.seek(std::io::SeekFrom::Start(at)).await?;
    }
    anyhow::ensure!(
        moov.zip(mdat).is_some_and(|(a, b)| a < b) && config,
        "missing faststart/decoder configuration"
    );
    Ok(())
}
pub async fn thumbnail(source: &Path, kind: &str, output: &Path) -> anyhow::Result<()> {
    validate_upload(source, kind).await?;
    if let Some(worker) = WORKER_EXECUTABLE.get() {
        let mut command = Command::new(worker);
        command.arg("__thumbnail").arg(source).arg(output);
        run(&mut command).await?;
    } else {
        let permit = WORKERS.clone().acquire_owned().await?;
        let source = source.to_path_buf();
        let output = output.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            crate::thumbnail::make(&source, &output)
        })
        .await??;
    }
    Ok(())
}

pub async fn reverse(
    source: &Path,
    kind: &str,
    output: &Path,
    source_pts: f64,
) -> anyhow::Result<()> {
    anyhow::ensure!(source_pts.is_finite(), "invalid original timestamp offset");
    let source_kind = match kind {
        "qcamera.ts" => "camera.mp4",
        "fcamera.hevc" => "fcamera.mp4",
        "ecamera.hevc" => "ecamera.mp4",
        _ => anyhow::bail!("unsupported camera original format"),
    };
    validate_upload(source, source_kind).await?;
    let before = signature(source, source_kind).await?;
    let temp = output.with_extension(format!("{}.tmp", crate::store::id()));
    let result = async {
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-v",
            "error",
            "-nostdin",
            "-n",
            "-protocol_whitelist",
            "file,pipe",
            "-f",
            "mov",
            "-copyts",
            "-i",
        ])
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-c:v",
            "copy",
            "-bsf:v",
            if kind == "qcamera.ts" {
                "h264_mp4toannexb"
            } else {
                "hevc_mp4toannexb"
            },
        ]);
        if kind == "qcamera.ts" {
            cmd.args([
                "-mpegts_copyts",
                "1",
                "-muxdelay",
                "0",
                "-output_ts_offset",
                &source_pts.to_string(),
            ]);
        }
        cmd.args([
            "-f",
            if kind == "qcamera.ts" {
                "mpegts"
            } else {
                "hevc"
            },
        ])
        .arg(&temp);
        run(&mut cmd).await?;
        anyhow::ensure!(
            before == signature(&temp, kind).await?,
            "compressed pictures changed during reverse export"
        );
        if kind == "qcamera.ts" {
            let original = probe(source, source_kind, true).await?;
            let restored = probe(&temp, kind, true).await?;
            let mut times = original["packets"]
                .as_array()
                .context("missing MP4 packets")?
                .iter()
                .filter(|p| p["codec_type"] == "video")
                .map(|p| {
                    p["pts_time"]
                        .as_str()
                        .context("missing MP4 PTS")?
                        .parse::<f64>()
                        .map(|p| p + source_pts)
                        .map_err(anyhow::Error::from)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            times.sort_by(f64::total_cmp);
            validate_times(&restored, &times)?;
        }
        tokio::fs::rename(&temp, output).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}
