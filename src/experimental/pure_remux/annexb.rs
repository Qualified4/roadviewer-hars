use crate::media::Signature;
use anyhow::ensure;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

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

/// Sample framing of FFmpeg's `ff_nal_parse_units`: only the `zero_byte` of a
/// following 4-byte start code is dropped; padding at a buffer end is kept.
/// Hashing always uses `trim`, so this affects stored bytes, never the SHA.
fn framed(bytes: &[u8], begin: usize, at: usize) -> &[u8] {
    let end = if at > begin && bytes[at - 1] == 0 {
        at - 1
    } else {
        at
    };
    &bytes[begin..end]
}

/// Calls `each` for every NAL unit of a complete Annex B buffer.
pub fn for_each_nal(
    bytes: &[u8],
    mut each: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Some(first) = find_start(bytes, 0) else {
        ensure!(bytes.iter().all(|b| *b == 0), "non Annex B prefix");
        return Ok(());
    };
    ensure!(bytes[..first].iter().all(|b| *b == 0), "non Annex B prefix");
    let mut begin = first + 3;
    while let Some(next) = find_start(bytes, begin) {
        each(framed(bytes, begin, next))?;
        begin = next + 3;
    }
    each(&bytes[begin..])
}

/// Bounded streaming splitter for raw Annex B files.
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
        mut each: impl FnMut(&[u8]) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.buffer.extend_from_slice(data);
        let mut consumed = 0;
        while let Some(at) = find_start(&self.buffer, self.searched) {
            match self.begin {
                None => ensure!(
                    self.buffer[..at].iter().all(|b| *b == 0),
                    "non Annex B prefix"
                ),
                Some(begin) => each(framed(&self.buffer, begin, at))?,
            }
            self.begin = Some(at + 3);
            self.searched = at + 3;
            consumed = at + 3;
        }
        if self.begin.is_none() {
            ensure!(self.buffer.iter().all(|b| *b == 0), "non Annex B prefix");
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
    pub fn finish(self, mut each: impl FnMut(&[u8]) -> anyhow::Result<()>) -> anyhow::Result<()> {
        if let Some(begin) = self.begin {
            each(&self.buffer[begin..])?;
        }
        Ok(())
    }
}

pub fn kind(unit: &[u8], hevc: bool) -> anyhow::Result<u8> {
    ensure!(unit.len() >= if hevc { 2 } else { 1 }, "truncated NAL unit");
    ensure!(unit[0] & 0x80 == 0, "forbidden NAL header bit");
    Ok(if hevc {
        (unit[0] >> 1) & 63
    } else {
        unit[0] & 31
    })
}

pub fn is_parameter_set(kind: u8, hevc: bool) -> bool {
    if hevc {
        matches!(kind, 32..=34)
    } else {
        matches!(kind, 7 | 8)
    }
}

/// Same policy as `media::NalHasher`: ordered VCL SHA plus decoder header set.
pub struct Pictures {
    hevc: bool,
    hash: Sha256,
    headers: BTreeSet<(u8, String)>,
    pictures: u64,
}
impl Pictures {
    pub fn new(hevc: bool) -> Self {
        Self {
            hevc,
            hash: Sha256::new(),
            headers: BTreeSet::new(),
            pictures: 0,
        }
    }
    /// Accepts framed units; framing zeros never reach the hash.
    pub fn add(&mut self, unit: &[u8]) -> anyhow::Result<u8> {
        let unit = trim(unit);
        let kind = kind(unit, self.hevc)?;
        if is_parameter_set(kind, self.hevc) {
            self.headers
                .insert((kind, hex::encode(Sha256::digest(unit))));
        }
        if (self.hevc && kind < 32) || (!self.hevc && (1..=5).contains(&kind)) {
            self.hash.update((unit.len() as u32).to_be_bytes());
            self.hash.update(unit);
            self.pictures += 1;
        }
        Ok(kind)
    }
    pub fn finish(self) -> anyhow::Result<Signature> {
        let required: &[u8] = if self.hevc { &[32, 33, 34] } else { &[7, 8] };
        ensure!(
            self.pictures > 0
                && required
                    .iter()
                    .all(|k| self.headers.iter().any(|(kind, _)| kind == k)),
            "missing pictures or decoder configuration"
        );
        Ok(Signature {
            pictures: hex::encode(self.hash.finalize()),
            headers: self.headers,
        })
    }
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
}
