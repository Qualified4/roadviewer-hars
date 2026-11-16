use super::{
    annexb::{Pictures, for_each_nal, is_parameter_set},
    mp4::{Span, Writer},
};
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
// Only a one-frame stream has no DTS delta to repeat; qcamera's nominal 20 fps.
const SINGLE_FRAME_DURATION: u32 = 4500;

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

struct Pes {
    pts: u64,
    dts: u64,
    expected: Option<usize>,
    data: Vec<u8>,
}

struct Pending {
    span: Span,
    dts: u64,
    cto: i64,
    keyframe: bool,
    entry: Option<SampleEntry>,
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

struct Muxer<'a> {
    writer: &'a mut Writer,
    pictures: &'a mut Pictures,
    clock: Clock,
    first: Option<(u64, u64)>,
    entry: Option<SampleEntry>,
    pending: Option<Pending>,
    frames: usize,
    keyframes: usize,
    last_delta: Option<u32>,
}
impl Muxer<'_> {
    fn emit(&mut self, next_dts: Option<u64>) -> anyhow::Result<()> {
        let Some(p) = self.pending.take() else {
            return Ok(());
        };
        let duration = match next_dts {
            Some(next) => {
                ensure!(next > p.dts, "invalid PTS/DTS");
                let delta = u32::try_from(next - p.dts).context("invalid PTS/DTS")?;
                self.last_delta = Some(delta);
                delta
            }
            None => self.last_delta.unwrap_or(SINGLE_FRAME_DURATION),
        };
        self.writer.append(
            p.span,
            duration,
            p.keyframe,
            (p.cto != 0).then_some(p.cto),
            p.entry,
        )
    }
    fn frame(&mut self, pes: Pes) -> anyhow::Result<()> {
        if let Some(expected) = pes.expected {
            ensure!(pes.data.len() == expected, "truncated PES packet");
        }
        ensure!(
            self.frames < PACKET_LIMIT,
            "invalid video packet or inspection limit"
        );
        let dts = self.clock.unwrap(pes.dts);
        let reorder = pes.pts.wrapping_sub(pes.dts) % WRAP;
        ensure!(reorder < WRAP / 2, "PTS before DTS");
        let pts = dts + reorder;
        let (pts0, dts0) = *self.first.get_or_insert((pts, dts));
        ensure!(dts >= dts0, "invalid PTS/DTS");
        let mut keyframe = false;
        let mut sets: [Vec<Vec<u8>>; 2] = Default::default();
        let configured = self.entry.is_some();
        let (pictures, writer) = (&mut *self.pictures, &mut *self.writer);
        for_each_nal(&pes.data, |unit| {
            let kind = pictures.add(unit)?;
            keyframe |= kind == 5;
            if !configured && is_parameter_set(kind, false) {
                sets[usize::from(kind - 7)].push(unit.to_vec());
            }
            writer.write_nal(unit)
        })?;
        let span = self.writer.end_data()?;
        let entry = if configured {
            None
        } else {
            let [sps, pps] = &sets;
            ensure!(
                !sps.is_empty() && !pps.is_empty(),
                "missing pictures or decoder configuration"
            );
            ensure!(keyframe, "first H.264 picture is not a keyframe");
            let entry = build_avc1_box(
                sps,
                pps,
                &H264SampleEntryConfig {
                    length_size: LengthSize::FourBytes,
                },
            )
            .map_err(|e| anyhow::anyhow!("invalid H.264 decoder configuration: {e}"))?;
            super::mp4::check_dimensions(entry.visual.width, entry.visual.height)?;
            let entry = SampleEntry::Avc1(entry);
            self.entry = Some(entry.clone());
            Some(entry)
        };
        self.emit(Some(dts))?;
        let cto = i64::try_from(pts - pts0)? - i64::try_from(dts - dts0)?;
        self.pending = Some(Pending {
            span,
            dts,
            cto,
            keyframe,
            entry,
        });
        self.frames += 1;
        self.keyframes += usize::from(keyframe);
        Ok(())
    }
}

pub struct Outcome {
    pub frames: usize,
    pub keyframes: usize,
}

/// One TS read demuxes PES, hashes NAL units and writes samples. Output
/// timestamps are PES clocks relative to the first PTS; PTS before the first
/// one become negative composition offsets (ctts v1) instead of an edit list.
pub fn mux(source: &Path, writer: &mut Writer, pictures: &mut Pictures) -> anyhow::Result<Outcome> {
    let file = std::fs::File::open(source)?;
    let mut reader = TsPacketReader::new(BufReader::with_capacity(1024 * 1024, file));
    let mut muxer = Muxer {
        writer,
        pictures,
        clock: Clock::default(),
        first: None,
        entry: None,
        pending: None,
        frames: 0,
        keyframes: 0,
        last_delta: None,
    };
    let mut video = None;
    let mut continuity: Option<u8> = None;
    let mut pes: Option<Pes> = None;
    while let Some(packet) = reader
        .read_ts_packet()
        .map_err(|e| anyhow::anyhow!("invalid MPEG-TS packet: {e}"))?
    {
        ensure!(
            !packet.header.transport_error_indicator,
            "MPEG-TS transport error"
        );
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
        ensure!(
            resync || continuity.is_none_or(|c| (c + 1) & 15 == counter),
            "MPEG-TS continuity error (missing packet)"
        );
        continuity = Some(counter);
        match payload {
            TsPayload::PesStart(start) => {
                if let Some(done) = pes.take() {
                    muxer.frame(done)?;
                }
                let pts = start.header.pts.context("PES without PTS")?.as_u64();
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
    let last = pes.context("missing pictures or decoder configuration")?;
    muxer.frame(last)?;
    muxer.emit(None)?;
    Ok(Outcome {
        frames: muxer.frames,
        keyframes: muxer.keyframes,
    })
}
