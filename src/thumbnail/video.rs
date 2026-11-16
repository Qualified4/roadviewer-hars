//! The first pictures of a camera MP4, decoded by pure Rust decoders (`rusty_h264-decoder`,
//! `rusty_h265`, both `forbid(unsafe_code)` with their SIMD features off) in the order and with
//! the times FFmpeg's `mov` demuxer and a two-thread frame-threaded decoder give them.
use super::scale::{Image, Plane};
use anyhow::{Context, bail, ensure};
use std::{
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
        ensure!(size >= prefix && size <= len - at, "invalid MP4 box size");
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
    let (width, height) = (usize::from(r.u16()?), usize::from(r.u16()?));
    ensure!(
        width > 0 && width <= MAX_WIDTH && height > 0 && height <= MAX_HEIGHT,
        "unsupported thumbnail dimensions"
    );
    r.take(50)?;
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

/// H.264 SPS `video_full_range_flag` (FFmpeg's h264 decoder then outputs `yuvj420p`).
fn h264_full_range(sps: &[u8]) -> anyhow::Result<bool> {
    let mut b = Bits::new(sps.get(1..).context("truncated parameter set")?);
    let profile = b.bits(8)?;
    b.bits(16)?;
    b.ue()?;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma = b.ue()?;
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
    b.ue()?;
    b.bit()?;
    b.ue()?;
    b.ue()?;
    if b.bit()? == 0 {
        b.bit()?;
    }
    b.bit()?;
    if b.bit()? == 1 {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    if b.bit()? == 0 {
        return Ok(false);
    }
    if b.bit()? == 1 && b.bits(8)? == 255 {
        b.bits(32)?;
    }
    if b.bit()? == 1 {
        b.bit()?;
    }
    if b.bit()? == 0 {
        return Ok(false);
    }
    b.bits(3)?;
    Ok(b.bit()? == 1)
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

/// Decodes the first `limit` video samples of `path` and calls `each` with every picture FFmpeg
/// would return, in its order, until `each` returns true. A sample that does not decode is
/// skipped, as `avcodec_send_packet` failures are. When the limit is reached the picture of the
/// last sample is not returned and nothing is flushed: frame threading with two threads holds it.
pub fn decode(
    path: &Path,
    limit: usize,
    mut each: impl FnMut(Frame) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let mut file = File::open(path)?;
    let track = track(&mut file, limit)?;
    let full_range = match track.codec {
        Codec::H264 => track
            .sets
            .iter()
            .find(|s| s.first().is_some_and(|b| b & 31 == 7))
            .map(|s| h264_full_range(s))
            .transpose()?
            .unwrap_or(false),
        Codec::Hevc => hevc_full_range(&track.sets),
    };
    let mut decoder = match track.codec {
        Codec::H264 => Decoder::H264(Box::default()),
        Codec::Hevc => {
            let mut d = Box::new(rusty_h265::Decoder::new());
            for set in &track.sets {
                guarded(|| d.push_nal(set, None))?.map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            Decoder::Hevc(d)
        }
    };
    let reorder = track.samples.iter().any(|s| s.3 != 0);
    let mut pending: Vec<(i64, Image)> = Vec::new();
    let time = |pts: i64| (pts - track.start) as f64 / f64::from(track.timescale);
    let limited = track.samples.len() == limit;
    let mut sample = Vec::new();
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
    for (index, &(position, size, dts, offset)) in track.samples.iter().enumerate() {
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
                if index == 0 {
                    for set in &track.sets {
                        au.extend_from_slice(&[0, 0, 0, 1]);
                        au.extend_from_slice(set);
                    }
                }
                for nal in &nals {
                    au.extend_from_slice(&[0, 0, 0, 1]);
                    au.extend_from_slice(nal);
                }
                if let Ok(Some(frame)) = guarded(|| d.decode(&au))? {
                    let size = (frame.width, frame.height);
                    out.push((pts, h264_image(size, frame.y, frame.u, frame.v)?));
                }
            }
            Decoder::Hevc(d) => {
                for (i, nal) in nals.iter().enumerate() {
                    let _ = guarded(|| d.push_nal(nal, (i == 0).then_some(pts)))?;
                }
                while let Ok(frame) = guarded(|| d.next_frame())? {
                    out.push((frame.pts.unwrap_or(pts), h265_image(&frame)?));
                }
            }
        }
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
                    return Ok(());
                }
            } else if emit(pts, image, &mut each)? {
                return Ok(());
            }
        }
        if last {
            return Ok(());
        }
    }
    if let Decoder::Hevc(d) = &mut decoder {
        guarded(|| d.flush())?;
        while let Ok(frame) = guarded(|| d.next_frame())? {
            let image = h265_image(&frame)?;
            let pts = frame.pts.unwrap_or(0);
            if reorder {
                pending.push((pts, image));
            } else if emit(pts, image, &mut each)? {
                return Ok(());
            }
        }
    }
    pending.sort_by_key(|p| p.0);
    for (pts, image) in pending {
        if emit(pts, image, &mut each)? {
            break;
        }
    }
    Ok(())
}

/// HEVC VUI `video_full_range_flag`, read through `rusty_h265`'s own SPS parser.
fn hevc_full_range(sets: &[Vec<u8>]) -> bool {
    sets.iter()
        .filter(|s| s.len() > 2 && (s[0] >> 1) & 63 == 33)
        .any(|s| {
            let rbsp = rusty_h265::nal::unescape(&s[2..]);
            rusty_h265::ps::parse_sps(&rbsp.data)
                .ok()
                .and_then(|sps| sps.vui.map(|v| v.video_full_range_flag))
                .unwrap_or(false)
        })
}
