use super::annexb::NAL_LIMIT;
use anyhow::{Context, ensure};
use shiguredo_mp4::{
    TrackKind,
    boxes::SampleEntry,
    mux::{Mp4FileMuxer, Mp4FileMuxerOptions, Sample},
};
use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
    num::NonZeroU32,
    path::Path,
};

pub const SAMPLE_LIMIT: u64 = 32 * 1024 * 1024;
// stco offsets that straddle 4 GiB would switch the table to co64 between the
// sizing run and the real one; camera segments are far below this.
const FILE_LIMIT: u64 = 3 << 30;
// Head size the muxer reserves around the free payload: free header + its own
// ftyp update margin (`RESERVED_FTYP_UPDATE_FREE_PAYLOAD_SIZE`).
const HEAD_OVERHEAD: usize = 8 + 64;

fn mux_error(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("MP4 mux failed: {error}")
}

pub fn check_dimensions(w: u16, h: u16) -> anyhow::Result<()> {
    ensure!(
        w > 0 && w <= 4096 && h > 0 && h <= 2160 && w.is_multiple_of(2) && h.is_multiple_of(2),
        "unsupported camera dimensions"
    );
    Ok(())
}

/// Every sample of the output, known from a first read of the input.
pub struct Plan {
    pub entry: SampleEntry,
    pub timescale: u32,
    /// (bytes incl. 4-byte length prefixes, duration, keyframe)
    pub samples: Vec<(u64, u32, bool)>,
}

fn table(muxer: &mut Mp4FileMuxer, plan: &Plan, start: u64) -> anyhow::Result<()> {
    let timescale = NonZeroU32::new(plan.timescale).context("invalid timescale")?;
    let mut offset = start;
    for (i, &(size, duration, keyframe)) in plan.samples.iter().enumerate() {
        muxer
            .append_sample(&Sample {
                track_kind: TrackKind::Video,
                sample_entry: (i == 0).then(|| plan.entry.clone()),
                keyframe,
                timescale,
                duration,
                composition_time_offset: None,
                data_offset: offset,
                data_size: usize::try_from(size)?,
            })
            .map_err(mux_error)?;
        offset += size;
    }
    Ok(())
}

/// Head reservation for which `ftyp + moov` fills the head exactly, so the
/// file is faststart with no padding `free` box and no rewrite of `mdat`.
fn exact_reservation(plan: &Plan) -> anyhow::Result<usize> {
    let bound = 4096usize.saturating_add(plan.samples.len().saturating_mul(48));
    let mut sizing = Mp4FileMuxer::with_options(Mp4FileMuxerOptions {
        reserved_moov_box_size: bound,
        ..Default::default()
    })
    .map_err(mux_error)?;
    let initial = sizing.initial_boxes_bytes();
    let first_ftyp = u32::from_be_bytes(initial[..4].try_into()?) as usize;
    let start = initial.len() as u64;
    table(&mut sizing, plan, start)?;
    let boxes = sizing.finalize().map_err(mux_error)?;
    ensure!(
        boxes.is_faststart_enabled(),
        "MP4 moov exceeds reserved head"
    );
    let (_, head) = boxes
        .offset_and_bytes_pairs()
        .next()
        .context("missing MP4 head")?;
    let final_ftyp = u32::from_be_bytes(head[..4].try_into()?) as usize;
    (final_ftyp + boxes.moov_box_size())
        .checked_sub(first_ftyp + HEAD_OVERHEAD)
        .context("MP4 head smaller than its reservation")
}

/// Streams length-prefixed samples straight into `mdat` behind a head sized
/// exactly for the final `moov`.
pub struct Writer {
    file: BufWriter<File>,
    muxer: Mp4FileMuxer,
    head: usize,
    plan: Plan,
    position: u64,
    sample_start: u64,
    index: usize,
}
impl Writer {
    pub fn create(path: &Path, plan: Plan) -> anyhow::Result<Self> {
        let total = plan.samples.iter().map(|s| s.0).sum::<u64>();
        ensure!(total < FILE_LIMIT, "unsupported: output beyond 3 GiB");
        let reserved = exact_reservation(&plan)?;
        let muxer = Mp4FileMuxer::with_options(Mp4FileMuxerOptions {
            reserved_moov_box_size: reserved,
            ..Default::default()
        })
        .map_err(mux_error)?;
        let mut file = BufWriter::with_capacity(
            1024 * 1024,
            File::options().write(true).create_new(true).open(path)?,
        );
        let initial = muxer.initial_boxes_bytes();
        file.write_all(initial)?;
        let position = initial.len() as u64;
        Ok(Self {
            file,
            head: initial.len() - 16,
            muxer,
            plan,
            position,
            sample_start: position,
            index: 0,
        })
    }
    pub fn write_nal(&mut self, unit: &[u8]) -> anyhow::Result<()> {
        ensure!(
            !unit.is_empty() && unit.len() <= NAL_LIMIT,
            "NAL unit exceeds bounded memory limit"
        );
        self.file.write_all(&(unit.len() as u32).to_be_bytes())?;
        self.file.write_all(unit)?;
        self.position += 4 + unit.len() as u64;
        ensure!(
            self.position - self.sample_start <= SAMPLE_LIMIT,
            "invalid video packet or inspection limit"
        );
        Ok(())
    }
    /// Closes the bytes written since the previous call as the next planned
    /// sample; a second read that disagrees with the first one is an error.
    pub fn end_sample(&mut self) -> anyhow::Result<()> {
        let size = self.position - self.sample_start;
        let &(planned, duration, keyframe) = self
            .plan
            .samples
            .get(self.index)
            .context("input changed between reads")?;
        ensure!(size == planned && size > 0, "input changed between reads");
        self.muxer
            .append_sample(&Sample {
                track_kind: TrackKind::Video,
                sample_entry: (self.index == 0).then(|| self.plan.entry.clone()),
                keyframe,
                timescale: NonZeroU32::new(self.plan.timescale).context("invalid timescale")?,
                duration,
                composition_time_offset: None,
                data_offset: self.sample_start,
                data_size: usize::try_from(size)?,
            })
            .map_err(mux_error)?;
        self.sample_start = self.position;
        self.index += 1;
        Ok(())
    }
    pub fn finish(mut self) -> anyhow::Result<()> {
        ensure!(
            self.index == self.plan.samples.len(),
            "input changed between reads"
        );
        let boxes = self.muxer.finalize().map_err(mux_error)?;
        ensure!(
            boxes.is_faststart_enabled(),
            "MP4 moov exceeds reserved head"
        );
        let moov = boxes.moov_box_size();
        let mut file = self.file.into_inner().map_err(|e| e.into_error())?;
        for (offset, bytes) in boxes.offset_and_bytes_pairs() {
            if offset == 0 {
                let ftyp = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
                ensure!(
                    bytes.len() == self.head && ftyp + moov == self.head,
                    "MP4 head is not exact"
                );
            }
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(bytes)?;
        }
        file.flush()?;
        Ok(())
    }
}
