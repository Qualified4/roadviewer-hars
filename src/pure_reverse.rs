//! Pure Rust export of a converted camera MP4 back to its original format (raw HEVC Annex B,
//! H.264 MPEG-TS), the default engine. Nothing is re-encoded: every sample's NAL units are copied
//! and the decoder configuration goes back in band. The input is read twice by unrelated parsers:
//! `pure_remux::readback` checks it and states what the export must hold, and `shiguredo_mp4`'s
//! demuxer feeds the writer. The written file is then read back (`media::NalHasher`, the forward
//! engine's TS demuxer) and must match the first read. Failures carry the forward engine's
//! `FailureClass`; `media` decides from it whether the FFmpeg export may try the input.
mod annexb;
mod ts;

use crate::{
    media::{NalHasher, Signature},
    pure_remux::{Failure, FailureClass, Stage, Unsupported, readback},
};
use anyhow::{Context, bail, ensure};
use shiguredo_mp4::{
    TrackKind,
    boxes::SampleEntry,
    demux::{DemuxError, Input, Mp4FileDemuxer},
};
use std::{
    fs::File,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

const MOOV_LIMIT: usize = 16 * 1024 * 1024;
const SAMPLE_LIMIT: usize = 32 * 1024 * 1024;
/// An export larger than this is left to the FFmpeg export.
pub const OUTPUT_LIMIT: u64 = 4 << 30;
pub const TIMESCALE: u64 = 90_000;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Job {
    /// `qcamera.ts`, `fcamera.hevc` or `ecamera.hevc`.
    pub kind: String,
    /// 90 kHz ticks added to every TS clock: the original's smallest PTS (`video_origins`).
    pub offset: u64,
}

/// What the read-backs found; `media` checks it again before the export is placed.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    /// The MP4 as `readback` read it.
    pub input: Signature,
    /// The written file as it read back.
    pub output: Signature,
    pub frames: usize,
    pub keyframes: Vec<usize>,
    /// TS only: (PTS, DTS) of every PES as it read back, in 90 kHz ticks.
    pub clocks: Vec<(u64, u64)>,
    pub bytes: u64,
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

/// `seconds` (the stored `video_origins` value) in 90 kHz ticks, as Python's export rounds it.
pub fn offset_ticks(seconds: f64) -> Result<u64, Failure> {
    let ticks = (seconds * TIMESCALE as f64).round();
    if !(ticks.is_finite() && (0.0..=ts::CLOCK_LIMIT as f64).contains(&ticks)) {
        return Err(Failure::new(
            FailureClass::Unsupported,
            "unsupported",
            Unsupported("original timestamp offset"),
        ));
    }
    Ok(ticks as u64)
}

fn hevc_kind(kind: &str) -> anyhow::Result<bool> {
    Ok(match kind {
        "fcamera.hevc" | "ecamera.hevc" => true,
        "qcamera.ts" => false,
        _ => bail!(Unsupported("camera original format")),
    })
}

/// `value` of `timescale` in 90 kHz ticks plus `offset`, rounded to nearest (half away from zero)
/// as libavformat rescales; None past the 33-bit PES clock.
fn ticks(value: u64, timescale: u64, offset: u64) -> Option<u64> {
    let scaled = (u128::from(value) * u128::from(TIMESCALE) + u128::from(timescale / 2))
        / u128::from(timescale);
    u64::try_from(scaled)
        .ok()?
        .checked_add(offset)
        .filter(|v| *v <= ts::CLOCK_LIMIT)
}

/// Writes `output` (a new path) and reads it back; nothing is left there on failure.
pub fn reverse(source: &Path, output: &Path, job: &Job) -> anyhow::Result<Outcome> {
    ensure!(source != output, "source and output must differ");
    ensure!(!output.exists(), "export output must be new");
    let hevc = hevc_kind(&job.kind)?;
    ensure!(
        job.offset <= ts::CLOCK_LIMIT,
        Unsupported("original timestamp offset")
    );
    let result = (|| {
        let expected = read(source, hevc, job.offset).map_err(|e| e.context(Stage::Read))?;
        write(source, output, hevc, job.offset).map_err(|e| e.context(Stage::Write))?;
        if crate::fault::injected("pure_reverse_panic") {
            panic!("injected export panic");
        }
        if crate::fault::injected("pure_reverse_corrupt") {
            crate::fault::flip_last_byte(output)?;
        }
        let _profile = crate::profile::Scope::new("pure_reverse_readback");
        verify(output, hevc, expected).map_err(|e| e.context(Stage::Verify))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result
}

/// What the export must hold, from the independent MP4 reader.
struct Expected {
    signature: Signature,
    frames: usize,
    keyframes: Vec<usize>,
    clocks: Vec<(u64, u64)>,
}

fn read(source: &Path, hevc: bool, offset: u64) -> anyhow::Result<Expected> {
    let (inspection, layout) = readback::inspect_copy(source, hevc)?;
    // libavformat applies an edit list to the clocks it exports; this engine copies the sample
    // table's clocks, which agree with it only for a single edit starting at media time 0.
    ensure!(
        layout.edit_list.is_empty() || (layout.edit_list.len() == 1 && layout.edit_list[0].1 == 0),
        Unsupported("MP4 edit list")
    );
    let packets = inspection.info["packets"]
        .as_array()
        .context("missing packet timestamps")?;
    let timescale = inspection.info["streams"][0]["time_base"]
        .as_str()
        .and_then(|t| t.strip_prefix("1/")?.parse::<u64>().ok())
        .filter(|t| *t > 0)
        .context("invalid stream time base")?;
    let clocks = if hevc {
        Vec::new()
    } else {
        packets
            .iter()
            .map(|p| {
                let pts = u64::try_from(p["pts"].as_i64().context("missing PTS")?)
                    .map_err(|_| Unsupported("negative presentation time"))?;
                let dts = u64::try_from(p["dts"].as_i64().context("missing DTS")?)?;
                ticks(pts, timescale, offset)
                    .zip(ticks(dts, timescale, offset))
                    .ok_or_else(|| Unsupported("clock past the 33-bit TS range").into())
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    };
    Ok(Expected {
        signature: inspection.signature,
        frames: packets.len(),
        keyframes: layout.keyframes,
        clocks,
    })
}

fn moov_demuxer(file: &mut File) -> anyhow::Result<Mp4FileDemuxer> {
    let len = file.metadata()?.len();
    let mut demuxer = Mp4FileDemuxer::new();
    let mut buffer = Vec::new();
    loop {
        match demuxer.tracks() {
            Ok(_) => return Ok(demuxer),
            Err(DemuxError::InputRequired(required)) => {
                let available = len
                    .checked_sub(required.position)
                    .context("truncated MP4 box")?;
                let size = match required.size {
                    Some(size) => u64::try_from(size)?.min(available),
                    None => available,
                };
                ensure!(size <= MOOV_LIMIT as u64, "MP4 metadata exceeds limit");
                buffer.resize(usize::try_from(size)?, 0);
                file.seek(SeekFrom::Start(required.position))?;
                file.read_exact(&mut buffer)?;
                demuxer.handle_input(Input {
                    position: required.position,
                    data: &buffer,
                });
            }
            Err(e) => bail!("invalid MP4: {e}"),
        }
    }
}

fn converter(entry: &SampleEntry, hevc: bool) -> anyhow::Result<annexb::Converter> {
    match entry {
        SampleEntry::Avc1(avc1) if !hevc => {
            let avcc = &avc1.avcc_box;
            ensure!(
                avcc.length_size_minus_one.get() == 3,
                "unsupported NAL length size"
            );
            Ok(annexb::Converter::h264(
                avcc.sps_list.clone(),
                avcc.pps_list.clone(),
            ))
        }
        SampleEntry::Hvc1(hvc1) if hevc => {
            let hvcc = &hvc1.hvcc_box;
            ensure!(
                hvcc.length_size_minus_one.get() == 3,
                "unsupported NAL length size"
            );
            Ok(annexb::Converter::hevc(
                hvcc.nalu_arrays
                    .iter()
                    .flat_map(|a| a.nalus.iter().cloned())
                    .collect(),
            ))
        }
        _ => bail!("camera codec mismatch"),
    }
}

enum Sink {
    Hevc(BufWriter<File>),
    Ts(ts::Muxer<BufWriter<File>>),
}

fn write(source: &Path, output: &Path, hevc: bool, offset: u64) -> anyhow::Result<()> {
    let mut input = File::open(source)?;
    let mut demuxer = moov_demuxer(&mut input)?;
    let tracks = demuxer.tracks().map_err(|e| anyhow::anyhow!("{e}"))?;
    let video = tracks.iter().filter(|t| t.kind == TrackKind::Video).count();
    ensure!(video == 1 && tracks.len() == 1, "expected one video stream");
    let timescale = u64::from(tracks[0].timescale.get());
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let writer = BufWriter::with_capacity(1024 * 1024, file);
    let mut sink = if hevc {
        Sink::Hevc(writer)
    } else {
        Sink::Ts(ts::Muxer::new(writer))
    };
    let mut convert = None;
    let (mut sample, mut annex_b) = (Vec::new(), Vec::new());
    let mut written = 0u64;
    loop {
        let next = demuxer.next_sample().map_err(|e| anyhow::anyhow!("{e}"))?;
        let Some(next) = next else { break };
        match (next.sample_entry, convert.is_none()) {
            (Some(entry), true) => convert = Some(converter(entry, hevc)?),
            (Some(_), false) => bail!(Unsupported("MP4 sample entry changes")),
            (None, true) => bail!("missing MP4 sample entry"),
            (None, false) => {}
        }
        ensure!(
            next.data_size > 0 && next.data_size <= SAMPLE_LIMIT,
            "invalid video packet or inspection limit"
        );
        let clocks = if hevc {
            None
        } else {
            let decode = next.timestamp;
            let presentation = i64::try_from(decode)?
                .checked_add(next.composition_time_offset.unwrap_or(0))
                .and_then(|v| u64::try_from(v).ok())
                .ok_or(Unsupported("negative presentation time"))?;
            Some(
                ticks(presentation, timescale, offset)
                    .zip(ticks(decode, timescale, offset))
                    .ok_or(Unsupported("clock past the 33-bit TS range"))?,
            )
        };
        let keyframe = next.keyframe;
        sample.resize(next.data_size, 0);
        input.seek(SeekFrom::Start(next.data_offset))?;
        input.read_exact(&mut sample)?;
        let Some(convert) = convert.as_mut() else {
            bail!("missing MP4 sample entry");
        };
        convert.convert(&sample, &mut annex_b)?;
        match (&mut sink, clocks) {
            (Sink::Hevc(out), _) => {
                out.write_all(&annex_b)?;
                written += annex_b.len() as u64;
            }
            (Sink::Ts(muxer), Some((pts, dts))) => {
                muxer.write(pts, dts, keyframe, &annex_b)?;
                written = muxer.written;
            }
            (Sink::Ts(_), None) => bail!("missing TS clocks"),
        }
        ensure!(written <= OUTPUT_LIMIT, Unsupported("export output size"));
    }
    ensure!(
        convert.is_some(),
        "missing pictures or decoder configuration"
    );
    let file = match sink {
        Sink::Hevc(out) => out.into_inner().map_err(|e| e.into_error())?,
        Sink::Ts(muxer) => muxer.finish()?.into_inner().map_err(|e| e.into_error())?,
    };
    crate::media::start_writeback(&file);
    Ok(())
}

fn verify(output: &Path, hevc: bool, expected: Expected) -> anyhow::Result<Outcome> {
    let bytes = std::fs::metadata(output)?.len();
    let mut hash = NalHasher::new(hevc);
    let mut clocks = Vec::new();
    let mut keyframes = Vec::new();
    let frames = if hevc {
        let mut file = File::open(output)?;
        let mut chunk = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            hash.feed(&chunk[..n])?;
        }
        expected.frames
    } else {
        crate::pure_remux::ts::demux(output, |pes| {
            ensure!(
                pes.data.starts_with(&[0, 0, 0, 1]) || pes.data.starts_with(&[0, 0, 1]),
                "PES without a start code"
            );
            if pes.random_access {
                keyframes.push(clocks.len());
            }
            clocks.push((pes.pts, pes.dts));
            hash.feed(&pes.data)
        })?;
        ensure!(
            clocks == expected.clocks,
            "video timestamps changed during export"
        );
        ensure!(
            keyframes == expected.keyframes,
            "keyframes changed during export"
        );
        clocks.len()
    };
    let output_signature = hash.finish()?;
    ensure!(
        output_signature == expected.signature,
        "compressed pictures changed during export"
    );
    ensure!(
        frames == expected.frames,
        "frame count changed during export"
    );
    Ok(Outcome {
        input: expected.signature,
        output: output_signature,
        frames,
        keyframes: expected.keyframes,
        clocks,
        bytes,
    })
}
