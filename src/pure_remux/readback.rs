//! Independent read-back of a finished MP4. It parses the box tree and sample
//! tables itself and shares no code with the writer or `shiguredo_mp4`, so a
//! writer bug cannot be confirmed by the same code that made it.
use super::annexb::{Pictures, is_parameter_set};
use crate::media::Inspection;
use anyhow::{Context, bail, ensure};
use serde_json::json;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const MOOV_LIMIT: u64 = 16 * 1024 * 1024;
const SAMPLE_LIMIT: u64 = 32 * 1024 * 1024;
const COUNT_LIMIT: usize = 262_144;

/// What the read-back saw besides the FFmpeg-shaped `Inspection`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Layout {
    pub top_level: Vec<String>,
    pub decoder_configuration: Vec<Vec<u8>>,
    pub keyframes: Vec<usize>,
    pub durations: Vec<u64>,
    pub movie_timescale: u32,
    pub movie_duration: u64,
    pub track_duration: u64,
    pub media_duration: u64,
    pub edit_list: Vec<(u64, i64)>,
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        ensure!(self.data.len() - self.at >= n, "truncated MP4 box");
        let out = &self.data[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }
    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> anyhow::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> anyhow::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn rest(&mut self) -> &'a [u8] {
        let out = &self.data[self.at..];
        self.at = self.data.len();
        out
    }
}

/// Child boxes of a container payload.
fn children(payload: &[u8]) -> anyhow::Result<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut r = Reader::new(payload);
    while r.at < payload.len() {
        let size = u64::from(r.u32()?);
        let kind: [u8; 4] = r.take(4)?.try_into()?;
        let (size, header) = match size {
            1 => (r.u64()?, 16),
            0 => ((payload.len() - r.at + 8) as u64, 8),
            n => (n, 8),
        };
        ensure!(size >= header, "invalid MP4 box size");
        let body = usize::try_from(size - header)?;
        out.push((kind, r.take(body)?));
    }
    Ok(out)
}

fn one<'a>(boxes: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> anyhow::Result<&'a [u8]> {
    let mut found = boxes.iter().filter(|(k, _)| k == kind);
    let first = found.next().map(|(_, b)| *b);
    ensure!(found.next().is_none(), "duplicate MP4 box");
    first.with_context(|| format!("missing MP4 {} box", String::from_utf8_lossy(kind)))
}

fn optional<'a>(boxes: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> anyhow::Result<Option<&'a [u8]>> {
    let mut found = boxes.iter().filter(|(k, _)| k == kind);
    let first = found.next().map(|(_, b)| *b);
    ensure!(found.next().is_none(), "duplicate MP4 box");
    Ok(first)
}

/// (version, payload after version/flags)
fn full(payload: &[u8]) -> anyhow::Result<(u8, &[u8])> {
    ensure!(payload.len() >= 4, "truncated MP4 box");
    Ok((payload[0], &payload[4..]))
}

/// (timescale, duration) of mvhd or mdhd.
fn header_clock(payload: &[u8]) -> anyhow::Result<(u32, u64)> {
    let (version, body) = full(payload)?;
    let mut r = Reader::new(body);
    Ok(match version {
        0 => {
            r.take(8)?;
            (r.u32()?, u64::from(r.u32()?))
        }
        1 => {
            r.take(16)?;
            (r.u32()?, r.u64()?)
        }
        _ => bail!("unsupported MP4 header version"),
    })
}

fn tkhd_duration(payload: &[u8]) -> anyhow::Result<u64> {
    let (version, body) = full(payload)?;
    let mut r = Reader::new(body);
    Ok(match version {
        0 => {
            r.take(16)?;
            u64::from(r.u32()?)
        }
        1 => {
            r.take(24)?;
            r.u64()?
        }
        _ => bail!("unsupported MP4 header version"),
    })
}

fn sets_avcc(payload: &[u8]) -> anyhow::Result<(u8, Vec<Vec<u8>>)> {
    let mut r = Reader::new(payload);
    ensure!(r.u8()? == 1, "invalid avcC");
    r.take(3)?;
    let length = (r.u8()? & 3) + 1;
    let mut sets = Vec::new();
    let sps = r.u8()? & 31;
    for _ in 0..sps {
        let n = usize::from(r.u16()?);
        sets.push(r.take(n)?.to_vec());
    }
    for _ in 0..r.u8()? {
        let n = usize::from(r.u16()?);
        sets.push(r.take(n)?.to_vec());
    }
    Ok((length, sets))
}

fn sets_hvcc(payload: &[u8]) -> anyhow::Result<(u8, Vec<Vec<u8>>)> {
    let mut r = Reader::new(payload);
    ensure!(r.u8()? == 1, "invalid hvcC");
    r.take(20)?;
    let length = (r.u8()? & 3) + 1;
    let mut sets = Vec::new();
    for _ in 0..r.u8()? {
        r.u8()?;
        for _ in 0..r.u16()? {
            let n = usize::from(r.u16()?);
            sets.push(r.take(n)?.to_vec());
        }
    }
    Ok((length, sets))
}

fn counted(count: u32) -> anyhow::Result<usize> {
    let count = usize::try_from(count)?;
    ensure!(
        count <= COUNT_LIMIT,
        "invalid video packet or inspection limit"
    );
    Ok(count)
}

struct Tables {
    durations: Vec<u64>,
    offsets: Vec<i64>,
    sizes: Vec<u64>,
    positions: Vec<u64>,
    sync: Option<Vec<usize>>,
}

fn tables(stbl: &[([u8; 4], &[u8])]) -> anyhow::Result<Tables> {
    let mut r = Reader::new(full(one(stbl, b"stsz")?)?.1);
    let constant = r.u32()?;
    let count = counted(r.u32()?)?;
    let sizes = if constant == 0 {
        (0..count)
            .map(|_| r.u32().map(u64::from))
            .collect::<anyhow::Result<Vec<_>>>()?
    } else {
        vec![u64::from(constant); count]
    };

    let mut durations = Vec::with_capacity(count);
    let mut r = Reader::new(full(one(stbl, b"stts")?)?.1);
    for _ in 0..counted(r.u32()?)? {
        let n = counted(r.u32()?)?;
        let delta = u64::from(r.u32()?);
        ensure!(
            durations.len() + n <= count,
            "inconsistent MP4 sample table"
        );
        durations.extend(std::iter::repeat_n(delta, n));
    }
    ensure!(durations.len() == count, "inconsistent MP4 sample table");

    let mut offsets = Vec::with_capacity(count);
    if let Some(ctts) = optional(stbl, b"ctts")? {
        let (version, body) = full(ctts)?;
        let mut r = Reader::new(body);
        for _ in 0..counted(r.u32()?)? {
            let n = counted(r.u32()?)?;
            let raw = r.u32()?;
            let value = if version == 1 {
                i64::from(raw as i32)
            } else {
                i64::from(raw)
            };
            ensure!(offsets.len() + n <= count, "inconsistent MP4 sample table");
            offsets.extend(std::iter::repeat_n(value, n));
        }
        ensure!(offsets.len() == count, "inconsistent MP4 sample table");
    } else {
        offsets.resize(count, 0);
    }

    let chunks = match (optional(stbl, b"stco")?, optional(stbl, b"co64")?) {
        (Some(stco), None) => {
            let mut r = Reader::new(full(stco)?.1);
            (0..counted(r.u32()?)?)
                .map(|_| r.u32().map(u64::from))
                .collect::<anyhow::Result<Vec<_>>>()?
        }
        (None, Some(co64)) => {
            let mut r = Reader::new(full(co64)?.1);
            (0..counted(r.u32()?)?)
                .map(|_| r.u64())
                .collect::<anyhow::Result<Vec<_>>>()?
        }
        _ => bail!("invalid MP4 chunk offsets"),
    };
    let mut r = Reader::new(full(one(stbl, b"stsc")?)?.1);
    let runs = (0..counted(r.u32()?)?)
        .map(|_| Ok((r.u32()?, r.u32()?, r.u32()?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    ensure!(
        runs.first().is_some_and(|r| r.0 == 1)
            && runs.windows(2).all(|w| w[1].0 > w[0].0)
            && runs.iter().all(|r| r.1 > 0 && r.2 == 1),
        "inconsistent MP4 sample table"
    );
    let mut positions = Vec::with_capacity(count);
    for (i, &chunk) in chunks.iter().enumerate() {
        let number = i as u32 + 1;
        let per = runs
            .iter()
            .rev()
            .find(|r| r.0 <= number)
            .context("inconsistent MP4 sample table")?
            .1;
        let mut at = chunk;
        for _ in 0..per {
            let Some(&size) = sizes.get(positions.len()) else {
                bail!("inconsistent MP4 sample table");
            };
            positions.push(at);
            at = at.checked_add(size).context("invalid MP4 sample offset")?;
        }
    }
    ensure!(positions.len() == count, "inconsistent MP4 sample table");

    let sync = optional(stbl, b"stss")?
        .map(|stss| -> anyhow::Result<Vec<usize>> {
            let mut r = Reader::new(full(stss)?.1);
            let list = (0..counted(r.u32()?)?)
                .map(|_| {
                    usize::try_from(r.u32()?)?
                        .checked_sub(1)
                        .context("invalid stss")
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            ensure!(
                list.windows(2).all(|w| w[1] > w[0]) && list.iter().all(|i| *i < count),
                "invalid stss"
            );
            Ok(list)
        })
        .transpose()?;
    Ok(Tables {
        durations,
        offsets,
        sizes,
        positions,
        sync,
    })
}

/// Reads `path` as the converted camera MP4 of `hevc` or H.264 and returns
/// the same `Inspection` shape the FFmpeg inspector produces, plus layout.
pub fn inspect(path: &Path, hevc: bool) -> anyhow::Result<(Inspection, Layout)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut at = 0;
    let mut top_level = Vec::new();
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
        let kind = String::from_utf8_lossy(&header[4..8]).into_owned();
        match &header[4..8] {
            b"moov" => {
                ensure!(
                    moov.is_none() && size <= MOOV_LIMIT,
                    "MP4 metadata exceeds limit"
                );
                moov = Some((at + prefix, size - prefix));
            }
            b"mdat" => {
                ensure!(mdat.is_none(), "multiple MP4 mdat boxes");
                mdat = Some((at + prefix, at + size));
            }
            _ => {}
        }
        top_level.push(kind);
        at += size;
    }
    ensure!(
        top_level.first().map(String::as_str) == Some("ftyp"),
        "missing MP4 ftyp"
    );
    let ((moov_at, moov_len), (data_start, data_end)) = moov
        .zip(mdat)
        .context("missing faststart/decoder configuration")?;
    ensure!(
        moov_at < data_start,
        "missing faststart/decoder configuration"
    );
    let mut bytes = vec![0u8; usize::try_from(moov_len)?];
    file.seek(SeekFrom::Start(moov_at))?;
    file.read_exact(&mut bytes)?;

    let moov = children(&bytes)?;
    let (movie_timescale, movie_duration) = header_clock(one(&moov, b"mvhd")?)?;
    let traks = moov
        .iter()
        .filter(|(k, _)| k == b"trak")
        .collect::<Vec<_>>();
    ensure!(traks.len() == 1, "expected one video stream");
    let trak = children(traks[0].1)?;
    let track_duration = tkhd_duration(one(&trak, b"tkhd")?)?;
    let mut edit_list = Vec::new();
    if let Some(edts) = optional(&trak, b"edts")? {
        let (version, body) = full(one(&children(edts)?, b"elst")?)?;
        let mut r = Reader::new(body);
        for _ in 0..counted(r.u32()?)? {
            let entry = if version == 1 {
                (r.u64()?, r.u64()? as i64)
            } else {
                (u64::from(r.u32()?), i64::from(r.u32()? as i32))
            };
            r.take(4)?;
            edit_list.push(entry);
        }
    }
    let mdia = children(one(&trak, b"mdia")?)?;
    let (timescale, media_duration) = header_clock(one(&mdia, b"mdhd")?)?;
    ensure!(
        timescale > 0 && movie_timescale > 0,
        "invalid stream time base"
    );
    let hdlr = full(one(&mdia, b"hdlr")?)?.1;
    ensure!(hdlr.get(4..8) == Some(b"vide"), "expected one video stream");
    let minf = children(one(&mdia, b"minf")?)?;
    let stbl = children(one(&minf, b"stbl")?)?;

    let mut r = Reader::new(full(one(&stbl, b"stsd")?)?.1);
    ensure!(r.u32()? == 1, "multiple MP4 sample entries");
    let entries = children(r.rest())?;
    ensure!(entries.len() == 1, "multiple MP4 sample entries");
    let (tag, entry) = entries[0];
    let (expected_tag, config_box) = if hevc {
        (b"hvc1", b"hvcC")
    } else {
        (b"avc1", b"avcC")
    };
    ensure!(&tag == expected_tag, "camera codec mismatch");
    let mut r = Reader::new(entry);
    r.take(24)?;
    let (width, height) = (r.u16()?, r.u16()?);
    r.take(50)?;
    let inner = children(r.rest())?;
    let config = one(&inner, config_box)?;
    let (length, sets) = if hevc {
        sets_hvcc(config)?
    } else {
        sets_avcc(config)?
    };
    super::mp4::check_dimensions(width, height)?;
    ensure!(length == 4, "unsupported NAL length size");

    let t = tables(&stbl)?;
    let count = t.sizes.len();
    ensure!(count > 0, "missing pictures or decoder configuration");
    let sum = t.durations.iter().sum::<u64>();
    ensure!(
        media_duration == sum,
        "MP4 media duration disagrees with its samples"
    );
    let keyframes = t.sync.clone().unwrap_or_else(|| (0..count).collect());
    ensure!(
        keyframes.first() == Some(&0),
        "first sample is not a keyframe"
    );

    let mut pictures = Pictures::new(hevc);
    for unit in &sets {
        pictures.add(unit)?;
    }
    let unit = 1f64 / f64::from(timescale);
    let time = |s: i64| (s as f64 * unit).to_string();
    let mut packets = Vec::with_capacity(count);
    let mut dts = 0u64;
    let mut sample = Vec::new();
    for i in 0..count {
        let (position, size) = (t.positions[i], t.sizes[i]);
        ensure!(
            size > 0
                && size <= SAMPLE_LIMIT
                && position >= data_start
                && position + size <= data_end,
            "invalid video packet or inspection limit"
        );
        sample.resize(usize::try_from(size)?, 0);
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut sample)?;
        let mut rest = &sample[..];
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
        let decode = i64::try_from(dts)?;
        let presentation = decode
            .checked_add(t.offsets[i])
            .context("invalid PTS/DTS")?;
        packets.push(json!({"codec_type":"video","pts":presentation,"dts":decode,"pts_time":time(presentation),"dts_time":time(decode)}));
        ensure!(t.durations[i] > 0 || i + 1 == count, "invalid PTS/DTS");
        dts += t.durations[i];
    }
    let reorder = t.offsets.iter().any(|o| *o != 0);
    let stream = json!({"codec_type":"video","codec_name":if hevc {"hevc"} else {"h264"},"codec_tag_string":String::from_utf8_lossy(&tag),"width":width,"height":height,"has_b_frames":i32::from(reorder),"time_base":format!("1/{timescale}"),"duration_ts":media_duration,"duration":(media_duration as f64 * unit).to_string()});
    let info = json!({"streams":[stream],"packets":packets});
    Ok((
        Inspection {
            info,
            signature: pictures.finish()?,
        },
        Layout {
            top_level,
            decoder_configuration: sets,
            keyframes,
            durations: t.durations,
            movie_timescale,
            movie_duration,
            track_duration,
            media_duration,
            edit_list,
        },
    ))
}
