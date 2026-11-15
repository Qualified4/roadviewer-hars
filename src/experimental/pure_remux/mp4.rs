use super::annexb::{NAL_LIMIT, Pictures, is_parameter_set};
use crate::media::Signature;
use anyhow::{Context, ensure};
use shiguredo_mp4::{
    TrackKind,
    boxes::SampleEntry,
    demux::{Input, Mp4FileDemuxer},
    mux::{Mp4FileMuxer, Mp4FileMuxerOptions, Sample},
};
use std::{
    fs::File,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    num::NonZeroU32,
    path::Path,
};

const SAMPLE_LIMIT: u64 = 32 * 1024 * 1024;
const MOOV_LIMIT: u64 = 16 * 1024 * 1024;
// Upper bound per sample of stts+ctts+stsz+stss+stsc+co64 entries, so the
// reserved head always fits moov and the file stays faststart without a rewrite.
const MOOV_BYTES_PER_SAMPLE: usize = 48;

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

/// Streams length-prefixed samples straight into `mdat` behind a reserved
/// head; `finish` writes `moov` into that head.
pub struct Writer {
    file: BufWriter<File>,
    muxer: Mp4FileMuxer,
    timescale: NonZeroU32,
    position: u64,
    sample_start: u64,
}
impl Writer {
    pub fn create(path: &Path, timescale: u32, samples: usize) -> anyhow::Result<Self> {
        let reserved = 4096usize.saturating_add(samples.saturating_mul(MOOV_BYTES_PER_SAMPLE));
        let muxer = Mp4FileMuxer::with_options(Mp4FileMuxerOptions {
            reserved_moov_box_size: reserved,
            ..Default::default()
        })
        .map_err(mux_error)?;
        let mut file = BufWriter::with_capacity(
            1024 * 1024,
            File::options().write(true).create_new(true).open(path)?,
        );
        let head = muxer.initial_boxes_bytes();
        file.write_all(head)?;
        let position = head.len() as u64;
        Ok(Self {
            file,
            muxer,
            timescale: NonZeroU32::new(timescale).context("invalid timescale")?,
            position,
            sample_start: position,
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
    /// Closes the bytes written since the previous call as one sample.
    pub fn end_data(&mut self) -> anyhow::Result<Span> {
        let size = self.position - self.sample_start;
        ensure!(size > 0, "empty video sample");
        let span = Span {
            offset: self.sample_start,
            size: size as usize,
        };
        self.sample_start = self.position;
        Ok(span)
    }
    /// Sample tables lag the data when the duration needs the next DTS.
    pub fn append(
        &mut self,
        span: Span,
        duration: u32,
        keyframe: bool,
        composition_time_offset: Option<i64>,
        sample_entry: Option<SampleEntry>,
    ) -> anyhow::Result<()> {
        self.muxer
            .append_sample(&Sample {
                track_kind: TrackKind::Video,
                sample_entry,
                keyframe,
                timescale: self.timescale,
                duration,
                composition_time_offset,
                data_offset: span.offset,
                data_size: span.size,
            })
            .map_err(mux_error)
    }
    pub fn finish(mut self) -> anyhow::Result<()> {
        let boxes = self.muxer.finalize().map_err(mux_error)?;
        ensure!(
            boxes.is_faststart_enabled(),
            "MP4 moov exceeds reserved head"
        );
        let mut file = self.file.into_inner().map_err(|e| e.into_error())?;
        for (offset, bytes) in boxes.offset_and_bytes_pairs() {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(bytes)?;
        }
        file.flush()?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct Span {
    offset: u64,
    size: usize,
}

pub struct Checked {
    pub signature: Signature,
    pub timescale: u32,
    pub pts: Vec<i64>,
    pub dts: Vec<u64>,
    pub keyframes: Vec<usize>,
    pub duration: u64,
}

/// Top-level layout only; returns the `moov` end so it can be read once.
fn moov_end(file: &mut File) -> anyhow::Result<u64> {
    let len = file.metadata()?.len();
    let mut at = 0;
    let mut moov = None;
    let mut mdat = None;
    while at < len {
        ensure!(len - at >= 8, "truncated MP4 box");
        file.seek(SeekFrom::Start(at))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8])?;
        let small = u32::from_be_bytes(header[..4].try_into()?);
        let (size, prefix) = match small {
            1 => {
                file.read_exact(&mut header[8..])?;
                (u64::from_be_bytes(header[8..].try_into()?), 16)
            }
            0 => (len - at, 8),
            n => (u64::from(n), 8),
        };
        ensure!(size >= prefix && size <= len - at, "invalid MP4 box size");
        match &header[4..8] {
            b"moov" => {
                ensure!(
                    moov.is_none() && size <= MOOV_LIMIT,
                    "MP4 metadata exceeds limit"
                );
                moov = Some(at + size);
            }
            b"mdat" => mdat = mdat.or(Some(at)),
            _ => {}
        }
        at += size;
    }
    let (end, mdat) = moov
        .zip(mdat)
        .context("missing faststart/decoder configuration")?;
    ensure!(end <= mdat, "missing faststart/decoder configuration");
    Ok(end)
}

/// Independent pure Rust read-back of a finished file: layout, configuration,
/// sample clocks and the same compressed-picture signature as the input.
pub fn inspect(path: &Path, hevc: bool) -> anyhow::Result<Checked> {
    let mut file = File::open(path)?;
    let end = moov_end(&mut file)?;
    let mut head = vec![0u8; end as usize];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut head)?;
    let mut demuxer = Mp4FileDemuxer::new();
    for _ in 0..8 {
        let Some(required) = demuxer.required_input() else {
            break;
        };
        ensure!(
            required.position + required.size.unwrap_or(0) as u64 <= end,
            "MP4 metadata outside faststart head"
        );
        demuxer.handle_input(Input {
            position: 0,
            data: &head,
        });
    }
    let tracks = demuxer.tracks().map_err(mux_error)?;
    ensure!(
        tracks.len() == 1 && tracks[0].kind == TrackKind::Video,
        "expected one video stream"
    );
    let timescale = tracks[0].timescale.get();
    let mut pictures = Pictures::new(hevc);
    let mut checked = Checked {
        signature: Signature {
            pictures: String::new(),
            headers: Default::default(),
        },
        timescale,
        pts: Vec::new(),
        dts: Vec::new(),
        keyframes: Vec::new(),
        duration: 0,
    };
    let mut data = Vec::new();
    let mut configured = false;
    loop {
        let (offset, size, keyframe, timestamp, duration, cto) = {
            let Some(sample) = demuxer.next_sample().map_err(mux_error)? else {
                break;
            };
            if let Some(entry) = sample.sample_entry {
                ensure!(!configured, "multiple MP4 sample entries");
                configured = true;
                let (w, h, length, sets) = match (entry, hevc) {
                    (SampleEntry::Hvc1(e), true) => (
                        e.visual.width,
                        e.visual.height,
                        e.hvcc_box.length_size_minus_one.get(),
                        e.hvcc_box
                            .nalu_arrays
                            .iter()
                            .flat_map(|a| a.nalus.iter().cloned())
                            .collect::<Vec<_>>(),
                    ),
                    (SampleEntry::Avc1(e), false) => (
                        e.visual.width,
                        e.visual.height,
                        e.avcc_box.length_size_minus_one.get(),
                        e.avcc_box
                            .sps_list
                            .iter()
                            .chain(&e.avcc_box.pps_list)
                            .cloned()
                            .collect(),
                    ),
                    _ => anyhow::bail!("camera codec mismatch"),
                };
                check_dimensions(w, h)?;
                ensure!(length == 3, "unsupported NAL length size");
                for unit in sets {
                    pictures.add(&unit)?;
                }
            }
            (
                sample.data_offset,
                sample.data_size,
                sample.keyframe,
                sample.timestamp,
                sample.duration,
                sample.composition_time_offset.unwrap_or(0),
            )
        };
        ensure!(configured, "missing decoder configuration");
        ensure!(
            size as u64 <= SAMPLE_LIMIT && checked.pts.len() < 262144,
            "invalid video packet or inspection limit"
        );
        data.resize(size, 0);
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut data)?;
        let mut rest = &data[..];
        while !rest.is_empty() {
            ensure!(rest.len() >= 4, "truncated MP4 sample");
            let n = u32::from_be_bytes(rest[..4].try_into()?) as usize;
            ensure!(n > 0 && n <= rest.len() - 4, "truncated MP4 sample");
            let kind = pictures.add(&rest[4..4 + n])?;
            ensure!(
                !(hevc && is_parameter_set(kind, true)),
                "hvc1 parameter sets must be out of band"
            );
            rest = &rest[4 + n..];
        }
        if let Some(&last) = checked.dts.last() {
            ensure!(timestamp > last, "invalid PTS/DTS");
        }
        if keyframe {
            checked.keyframes.push(checked.dts.len());
        }
        checked.dts.push(timestamp);
        checked.pts.push(
            i64::try_from(timestamp)?
                .checked_add(cto)
                .context("invalid PTS/DTS")?,
        );
        checked.duration = timestamp + u64::from(duration);
    }
    ensure!(
        checked.keyframes.first() == Some(&0),
        "first sample is not a keyframe"
    );
    checked.signature = pictures.finish()?;
    Ok(checked)
}
