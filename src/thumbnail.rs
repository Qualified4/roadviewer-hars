//! Bounded pixel decoding without FFmpeg (docs/hars/THUMBNAIL-DECODER.md); all selection and luma
//! calculations are Rust.
mod jpeg;
mod scale;
mod video;

use anyhow::Context;
use scale::Image;
use serde_json::{Value, json};
use std::path::Path;

const WIDTH: usize = 320;
/// Python `MAX_PACKETS`: about 7 s at 20 fps.
const MAX_PACKETS: usize = 140;
/// Address space the `__thumbnail` worker may add to its own (`pure_remux::limit_memory`): the
/// decoder's reference pictures at the largest accepted size (4096x2160) with room to spare.
pub const MEMORY_BUDGET_MIB: u64 = 512;

fn stats(image: &Image) -> (f64, f64, u8, u8) {
    let mut hist = [0u64; 256];
    let plane = &image.y;
    for row in (0..plane.height).step_by(2) {
        for col in (0..plane.width).step_by(2) {
            hist[plane.data[row * plane.width + col] as usize] += 1;
        }
    }
    let n = hist.iter().sum::<u64>() as f64;
    let mean = hist
        .iter()
        .enumerate()
        .map(|(i, c)| i as f64 * (*c as f64))
        .sum::<f64>()
        / n;
    let std = (hist
        .iter()
        .enumerate()
        .map(|(i, c)| *c as f64 * (i as f64 - mean).powi(2))
        .sum::<f64>()
        / n)
        .sqrt();
    let mut count = 0;
    let mut low = 0;
    let mut high = 0;
    let mut found = (false, false);
    for (i, c) in hist.iter().enumerate() {
        count += c;
        if !found.0 && count as f64 >= n * 0.01 {
            low = i as u8;
            found.0 = true;
        }
        if !found.1 && count as f64 >= n * 0.99 {
            high = i as u8;
            found.1 = true;
        }
    }
    (mean, std, low, high)
}
fn score((mean, std, _, _): (f64, f64, u8, u8)) -> f64 {
    std * (mean / 60.).min(1.) * ((250. - mean).max(0.) / 60.).min(1.)
}
fn level(image: &mut Image, (mean, _, low, high): (f64, f64, u8, u8)) {
    let gain = (235. / (high as f64 - low as f64).max(1.)).min(3.);
    let stretched = ((mean - low as f64) * gain).clamp(1., 255.);
    let gamma = if stretched >= 100. {
        1.
    } else {
        ((110f64 / 255.).ln() / (stretched / 255.).ln()).max(0.6)
    };
    let lut: Vec<u8> = (0..256)
        .map(|v| {
            let stretched = ((v as f64 - low as f64) * gain)
                .round_ties_even()
                .clamp(0., 255.);
            (255. * (stretched / 255.).powf(gamma))
                .round_ties_even()
                .min(255.) as u8
        })
        .collect();
    for v in &mut image.y.data {
        *v = lut[*v as usize];
    }
}

pub fn make(source: &Path, output: &Path) -> anyhow::Result<Value> {
    let _profile = crate::profile::Scope::new("thumbnail_native");
    let mut candidates: Vec<(u8, Image)> = vec![];
    let mut first = None;
    let mut start = None;
    video::decode(source, MAX_PACKETS, |frame| {
        let t = frame.time;
        let start = *start.get_or_insert(t);
        if first.is_none() {
            first = Some(scale::small(&frame.image, WIDTH, frame.full_range)?);
        }
        for second in 1..=5u8 {
            if !candidates.iter().any(|v| v.0 == second) && t - start >= f64::from(second) {
                candidates.push((second, scale::small(&frame.image, WIDTH, frame.full_range)?));
            }
        }
        Ok((1..=5u8)
            .find(|second| !candidates.iter().any(|v| v.0 == *second))
            .map(|second| start + f64::from(second)))
    })?;
    if candidates.is_empty()
        && let Some(first) = first
    {
        candidates.push((0, first));
    }
    let index = candidates
        .iter()
        .enumerate()
        .max_by(|(i, a), (j, b)| {
            score(stats(&a.1))
                .total_cmp(&score(stats(&b.1)))
                .then_with(|| j.cmp(i))
        })
        .map(|v| v.0)
        .context("no decodable thumbnail frame")?;
    let (second, mut image) = candidates.swap_remove(index);
    let measured = stats(&image);
    level(&mut image, measured);
    let jpeg = jpeg::encode(&image)?;
    anyhow::ensure!(jpeg.starts_with(&[0xff, 0xd8]), "JPEG encoding failed");
    let temp = output.with_extension("jpg.tmp");
    std::fs::write(&temp, jpeg)?;
    std::fs::rename(temp, output)?;
    let meta = json!({"version":2,"score":crate::replay::r(score(measured),2)});
    let metadata = output.with_extension("json");
    let temp = metadata.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec(&meta)?)?;
    std::fs::rename(temp, metadata)?;
    Ok(json!({"second":second,"score":meta["score"]}))
}

#[cfg(test)]
mod tests;
