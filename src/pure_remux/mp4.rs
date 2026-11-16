use super::{Unsupported, annexb::NAL_LIMIT};
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

/// Length-prefixed sample bytes, written once while the input is read into an
/// unnamed file next to the output, then copied behind the exact head. The
/// engine's memory therefore stays bounded whatever the input size, the input
/// (possibly on a share) is read once, and no staging file outlives the process.
pub struct Payload {
    file: BufWriter<File>,
    len: u64,
    sample_start: u64,
    limit: u64,
}
impl Payload {
    pub fn create(output: &Path) -> anyhow::Result<Self> {
        Self::with_limit(output, FILE_LIMIT)
    }
    fn with_limit(output: &Path, limit: u64) -> anyhow::Result<Self> {
        let dir = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Ok(Self {
            file: BufWriter::with_capacity(1024 * 1024, tempfile::tempfile_in(dir)?),
            len: 0,
            sample_start: 0,
            limit,
        })
    }
    pub fn nal(&mut self, unit: &[u8]) -> anyhow::Result<()> {
        ensure!(
            !unit.is_empty() && unit.len() <= NAL_LIMIT,
            "NAL unit exceeds bounded memory limit"
        );
        // Refused before the bytes are staged, so a large input is handed to FFmpeg early.
        if self.len + 4 + unit.len() as u64 >= self.limit {
            return Err(Unsupported("output beyond 3 GiB").into());
        }
        self.file.write_all(&(unit.len() as u32).to_be_bytes())?;
        self.file.write_all(unit)?;
        self.len += 4 + unit.len() as u64;
        ensure!(
            self.len - self.sample_start <= SAMPLE_LIMIT,
            "invalid video packet or inspection limit"
        );
        Ok(())
    }
    /// Size of the bytes written since the previous sample ended.
    pub fn pending(&self) -> u64 {
        self.len - self.sample_start
    }
    /// Ends the current sample and returns its size.
    pub fn end_sample(&mut self) -> u64 {
        let size = self.pending();
        self.sample_start = self.len;
        size
    }
}
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

/// Writes `path` (new) as `ftyp`, a `moov` sized exactly for `plan`, and an
/// `mdat` holding `payload`, so the file is faststart with no padding `free`.
pub fn write(path: &Path, plan: &Plan, payload: Payload) -> anyhow::Result<()> {
    let total = plan.samples.iter().map(|s| s.0).sum::<u64>();
    ensure!(
        total == payload.len && payload.pending() == 0,
        "sample table disagrees with its payload"
    );
    if total >= payload.limit {
        return Err(Unsupported("output beyond 3 GiB").into());
    }
    let reserved = exact_reservation(plan)?;
    let mut muxer = Mp4FileMuxer::with_options(Mp4FileMuxerOptions {
        reserved_moov_box_size: reserved,
        ..Default::default()
    })
    .map_err(mux_error)?;
    let mut file = File::options().write(true).create_new(true).open(path)?;
    let initial = muxer.initial_boxes_bytes().to_vec();
    file.write_all(&initial)?;
    let head = initial.len() - 16;
    table(&mut muxer, plan, initial.len() as u64)?;
    crate::fault::point("pure_remux_sample");
    let mut data = payload.file.into_inner().map_err(|e| e.into_error())?;
    data.seek(SeekFrom::Start(0))?;
    // File to file: the kernel copies (copy_file_range), no user-space buffer.
    let copied = std::io::copy(&mut data, &mut file)?;
    ensure!(copied == total, "sample table disagrees with its payload");
    let boxes = muxer.finalize().map_err(mux_error)?;
    ensure!(
        boxes.is_faststart_enabled(),
        "MP4 moov exceeds reserved head"
    );
    let moov = boxes.moov_box_size();
    for (offset, bytes) in boxes.offset_and_bytes_pairs() {
        if offset == 0 {
            let ftyp = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
            ensure!(
                bytes.len() == head && ftyp + moov == head,
                "MP4 head is not exact"
            );
        }
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)?;
    }
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pure_remux::{Failure, FailureClass};

    /// The output bound stops staging at the unit that would cross it, as an unsupported input
    /// (FFmpeg copies it), without writing the input out first.
    #[test]
    fn the_output_bound_refuses_early_as_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let limit = 10_000;
        let mut payload = Payload::with_limit(&dir.path().join("out.mp4"), limit).unwrap();
        let unit = [0x26u8; 996];
        let mut accepted = 0;
        let error = loop {
            match payload.nal(&unit) {
                Ok(()) => accepted += 1,
                Err(e) => break e,
            }
        };
        assert_eq!(accepted, 9);
        assert!(payload.len < limit);
        assert_eq!(payload.len, 9 * 1000);
        assert!(error.downcast_ref::<Unsupported>().is_some(), "{error:#}");
        let failure = Failure::classify(&error.context(crate::pure_remux::Stage::Read));
        assert_eq!(failure.class, FailureClass::Unsupported);
        assert!(crate::media::falls_back(failure.class));
    }
}
