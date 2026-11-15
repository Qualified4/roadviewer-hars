use super::{
    annexb::{Bits, Pictures, Scanner, is_parameter_set},
    mp4::Writer,
};
use anyhow::{Context, ensure};
use shiguredo_mp4::{
    bitstream::h265::{H265ConstantFrameRate, H265SampleEntryConfig, LengthSize, build_hvc1_box},
    boxes::SampleEntry,
};
use std::{io::Read, path::Path};

pub const TIMESCALE: u32 = 1_000_000;
// Same last-sample duration the FFmpeg engine (`native_video::remux_hevc`) assigns.
const LAST_DURATION: f64 = 0.05;
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
struct Unit {
    open: bool,
    vcl: bool,
    key: bool,
}

struct Muxer<'a> {
    writer: &'a mut Writer,
    pictures: &'a mut Pictures,
    stamps: &'a [u64],
    last_duration: u32,
    parameter_sets: [Vec<Vec<u8>>; 3],
    entry: Option<SampleEntry>,
    unit: Unit,
    index: usize,
    keyframes: usize,
}
impl Muxer<'_> {
    fn entry(&mut self) -> anyhow::Result<SampleEntry> {
        let [vps, sps, pps] = &self.parameter_sets;
        ensure!(
            !vps.is_empty() && !sps.is_empty() && !pps.is_empty(),
            "missing pictures or decoder configuration"
        );
        ensure!(
            reorder_pictures(&sps[0])? == 0,
            "HEVC B frame mapping is not supported"
        );
        let entry = build_hvc1_box(
            vps,
            sps,
            pps,
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
    fn close(&mut self) -> anyhow::Result<()> {
        if !self.unit.open {
            return Ok(());
        }
        ensure!(self.unit.vcl, "HEVC access unit without picture");
        ensure!(
            self.index < self.stamps.len(),
            "invalid HEVC packet count or corrupt packet"
        );
        let entry = match self.entry {
            None => {
                let entry = self.entry()?;
                self.entry = Some(entry.clone());
                Some(entry)
            }
            Some(_) => None,
        };
        let duration = match self.stamps.get(self.index + 1) {
            Some(next) => next - self.stamps[self.index],
            None => self.last_duration.into(),
        };
        ensure!(
            self.index > 0 || self.unit.key,
            "first HEVC picture is not a keyframe"
        );
        let span = self.writer.end_data()?;
        self.writer.append(
            span,
            u32::try_from(duration).context("HEVC sample duration overflow")?,
            self.unit.key,
            None,
            entry,
        )?;
        self.keyframes += usize::from(self.unit.key);
        self.index += 1;
        self.unit = Unit::default();
        Ok(())
    }
    fn nal(&mut self, unit: &[u8]) -> anyhow::Result<()> {
        let kind = self.pictures.add(unit)?;
        // FFmpeg's HEVC parser boundaries: these units start a new access unit
        // once the current one has a picture; so does a first slice segment.
        let starts = matches!(kind, 32..=35 | 39 | 41..=44 | 48..=55)
            || (kind < 32 && unit.len() > 2 && unit[2] & 0x80 != 0);
        if starts && self.unit.vcl {
            self.close()?;
        }
        if is_parameter_set(kind, true) {
            if self.entry.is_none() {
                self.parameter_sets[usize::from(kind - 32)].push(unit.to_vec());
            }
            return Ok(());
        }
        self.unit.open = true;
        if kind < 32 {
            self.unit.vcl = true;
            self.unit.key |= (16..=23).contains(&kind);
        }
        self.writer.write_nal(unit)
    }
}

pub struct Outcome {
    pub frames: usize,
    pub keyframes: usize,
}

/// One read of the Annex B input hashes it and writes every sample.
pub fn mux(
    source: &Path,
    writer: &mut Writer,
    pictures: &mut Pictures,
    times: &[f64],
) -> anyhow::Result<Outcome> {
    let stamps = times
        .iter()
        .map(|t| (t * f64::from(TIMESCALE)).round() as u64)
        .collect::<Vec<_>>();
    ensure!(
        stamps.first() == Some(&0) && stamps.windows(2).all(|s| s[1] > s[0]),
        "invalid camera clock"
    );
    let mut muxer = Muxer {
        writer,
        pictures,
        stamps: &stamps,
        last_duration: (LAST_DURATION * f64::from(TIMESCALE)).round() as u32,
        parameter_sets: Default::default(),
        entry: None,
        unit: Unit::default(),
        index: 0,
        keyframes: 0,
    };
    let mut file = std::fs::File::open(source)?;
    let mut scanner = Scanner::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        scanner.push(&chunk[..n], |unit| muxer.nal(unit))?;
    }
    scanner.finish(|unit| muxer.nal(unit))?;
    muxer.close()?;
    ensure!(
        muxer.index == stamps.len(),
        "HEVC frame count differs from logged clock"
    );
    Ok(Outcome {
        frames: muxer.index,
        keyframes: muxer.keyframes,
    })
}
