use super::{
    Unsupported,
    annexb::{Bits, Scanner, Unit, configuration_sets, is_parameter_set, kind},
    mp4::{Plan, Writer},
};
use crate::media::{NalHasher, Signature};
use anyhow::{Context, ensure};
use shiguredo_mp4::{
    bitstream::h265::{H265ConstantFrameRate, H265SampleEntryConfig, LengthSize, build_hvc1_box},
    boxes::SampleEntry,
};
use std::{io::Read, path::Path};

pub const TIMESCALE: u32 = 1_000_000;
const READ_CHUNK: usize = 1024 * 1024;

/// `sps_max_num_reorder_pics` of the highest sub-layer, which FFmpeg exposes
/// as `has_b_frames` and the FFmpeg engine requires to be zero.
pub fn reorder_pictures(sps: &[u8]) -> anyhow::Result<u32> {
    ensure!(sps.len() > 2, "truncated SPS");
    let mut r = Bits::new(&sps[2..]);
    r.skip(4)?;
    let sub_layers = r.bits(3)? as usize;
    ensure!(sub_layers <= 6, "invalid SPS sub-layer count");
    r.skip(1)?;
    r.skip(96)?;
    let mut present = Vec::with_capacity(sub_layers);
    for _ in 0..sub_layers {
        present.push((r.bit()?, r.bit()?));
    }
    if sub_layers > 0 {
        r.skip(2 * (8 - sub_layers))?;
    }
    for (profile, level) in present {
        r.skip(if profile == 1 { 88 } else { 0 } + if level == 1 { 8 } else { 0 })?;
    }
    r.ue()?;
    if r.ue()? == 3 {
        r.skip(1)?;
    }
    r.ue()?;
    r.ue()?;
    if r.bit()? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    r.ue()?;
    r.ue()?;
    r.ue()?;
    let first = if r.bit()? == 1 { 0 } else { sub_layers };
    let mut reorder = 0;
    for _ in first..=sub_layers {
        r.ue()?;
        reorder = r.ue()?;
        r.ue()?;
    }
    Ok(reorder)
}

#[derive(Default)]
struct Access {
    open: bool,
    vcl: bool,
    key: bool,
}

trait Sink {
    fn nal(&mut self, kind: u8, unit: &Unit<'_>) -> anyhow::Result<()>;
    /// Ends an access unit; `key` is its IRAP flag.
    fn close(&mut self, key: bool) -> anyhow::Result<()>;
}

/// Access units of a raw HEVC file, split as FFmpeg's HEVC parser does:
/// VPS/SPS/PPS/AUD/prefix SEI/41..44/48..55 or a first slice segment start a
/// new unit once the current one has a picture.
fn walk(
    source: &Path,
    mut raw: Option<&mut NalHasher>,
    sink: &mut impl Sink,
) -> anyhow::Result<()> {
    let mut unit = Access::default();
    let mut on = |u: Unit<'_>| -> anyhow::Result<()> {
        let kind = kind(u.raw, true)?;
        let body = super::annexb::trim(u.raw);
        let starts = matches!(kind, 32..=35 | 39 | 41..=44 | 48..=55)
            || (kind < 32 && body.len() > 2 && body[2] & 0x80 != 0);
        if starts && unit.vcl {
            sink.close(unit.key)?;
            unit = Access::default();
        }
        sink.nal(kind, &u)?;
        if !is_parameter_set(kind, true) {
            unit.open = true;
            if kind < 32 {
                unit.vcl = true;
                unit.key |= (16..=23).contains(&kind);
            }
        }
        Ok(())
    };
    let mut file = std::fs::File::open(source)?;
    let mut scanner = Scanner::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if let Some(hash) = raw.as_deref_mut() {
            hash.feed(&chunk[..n])?;
        }
        scanner.push(&chunk[..n], &mut on)?;
    }
    scanner.finish(&mut on)?;
    if unit.open {
        ensure!(unit.vcl, "HEVC access unit without picture");
        sink.close(unit.key)?;
    }
    Ok(())
}

struct Sizing {
    limit: usize,
    sizes: Vec<(u64, bool)>,
    size: u64,
    first: Vec<(u8, Vec<u8>)>,
}
impl Sink for Sizing {
    fn nal(&mut self, kind: u8, unit: &Unit<'_>) -> anyhow::Result<()> {
        if is_parameter_set(kind, true) {
            if self.sizes.is_empty() {
                self.first.push((kind, unit.raw.to_vec()));
            }
        } else {
            self.size += 4 + unit.sample().len() as u64;
        }
        Ok(())
    }
    fn close(&mut self, key: bool) -> anyhow::Result<()> {
        ensure!(
            self.sizes.len() < self.limit,
            "invalid HEVC packet count or corrupt packet"
        );
        ensure!(
            !self.sizes.is_empty() || key,
            "first HEVC picture is not a keyframe"
        );
        self.sizes.push((std::mem::take(&mut self.size), key));
        Ok(())
    }
}

impl Sink for Writer {
    fn nal(&mut self, kind: u8, unit: &Unit<'_>) -> anyhow::Result<()> {
        if is_parameter_set(kind, true) {
            return Ok(());
        }
        self.write_nal(unit.sample())
    }
    fn close(&mut self, _: bool) -> anyhow::Result<()> {
        self.end_sample()
    }
}

fn entry(sets: &[(u8, Vec<u8>)]) -> anyhow::Result<SampleEntry> {
    let pick = |k: u8| {
        sets.iter()
            .filter(|(kind, _)| *kind == k)
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    let (vps, sps, pps) = (pick(32), pick(33), pick(34));
    ensure!(
        !vps.is_empty() && !sps.is_empty() && !pps.is_empty(),
        "missing pictures or decoder configuration"
    );
    if reorder_pictures(&sps[0])? != 0 {
        return Err(Unsupported("HEVC B frame mapping is not supported").into());
    }
    let entry = build_hvc1_box(
        &vps,
        &sps,
        &pps,
        &H265SampleEntryConfig {
            length_size: LengthSize::FourBytes,
            avg_frame_rate: H265SampleEntryConfig::AVG_FRAME_RATE_UNSPECIFIED,
            constant_frame_rate: H265ConstantFrameRate::Unknown,
        },
    )
    .map_err(|e| anyhow::anyhow!("invalid HEVC decoder configuration: {e}"))?;
    super::mp4::check_dimensions(entry.visual.width, entry.visual.height)?;
    Ok(SampleEntry::Hvc1(entry))
}

/// Two reads of the Annex B input: the first hashes it with the independent
/// `NalHasher` splitter and sizes every sample, the second writes them.
pub fn remux(
    source: &Path,
    output: &Path,
    times: &[f64],
    terminal: u32,
) -> anyhow::Result<Signature> {
    let stamps = times
        .iter()
        .map(|t| (t * f64::from(TIMESCALE)).round() as u64)
        .collect::<Vec<_>>();
    ensure!(
        stamps.windows(2).all(|s| s[1] > s[0]),
        "invalid camera clock"
    );
    if stamps.first() != Some(&0) {
        return Err(Unsupported("HEVC clock does not start at zero").into());
    }
    let mut hash = NalHasher::new(true);
    let mut sizing = Sizing {
        limit: stamps.len(),
        sizes: Vec::with_capacity(stamps.len()),
        size: 0,
        first: Vec::new(),
    };
    walk(source, Some(&mut hash), &mut sizing)?;
    let signature = hash.finish()?;
    ensure!(
        sizing.sizes.len() == stamps.len(),
        "HEVC frame count differs from logged clock"
    );
    let raw = sizing
        .first
        .iter()
        .map(|(k, v)| (*k, v.as_slice()))
        .collect::<Vec<_>>();
    let entry = entry(&configuration_sets(&raw))?;
    let samples = sizing
        .sizes
        .iter()
        .enumerate()
        .map(|(i, &(size, key))| {
            let duration = match stamps.get(i + 1) {
                Some(next) => {
                    u32::try_from(next - stamps[i]).context("HEVC sample duration overflow")?
                }
                None => terminal,
            };
            Ok((size, duration, key))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut writer = Writer::create(
        output,
        Plan {
            entry,
            timescale: TIMESCALE,
            samples,
        },
    )?;
    walk(source, None, &mut writer)?;
    writer.finish()?;
    Ok(signature)
}
