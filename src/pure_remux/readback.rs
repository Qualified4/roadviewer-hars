//! Read-back of a finished MP4, for either engine's output. It parses the box
//! tree and sample tables itself, without `shiguredo_mp4` (which wrote them)
//! or the writer's Annex B splitting, and hashes the samples with
//! `media::NalHasher`, the splitter that also hashed the input. Shared with
//! the writer: only `mp4::check_dimensions` (the same size rule both apply).
use crate::media::{Inspection, NalHasher};
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

/// What the read-back saw besides the probe-shaped `Inspection`.
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

/// One sample's pictures as their slice headers state them, so a sample
/// boundary moved by the writer or a wrong sync mark is caught even when the
/// ordered picture hash is unchanged.
#[derive(Default)]
struct Access {
    vcl: usize,
    starts: usize,
    first_starts: bool,
    irap: bool,
    intra: bool,
}
impl Access {
    fn add(&mut self, hevc: bool, kind: u8, nal: &[u8]) -> anyhow::Result<()> {
        if !(if hevc {
            kind < 32
        } else {
            (1..=5).contains(&kind)
        }) {
            return Ok(());
        }
        // first_slice_segment_in_pic_flag (HEVC); first_mb_in_slice == 0, a
        // single 1 bit in Exp-Golomb (H.264).
        // H.264 data partitions B and C carry no slice header.
        let header = if hevc { 2 } else { 1 };
        ensure!(nal.len() > header, "truncated slice header");
        let starts = (hevc || matches!(kind, 1 | 2 | 5)) && nal[header] & 0x80 != 0;
        if self.vcl == 0 {
            self.first_starts = starts;
            self.intra = !hevc && starts && h264_intra(&nal[1..]);
        }
        self.vcl += 1;
        self.starts += usize::from(starts);
        self.irap |= if hevc {
            (16..=23).contains(&kind)
        } else {
            kind == 5
        };
        Ok(())
    }
    /// HEVC sync samples are exactly the IRAP pictures. H.264 IDR pictures
    /// must be sync samples; libavformat also marks I pictures with a
    /// recovery point, so a non-IDR sync sample must at least be intra.
    /// `fields`: an H.264 sample may hold the two field pictures of one frame (PAFF), as FFmpeg
    /// copies them; this engine refuses interlaced input, so its own samples hold one picture.
    fn check(&self, sync: bool, fields: bool) -> anyhow::Result<()> {
        ensure!(
            self.vcl > 0 && self.first_starts && (self.starts == 1 || fields && self.starts == 2),
            "MP4 sample is not one picture"
        );
        ensure!(
            sync == self.irap || (sync && self.intra),
            "MP4 sync samples disagree with the pictures"
        );
        Ok(())
    }
}

/// slice_type I or SI after first_mb_in_slice == 0; both fit the first two
/// bytes, before any emulation prevention byte can occur.
fn h264_intra(body: &[u8]) -> bool {
    let bits = u16::from_be_bytes([body[0], body.get(1).copied().unwrap_or(0)]) << 1;
    let zeros = bits.leading_zeros();
    if zeros > 7 {
        return false;
    }
    let value = (bits >> (15 - 2 * zeros)) - 1;
    matches!(value % 5, 2 | 4)
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

/// The upload check of an MP4: its stream table as a probe reads it. The `moov` box must be whole
/// and its sample tables consistent; the media data may be cut short (a later read of the samples
/// decides), other tracks may sit beside the one video track, and `moov` may follow `mdat`.
pub fn probe(path: &Path) -> anyhow::Result<serde_json::Value> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut at = 0;
    let mut moov = None;
    while at < len && len - at >= 8 {
        file.seek(SeekFrom::Start(at))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8])?;
        let small = u32::from_be_bytes(header[..4].try_into()?);
        let (size, prefix) = match small {
            1 => {
                ensure!(len - at >= 16, "truncated MP4 box");
                file.read_exact(&mut header[8..])?;
                (u64::from_be_bytes(header[8..].try_into()?), 16)
            }
            0 => (len - at, 8),
            n => (u64::from(n), 8),
        };
        ensure!(size >= prefix, "invalid MP4 box size");
        if &header[4..8] == b"moov" {
            ensure!(
                moov.is_none() && size <= MOOV_LIMIT && size <= len - at,
                "MP4 metadata exceeds limit"
            );
            moov = Some((at + prefix, size - prefix));
        }
        at = at.saturating_add(size);
    }
    let (moov_at, moov_len) = moov.context("missing MP4 moov box")?;
    let mut bytes = vec![0u8; usize::try_from(moov_len)?];
    file.seek(SeekFrom::Start(moov_at))?;
    file.read_exact(&mut bytes)?;
    let moov = children(&bytes)?;
    header_clock(one(&moov, b"mvhd")?)?;
    let mut streams = Vec::new();
    for (_, trak) in moov.iter().filter(|(k, _)| k == b"trak") {
        let trak = children(trak)?;
        let mdia = children(one(&trak, b"mdia")?)?;
        let hdlr = full(one(&mdia, b"hdlr")?)?.1;
        if hdlr.get(4..8) != Some(b"vide") {
            streams.push(json!({"codec_type":"other"}));
            continue;
        }
        let (timescale, _) = header_clock(one(&mdia, b"mdhd")?)?;
        ensure!(timescale > 0, "invalid stream time base");
        let stbl = children(one(&children(one(&mdia, b"minf")?)?, b"stbl")?)?;
        let mut r = Reader::new(full(one(&stbl, b"stsd")?)?.1);
        ensure!(r.u32()? >= 1, "missing MP4 sample entry");
        let entries = children(r.rest())?;
        let (tag, entry) = *entries.first().context("missing MP4 sample entry")?;
        let mut r = Reader::new(entry);
        r.take(24)?;
        let (width, height) = (r.u16()?, r.u16()?);
        tables(&stbl)?;
        let codec = match &tag {
            b"avc1" | b"avc3" => "h264",
            b"hvc1" | b"hev1" => "hevc",
            _ => "other",
        };
        streams.push(json!({"codec_type":"video","codec_name":codec,"codec_tag_string":String::from_utf8_lossy(&tag),"width":width,"height":height,"time_base":format!("1/{timescale}")}));
    }
    Ok(json!({"streams":streams}))
}

/// Reads `path` as the converted camera MP4 of `hevc` or H.264 and returns
/// the same `Inspection` shape the input inspection produces, plus layout.
pub fn inspect(path: &Path, hevc: bool) -> anyhow::Result<(Inspection, Layout)> {
    read(path, hevc, false)
}

/// [`inspect`] for an MP4 another muxer wrote (an input, or the MP4 an export reads): one H.264
/// sample is one access unit, a field pair included; `moov` may follow `mdat`, and tracks other
/// than the one video track are left out.
pub fn inspect_copy(path: &Path, hevc: bool) -> anyhow::Result<(Inspection, Layout)> {
    read(path, hevc, true)
}

fn is_video(trak: &[([u8; 4], &[u8])]) -> anyhow::Result<bool> {
    let mdia = children(one(trak, b"mdia")?)?;
    Ok(full(one(&mdia, b"hdlr")?)?.1.get(4..8) == Some(b"vide"))
}

fn read(path: &Path, hevc: bool, copy: bool) -> anyhow::Result<(Inspection, Layout)> {
    let fields = copy && !hevc;
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
        copy || moov_at < data_start,
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
        .map(|(_, body)| children(body))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut videos = Vec::new();
    for trak in &traks {
        if is_video(trak)? {
            videos.push(trak);
        }
    }
    ensure!(
        videos.len() == 1 && (copy || traks.len() == 1),
        "expected one video stream"
    );
    let trak = videos[0];
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
    // hev1/avc3 may also carry parameter sets inside the samples; another muxer's file may use
    // them, the engine writes hvc1/avc1 only.
    let in_band = copy && &tag == if hevc { b"hev1" } else { b"avc3" };
    ensure!(&tag == expected_tag || in_band, "camera codec mismatch");
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
    let keyframes = t.sync.clone().unwrap_or_else(|| (0..count).collect());
    ensure!(
        keyframes.first() == Some(&0),
        "first sample is not a keyframe"
    );

    let mut pictures = NalHasher::new(hevc);
    for unit in &sets {
        pictures.unit(unit)?;
    }
    let sync = keyframes
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
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
                && position
                    .checked_add(size)
                    .is_some_and(|end| end <= data_end),
            "invalid video packet or inspection limit"
        );
        sample.resize(usize::try_from(size)?, 0);
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut sample)?;
        let mut rest = &sample[..];
        let mut access = Access::default();
        while !rest.is_empty() {
            ensure!(rest.len() >= 4, "truncated MP4 sample");
            let n = u32::from_be_bytes(rest[..4].try_into()?) as usize;
            ensure!(n > 0 && n <= rest.len() - 4, "truncated MP4 sample");
            // A flagged unit is the input's, copied as it is (`annexb::kind`);
            // the signature still compares it byte for byte.
            let nal = &rest[4..4 + n];
            let kind = pictures.unit(nal)?;
            ensure!(
                in_band || !(hevc && matches!(kind, 32..=34)),
                "hvc1 parameter sets must be out of band"
            );
            access.add(hevc, kind, nal)?;
            rest = &rest[4 + n..];
        }
        access.check(sync.contains(&i), fields)?;
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

#[cfg(test)]
mod tests {
    use super::Access;

    fn sample(nals: &[&[u8]]) -> Access {
        let mut access = Access::default();
        for nal in nals {
            access.add(false, nal[0] & 31, nal).unwrap();
        }
        access
    }

    /// An IDR frame coded as two field pictures: both slices start a picture.
    #[test]
    fn a_field_pair_is_one_sample_only_in_another_muxers_copy() {
        let field: &[u8] = &[0x65, 0x88, 0x84];
        let pair = sample(&[field, field]);
        assert!(pair.check(true, false).is_err());
        pair.check(true, true).unwrap();
        assert!(sample(&[field, field, field]).check(true, true).is_err());
        sample(&[field]).check(true, true).unwrap();
        sample(&[field]).check(true, false).unwrap();
    }
}
