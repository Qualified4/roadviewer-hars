//! The first pictures of a camera MP4, decoded by pure Rust decoders (`rusty_h264-decoder`,
//! `rusty_h265`, both `forbid(unsafe_code)` with their SIMD features off) in the order and with
//! the times FFmpeg's `mov` demuxer and a two-thread frame-threaded decoder give them.
use super::scale::{Image, Plane};
use anyhow::{Context, bail, ensure};
use std::{
    cell::Cell,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const MOOV_LIMIT: u64 = 16 * 1024 * 1024;
const SAMPLE_LIMIT: u64 = 32 * 1024 * 1024;
const COUNT_LIMIT: u32 = 262_144;
const PARAMETER_SETS: usize = 64;
pub const MAX_WIDTH: usize = 4096;
pub const MAX_HEIGHT: usize = 2160;

/// A decoded picture and its presentation time in seconds, as `frame.pts * time_base`.
pub struct Frame {
    pub time: f64,
    pub image: Image,
    /// FFmpeg would output `yuvj420p`: the stream signals full range.
    pub full_range: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    H264,
    Hevc,
}

struct Track {
    codec: Codec,
    length_size: usize,
    sets: Vec<Vec<u8>>,
    timescale: u32,
    /// Media time of the first presented sample (`elst`); earlier samples are decoded only.
    start: i64,
    /// (file offset, size, decode time, composition offset) of the first samples.
    samples: Vec<(u64, u64, i64, i64)>,
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
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
    fn count(&mut self) -> anyhow::Result<u32> {
        let n = self.u32()?;
        ensure!(n <= COUNT_LIMIT, "MP4 table exceeds limit");
        Ok(n)
    }
}

fn children(payload: &[u8]) -> anyhow::Result<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut r = Reader {
        data: payload,
        at: 0,
    };
    while r.at < payload.len() {
        ensure!(out.len() < 4096, "MP4 box count exceeds limit");
        let size = u64::from(r.u32()?);
        let kind: [u8; 4] = r.take(4)?.try_into()?;
        let (size, header) = match size {
            1 => (r.u64()?, 16),
            0 => ((payload.len() - r.at + 8) as u64, 8),
            n => (n, 8),
        };
        ensure!(size >= header, "invalid MP4 box size");
        out.push((kind, r.take(usize::try_from(size - header)?)?));
    }
    Ok(out)
}

fn one<'a>(boxes: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> anyhow::Result<&'a [u8]> {
    boxes
        .iter()
        .find(|(k, _)| k == kind)
        .map(|(_, b)| *b)
        .with_context(|| format!("missing MP4 {} box", String::from_utf8_lossy(kind)))
}

fn full(payload: &[u8]) -> anyhow::Result<(u8, Reader<'_>)> {
    ensure!(payload.len() >= 4, "truncated MP4 box");
    Ok((
        payload[0],
        Reader {
            data: &payload[4..],
            at: 0,
        },
    ))
}

fn sets_avcc(payload: &[u8]) -> anyhow::Result<(usize, Vec<Vec<u8>>)> {
    let mut r = Reader {
        data: payload,
        at: 0,
    };
    ensure!(r.u8()? == 1, "invalid avcC");
    r.take(3)?;
    let length = usize::from(r.u8()? & 3) + 1;
    let mut sets = Vec::new();
    for _ in 0..(r.u8()? & 31) {
        let n = usize::from(r.u16()?);
        sets.push(r.take(n)?.to_vec());
    }
    for _ in 0..r.u8()? {
        let n = usize::from(r.u16()?);
        sets.push(r.take(n)?.to_vec());
    }
    Ok((length, sets))
}

fn sets_hvcc(payload: &[u8]) -> anyhow::Result<(usize, Vec<Vec<u8>>)> {
    let mut r = Reader {
        data: payload,
        at: 0,
    };
    ensure!(r.u8()? == 1, "invalid hvcC");
    r.take(20)?;
    let length = usize::from(r.u8()? & 3) + 1;
    let mut sets = Vec::new();
    for _ in 0..r.u8()? {
        r.u8()?;
        for _ in 0..r.u16()? {
            ensure!(sets.len() < PARAMETER_SETS, "too many parameter sets");
            let n = usize::from(r.u16()?);
            sets.push(r.take(n)?.to_vec());
        }
    }
    Ok((length, sets))
}

fn track(file: &mut File, wanted: usize) -> anyhow::Result<Track> {
    let len = file.metadata()?.len();
    let mut at = 0;
    let mut moov = None;
    while at < len {
        ensure!(len - at >= 8, "truncated MP4 box");
        file.seek(SeekFrom::Start(at))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8])?;
        let (size, prefix) = match u32::from_be_bytes(header[..4].try_into()?) {
            1 => {
                file.read_exact(&mut header[8..])?;
                (u64::from_be_bytes(header[8..].try_into()?), 16)
            }
            0 => (len - at, 8),
            n => (u64::from(n), 8),
        };
        ensure!(size >= prefix, "invalid MP4 box size");
        if size > len - at {
            // A file cut short (the first bytes of an archived video) keeps its complete boxes;
            // its samples are read until the end, as FFmpeg does.
            ensure!(&header[4..8] != b"moov", "truncated MP4 metadata");
            break;
        }
        if &header[4..8] == b"moov" {
            ensure!(
                moov.is_none() && size <= MOOV_LIMIT,
                "MP4 metadata exceeds limit"
            );
            moov = Some((at + prefix, size - prefix));
        }
        at += size;
    }
    let (moov_at, moov_len) = moov.context("missing MP4 moov box")?;
    let mut bytes = vec![0u8; usize::try_from(moov_len)?];
    file.seek(SeekFrom::Start(moov_at))?;
    file.read_exact(&mut bytes)?;
    let moov = children(&bytes)?;
    let mut video = None;
    for (_, trak) in moov.iter().filter(|(k, _)| k == b"trak") {
        let trak = children(trak)?;
        let mdia = children(one(&trak, b"mdia")?)?;
        let (_, mut hdlr) = full(one(&mdia, b"hdlr")?)?;
        hdlr.take(4)?;
        if hdlr.take(4)? == b"vide" {
            ensure!(video.is_none(), "expected one video stream");
            video = Some((trak, mdia));
        }
    }
    let (trak, mdia) = video.context("missing thumbnail video stream")?;
    let (version, mut r) = full(one(&mdia, b"mdhd")?)?;
    let timescale = match version {
        0 => {
            r.take(8)?;
            r.u32()?
        }
        1 => {
            r.take(16)?;
            r.u32()?
        }
        _ => bail!("unsupported MP4 header version"),
    };
    ensure!(timescale > 0, "invalid stream time base");
    let mut start = 0;
    if let Some((_, edts)) = trak.iter().find(|(k, _)| k == b"edts") {
        let (version, mut r) = full(one(&children(edts)?, b"elst")?)?;
        let mut edits = Vec::new();
        for _ in 0..r.count()? {
            let (_, time) = if version == 1 {
                (r.u64()?, r.u64()? as i64)
            } else {
                (u64::from(r.u32()?), i64::from(r.u32()? as i32))
            };
            ensure!(r.u32()? == 0x10000, "unsupported MP4 edit list");
            edits.push(time);
        }
        let normal = edits.iter().filter(|t| **t != -1).collect::<Vec<_>>();
        ensure!(
            normal.len() <= 1 && edits.iter().rev().skip(normal.len()).all(|t| *t == -1),
            "unsupported MP4 edit list"
        );
        start = normal.first().map_or(0, |t| **t);
        ensure!(start >= 0, "unsupported MP4 edit list");
    }
    let stbl = children(one(&children(one(&mdia, b"minf")?)?, b"stbl")?)?;
    let (_, mut stsd) = full(one(&stbl, b"stsd")?)?;
    ensure!(stsd.u32()? == 1, "multiple MP4 sample entries");
    let entries = children(&stsd.data[stsd.at..])?;
    ensure!(entries.len() == 1, "multiple MP4 sample entries");
    let (tag, entry) = entries[0];
    let codec = match &tag {
        b"avc1" | b"avc3" => Codec::H264,
        b"hvc1" | b"hev1" => Codec::Hevc,
        _ => bail!("unsupported thumbnail codec"),
    };
    let mut r = Reader { data: entry, at: 0 };
    r.take(24)?;
    // The sample entry's own width and height are not used: FFmpeg sizes the stream from its
    // parameter sets.
    r.take(54)?;
    let inner = children(&entry[r.at..])?;
    let (length_size, sets) = match codec {
        Codec::H264 => sets_avcc(one(&inner, b"avcC")?)?,
        Codec::Hevc => sets_hvcc(one(&inner, b"hvcC")?)?,
    };
    ensure!(sets.len() <= PARAMETER_SETS, "too many parameter sets");

    let (_, mut r) = full(one(&stbl, b"stsz")?)?;
    let constant = r.u32()?;
    let total = r.count()? as usize;
    let wanted_n = wanted.min(total);
    let sizes = (0..wanted_n)
        .map(|_| {
            if constant == 0 {
                r.u32().map(u64::from)
            } else {
                Ok(u64::from(constant))
            }
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let expand = |payload: &[u8], signed: bool| -> anyhow::Result<Vec<i64>> {
        let (version, mut r) = full(payload)?;
        let mut out = Vec::with_capacity(wanted_n);
        for _ in 0..r.count()? {
            if out.len() >= wanted_n {
                break;
            }
            let n = r.u32()? as usize;
            let raw = r.u32()?;
            let value = if signed && version == 1 {
                i64::from(raw as i32)
            } else {
                i64::from(raw)
            };
            out.extend(std::iter::repeat_n(value, n.min(wanted_n - out.len())));
        }
        Ok(out)
    };
    let deltas = expand(one(&stbl, b"stts")?, false)?;
    ensure!(deltas.len() == wanted_n, "inconsistent MP4 sample table");
    let offsets = match stbl.iter().find(|(k, _)| k == b"ctts") {
        Some((_, ctts)) => {
            let v = expand(ctts, true)?;
            ensure!(v.len() == wanted_n, "inconsistent MP4 sample table");
            v
        }
        None => vec![0; wanted_n],
    };
    let chunks: Vec<u64> = match (
        stbl.iter().find(|(k, _)| k == b"stco"),
        stbl.iter().find(|(k, _)| k == b"co64"),
    ) {
        (Some((_, stco)), None) => {
            let (_, mut r) = full(stco)?;
            (0..r.count()?)
                .map(|_| r.u32().map(u64::from))
                .collect::<anyhow::Result<_>>()?
        }
        (None, Some((_, co64))) => {
            let (_, mut r) = full(co64)?;
            (0..r.count()?)
                .map(|_| r.u64())
                .collect::<anyhow::Result<_>>()?
        }
        _ => bail!("invalid MP4 chunk offsets"),
    };
    let (_, mut r) = full(one(&stbl, b"stsc")?)?;
    let runs = (0..r.count()?)
        .map(|_| Ok((r.u32()?, r.u32()?, r.u32()?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    ensure!(
        runs.first().is_some_and(|r| r.0 == 1)
            && runs.windows(2).all(|w| w[1].0 > w[0].0)
            && runs.iter().all(|r| r.1 > 0 && r.2 == 1),
        "inconsistent MP4 sample table"
    );
    let mut samples = Vec::with_capacity(wanted_n);
    let mut dts = 0i64;
    'chunks: for (i, &chunk) in chunks.iter().enumerate() {
        let number = i as u32 + 1;
        let per = runs
            .iter()
            .rev()
            .find(|r| r.0 <= number)
            .context("inconsistent MP4 sample table")?
            .1;
        let mut position = chunk;
        for _ in 0..per {
            let n = samples.len();
            if n == wanted_n {
                break 'chunks;
            }
            let size = sizes[n];
            ensure!(
                size > 0 && size <= SAMPLE_LIMIT,
                "invalid video packet or inspection limit"
            );
            samples.push((position, size, dts, offsets[n]));
            position = position
                .checked_add(size)
                .context("invalid MP4 sample offset")?;
            dts = dts
                .checked_add(deltas[n])
                .context("invalid MP4 sample time")?;
        }
    }
    ensure!(samples.len() == wanted_n, "inconsistent MP4 sample table");
    Ok(Track {
        codec,
        length_size,
        sets,
        timescale,
        start,
        samples,
    })
}

/// NAL units of one length-prefixed sample.
fn units(sample: &[u8], length_size: usize) -> anyhow::Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut rest = sample;
    while !rest.is_empty() {
        ensure!(rest.len() >= length_size, "truncated MP4 sample");
        let n = rest[..length_size]
            .iter()
            .fold(0usize, |a, b| (a << 8) | usize::from(*b));
        ensure!(
            n > 0 && n <= rest.len() - length_size,
            "truncated MP4 sample"
        );
        out.push(&rest[length_size..length_size + n]);
        rest = &rest[length_size + n..];
    }
    Ok(out)
}

struct Bits {
    rbsp: Vec<u8>,
    at: usize,
}
impl Bits {
    fn new(ebsp: &[u8]) -> Self {
        let mut rbsp = Vec::with_capacity(ebsp.len());
        let mut zeros = 0;
        for &b in ebsp {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            rbsp.push(b);
        }
        Self { rbsp, at: 0 }
    }
    fn bit(&mut self) -> anyhow::Result<u32> {
        let byte = *self
            .rbsp
            .get(self.at / 8)
            .context("truncated parameter set")?;
        self.at += 1;
        Ok(u32::from((byte >> (7 - (self.at - 1) % 8)) & 1))
    }
    fn bits(&mut self, n: u32) -> anyhow::Result<u32> {
        (0..n).try_fold(0, |v, _| Ok((v << 1) | self.bit()?))
    }
    fn ue(&mut self) -> anyhow::Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            ensure!(zeros < 32, "invalid Exp-Golomb code");
        }
        Ok((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
}

/// What the decoder configuration says before any picture: the cropped size, which FFmpeg's
/// stream parameters carry and the size bound is checked on, and `video_full_range_flag`.
struct Sequence {
    width: u64,
    height: u64,
    full_range: bool,
}

fn h264_sequence(sps: &[u8]) -> anyhow::Result<Sequence> {
    let mut b = Bits::new(sps.get(1..).context("truncated parameter set")?);
    let chroma = h264_until_references(&mut b)?;
    b.ue()?;
    b.bit()?;
    let width_mbs = u64::from(b.ue()?) + 1;
    let height_units = u64::from(b.ue()?) + 1;
    let frames_only = b.bit()? == 1;
    if !frames_only {
        b.bit()?;
    }
    b.bit()?;
    let fields = if frames_only { 1 } else { 2 };
    let (unit_x, unit_y) = match chroma {
        0 | 3 => (1, fields),
        2 => (2, fields),
        _ => (2, 2 * fields),
    };
    let mut crop = [0u64; 4];
    if b.bit()? == 1 {
        for c in &mut crop {
            *c = u64::from(b.ue()?);
        }
    }
    let width = (width_mbs * 16).saturating_sub(unit_x * (crop[0] + crop[1]));
    let height = (height_units * 16 * fields).saturating_sub(unit_y * (crop[2] + crop[3]));
    let mut full_range = false;
    if b.bit()? == 1 {
        if b.bit()? == 1 && b.bits(8)? == 255 {
            b.bits(32)?;
        }
        if b.bit()? == 1 {
            b.bit()?;
        }
        if b.bit()? == 1 {
            b.bits(3)?;
            full_range = b.bit()? == 1;
        }
    }
    Ok(Sequence {
        width,
        height,
        full_range,
    })
}

/// Reads an SPS up to `max_num_ref_frames`; returns `chroma_format_idc`.
fn h264_until_references(b: &mut Bits) -> anyhow::Result<u32> {
    let profile = b.bits(8)?;
    b.bits(16)?;
    b.ue()?;
    let mut chroma = 1;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma = b.ue()?;
        if chroma == 3 {
            b.bit()?;
        }
        b.ue()?;
        b.ue()?;
        b.bit()?;
        if b.bit()? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..size {
                        if next != 0 {
                            let v = i64::from(b.ue()?);
                            let delta = if v % 2 == 1 { (v + 1) / 2 } else { -v / 2 };
                            next = (last + delta + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    b.ue()?;
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.bit()?;
            b.ue()?;
            b.ue()?;
            for _ in 0..b.ue()?.min(256) {
                b.ue()?;
            }
        }
        _ => {}
    }
    Ok(chroma)
}

/// Bytes of grey reference pictures a restart probe may make (`h264_probe_sps`), at least one
/// picture: 5 at 1920x1080, 1 at 4096x2160, and the stream's own count at 526x330.
const PROBE_REFERENCE_BYTES: u64 = 16 * 1024 * 1024;

/// The SPS a restart probe (`finishes`) decodes with. `gaps_in_frame_num_value_allowed_flag` is
/// set: a fresh `rusty_h264` decoder then gives a P picture grey pictures for the references it
/// lacks, which it otherwise refuses; nothing else reads the flag. `max_num_ref_frames` is
/// lowered so that those pictures take at most `PROBE_REFERENCE_BYTES`: with fewer references a
/// slice decodes the same macroblocks or fails, never the other way round
/// (docs/hars/THUMBNAIL-DECODER.md, "Memory").
pub(super) fn h264_probe_sps(sps: &[u8]) -> Option<Vec<u8>> {
    let mut b = Bits {
        rbsp: h264_rbsp(sps.get(1..)?),
        at: 0,
    };
    h264_until_references(&mut b).ok()?;
    let references_at = b.at;
    let references = u64::from(b.ue().ok()?);
    b.bit().ok()?;
    let rest = b.at;
    let width = u64::from(b.ue().ok()?) + 1;
    let height = (u64::from(b.ue().ok()?) + 1) * if b.bit().ok()? == 1 { 1 } else { 2 };
    let picture = width.saturating_mul(height).saturating_mul(16 * 16 * 3 / 2);
    let references = references.min((PROBE_REFERENCE_BYTES / picture).max(1));
    let bit = |i: usize| (b.rbsp[i / 8] >> (7 - i % 8)) & 1 == 1;
    // The last set bit is the stop bit; nothing after it is read.
    let end = (rest..b.rbsp.len() * 8).rev().find(|i| bit(*i))?;
    let mut bits: Vec<bool> = (0..references_at).map(bit).collect();
    let code = references + 1;
    let length = 63 - code.leading_zeros();
    bits.extend((0..length).map(|_| false));
    bits.extend((0..=length).rev().map(|i| (code >> i) & 1 == 1));
    bits.push(true);
    bits.extend((rest..end).map(bit));
    bits.push(true);
    let rbsp = bits.chunks(8).map(|c| {
        c.iter()
            .enumerate()
            .fold(0u8, |v, (i, set)| v | (u8::from(*set) << (7 - i)))
    });
    let mut out = vec![sps[0]];
    let mut zeros = 0;
    for v in rbsp {
        if zeros >= 2 && v <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(v);
        zeros = if v == 0 { zeros + 1 } else { 0 };
    }
    Some(out)
}

fn hevc_sequence(sps: &[u8]) -> anyhow::Result<Sequence> {
    ensure!(sps.len() > 2, "truncated parameter set");
    let rbsp = rusty_h265::nal::unescape(&sps[2..]);
    let sps = rusty_h265::ps::parse_sps(&rbsp.data).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (sub_x, sub_y) = match sps.chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let [left, right, top, bottom] = sps.conf_win.map(u64::from);
    Ok(Sequence {
        width: u64::from(sps.width).saturating_sub(sub_x * (left + right)),
        height: u64::from(sps.height).saturating_sub(sub_y * (top + bottom)),
        full_range: sps.vui.is_some_and(|v| v.video_full_range_flag),
    })
}

fn plane(width: usize, height: usize, data: Vec<u8>) -> Plane {
    Plane {
        width,
        height,
        data,
    }
}

fn h265_image(frame: &rusty_h265::Frame) -> anyhow::Result<Image> {
    let p = &frame.picture;
    ensure!(
        p.chroma_format_idc == 1 && p.bit_depth_luma == 8 && p.bit_depth_chroma == 8,
        "unsupported thumbnail pixel format"
    );
    let (x, y, w, h) = p.crop;
    ensure!(x % 2 == 0 && y % 2 == 0, "unsupported thumbnail crop");
    let cut = |plane: &rusty_h265::Plane, x: usize, y: usize, w: usize, h: usize| {
        ensure!(
            x + w <= plane.width && y + h <= plane.height,
            "invalid thumbnail crop"
        );
        let mut data = Vec::with_capacity(w * h);
        for row in y..y + h {
            data.extend(plane.row(row)[x..x + w].iter().map(|v| *v as u8));
        }
        Ok(self::plane(w, h, data))
    };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    Ok(Image {
        y: cut(&p.planes[0], x, y, w, h)?,
        u: cut(&p.planes[1], x / 2, y / 2, cw, ch)?,
        v: cut(&p.planes[2], x / 2, y / 2, cw, ch)?,
    })
}

fn h264_image((w, h): (usize, usize), y: Vec<u8>, u: Vec<u8>, v: Vec<u8>) -> anyhow::Result<Image> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    ensure!(
        y.len() == w * h && u.len() == cw * ch && v.len() == cw * ch,
        "invalid decoded picture"
    );
    Ok(Image {
        y: plane(w, h, y),
        u: plane(cw, ch, u),
        v: plane(cw, ch, v),
    })
}

fn guarded<T>(f: impl FnOnce() -> T) -> anyhow::Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|_| anyhow::anyhow!("thumbnail decoder panicked"))
}

enum Decoder {
    H264(Box<rusty_h264_decoder::Decoder>),
    Hevc(Box<rusty_h265::Decoder>),
}

fn new_decoder(track: &Track) -> anyhow::Result<Decoder> {
    Ok(match track.codec {
        Codec::H264 => Decoder::H264(Box::default()),
        Codec::Hevc => {
            let mut d = Box::new(rusty_h265::Decoder::new());
            for set in &track.sets {
                guarded(|| d.push_nal(set, None))?.map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            Decoder::Hevc(d)
        }
    })
}

/// A picture presented this close before the time the caller wants is still decoded, so the
/// rounding of the caller's own comparison cannot make a needed picture be skipped.
const SKIP_MARGIN: f64 = 1e-3;

/// How a decode went, for tests and measurements; the pictures do not depend on it.
#[derive(Debug, Default)]
pub struct Work {
    /// Samples given to a decoder, a stretch decoded again counted again.
    pub decoded: usize,
    /// Samples read before decoding to check the stream (`shortcuts`), and to probe the sample
    /// before each H.264 restart candidate.
    pub surveyed: usize,
    /// The samples a fresh decoder started at.
    pub restarts: Vec<usize>,
    pub flushes: usize,
    /// Stretches decoded again after a flush.
    pub replays: usize,
}

/// The RBSP `rusty_h264` reads (`rusty_h264_common::nal::emulation_unprevent`): an
/// emulation-prevention byte is dropped only before a byte up to 3.
pub(super) fn h264_rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    for (i, &b) in ebsp.iter().enumerate() {
        if zeros >= 2 && b == 3 && ebsp.get(i + 1).is_some_and(|n| *n <= 3) {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// `first_mb_in_slice` and `slice_type` of an H.264 slice.
fn h264_slice(nal: &[u8]) -> Option<(u32, u32)> {
    let mut b = Bits {
        rbsp: h264_rbsp(nal.get(1..nal.len().min(64))?),
        at: 0,
    };
    Some((b.ue().ok()?, b.ue().ok()?))
}

/// `slice_type` of the first slice segment of a HEVC IRAP picture whose PPS adds `extra` bits.
fn hevc_irap_slice_type(nal: &[u8], pps: u8, extra: u8) -> Option<u32> {
    let mut b = Bits {
        rbsp: rusty_h265::nal::unescape(nal.get(2..nal.len().min(64))?).data,
        at: 0,
    };
    if b.bit().ok()? != 1 {
        return None;
    }
    b.bit().ok()?;
    if b.ue().ok()? != u32::from(pps) {
        return None;
    }
    b.bits(u32::from(extra)).ok()?;
    b.ue().ok()
}

/// The samples a fresh decoder can start at, `None` when neither a restart nor a HEVC flush may
/// be used: decoding then goes through every sample in order. A restart at IDR sample `s` and a
/// flush leave every picture the caller is given, its time and its order as decoding every
/// sample gives them when (docs/hars/THUMBNAIL-DECODER.md, "When skipping is exact"):
///
/// 1. pictures come out in decode order: no composition offset, presentation times that only
///    grow, and for HEVC `sps_max_num_reorder_pics` zero in every sub-layer (`rusty_h265` then
///    outputs each picture when it is finished, whatever `sps_max_latency_increase_plus1` says);
/// 2. the decoder that went through every sample holds the parameter sets a fresh one is given:
///    one set of each kind in the track, every in-band set parsed to the same fields as it, and
///    for H.264 no redundant pictures;
/// 3. for HEVC every picture carries the time of its own sample and is finished by the next
///    sample: every NAL header parses, and the first slice of each sample, only it, starts a
///    picture;
/// 4. sample `s` starts from nothing: every slice is IDR (HEVC: temporal layer 0), its first
///    slice is an I slice starting the picture, and for H.264 every slice is a reference;
/// 5. for H.264 sample `s - 1` ends with its picture finished, since `rusty_h264` refuses an IDR
///    after an unfinished picture: it holds one picture whose first slice starts it, and a fresh
///    decoder finishes it (`finishes`, run here for every candidate).
///
/// What this cannot see is a decoder panic in a skipped sample, which decoding every sample
/// would have turned into a failure.
fn shortcuts(file: &mut File, track: &Track, work: &mut Work) -> anyhow::Result<Option<Vec<bool>>> {
    let hevc = track.codec == Codec::Hevc;
    if track
        .sets
        .iter()
        .any(|s| s.len() < if hevc { 3 } else { 2 })
    {
        return Ok(None);
    }
    let kind = |nal: &[u8]| {
        if hevc {
            (nal[0] >> 1) & 63
        } else {
            nal[0] & 31
        }
    };
    let is_set = |t: u8| {
        if hevc {
            matches!(t, 32..=34)
        } else {
            matches!(t, 7 | 8)
        }
    };
    let mut kinds: Vec<u8> = track.sets.iter().map(|s| kind(s)).collect();
    kinds.sort_unstable();
    let count = kinds.len();
    kinds.dedup();
    let set = |t: u8| track.sets.iter().find(|s| kind(s) == t);
    if kinds.len() != count
        || track.samples.iter().any(|s| s.3 != 0)
        || track.samples.windows(2).any(|w| w[1].2 <= w[0].2)
    {
        return Ok(None);
    }
    // What the decoder keeps of a parameter set: its parsed fields (camera files repeat the
    // track's sets in band without the trailing zero byte of `avcC`). A parser panic means no,
    // and the decoder then meets it as before.
    let parsed = |nal: &[u8]| -> Option<String> {
        guarded(|| parsed_fields(track.codec, kind(nal), nal))
            .ok()
            .flatten()
    };
    fn parsed_fields(codec: Codec, kind: u8, nal: &[u8]) -> Option<String> {
        Some(match (codec, kind) {
            (Codec::H264, 7) => format!(
                "{:?}",
                rusty_h264_decoder::Sps::parse(&h264_rbsp(&nal[1..])).ok()?
            ),
            (Codec::H264, 8) => format!(
                "{:?}",
                rusty_h264_decoder::Pps::parse(&h264_rbsp(&nal[1..])).ok()?
            ),
            (Codec::Hevc, t @ 32..=34) => {
                let rbsp = rusty_h265::nal::unescape(nal.get(2..)?).data;
                match t {
                    32 => format!("{:?}", rusty_h265::ps::parse_vps(&rbsp).ok()?),
                    33 => format!("{:?}", rusty_h265::ps::parse_sps(&rbsp).ok()?),
                    _ => format!("{:?}", rusty_h265::ps::parse_pps(&rbsp).ok()?),
                }
            }
            _ => return None,
        })
    }
    let mut fields = Vec::new();
    for s in track.sets.iter().filter(|s| is_set(kind(s))) {
        let Some(f) = parsed(s) else {
            return Ok(None);
        };
        fields.push((kind(s), f));
    }
    let mut irap_pps = (0, 0);
    match track.codec {
        Codec::H264 => {
            let Some(pps) =
                set(8).and_then(|p| rusty_h264_decoder::Pps::parse(&h264_rbsp(&p[1..])).ok())
            else {
                return Ok(None);
            };
            // Kept though `rusty_h264` drops a redundant slice having changed only the POC
            // state, which an IDR picture resets: no stream with such slices was ever compared.
            if pps.redundant_pic_cnt_present_flag {
                return Ok(None);
            }
        }
        Codec::Hevc => {
            let unescaped = |s: &[u8]| rusty_h265::nal::unescape(&s[2..]).data;
            let (Some(sps), Some(pps)) = (
                set(33).and_then(|s| rusty_h265::ps::parse_sps(&unescaped(s)).ok()),
                set(34).and_then(|p| rusty_h265::ps::parse_pps(&unescaped(p)).ok()),
            ) else {
                return Ok(None);
            };
            let layers = usize::from(sps.max_sub_layers_minus1) + 1;
            if sps
                .ordering
                .iter()
                .take(layers)
                .any(|o| o.max_num_reorder_pics != 0)
            {
                return Ok(None);
            }
            irap_pps = (pps.id, pps.num_extra_slice_header_bits);
        }
    }
    let probe = if hevc {
        None
    } else {
        set(7).and_then(|s| h264_probe_sps(s))
    };
    let len = file.metadata()?.len();
    let mut bytes = Vec::new();
    let mut starts = Vec::with_capacity(track.samples.len());
    // H.264: the sample before holds one picture, which its first slice starts.
    let mut one_picture = false;
    for &(position, size, _, _) in &track.samples {
        if position.checked_add(size).is_none_or(|end| end > len) {
            break;
        }
        bytes.resize(usize::try_from(size)?, 0);
        file.seek(SeekFrom::Start(position))?;
        if file.read_exact(&mut bytes).is_err() {
            break;
        }
        work.surveyed += 1;
        let Ok(nals) = units(&bytes, track.length_size) else {
            // Not decoded at all, as decoding every sample does with it.
            starts.push(false);
            one_picture = false;
            continue;
        };
        for n in nals.iter().filter(|n| is_set(kind(n))) {
            let same = |(k, f): &(u8, String)| *k == kind(n) && parsed(n).as_ref() == Some(f);
            if !track.sets.iter().any(|s| s.as_slice() == *n) && !fields.iter().any(same) {
                return Ok(None);
            }
        }
        let start = match track.codec {
            Codec::H264 => {
                let slices: Vec<_> = nals
                    .iter()
                    .filter(|n| (1..=5).contains(&(n[0] & 31)))
                    .collect();
                let headers: Vec<_> = slices.iter().map(|n| h264_slice(n)).collect();
                let idr = !slices.is_empty()
                    && slices.iter().all(|n| n[0] & 31 == 5 && n[0] & 0x60 != 0)
                    && headers
                        .iter()
                        .all(|h| h.is_some_and(|h| matches!(h.1, 2 | 7)))
                    && headers[0].is_some_and(|h| h.0 == 0);
                let start = idr && one_picture;
                one_picture = !slices.is_empty()
                    && slices.iter().all(|n| matches!(n[0] & 31, 1 | 5))
                    && headers
                        .iter()
                        .enumerate()
                        .all(|(i, h)| h.is_some_and(|h| (h.0 == 0) == (i == 0)));
                start
            }
            Codec::Hevc => {
                if nals
                    .iter()
                    .any(|n| n.len() < 2 || n[0] & 0x80 != 0 || n[1] & 7 == 0)
                {
                    return Ok(None);
                }
                let slices: Vec<_> = nals
                    .iter()
                    .filter(|n| (n[0] >> 1) & 63 <= 21 && n[0] & 1 == 0 && n[1] >> 3 == 0)
                    .collect();
                if slices.iter().any(|n| n.len() < 3)
                    || slices
                        .iter()
                        .enumerate()
                        .any(|(i, n)| (n[2] >> 7 == 1) != (i == 0))
                {
                    return Ok(None);
                }
                !slices.is_empty()
                    && slices
                        .iter()
                        .all(|n| matches!((n[0] >> 1) & 63, 19 | 20) && n[1] & 7 == 1)
                    && hevc_irap_slice_type(slices[0], irap_pps.0, irap_pps.1) == Some(2)
            }
        };
        // Probed here, before any picture is decoded, so the probe never shares the memory
        // limit with a decoder holding the stream's references.
        let start = start
            && match &probe {
                None => track.codec == Codec::Hevc,
                Some(sps) => {
                    work.surveyed += 1;
                    finishes(file, track, sps, starts.len() - 1, &mut bytes)?
                }
            };
        starts.push(start);
    }
    Ok(Some(starts))
}

/// Whether a fresh `rusty_h264` decoder given `sps` (`h264_probe_sps`, missing references made
/// grey) finishes the one picture of sample `index` without an error. Slices parse the same
/// whatever reference pictures a decoder holds, so the decoder that went through every sample
/// ends that sample with the picture finished or after an error that replaced it: either way it
/// meets the next IDR picture as a fresh one does. `bytes` is the caller's sample buffer.
fn finishes(
    file: &mut File,
    track: &Track,
    sps: &[u8],
    index: usize,
    bytes: &mut Vec<u8>,
) -> anyhow::Result<bool> {
    let (position, size, _, _) = track.samples[index];
    bytes.resize(usize::try_from(size)?, 0);
    file.seek(SeekFrom::Start(position))?;
    file.read_exact(bytes)?;
    let mut au = Vec::with_capacity(bytes.len() + 64);
    let sets = track.sets.iter().filter(|s| s[0] & 31 != 7);
    for nal in std::iter::once(sps)
        .chain(sets.map(Vec::as_slice))
        .chain(units(bytes, track.length_size)?)
    {
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(nal);
    }
    let mut d = rusty_h264_decoder::Decoder::new();
    Ok(matches!(guarded(|| d.decode(&au)), Ok(Ok(Some(_)))))
}

/// The sample a fresh decoder can start at instead of decoding `from..` in order: the last start
/// (`shortcuts`) before the first sample presented at `wanted`, so the samples skipped present
/// before it.
fn restart(
    track: &Track,
    starts: &[bool],
    from: usize,
    wanted: f64,
    time: impl Fn(i64) -> f64,
) -> Option<usize> {
    let needed = (from..track.samples.len())
        .find(|i| time(track.samples[*i].2 + track.samples[*i].3) >= wanted - SKIP_MARGIN)?;
    (from + 1..=needed)
        .rev()
        .find(|i| starts.get(*i) == Some(&true))
}

/// Decodes the first `limit` video samples of `path` and calls `each` with every picture FFmpeg
/// would return, in its order, until `each` returns `None`. `Some(t)` says no picture presented
/// before `t` is needed any more: when the stream allows it (`shortcuts`), the samples before
/// the last IDR picture ahead of `t` are then not decoded, which leaves every picture from that
/// IDR on as decoding all of them gives. A sample that does not decode is skipped, as
/// `avcodec_send_packet` failures are. When the limit is reached the picture of the last sample
/// is not returned and nothing is flushed: frame threading with two threads holds it.
/// With `skip` false every sample is decoded in order, as before any skipping.
pub fn decode(
    path: &Path,
    limit: usize,
    skip: bool,
    mut each: impl FnMut(Frame) -> anyhow::Result<Option<f64>>,
) -> anyhow::Result<Work> {
    let mut file = File::open(path)?;
    let track = track(&mut file, limit)?;
    let sequence = match track.codec {
        Codec::H264 => track
            .sets
            .iter()
            .find(|s| s.first().is_some_and(|b| b & 31 == 7))
            .map(|s| h264_sequence(s)),
        Codec::Hevc => track
            .sets
            .iter()
            .find(|s| s.first().is_some_and(|b| (b >> 1) & 63 == 33))
            .map(|s| hevc_sequence(s)),
    }
    .transpose()?;
    // Parameter sets only in band (avc3, hev1): the decoded pictures are checked instead.
    if let Some(s) = &sequence {
        ensure!(
            s.width > 0
                && s.width <= MAX_WIDTH as u64
                && s.height > 0
                && s.height <= MAX_HEIGHT as u64,
            "unsupported thumbnail dimensions"
        );
    }
    let full_range = sequence.is_some_and(|s| s.full_range);
    let mut decoder = new_decoder(&track)?;
    let mut work = Work::default();
    let starts = if skip {
        shortcuts(&mut file, &track, &mut work)?
    } else {
        None
    };
    let reorder = track.samples.iter().any(|s| s.3 != 0);
    let mut pending: Vec<(i64, Image)> = Vec::new();
    let time = |pts: i64| (pts - track.start) as f64 / f64::from(track.timescale);
    let limited = track.samples.len() == limit;
    let mut sample = Vec::new();
    let mut fresh = true;
    let wanted = Cell::new(f64::NEG_INFINITY);
    let mut planned = f64::NEG_INFINITY;
    let mut each = |frame: Frame| -> anyhow::Result<bool> {
        Ok(match each(frame)? {
            None => true,
            Some(t) => {
                wanted.set(t);
                false
            }
        })
    };
    let emit = |pts: i64,
                image: Image,
                each: &mut dyn FnMut(Frame) -> anyhow::Result<bool>|
     -> anyhow::Result<bool> {
        ensure!(
            image.y.width <= MAX_WIDTH && image.y.height <= MAX_HEIGHT,
            "unsupported thumbnail dimensions"
        );
        if pts < track.start {
            return Ok(false);
        }
        each(Frame {
            time: time(pts),
            image,
            full_range,
        })
    };
    // HEVC finishes a picture when the next one starts; `flush` finishes it at once, but leaves
    // a decoder that cannot go on. Used on a sample that may hold the wanted picture: when the
    // caller then wants a time with an IDR picture ahead, the decoder is replaced anyway;
    // otherwise the stretch from `began` is decoded again, its pictures not passed on twice.
    let mut began = 0;
    let mut flushed = None;
    let mut shown = i64::MIN;
    let mut early = true;
    let mut next = 0;
    while next < track.samples.len() {
        if let Some(starts) = &starts
            && wanted.get() > planned
        {
            planned = wanted.get();
            if let Some(start) = restart(&track, starts, next, planned, time) {
                next = start;
                began = start;
                decoder = new_decoder(&track)?;
                fresh = true;
                flushed = None;
                early = true;
                work.restarts.push(start);
            }
        }
        if let Some(pts) = flushed.take() {
            next = began;
            decoder = new_decoder(&track)?;
            fresh = true;
            shown = pts;
            early = false;
            work.replays += 1;
        }
        let index = next;
        next += 1;
        work.decoded += 1;
        let (position, size, dts, offset) = track.samples[index];
        let last = limited && index + 1 == limit;
        let pts = dts.checked_add(offset).context("invalid PTS/DTS")?;
        sample.resize(usize::try_from(size)?, 0);
        file.seek(SeekFrom::Start(position))?;
        if file.read_exact(&mut sample).is_err() {
            // A file cut short keeps what decoded before its end.
            break;
        }
        let Ok(nals) = units(&sample, track.length_size) else {
            continue;
        };
        let mut out = Vec::new();
        match &mut decoder {
            Decoder::H264(d) => {
                let mut au = Vec::with_capacity(sample.len() + 64);
                if fresh {
                    for set in &track.sets {
                        au.extend_from_slice(&[0, 0, 0, 1]);
                        au.extend_from_slice(set);
                    }
                    fresh = false;
                }
                for nal in &nals {
                    au.extend_from_slice(&[0, 0, 0, 1]);
                    au.extend_from_slice(nal);
                }
                match guarded(|| d.decode(&au))? {
                    Ok(Some(frame)) => {
                        let size = (frame.width, frame.height);
                        out.push((pts, h264_image(size, frame.y, frame.u, frame.v)?));
                    }
                    Ok(None) => {}
                    // The decoder keeps no usable state after an error; like FFmpeg, decoding
                    // resumes at the next picture that needs no reference.
                    Err(_) => {
                        **d = rusty_h264_decoder::Decoder::new();
                        fresh = true;
                    }
                }
            }
            Decoder::Hevc(d) => {
                for (i, nal) in nals.iter().enumerate() {
                    let _ = guarded(|| d.push_nal(nal, (i == 0).then_some(pts)))?;
                }
                while let Ok(frame) = guarded(|| d.next_frame())? {
                    out.push((frame.pts.unwrap_or(pts), h265_image(&frame)?));
                }
                // No margin here: a picture just before the wanted time would only make the
                // stretch be decoded again.
                if early && starts.is_some() && !last && time(pts) >= wanted.get() {
                    guarded(|| d.flush())?;
                    while let Ok(frame) = guarded(|| d.next_frame())? {
                        out.push((frame.pts.unwrap_or(pts), h265_image(&frame)?));
                    }
                    flushed = Some(pts);
                    work.flushes += 1;
                }
            }
        }
        out.retain(|(pts, _)| reorder || *pts > shown);
        if last && matches!(decoder, Decoder::H264(_)) {
            out.clear();
        }
        for (pts, image) in out {
            if reorder {
                pending.push((pts, image));
                if pending.len() <= 16 {
                    continue;
                }
                let at = (0..pending.len())
                    .min_by_key(|i| pending[*i].0)
                    .unwrap_or(0);
                let (pts, image) = pending.swap_remove(at);
                if emit(pts, image, &mut each)? {
                    return Ok(work);
                }
            } else if emit(pts, image, &mut each)? {
                return Ok(work);
            }
        }
        if last {
            return Ok(work);
        }
    }
    if let Decoder::Hevc(d) = &mut decoder {
        guarded(|| d.flush())?;
        while let Ok(frame) = guarded(|| d.next_frame())? {
            let image = h265_image(&frame)?;
            let pts = frame.pts.unwrap_or(0);
            if reorder {
                pending.push((pts, image));
            } else if pts > shown && emit(pts, image, &mut each)? {
                return Ok(work);
            }
        }
    }
    pending.sort_by_key(|p| p.0);
    for (pts, image) in pending {
        if emit(pts, image, &mut each)? {
            break;
        }
    }
    Ok(work)
}
