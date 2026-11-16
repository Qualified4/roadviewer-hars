//! Pure Rust lossless remux experiment (feature `pure-remux`, off by default).
//! It mirrors the FFmpeg engine's input contract and checks so both can be
//! compared on the same corpus; product conversion never calls it.
mod annexb;
mod hevc;
mod mp4;
mod ts;

pub use mp4::{Checked, inspect};

use crate::media::Signature;
use anyhow::{bail, ensure};
use std::path::Path;

#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub signature: Signature,
    pub frames: usize,
    pub keyframes: usize,
    pub timescale: u32,
    pub verified: bool,
}

fn validate_times(checked: &Checked, times: &[f64]) -> anyhow::Result<()> {
    let scale = f64::from(checked.timescale);
    let mut stamps = checked
        .pts
        .iter()
        .map(|p| *p as f64 / scale)
        .collect::<Vec<_>>();
    stamps.sort_by(f64::total_cmp);
    ensure!(
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

/// One streaming pass reads, hashes and muxes the input. With `verify`, the
/// finished file is read back once in pure Rust, as the FFmpeg engine re-inspects
/// its output, before the atomic rename.
pub fn remux(
    source: &Path,
    kind: &str,
    output: &Path,
    times: &[f64],
    verify: bool,
) -> anyhow::Result<Report> {
    ensure!(source != output, "source and output must differ");
    ensure!(
        !times.is_empty()
            && times.iter().all(|v| v.is_finite())
            && times.windows(2).all(|v| v[1] > v[0]),
        "invalid camera clock"
    );
    let hevc = match kind {
        "fcamera.hevc" | "ecamera.hevc" => true,
        "qcamera.ts" => false,
        _ => bail!("unsupported camera original format"),
    };
    let timescale = if hevc { hevc::TIMESCALE } else { ts::TIMESCALE };
    let temp = output.with_extension(format!("{}.tmp.mp4", crate::store::id()));
    let result = (|| {
        let mut writer = mp4::Writer::create(&temp, timescale, times.len())?;
        let mut pictures = annexb::Pictures::new(hevc);
        let (frames, keyframes) = if hevc {
            let done = hevc::mux(source, &mut writer, &mut pictures, times)?;
            (done.frames, done.keyframes)
        } else {
            let done = ts::mux(source, &mut writer, &mut pictures)?;
            (done.frames, done.keyframes)
        };
        ensure!(
            frames == times.len(),
            "video frame count or timestamps disagree with log"
        );
        writer.finish()?;
        let signature = pictures.finish()?;
        if verify {
            let checked = inspect(&temp, hevc)?;
            ensure!(
                checked.signature == signature,
                "compressed pictures changed during remux"
            );
            validate_times(&checked, times)?;
        }
        std::fs::rename(&temp, output)?;
        Ok(Report {
            signature,
            frames,
            keyframes,
            timescale,
            verified: verify,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
