use anyhow::ensure;

pub const NAL_LIMIT: usize = 32 * 1024 * 1024;

/// Index of the first zero of the next `00 00 01`, at or after `from`.
pub fn find_start(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from.max(2);
    while i < bytes.len() {
        let j = i + bytes[i..].iter().position(|&b| b == 1)?;
        if bytes[j - 1] == 0 && bytes[j - 2] == 0 {
            return Some(j - 2);
        }
        i = j + 1;
    }
    None
}

/// Trailing zeros are Annex B framing (`trailing_zero_8bits`, 4-byte start
/// codes); a NAL unit itself never ends in a zero byte.
pub fn trim(mut unit: &[u8]) -> &[u8] {
    while let [rest @ .., 0] = unit {
        unit = rest;
    }
    unit
}

/// A unit as FFmpeg's `ff_nal_find_startcode` bounds it inside a buffer: the
/// `zero_byte` of a following 4-byte start code is dropped, padding is kept.
pub fn framed(raw: &[u8]) -> &[u8] {
    match raw {
        [rest @ .., 0] => rest,
        _ => raw,
    }
}

/// A unit of an Annex B buffer: `raw` ends where the next `00 00 01` begins
/// (`ff_h2645_extract_rbsp`'s `raw_size`), or at the buffer end when `last`.
pub struct Unit<'a> {
    pub raw: &'a [u8],
    pub last: bool,
}
impl Unit<'_> {
    /// Sample bytes: `ff_nal_parse_units` framing within one packet.
    pub fn sample(&self) -> &[u8] {
        if self.last {
            self.raw
        } else {
            framed(self.raw)
        }
    }
}

/// Calls `each` for every NAL unit of a complete Annex B buffer.
pub fn for_each_nal(
    bytes: &[u8],
    mut each: impl FnMut(Unit<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Some(first) = find_start(bytes, 0) else {
        ensure!(bytes.iter().all(|b| *b == 0), "non Annex B prefix");
        return Ok(());
    };
    ensure!(bytes[..first].iter().all(|b| *b == 0), "non Annex B prefix");
    let mut begin = first + 3;
    while let Some(next) = find_start(bytes, begin) {
        each(Unit {
            raw: &bytes[begin..next],
            last: false,
        })?;
        begin = next + 3;
    }
    each(Unit {
        raw: &bytes[begin..],
        last: true,
    })
}

/// Bounded streaming splitter for raw Annex B files. Bytes before the first
/// start code hold no NAL unit; they are skipped as libavformat's raw HEVC
/// demuxer and MOV muxer skip them (`media::leading_bytes` reports them).
pub struct Scanner {
    buffer: Vec<u8>,
    begin: Option<usize>,
    searched: usize,
}
impl Scanner {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            begin: None,
            searched: 0,
        }
    }
    pub fn push(
        &mut self,
        data: &[u8],
        mut each: impl FnMut(Unit<'_>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.buffer.extend_from_slice(data);
        let mut consumed = 0;
        while let Some(at) = find_start(&self.buffer, self.searched) {
            if let Some(begin) = self.begin {
                each(Unit {
                    raw: &self.buffer[begin..at],
                    last: false,
                })?;
            }
            self.begin = Some(at + 3);
            self.searched = at + 3;
            consumed = at + 3;
        }
        self.buffer.drain(..consumed);
        self.begin = self.begin.map(|b| b - consumed);
        self.searched = self
            .buffer
            .len()
            .saturating_sub(2)
            .max(self.begin.unwrap_or(0));
        ensure!(
            self.buffer.len() <= NAL_LIMIT,
            "NAL unit exceeds bounded memory limit"
        );
        Ok(())
    }
    pub fn finish(
        self,
        mut each: impl FnMut(Unit<'_>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if let Some(begin) = self.begin {
            each(Unit {
                raw: &self.buffer[begin..],
                last: true,
            })?;
        }
        Ok(())
    }
}

pub fn kind(unit: &[u8], hevc: bool) -> anyhow::Result<u8> {
    let unit = trim(unit);
    ensure!(unit.len() >= if hevc { 2 } else { 1 }, "truncated NAL unit");
    let kind = if hevc {
        (unit[0] >> 1) & 63
    } else {
        unit[0] & 31
    };
    // A flagged parameter set leaves no picture decodable (libavformat, and so
    // Python, refuse it too); any other flagged unit is copied byte for byte as
    // libavformat's packet copy does, so the file keeps every input picture.
    ensure!(
        unit[0] & 0x80 == 0 || !is_parameter_set(kind, hevc),
        "forbidden NAL header bit"
    );
    Ok(kind)
}

pub fn is_parameter_set(kind: u8, hevc: bool) -> bool {
    if hevc {
        matches!(kind, 32..=34)
    } else {
        matches!(kind, 7 | 8)
    }
}

/// Decoder configuration bytes as libavformat derives them: the
/// `extract_extradata` filter joins the first packet's raw parameter sets with
/// 3-byte start codes, and the hvcC/avcC writer re-splits that buffer, so every
/// set but the last loses one trailing zero and the last keeps its padding.
pub fn configuration_sets(raw_in_order: &[(u8, &[u8])]) -> Vec<(u8, Vec<u8>)> {
    let last = raw_in_order.len().saturating_sub(1);
    raw_in_order
        .iter()
        .enumerate()
        .map(|(i, (kind, raw))| (*kind, if i == last { *raw } else { framed(raw) }.to_vec()))
        .collect()
}

/// RBSP reader for the few SPS fields the MP4 helpers do not expose.
pub struct Bits {
    rbsp: Vec<u8>,
    at: usize,
}
impl Bits {
    pub fn new(ebsp: &[u8]) -> Self {
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
    pub fn bit(&mut self) -> anyhow::Result<u32> {
        let byte = *self
            .rbsp
            .get(self.at / 8)
            .ok_or_else(|| anyhow::anyhow!("truncated parameter set"))?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Ok(bit.into())
    }
    pub fn bits(&mut self, n: u32) -> anyhow::Result<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }
    pub fn skip(&mut self, n: usize) -> anyhow::Result<()> {
        ensure!(
            self.at + n <= self.rbsp.len() * 8,
            "truncated parameter set"
        );
        self.at += n;
        Ok(())
    }
    pub fn ue(&mut self) -> anyhow::Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            ensure!(zeros < 32, "invalid Exp-Golomb code");
        }
        Ok((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    pub fn se(&mut self) -> anyhow::Result<i64> {
        let v = i64::from(self.ue()?);
        Ok(if v % 2 == 1 { (v + 1) / 2 } else { -v / 2 })
    }
}

/// Parameter sets seen before the first picture, which become the decoder
/// configuration. Bounded: a stream of sets alone never reaches a picture.
#[derive(Default)]
pub struct Sets {
    raw: Vec<(u8, Vec<u8>)>,
    bytes: usize,
}
pub const SETS_LIMIT: usize = 64;
pub const SETS_BYTES: usize = 1024 * 1024;
impl Sets {
    pub fn add(&mut self, kind: u8, raw: &[u8]) -> anyhow::Result<()> {
        self.bytes += raw.len();
        ensure!(
            self.raw.len() < SETS_LIMIT && self.bytes <= SETS_BYTES,
            "too many parameter sets before the first picture"
        );
        self.raw.push((kind, raw.to_vec()));
        Ok(())
    }
    pub fn contains(&self, kind: u8, raw: &[u8]) -> bool {
        let raw = trim(raw);
        self.raw.iter().any(|(k, v)| *k == kind && trim(v) == raw)
    }
    pub fn configuration(&self) -> Vec<(u8, Vec<u8>)> {
        configuration_sets(
            &self
                .raw
                .iter()
                .map(|(k, v)| (*k, v.as_slice()))
                .collect::<Vec<_>>(),
        )
    }
}
