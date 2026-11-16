//! MP4 samples (4-byte NAL lengths) back to Annex B with the choices libavcodec's
//! `h264_mp4toannexb`/`hevc_mp4toannexb` make: where the decoder configuration goes back in band
//! and which start code each unit gets. Python's export and the FFmpeg engine both pass through
//! those filters, so the same choices keep an export equal to theirs byte for byte.
use anyhow::ensure;

const FOUR: [u8; 4] = [0, 0, 0, 1];

fn units(sample: &[u8]) -> impl Iterator<Item = anyhow::Result<&[u8]>> {
    let mut rest = sample;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        Some(
            (|| {
                ensure!(rest.len() >= 4, "truncated MP4 sample");
                let n = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
                ensure!(n > 0 && n <= rest.len() - 4, "truncated MP4 sample");
                let unit = &rest[4..4 + n];
                rest = &rest[4 + n..];
                Ok(unit)
            })()
            .inspect_err(|_| rest = &[]),
        )
    })
}

pub enum Converter {
    H264 {
        sps: Vec<Vec<u8>>,
        pps: Vec<Vec<u8>>,
        /// The filter's state, carried from sample to sample as it carries it.
        new_idr: bool,
        sps_seen: bool,
        pps_seen: bool,
    },
    Hevc {
        sets: Vec<Vec<u8>>,
    },
}

impl Converter {
    pub fn h264(sps: Vec<Vec<u8>>, pps: Vec<Vec<u8>>) -> Self {
        Self::H264 {
            sps,
            pps,
            new_idr: true,
            sps_seen: false,
            pps_seen: false,
        }
    }

    pub fn hevc(sets: Vec<Vec<u8>>) -> Self {
        Self::Hevc { sets }
    }

    /// `out` is replaced by the sample's Annex B bytes.
    pub fn convert(&mut self, sample: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
        out.clear();
        match self {
            Self::Hevc { sets } => {
                let mut irap_seen = false;
                for unit in units(sample) {
                    let unit = unit?;
                    ensure!(unit.len() >= 2, "truncated NAL unit");
                    let kind = (unit[0] >> 1) & 63;
                    let irap = (16..=23).contains(&kind);
                    if irap && !irap_seen {
                        for set in sets.iter() {
                            out.extend_from_slice(&FOUR);
                            out.extend_from_slice(set);
                        }
                    }
                    irap_seen |= irap;
                    out.extend_from_slice(&FOUR);
                    out.extend_from_slice(unit);
                }
            }
            Self::H264 {
                sps,
                pps,
                new_idr,
                sps_seen,
                pps_seen,
            } => {
                let put = |out: &mut Vec<u8>, unit: &[u8], long: bool| {
                    out.extend_from_slice(if long || out.is_empty() {
                        &FOUR
                    } else {
                        &FOUR[1..]
                    });
                    out.extend_from_slice(unit);
                };
                let configuration = |out: &mut Vec<u8>, sets: &[Vec<u8>]| {
                    for set in sets {
                        out.extend_from_slice(&FOUR);
                        out.extend_from_slice(set);
                    }
                };
                for unit in units(sample) {
                    let unit = unit?;
                    let kind = unit[0] & 31;
                    if kind == 7 {
                        *sps_seen = true;
                        *new_idr = true;
                    } else if kind == 8 {
                        *pps_seen = true;
                        *new_idr = true;
                        if !*sps_seen && !sps.is_empty() {
                            configuration(out, sps);
                            *sps_seen = true;
                        }
                    }
                    // first_mb_in_slice == 0: a new IDR picture right after another.
                    if !*new_idr && kind == 5 && unit.get(1).is_some_and(|b| b & 0x80 != 0) {
                        *new_idr = true;
                    }
                    if *new_idr && kind == 5 && !*sps_seen && !*pps_seen {
                        configuration(out, sps);
                        configuration(out, pps);
                        *new_idr = false;
                    } else if *new_idr && kind == 5 && *sps_seen && !*pps_seen {
                        configuration(out, pps);
                    }
                    put(out, unit, kind == 7 || kind == 8);
                    if !*new_idr && kind == 1 {
                        *new_idr = true;
                        *sps_seen = false;
                        *pps_seen = false;
                    }
                }
            }
        }
        ensure!(!out.is_empty(), "empty MP4 sample");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Converter;

    fn sample(units: &[&[u8]]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|u| {
                (u.len() as u32)
                    .to_be_bytes()
                    .into_iter()
                    .chain(u.iter().copied())
            })
            .collect()
    }

    #[test]
    fn h264_configuration_goes_before_each_idr_without_its_own() {
        let mut c = Converter::h264(vec![vec![0x67, 1]], vec![vec![0x68, 2]]);
        let mut out = Vec::new();
        c.convert(&sample(&[&[0x09, 0xf0], &[0x65, 0x88]]), &mut out)
            .unwrap();
        assert_eq!(
            out,
            [
                &[0, 0, 0, 1, 0x09, 0xf0][..],
                &[0, 0, 0, 1, 0x67, 1],
                &[0, 0, 0, 1, 0x68, 2],
                &[0, 0, 1, 0x65, 0x88]
            ]
            .concat()
        );
        c.convert(&sample(&[&[0x41, 0x9a]]), &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x41, 0x9a]);
        c.convert(&sample(&[&[0x67, 1], &[0x68, 2], &[0x65, 0x88]]), &mut out)
            .unwrap();
        assert_eq!(
            out,
            [
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 0x88
            ]
        );
    }

    #[test]
    fn hevc_sets_go_once_before_the_first_irap_unit_and_bad_lengths_fail() {
        let mut c = Converter::hevc(vec![vec![0x40, 1], vec![0x42, 1], vec![0x44, 1]]);
        let mut out = Vec::new();
        c.convert(&sample(&[&[0x26, 1, 0x80], &[0x26, 1, 0x00]]), &mut out)
            .unwrap();
        assert_eq!(out.len(), 3 * 6 + 2 * 7);
        c.convert(&sample(&[&[0x02, 1, 0x80]]), &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x02, 1, 0x80]);
        assert!(c.convert(&[0, 0, 0, 9, 1], &mut out).is_err());
        assert!(c.convert(&[0, 0, 0], &mut out).is_err());
        assert!(c.convert(&[0, 0, 0, 1, 0x26], &mut out).is_err());
        assert!(c.convert(&[], &mut out).is_err());
    }
}
