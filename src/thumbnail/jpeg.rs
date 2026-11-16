//! Baseline 4:2:0 JPEG of the thumbnail with the quantisation FFmpeg's `mjpeg` encoder uses at
//! `qmin = qmax = 4` (the MPEG-1 intra matrix times 4/8, DC step 8, one table for all planes)
//! and per-picture optimal Huffman tables, as its default `huffman=optimal`. The DCT is the
//! libjpeg integer one, so the bytes differ from FFmpeg's while the quantisation steps are equal.
use super::scale::{Image, Plane};

const MPEG1_INTRA: [u8; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, 16, 16, 22, 24, 27, 29, 34, 37, 19, 22, 26, 27, 29, 34, 34, 38,
    22, 22, 26, 27, 29, 34, 37, 40, 22, 26, 27, 29, 32, 35, 40, 48, 26, 27, 29, 32, 35, 40, 48, 58,
    26, 27, 29, 34, 38, 46, 56, 69, 27, 29, 35, 38, 46, 56, 69, 83,
];
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
const QSCALE: u32 = 4;

fn table() -> [u16; 64] {
    let mut q = [0u16; 64];
    for (i, v) in q.iter_mut().enumerate() {
        *v = if i == 0 {
            8
        } else {
            ((u32::from(MPEG1_INTRA[i]) * QSCALE) >> 3).clamp(1, 255) as u16
        };
    }
    q
}

/// libjpeg `jpeg_fdct_islow`: output scaled up by 8.
fn fdct(block: &mut [i32; 64]) {
    const BITS: i32 = 13;
    const PASS1: i32 = 2;
    const F0298: i32 = 2446;
    const F0390: i32 = 3196;
    const F0541: i32 = 4433;
    const F0765: i32 = 6270;
    const F0899: i32 = 7373;
    const F1175: i32 = 9633;
    const F1501: i32 = 12299;
    const F1847: i32 = 15137;
    const F1961: i32 = 16069;
    const F2053: i32 = 16819;
    const F2562: i32 = 20995;
    const F3072: i32 = 25172;
    let descale = |x: i32, n: i32| (x + (1 << (n - 1))) >> n;
    for row in 0..8 {
        let d = &mut block[row * 8..row * 8 + 8];
        let t0 = d[0] + d[7];
        let t7 = d[0] - d[7];
        let t1 = d[1] + d[6];
        let t6 = d[1] - d[6];
        let t2 = d[2] + d[5];
        let t5 = d[2] - d[5];
        let t3 = d[3] + d[4];
        let t4 = d[3] - d[4];
        let t10 = t0 + t3;
        let t13 = t0 - t3;
        let t11 = t1 + t2;
        let t12 = t1 - t2;
        d[0] = (t10 + t11) << PASS1;
        d[4] = (t10 - t11) << PASS1;
        let z1 = (t12 + t13) * F0541;
        d[2] = descale(z1 + t13 * F0765, BITS - PASS1);
        d[6] = descale(z1 - t12 * F1847, BITS - PASS1);
        let z1 = t4 + t7;
        let z2 = t5 + t6;
        let z3 = t4 + t6;
        let z4 = t5 + t7;
        let z5 = (z3 + z4) * F1175;
        let (t4, t5, t6, t7) = (t4 * F0298, t5 * F2053, t6 * F3072, t7 * F1501);
        let (z1, z2) = (-z1 * F0899, -z2 * F2562);
        let (z3, z4) = (-z3 * F1961 + z5, -z4 * F0390 + z5);
        d[7] = descale(t4 + z1 + z3, BITS - PASS1);
        d[5] = descale(t5 + z2 + z4, BITS - PASS1);
        d[3] = descale(t6 + z2 + z3, BITS - PASS1);
        d[1] = descale(t7 + z1 + z4, BITS - PASS1);
    }
    for col in 0..8 {
        let g = |i: usize| block[i * 8 + col];
        let t0 = g(0) + g(7);
        let t7 = g(0) - g(7);
        let t1 = g(1) + g(6);
        let t6 = g(1) - g(6);
        let t2 = g(2) + g(5);
        let t5 = g(2) - g(5);
        let t3 = g(3) + g(4);
        let t4 = g(3) - g(4);
        let t10 = t0 + t3;
        let t13 = t0 - t3;
        let t11 = t1 + t2;
        let t12 = t1 - t2;
        let mut out = [0i32; 8];
        out[0] = descale(t10 + t11, PASS1);
        out[4] = descale(t10 - t11, PASS1);
        let z1 = (t12 + t13) * F0541;
        out[2] = descale(z1 + t13 * F0765, BITS + PASS1);
        out[6] = descale(z1 - t12 * F1847, BITS + PASS1);
        let z1 = t4 + t7;
        let z2 = t5 + t6;
        let z3 = t4 + t6;
        let z4 = t5 + t7;
        let z5 = (z3 + z4) * F1175;
        let (t4, t5, t6, t7) = (t4 * F0298, t5 * F2053, t6 * F3072, t7 * F1501);
        let (z1, z2) = (-z1 * F0899, -z2 * F2562);
        let (z3, z4) = (-z3 * F1961 + z5, -z4 * F0390 + z5);
        out[7] = descale(t4 + z1 + z3, BITS + PASS1);
        out[5] = descale(t5 + z2 + z4, BITS + PASS1);
        out[3] = descale(t6 + z2 + z3, BITS + PASS1);
        out[1] = descale(t7 + z1 + z4, BITS + PASS1);
        for (i, v) in out.into_iter().enumerate() {
            block[i * 8 + col] = v;
        }
    }
}

/// Quantised coefficients of the 8x8 block at (`bx`, `by`), edges repeated past the plane.
fn block(plane: &Plane, bx: usize, by: usize, q: &[u16; 64]) -> [i32; 64] {
    let mut b = [0i32; 64];
    for y in 0..8 {
        let row = (by * 8 + y).min(plane.height - 1) * plane.width;
        for x in 0..8 {
            let col = (bx * 8 + x).min(plane.width - 1);
            b[y * 8 + x] = i32::from(plane.data[row + col]) - 128;
        }
    }
    fdct(&mut b);
    for (v, &step) in b.iter_mut().zip(q) {
        let step = i32::from(step) * 8;
        let magnitude = (v.abs() + step / 2) / step;
        *v = if *v < 0 { -magnitude } else { magnitude };
    }
    b
}

fn category(v: i32) -> u8 {
    (32 - v.unsigned_abs().leading_zeros()) as u8
}

/// One block's symbols: (table: 0 DC / 1 AC, symbol, extra bits, extra bit count).
fn symbols(b: &[i32; 64], previous_dc: &mut i32, out: &mut Vec<(u8, u8, u16, u8)>) {
    let diff = b[0] - *previous_dc;
    *previous_dc = b[0];
    let size = category(diff);
    let bits = if diff < 0 { diff - 1 } else { diff } as u16 & ((1u32 << size) - 1) as u16;
    out.push((0, size, bits, size));
    let mut run = 0u8;
    for &index in &ZIGZAG[1..] {
        let v = b[index];
        if v == 0 {
            run += 1;
            continue;
        }
        while run > 15 {
            out.push((1, 0xf0, 0, 0));
            run -= 16;
        }
        let size = category(v);
        let bits = if v < 0 { v - 1 } else { v } as u16 & ((1u32 << size) - 1) as u16;
        out.push((1, (run << 4) | size, bits, size));
        run = 0;
    }
    if run > 0 {
        out.push((1, 0, 0, 0));
    }
}

/// libjpeg `jpeg_gen_optimal_table`: (BITS counts 1..=16, symbols by length).
fn optimal(freq: &[u32; 256]) -> ([u8; 16], Vec<u8>) {
    let mut freq: Vec<i64> = freq.iter().map(|&f| i64::from(f)).collect();
    freq.push(1);
    let mut codesize = [0usize; 257];
    let mut others = [-1i64; 257];
    loop {
        let mut c1 = -1i64;
        let mut v = i64::MAX;
        for (i, &f) in freq.iter().enumerate() {
            if f > 0 && f <= v {
                v = f;
                c1 = i as i64;
            }
        }
        let mut c2 = -1i64;
        let mut v = i64::MAX;
        for (i, &f) in freq.iter().enumerate() {
            if f > 0 && f <= v && i as i64 != c1 {
                v = f;
                c2 = i as i64;
            }
        }
        if c2 < 0 {
            break;
        }
        let (mut a, b) = (c1 as usize, c2 as usize);
        freq[a] += freq[b];
        freq[b] = 0;
        codesize[a] += 1;
        while others[a] >= 0 {
            a = others[a] as usize;
            codesize[a] += 1;
        }
        others[a] = b as i64;
        let mut b = b;
        codesize[b] += 1;
        while others[b] >= 0 {
            b = others[b] as usize;
            codesize[b] += 1;
        }
    }
    let mut bits = [0usize; 33];
    for &size in &codesize {
        if size > 0 {
            bits[size.min(32)] += 1;
        }
    }
    for i in (17..=32).rev() {
        while bits[i] > 0 {
            let mut j = i - 2;
            while bits[j] == 0 {
                j -= 1;
            }
            bits[i] -= 2;
            bits[i - 1] += 1;
            bits[j + 1] += 2;
            bits[j] -= 1;
        }
    }
    let mut i = 16;
    while bits[i] == 0 {
        i -= 1;
    }
    bits[i] -= 1;
    let mut counts = [0u8; 16];
    for (k, c) in counts.iter_mut().enumerate() {
        *c = bits[k + 1] as u8;
    }
    let mut values = Vec::new();
    for size in 1..=32 {
        for (symbol, &s) in codesize.iter().enumerate().take(256) {
            if s == size {
                values.push(symbol as u8);
            }
        }
    }
    (counts, values)
}

/// Canonical codes (code, length) per symbol from a BITS / HUFFVAL pair.
fn codes(counts: &[u8; 16], values: &[u8]) -> [(u16, u8); 256] {
    let mut out = [(0u16, 0u8); 256];
    let mut code = 0u16;
    let mut k = 0;
    for (len, &n) in counts.iter().enumerate() {
        for _ in 0..n {
            out[usize::from(values[k])] = (code, len as u8 + 1);
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    out
}

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}
impl BitWriter {
    fn put(&mut self, value: u16, len: u8) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | u32::from((value >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                let byte = self.acc as u8;
                self.out.push(byte);
                if byte == 0xff {
                    self.out.push(0);
                }
                self.acc = 0;
                self.n = 0;
            }
        }
    }
    fn finish(mut self) -> Vec<u8> {
        while self.n != 0 {
            self.put(1, 1);
        }
        self.out
    }
}

fn segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
    out.extend_from_slice(&[0xff, marker]);
    out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(body);
}

/// The JPEG of a 4:2:0 full-range `image`.
pub fn encode(image: &Image) -> anyhow::Result<Vec<u8>> {
    let (w, h) = (image.y.width, image.y.height);
    anyhow::ensure!(
        w > 0 && h > 0 && w <= 65535 && h <= 65535 && image.u.width > 0 && image.u.height > 0,
        "JPEG encoding failed"
    );
    let q = table();
    let (mx, my) = (w.div_ceil(16), h.div_ceil(16));
    let mut syms = Vec::new();
    let mut dc = [0i32; 3];
    for by in 0..my {
        for bx in 0..mx {
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                symbols(
                    &block(&image.y, bx * 2 + dx, by * 2 + dy, &q),
                    &mut dc[0],
                    &mut syms,
                );
            }
            symbols(&block(&image.u, bx, by, &q), &mut dc[1], &mut syms);
            symbols(&block(&image.v, bx, by, &q), &mut dc[2], &mut syms);
        }
    }
    // Symbols alternate luma (4 blocks) and chroma (2 blocks) per MCU; each class has its tables.
    let mut freq = [[0u32; 256]; 4];
    let mut class = Vec::with_capacity(syms.len());
    let mut block_index = 0usize;
    for &(t, s, _, _) in &syms {
        if t == 0 {
            block_index += 1;
        }
        let chroma = (block_index - 1) % 6 >= 4;
        let k = usize::from(t) + if chroma { 2 } else { 0 };
        freq[k][usize::from(s)] += 1;
        class.push(k);
    }
    let tables = freq.map(|f| optimal(&f));
    let lookup = tables.clone().map(|(c, v)| codes(&c, &v));

    let mut out = vec![0xff, 0xd8];
    segment(&mut out, 0xe0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0");
    let mut dqt = vec![0u8];
    dqt.extend(ZIGZAG.iter().map(|&i| q[i] as u8));
    segment(&mut out, 0xdb, &dqt);
    let mut sof = vec![8];
    sof.extend_from_slice(&(h as u16).to_be_bytes());
    sof.extend_from_slice(&(w as u16).to_be_bytes());
    sof.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 0, 3, 0x11, 0]);
    segment(&mut out, 0xc0, &sof);
    let mut dht = Vec::new();
    for (k, id) in [(0usize, 0x00u8), (1, 0x10), (2, 0x01), (3, 0x11)] {
        dht.push(id);
        dht.extend_from_slice(&tables[k].0);
        dht.extend_from_slice(&tables[k].1);
    }
    segment(&mut out, 0xc4, &dht);
    segment(&mut out, 0xda, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    let mut bits = BitWriter {
        out: Vec::new(),
        acc: 0,
        n: 0,
    };
    for (&(_, s, extra, n), &k) in syms.iter().zip(&class) {
        let (code, len) = lookup[k][usize::from(s)];
        anyhow::ensure!(len > 0, "JPEG encoding failed");
        bits.put(code, len);
        bits.put(extra, n);
    }
    out.extend(bits.finish());
    out.extend_from_slice(&[0xff, 0xd9]);
    Ok(out)
}
