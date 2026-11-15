//! Bounded native pixel decoding; all selection and luma calculations are Rust.
use anyhow::Context;
use ffmpeg_next as av;
use serde_json::{Value, json};
use std::path::Path;

fn small(frame: &av::frame::Video) -> anyhow::Result<av::frame::Video> {
    let height =
        ((frame.height() as f64 * 320. / frame.width() as f64 / 2.).round_ties_even() as u32 * 2)
            .max(2);
    let mut scaler = av::software::scaling::Context::get(
        frame.format(),
        frame.width(),
        frame.height(),
        av::format::Pixel::YUVJ420P,
        320,
        height,
        av::software::scaling::flag::Flags::BILINEAR,
    )?;
    let mut output = av::frame::Video::empty();
    scaler.run(frame, &mut output)?;
    Ok(output)
}
fn stats(frame: &av::frame::Video) -> (f64, f64, u8, u8) {
    let mut hist = [0u64; 256];
    let plane = frame.data(0);
    let stride = frame.stride(0);
    for row in (0..frame.height() as usize).step_by(2) {
        for col in (0..frame.width() as usize).step_by(2) {
            hist[plane[row * stride + col] as usize] += 1;
        }
    }
    let n = hist.iter().sum::<u64>() as f64;
    let mean = hist
        .iter()
        .enumerate()
        .map(|(i, c)| i as f64 * (*c as f64))
        .sum::<f64>()
        / n;
    let std = (hist
        .iter()
        .enumerate()
        .map(|(i, c)| *c as f64 * (i as f64 - mean).powi(2))
        .sum::<f64>()
        / n)
        .sqrt();
    let mut count = 0;
    let mut low = None;
    let mut high = None;
    for (i, c) in hist.iter().enumerate() {
        count += c;
        if low.is_none() && count as f64 >= n * 0.01 {
            low = Some(i as u8);
        }
        if high.is_none() && count as f64 >= n * 0.99 {
            high = Some(i as u8);
        }
    }
    (mean, std, low.unwrap(), high.unwrap())
}
fn score((mean, std, _, _): (f64, f64, u8, u8)) -> f64 {
    std * (mean / 60.).min(1.) * ((250. - mean).max(0.) / 60.).min(1.)
}
fn level(frame: &mut av::frame::Video, (mean, _, low, high): (f64, f64, u8, u8)) {
    let gain = (235. / (high as f64 - low as f64).max(1.)).min(3.);
    let stretched = ((mean - low as f64) * gain).clamp(1., 255.);
    let gamma = if stretched >= 100. {
        1.
    } else {
        ((110f64 / 255.).ln() / (stretched / 255.).ln()).max(0.6)
    };
    let lut: Vec<u8> = (0..256)
        .map(|v| {
            let stretched = ((v as f64 - low as f64) * gain)
                .round_ties_even()
                .clamp(0., 255.);
            (255. * (stretched / 255.).powf(gamma))
                .round_ties_even()
                .min(255.) as u8
        })
        .collect();
    for v in frame.data_mut(0) {
        *v = lut[*v as usize];
    }
}

pub fn make(source: &Path, output: &Path) -> anyhow::Result<Value> {
    av::init()?;
    av::log::set_level(av::log::Level::Quiet);
    let mut options = av::Dictionary::new();
    options.set("protocol_whitelist", "file,pipe");
    options.set("format_whitelist", "mov,mpegts,hevc");
    let mut input = av::format::input_with_dictionary(source, options)?;
    let stream = input
        .streams()
        .best(av::media::Type::Video)
        .context("missing thumbnail video stream")?;
    let index = stream.index();
    let tb = stream.time_base();
    let mut context = av::codec::Context::from_parameters(stream.parameters())?;
    let mut threads = av::codec::threading::Config::count(2);
    threads.kind = av::codec::threading::Type::Frame;
    context.set_threading(threads);
    let mut decoder = context.decoder().video()?;
    anyhow::ensure!(
        decoder.width() > 0
            && decoder.width() <= 4096
            && decoder.height() > 0
            && decoder.height() <= 2160,
        "unsupported thumbnail dimensions"
    );
    let mut candidates: Vec<(u8, av::frame::Video)> = vec![];
    let mut first = None;
    let mut start = None;
    let mut receive = |decoder: &mut av::decoder::Video| -> anyhow::Result<bool> {
        let mut frame = av::frame::Video::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            let t = frame
                .pts()
                .map(|p| p as f64 * tb.numerator() as f64 / tb.denominator() as f64)
                .unwrap_or(0.);
            if first.is_none() {
                first = Some(small(&frame)?);
                start = Some(t);
            }
            for second in 1..=5 {
                if !candidates.iter().any(|v| v.0 == second) && t - start.unwrap() >= second as f64
                {
                    candidates.push((second, small(&frame)?));
                }
            }
        }
        Ok(candidates.len() == 5)
    };
    let mut count = 0;
    loop {
        let mut packet = av::Packet::empty();
        if packet.read(&mut input).is_err() {
            if count < 140 && decoder.send_eof().is_ok() {
                receive(&mut decoder)?;
            }
            break;
        }
        if packet.stream() != index {
            continue;
        }
        if count >= 140 {
            break;
        }
        count += 1;
        if decoder.send_packet(&packet).is_ok() && receive(&mut decoder)? {
            break;
        }
    }
    if candidates.is_empty()
        && let Some(first) = first
    {
        candidates.push((0, first));
    }
    let index = candidates
        .iter()
        .enumerate()
        .max_by(|(i, a), (j, b)| {
            score(stats(&a.1))
                .total_cmp(&score(stats(&b.1)))
                .then_with(|| j.cmp(i))
        })
        .map(|v| v.0)
        .context("no decodable thumbnail frame")?;
    let (second, mut image) = candidates.swap_remove(index);
    let measured = stats(&image);
    level(&mut image, measured);
    image.set_pts(Some(0));
    let codec = av::encoder::find(av::codec::Id::MJPEG).context("missing JPEG encoder")?;
    let mut encoder = av::codec::Context::new_with_codec(codec)
        .encoder()
        .video()?;
    encoder.set_width(image.width());
    encoder.set_height(image.height());
    encoder.set_format(av::format::Pixel::YUVJ420P);
    encoder.set_time_base((1, 25));
    let mut options = av::Dictionary::new();
    options.set("qmin", "4");
    options.set("qmax", "4");
    let mut encoder = encoder.open_with(options)?;
    encoder.send_frame(&image)?;
    encoder.send_eof()?;
    let mut jpeg = vec![];
    let mut packet = av::Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        jpeg.extend_from_slice(packet.data().unwrap_or(&[]));
    }
    anyhow::ensure!(jpeg.starts_with(&[0xff, 0xd8]), "JPEG encoding failed");
    let temp = output.with_extension("jpg.tmp");
    std::fs::write(&temp, jpeg)?;
    std::fs::rename(temp, output)?;
    let meta = json!({"version":2,"score":crate::replay::r(score(measured),2)});
    let metadata = output.with_extension("json");
    let temp = metadata.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec(&meta)?)?;
    std::fs::rename(temp, metadata)?;
    Ok(json!({"second":second,"score":meta["score"]}))
}
