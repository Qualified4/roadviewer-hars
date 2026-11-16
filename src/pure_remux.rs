//! Pure Rust lossless remux engine for raw camera originals (H.264 MPEG-TS and
//! raw HEVC), the default engine. Inputs it cannot copy exactly as the FFmpeg
//! engine would are refused with `Unsupported`; `media` then uses FFmpeg.
mod annexb;
mod hevc;
mod mp4;
pub mod readback;
mod ts;

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
    Unsupported { reason: String },
    Failed { reason: String },
}
impl From<anyhow::Result<Outcome>> for Reply {
    fn from(result: anyhow::Result<Outcome>) -> Self {
        match result {
            Ok(outcome) => Self::Done(Box::new(outcome)),
            Err(error) => match error.downcast_ref::<Unsupported>() {
                Some(reason) => Self::Unsupported {
                    reason: reason.0.to_owned(),
                },
                None => Self::Failed {
                    reason: format!("{error:#}"),
                },
            },
        }
    }
}

/// Writes `output` (a new path) and reads it back with `readback`, which
/// shares no code with the writer. Nothing is left at `output` on failure.
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
        ensure!(
            layout.durations.last() == Some(&job.terminal),
            "last sample duration changed during remux"
        );
        Ok(Outcome {
            input,
            output: checked,
            layout,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result
}
