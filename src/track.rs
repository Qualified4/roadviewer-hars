//! GPS and wall-clock calculations from the preserved Python conversion contract.
use crate::{logs::Decoded, replay::r};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|v| v.is_finite())
}
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
fn plausible(t: f64, now: f64) -> bool {
    (1420070400.0..=now + 86400.).contains(&t)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Starts {
    first: Option<u64>,
    init_mono: Option<u64>,
    init: Option<(u64, f64)>,
    clock: Option<(u64, f64)>,
    gps: Option<(u64, f64)>,
}
impl Starts {
    pub fn note(&mut self, kind: &str, mono: u64) {
        if kind == "initData" {
            self.init_mono.get_or_insert(mono);
        } else {
            self.first = Some(self.first.map_or(mono, |first| first.min(mono)));
        }
    }
    pub fn add(&mut self, kind: &str, mono: u64, valid: bool, value: &Value) {
        if kind == "initData" && self.init.is_none() {
            if let Some(wall) = number(&value["wallTimeNanos"]).filter(|v| *v != 0.) {
                self.init = Some((mono, wall / 1e9));
            }
        } else if kind == "clocks" {
            if let Some(wall) = number(&value["wallTimeNanos"])
                .map(|v| v / 1e9)
                .filter(|v| plausible(*v, now()))
            {
                self.clock = Some((mono, wall));
            }
        } else if matches!(kind, "gpsLocation" | "gpsLocationExternal")
            && self.gps.is_none()
            && valid
            && value["hasFix"] == true
            && let Some(wall) = number(&value["unixTimestampMillis"])
                .map(|v| v / 1e3)
                .filter(|v| plausible(*v, now()))
        {
            self.gps = Some((mono, wall));
        }
    }
    pub fn fields(&self, now: f64) -> Value {
        let first = self.first.or(self.init_mono);
        let chosen = first.and_then(|first| {
            [
                (self.gps, "gps"),
                (self.clock, "clock"),
                (self.init, "device"),
            ]
            .into_iter()
            .find_map(|(candidate, name)| {
                let (mono, wall) = candidate?;
                // Subtraction in floating point must permit an initData earlier than this segment.
                let start = wall - (mono as i128 - first as i128) as f64 / 1e9;
                (plausible(wall, now) && plausible(start, now)).then_some((r(start, 3), name))
            })
        });
        json!({"started_at":chosen.map(|v|v.0),"start_source":chosen.map(|v|v.1),"first_mono":first,
            "gps_offset":if chosen.is_some_and(|v|v.1=="gps") {self.gps.map(|(m,w)|r(w-m as f64/1e9,6))} else {None},
            "clock_offset":self.clock.map(|(m,w)|r(w-m as f64/1e9,6)),"start_v":3})
    }
    pub fn summary(&self) -> Value {
        self.fields(now())
    }
}

pub fn distance(a: &[f64; 2], b: &[f64; 2]) -> f64 {
    let (la1, lo1, la2, lo2) = (
        a[0].to_radians(),
        a[1].to_radians(),
        b[0].to_radians(),
        b[1].to_radians(),
    );
    let h =
        ((la2 - la1) / 2.).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.).sin().powi(2);
    2. * 6371008.8 * h.sqrt().min(1.).asin()
}
pub fn collect(decoded: &Decoded, origin: f64) -> Value {
    let mut chosen = vec![];
    let mut source = None;
    for topic in [
        "gpsLocationExternal",
        "gpsLocation",
        "vehicleGps",
        "carrotMan",
    ] {
        let mut fixes = vec![];
        let mut previous = None;
        let rows = decoded
            .streams
            .get(if topic == "vehicleGps" {
                "carState"
            } else {
                topic
            })
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for row in rows {
            let v = &row.value;
            let (stamp, lat, lon, speed, bearing) = match topic {
                "vehicleGps" => {
                    let g = &v["vehicleGps"];
                    if g["available"] != true || g["valid"] != true {
                        continue;
                    }
                    let sample = g["sampleMonoTime"].as_u64().filter(|v| *v != 0);
                    if sample.is_some() && sample == previous {
                        continue;
                    }
                    (
                        sample.unwrap_or(row.stamp),
                        number(&g["latitude"]),
                        number(&g["longitude"]),
                        number(&v["vEgo"]),
                        if g["headingValid"] == true {
                            number(&g["headingDeg"])
                        } else {
                            None
                        },
                    )
                }
                "carrotMan" => (
                    row.stamp,
                    number(&v["xPosLat"]),
                    number(&v["xPosLon"]),
                    None,
                    number(&v["xPosAngle"]),
                ),
                _ => {
                    if !row.valid
                        || v["hasFix"] != true
                        || !number(&v["horizontalAccuracy"])
                            .is_some_and(|v| (0.0..=100.).contains(&v))
                    {
                        continue;
                    }
                    (
                        row.stamp,
                        number(&v["latitude"]),
                        number(&v["longitude"]),
                        number(&v["speed"]),
                        number(&v["bearingDeg"]),
                    )
                }
            };
            let (Some(lat), Some(lon)) = (lat, lon) else {
                continue;
            };
            if !(-90.0..=90.).contains(&lat)
                || !(-180.0..=180.).contains(&lon)
                || (lat.abs() < 1e-7 && lon.abs() < 1e-7)
            {
                continue;
            }
            if topic == "vehicleGps" {
                previous = v["vehicleGps"]["sampleMonoTime"]
                    .as_u64()
                    .filter(|v| *v != 0);
            }
            fixes.push((
                stamp,
                lat,
                lon,
                speed.filter(|v| *v >= 0.),
                bearing.map(|v| v.rem_euclid(360.)),
            ));
        }
        if !fixes.is_empty() {
            chosen = fixes;
            source = Some(topic);
            break;
        }
    }
    chosen.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then(a.2.total_cmp(&b.2))
    });
    let mut points: Vec<Value> = vec![];
    let mut second = None;
    for (stamp, lat, lon, speed, bearing) in chosen {
        let t = stamp as f64 / 1e9 - origin;
        let bucket = t.floor() as i64;
        if second == Some(bucket) {
            continue;
        }
        if let Some(last) = points.last() {
            let gap = t - last[0].as_f64().unwrap();
            if gap > 0.
                && distance(
                    &[last[1].as_f64().unwrap(), last[2].as_f64().unwrap()],
                    &[lat, lon],
                ) / gap
                    > 90.
            {
                continue;
            }
        }
        second = Some(bucket);
        points.push(json!([
            r(t, 2),
            r(lat, 6),
            r(lon, 6),
            speed.map(|v| r(v, 1)),
            bearing.map(|v| r(v, 0))
        ]));
    }
    json!({"v":2,"source":source,"points":points})
}
pub fn summary(track: &Value) -> Value {
    let points = track["points"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let coord = |p: &Value| [p[1].as_f64().unwrap(), p[2].as_f64().unwrap()];
    let distance = points
        .windows(2)
        .map(|p| distance(&coord(&p[0]), &coord(&p[1])))
        .sum::<f64>();
    let bbox = if points.is_empty() {
        None
    } else {
        let mut b = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for p in points {
            let [lat, lon] = coord(p);
            b[0] = b[0].min(lat);
            b[1] = b[1].min(lon);
            b[2] = b[2].max(lat);
            b[3] = b[3].max(lon);
        }
        Some(b)
    };
    json!({"track_v":2,"has_track":!points.is_empty(),"track_distance_m":if points.is_empty(){None}else{Some(distance.round_ties_even() as u64)},"track_bbox":bbox,"track_read":true})
}

pub fn simplify(coords: &[[f64; 2]], limit: usize) -> Vec<[f64; 2]> {
    if coords.len() <= limit {
        return coords.to_vec();
    }
    assert!(limit >= 2);
    let scale = (coords.iter().map(|p| p[0]).sum::<f64>() / coords.len() as f64)
        .to_radians()
        .cos();
    let xy = coords
        .iter()
        .map(|p| {
            [
                p[1].to_radians() * scale * 6371008.8,
                p[0].to_radians() * 6371008.8,
            ]
        })
        .collect::<Vec<_>>();
    let mut tolerance = 2.;
    loop {
        let mut marked = vec![false; xy.len()];
        marked[0] = true;
        marked[xy.len() - 1] = true;
        let mut stack = vec![(0, xy.len() - 1)];
        while let Some((a, b)) = stack.pop() {
            let ([ax, ay], [bx, by]) = (xy[a], xy[b]);
            let (dx, dy) = (bx - ax, by - ay);
            let length = dx.hypot(dy);
            let mut best = -1.;
            let mut index = None;
            for (i, [px, py]) in xy.iter().copied().enumerate().take(b).skip(a + 1) {
                let distance = if length != 0. {
                    (dy * (px - ax) - dx * (py - ay)).abs() / length
                } else {
                    (px - ax).hypot(py - ay)
                };
                if distance > best {
                    best = distance;
                    index = Some(i);
                }
            }
            if best > tolerance
                && let Some(index) = index
            {
                marked[index] = true;
                stack.push((a, index));
                stack.push((index, b));
            }
        }
        let out = coords
            .iter()
            .zip(marked)
            .filter_map(|(p, keep)| keep.then_some(*p))
            .collect::<Vec<_>>();
        if out.len() <= limit {
            return out;
        }
        tolerance *= 2.;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn route_init_does_not_set_segments_first_clock_and_gps_wins() {
        let mut clock = Starts::default();
        clock.note("initData", 1_000_000_000);
        clock.add(
            "initData",
            1_000_000_000,
            true,
            &json!({"wallTimeNanos":1600000000000000000u64}),
        );
        clock.note("unselectedEvent", 11_000_000_000);
        clock.add(
            "clocks",
            12_000_000_000,
            true,
            &json!({"wallTimeNanos":1700000001000000000u64}),
        );
        clock.add(
            "gpsLocation",
            13_000_000_000,
            true,
            &json!({"hasFix":true,"unixTimestampMillis":1700000003000u64}),
        );
        let fields = clock.fields(1800000000.);
        assert_eq!(fields["first_mono"], 11_000_000_000u64);
        assert_eq!(fields["started_at"], 1700000001.);
        assert_eq!(fields["start_source"], "gps");
        assert_eq!(fields["clock_offset"], 1699999989.);
    }
}
