use super::{
    Unsupported,
    annexb::NAL_LIMIT,
    annexb::{Bits, configuration_sets, for_each_nal, is_parameter_set, kind},
    mp4::{Kept, Plan, SAMPLE_LIMIT, Writer},
};
use crate::media::{NalHasher, Signature};
use anyhow::{Context, bail, ensure};
use mpeg2ts::{
    es::StreamType,
    ts::{ReadTsPacket, TransportScramblingControl, TsPacketReader, TsPayload},
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

struct Pes {
    pts: u64,
    dts: u64,
    expected: Option<usize>,
    data: Vec<u8>,
}

/// One complete PES of the single H.264 elementary stream: clocks are raw
/// 33-bit values. Conditions FFmpeg tolerates silently (lost packets,
/// scrambling, transport errors, missing PTS) are refused as unsupported so
/// the FFmpeg engine handles them instead of a different guess here.
fn demux(source: &Path, mut each: impl FnMut(Pes) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let file = std::fs::File::open(source)?;
    let mut reader = TsPacketReader::new(BufReader::with_capacity(1024 * 1024, file));
    let mut video = None;
    let mut continuity: Option<u8> = None;
    let mut pes: Option<Pes> = None;
    let mut finish = |done: Pes| -> anyhow::Result<()> {
        if let Some(expected) = done.expected {
            ensure!(done.data.len() == expected, "truncated PES packet");
        }
        each(done)
    };
    while let Some(packet) = reader
        .read_ts_packet()
        .map_err(|e| anyhow::anyhow!("invalid MPEG-TS packet: {e}"))?
    {
        if packet.header.transport_error_indicator {
            return Err(Unsupported("MPEG-TS transport error").into());
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
        if packet.header.transport_scrambling_control != TransportScramblingControl::NotScrambled {
            return Err(Unsupported("scrambled MPEG-TS").into());
        }
        let counter = packet.header.continuity_counter.as_u8();
        let resync = packet
            .adaptation_field
            .as_ref()
            .is_some_and(|a| a.discontinuity_indicator);
        if !(resync || continuity.is_none_or(|c| (c + 1) & 15 == counter)) {
            return Err(Unsupported("MPEG-TS continuity error (missing packet)").into());
        }
        continuity = Some(counter);
        match payload {
            TsPayload::PesStart(start) => {
                if let Some(done) = pes.take() {
                    finish(done)?;
                }
                let Some(pts) = start.header.pts else {
                    return Err(Unsupported("PES without PTS").into());
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
    finish(pes.context("missing pictures or decoder configuration")?)
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

/// The first read of the TS input hashes the elementary stream with the
/// independent `NalHasher` splitter, sizes every sample and keeps the sample
/// bytes when they fit `BUFFER_LIMIT`; otherwise a second read writes them. Output clocks are DTS minus `offset`, the first (smallest) PTS.
pub fn remux(
    source: &Path,
    output: &Path,
    frames_expected: usize,
    offset: i64,
    terminal: u32,
) -> anyhow::Result<Signature> {
    let mut hash = NalHasher::new(false);
    let mut clock = Clock::default();
    let mut frames: Vec<Frame> = Vec::new();
    let mut first: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut kept = Kept::new();
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
        let (mut size, mut keyframe, mut delimiters, mut pictures) = (0u64, false, 0, 0);
        for_each_nal(&pes.data, |unit| {
            let kind = kind(unit.raw, false)?;
            keyframe |= kind == 5;
            delimiters += usize::from(kind == 9);
            pictures += usize::from((1..=5).contains(&kind));
            if frames.is_empty() && is_parameter_set(kind, false) {
                first.push((kind, unit.raw.to_vec()));
            }
            let sample = unit.sample();
            ensure!(
                !sample.is_empty() && sample.len() <= NAL_LIMIT,
                "NAL unit exceeds bounded memory limit"
            );
            size += 4 + sample.len() as u64;
            kept.nal(sample);
            Ok(())
        })?;
        ensure!(
            size <= SAMPLE_LIMIT,
            "invalid video packet or inspection limit"
        );
        if delimiters > 1 || pictures == 0 {
            return Err(Unsupported("TS packet is not one access unit").into());
        }
        frames.push(Frame {
            dts,
            size,
            keyframe,
        });
        Ok(())
    })?;
    let signature = hash.finish()?;
    ensure!(
        frames.len() == frames_expected,
        "video frame count or timestamps disagree with log"
    );
    let sets = configuration_sets(
        &first
            .iter()
            .map(|(k, v)| (*k, v.as_slice()))
            .collect::<Vec<_>>(),
    );
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
    let mut writer = Writer::create(
        output,
        Plan {
            entry: SampleEntry::Avc1(entry),
            timescale: TIMESCALE,
            samples,
        },
    )?;
    match kept.take() {
        Some(data) => writer.write_samples(&data)?,
        None => demux(source, |pes| {
            for_each_nal(&pes.data, |unit| writer.write_nal(unit.sample()))?;
            writer.end_sample()
        })?,
    }
    writer.finish()?;
    Ok(signature)
}
