//! Pure Rust lossless remux engine for raw camera originals (H.264 MPEG-TS and
//! raw HEVC), the default engine. Every failure carries a `FailureClass`;
//! `media` decides from it alone whether the FFmpeg engine may try the input.
pub(crate) mod annexb;
pub(crate) mod hevc;
mod mp4;
pub mod readback;
pub(crate) mod ts;

use crate::media::{Inspection, Signature};
use anyhow::ensure;
use std::path::Path;

/// An input outside the range this engine reproduces exactly.
#[derive(Debug)]
pub struct Unsupported(pub &'static str);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported by pure remux: {}", self.0)
    }
}
impl std::error::Error for Unsupported {}

/// Where an engine error arose. It classifies an error that has no more
/// specific cause: the input is judged while it is read, so a later error is
/// the engine's own.
#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Read,
    Write,
    Verify,
}
impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Read => "reading the camera original",
            Self::Write => "writing the MP4",
            Self::Verify => "reading the MP4 back",
        })
    }
}

/// Why a remux attempt ended without a published file (policy: `media`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// A normal input outside the engine's range.
    Unsupported,
    /// The input broke a format rule or a bound while it was read.
    Invalid,
    /// An engine error after the input was accepted, a panic or a crashed worker.
    Internal,
    /// A written file did not read back as the input.
    Verification,
    /// Full or failing storage, the memory limit, the deadline or a killed worker.
    Resource,
}
impl FailureClass {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Invalid => "invalid",
            Self::Internal => "internal",
            Self::Verification => "verification",
            Self::Resource => "resource",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Failure {
    pub class: FailureClass,
    /// Stable code for aggregation; `reason` is the human-readable chain.
    pub code: String,
    pub reason: String,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class.name(), self.reason)
    }
}
impl std::error::Error for Failure {}

/// OS errors that mean the machine, not the input or the engine, failed: conditions that clear
/// (space, quota, file size limit, memory). Every other errno (EIO from a bad sector, EACCES,
/// ENOENT, a dropped mount) fails the same way on every attempt, so it is the input's or the
/// engine's by the `Stage` it came from, and the camera ends with a warning instead of keeping
/// a conversion failing (docs/hars/PURE-REMUX.md).
#[cfg(unix)]
pub fn resource_code(error: &std::io::Error) -> Option<&'static str> {
    Some(match error.raw_os_error()? {
        libc::ENOSPC => "no_space",
        libc::EFBIG => "file_too_large",
        libc::EDQUOT => "quota",
        libc::ENOMEM => "out_of_memory",
        _ => return None,
    })
}
#[cfg(not(unix))]
pub fn resource_code(error: &std::io::Error) -> Option<&'static str> {
    use std::io::ErrorKind as K;
    Some(match error.kind() {
        K::StorageFull => "no_space",
        K::FileTooLarge => "file_too_large",
        K::QuotaExceeded => "quota",
        K::OutOfMemory => "out_of_memory",
        _ => return None,
    })
}

impl Failure {
    pub fn new(class: FailureClass, code: &str, reason: impl std::fmt::Display) -> Self {
        Self {
            class,
            code: code.to_owned(),
            reason: reason.to_string().chars().take(300).collect(),
        }
    }
    /// By type only: an `Unsupported` cause, then an OS resource error, then
    /// the `Stage` the error came from.
    pub fn classify(error: &anyhow::Error) -> Self {
        if let Some(failure) = error.downcast_ref::<Failure>() {
            return failure.clone();
        }
        let reason = format!("{error:#}");
        if error.downcast_ref::<Unsupported>().is_some() {
            return Self::new(FailureClass::Unsupported, "unsupported", reason);
        }
        if let Some(code) = error
            .downcast_ref::<std::io::Error>()
            .and_then(resource_code)
        {
            return Self::new(FailureClass::Resource, code, reason);
        }
        let (class, code) = match error.downcast_ref::<Stage>() {
            Some(Stage::Read) => (FailureClass::Invalid, "invalid_input"),
            Some(Stage::Verify) => (FailureClass::Verification, "readback_mismatch"),
            Some(Stage::Write) | None => (FailureClass::Internal, "engine_error"),
        };
        Self::new(class, code, reason)
    }
}

/// Address space a worker may add once its job is read: twice the largest
/// buffers a valid input can need (NAL splitter and hash buffer, one sample,
/// one PES: 32 MiB each, with growth slack). Internal setting
/// `HARS_PURE_WORKER_MEMORY_MB` changes it in debug builds, for fault tests.
pub const MEMORY_BUDGET_MIB: u64 = 256;

/// Caps the worker's address space at its current size plus `budget_mib`, so
/// an input needing more memory than any valid one makes the worker abort on
/// an allocation failure (`media` reports `out_of_memory`, no fallback)
/// instead of pressing the host. The base size is measured, not assumed: the
/// shared libraries mapped differ between hosts.
pub fn limit_memory(budget_mib: u64) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use anyhow::Context;
        let status = std::fs::read_to_string("/proc/self/status")?;
        let kib = status
            .lines()
            .find_map(|l| l.strip_prefix("VmSize:"))
            .and_then(|v| v.trim().strip_suffix("kB")?.trim().parse::<u64>().ok())
            .context("worker address space size")?;
        let limit = (kib + budget_mib * 1024) * 1024;
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit/setrlimit read and write the one rlimit struct passed.
        unsafe {
            ensure!(
                libc::getrlimit(libc::RLIMIT_AS, &mut current) == 0,
                "read worker memory limit"
            );
            let capped = libc::rlimit {
                rlim_cur: limit.min(current.rlim_max),
                rlim_max: current.rlim_max,
            };
            ensure!(
                libc::setrlimit(libc::RLIMIT_AS, &capped) == 0,
                "set worker memory limit"
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = budget_mib;
    Ok(())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub kind: String,
    pub times: Vec<f64>,
    /// Last sample duration in the output timescale (`terminal_duration`).
    pub terminal: u64,
    /// Smallest source PTS of a TS input; output clocks start there.
    pub offset: i64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    /// Input signature from the independent `media::NalHasher` splitter.
    pub input: Signature,
    pub output: Inspection,
    pub layout: readback::Layout,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply {
    Done(Box<Outcome>),
    Failed(Failure),
}
impl From<anyhow::Result<Outcome>> for Reply {
    fn from(result: anyhow::Result<Outcome>) -> Self {
        match result {
            Ok(outcome) => Self::Done(Box::new(outcome)),
            Err(error) => Self::Failed(Failure::classify(&error)),
        }
    }
}

/// Writes `output` (a new path) and reads it back with `readback`, which
/// hashes it with the same independent splitter as the input. Nothing is left
/// at `output` on failure.
pub fn remux(source: &Path, output: &Path, job: &Job) -> anyhow::Result<Outcome> {
    ensure!(source != output, "source and output must differ");
    ensure!(!output.exists(), "remux output must be new");
    ensure!(
        !job.times.is_empty()
            && job.times.iter().all(|v| v.is_finite())
            && job.times.windows(2).all(|v| v[1] > v[0]),
        "invalid camera clock"
    );
    let hevc = match job.kind.as_str() {
        "fcamera.hevc" | "ecamera.hevc" => true,
        "qcamera.ts" => false,
        _ => anyhow::bail!("unsupported camera original format"),
    };
    let terminal = u32::try_from(job.terminal)
        .ok()
        .filter(|t| *t > 0)
        .ok_or(Unsupported("last sample duration"))?;
    let result = (|| {
        let input = if hevc {
            hevc::remux(source, output, &job.times, terminal)?
        } else {
            ts::remux(source, output, job.times.len(), job.offset, terminal)?
        };
        if crate::fault::injected("pure_remux_error") {
            return Err(anyhow::anyhow!("injected engine error").context(Stage::Write));
        }
        if crate::fault::injected("pure_remux_panic") {
            panic!("injected engine panic");
        }
        if crate::fault::injected("pure_remux_corrupt") {
            crate::fault::flip_last_byte(output)?;
        }
        let _profile = crate::profile::Scope::new("pure_readback");
        verify(output, hevc, job, input).map_err(|e| e.context(Stage::Verify))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result
}

fn verify(output: &Path, hevc: bool, job: &Job, input: Signature) -> anyhow::Result<Outcome> {
    let (checked, layout) = readback::inspect(output, hevc)?;
    crate::media::validate_times(&checked.info, &job.times)?;
    ensure!(
        checked.signature == input,
        "compressed pictures changed during remux"
    );
    ensure!(
        layout.top_level == ["ftyp", "moov", "mdat"],
        "unexpected MP4 layout"
    );
    // This engine writes no composition offsets, so its media ends with its last sample. Not a check for any
    // MP4: FFmpeg's copy of a B-frame input ends its media at the last presentation time instead.
    ensure!(
        layout.media_duration == layout.durations.iter().sum::<u64>(),
        "MP4 media duration disagrees with its samples"
    );
    ensure!(
        layout.durations.last() == Some(&job.terminal),
        "last sample duration changed during remux"
    );
    Ok(Outcome {
        input,
        output: checked,
        layout,
    })
}
