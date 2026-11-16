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
/// Exit status of a media worker whose job failed for the machine (EX_TEMPFAIL).
pub const RESOURCE_EXIT: i32 = 75;
/// A media worker that ended without success; how it ended tells a crash or
/// a kill (the kernel's OOM killer, a shutdown) from a reported error.
#[derive(Debug)]
pub struct WorkerExit(pub std::process::ExitStatus);
impl std::fmt::Display for WorkerExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native media validation failed ({})", self.0)
    }
}
impl std::error::Error for WorkerExit {}
#[derive(Debug)]
pub struct Deadline;
impl std::fmt::Display for Deadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("media processing deadline exceeded")
    }
}
impl std::error::Error for Deadline {}

/// The child is killed when the process that started it dies, even by
/// SIGKILL, so no orphaned worker keeps writing into a staging folder.
pub fn die_with_parent(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getpid/prctl/getppid are async-signal-safe and touch no
        // memory of the parent; the closure only copies `parent`.
        unsafe {
            let parent = libc::getpid();
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent died before prctl took effect.
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

/// 180 s per worker; debug builds let tests shorten it.
fn deadline() -> Duration {
    let debug = cfg!(debug_assertions)
        .then(|| {
            std::env::var("HARS_MEDIA_DEADLINE_SECONDS")
                .ok()?
                .parse()
                .ok()
        })
        .flatten();
    Duration::from_secs(debug.unwrap_or(180))
}

async fn run(command: &mut Command) -> anyhow::Result<Vec<u8>> {
    let _profile = crate::profile::Scope::new("native_process_ipc");
    use std::process::Stdio;
    let _permit = WORKERS.acquire().await?;
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    die_with_parent(command);
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
        let status = child.wait().await?;
        if !status.success() {
            return Err(WorkerExit(status).into());
        }
        Ok(bytes)
    };
    tokio::time::timeout(deadline(), work)
        .await
        .map_err(|_| anyhow::Error::new(Deadline))?
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

/// What a lossless remux must preserve: the ordered VCL units (`pictures`), the
/// set of decoder parameter sets (`headers`, in band or in avcC/hvcC) and the
/// ordered other units such as SEI, AUD and end of sequence (`others`).
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Signature {
    pub pictures: String,
    pub headers: BTreeSet<(u8, String)>,
    pub others: String,
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
    searched: usize,
    units: Units,
}
/// Distinct parameter sets a camera stream may carry; a hostile file of unique
/// sets would otherwise grow the header set with the file size.
pub const HEADER_LIMIT: usize = 64;
struct Units {
    hevc: bool,
    hash: Sha256,
    others: Sha256,
    headers: BTreeSet<(u8, String)>,
    pictures: u64,
}
impl Units {
    fn add(&mut self, mut unit: &[u8]) -> anyhow::Result<u8> {
        let hevc = self.hevc;
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
            self.headers
                .insert((kind, hex::encode(Sha256::digest(unit))));
            anyhow::ensure!(
                self.headers.len() <= HEADER_LIMIT,
                "too many distinct parameter sets"
            );
        } else if (hevc && kind < 32) || (!hevc && (1..=5).contains(&kind)) {
            self.hash.update((unit.len() as u32).to_be_bytes());
            self.hash.update(unit);
            self.pictures += 1;
        } else {
            self.others.update([kind]);
            self.others.update((unit.len() as u32).to_be_bytes());
            self.others.update(unit);
        }
        Ok(kind)
    }
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
            searched: 0,
            units: Units {
                hevc,
                hash: Sha256::new(),
                others: Sha256::new(),
                headers: BTreeSet::new(),
                pictures: 0,
            },
        }
    }
    /// Linear in the fed bytes however small the units are: the consumed
    /// prefix is dropped once per call, not once per unit. Bytes before the
    /// stream's first start code hold no unit and are not hashed, as Python's
    /// `Pictures` split leaves them out (`leading_bytes` reports them).
    pub fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.buffer.extend_from_slice(bytes);
        let mut consumed = 0;
        let mut result = Ok(());
        while let Some((_, begin)) = start_code(&self.buffer, consumed) {
            let Some((end, _)) = start_code(&self.buffer, begin.max(self.searched)) else {
                self.searched = self.buffer.len().saturating_sub(3).max(begin);
                break;
            };
            if let Err(e) = self.units.add(&self.buffer[begin..end]) {
                result = Err(e);
                break;
            }
            consumed = end;
            self.searched = 0;
        }
        self.buffer.drain(..consumed);
        self.searched = self.searched.saturating_sub(consumed);
        result?;
        anyhow::ensure!(
            self.buffer.len() <= 32 * 1024 * 1024,
            "NAL unit exceeds bounded memory limit"
        );
        Ok(())
    }
    /// Adds one NAL unit given without its start code (trailing zeros are
    /// framing) and returns its type; for units already split, as in MP4.
    pub fn unit(&mut self, unit: &[u8]) -> anyhow::Result<u8> {
        self.units.add(unit)
    }
    pub fn finish(mut self) -> anyhow::Result<Signature> {
        if let Some((_, begin)) = start_code(&self.buffer, 0) {
            self.units.add(&self.buffer[begin..])?;
        }
        let units = self.units;
        let required: &[u8] = if units.hevc { &[32, 33, 34] } else { &[7, 8] };
        anyhow::ensure!(
            units.pictures > 0
                && required
                    .iter()
                    .all(|k| units.headers.iter().any(|(kind, _)| kind == k)),
            "missing pictures or decoder configuration"
        );
        Ok(Signature {
            pictures: hex::encode(units.hash.finalize()),
            headers: units.headers,
            others: hex::encode(units.others.finalize()),
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
/// Process-wide (pure, ffmpeg, pure failures) remux counts, for diagnostics.
pub fn remux_counts() -> [u64; 3] {
    REMUXED
        .each_ref()
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
}
fn count(index: usize) {
    REMUXED[index].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The server runs conversions in a child process; its remux events are
/// logged again here with only their known fields and the record id, at the
/// level the child chose, so fallbacks and failures stay visible and countable.
pub fn relay_remux_event(record: &str, line: &str) {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let fields = &event["fields"];
    let text = |key: &str| {
        fields[key]
            .as_str()
            .map(|s| s.chars().take(300).collect::<String>())
            .unwrap_or_default()
    };
    match fields["event"].as_str() {
        Some("remux_engine") => tracing::info!(
            event = "remux_engine",
            record,
            engine = text("engine"),
            kind = text("kind"),
            terminal_ticks = fields["terminal_ticks"].as_u64(),
            terminal_basis = text("terminal_basis"),
        ),
        Some("remux_input_prefix") => tracing::warn!(
            event = "remux_input_prefix",
            record,
            kind = text("kind"),
            bytes = fields["bytes"].as_u64(),
        ),
        Some("conversion_kept_previous") => tracing::error!(
            event = "conversion_kept_previous",
            record,
            kind = text("kind"),
            failure_class = text("failure_class"),
            reason_code = text("reason_code"),
            reason = text("reason"),
        ),
        Some(name @ ("remux_fallback" | "remux_failure")) => {
            let level = event["level"].as_str().unwrap_or("WARN");
            macro_rules! relay {
                ($level:ident) => {
                    tracing::$level!(
                        event = name,
                        record,
                        engine = text("engine"),
                        kind = text("kind"),
                        failure_class = text("failure_class"),
                        reason_code = text("reason_code"),
                        reason = text("reason"),
                        fallback = fields["fallback"].as_bool(),
                        fallback_result = text("fallback_result"),
                        fallback_reason = text("fallback_reason"),
                    )
                };
            }
            match level {
                "ERROR" => relay!(error),
                "INFO" => relay!(info),
                _ => relay!(warn),
            }
        }
        _ => {}
    }
}

/// Bytes before a raw Annex B file's first start code, up to the last one that
/// is not zero padding (`leading_zero_8bits`, `zero_byte`), or 0: no NAL unit
/// holds them, so every engine and the input signature leave them out, as
/// libavformat does. Reading stops past the NAL limit, where the engines
/// refuse the file anyway.
pub fn leading_bytes(source: &Path) -> std::io::Result<u64> {
    use std::io::Read;
    let mut file = std::fs::File::open(source)?;
    let mut window = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let (mut passed, mut extent) = (0u64, 0u64);
    loop {
        let n = file.read(&mut chunk)?;
        window.extend_from_slice(&chunk[..n]);
        let found = window.windows(3).position(|w| w == [0, 0, 1]);
        let end = found.unwrap_or(if n == 0 {
            window.len()
        } else {
            window.len().saturating_sub(2)
        });
        if let Some(i) = window[..end].iter().rposition(|b| *b != 0) {
            extent = passed + i as u64 + 1;
        }
        if found.is_some() || n == 0 || passed > crate::pure_remux::annexb::NAL_LIMIT as u64 {
            return Ok(extent);
        }
        passed += end as u64;
        window.drain(..end);
    }
}

/// Internal diagnostics of one remux; `info` is the only product output.
#[derive(Debug)]
pub struct Remuxed {
    pub info: Value,
    pub engine: Engine,
    /// Why the pure engine's attempt was discarded, when FFmpeg wrote the file.
    pub fallback: Option<crate::pure_remux::Failure>,
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

/// Whether a pure engine failure may be retried by the FFmpeg engine, once.
/// Resource failures are not: the same disk, memory or deadline would fail
/// again, and a killed worker may have been the shutdown or the OOM killer.
/// A retried input passes the same checks and limits as the pure output.
pub fn falls_back(class: crate::pure_remux::FailureClass) -> bool {
    use crate::pure_remux::FailureClass as C;
    match class {
        C::Unsupported | C::Invalid | C::Internal | C::Verification => true,
        C::Resource => false,
    }
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
    if kind.ends_with(".hevc") {
        let path = source.to_path_buf();
        // A read error here surfaces, classified, in the engine that reads next.
        if let Ok(Ok(bytes @ 1..)) = tokio::task::spawn_blocking(move || leading_bytes(&path)).await
        {
            tracing::warn!(event = "remux_input_prefix", kind, bytes);
        }
    }
    let mut fallback = None;
    if engine == Engine::Pure
        && let Some(short) = short_for_pure(source, output)
    {
        fallback = Some(short);
    } else if engine == Engine::Pure {
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
            Err(failure) => {
                count(2);
                if !falls_back(failure.class) {
                    tracing::warn!(
                        event = "remux_failure",
                        engine = "pure",
                        kind,
                        failure_class = failure.class.name(),
                        reason_code = failure.code,
                        reason = failure.reason,
                        fallback = false,
                    );
                    return Err(failure.into());
                }
                fallback = Some(failure);
            }
        }
    }
    let result = ffmpeg(source, kind, output, times, &clocks, inspection).await;
    if let Some(failure) = &fallback {
        log_fallback(kind, failure, &result);
    }
    let info = result?;
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

/// Room kept free beyond what an engine writes, as the conversion's own floor (`convert`).
pub const SPACE_MARGIN: u64 = 100 * 1024 * 1024;

/// The pure engine holds its payload next to the MP4 it is copied into, so it needs about twice
/// the input; FFmpeg writes the MP4 alone.
pub fn pure_fits(input: u64, available: u64) -> bool {
    input
        .checked_mul(2)
        .and_then(|n| n.checked_add(SPACE_MARGIN))
        .is_some_and(|needed| available >= needed)
}

/// Why the pure engine is skipped for lack of space, if it is. The FFmpeg engine then writes the
/// single copy. Debug builds let tests state the free space.
fn short_for_pure(source: &Path, output: &Path) -> Option<crate::pure_remux::Failure> {
    let input = std::fs::metadata(source).ok()?.len();
    let dir = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stated = cfg!(debug_assertions)
        .then(|| {
            std::env::var("HARS_AVAILABLE_SPACE_BYTES")
                .ok()?
                .parse()
                .ok()
        })
        .flatten();
    let available = match stated {
        Some(v) => v,
        None => fs2::available_space(dir).ok()?,
    };
    (!pure_fits(input, available)).then(|| {
        crate::pure_remux::Failure::new(
            crate::pure_remux::FailureClass::Unsupported,
            "low_space",
            format!("{available} bytes free for a {input} byte input: one copy only"),
        )
    })
}

/// An unsupported input is routine; damaged input is a warning; an engine
/// error or a file that did not read back is an error even when FFmpeg's
/// checked copy was published.
fn log_fallback(kind: &str, failure: &crate::pure_remux::Failure, result: &anyhow::Result<Value>) {
    use crate::pure_remux::FailureClass as C;
    let (fallback_result, fallback_reason) = match result {
        Ok(_) => ("published", String::new()),
        Err(e) => ("failed", format!("{e:#}").chars().take(300).collect()),
    };
    macro_rules! log {
        ($level:ident) => {
            tracing::$level!(
                event = "remux_fallback",
                from = "pure",
                to = "ffmpeg",
                engine = "pure",
                kind,
                failure_class = failure.class.name(),
                reason_code = failure.code,
                reason = failure.reason,
                fallback = true,
                fallback_result,
                fallback_reason,
            )
        };
    }
    match failure.class {
        C::Unsupported => log!(info),
        C::Invalid | C::Resource => log!(warn),
        C::Internal | C::Verification => log!(error),
    }
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
            .map(|t| (t * 1_000_000.).round_ties_even() as i64)
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

/// Removes its paths when dropped: on failure, and also when the remux future
/// is dropped by a cancellation, a deadline or a superseded conversion. After
/// a successful rename the temporary path no longer exists.
struct Scratch(Vec<PathBuf>);
impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Any media error as a failure class: how a worker process ended, the
/// deadline, then `Failure::classify` (an engine's own class, OS errors).
pub fn failure(error: &anyhow::Error) -> crate::pure_remux::Failure {
    use crate::pure_remux::{Failure, FailureClass as C};
    if error.downcast_ref::<Deadline>().is_some() {
        return Failure::new(C::Resource, "deadline", format!("{error:#}"));
    }
    if let Some(WorkerExit(status)) = error.downcast_ref::<WorkerExit>() {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            let (class, code) = match status.signal() {
                // The kernel's OOM killer, a service stop or an operator.
                Some(libc::SIGKILL) => (C::Resource, "worker_killed"),
                // Rust aborts when an allocation fails (the worker memory limit).
                Some(libc::SIGABRT) => (C::Resource, "out_of_memory"),
                Some(_) => (C::Internal, "worker_crash"),
                // An unwinding panic exits with 101.
                None if status.code() == Some(101) => (C::Internal, "panic"),
                None if status.code() == Some(RESOURCE_EXIT) => (C::Resource, "worker_resource"),
                None => (C::Internal, "worker_error"),
            };
            return Failure::new(class, code, format!("{error:#}"));
        }
        #[cfg(not(unix))]
        return Failure::new(C::Internal, "worker_error", format!("{error:#} {status}"));
    }
    let f = Failure::classify(error);
    match crate::native_video::ffmpeg_resource_code(error) {
        Some(code) if f.class != C::Resource => Failure::new(C::Resource, code, f.reason),
        _ => f,
    }
}

/// The pure engine's attempt. Any failure is returned classified; its output
/// is removed and `remux_inner` decides whether FFmpeg tries the input.
async fn pure(
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
    clocks: &Clocks,
    inspection: Option<&Inspection>,
) -> Result<Value, crate::pure_remux::Failure> {
    use crate::pure_remux::{Failure, FailureClass as C, Job, Reply};
    let temp = temp_for(output);
    let job_path = temp.with_extension("job.json");
    let _scratch = Scratch(vec![temp.clone(), job_path.clone()]);
    let job = Job {
        kind: kind.to_owned(),
        times: times.to_vec(),
        terminal: clocks.terminal.ticks,
        offset: clocks.offset,
    };
    let reply = if let Some(worker) = WORKER_EXECUTABLE.get() {
        crate::store::atomic_json(&job_path, &job)
            .await
            .map_err(|e| Failure::classify(&anyhow::Error::from(e)))?;
        let mut command = Command::new(worker);
        command
            .arg("__pure-remux")
            .arg(source)
            .arg(&job_path)
            .arg(&temp);
        let bytes = run(&mut command).await.map_err(|e| failure(&e))?;
        serde_json::from_slice::<Reply>(&bytes)
            .map_err(|e| Failure::new(C::Internal, "worker_reply", e))?
    } else {
        // Library embedding/tests: a panic must not take the caller down.
        let permit = WORKERS
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| Failure::new(C::Internal, "worker_start", e))?;
        let input = source.to_path_buf();
        let target = temp.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Reply::from(crate::pure_remux::remux(&input, &target, &job))
            }))
            .unwrap_or_else(|_| Reply::Failed(Failure::new(C::Internal, "panic", "panic")))
        })
        .await
        .map_err(|e| Failure::new(C::Internal, "worker_start", e))?
    };
    let outcome = match reply {
        Reply::Done(outcome) => outcome,
        Reply::Failed(failure) => return Err(failure),
    };
    if let Some(input) = inspection
        && input.signature != outcome.input
    {
        return Err(Failure::new(
            C::Verification,
            "input_signature",
            "input signatures disagree",
        ));
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
    .map_err(|e| match Failure::classify(&e) {
        f if f.class == C::Resource => f,
        _ => Failure::new(C::Verification, "verification", format!("{e:#}")),
    })
}

/// The raw file's signature from `NalHasher` alone, independent of
/// libavformat's parser: an FFmpeg copy of raw HEVC must match it, so a
/// damaged input the parser tolerates cannot lose bytes silently.
async fn raw_signature(source: &Path, hevc: bool) -> anyhow::Result<Signature> {
    let _profile = crate::profile::Scope::new("raw_input_signature");
    let source = source.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(source)?;
        let mut hash = NalHasher::new(hevc);
        let mut chunk = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            hash.feed(&chunk[..n])?;
        }
        hash.finish()
    })
    .await?
    .context("camera original fails the input checks")
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
    let clock_path = temp.with_extension("clock.json");
    let _scratch = Scratch(vec![temp.clone(), clock_path.clone()]);
    let terminal = i64::try_from(clocks.terminal.ticks)?;
    let hevc = kind.ends_with(".hevc");
    let before = if hevc {
        let raw = raw_signature(source, true).await?;
        let copied = if let Some(worker) = WORKER_EXECUTABLE.get() {
            crate::store::atomic_json(&clock_path, &times).await?;
            let mut command = Command::new(worker);
            command
                .arg("__native-hevc")
                .arg(source)
                .arg(&clock_path)
                .arg(&temp)
                .arg(terminal.to_string());
            serde_json::from_slice::<Signature>(&run(&mut command).await?)?
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
        };
        anyhow::ensure!(copied == raw, "compressed pictures changed during remux");
        raw
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
    if crate::fault::injected("ffmpeg_remux_corrupt") {
        crate::fault::flip_last_byte(&temp)?;
    }
    start_writeback(&std::fs::File::open(&temp)?);
    let checked = inspect(&temp, target_kind(kind)).await?;
    // The pure engine's read-back also checks this copy: sync samples against
    // the pictures, one picture per sample and the same signature.
    let file = temp.clone();
    let (read, layout) =
        tokio::task::spawn_blocking(move || crate::pure_remux::readback::inspect_copy(&file, hevc))
            .await??;
    anyhow::ensure!(
        read.signature == before,
        "compressed pictures changed during remux"
    );
    anyhow::ensure!(
        layout.durations.last() == Some(&clocks.terminal.ticks),
        "last sample duration changed during remux"
    );
    verify_and_place(&temp, output, kind, times, clocks, &before, checked).await
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
    // Without composition offsets libavformat's stream duration is the decode span plus the last sample.
    // FFmpeg's copy of B-frames reports one that follows its edit list instead; `ffmpeg` checks that
    // copy's last sample in its sample table.
    anyhow::ensure!(
        packets.iter().any(|(pts, dts)| pts != dts)
            || span
                .zip(stream["duration_ts"].as_i64())
                .is_some_and(|((last, first), total)| {
                    u64::try_from(total - (last - first)).ok() == Some(clocks.terminal.ticks)
                }),
        "last sample duration changed during remux"
    );
    verify_mp4_boxes(temp, kind != "qcamera.ts").await?;
    durable_rename(temp, output).await?;
    Ok(info)
}

/// The MP4's bytes and its name reach the disk before it counts as written: a publication with
/// retention off deletes the original it replaces.
async fn durable_rename(temp: &Path, output: &Path) -> std::io::Result<()> {
    let (temp, output) = (temp.to_path_buf(), output.to_path_buf());
    tokio::task::spawn_blocking(move || {
        std::fs::File::open(&temp)?.sync_all()?;
        std::fs::rename(&temp, &output)?;
        sync_parent(&output)
    })
    .await?
}

/// Starts writing `file` back without waiting for it, so the read-back checks overlap the disk
/// write and the fsync before the rename finds little left.
pub fn start_writeback(file: &std::fs::File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: sync_file_range only reads the open descriptor; a failure leaves the fsync to do it all.
        unsafe { libc::sync_file_range(file.as_raw_fd(), 0, 0, libc::SYNC_FILE_RANGE_WRITE) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = file;
}

/// fsync of the directory holding `path`, so a rename or removal in it survives a power cut.
pub fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
