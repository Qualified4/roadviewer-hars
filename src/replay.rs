//! Python decoder's camera-clock frame analysis and replayV2 packing.
use crate::{
    logs::{Decoded, Sample},
    telemetry::nested,
};
use anyhow::{Context, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn r(v: f64, digits: usize) -> f64 {
    format!("{v:.digits$}").parse().unwrap_or(v)
}
fn num(v: &Value) -> Option<f64> {
    v.as_f64().filter(|v| v.is_finite())
}
fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn val(v: &Value, k: &str, default: Value) -> Value {
    v.get(k).cloned().unwrap_or(default)
}
pub struct View<'a>(BTreeMap<&'a str, Vec<&'a Sample>>);
impl<'a> View<'a> {
    pub fn new(streams: &'a BTreeMap<String, Vec<Sample>>) -> Self {
        Self(
            streams
                .iter()
                .map(|(k, rows)| {
                    let mut rows = rows.iter().collect::<Vec<_>>();
                    if k != "initData" {
                        rows.sort_by_key(|r| r.stamp);
                    }
                    (k.as_str(), rows)
                })
                .collect(),
        )
    }
    fn rows(&self, k: &str) -> &[&'a Sample] {
        self.0.get(k).map(Vec::as_slice).unwrap_or(&[])
    }
    pub fn past(&self, k: &str, t: f64, age: f64, leading: bool) -> Option<&'a Value> {
        let rows = self.rows(k);
        let n = rows.partition_point(|r| r.stamp as f64 <= t);
        let row = if n == 0 {
            if !leading {
                return None;
            }
            let row = *rows.first()?;
            if row.stamp as f64 - t > 2e9 {
                return None;
            }
            row
        } else {
            rows[n - 1]
        };
        (row.valid
            && if leading {
                t - (row.stamp as f64) <= age
            } else {
                t - (row.stamp as f64) < age
            })
        .then_some(&row.value)
    }
}
fn nearest<'a>(rows: &[&'a Sample], t: f64, radar: bool) -> Option<(&'a Sample, f64)> {
    let time = |r: &&Sample| {
        if radar {
            r.value["mdMonoTime"].as_u64().unwrap_or(r.stamp) as f64
        } else {
            r.stamp as f64
        }
    };
    let k = rows.partition_point(|r| time(r) < t);
    let index = if k == 0 {
        0
    } else if k == rows.len() || (time(&rows[k - 1]) - t).abs() <= (time(&rows[k]) - t).abs() {
        k - 1
    } else {
        k
    };
    rows.get(index).map(|r| (*r, time(r) - t))
}
pub fn points3(line: &Value) -> Value {
    json!(
        arr(&line["x"])
            .iter()
            .zip(arr(&line["y"]))
            .zip(arr(&line["z"]))
            .map(|((x, y), z)| json!([x, y, z]))
            .collect::<Vec<_>>()
    )
}
fn points2(line: &Value) -> anyhow::Result<Value> {
    let xs = line["x"].as_array().context("model line missing x")?;
    let ys = line["y"].as_array().context("model line missing y")?;
    Ok(json!(
        xs.iter()
            .zip(ys)
            .filter_map(|(x, y)| Some(json!([r(num(x)?, 3), r(num(y)?, 3)])))
            .collect::<Vec<_>>()
    ))
}
fn sample_line(line: &Value, x: f64) -> Option<[f64; 3]> {
    for pair in arr(line).windows(2) {
        let a = &pair[0];
        let b = &pair[1];
        let p = [
            num(&a[0]),
            num(&a[1]),
            num(&a[2]),
            num(&b[0]),
            num(&b[1]),
            num(&b[2]),
        ];
        if let [Some(ax), Some(ay), Some(az), Some(bx), Some(by), Some(bz)] = p
            && ax <= x
            && x <= bx
            && bx > ax
        {
            let ratio = (x - ax) / (bx - ax);
            return Some([x, ay + (by - ay) * ratio, az + (bz - az) * ratio]);
        }
    }
    None
}
pub fn restore(targets: &Value, m: &Value, valid: bool) -> Value {
    let _profile = crate::profile::Scope::new("ccnc_geometry");
    if targets.is_null() {
        return Value::Null;
    }
    let lanes = arr(&m["laneLines"]);
    let curves = if valid && lanes.len() >= 3 {
        vec![points3(&lanes[1]), points3(&lanes[2])]
    } else {
        vec![]
    };
    json!(
        arr(targets)
            .iter()
            .map(|target| {
                let x = target["x"].as_f64().unwrap() / 0.8;
                let mut correction = if valid && target["slot"] == "FF" {
                    sample_line(&points3(&m["position"]), x).map(|p| p[1])
                } else {
                    None
                };
                if correction.is_none() && curves.len() == 2 {
                    correction = sample_line(&curves[0], 0.)
                        .zip(sample_line(&curves[0], x))
                        .zip(sample_line(&curves[1], 0.).zip(sample_line(&curves[1], x)))
                        .map(|((a, b), (c, d))| ((b[1] - a[1]) + (d[1] - c[1])) / 2.);
                }
                let y = target["y"].as_f64().unwrap() + correction.unwrap_or(0.);
                let mut out = target.clone();
                out["displayX"] = target["x"].clone();
                out["displayY"] = target["y"].clone();
                out["x"] = json!(r(x, 3));
                out["y"] = json!(r(y, 3));
                out["yRel"] = json!(r(-y, 3));
                out["curveRestored"] = json!(correction.is_some());
                out
            })
            .collect::<Vec<_>>()
    )
}
pub fn can_targets(payload: &[u8], rear: bool) -> Value {
    if payload.len() != 32 {
        return json!([]);
    }
    let bits = |offset: usize, width: usize| -> u64 {
        (0..width).fold(0, |out, i| {
            out | (((payload[(offset + i) / 8] >> ((offset + i) % 8)) & 1) as u64) << i
        })
    };
    let mut out = vec![];
    if rear {
        for (slot, detect, distance, lateral, sign) in
            [("LR", 163, 168, 176, 1.), ("RR", 192, 197, 205, -1.)]
        {
            let d = bits(detect, 5);
            if !(1..=14).contains(&d) {
                continue;
            }
            let x = -r(bits(distance, 8) as f64 * 0.1, 1);
            let y = r(bits(lateral, 7) as f64 * 0.1 * sign, 1);
            out.push(json!({"slot":slot,"detect":d,"x":x,"yRel":y,"y":-y}));
        }
    } else {
        for (slot, offset) in [("LF", 112), ("FF", 64), ("RF", 136)] {
            let detect = bits(offset, 5);
            let distance = bits(offset + 5, 11);
            let mut lateral = bits(offset + 16, 7) as i64;
            if !(1..=14).contains(&detect) || distance >= 2046 {
                continue;
            }
            if slot == "FF" && lateral >= 64 {
                lateral -= 128;
            }
            let y = r(
                lateral as f64 * 0.1 * if slot == "LF" { 1. } else { -1. },
                1,
            );
            out.push(
                json!({"slot":slot,"detect":detect,"x":r(distance as f64*0.1,1),"yRel":y,"y":-y}),
            );
        }
    }
    json!(out)
}
pub fn can_road(payload: &[u8]) -> Value {
    if payload.len() != 32 {
        return Value::Null;
    }
    let bits = |offset: usize, width: usize| -> u64 {
        (0..width).fold(0, |out, i| {
            out | (((payload[(offset + i) / 8] >> ((offset + i) % 8)) & 1) as u64) << i
        })
    };
    json!({"target":bits(66,3),"distance":r(bits(69,11) as f64*0.1,1),"highlight":bits(105,4),"left":bits(120,3),"right":bits(123,3),"laneLeft":bits(80,4),"laneRight":bits(90,4),"blinkerLeft":bits(57,3)!=0,"blinkerRight":bits(60,3)!=0})
}
fn held(rows: &[(u64, bool, Value)], t: f64) -> Value {
    let i = rows.partition_point(|r| r.0 as f64 <= t);
    if i == 0 {
        return Value::Null;
    }
    let row = &rows[i - 1];
    if !row.1 || t - row.0 as f64 >= 150e6 {
        Value::Null
    } else {
        row.2.clone()
    }
}
pub fn camera_info(view: &View<'_>, t: f64) -> Value {
    let cal = view
        .past("liveCalibration", t, 10e9, true)
        .cloned()
        .unwrap_or(json!({}));
    let get = |kind: &str, field: &str| {
        view.past(kind, t, f64::INFINITY, true)
            .and_then(|v| v.get(field))
            .cloned()
            .unwrap_or(json!("unknown"))
    };
    let rpy = |key: &str| {
        if arr(&cal[key]).len() == 3 && arr(&cal[key]).iter().all(|v| num(v).is_some()) {
            cal[key].clone()
        } else {
            Value::Null
        }
    };
    let height = arr(&cal["height"])
        .first()
        .and_then(num)
        .filter(|v| *v > 0.3 && *v < 3.);
    let device_id = view
        .rows("initData")
        .iter()
        .find(|row| {
            row.valid
                && row.value["dongleId"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty())
        })
        .map(|r| r.value["dongleId"].clone())
        .unwrap_or(Value::Null);
    json!({"device":get("deviceState","deviceType"),"deviceId":device_id,"sensor":get("roadCameraState","sensor"),"calibrationStatus":val(&cal,"calStatus",json!("unknown")),"rpy":rpy("rpyCalib"),"height":height.unwrap_or(1.22),"heightDefault":height.is_none(),"wideRpy":rpy("wideFromDeviceEuler"),"wideSensor":get("wideRoadCameraState","sensor")})
}
pub fn overlay(info: &Value, m: &Value) -> anyhow::Result<Value> {
    let _profile = crate::profile::Scope::new("overlay_geometry");
    if !matches!(
        info["calibrationStatus"].as_str(),
        Some("calibrated" | "recalibrating")
    ) || info["rpy"].is_null()
    {
        return Ok(Value::Null);
    }
    let device = info["device"].as_str().unwrap_or("");
    let sensor = info["sensor"].as_str().unwrap_or("");
    let (w, h, f) = match (device, sensor) {
        ("neo", "unknown") => (1164., 874., 910.),
        (_, "ar0231" | "ox03c10") | ("tici" | "pc", "unknown") => (1928., 1208., 2648.),
        ("tici" | "tizi" | "mici", "os04c10") => (1344., 760., 1141.5),
        _ => return Ok(Value::Null),
    };
    let (roll, pitch, yaw) = (
        info["rpy"][0].as_f64().unwrap(),
        info["rpy"][1].as_f64().unwrap(),
        info["rpy"][2].as_f64().unwrap(),
    );
    let (cr, sr, cp, sp, ca, sa) = (
        roll.cos(),
        roll.sin(),
        pitch.cos(),
        pitch.sin(),
        yaw.cos(),
        yaw.sin(),
    );
    let rot = [
        [ca * cp, ca * sp * sr - sa * cr, ca * sp * cr + sa * sr],
        [sa * cp, sa * sp * sr + ca * cr, sa * sp * cr - ca * sr],
        [-sp, cp * sr, cp * cr],
    ];
    let basis = (0..3)
        .map(|i| {
            json!([
                0.5 * rot[0][i] + f / w * rot[1][i],
                0.5 * rot[0][i] + f / h * rot[2][i],
                rot[0][i]
            ])
        })
        .collect::<Vec<_>>();
    let lanes = arr(&m["laneLines"]).iter().map(points3).collect::<Vec<_>>();
    let edges = arr(&m["roadEdges"]).iter().map(points3).collect::<Vec<_>>();
    let position = points3(&m["position"]);
    for line in lanes.iter().chain(&edges).chain(std::iter::once(&position)) {
        if arr(line)
            .iter()
            .any(|p| arr(p).iter().any(|v| num(v).is_none()))
        {
            bail!("non-finite model overlay coordinates");
        }
    }
    Ok(
        json!({"geometry":{"basis":basis,"height":info["height"],"lanes":lanes,"edges":edges,"position":position}}),
    )
}
struct Steering {
    torque: f64,
    previous: Option<f64>,
}
impl Steering {
    fn at(&mut self, view: &View<'_>, t: f64) -> Value {
        let get = |k, age| view.past(k, t, age, false);
        let empty = json!({});
        let cs = get("carState", 150e6).unwrap_or(&empty);
        let cc = get("carControl", 150e6).unwrap_or(&empty);
        let ctl = get("controlsState", 150e6).unwrap_or(&empty);
        let out = get("carOutput", 150e6).unwrap_or(&empty);
        let sd = get("selfdriveState", 150e6).unwrap_or(&empty);
        let angle = num(&cs["steeringAngleDeg"]);
        let pressed = cs["steeringPressed"].as_bool();
        let active = cc["latActive"].as_bool();
        let mut torque = None;
        if ctl["lateralControlState"].get("angleState").is_some() {
            if active == Some(false) {
                torque = Some(0.);
            } else if active == Some(true)
                && let Some(lp) = get("liveParameters", 2e9)
            {
                let cp = get("carParams", f64::INFINITY).unwrap_or(&empty);
                let maximum = num(cp.get("maxLateralAccel").unwrap_or(&json!(3.)));
                if let (Some(speed), Some(roll), Some(desired), Some(max)) = (
                    num(&cs["vEgo"]),
                    num(&lp["roll"]),
                    num(&ctl["desiredCurvature"]),
                    maximum,
                ) && max > 0.
                {
                    torque = Some(
                        ((desired * speed * speed
                            - roll * 9.81 * ((speed - 5.) / 10.).clamp(0., 1.))
                            / max)
                            .clamp(-1., 1.),
                    );
                }
            }
        } else if !ctl.as_object().is_none_or(|v| v.is_empty()) {
            torque = nested(out, "actuatorsOutput.torque")
                .and_then(num)
                .map(|v| -v);
        }
        let mut dt = self.previous.map(|p| (t - p) / 1e9).unwrap_or(0.05);
        if let Some(torque) = torque {
            if dt <= 0. || dt > 0.15 {
                self.torque = 0.;
                dt = 0.05;
            }
            self.torque += dt.max(0.) / (0.1 + dt.max(0.)) * (torque - self.torque);
        } else {
            self.torque = 0.;
        }
        self.previous = Some(t);
        let magnitude = torque.map(|_| self.torque.abs());
        let mag = magnitude.unwrap_or(0.);
        let blend = ((mag - 0.75) * 4.).clamp(0., 1.);
        let (state, color) = if pressed == Some(true) {
            ("driver", [255, 255, 255])
        } else if pressed.is_none() || active.is_none() {
            ("unknown", [148, 165, 184])
        } else if active == Some(true) {
            let hue = (1. / 3. + blend * ((115. / 255.) / 6. - 1. / 3.)) * 6.;
            let sector = hue.floor() as i32;
            let f = hue - hue.floor();
            let rgb = match sector.rem_euclid(6) {
                0 => [1., f, 0.],
                1 => [1. - f, 1., 0.],
                2 => [0., 1., f],
                3 => [0., 1. - f, 1.],
                4 => [f, 0., 1.],
                _ => [1., 0., 1. - f],
            };
            ("active", rgb.map(|v| (v * 255.) as i32))
        } else {
            ("inactive", [230, 230, 230])
        };
        json!({"angle":angle,"pressed":pressed,"active":active,"torque":magnitude,"scale":1.+(mag-0.5).clamp(0.,0.5),"color":color,"state":state,"lane":ctl["activeLaneLine"]==true,"critical":sd["alertHudVisual"]=="steerRequired" && !sd["alertSize"].is_null() && sd["alertSize"]!="none"})
    }
}

fn camera_times(view: &View<'_>) -> BTreeMap<u64, u64> {
    let camera: BTreeMap<u64, u64> = view
        .rows("qRoadEncodeIdx")
        .iter()
        .filter_map(|r| {
            Some((
                r.value["frameId"].as_u64()?,
                r.value["timestampEof"].as_u64()?,
            ))
        })
        .collect();
    camera
}
fn model_time(camera: &BTreeMap<u64, u64>, row: &Sample) -> f64 {
    camera
        .get(&row.value["frameId"].as_u64().unwrap_or(0))
        .copied()
        .unwrap_or_else(|| {
            row.value["timestampEof"]
                .as_u64()
                .filter(|v| *v != 0)
                .unwrap_or(row.stamp)
        }) as f64
        / 1e9
}
/// Obtain the same model origin before allocating replay geometry.
pub fn origin(decoded: &Decoded) -> anyhow::Result<f64> {
    let view = View::new(&decoded.streams);
    let camera = camera_times(&view);
    view.rows("modelV2")
        .iter()
        .map(|row| model_time(&camera, row))
        .min_by(f64::total_cmp)
        .context("이 로그에 modelV2 데이터가 없습니다.")
}

pub fn frames(
    decoded: &Decoded,
    mut progress: impl FnMut(usize, usize),
) -> anyhow::Result<(Vec<Value>, f64)> {
    let view = View::new(&decoded.streams);
    let mut models = view.rows("modelV2").to_vec();
    if models.is_empty() {
        bail!("이 로그에 modelV2 데이터가 없습니다.");
    }
    let camera = camera_times(&view);
    let clock = |row: &&Sample| model_time(&camera, row);
    models.sort_by(|a, b| clock(a).total_cmp(&clock(b)));
    let origin = clock(&models[0]);
    let mut radar = view.rows("radarState").to_vec();
    radar.sort_by_key(|r| r.value["mdMonoTime"].as_u64().unwrap_or(r.stamp));
    let hyundai = view
        .rows("carParams")
        .iter()
        .any(|r| r.valid && r.value["brand"] == "hyundai");
    let mut commands = vec![];
    let mut rears = vec![];
    let mut roads = vec![];
    let mut bars = vec![];
    let mut distances = vec![];
    let mut signals = vec![];
    for row in view.rows("sendcan") {
        for msg in arr(&row.value) {
            let payload = hex::decode(msg["dat"].as_str().unwrap_or("")).unwrap_or_default();
            if msg["address"] == 0x162 && hyundai {
                commands.push((row.stamp, row.valid, can_targets(&payload, false)));
                rears.push((row.stamp, row.valid, can_targets(&payload, true)));
            }
            if msg["address"] == 0x161 && hyundai {
                roads.push((row.stamp, row.valid, can_road(&payload)));
            }
        }
    }
    for row in view.rows("carControl") {
        bars.push((
            row.stamp,
            row.valid,
            nested(&row.value, "hudControl.leadDistanceBars")
                .cloned()
                .unwrap_or(json!(0)),
        ));
    }
    for row in view.rows("longitudinalPlan") {
        distances.push((
            row.stamp,
            row.valid,
            num(&row.value["desiredDistance"])
                .filter(|v| *v > 0.)
                .map(|v| json!(r(v, 1)))
                .unwrap_or(Value::Null),
        ));
    }
    for row in view.rows("carState") {
        let mut value = json!({"acceleration":num(&row.value["aEgo"])});
        for (to, from) in [
            ("blinkerLeft", "leftBlinker"),
            ("blinkerRight", "rightBlinker"),
            ("blindspotLeft", "leftBlindspot"),
            ("blindspotRight", "rightBlindspot"),
        ] {
            value[to] = json!(row.value[from] == true);
        }
        signals.push((row.stamp, row.valid, value));
    }
    let mut steering = Steering {
        torque: 0.,
        previous: None,
    };
    let mut frames = Vec::with_capacity(models.len());
    for row in &models {
        let m = &row.value;
        let time = clock(row);
        let stamp = time * 1e9;
        let model_stamp = row.stamp as f64;
        let speed = nearest(view.rows("carState"), stamp, false)
            .filter(|(r, dt)| r.valid && dt.abs() < 150e6)
            .and_then(|(r, _)| num(&r.value["vEgo"]))
            .map(|v| v * 3.6);
        let mut tracks = vec![];
        let mut live_valid = false;
        let mut live_delta = None;
        if let Some((live, dt)) = nearest(view.rows("liveTracks"), model_stamp, false) {
            live_delta = Some(dt / 1e6);
            live_valid = live.valid && dt.abs() < 150e6;
            if live_valid {
                for (i, v) in arr(&live.value["points"]).iter().enumerate() {
                    if let (Some(x), Some(y), Some(vrel)) =
                        (num(&v["dRel"]), num(&v["yRel"]), num(&v["vRel"]))
                    {
                        tracks.push(json!({"index":i,"trackId":val(v,"trackId",json!(-1)),"x":x,"y":-y,"yRel":y,"vRel":vrel,"measured":val(v,"measured",json!(false)),"source":val(v,"radarSource",json!("unknown")),"trackState":val(v,"trackState",json!(0))}));
                    }
                }
            }
        }
        let mut selected = Value::Null;
        let mut targets = vec![];
        if let Some((radar, dt)) = nearest(&radar, model_stamp, true)
            && radar.valid
            && dt.abs() < 150e6
        {
            let lead = &radar.value["leadOne"];
            if lead["status"] == true {
                let (Some(x), Some(y), Some(vrel)) =
                    (num(&lead["dRel"]), num(&lead["yRel"]), num(&lead["vRel"]))
                else {
                    bail!("non-finite selected radar target");
                };
                selected = json!({"x":x,"y":-y,"radar":lead["radar"],"vRel":vrel,"trackId":val(lead,"radarTrackId",json!(-1))});
            }
            for (group, key) in [
                ("center", "leadsCenter"),
                ("left", "leadsLeft"),
                ("right", "leadsRight"),
            ] {
                for (i, v) in arr(&radar.value[key]).iter().enumerate() {
                    if v["status"] == true
                        && let (Some(x), Some(y), Some(vrel)) =
                            (num(&v["dRel"]), num(&v["yRel"]), num(&v["vRel"]))
                    {
                        targets.push(json!({"group":group,"index":i,"x":x,"y":-y,"yRel":y,"vRel":vrel,"radar":val(v,"radar",json!(false)),"trackId":val(v,"radarTrackId",json!(-1)),"modelProb":val(v,"modelProb",json!(0))}));
                    }
                }
            }
        }
        let mut leads = vec![];
        for lead in arr(&m["leadsV3"]).iter().take(2) {
            if !arr(&lead["x"]).is_empty() && !arr(&lead["y"]).is_empty() {
                anyhow::ensure!(
                    num(&lead["x"][0]).is_some()
                        && num(&lead["y"][0]).is_some()
                        && num(&lead["prob"]).is_some(),
                    "non-finite model lead"
                );
                leads.push(json!({"x":lead["x"][0],"y":lead["y"][0],"p":lead["prob"],"speedKph":num(&lead["v"][0]).map(|v|v*3.6)}));
            }
        }
        let lines = |key: &str| -> anyhow::Result<Value> {
            Ok(json!(
                m[key]
                    .as_array()
                    .context("missing model line list")?
                    .iter()
                    .map(points2)
                    .collect::<anyhow::Result<Vec<_>>>()?
            ))
        };
        let firsts = |key: &str| {
            json!(
                arr(&m[key])
                    .iter()
                    .map(|l| num(&l["y"][0]))
                    .collect::<Vec<_>>()
            )
        };
        for key in ["laneLineProbs", "roadEdgeStds"] {
            anyhow::ensure!(
                m[key]
                    .as_array()
                    .is_some_and(|v| v.iter().all(|x| num(x).is_some())),
                "invalid model confidence array"
            );
        }
        let empty = json!({"x":[],"y":[]});
        let info = camera_info(&view, stamp);
        let gap = held(&bars, stamp);
        let frame = json!({"t":r(time-origin,6),"id":m["frameId"],"egoSpeedKph":speed,"steering":steering.at(&view,stamp),"valid":row.valid,"position":points2(m.get("position").unwrap_or(&empty))?,"lanes":lines("laneLines")?,"laneY0":firsts("laneLines"),"lp":m["laneLineProbs"],"edges":lines("roadEdges")?,"edgeY0":firsts("roadEdges"),"es":m["roadEdgeStds"],"leads":leads,"selected":selected,"radarTargets":targets,"liveTracks":tracks,"liveTracksValid":live_valid,"liveTracksDeltaMs":live_delta,"ccncTargets":restore(&held(&commands,stamp),m,row.valid),"ccncRearTargets":if commands.is_empty(){Value::Null}else{held(&rears,stamp)},"ccncRoad":held(&roads,stamp),"roadSignals":held(&signals,stamp),"gap":if gap.as_u64().is_some_and(|v|(1..=4).contains(&v)){gap}else{Value::Null},"gapTarget":held(&distances,stamp),"cameraInfo":info,"overlay":overlay(&info,m)?});
        frames.push(frame);
        progress(frames.len(), models.len());
    }
    Ok((frames, origin))
}

pub fn compact(data: &mut Value) {
    let _profile = crate::profile::Scope::new("replay_compact");
    fn rounded(v: &mut Value, d: usize) {
        match v {
            Value::Array(a) => {
                for x in a {
                    rounded(x, d);
                }
            }
            Value::Number(_) => {
                if let Some(n) = v.as_f64() {
                    *v = json!(r(n, d));
                }
            }
            _ => {}
        }
    }
    fn same_flat(a: &Value, source: &Value) -> bool {
        let Some(points) = a.as_array() else {
            return false;
        };
        let mut expected = arr(source)
            .iter()
            .filter_map(|p| Some((r(num(&p[0])?, 3), r(num(&p[1])?, 3))));
        for point in points {
            let Some((x, y)) = expected.next() else {
                return false;
            };
            if arr(point).len() != 2
                || !num(&point[0]).is_some_and(|v| (v - x).abs() <= 0.002)
                || !num(&point[1]).is_some_and(|v| (v - y).abs() <= 0.002)
            {
                return false;
            }
        }
        expected.next().is_none()
    }
    let v2 = data["replayV"] != 2;
    let mut infos = data["cameraInfos"].as_array().cloned().unwrap_or_default();
    if let Some(frames) = data["frames"].as_array_mut() {
        for frame in frames.iter_mut() {
            if frame["cameraInfo"].is_object() {
                let info = frame["cameraInfo"].clone();
                let i = infos.iter().position(|v| v == &info).unwrap_or_else(|| {
                    infos.push(info);
                    infos.len() - 1
                });
                frame["cameraInfo"] = json!(i);
            }
            for key in ["liveTracks", "radarTargets"] {
                if let Some(targets) = frame[key].as_array_mut() {
                    for target in targets {
                        if key == "liveTracks" {
                            target.as_object_mut().unwrap().remove("index");
                        }
                        for field in ["x", "yRel", "vRel"] {
                            if let Some(v) = target.get_mut(field) {
                                rounded(v, 4);
                            }
                        }
                        target.as_object_mut().unwrap().remove("y");
                    }
                }
            }
            if let Some(leads) = frame["leads"].as_array_mut() {
                for lead in leads {
                    for key in ["x", "y", "speedKph"] {
                        if let Some(v) = lead.get_mut(key) {
                            rounded(v, 4);
                        }
                    }
                }
            }
            if frame["selected"].is_object() {
                for key in ["x", "y", "vRel"] {
                    if let Some(v) = frame["selected"].get_mut(key) {
                        rounded(v, 4);
                    }
                }
            }
            if let Some(geometry) = frame
                .get_mut("overlay")
                .and_then(|v| v.get_mut("geometry"))
                .and_then(Value::as_object_mut)
            {
                for (key, v) in geometry {
                    rounded(v, if key == "basis" { 6 } else { 4 });
                }
            }
            if v2 && frame["overlay"]["geometry"].is_object() {
                for (key, lines) in [("lanes", true), ("edges", true), ("position", false)] {
                    let source = &frame["overlay"]["geometry"][key];
                    if source.is_null() {
                        continue;
                    }
                    let equal = if lines {
                        frame[key].is_array()
                            && arr(&frame[key]).len() == arr(source).len()
                            && arr(&frame[key])
                                .iter()
                                .zip(arr(source))
                                .all(|(a, b)| same_flat(a, b))
                    } else {
                        same_flat(&frame[key], source)
                    };
                    if equal {
                        frame.as_object_mut().unwrap().remove(key);
                    } else if frame.get(key).is_none() {
                        frame[key] = Value::Null;
                    }
                }
            }
        }
    }
    if v2 {
        let mut grid = data["laneX"].as_array().cloned().filter(|v| !v.is_empty());
        if grid.is_none() {
            'find: for frame in arr(&data["frames"]) {
                let g = &frame["overlay"]["geometry"];
                for line in arr(&g["lanes"]).iter().chain(arr(&g["edges"])) {
                    if !arr(line).is_empty() && arr(line).iter().all(|p| arr(p).len() == 3) {
                        grid = Some(arr(line).iter().map(|p| p[0].clone()).collect());
                        break 'find;
                    }
                }
            }
        }
        if let Some(grid) = grid {
            data["laneX"] = json!(grid);
            if let Some(frames) = data["frames"].as_array_mut() {
                for frame in frames {
                    for key in ["lanes", "edges"] {
                        if let Some(lines) = frame
                            .get_mut("overlay")
                            .and_then(|v| v.get_mut("geometry"))
                            .and_then(|v| v.get_mut(key))
                            .and_then(Value::as_array_mut)
                        {
                            for line in lines {
                                if arr(line).len() == grid.len()
                                    && arr(line)
                                        .iter()
                                        .zip(&grid)
                                        .all(|(p, x)| arr(p).len() == 3 && p[0] == *x)
                                {
                                    *line = json!(
                                        arr(line)
                                            .iter()
                                            .map(|p| json!([p[1], p[2]]))
                                            .collect::<Vec<_>>()
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        data["replayV"] = json!(2);
    }
    if !infos.is_empty() {
        data["cameraInfos"] = json!(infos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_uses_camera_then_model_then_event_clock_including_zero_camera_time() {
        let sample = |stamp, value| Sample {
            stamp,
            valid: true,
            value,
        };
        let mut decoded = Decoded::default();
        decoded.streams.insert(
            "modelV2".into(),
            vec![
                sample(
                    3_000_000_000,
                    json!({"frameId":1,"timestampEof":2_000_000_000u64}),
                ),
                sample(4_000_000_000, json!({"frameId":2,"timestampEof":0})),
            ],
        );
        assert_eq!(origin(&decoded).unwrap(), 2.);
        decoded.streams.insert(
            "qRoadEncodeIdx".into(),
            vec![sample(
                1,
                json!({"frameId":1,"timestampEof":1_500_000_000u64}),
            )],
        );
        assert_eq!(origin(&decoded).unwrap(), 1.5);
        decoded.streams.get_mut("qRoadEncodeIdx").unwrap()[0].value["timestampEof"] = json!(0);
        assert_eq!(origin(&decoded).unwrap(), 0.);
        decoded.streams.get_mut("modelV2").unwrap().clear();
        assert!(origin(&decoded).is_err());
    }

    #[test]
    fn compact_keeps_missing_overlay_null_even_with_other_frames_grid() {
        let mut data = json!({"frames":[{"overlay":null},{"overlay":{"geometry":{"lanes":[[[0.,1.,2.],[1.,2.,3.]]],"edges":[],"position":[]}}}]});
        compact(&mut data);
        assert!(data["frames"][0]["overlay"].is_null());
        let once = data.clone();
        compact(&mut data);
        assert_eq!(data, once);
    }

    #[test]
    fn fresh_past_and_leading_metadata_have_distinct_boundaries() {
        let streams = BTreeMap::from([(
            "carState".into(),
            vec![
                Sample {
                    stamp: 1_000_000_000,
                    valid: true,
                    value: json!({}),
                },
                Sample {
                    stamp: 2_000_000_000,
                    valid: false,
                    value: json!({}),
                },
            ],
        )]);
        let view = View::new(&streams);
        assert!(view.past("carState", 999_999_999., 150e6, false).is_none());
        assert!(
            view.past("carState", 1_149_999_999., 150e6, false)
                .is_some()
        );
        assert!(
            view.past("carState", 1_150_000_000., 150e6, false)
                .is_none()
        );
        assert!(view.past("carState", 0., 10e9, true).is_some());
        assert!(view.past("carState", 2_000_000_000., 10e9, true).is_none());
    }
}
