use super::{
    Stage, Unsupported,
    annexb::{Bits, Sets, for_each_nal, is_parameter_set, kind},
    mp4::{Payload, Plan},
};
use crate::media::{Inspection, NalHasher, Signature};
use anyhow::{Context, bail, ensure};
use mpeg2ts::{
    es::StreamType,
    ts::{ReadTsPacket, TsPacketReader, TsPayload},
};
use shiguredo_mp4::{
    bitstream::h264::{H264SampleEntryConfig, LengthSize, build_avc1_box},
    boxes::SampleEntry,
};
use std::{io::BufReader, path::Path};

pub const TIMESCALE: u32 = 90_000;
const PES_LIMIT: usize = 32 * 1024 * 1024;
const PACKET_LIMIT: usize = 262_144;
const WRAP: u64 = 1 << 33;

fn is_video(kind: StreamType) -> bool {
    matches!(
        kind,
        StreamType::Mpeg1Video
            | StreamType::Mpeg2Video
            | StreamType::Mpeg4H263BasedVideo
            | StreamType::H264
            | StreamType::H265
            | StreamType::ChineseVideoStandard
            | StreamType::UltraHdVideo
            | StreamType::H264WithAes128Cbc
            | StreamType::MicrosoftWindowsMediaVideo9
    )
}

/// 33-bit MPEG clocks made monotonic across a single wrap per step.
#[derive(Default)]
struct Clock {
    last: Option<u64>,
}
impl Clock {
    fn unwrap(&mut self, raw: u64) -> u64 {
        let value = match self.last {
            None => raw,
            Some(last) => {
                let base = last - last % WRAP;
                [
                    base + raw,
                    base + raw + WRAP,
                    (base + raw).wrapping_sub(WRAP),
                ]
                .into_iter()
                .min_by_key(|v| v.abs_diff(last))
                .unwrap_or(raw)
            }
        };
        self.last = Some(value);
        value
    }
}

pub(crate) struct Pes {
    pub pts: u64,
    pub dts: u64,
    /// `random_access_indicator` of the packet that starts this PES.
    pub random_access: bool,
    expected: Option<usize>,
    pub data: Vec<u8>,
}

/// One complete PES of the single H.264 elementary stream: clocks are raw
/// 33-bit values. A lost packet, a transport error or a PES without PTS fails
/// the read: guessing the missing bytes or clock would change the stream. The
/// scrambling bits are ignored, as the Python reference's demuxer ignores
/// them: the payload is copied as it is and the read-back compares it byte for
/// byte. A final packet shorter than 188 bytes is left out unless it holds
/// video payload, which fails the read (`truncated_video_payload`). Returns
/// the video PID.
pub(crate) fn demux(
    source: &Path,
    each: impl FnMut(Pes) -> anyhow::Result<()>,
) -> anyhow::Result<u16> {
    use std::io::Read;
    let file = std::fs::File::open(source)?;
    let length = file.metadata()?.len();
    let whole = length - length % 188;
    let pid = demux_from(
        BufReader::with_capacity(1024 * 1024, file).take(whole),
        each,
    )?;
    ensure!(
        !truncated_video_payload(source, pid)?,
        "the last MPEG-TS packet is cut inside video payload"
    );
    Ok(pid)
}

/// Keeps the reader's own error: the packet reader reports it as text only, and a failing disk
/// must stay an OS error (a resource failure), not read as a damaged input.
struct Watched<R> {
    inner: R,
    failed: std::rc::Rc<std::cell::RefCell<Option<std::io::Error>>>,
}
impl<R: std::io::Read> std::io::Read for Watched<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf).inspect_err(|e| {
            if e.kind() == std::io::ErrorKind::Interrupted {
                return;
            }
            let copy = match e.raw_os_error() {
                Some(code) => std::io::Error::from_raw_os_error(code),
                None => std::io::Error::new(e.kind(), e.to_string()),
            };
            *self.failed.borrow_mut() = Some(copy);
        })
    }
}

fn demux_from(
    input: impl std::io::Read,
    mut each: impl FnMut(Pes) -> anyhow::Result<()>,
) -> anyhow::Result<u16> {
    let failed = std::rc::Rc::default();
    let mut reader = TsPacketReader::new(Watched {
        inner: input,
        failed: std::rc::Rc::clone(&failed),
    });
    let mut video = None;
    let mut continuity: Option<u8> = None;
    let mut pes: Option<Pes> = None;
    let mut finish = |done: Pes| -> anyhow::Result<()> {
        if let Some(expected) = done.expected {
            ensure!(done.data.len() == expected, "truncated PES packet");
        }
        each(done)
    };
    while let Some(packet) = reader.read_ts_packet().map_err(|e| match failed.take() {
        Some(io) => anyhow::Error::new(io).context("reading MPEG-TS"),
        None => anyhow::anyhow!("invalid MPEG-TS packet: {e}"),
    })? {
        if packet.header.transport_error_indicator {
            bail!("MPEG-TS transport error");
        }
        let pid = packet.header.pid.as_u16();
        let payload = match packet.payload {
            Some(TsPayload::Pmt(pmt)) => {
                let videos = pmt
                    .es_info
                    .iter()
                    .filter(|es| is_video(es.stream_type))
                    .collect::<Vec<_>>();
                ensure!(videos.len() == 1, "expected one video stream");
                ensure!(
                    videos[0].stream_type == StreamType::H264,
                    "camera codec mismatch"
                );
                let found = videos[0].elementary_pid.as_u16();
                ensure!(video.is_none_or(|v| v == found), "video PID changed");
                video = Some(found);
                continue;
            }
            Some(payload) if Some(pid) == video => payload,
            _ => continue,
        };
        let counter = packet.header.continuity_counter.as_u8();
        let resync = packet
            .adaptation_field
            .as_ref()
            .is_some_and(|a| a.discontinuity_indicator);
        let random_access = packet
            .adaptation_field
            .as_ref()
            .is_some_and(|a| a.random_access_indicator);
        if !(resync || continuity.is_none_or(|c| (c + 1) & 15 == counter)) {
            bail!("MPEG-TS continuity error (missing packet)");
        }
        continuity = Some(counter);
        match payload {
            TsPayload::PesStart(start) => {
                if let Some(done) = pes.take() {
                    finish(done)?;
                }
                let Some(pts) = start.header.pts else {
                    bail!("PES without PTS");
                };
                let pts = pts.as_u64();
                let dts = start.header.dts.map_or(pts, |d| d.as_u64());
                // mpeg2ts hides PES header stuffing; its own reader uses this
                // length, so stuffed headers fail as a mismatch, never silently.
                let header = 3
                    + start.header.pts.map_or(0, |_| 5)
                    + start.header.dts.map_or(0, |_| 5)
                    + start.header.escr.map_or(0, |_| 6);
                let expected = (start.pes_packet_len != 0)
                    .then(|| usize::from(start.pes_packet_len).checked_sub(header))
                    .map(|v| v.context("invalid PES length"))
                    .transpose()?;
                pes = Some(Pes {
                    pts,
                    dts,
                    random_access,
                    expected,
                    data: start.data.to_vec(),
                });
            }
            TsPayload::PesContinuation(bytes) => {
                let current = pes.as_mut().context("PES continuation without start")?;
                ensure!(
                    current.data.len() + bytes.len() <= PES_LIMIT,
                    "PES exceeds bounded memory limit"
                );
                current.data.extend_from_slice(&bytes);
            }
            _ => bail!("unexpected payload on video PID"),
        }
    }
    finish(pes.context("missing pictures or decoder configuration")?)?;
    video.context("expected one video stream")
}

/// A final packet shorter than 188 bytes is dropped by the packet reader. When it carries payload
/// of the video stream, the last picture would lose bytes that are in the file, so the read refuses
/// it. A cut at a packet boundary, or inside the first four bytes, drops nothing the file holds.
fn truncated_video_payload(source: &Path, pid: u16) -> anyhow::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(source)?;
    let length = file.metadata()?.len();
    let partial = (length % 188) as usize;
    if partial < 4 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(length - partial as u64))?;
    let mut tail = vec![0; partial];
    file.read_exact(&mut tail)?;
    // Not a packet start: the file is not 188-byte framed from its first byte,
    // so what the reader drops is unknown.
    if tail[0] != 0x47 {
        return Ok(true);
    }
    if (u16::from(tail[1] & 0x1f) << 8 | u16::from(tail[2])) != pid || tail[3] & 0x10 == 0 {
        return Ok(false);
    }
    let payload = if tail[3] & 0x20 != 0 {
        5 + usize::from(tail.get(4).copied().unwrap_or(183))
    } else {
        4
    };
    Ok(partial > payload)
}

/// Width and height the first access unit's SPS states, cropped; its SPS and PPS must precede
/// its picture, as the conversion's decoder configuration is taken from there.
fn first_picture_size(data: &[u8]) -> anyhow::Result<(u16, u16)> {
    let mut sets = Sets::default();
    for_each_nal(data, |unit| {
        let kind = kind(unit.raw, false)?;
        if is_parameter_set(kind, false) {
            sets.add(kind, unit.raw)?;
        }
        Ok(())
    })?;
    super::annexb::picture_size(false, &sets.configuration())
}

fn stream_info(width: u16, height: u16) -> serde_json::Value {
    serde_json::json!({"codec_type":"video","codec_name":"h264","width":width,"height":height,"time_base":format!("1/{TIMESCALE}")})
}

/// The camera original as the conversion reads it before either step: its one video stream, the
/// clock of every PES (33-bit clocks unwrapped), the signature of its elementary stream and the
/// size its first SPS states.
pub fn inspect(source: &Path) -> anyhow::Result<Inspection> {
    let mut hash = NalHasher::new(false);
    let (mut pts_clock, mut dts_clock) = (Clock::default(), Clock::default());
    let mut packets = Vec::new();
    let mut size = None;
    let unit = 1f64 / f64::from(TIMESCALE);
    demux(source, |pes| {
        ensure!(
            packets.len() < PACKET_LIMIT,
            "invalid video packet or inspection limit"
        );
        if size.is_none() {
            size = Some(first_picture_size(&pes.data)?);
        }
        hash.feed(&pes.data)?;
        let pts = i64::try_from(pts_clock.unwrap(pes.pts))?;
        let dts = i64::try_from(dts_clock.unwrap(pes.dts))?;
        let time = |s: i64| (s as f64 * unit).to_string();
        packets.push(serde_json::json!({"codec_type":"video","pts":pts,"dts":dts,"pts_time":time(pts),"dts_time":time(dts)}));
        Ok(())
    })?;
    let (width, height) = size.context("missing pictures or decoder configuration")?;
    Ok(Inspection {
        info: serde_json::json!({"streams":[stream_info(width, height)],"packets":packets}),
        signature: hash.finish()?,
    })
}

/// The upload check of a TS original: its stream table and first access unit only, as a probe
/// reads the start of a file.
pub fn probe(source: &Path) -> anyhow::Result<serde_json::Value> {
    #[derive(Debug)]
    struct Enough;
    impl std::fmt::Display for Enough {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("first access unit read")
        }
    }
    impl std::error::Error for Enough {}
    let mut size = None;
    let result = demux(source, |pes| {
        size = Some(first_picture_size(&pes.data)?);
        Err(Enough.into())
    });
    match result {
        Err(e) if e.downcast_ref::<Enough>().is_some() => {}
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    let (width, height) = size.context("missing pictures or decoder configuration")?;
    Ok(serde_json::json!({"streams":[stream_info(width, height)]}))
}

/// `frame_mbs_only_flag`: field-coded pictures may span two PES packets that
/// FFmpeg's parser joins, so only progressive SPS are handled here.
fn progressive(sps: &[u8]) -> anyhow::Result<bool> {
    let sps = super::annexb::trim(sps);
    ensure!(sps.len() > 4, "truncated SPS");
    let mut r = Bits::new(&sps[1..]);
    let profile = r.bits(8)?;
    r.skip(16)?;
    r.ue()?;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        if r.ue()? == 3 {
            r.skip(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.skip(1)?;
        if r.bit()? == 1 {
            return Err(Unsupported("H.264 scaling matrix").into());
        }
    }
    r.ue()?;
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.skip(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()?.min(256) {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?;
    r.skip(1)?;
    r.ue()?;
    r.ue()?;
    Ok(r.bit()? == 1)
}

struct Frame {
    dts: u64,
    size: u64,
    keyframe: bool,
}

/// One read of the TS input hashes the elementary stream with the independent
/// `NalHasher` splitter, sizes every sample and stages the sample bytes
/// (`Payload`). Output clocks are DTS minus `offset`, the first (smallest) PTS.
pub fn remux(
    source: &Path,
    output: &Path,
    frames_expected: usize,
    offset: i64,
    terminal: u32,
) -> anyhow::Result<Signature> {
    let (plan, signature, payload) = (|| {
        let _profile = crate::profile::Scope::new("pure_ts_read");
        let mut hash = NalHasher::new(false);
        let mut clock = Clock::default();
        let mut frames: Vec<Frame> = Vec::new();
        let mut sets = Sets::default();
        let mut payload = Payload::create(output)?;
        demux(source, |pes| {
            ensure!(
                frames.len() < PACKET_LIMIT,
                "invalid video packet or inspection limit"
            );
            hash.feed(&pes.data)?;
            let dts = clock.unwrap(pes.dts);
            if pes.pts != pes.dts {
                return Err(Unsupported("H.264 picture reordering in TS").into());
            }
            if let Some(last) = frames.last() {
                ensure!(dts > last.dts, "invalid PTS/DTS");
            } else if i64::try_from(pes.pts)? != offset {
                return Err(Unsupported("first TS picture is not the earliest").into());
            }
            let (mut keyframe, mut delimiters, mut starts) = (false, 0, 0);
            for_each_nal(&pes.data, |unit| {
                let kind = kind(unit.raw, false)?;
                keyframe |= kind == 5;
                delimiters += usize::from(kind == 9);
                if matches!(kind, 1 | 2 | 5) {
                    let body = super::annexb::trim(unit.raw);
                    // first_mb_in_slice == 0 starts a picture.
                    starts += usize::from(body.get(1).is_some_and(|b| b & 0x80 != 0));
                }
                if frames.is_empty() && is_parameter_set(kind, false) {
                    sets.add(kind, unit.raw)?;
                }
                payload.nal(unit.sample())
            })?;
            if delimiters > 1 || starts != 1 {
                return Err(Unsupported("TS packet is not one access unit").into());
            }
            frames.push(Frame {
                dts,
                size: payload.end_sample(),
                keyframe,
            });
            Ok(())
        })?;
        let signature = hash.finish()?;
        ensure!(
            frames.len() == frames_expected,
            "video frame count or timestamps disagree with log"
        );
        let sets = sets.configuration();
        let pick = |k: u8| {
            sets.iter()
                .filter(|(kind, _)| *kind == k)
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        };
        let (sps, pps) = (pick(7), pick(8));
        ensure!(
            !sps.is_empty() && !pps.is_empty(),
            "missing pictures or decoder configuration"
        );
        ensure!(frames[0].keyframe, "first H.264 picture is not a keyframe");
        if !progressive(&sps[0])? {
            return Err(Unsupported("interlaced H.264").into());
        }
        let entry = build_avc1_box(
            &sps,
            &pps,
            &H264SampleEntryConfig {
                length_size: LengthSize::FourBytes,
            },
        )
        .map_err(|e| anyhow::anyhow!("invalid H.264 decoder configuration: {e}"))?;
        super::mp4::check_dimensions(entry.visual.width, entry.visual.height)?;
        let samples = frames
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let duration = match frames.get(i + 1) {
                    Some(next) => u32::try_from(next.dts - f.dts).context("invalid PTS/DTS")?,
                    None => terminal,
                };
                Ok((f.size, duration, f.keyframe))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let plan = Plan {
            entry: SampleEntry::Avc1(entry),
            timescale: TIMESCALE,
            samples,
        };
        anyhow::Ok((plan, signature, payload))
    })()
    .context(Stage::Read)?;
    let _profile = crate::profile::Scope::new("pure_ts_write");
    super::mp4::write(output, &plan, payload).context(Stage::Write)?;
    Ok(signature)
}

#[cfg(test)]
mod tests {
    use crate::pure_remux::{Failure, FailureClass, Stage};

    fn class(input: impl std::io::Read) -> (FailureClass, String) {
        let error = super::demux_from(input, |_| Ok(())).unwrap_err();
        let failure = Failure::classify(&error.context(Stage::Read));
        (failure.class, failure.code)
    }

    /// Memory is the machine (a later attempt may pass); a bad sector, a denied read or a damaged
    /// file fails every attempt, so it is the input's.
    #[test]
    fn only_clearing_os_errors_are_resource_failures_and_garbage_stays_invalid() {
        struct Failing(i32);
        impl std::io::Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(self.0))
            }
        }
        assert_eq!(
            class(Failing(libc::ENOMEM)),
            (FailureClass::Resource, "out_of_memory".into())
        );
        for errno in [libc::EIO, libc::EACCES, libc::ENOENT, libc::ESTALE] {
            assert_eq!(
                class(Failing(errno)),
                (FailureClass::Invalid, "invalid_input".into()),
                "{errno}"
            );
        }
        assert_eq!(
            class(&[0u8; 188 * 2][..]),
            (FailureClass::Invalid, "invalid_input".into())
        );
    }
}
