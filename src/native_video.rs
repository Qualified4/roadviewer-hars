//! Native packet timestamp assignment for raw HEVC; never opens an encoder.
use anyhow::{Context, bail};
use ffmpeg_next as ffmpeg;
use std::path::Path;

// Own the FFmpeg bitstream filter. Send transfers packet ownership; receive
// returns owned packets. No raw pointers leave this synchronous worker.
struct AnnexB(*mut ffmpeg::ffi::AVBSFContext);
impl Drop for AnnexB {
    fn drop(&mut self) {
        // SAFETY: only this owner frees the context allocated by av_bsf_alloc.
        unsafe { ffmpeg::ffi::av_bsf_free(&mut self.0) };
    }
}
impl AnnexB {
    fn new(stream: &ffmpeg::Stream<'_>, hevc: bool) -> anyhow::Result<Self> {
        Self::configured(
            stream,
            if hevc {
                c"hevc_mp4toannexb"
            } else {
                c"h264_mp4toannexb"
            },
            None,
        )
    }
    fn configured(
        stream: &ffmpeg::Stream<'_>,
        name: &std::ffi::CStr,
        remove_types: Option<&std::ffi::CStr>,
    ) -> anyhow::Result<Self> {
        use ffmpeg::ffi::*;
        // SAFETY: static C names, initialized out-pointer, and stream parameters
        // remain live until copied. RAII frees even partially initialized contexts.
        unsafe {
            let filter = av_bsf_get_by_name(name.as_ptr());
            anyhow::ensure!(!filter.is_null(), "missing Annex B filter");
            let mut ptr = std::ptr::null_mut();
            anyhow::ensure!(
                av_bsf_alloc(filter, &mut ptr) >= 0,
                "allocate Annex B filter"
            );
            let owner = Self(ptr);
            anyhow::ensure!(
                avcodec_parameters_copy((*ptr).par_in, stream.parameters().as_ptr()) >= 0,
                "copy filter parameters"
            );
            (*ptr).time_base_in = stream.time_base().into();
            if let Some(types) = remove_types {
                anyhow::ensure!(
                    av_opt_set(
                        (*ptr).priv_data,
                        c"remove_types".as_ptr(),
                        types.as_ptr(),
                        0
                    ) >= 0,
                    "configure parameter set filter"
                );
            }
            anyhow::ensure!(av_bsf_init(ptr) >= 0, "initialize Annex B filter");
            Ok(owner)
        }
    }
    fn filter_one(&mut self, packet: &mut ffmpeg::Packet) -> anyhow::Result<ffmpeg::Packet> {
        use ffmpeg::codec::packet::Mut;
        // SAFETY: filter_units is a synchronous one-input/one-output filter.
        // Packet ownership is transferred to this live context and a fresh
        // owned packet receives it. Check the drain contract on every call.
        unsafe {
            anyhow::ensure!(
                ffmpeg::ffi::av_bsf_send_packet(self.0, packet.as_mut_ptr()) >= 0,
                "send parameter set filter packet"
            );
            let mut out = ffmpeg::Packet::empty();
            anyhow::ensure!(
                ffmpeg::ffi::av_bsf_receive_packet(self.0, out.as_mut_ptr()) >= 0
                    && !out.is_corrupt()
                    && out.size() > 0,
                "filter removed a video frame"
            );
            let mut extra = ffmpeg::Packet::empty();
            anyhow::ensure!(
                ffmpeg::ffi::av_bsf_receive_packet(self.0, extra.as_mut_ptr()) == -libc::EAGAIN,
                "unexpected buffered video packet"
            );
            Ok(out)
        }
    }
    fn feed(
        &mut self,
        packet: Option<&mut ffmpeg::Packet>,
        hash: &mut crate::media::NalHasher,
    ) -> anyhow::Result<()> {
        use ffmpeg::codec::packet::Mut;
        // SAFETY: the filter owns its initialized context; input is owned and
        // reset by send, each receive writes a fresh RAII-managed packet.
        unsafe {
            let input = packet.map_or(std::ptr::null_mut(), |p| p.as_mut_ptr());
            anyhow::ensure!(
                ffmpeg::ffi::av_bsf_send_packet(self.0, input) >= 0,
                "send bitstream packet"
            );
            loop {
                let mut output = ffmpeg::Packet::empty();
                let code = ffmpeg::ffi::av_bsf_receive_packet(self.0, output.as_mut_ptr());
                if code == -libc::EAGAIN || code == ffmpeg::ffi::AVERROR_EOF {
                    break;
                }
                anyhow::ensure!(
                    code >= 0 && !output.is_corrupt(),
                    "invalid bitstream packet"
                );
                hash.feed(output.data().context("missing bitstream bytes")?)?;
            }
        }
        Ok(())
    }
}

/// One demux pass supplies both packet clocks and the existing Annex B SHA policy.
/// The CLI runs this in an isolated worker with its existing timeout/output cap.
pub fn inspect(source: &Path, kind: &str) -> anyhow::Result<crate::media::Inspection> {
    let _profile = crate::profile::Scope::new("native_inspection");
    use serde_json::json;
    ffmpeg::init()?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);
    let hevc = matches!(
        kind,
        "fcamera.hevc" | "ecamera.hevc" | "fcamera.mp4" | "ecamera.mp4"
    );
    anyhow::ensure!(
        hevc || matches!(kind, "qcamera.ts" | "camera.mp4" | "qcamera.mp4"),
        "unsupported inspection kind"
    );
    let mut options = ffmpeg::Dictionary::new();
    options.set("protocol_whitelist", "file,pipe");
    options.set(
        "format_whitelist",
        if kind.ends_with(".mp4") {
            "mov"
        } else if hevc {
            "hevc"
        } else {
            "mpegts"
        },
    );
    let mut input = ffmpeg::format::input_with_dictionary(source, options)?;
    let videos = input
        .streams()
        .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Video)
        .collect::<Vec<_>>();
    anyhow::ensure!(videos.len() == 1, "expected one video stream");
    let stream = &videos[0];
    anyhow::ensure!(
        stream.parameters().id()
            == if hevc {
                ffmpeg::codec::Id::HEVC
            } else {
                ffmpeg::codec::Id::H264
            },
        "camera codec mismatch"
    );
    let index = stream.index();
    let tb = stream.time_base();
    anyhow::ensure!(
        tb.numerator() > 0 && tb.denominator() > 0,
        "invalid stream time base"
    );
    let duration = (stream.duration() != ffmpeg::ffi::AV_NOPTS_VALUE).then_some(stream.duration());
    let parameters = stream.parameters();
    // SAFETY: the immutable parameters wrapper owns a live AVCodecParameters.
    let (width, height, delay, tag) = unsafe {
        let p = &*parameters.as_ptr();
        (p.width, p.height, p.video_delay, p.codec_tag)
    };
    anyhow::ensure!(
        width > 0
            && width <= 4096
            && height > 0
            && height <= 2160
            && width % 2 == 0
            && height % 2 == 0,
        "unsupported camera dimensions"
    );
    let tag = String::from_utf8_lossy(&tag.to_le_bytes()).into_owned();
    let info = json!({"codec_type":"video","codec_name":if hevc {"hevc"} else {"h264"},"codec_tag_string":tag,"width":width,"height":height,"has_b_frames":delay,"time_base":format!("{}/{}",tb.numerator(),tb.denominator()),"duration_ts":duration,"duration":duration.map(|d|d as f64*f64::from(tb)).map(|d|d.to_string())});
    let mut filter = AnnexB::new(stream, hevc)?;
    drop(videos);
    let mut hash = crate::media::NalHasher::new(hevc);
    let mut packets = Vec::new();
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => break,
            Err(_) => bail!("video packet read failed"),
        }
        if packet.stream() != index || packet.size() == 0 {
            continue;
        }
        anyhow::ensure!(
            !packet.is_corrupt() && packets.len() < 262144 && packet.size() <= 32 * 1024 * 1024,
            "invalid video packet or inspection limit"
        );
        let time = |stamp: Option<i64>| stamp.map(|s| (s as f64 * f64::from(tb)).to_string());
        packets.push(json!({"codec_type":"video","pts":packet.pts(),"dts":packet.dts(),"pts_time":time(packet.pts()),"dts_time":time(packet.dts())}));
        filter.feed(Some(&mut packet), &mut hash)?;
    }
    filter.feed(None, &mut hash)?;
    let mut info = json!({"streams":[info]});
    info["packets"] = serde_json::Value::Array(packets);
    Ok(crate::media::Inspection {
        info,
        signature: hash.finish()?,
    })
}

/// Copy H.264 TS packets into MP4 exactly as the Python PyAV loop does.
/// The input is opened with library defaults: the ffmpeg/ffprobe CLIs force
/// `scan_all_pmts=1`, which makes stream probing read about five seconds
/// instead of stopping after 20 frame intervals. The probed r_frame_rate sets
/// every demuxed packet duration, and MP4 stores the last one as the final
/// sample duration, so the CLI and the Python reference disagree on jittery
/// camera clocks. HARS instead sets the last packet's duration to `terminal`
/// (input time base ticks) from `terminal_duration`, the same value the pure
/// engine writes.
pub fn remux_ts(source: &Path, output: &Path, offset: i64, terminal: i64) -> anyhow::Result<()> {
    anyhow::ensure!(terminal > 0, "invalid last sample duration");
    let _profile = crate::profile::Scope::new("native_ts_mux");
    ffmpeg::init()?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);
    let mut input_options = ffmpeg::Dictionary::new();
    input_options.set("protocol_whitelist", "file,pipe");
    input_options.set("format_whitelist", "mpegts");
    let mut input = ffmpeg::format::input_with_dictionary(source, input_options)?;
    let videos = input
        .streams()
        .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Video)
        .collect::<Vec<_>>();
    anyhow::ensure!(videos.len() == 1, "expected one video stream");
    let stream = &videos[0];
    anyhow::ensure!(
        stream.parameters().id() == ffmpeg::codec::Id::H264,
        "not H.264"
    );
    let stream_index = stream.index();
    let input_time_base = stream.time_base();
    let mut out = ffmpeg::format::output_as(output, "mp4")?;
    {
        let mut target = out.add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None))?;
        target.set_parameters(stream.parameters());
        target.set_time_base(input_time_base);
        // SAFETY: target owns the AVCodecParameters for the lifetime of this block.
        // The MPEG-TS stream type is not an MP4 tag; zero lets the muxer pick avc1.
        unsafe {
            (*target.parameters().as_mut_ptr()).codec_tag = 0;
        }
    }
    drop(videos);
    let mut options = ffmpeg::Dictionary::new();
    options.set("movflags", "+faststart");
    drop(out.write_header_with(options)?);
    let time_base = out.stream(0).context("missing output stream")?.time_base();
    let mut count = 0usize;
    // One packet of lookahead: only the last packet's duration is replaced.
    let mut held: Option<ffmpeg::Packet> = None;
    let mut write = |mut packet: ffmpeg::Packet| -> anyhow::Result<()> {
        packet.rescale_ts(input_time_base, time_base);
        packet.set_position(-1);
        packet.set_stream(0);
        packet.write_interleaved(&mut out)?;
        Ok(())
    };
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => break,
            Err(_) => bail!("video packet read failed"),
        }
        if packet.stream() != stream_index || packet.size() == 0 {
            continue;
        }
        let (Some(pts), Some(dts)) = (packet.pts(), packet.dts()) else {
            bail!("video packet without timestamps");
        };
        anyhow::ensure!(
            !packet.is_corrupt() && count < 262144 && packet.size() <= 32 * 1024 * 1024,
            "invalid video packet or mux limit"
        );
        packet.set_pts(Some(pts - offset));
        packet.set_dts(Some(dts - offset));
        if let Some(previous) = held.replace(packet) {
            write(previous)?;
        }
        count += 1;
    }
    let mut last = held.context("no video packets")?;
    last.set_duration(terminal);
    write(last)?;
    drop(write);
    out.write_trailer()?;
    Ok(())
}

/// `terminal` is the last sample duration in microseconds (`terminal_duration`).
pub fn remux_hevc(
    source: &Path,
    output: &Path,
    times: &[f64],
    terminal: i64,
) -> anyhow::Result<crate::media::Signature> {
    let _profile = crate::profile::Scope::new("native_hevc_mux_with_input_hash");
    anyhow::ensure!(
        !times.is_empty()
            && times.iter().all(|t| t.is_finite())
            && times.windows(2).all(|t| t[1] > t[0]),
        "invalid camera clock"
    );
    anyhow::ensure!(terminal > 0, "invalid last sample duration");
    ffmpeg::init()?;
    // FFmpeg's logging is process-global and may include arbitrary media metadata.
    ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);
    let mut input_options = ffmpeg::Dictionary::new();
    input_options.set("protocol_whitelist", "file,pipe");
    input_options.set("format_whitelist", "hevc");
    let mut input = ffmpeg::format::input_with_dictionary(source, input_options)?;
    anyhow::ensure!(
        input
            .streams()
            .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Video)
            .count()
            == 1,
        "expected one video stream"
    );
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .context("missing HEVC stream")?;
    anyhow::ensure!(
        stream.parameters().id() == ffmpeg::codec::Id::HEVC,
        "not HEVC"
    );
    let stream_index = stream.index();
    // hvc1 parameter sets belong to hvcC, not samples. FFmpeg 5/6's MOV muxer
    // keeps in-band copies; filter only VPS/SPS/PPS after hashing the original.
    // The stream parameters copied to the output retain the decoder config.
    let mut parameter_sets = AnnexB::configured(&stream, c"filter_units", Some(c"32|33|34"))?;
    let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;
    anyhow::ensure!(
        !decoder.has_b_frames(),
        "HEVC B frame mapping is not supported"
    );
    let (w, h) = (decoder.width(), decoder.height());
    anyhow::ensure!(
        w > 0 && w <= 4096 && h > 0 && h <= 2160 && w % 2 == 0 && h % 2 == 0,
        "unsupported camera dimensions"
    );
    let mut out = ffmpeg::format::output_as(output, "mp4")?;
    {
        let mut target = out.add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None))?;
        target.set_parameters(stream.parameters());
        target.set_time_base(ffmpeg::Rational(1, 1_000_000));
        // SAFETY: target owns the AVCodecParameters for the lifetime of this block.
        // libavformat has no safe codec_tag setter; only the integer tag is changed.
        unsafe {
            (*target.parameters().as_mut_ptr()).codec_tag = u32::from_le_bytes(*b"hvc1");
        }
    }
    let mut options = ffmpeg::Dictionary::new();
    options.set("movflags", "+faststart");
    drop(out.write_header_with(options)?);
    let time_base = out.stream(0).context("missing output stream")?.time_base();
    let mut index = 0;
    let mut hash = crate::media::NalHasher::new(true);
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => break,
            Err(_) => bail!("HEVC packet read failed"),
        }
        if packet.stream() != stream_index || packet.size() == 0 {
            continue;
        }
        anyhow::ensure!(
            !packet.is_corrupt() && index < times.len() && packet.size() <= 32 * 1024 * 1024,
            "invalid HEVC packet count or corrupt packet"
        );
        // Raw HEVC packets are Annex B already; the existing mp4toannexb
        // filter is a passthrough for this input format. Keep the exact same
        // bounded NAL/header SHA policy, in the pass that copies the packets.
        hash.feed(packet.data().context("missing HEVC packet bytes")?)?;
        let mut packet = parameter_sets.filter_one(&mut packet)?;
        let stamp = (times[index] * 1_000_000.).round_ties_even() as i64;
        let duration = match times.get(index + 1) {
            Some(next) => ((next - times[index]) * 1_000_000.).round() as i64,
            None => terminal,
        };
        packet.set_pts(Some(stamp));
        packet.set_dts(Some(stamp));
        packet.set_duration(duration);
        packet.rescale_ts(ffmpeg::Rational(1, 1_000_000), time_base);
        packet.set_position(-1);
        packet.set_stream(0);
        packet.write_interleaved(&mut out)?;
        index += 1;
    }
    anyhow::ensure!(
        index == times.len(),
        "HEVC frame count differs from logged clock"
    );
    out.write_trailer()?;
    hash.finish()
}
