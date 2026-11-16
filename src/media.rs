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

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Signature {
    pub pictures: String,
    pub headers: BTreeSet<(u8, String)>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Inspection {
    pub info: Value,
    pub signature: Signature,
}

pub async fn inspect(path: &Path, kind: &str) -> anyhow::Result<Inspection> {
    let _profile = crate::profile::Scope::new("video_inspect");
    if let Some(worker) = WORKER_EXECUTABLE.get() {
        let mut command = Command::new(worker);
        command.arg("__inspect").arg(path).arg(kind);
        Ok(serde_json::from_slice(&run(&mut command).await?)?)
    } else {
        let permit = WORKERS.clone().acquire_owned().await?;
        let source = path.to_path_buf();
        let kind = kind.to_owned();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            crate::native_video::inspect(&source, &kind)
        })
        .await?
    }
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
/// Which engine copies raw camera originals into MP4. Internal setting only:
/// outputs, file names and API responses do not depend on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Pure,
    Ffmpeg,
}
pub const ENGINE_SETTING: &str = "HARS_REMUX_ENGINE";
impl Engine {
    pub fn configured() -> anyhow::Result<Self> {
        match std::env::var(ENGINE_SETTING) {
            Err(std::env::VarError::NotPresent) => Ok(Self::Pure),
            Ok(v) if v == "pure" => Ok(Self::Pure),
            Ok(v) if v == "ffmpeg" => Ok(Self::Ffmpeg),
            _ => bail!("{ENGINE_SETTING} must be pure or ffmpeg"),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::Ffmpeg => "ffmpeg",
        }
    }
}

static REMUXED: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];
/// Process-wide (pure, ffmpeg, fallbacks) remux counts, for diagnostics.
pub fn remux_counts() -> [u64; 3] {
    REMUXED
        .each_ref()
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
}
fn count(index: usize) {
    REMUXED[index].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Internal diagnostics of one remux; `info` is the only product output.
#[derive(Debug)]
pub struct Remuxed {
    pub info: Value,
    pub engine: Engine,
    /// Why the pure engine's attempt was discarded, when FFmpeg wrote the file.
    pub fallback: Option<String>,
    pub terminal: Option<crate::terminal_duration::Terminal>,
}

pub async fn remux(
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
) -> anyhow::Result<Value> {
    Ok(
        remux_using(Engine::configured()?, source, kind, output, times, None)
            .await?
            .info,
    )
}

/// `next_frame_ns`: logged gap from the last frame to the next camera frame.
pub async fn remux_using(
    engine: Engine,
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
    next_frame_ns: Option<u64>,
) -> anyhow::Result<Remuxed> {
    remux_inner(engine, source, kind, output, times, next_frame_ns, None).await
}

// A scan may be shared only inside the same conversion, never across jobs or requests.
pub(crate) async fn remux_probed(
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
    next_frame_ns: Option<u64>,
    provided: Option<Inspection>,
) -> anyhow::Result<Value> {
    let engine = Engine::configured()?;
    Ok(
        remux_inner(engine, source, kind, output, times, next_frame_ns, provided)
            .await?
            .info,
    )
}

struct Clocks {
    timescale: u32,
    /// TS source (pts, dts) relative to `offset`, in decode order.
    source: Option<Vec<(i64, i64)>>,
    offset: i64,
    terminal: crate::terminal_duration::Terminal,
}

async fn remux_inner(
    engine: Engine,
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
    next_frame_ns: Option<u64>,
    provided: Option<Inspection>,
) -> anyhow::Result<Remuxed> {
    anyhow::ensure!(source != output, "source and output must differ");
    anyhow::ensure!(
        !times.is_empty()
            && times.iter().all(|v| v.is_finite())
            && times.windows(2).all(|v| v[1] > v[0]),
        "invalid camera clock"
    );
    // Raw HEVC hashes its input during the mux pass. Other formats also need
    // packet clocks before muxing, so share their inspection within this job.
    let inspection = if kind.ends_with(".hevc") {
        None
    } else {
        let value = if let Some(value) = provided {
            value
        } else {
            inspect(source, kind).await?
        };
        validate_info(&value.info, kind)?;
        Some(value)
    };
    if kind.ends_with(".mp4") {
        let input = inspection.context("missing MP4 inspection")?;
        validate_times(&input.info, times)?;
        return Ok(Remuxed {
            info: input.info,
            engine,
            fallback: None,
            terminal: None,
        });
    }
    let clocks = clocks(kind, times, inspection.as_ref(), next_frame_ns)?;
    let mut fallback = None;
    if engine == Engine::Pure {
        match pure(source, kind, output, times, &clocks, inspection.as_ref()).await {
            Ok(info) => {
                count(0);
                tracing::info!(
                    event = "remux_engine",
                    engine = "pure",
                    kind,
                    terminal_ticks = clocks.terminal.ticks,
                    terminal_basis = ?clocks.terminal.basis,
                );
                return Ok(Remuxed {
                    info,
                    engine,
                    fallback: None,
                    terminal: Some(clocks.terminal),
                });
            }
            Err(reason) => {
                count(2);
                tracing::warn!(event = "remux_fallback", from = "pure", to = "ffmpeg", kind, reason = %reason);
                fallback = Some(reason);
            }
        }
    }
    let info = ffmpeg(source, kind, output, times, &clocks, inspection).await?;
    count(1);
    tracing::info!(
        event = "remux_engine",
        engine = Engine::Ffmpeg.name(),
        requested = engine.name(),
        kind,
        terminal_ticks = clocks.terminal.ticks,
        terminal_basis = ?clocks.terminal.basis,
    );
    Ok(Remuxed {
        info,
        engine: Engine::Ffmpeg,
        fallback,
        terminal: Some(clocks.terminal),
    })
}

/// Output clocks both engines must produce, decided once before either runs.
fn clocks(
    kind: &str,
    times: &[f64],
    inspection: Option<&Inspection>,
    next_frame_ns: Option<u64>,
) -> anyhow::Result<Clocks> {
    let Some(input) = inspection else {
        let stamps = times
            .iter()
            .map(|t| (t * 1_000_000.).round() as i64)
            .collect::<Vec<_>>();
        return Ok(Clocks {
            timescale: 1_000_000,
            source: None,
            offset: 0,
            terminal: crate::terminal_duration::decide(&stamps, 1_000_000, next_frame_ns)?,
        });
    };
    anyhow::ensure!(kind == "qcamera.ts", "unsupported camera original format");
    anyhow::ensure!(
        input.info["streams"][0]["time_base"] == "1/90000",
        "invalid stream time base"
    );
    let packets = input.info["packets"]
        .as_array()
        .context("missing packet timestamps")?
        .iter()
        .map(|p| Some((p["pts"].as_i64()?, p["dts"].as_i64()?)))
        .collect::<Option<Vec<_>>>()
        .context("missing PTS")?;
    // Python subtracts the smallest source PTS from both clocks.
    let offset = packets.iter().map(|p| p.0).min().context("missing PTS")?;
    let dts = packets.iter().map(|p| p.1).collect::<Vec<_>>();
    Ok(Clocks {
        timescale: 90_000,
        source: Some(
            packets
                .iter()
                .map(|(p, d)| (p - offset, d - offset))
                .collect(),
        ),
        offset,
        terminal: crate::terminal_duration::decide(&dts, 90_000, next_frame_ns)?,
    })
}

fn temp_for(output: &Path) -> PathBuf {
    output.with_extension(format!("{}.tmp.mp4", crate::store::id()))
}

/// The pure engine's attempt. Any refusal, crash or failed check is returned
/// as a reason; its output is removed and FFmpeg then handles the input.
async fn pure(
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
    clocks: &Clocks,
    inspection: Option<&Inspection>,
) -> Result<Value, String> {
    use crate::pure_remux::{Job, Reply};
    let temp = temp_for(output);
    let job = Job {
        kind: kind.to_owned(),
        times: times.to_vec(),
        terminal: clocks.terminal.ticks,
        offset: clocks.offset,
    };
    let reply = if let Some(worker) = WORKER_EXECUTABLE.get() {
        let job_path = temp.with_extension("job.json");
        let result = async {
            crate::store::atomic_json(&job_path, &job).await?;
            let mut command = Command::new(worker);
            command
                .arg("__pure-remux")
                .arg(source)
                .arg(&job_path)
                .arg(&temp);
            anyhow::Ok(serde_json::from_slice::<Reply>(&run(&mut command).await?)?)
        }
        .await;
        let _ = tokio::fs::remove_file(&job_path).await;
        result.map_err(|e| format!("worker: {e:#}"))
    } else {
        // Library embedding/tests: a panic must not take the caller down.
        match WORKERS.clone().acquire_owned().await {
            Ok(permit) => {
                let input = source.to_path_buf();
                let target = temp.clone();
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Reply::from(crate::pure_remux::remux(&input, &target, &job))
                    }))
                    .unwrap_or_else(|_| Reply::Failed {
                        reason: "panic".into(),
                    })
                })
                .await
                .map_err(|e| format!("worker: {e}"))
            }
            Err(e) => Err(format!("worker: {e}")),
        }
    };
    let result = async {
        let outcome = match reply? {
            Reply::Done(outcome) => outcome,
            Reply::Unsupported { reason } => return Err(format!("unsupported: {reason}")),
            Reply::Failed { reason } => return Err(format!("failed: {reason}")),
        };
        if let Some(input) = inspection
            && input.signature != outcome.input
        {
            return Err("verification: input signatures disagree".into());
        }
        verify_and_place(
            &temp,
            output,
            kind,
            times,
            clocks,
            &outcome.input,
            outcome.output,
        )
        .await
        .map_err(|e| format!("verification: {e:#}"))
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

async fn ffmpeg(
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
    clocks: &Clocks,
    inspection: Option<Inspection>,
) -> anyhow::Result<Value> {
    let temp = temp_for(output);
    let terminal = i64::try_from(clocks.terminal.ticks)?;
    let result = async {
        let before = if kind.ends_with(".hevc") {
            if let Some(worker) = WORKER_EXECUTABLE.get() {
                let clock_path = temp.with_extension("clock.json");
                crate::store::atomic_json(&clock_path, &times).await?;
                let mut command = Command::new(worker);
                command
                    .arg("__native-hevc")
                    .arg(source)
                    .arg(&clock_path)
                    .arg(&temp)
                    .arg(terminal.to_string());
                let result = run(&mut command).await;
                let _ = tokio::fs::remove_file(&clock_path).await;
                serde_json::from_slice::<Signature>(&result?)?
            } else {
                // Library embedding/tests: keep the permit in the blocking job even if its caller cancels.
                let permit = WORKERS.clone().acquire_owned().await?;
                let input = source.to_path_buf();
                let output = temp.clone();
                let clock = times.to_vec();
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    crate::native_video::remux_hevc(&input, &output, &clock, terminal)
                })
                .await??
            }
        } else {
            let input = inspection.context("missing source inspection")?;
            let offset = clocks.offset;
            if let Some(worker) = WORKER_EXECUTABLE.get() {
                let mut command = Command::new(worker);
                command
                    .arg("__native-ts")
                    .arg(source)
                    .arg(&temp)
                    .arg(offset.to_string())
                    .arg(terminal.to_string());
                run(&mut command).await?;
            } else {
                let permit = WORKERS.clone().acquire_owned().await?;
                let input = source.to_path_buf();
                let output = temp.clone();
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    crate::native_video::remux_ts(&input, &output, offset, terminal)
                })
                .await??;
            }
            input.signature
        };
        let checked = inspect(&temp, target_kind(kind)).await?;
        verify_and_place(&temp, output, kind, times, clocks, &before, checked).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

fn target_kind(kind: &str) -> &'static str {
    if kind == "qcamera.ts" {
        "camera.mp4"
    } else {
        "fcamera.mp4"
    }
}

/// Checks every engine's output the same way before the atomic rename.
async fn verify_and_place(
    temp: &Path,
    output: &Path,
    kind: &str,
    times: &[f64],
    clocks: &Clocks,
    before: &Signature,
    checked: Inspection,
) -> anyhow::Result<Value> {
    let info = checked.info;
    validate_info(&info, target_kind(kind))?;
    validate_times(&info, times)?;
    anyhow::ensure!(
        *before == checked.signature,
        "compressed pictures changed during remux"
    );
    let stream = &info["streams"][0];
    anyhow::ensure!(
        stream["time_base"] == format!("1/{}", clocks.timescale),
        "invalid stream time base"
    );
    let packets = info["packets"]
        .as_array()
        .context("missing packet timestamps")?
        .iter()
        .map(|p| Some((p["pts"].as_i64()?, p["dts"].as_i64()?)))
        .collect::<Option<Vec<_>>>()
        .context("missing PTS")?;
    if let Some(source) = &clocks.source {
        anyhow::ensure!(*source == packets, "video timestamps changed during remux");
    }
    let span = packets
        .last()
        .map(|p| p.1)
        .zip(packets.first().map(|p| p.1));
    anyhow::ensure!(
        span.zip(stream["duration_ts"].as_i64())
            .is_some_and(|((last, first), total)| {
                u64::try_from(total - (last - first)).ok() == Some(clocks.terminal.ticks)
            }),
        "last sample duration changed during remux"
    );
    verify_mp4_boxes(temp, kind != "qcamera.ts").await?;
    tokio::fs::rename(temp, output).await?;
    Ok(info)
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
