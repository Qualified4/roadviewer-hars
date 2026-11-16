use super::{
    Stage, Unsupported,
    annexb::{Bits, Scanner, Sets, Unit, is_parameter_set, kind},
    mp4::{Payload, Plan},
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

/// Access units of a raw HEVC file, split as FFmpeg's HEVC parser
/// (`hevc_find_frame_end`) does: VPS..EOB, prefix SEI, 41..44 and 48..55 end
/// a picture, and so does the first slice segment of a picture of a type the
/// parser treats as a slice (TRAIL..RASL, BLA..CRA).
fn walk(source: &Path, raw: &mut NalHasher, sink: &mut Sizing) -> anyhow::Result<()> {
    let mut unit = Access::default();
    let mut on = |u: Unit<'_>| -> anyhow::Result<()> {
        let kind = kind(u.raw, true)?;
        let body = super::annexb::trim(u.raw);
        let slice = kind <= 9 || (16..=21).contains(&kind);
        let starts = matches!(kind, 32..=37 | 39 | 41..=44 | 48..=55)
            || (slice && body.len() > 2 && body[2] & 0x80 != 0);
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
        raw.feed(&chunk[..n])?;
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
    sets: Sets,
    payload: Payload,
}
impl Sizing {
    fn nal(&mut self, kind: u8, unit: &Unit<'_>) -> anyhow::Result<()> {
        if is_parameter_set(kind, true) {
            if self.sizes.is_empty() {
                self.sets.add(kind, unit.raw)?;
            } else if !self.sets.contains(kind, unit.raw) {
                // hvc1 keeps one configuration out of band; a new set would
                // be lost from the samples.
                return Err(Unsupported("HEVC parameter sets change mid-stream").into());
            }
            Ok(())
        } else {
            self.payload.nal(unit.sample())
        }
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
        self.sizes.push((self.payload.end_sample(), key));
        Ok(())
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

/// The upload check of a raw HEVC original: the size its first SPS states, from the parameter
/// sets before its first picture. Reads no further than that picture, within the NAL bound.
pub fn probe(source: &Path) -> anyhow::Result<serde_json::Value> {
    let mut sets = Sets::default();
    let picture = std::cell::Cell::new(false);
    let mut on = |u: Unit<'_>| -> anyhow::Result<()> {
        if picture.get() {
            return Ok(());
        }
        let kind = kind(u.raw, true)?;
        if is_parameter_set(kind, true) {
            sets.add(kind, u.raw)?;
        } else if kind < 32 {
            picture.set(true);
        }
        Ok(())
    };
    let mut file = std::fs::File::open(source)?;
    let mut scanner = Scanner::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut read = 0;
    while !picture.get() {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        read += n;
        ensure!(
            read <= super::annexb::NAL_LIMIT + READ_CHUNK,
            "no HEVC picture within the NAL bound"
        );
        scanner.push(&chunk[..n], &mut on)?;
    }
    if !picture.get() {
        scanner.finish(&mut on)?;
    }
    ensure!(picture.get(), "missing pictures or decoder configuration");
    let sets = sets.configuration();
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
    Ok(
        serde_json::json!({"streams":[{"codec_type":"video","codec_name":"hevc","width":entry.visual.width,"height":entry.visual.height}]}),
    )
}

/// One read of the Annex B input hashes it with the independent `NalHasher`
/// splitter, sizes every sample and stages the sample bytes (`Payload`).
pub fn remux(
    source: &Path,
    output: &Path,
    times: &[f64],
    terminal: u32,
) -> anyhow::Result<Signature> {
    // Python's `round()` (half to even) sets these stamps in the reference;
    // log clocks ending in exactly 500 ns land on a tie.
    let stamps = times
        .iter()
        .map(|t| (t * f64::from(TIMESCALE)).round_ties_even() as u64)
        .collect::<Vec<_>>();
    ensure!(
        stamps.windows(2).all(|s| s[1] > s[0]),
        "invalid camera clock"
    );
    if stamps.first() != Some(&0) {
        return Err(Unsupported("HEVC clock does not start at zero").into());
    }
    let (plan, signature, payload) = (|| {
        let _profile = crate::profile::Scope::new("pure_hevc_read");
        let mut hash = NalHasher::new(true);
        let mut sizing = Sizing {
            limit: stamps.len(),
            sizes: Vec::with_capacity(stamps.len()),
            sets: Sets::default(),
            payload: Payload::create(output)?,
        };
        walk(source, &mut hash, &mut sizing)?;
        let signature = hash.finish()?;
        ensure!(
            sizing.sizes.len() == stamps.len(),
            "HEVC frame count differs from logged clock"
        );
        let entry = entry(&sizing.sets.configuration())?;
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
        let plan = Plan {
            entry,
            timescale: TIMESCALE,
            samples,
        };
        anyhow::Ok((plan, signature, sizing.payload))
    })()
    .context(Stage::Read)?;
    let _profile = crate::profile::Scope::new("pure_hevc_write");
    super::mp4::write(output, &plan, payload).context(Stage::Write)?;
    Ok(signature)
}
