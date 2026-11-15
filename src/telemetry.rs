//! Original-clock telemetry and driving summary; field contract is extracted from Python.
use crate::logs::Sample;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::LazyLock};
static FIELDS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../resources/telemetry-fields.json"))
        .expect("valid telemetry fields")
});
pub fn nested<'a>(record: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(record, |r, k| r.get(k))
}
pub fn round(value: f64, digits: i32) -> f64 {
    crate::replay::r(value, digits.max(0) as usize)
}
fn number(value: Option<&Value>, factor: f64) -> Value {
    value
        .and_then(Value::as_f64)
        .map(|v| v * factor)
        .filter(|v| v.is_finite())
        .map(|v| json!(round(v, 6)))
        .unwrap_or(Value::Null)
}
fn long_state(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_str) {
        Some("off") => json!(0),
        Some("pid") => json!(1),
        Some("stopping") => json!(2),
        Some("starting") => json!(3),
        _ => Value::Null,
    }
}

pub fn extract(
    streams: &BTreeMap<String, Vec<Sample>>,
    origin: f64,
    duration: f64,
    frames: &[Value],
) -> Value {
    let mut result = serde_json::Map::new();
    for (topic, fields) in FIELDS.as_object().unwrap() {
        let fallback = if topic == "carOutput" && streams.get(topic).is_none_or(Vec::is_empty) {
            streams
                .get("carControl")
                .into_iter()
                .flatten()
                .filter_map(|row| {
                    row.value
                        .get("actuatorsOutputDEPRECATED")
                        .filter(|v| v.is_object())
                        .map(|v| Sample {
                            stamp: row.stamp,
                            valid: row.valid,
                            value: json!({"actuatorsOutput":v}),
                        })
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let rows = if !fallback.is_empty() {
            &fallback
        } else {
            streams.get(topic).map(Vec::as_slice).unwrap_or(&[])
        };
        let mut rows = rows.iter().collect::<Vec<_>>();
        rows.sort_by_key(|r| r.stamp);
        let fields = fields.as_object().unwrap();
        let mut times = Vec::new();
        let mut values: BTreeMap<String, Vec<Value>> =
            fields.keys().map(|k| (k.clone(), Vec::new())).collect();
        if topic == "controlsState" {
            values.insert("desiredAngle".into(), Vec::new());
        }
        if topic == "carControl" {
            values.insert("accelRequested".into(), Vec::new());
            values.insert("decelRequested".into(), Vec::new());
        }
        for row in rows {
            let time = round(row.stamp as f64 / 1e9 - origin, 6);
            if time < -0.15 || time > duration + 0.15 {
                continue;
            }
            times.push(time);
            let record = &row.value;
            for (key, spec) in fields {
                let raw = if row.valid {
                    nested(record, spec[0].as_str().unwrap())
                } else {
                    None
                };
                let factor = &spec[1];
                let mut value = if factor.is_null() {
                    raw.and_then(Value::as_bool)
                        .map(|v| json!(v))
                        .unwrap_or(Value::Null)
                } else if factor == "state" {
                    long_state(raw)
                } else {
                    number(raw, factor.as_f64().unwrap())
                };
                if topic == "carControl"
                    && ((matches!(
                        key.as_str(),
                        "targetAcceleration" | "plannedAcceleration" | "jerk"
                    ) && record["longActive"] != true)
                        || (matches!(
                            key.as_str(),
                            "targetAngle" | "commandTorque" | "commandCurvature"
                        ) && record["latActive"] != true))
                {
                    value = Value::Null;
                }
                if topic == "controlsState"
                    && matches!(key.as_str(), "actualLateralAccel" | "desiredLateralAccel")
                    && nested(record, "lateralControlState.torqueState.active")
                        != Some(&json!(true))
                {
                    value = Value::Null;
                }
                values.get_mut(key).unwrap().push(value);
            }
            if topic == "carControl" {
                let active = if row.valid {
                    record["longActive"].as_bool()
                } else {
                    None
                };
                let accel = if row.valid {
                    number(nested(record, "actuators.accel"), 1.).as_f64()
                } else {
                    None
                };
                for (key, positive) in [("accelRequested", true), ("decelRequested", false)] {
                    let value = match active {
                        Some(false) => json!(false),
                        Some(true) => accel
                            .map(|v| json!(if positive { v > 0. } else { v < 0. }))
                            .unwrap_or(Value::Null),
                        None => Value::Null,
                    };
                    values.get_mut(key).unwrap().push(value);
                }
            }
            if topic == "controlsState" {
                let state = ["angleState", "pidState"]
                    .iter()
                    .filter_map(|name| nested(record, &format!("lateralControlState.{name}")))
                    .find(|state| row.valid && state["active"] == true);
                values.get_mut("desiredAngle").unwrap().push(number(
                    state.and_then(|s| s.get("steeringAngleDesiredDeg")),
                    1.,
                ));
            }
        }
        if let Some(states) = values.get("longState") {
            let extra =
                [("off", 0), ("pid", 1), ("stopping", 2), ("starting", 3)].map(|(name, code)| {
                    (
                        format!("long_{name}"),
                        states
                            .iter()
                            .map(|v| {
                                if v.is_null() {
                                    Value::Null
                                } else {
                                    json!(*v == code)
                                }
                            })
                            .collect::<Vec<_>>(),
                    )
                });
            values.extend(extra);
        }
        result.insert(topic.clone(), json!({"times":times,"values":values}));
    }
    let mut values: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for frame in frames {
        let targets = if frame["valid"] == true {
            frame["ccncTargets"].as_array()
        } else {
            None
        };
        for slot in ["LF", "FF", "RF"] {
            let target = targets.and_then(|rows| rows.iter().find(|t| t["slot"] == slot));
            values.entry(format!("{slot}Visible")).or_default().push(
                targets
                    .map(|_| json!(target.is_some()))
                    .unwrap_or(Value::Null),
            );
            values
                .entry(format!("{slot}Distance"))
                .or_default()
                .push(number(target.and_then(|t| t.get("x")), 1.));
            values
                .entry(format!("{slot}Lateral"))
                .or_default()
                .push(number(target.and_then(|t| t.get("yRel")), 1.));
        }
    }
    for slot in ["LF", "FF", "RF"] {
        for field in ["Visible", "Distance", "Lateral"] {
            values.entry(format!("{slot}{field}")).or_default();
        }
    }
    result.insert(
        "ccnc".into(),
        json!({"times":frames.iter().map(|f|f["t"].clone()).collect::<Vec<_>>(),"values":values}),
    );
    json!({"duration":duration,"maxGap":0.15,"streams":result})
}

pub fn drive_summary(telemetry: &Value) -> Value {
    let Some(duration) = telemetry["duration"].as_f64().filter(|v| *v > 0.) else {
        return Value::Null;
    };
    let gap = telemetry["maxGap"]
        .as_f64()
        .filter(|v| *v != 0.)
        .unwrap_or(0.15);
    let control = &telemetry["streams"]["carControl"];
    let times = control["times"].as_array().cloned().unwrap_or_default();
    let enabled = control["values"]["enabled"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let known = times
        .iter()
        .zip(&enabled)
        .filter_map(|(t, v)| Some((t.as_f64()?, v.as_bool()?)))
        .collect::<Vec<_>>();
    if known.is_empty() {
        return Value::Null;
    }
    let mut engaged = 0.;
    let mut events = Vec::new();
    let mut previous = None;
    for (index, (time, value)) in known.iter().copied().enumerate() {
        if value {
            let end = known
                .get(index + 1)
                .map(|v| v.0)
                .unwrap_or(duration)
                .min(time + gap)
                .min(duration);
            engaged += (end - time.max(0.)).max(0.);
        } else if previous == Some(true) {
            events.push(time);
        }
        previous = Some(value);
    }
    let state = &telemetry["streams"]["carState"];
    for key in ["steeringPressed", "gasPressed", "brakePressed"] {
        let mut previous = None;
        if let (Some(times), Some(values)) =
            (state["times"].as_array(), state["values"][key].as_array())
        {
            for (t, v) in times.iter().zip(values) {
                let (Some(time), Some(value)) = (t.as_f64(), v.as_bool()) else {
                    continue;
                };
                if value && previous == Some(false) {
                    let i = known.partition_point(|(t, _)| *t <= time);
                    if i > 0 && known[i - 1].1 {
                        events.push(time);
                    }
                }
                previous = Some(value);
            }
        }
    }
    events.sort_by(f64::total_cmp);
    let mut takeovers = 0;
    let mut last = None;
    for time in events {
        if last.is_none_or(|v| time - v >= 1.) {
            takeovers += 1;
        }
        last = Some(time);
    }
    json!({"engaged_ratio":round((engaged/duration).clamp(0.,1.),4),"takeovers":takeovers})
}
