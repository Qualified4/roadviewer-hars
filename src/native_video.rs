//! Native packet timestamp assignment for raw HEVC; never opens an encoder.
use anyhow::{Context, bail};
use ffmpeg_next as ffmpeg;
use std::path::Path;

pub fn remux_hevc(source: &Path, output: &Path, times: &[f64]) -> anyhow::Result<()> {
    ffmpeg::init()?;
    // FFmpeg's logging is process-global and may include arbitrary media metadata.
    ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);
    let mut input_options = ffmpeg::Dictionary::new();
    input_options.set("protocol_whitelist", "file,pipe");
    input_options.set("format_whitelist", "hevc");
    let mut input = ffmpeg::format::input_with_dictionary(source, input_options)?;
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .context("missing HEVC stream")?;
    anyhow::ensure!(
        stream.parameters().id() == ffmpeg::codec::Id::HEVC,
        "not HEVC"
    );
    let stream_index = stream.index();
    let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;
    anyhow::ensure!(
        !decoder.has_b_frames(),
        "HEVC B frame mapping is not supported"
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
            !packet.is_corrupt() && index < times.len(),
            "invalid HEVC packet count or corrupt packet"
        );
        let stamp = (times[index] * 1_000_000.).round() as i64;
        let duration = ((times.get(index + 1).copied().unwrap_or(times[index] + 0.05)
            - times[index])
            * 1_000_000.)
            .round() as i64;
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
    Ok(())
}
