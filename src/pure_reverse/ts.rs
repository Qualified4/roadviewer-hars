//! MPEG-TS writer for one H.264 stream, laid out as libavformat's `mpegts` muxer lays out the
//! camera originals (one program, PMT PID 0x1000, video PID 0x100 carrying the PCR, PSI repeated on
//! the PCR clock, PES without length, a PCR at every PES as for a stream of unknown frame rate).
//! An original exported that way comes back byte for byte.
use std::io::Write;

const PACKET: usize = 188;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x100;
const SDT_PID: u16 = 0x11;
/// Repetition periods in 27 MHz PCR ticks: PAT/PMT 100 ms, SDT 500 ms.
const PAT_PERIOD: u64 = 2_700_000;
const SDT_PERIOD: u64 = 13_500_000;
/// Largest 33-bit PES clock.
pub const CLOCK_LIMIT: u64 = (1 << 33) - 1;

/// CRC-32/MPEG-2 of PSI sections.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// A long-form section: table header, `body`, CRC.
fn section(table: u8, id: u16, body: &[u8]) -> Vec<u8> {
    let flags: u16 = if table == 0x42 { 0xf000 } else { 0xb000 };
    let length = u16::try_from(body.len() + 9).unwrap_or(u16::MAX) & 0x0fff;
    let mut out = vec![table];
    out.extend_from_slice(&(flags | length).to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0xc1, 0, 0]);
    out.extend_from_slice(body);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn pat() -> Vec<u8> {
    section(
        0x00,
        1,
        &[0x00, 0x01, 0xe0 | (PMT_PID >> 8) as u8, PMT_PID as u8],
    )
}

fn pmt() -> Vec<u8> {
    let pid = [0xe0 | (VIDEO_PID >> 8) as u8, VIDEO_PID as u8];
    section(
        0x02,
        1,
        &[pid[0], pid[1], 0xf0, 0x00, 0x1b, pid[0], pid[1], 0xf0, 0x00],
    )
}

/// The service description the originals carry (libavformat's default names).
fn sdt() -> Vec<u8> {
    let mut descriptor = vec![0x48, 0, 0x01];
    for name in [&b"FFmpeg"[..], b"Service01"] {
        descriptor.push(name.len() as u8);
        descriptor.extend_from_slice(name);
    }
    descriptor[1] = (descriptor.len() - 2) as u8;
    let mut body = vec![0xff, 0x01, 0xff, 0x00, 0x01, 0xfc];
    body.extend_from_slice(&(0x8000u16 | descriptor.len() as u16).to_be_bytes());
    body.extend_from_slice(&descriptor);
    section(0x42, 1, &body)
}

fn clock(prefix: u8, value: u64) -> [u8; 5] {
    [
        (prefix << 4) | (((value >> 30) & 7) as u8) << 1 | 1,
        (value >> 22) as u8,
        (((value >> 15) & 0x7f) as u8) << 1 | 1,
        (value >> 7) as u8,
        ((value & 0x7f) as u8) << 1 | 1,
    ]
}

pub struct Muxer<W: Write> {
    out: W,
    tables: [Vec<u8>; 3],
    /// Continuity counters of SDT, PAT, PMT and video.
    counters: [u8; 4],
    last_sdt: Option<u64>,
    last_pat: Option<u64>,
    previous_key: bool,
    pub written: u64,
}

impl<W: Write> Muxer<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            tables: [sdt(), pat(), pmt()],
            counters: [15; 4],
            last_sdt: None,
            last_pat: None,
            previous_key: false,
            written: 0,
        }
    }

    fn emit(&mut self, packet: &[u8; PACKET]) -> std::io::Result<()> {
        self.written += PACKET as u64;
        self.out.write_all(packet)
    }

    fn table(&mut self, index: usize, pid: u16) -> std::io::Result<()> {
        self.counters[index] = (self.counters[index] + 1) & 15;
        let mut packet = [0xffu8; PACKET];
        packet[..5].copy_from_slice(&[
            0x47,
            0x40 | (pid >> 8) as u8,
            pid as u8,
            0x10 | self.counters[index],
            0,
        ]);
        let body = &self.tables[index];
        packet[5..5 + body.len()].copy_from_slice(body);
        self.emit(&packet)
    }

    /// One access unit, already in Annex B. Clocks are 90 kHz and at most [`CLOCK_LIMIT`].
    pub fn write(&mut self, pts: u64, dts: u64, key: bool, es: &[u8]) -> std::io::Result<()> {
        let mut aud;
        let es = if starts_with_aud(es) {
            es
        } else {
            aud = Vec::with_capacity(es.len() + 6);
            aud.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xf0]);
            aud.extend_from_slice(es);
            &aud[..]
        };
        let pcr = dts * 300;
        if self
            .last_sdt
            .is_none_or(|last| pcr.saturating_sub(last) >= SDT_PERIOD)
        {
            self.last_sdt = Some(self.last_sdt.map_or(pcr, |last| last.max(pcr)));
            self.table(0, SDT_PID)?;
        }
        if self
            .last_pat
            .is_none_or(|last| pcr.saturating_sub(last) >= PAT_PERIOD)
            || (key && !self.previous_key)
        {
            self.last_pat = Some(self.last_pat.map_or(pcr, |last| last.max(pcr)));
            self.table(1, 0)?;
            self.table(2, PMT_PID)?;
        }
        self.previous_key = key;

        let mut header = vec![0, 0, 1, 0xe0, 0, 0, 0x80];
        if pts == dts {
            header.extend_from_slice(&[0x80, 5]);
            header.extend_from_slice(&clock(2, pts));
        } else {
            header.extend_from_slice(&[0xc0, 10]);
            header.extend_from_slice(&clock(3, pts));
            header.extend_from_slice(&clock(1, dts));
        }
        let mut rest = es;
        let mut first = true;
        while first || !rest.is_empty() {
            self.counters[3] = (self.counters[3] + 1) & 15;
            let mut packet = [0xffu8; PACKET];
            packet[0] = 0x47;
            packet[1] = if first { 0x40 } else { 0 } | (VIDEO_PID >> 8) as u8;
            packet[2] = VIDEO_PID as u8;
            packet[3] = 0x10 | self.counters[3];
            let mut adaptation = Vec::new();
            if first {
                let base = pcr / 300;
                let extension = pcr % 300;
                adaptation.push(0x10 | if key { 0x40 } else { 0 });
                adaptation.extend_from_slice(&[
                    (base >> 25) as u8,
                    (base >> 17) as u8,
                    (base >> 9) as u8,
                    (base >> 1) as u8,
                    ((base << 7) as u8) | ((extension >> 8) as u8) | 0x7e,
                    extension as u8,
                ]);
            }
            let leading = if first { header.len() } else { 0 };
            let used = 4 + if adaptation.is_empty() {
                0
            } else {
                1 + adaptation.len()
            };
            let room = PACKET - used - leading;
            let take = room.min(rest.len());
            let stuffing = room - take;
            let mut at = 4;
            if !adaptation.is_empty() || stuffing > 0 {
                packet[3] |= 0x20;
                let length = if adaptation.is_empty() {
                    stuffing - 1
                } else {
                    adaptation.len() + stuffing
                };
                packet[4] = length as u8;
                at = 5;
                if adaptation.is_empty() {
                    if stuffing >= 2 {
                        packet[5] = 0;
                    }
                } else {
                    packet[5..5 + adaptation.len()].copy_from_slice(&adaptation);
                }
                at += length;
            }
            packet[at..at + leading].copy_from_slice(&header[..leading]);
            at += leading;
            packet[at..at + take].copy_from_slice(&rest[..take]);
            rest = &rest[take..];
            first = false;
            self.emit(&packet)?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Whether the first NAL unit that is an AUD or a slice is the AUD; the muxer adds one otherwise.
fn starts_with_aud(es: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            match es[i + 3] & 31 {
                9 => return true,
                1 | 5 => return false,
                _ => {}
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_carry_the_crc_the_originals_carry() {
        assert_eq!(hex::encode(pat()), "00b00d0001c100000001f0002ab104b2");
        assert_eq!(
            hex::encode(pmt()),
            "02b0120001c10000e100f0001be100f00015bd4d56"
        );
        assert!(hex::encode(sdt()).starts_with("42f0250001c10000ff01ff0001fc8014481201064646"));
    }

    #[test]
    fn every_packet_is_188_bytes_with_counters_and_the_whole_payload() {
        let mut muxer = Muxer::new(Vec::new());
        let es = [vec![0, 0, 0, 1, 9, 0xf0], vec![7u8; 500]].concat();
        muxer.write(9000, 9000, true, &es).unwrap();
        muxer.write(13500, 13500, false, &es[..6]).unwrap();
        let out = muxer.finish().unwrap();
        assert_eq!(out.len() % 188, 0);
        assert!(out.chunks(188).all(|p| p[0] == 0x47));
        let video = out
            .chunks(188)
            .filter(|p| ((u16::from(p[1]) & 31) << 8 | u16::from(p[2])) == VIDEO_PID)
            .collect::<Vec<_>>();
        assert_eq!(
            video.iter().map(|p| p[3] & 15).collect::<Vec<_>>(),
            (0..video.len() as u8).collect::<Vec<_>>()
        );
    }
}
