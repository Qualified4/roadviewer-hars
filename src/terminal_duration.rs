//! Duration of a remuxed video's last sample, shared by both remux engines so a
//! fallback from one to the other never changes the result.
//!
//! Evidence and rule choice: `docs/hars/TERMINAL-DURATION.md`. In short, the
//! rlog of a segment logs the camera frame that follows the last encoded one;
//! the end-of-exposure gap to it equals the real interval to the next segment's
//! first frame on all 13 measured pairs. Without that record the median frame
//! interval had the smallest worst-case error of the measured estimates; the
//! Python reference's probed `r_frame_rate` and the previous interval are worse.
use serde::{Deserialize, Serialize};

/// openpilot road and wide cameras run at 20 fps; only a one-frame clip uses it.
pub const NOMINAL_FPS: u64 = 20;
/// A logged next frame farther than this many median intervals is not taken as
/// the length of the last frame (a stall or a dropped run, not one frame).
const MAX_INTERVALS: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Logged end of exposure of the camera frame after the last encoded one.
    NextFrame,
    /// Lower median of the clip's own frame intervals.
    MedianInterval,
    /// Single-frame clip without a usable next frame.
    Nominal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terminal {
    pub ticks: u64,
    pub basis: Basis,
}

/// Nanoseconds to `timescale` ticks, rounded half up, in integers only.
pub fn ticks_from_ns(ns: u64, timescale: u32) -> u64 {
    let scaled = u128::from(ns) * u128::from(timescale) + 500_000_000;
    u64::try_from(scaled / 1_000_000_000).unwrap_or(u64::MAX)
}

/// `stamps` are the clip's strictly increasing decode clocks in `timescale`
/// ticks; `next_frame_ns` is the logged gap from the last frame to the next one.
pub fn decide(
    stamps: &[i64],
    timescale: u32,
    next_frame_ns: Option<u64>,
) -> anyhow::Result<Terminal> {
    anyhow::ensure!(
        !stamps.is_empty() && stamps.windows(2).all(|s| s[1] > s[0]) && timescale > 0,
        "invalid PTS/DTS"
    );
    let mut intervals = stamps
        .windows(2)
        .map(|s| s[1].abs_diff(s[0]))
        .collect::<Vec<_>>();
    intervals.sort_unstable();
    let typical = intervals
        .get(intervals.len().saturating_sub(1) / 2)
        .copied()
        .map(|ticks| (ticks, Basis::MedianInterval));
    let reference = typical.map_or_else(|| u64::from(timescale).div_ceil(NOMINAL_FPS), |(t, _)| t);
    if let Some(ns) = next_frame_ns {
        let ticks = ticks_from_ns(ns, timescale);
        if ticks > 0 && ticks <= reference.saturating_mul(MAX_INTERVALS) {
            return Ok(Terminal {
                ticks,
                basis: Basis::NextFrame,
            });
        }
    }
    let (ticks, basis) = typical.unwrap_or((reference, Basis::Nominal));
    anyhow::ensure!(ticks > 0, "invalid PTS/DTS");
    Ok(Terminal { ticks, basis })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_is_integer_and_half_up() {
        assert_eq!(ticks_from_ns(49_959_820, 90_000), 4496);
        assert_eq!(ticks_from_ns(50_000_000, 90_000), 4500);
        assert_eq!(ticks_from_ns(5_000, 1_000_000), 5);
        assert_eq!(ticks_from_ns(4_500, 1_000_000), 5);
        assert_eq!(ticks_from_ns(4_499, 1_000_000), 4);
    }
}
