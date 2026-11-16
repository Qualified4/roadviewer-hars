//! The thumbnail's reduction to 320 pixels wide, as FFmpeg 6.1 libswscale does it for
//! `sws_getContext(yuv420p or yuvj420p -> yuvj420p, SWS_BILINEAR)` on x86-64: the filter
//! construction of `initFilter`, the 8 to 15 bit horizontal pass with the limited to full range
//! step, and the vertical pass of the x86 `yuv2yuvX` kernel, whose rounding differs from the C
//! one for every output row except the last two (where swscale itself falls back to C). Integer
//! arithmetic throughout, so the result does not depend on the machine it runs on.

/// One 8-bit plane, rows packed without padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// A 4:2:0 picture: luma and the two half-size chroma planes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub y: Plane,
    pub u: Plane,
    pub v: Plane,
}

/// libswscale's `SWS_MAX_REDUCE_CUTOFF`.
const REDUCE_CUTOFF: f64 = 0.002;
/// libswscale's `MAX_FILTER_SIZE`; a longer filter needs its cascaded scaler.
const MAX_FILTER_SIZE: usize = 256;
/// Pixel centres (`get_local_pos` for both the luma and the default chroma siting).
const POSITION: i64 = 128;
/// `sws_pb_64`: the dither of an 8-bit source.
const DITHER: i32 = 64;

struct Filter {
    size: usize,
    pos: Vec<i64>,
    coeff: Vec<i16>,
}

fn log2(v: i64) -> u32 {
    if v <= 0 { 0 } else { 63 - v.leading_zeros() }
}

fn rounded_div(a: i64, b: i64) -> i64 {
    if a >= 0 {
        (a + (b >> 1)) / b
    } else {
        (a - (b >> 1)) / b
    }
}

/// `initFilter` for `SWS_BILINEAR` with no source or destination filter vectors.
fn filter(inc: i64, src: usize, dst: usize, align: usize, one: i64) -> anyhow::Result<Filter> {
    anyhow::ensure!(src > 0 && dst > 0, "empty thumbnail picture");
    let (src_i, dst_i) = (src as i64, dst as i64);
    let fone: i64 = 1 << (54 - log2(src_i / dst_i).min(8));
    let (size, mut pos, mut taps) = if (inc - 0x10000).abs() < 10 {
        (1, (0..dst_i).collect::<Vec<_>>(), vec![fone; dst])
    } else {
        let size = if inc <= 1 << 16 {
            3
        } else {
            1 + (2 * src_i + dst_i - 1) / dst_i
        };
        let size = size.min(src_i - 2).max(1) as usize;
        let mut pos = Vec::with_capacity(dst);
        let mut taps = vec![0i64; dst * size];
        let mut at = ((POSITION * inc) >> 7) - ((POSITION * 0x10000) >> 7);
        for i in 0..dst {
            let first = (at - (size as i64 - 2) * (1 << 16)) / (1 << 17);
            pos.push(first);
            for (xx, tap) in (first..).zip(&mut taps[i * size..(i + 1) * size]) {
                let mut d = (xx * (1 << 17) - at).abs() << 13;
                if inc > 1 << 16 {
                    d = d * dst_i / src_i;
                }
                *tap = ((1i64 << 30) - d).max(0) * (fone >> 30);
            }
            at += 2 * inc;
        }
        (size, pos, taps)
    };

    let cutoff = REDUCE_CUTOFF * fone as f64;
    let mut min_size = 0;
    for i in (0..dst).rev() {
        let row = &mut taps[i * size..(i + 1) * size];
        let mut sum = 0i64;
        for _ in 0..size {
            sum += row[0].abs();
            if sum as f64 > cutoff || (i + 1 < dst && pos[i] >= pos[i + 1]) {
                break;
            }
            row.rotate_left(1);
            row[size - 1] = 0;
            pos[i] += 1;
        }
        let mut sum = 0i64;
        let mut min = size;
        for j in (1..size).rev() {
            sum += row[j].abs();
            if sum as f64 > cutoff {
                break;
            }
            min -= 1;
        }
        min_size = min_size.max(min);
    }
    // x86 swscale's special case for an unscaled vertical filter.
    let align = if min_size == 1 && align == 2 {
        1
    } else {
        align
    };
    let out_size = (min_size + align - 1) & !(align - 1);
    anyhow::ensure!(
        out_size > 0 && out_size < MAX_FILTER_SIZE,
        "unsupported thumbnail scale"
    );
    let mut reduced = vec![0i64; dst * out_size];
    for i in 0..dst {
        for j in 0..out_size.min(size) {
            reduced[i * out_size + j] = taps[i * size + j];
        }
    }

    let src_len = src_i;
    for i in 0..dst {
        let row = &mut reduced[i * out_size..(i + 1) * out_size];
        if pos[i] < 0 {
            for j in 1..out_size {
                let left = (j as i64 + pos[i]).max(0) as usize;
                row[left] += row[j];
                row[j] = 0;
            }
            pos[i] = 0;
        }
        if pos[i] + out_size as i64 > src_len {
            let shift = pos[i] + (out_size as i64 - src_len).min(0);
            let mut acc = 0;
            for j in (0..out_size).rev() {
                if pos[i] + j as i64 >= src_len {
                    acc += row[j];
                    row[j] = 0;
                }
            }
            for j in (0..out_size).rev() {
                row[j] = if (j as i64) < shift {
                    0
                } else {
                    row[(j as i64 - shift) as usize]
                };
            }
            pos[i] -= shift;
            let last = (src_len - 1 - pos[i]) as usize;
            anyhow::ensure!(last < out_size, "unsupported thumbnail scale");
            row[last] += acc;
        }
        anyhow::ensure!(
            pos[i] >= 0 && pos[i] < src_len,
            "unsupported thumbnail scale"
        );
    }

    let mut coeff = Vec::with_capacity(dst * out_size);
    for i in 0..dst {
        let row = &reduced[i * out_size..(i + 1) * out_size];
        let sum = (row.iter().sum::<i64>() + one / 2) / one;
        let sum = if sum == 0 { 1 } else { sum };
        let mut error = 0;
        for &tap in row {
            let v = tap + error;
            let int = rounded_div(v, sum);
            coeff.push(
                i16::try_from(int).map_err(|_| anyhow::anyhow!("unsupported thumbnail scale"))?,
            );
            error = v - int * sum;
        }
    }
    Ok(Filter {
        size: out_size,
        pos,
        coeff,
    })
}

fn increment(src: usize, dst: usize) -> i64 {
    (((src as i64) << 16) + (dst as i64 >> 1)) / dst as i64
}

/// `hScale8To15_c`, then `lumRangeToJpeg_c` / `chrRangeToJpeg_c` when the source is limited range.
fn horizontal(row: &[u8], f: &Filter, out: &mut [i16], convert: Option<bool>) {
    for (i, value) in out.iter_mut().enumerate() {
        let start = f.pos[i] as usize;
        let mut acc = 0i32;
        for (j, &c) in f.coeff[i * f.size..(i + 1) * f.size].iter().enumerate() {
            if c != 0 {
                acc += i32::from(row.get(start + j).copied().unwrap_or(0)) * i32::from(c);
            }
        }
        let v = (acc >> 7).min((1 << 15) - 1);
        *value = match convert {
            None => v,
            Some(true) => (v.min(30189) * 19077 - 39_057_361) >> 14,
            Some(false) => (v.min(30775) * 4663 - 9_289_992) >> 12,
        } as i16;
    }
}

/// One output row from the horizontally scaled rows `rows` (`None` past the source's end, whose
/// weight is always zero). `simd`: the x86 `yuv2yuvX` rounding, else `yuv2planeX_8_c`.
fn vertical(rows: &[Option<&[i16]>], taps: &[i16], out: &mut [u8], simd: bool) {
    for (i, value) in out.iter_mut().enumerate() {
        let sample = |j: usize| rows[j].map_or(0, |r| i32::from(r[i]));
        *value = if simd {
            let mut acc = ((DITHER + 8 * (taps.len() as i32 - 1)) >> 4) as i16;
            for (j, &c) in taps.iter().enumerate() {
                acc = acc.wrapping_add(((sample(j) * i32::from(c)) >> 16) as i16);
            }
            (acc >> 3).clamp(0, 255) as u8
        } else {
            let mut acc = DITHER << 12;
            for (j, &c) in taps.iter().enumerate() {
                acc += sample(j) * i32::from(c);
            }
            (acc >> 19).clamp(0, 255) as u8
        };
    }
}

fn plane(
    src: &Plane,
    width: usize,
    height: usize,
    (out_rows, step): (usize, usize),
    convert: Option<bool>,
) -> anyhow::Result<Plane> {
    let h = filter(increment(src.width, width), src.width, width, 4, 1 << 14)?;
    let v = filter(
        increment(src.height, height),
        src.height,
        height,
        2,
        1 << 12,
    )?;
    let mut scaled = vec![0i16; src.height * width];
    for (row, out) in src
        .data
        .chunks_exact(src.width)
        .zip(scaled.chunks_exact_mut(width))
    {
        horizontal(row, &h, out, convert);
    }
    let mut data = vec![0u8; width * height];
    let mut rows = Vec::with_capacity(v.size);
    for (y, out) in data.chunks_exact_mut(width).enumerate() {
        rows.clear();
        for j in 0..v.size {
            let line = v.pos[y] as usize + j;
            rows.push((line < src.height).then(|| &scaled[line * width..(line + 1) * width]));
        }
        if v.size == 1 {
            let row = rows[0].unwrap_or(&[]);
            for (i, value) in out.iter_mut().enumerate() {
                let s = row.get(i).copied().map_or(0, i32::from);
                *value = ((s + DITHER) >> 7).clamp(0, 255) as u8;
            }
            continue;
        }
        // swscale switches to the C output functions for the last two luma rows; a chroma row
        // is written along with the luma row `step` times its index.
        let simd = y * step + 2 < out_rows;
        vertical(&rows, &v.coeff[y * v.size..(y + 1) * v.size], out, simd);
    }
    Ok(Plane {
        width,
        height,
        data,
    })
}

/// The 4:2:0 `image` reduced to `width` (even) by the thumbnail's height rule, in full range.
/// `full_range`: the decoder marked the source full range (FFmpeg's `yuvj420p`), so no range step.
pub fn small(image: &Image, width: usize, full_range: bool) -> anyhow::Result<Image> {
    let (w, h) = (image.y.width, image.y.height);
    anyhow::ensure!(
        w >= 2 && h >= 2 && image.u.width == w.div_ceil(2) && image.u.height == h.div_ceil(2),
        "unsupported thumbnail picture"
    );
    anyhow::ensure!(
        image.v.width == image.u.width && image.v.height == image.u.height,
        "unsupported thumbnail picture"
    );
    let height =
        (((h as f64 * width as f64 / w as f64 / 2.).round_ties_even() as usize) * 2).max(2);
    if w == width && h == height && full_range {
        return Ok(image.clone());
    }
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    Ok(Image {
        y: plane(
            &image.y,
            width,
            height,
            (height, 1),
            (!full_range).then_some(true),
        )?,
        u: plane(
            &image.u,
            cw,
            ch,
            (height, 2),
            (!full_range).then_some(false),
        )?,
        v: plane(
            &image.v,
            cw,
            ch,
            (height, 2),
            (!full_range).then_some(false),
        )?,
    })
}
